//! Intermediate-message delivery: standalone narration posts (rich-first
//! with HTML fallback), footer append to the last intermediate, the
//! rate-limit-retrying send wrapper and the HTML-or-plain send.
//!
//! Moved VERBATIM out of handler.rs (#471 phase 1, pure decomposition —
//! only visibility widened to pub(crate) so the handler glob re-export
//! keeps every existing call site and test import stable).

use super::flow::SentBubble;
use super::handler::StreamingState;
use super::markdown::{markdown_to_telegram_html, split_message, strip_html_tags};
use super::send::message_in_thread;
use crate::utils::LocalImageFailure;
use std::sync::Arc;
use teloxide::prelude::*;
use teloxide::types::{MessageId, ParseMode, ReplyParameters};
use uuid::Uuid;

/// Send an HTML message, falling back to plain text if Telegram rejects the HTML.
/// Returns the resulting `MessageId` so callers that need to track or later delete
/// the message (e.g. intermediate cleanup on cancellation) can do so.
/// Build the edited message body for appending the ctx/tok-s footer to the
/// last intermediate message.
///
/// Used when a turn's final response text deduped to empty because all of
/// it was already delivered as intermediate messages (the common tool-
/// using case). Rather than drop the footer (which left the user never
/// seeing ctx budget on Telegram — 2026-06-06) or send a standalone
/// footer bubble (removed in 7a0ca1c9), we edit the last intermediate
/// message to carry the footer inline.
///
/// Reconstructs the last chunk exactly as it was originally sent
/// (`markdown_to_telegram_html` + `split_message(_, 4096)` then `.last()`),
/// appends the footer, and returns `None` when:
/// - the footer or intermediate text is empty, OR
/// - the combined result would exceed Telegram's 4096-char cap (never
///   truncate real content to make room for metadata).
///
/// Pure + free function so the fit/reconstruct logic is unit-testable
/// without a live bot.
// Channel-unused since the ctx footer moved onto the flow message (the
// intermediate-footer append path went with the pre-block status bubble);
// kept because the reconstruct-last-chunk logic is nontrivial and its tests
// pin the split/fit contract meanwhile.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn build_last_intermediate_with_footer(
    last_intermediate_text: &str,
    footer: &str,
) -> Option<String> {
    if footer.is_empty() || last_intermediate_text.is_empty() {
        return None;
    }
    let html = markdown_to_telegram_html(last_intermediate_text);
    let chunks = split_message(&html, 4096);
    let last_chunk = chunks.last()?;
    let combined = format!("{last_chunk}\n\n{footer}");
    if combined.chars().count() > 4096 {
        None
    } else {
        Some(combined)
    }
}

/// Send a structured intermediate segment as a native rich message, returning
/// its id for tracking. Mermaid-aware (#1044/#1202): fences resolve to a media
/// array; with no fence this is byte-identical to `send_rich_markdown_id`.
/// Returns `None` when the text carries no rich structure
/// or the rich API rejects it — the caller then falls back to the HTML path.
///
/// `media` carries resolved LOCAL images (#502) whose ids the caller has
/// already rewritten into `text` as `tg://photo?id=<id>` references. The media
/// array is the sole authority for such a reference (#334), so the gate below
/// must let a media-bearing message onto the rich plane even when its text has
/// no block structure — otherwise a thin prose intermediate with one chart
/// takes the `None` arm, falls back to HTML, and the picture is lost a second
/// time.
pub(crate) async fn try_send_intermediate_rich(
    session_id: Uuid,
    bot: &Bot,
    chat_id: ChatId,
    thread_id: Option<teloxide::types::ThreadId>,
    text: &str,
    media: &[super::rich::mermaid::MediaEntry],
) -> Option<MessageId> {
    if !super::rich::should_send_native_rich_for_media(text, !media.is_empty()) {
        return None;
    }
    // Mermaid-aware sender (#1044/#1202): resolves fences into the rich
    // markdown media array; byte-identical to send_rich_markdown_id when no
    // fence is present, so non-diagram reports are unaffected. Guarded against
    // the abandoned-edit-loop duplicate (#500): this is the silent sender whose
    // message and the tail's landed as two copies of one turn's text.
    match super::delivery_dedup::send_rich_turn_guarded(
        session_id, bot, chat_id, thread_id, text, media,
    )
    .await
    {
        Ok(id) => Some(MessageId(id)),
        Err(e) => {
            tracing::warn!("Telegram: intermediate rich send failed, using HTML: {e}");
            None
        }
    }
}

/// True when an intermediate message contains a substantial markdown status report
/// (e.g. status/progress/pipeline heading or substantial section) worth delivering
/// as its own message rather than burying in collapsible flow (#215).
pub(crate) fn is_deliverable_status_report(text: &str) -> bool {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return false;
    }

    let total_chars = trimmed.chars().count();
    let line_count = text.lines().filter(|l| !l.trim().is_empty()).count();

    let mut in_fence = false;
    let mut fence_char = ' ';
    let mut fence_len = 0;
    let mut has_keyword_heading = false;
    let mut has_atx_heading = false;
    let mut has_status_callout = false;

    const STATUS_KEYWORDS: &[&str] = &[
        "status",
        "update",
        "progress",
        "pipeline",
        "summary",
        "verification",
        "verdict",
        "phase",
        "findings",
        "plan",
        "results",
    ];

    for line in text.lines() {
        let t = line.trim_start();
        let leading_spaces = line.len() - t.len();
        if leading_spaces < 4 {
            let fence_run: String = t.chars().take_while(|&c| c == '`' || c == '~').collect();
            if fence_run.len() >= 3 {
                let f_char = fence_run.chars().next().unwrap();
                if !in_fence {
                    in_fence = true;
                    fence_char = f_char;
                    fence_len = fence_run.len();
                    continue;
                } else if f_char == fence_char && fence_run.len() >= fence_len {
                    in_fence = false;
                    continue;
                }
            }
        }
        if in_fence {
            continue;
        }

        if super::rich::is_atx_heading(t) {
            has_atx_heading = true;
            let lower = t.to_lowercase();
            if STATUS_KEYWORDS.iter().any(|kw| lower.contains(kw)) {
                has_keyword_heading = true;
            }
        }

        // Rule C — structured callout: non-empty line starting with `>` and `**` followed by status/update/progress
        let trimmed_line = line.trim();
        if let Some(after_gt) = trimmed_line.strip_prefix('>') {
            let after_gt = after_gt.trim_start();
            if let Some(after_stars) = after_gt.strip_prefix("**") {
                let lower_callout = after_stars.to_lowercase();
                if lower_callout.starts_with("status")
                    || lower_callout.starts_with("update")
                    || lower_callout.starts_with("progress")
                {
                    has_status_callout = true;
                }
            }
        }
    }

    // Rule A — keyword heading: line_count >= 2 and total_chars >= 50
    if has_keyword_heading && line_count >= 2 && total_chars >= 50 {
        return true;
    }

    // Rule B — substantial section: any ATX heading, line_count >= 3 and total_chars >= 150
    if has_atx_heading && line_count >= 3 && total_chars >= 150 {
        return true;
    }

    // Rule C — structured callout: line_count >= 2 and total_chars >= 60
    if has_status_callout && line_count >= 2 && total_chars >= 60 {
        return true;
    }

    false
}

/// True when a folded intermediate is a substantial rich report worth
/// delivering as its OWN message rather than burying in the collapsed
/// processing log (#582). Keyed on a real markdown table plus some length, so
/// thin narration (no table) keeps folding and only report-shaped content —
/// which the model may emit before a tool call (e.g. text + `plan complete` in
/// one step) — is surfaced.
pub(crate) fn is_deliverable_rich_report(text: &str) -> bool {
    // #690 follow-up (#980): a table collapsed onto ONE line is invisible to
    // contains_table (which needs the header and separator each on their own
    // line), so a collapsed report would fail this gate and get buried in the
    // folded log as raw pipes. Reflow first — the same recovery the final-
    // response and HTML-render paths already apply. Idempotent.
    let reflowed = super::rich::reflow_collapsed_tables(text);
    // A mermaid fence (tagged or content-classified, #1202) is report-shaped
    // on its own: folding buries the diagram behind a tap-to-expand tap AND
    // leaves raw fence text in the log, because neither the fold renderer nor
    // the pre-fix rich path resolved fences. Surfaced intermediates go
    // through deliver_intermediate_message, which now resolves them.
    if super::rich::mermaid::has_mermaid_fence(&reflowed) {
        return true;
    }
    if super::rich::contains_table(&reflowed) && text.trim().chars().count() >= 200 {
        return true;
    }
    is_deliverable_status_report(text)
}

/// The promotion decision for a mid-turn intermediate (#582 + #502).
///
/// The fourth trigger: a reference to a picture this turn has not put in the
/// chat yet is report-shaped content on its own, and burying it in the
/// collapsed processing log loses it outright — the fold path strips image
/// references, and the final-response leg only re-scans the FINAL text, so a
/// turn that streams a chart and closes with a short summary delivered neither
/// the picture nor a notice (#502).
///
/// `fresh_images` counts images NOT yet delivered this turn — a reference whose
/// picture already rode an earlier bubble is not a reason to open a second one.
///
/// Deliberately a separate function rather than a wider
/// [`is_deliverable_rich_report`]: that predicate is pure and has 27 test call
/// sites, and putting the image count into its signature would drag the
/// filesystem (and the turn's delivered-path snapshot) into a pure verdict.
/// The text argument is the STRIPPED form, so the three existing arms see
/// byte-for-byte what they see today and no existing verdict moves.
pub(crate) fn should_promote_intermediate(text: &str, fresh_images: usize) -> bool {
    fresh_images > 0 || is_deliverable_rich_report(text)
}

/// Is a promoted bubble burial evidence — text the final rich message repeats,
/// and therefore safe to delete once that message lands?
///
/// #617: a bubble that carried a local image is a **deliverable**, not burial
/// evidence. The supersession cleanup (`delivery.rs`) deletes the ids a bubble
/// records in `sent_bubbles` when the rich fallback succeeds — and that
/// fallback carries no media — so giving a media bubble ids would remove the
/// only copy of the picture, *after* it was delivered. The final leg has
/// already skipped those paths via `delivered_image_paths`, so nothing
/// re-sends it. Such a bubble therefore records EMPTY ids.
///
/// Deliberately a pure predicate, for the same reason as
/// [`should_promote_intermediate`]: both directions are testable without a live
/// bot, and the caller keeps the lock discipline.
pub(crate) fn promoted_bubble_is_burial_evidence(media_count: usize) -> bool {
    media_count == 0
}

/// Normalize a bubble body for the dedup comparison — collapse runs of
/// whitespace (including newlines) to single spaces, so minor formatting
/// differences between a streamed intermediate and the final response do not
/// bypass dedup.
///
/// ONE home for the predicate (#620). Two consumers must agree on what "the
/// same text" means: the dedup that SUPPRESSES the final response, and the
/// burial that DELETES intermediates. Split them and either arm can be wrong
/// in both directions — a bubble deleted that the dedup never matched (silent
/// text loss), or a bubble spared whose text the rich message just re-sent
/// (a duplicate). So the comparison is shared by construction, not by
/// convention.
pub(crate) fn normalize_for_dedup(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Which intermediates does re-sending `rich_text` as one rich bubble REPLACE?
///
/// #620: the burial arm deleted EVERY id in the list on the premise that the
/// rich message supersedes all of them. It supersedes the bubbles whose text it
/// actually repeats — and only those. A narration bubble whose text the rich
/// message does not carry is the sole copy of that prose; deleting it loses the
/// text with no trace.
///
/// Composition with [`promoted_bubble_is_burial_evidence`] (#617) is deliberate
/// and one-directional: that guard keeps a media bubble's ids out of the list
/// entirely, so a media bubble reaches this filter carrying NO ids and can
/// contribute none. The guard stays load-bearing and is not duplicated here.
///
/// Pure + free function for the same reason as that predicate — both directions
/// (selected and spared) are testable without a live bot.
pub(crate) fn superseded_ids(bubbles: &[SentBubble], rich_text: &str) -> Vec<MessageId> {
    let norm_final = normalize_for_dedup(rich_text);
    bubbles
        .iter()
        .filter(|b| normalize_for_dedup(&b.text) == norm_final)
        .flat_map(|b| b.ids.iter().copied())
        .collect()
}

/// Does an UNDELETABLE bubble already carry `rich_text` — so that the rich
/// fallback's re-send would be pure duplication (#1939)?
///
/// The fallback arm's premise is that it REPLACES the intermediates it deletes.
/// A bubble that carried media records EMPTY ids (#617, [`promoted_bubble_is_burial_evidence`]),
/// so [`superseded_ids`] cannot select it and nothing is deleted — yet the arm
/// still sent, and the reader got the body twice: once in the media-bearing rich
/// intermediate and once in the fallback. The second copy's `📎` marker also
/// pointed nowhere, because the document rode the first.
///
/// Composition with [`superseded_ids`] is deliberate: the two answer different
/// questions about the same list. `superseded_ids` names what may be DELETED;
/// this names whether there is anything left to REPLACE. When it is true the
/// send is skipped, and `superseded_ids` is still consulted for the bubbles the
/// surviving copy supersedes.
///
/// Pure + free for the same reason as its neighbours: both directions are
/// testable without a live bot.
pub(crate) fn fallback_would_duplicate(bubbles: &[SentBubble], rich_text: &str) -> bool {
    let norm_final = normalize_for_dedup(rich_text);
    bubbles
        .iter()
        .any(|b| b.ids.is_empty() && normalize_for_dedup(&b.text) == norm_final)
}

/// The documents the intermediates being superseded already delivered, as
/// `(path, bubble id)` (#1939).
///
/// The rich fallback's own `file_links` is built from the FINAL leg's `delivered`
/// list, which is empty whenever the rich plane owns documents — nothing was
/// sent, so nothing could be linked. But the documents did reach the chat, in an
/// intermediate, and their addresses are recorded on the bubble that delivered
/// them. Restricted to the bubbles the fallback actually supersedes: a
/// narration bubble's documents belong to that bubble, which survives.
pub(crate) fn intermediate_file_links(
    bubbles: &[SentBubble],
    rich_text: &str,
) -> Vec<(std::path::PathBuf, i32)> {
    let norm_final = normalize_for_dedup(rich_text);
    bubbles
        .iter()
        .filter(|b| normalize_for_dedup(&b.text) == norm_final)
        .flat_map(|b| b.delivered_files.iter().cloned())
        .collect()
}

/// The two halves of one intermediate turn's media partition (#465), plus the
/// image family's second rewrite (#732).
///
/// The image and video walks are one partition, not two independent scans: a
/// reference belongs to exactly one family, and only the pair can say which.
/// They travel as one value for that reason — and because taking them as two
/// more positional parameters would push the delivery below to nine
/// arguments, past the point a positional list stays readable
/// (`clippy::too_many_arguments`). #1918 adds the file family's rich walk, the
/// third of the same partition.
pub(crate) struct IntermediateMedia<'a> {
    /// The image family's rewrite for the RICH plane. Built on the video
    /// family's rich form (`vw.rich`), so both families' `tg://` references
    /// survive in `rich` for the media array to answer — a `tg://video` target
    /// classifies as a media ref, so the image walk leaves it verbatim.
    pub(crate) rich_images: &'a crate::utils::image::LocalImageRewrite,
    /// The image family's rewrite for the HTML plane and the dedup record.
    /// Built on the video family's stripped form (`vw.stripped`), so NEITHER
    /// family's reference survives: the HTML plane has no media array and would
    /// ship a `tg://` reference as dead visible markdown (#732).
    pub(crate) stripped_images: &'a crate::utils::image::LocalImageRewrite,
    /// The video family's rewrite. Its entries ride the rich media array beside
    /// the pictures, each carrying its own `MediaKind::Video`; only when the
    /// rich send is refused does a clip fall back to its own bubble.
    pub(crate) videos: &'a crate::utils::image::LocalVideoRewrite,
    /// The file family's rewrite for the RICH plane (#1918). Built on the image
    /// family's rich form, so every family's `tg://` reference survives in
    /// `rich` for the shared media array to answer. Its entries ride that same
    /// array as `MediaKind::Document`, each carrying the file's own name — the
    /// promoted bubble inlines the document at its reference instead of showing
    /// the bare `📎 <label>` marker the #1918 probe found on `90894`.
    pub(crate) rich_files: &'a crate::utils::image::LocalFileRewrite,
}

/// Deliver a promoted intermediate as its own message (rich-first, HTML
/// fallback) and record it in `sent_bubbles` so the final-response dedup will
/// not resend it and the supersession cleanup knows which ids it occupies.
/// Returns true when something was delivered.
///
/// Takes the rewrites WHOLE rather than as `&str`s (#502) so no call site can
/// hand this function the wrong text form. That matters: the rich form carries
/// `tg://photo?id=imgN` AND `tg://video?id=vidN` references that only resolve
/// against the media array sent with it, while the stripped form is the shape
/// the HTML fallback and the dedup record need — the HTML plane has no media
/// array and would ship a `tg://` reference as dead visible markdown.
///
/// [`IntermediateMedia`] carries all FOUR rewrites — see that type for why they
/// travel as one value rather than as more parameters.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn deliver_intermediate_message(
    session_id: Uuid,
    bot: &Bot,
    chat: ChatId,
    thread_id: Option<teloxide::types::ThreadId>,
    streaming: &Arc<std::sync::Mutex<StreamingState>>,
    tg: &super::state::TelegramState,
    base_dir: &std::path::Path,
    walks: IntermediateMedia<'_>,
) -> bool {
    let rw_rich = walks.rich_images;
    let rw_stripped = walks.stripped_images;
    let vw = walks.videos;
    // #690 follow-up (#980): re-expand a collapsed table once, up front, so the
    // dedup record, the rich send and the HTML fallback all see the same
    // expanded shape. The HTML path reflows again internally but is idempotent.
    let stripped_expanded = super::rich::reflow_collapsed_tables(&rw_stripped.stripped);
    // #1918: the FILE family now owns the rich plane's document references, so
    // the rich body is the FILE walk's `rich` form — built on the image family's
    // rich form, so it carries `tg://photo`, `tg://video` AND `tg://document`
    // references, each answered by the shared media array below. Taking the
    // marker form here instead (which is what the pre-#1918 line did) would
    // leave the array's document entries with no reference to attach to.
    //
    // The HTML fallback and the dedup record keep the MARKER form, deliberately:
    // that plane has no media array, so a `tg://document` reference there would
    // ship as dead visible markdown — and a marker at least names the document
    // the reader is about to get as its own bubble.
    let rich_expanded = super::rich::reflow_collapsed_tables(walks.rich_files.rich.as_str());
    let stripped_files =
        crate::utils::extract_local_files(stripped_expanded.as_str(), Some(base_dir));
    let rich = rich_expanded.as_str();
    let text = stripped_files.text.as_str();
    {
        let s = streaming.lock().unwrap_or_else(|e| e.into_inner());
        if s.sent_bubbles.iter().any(|prev| prev.text == text) {
            return true;
        }
    }

    // Read the resolved bytes ONCE for the rich media array. A read that fails
    // here is an honest failure for the notice, never a panic — the reference
    // validated seconds ago, but a file can disappear between the two.
    //
    // The failure list is the SHARED one (`intermediate_failures`), not
    // `rw_stripped.failures` alone: the two walks are two halves of one
    // partition, and only the pair knows which reference each family actually
    // claimed (#465).
    //
    // #732: the clip rides the rich media array BESIDE the pictures, exactly as
    // it does on the final leg. What makes that possible is the base this path's
    // image walk is given: `rw_rich` is built on `vw.rich`, so the video
    // family's `![clip](tg://video?id=vidN)` reference survives into the body
    // the rich plane sends, and the array entry below answers it. An entry whose
    // reference is absent is exactly what `neutralize_orphan_photo_refs` defuses
    // (`mermaid.rs:1285`), which is why the RICH-plane base is the one the media
    // array must be paired with.
    //
    // The clip's own bubble is not gone — it MOVED, into the HTML fallback at
    // the bottom of this function, where it is reached only when the rich send
    // returned `None`. The floor helper is the same one the final leg uses, so
    // kind selection, captions, telemetry and failure reasons cannot drift
    // between the two (#502). `vw.entries` already excludes anything this turn
    // has delivered, so a repeated intermediate cannot ship a clip twice.
    let mut failures = super::delivery::intermediate_failures(rw_stripped, vw);
    // #1918: the documents' read failures are their OWN list, exactly as they are
    // on the final leg — the notice names the family, so a document that could
    // not be read must not be answered with "Image not attached".
    let mut file_failures: Vec<LocalImageFailure> = Vec::new();
    let mut media: Vec<super::rich::mermaid::MediaEntry> = Vec::new();
    let (entries, image_failures, _) = super::delivery::read_media_entries(
        &rw_rich.entries,
        super::rich::mermaid::MediaKind::Photo,
        "promoted image",
        |_| None,
    )
    .await;
    media.extend(entries);
    failures.extend(image_failures);
    // #732: the clips ride the SAME array, each carrying its own kind so the
    // builder emits the string that matches the bytes — exactly as the final
    // leg does. Their `tg://video?id=vidN` references are present in `rich`
    // because it was built on the video family's rich form (`rw_rich`). A read
    // that fails here joins the failure list, never a panic.
    let (entries, video_failures, delivered_video_paths) = super::delivery::read_media_entries(
        &vw.entries,
        super::rich::mermaid::MediaKind::Video,
        "promoted video",
        |_| None,
    )
    .await;
    media.extend(entries);
    failures.extend(video_failures);

    // #1918: the documents ride the SAME array, each carrying its own kind and
    // the file's OWN name — the entry's part name is what Telegram shows as the
    // file name in the chat, and bytes alone cannot recover it (`document_part_name`).
    // Their `tg://document?id=docN` references are present in `rich` because it
    // is the file walk's own rich form. A read that fails here joins the failure
    // list, never a panic.
    let (entries, file_read_failures, delivered_file_paths) = super::delivery::read_media_entries(
        &walks.rich_files.entries,
        super::rich::mermaid::MediaKind::Document,
        "promoted file",
        |entry| Some(super::delivery::document_part_name(&entry.file.path)),
    )
    .await;
    media.extend(entries);
    file_failures.extend(file_read_failures);
    // The entries whose bytes actually READ, for the HTML fallback's floor
    // below. Rebuilding that list from `rich_files.entries` wholesale would hand
    // `send_local_files` the unreadable ones too, and it would report each of
    // them a second time — the same file, the same noun, two notices. So the
    // floor gets exactly the paths the read above returned.
    let readable_files: Vec<crate::utils::image::LocalFile> = walks
        .rich_files
        .entries
        .iter()
        .filter(|entry| delivered_file_paths.contains(&entry.file.path))
        .map(|entry| entry.file.clone())
        .collect();

    // An image the bubble announced must not vanish silently when it cannot be
    // read: the notice rides the same bubble the model wrote (#502). The
    // documents get their own notice under their own noun (#1918) — same rule,
    // different family, which is why the wording is not shared.
    let body = crate::utils::append_failure_notice(rich, &failures);
    let body = crate::utils::append_file_failure_notice(&body, &file_failures);

    if let Some(id) = try_send_intermediate_rich(session_id, bot, chat, thread_id, &body, &media)
        .await
    {
        // The bubble is non-sticky burial evidence (#1150): the flow block must
        // restick below its own output on the next append.
        tg.note_bot_bubble(chat.0, id.0);
        let mut s = streaming.lock().unwrap_or_else(|e| e.into_inner());
        // #617: only a bubble that carried NO media is burial evidence. A bubble
        // that carried a local image holds the only copy of that picture — the
        // rich-fallback cleanup deletes the ids a bubble carries, and the final
        // leg has already skipped these paths, so a delete would remove a picture
        // the user has already been shown. Such a bubble records EMPTY ids, which
        // is also what keeps it out of the #620 delete set.
        let ids = if promoted_bubble_is_burial_evidence(media.len()) {
            vec![id]
        } else {
            Vec::new()
        };
        // #1939: the documents rode THIS bubble's media array, so this bubble is
        // their address — the fallback leg needs it to point the `📎` marker at
        // something the reader can open. Recorded before the paths move into
        // `delivered_image_paths` below.
        let delivered_files: Vec<(std::path::PathBuf, i32)> = delivered_file_paths
            .iter()
            .cloned()
            .map(|path| (path, id.0))
            .collect();
        s.sent_bubbles.push(SentBubble {
            text: text.to_string(),
            ids,
            delivered_files,
        });
        // Record the paths that just rode this bubble, so neither a later
        // intermediate nor the final leg ships the same media twice (#502).
        // The API's own success is the receipt here: the bytes went out with
        // this request.
        //
        // One list for both families (#465): a path is either already in the
        // chat or it is not, whichever plane put it there, so the delivered-media
        // record is not split by kind. #732: the clips rode THIS request's media
        // array, so they are recorded here beside the pictures — the only copy
        // of each clip is now in the rich message the reader was shown.
        for entry in &rw_rich.entries {
            s.delivered_image_paths.push(entry.image.path.clone());
        }
        s.delivered_image_paths.extend(delivered_video_paths);
        // #1918: and the documents, for the same reason — the only copy of each
        // is now in the rich message the reader was shown, so the final leg's
        // floor must not ship it a second time as its own bubble.
        s.delivered_image_paths.extend(delivered_file_paths);
        return true;
    }

    // ── HTML fallback ───────────────────────────────────────────────────────
    // Uses the STRIPPED form, never a re-strip of the rich form: the HTML plane
    // has no media array, and `record_candidate` leaves a `tg://photo?id=` ref
    // verbatim (it classifies as MediaRef), so rendering the rich form here
    // would show the user dead markdown where the picture should be. The same
    // holds for `tg://video?id=` (#732) — `rw_stripped` is built on the video
    // family's stripped form, so NEITHER family's reference reaches this plane.
    //
    // The media is not lost on this plane either (#502): each resolved image
    // ships as its own bubble through the SAME helper the final leg uses, so
    // the two cannot drift on kind selection, captions or failure reasons.
    // #732: the clips ship here TOO — this is the site their bubble MOVED to,
    // from the pre-text send the rich path used to make unconditionally. It is
    // reached only when the rich send returned `None`, which is exactly when the
    // clip has nowhere else to go: it cannot ride an array this plane does not
    // have. Each family keeps its own helper and its own refusal list.
    let mut plain = crate::utils::append_failure_notice(text, &failures);
    let images: Vec<crate::utils::image::LocalImage> =
        rw_stripped.entries.iter().map(|e| e.image.clone()).collect();
    let (delivered, refused) =
        super::delivery::send_local_images(session_id, bot, chat, thread_id, &images).await;
    if !delivered.is_empty() {
        let mut s = streaming.lock().unwrap_or_else(|e| e.into_inner());
        s.delivered_image_paths.extend(delivered);
    }
    plain = crate::utils::append_failure_notice(&plain, &refused);
    let videos: Vec<crate::utils::image::LocalVideo> =
        vw.entries.iter().map(|e| e.video.clone()).collect();
    let (delivered_videos, refused_videos) =
        super::delivery::send_local_videos(session_id, bot, chat, thread_id, &videos).await;
    if !delivered_videos.is_empty() {
        let mut s = streaming.lock().unwrap_or_else(|e| e.into_inner());
        s.delivered_image_paths.extend(delivered_videos);
    }
    // A clip the channel refused is an honest failure of the same turn, and it
    // rides the text bubble the reader is about to get — never a silent drop.
    plain = crate::utils::append_failure_notice(&plain, &refused_videos);
    // #1918: the documents ship here TOO — this is the site their bubble moved
    // to, reached only when the rich send returned `None`, which is exactly when
    // the document has nowhere else to go: it cannot ride an array this plane
    // does not have. Same helper as the final leg's floor, so kind selection,
    // the file's own name and the failure reasons cannot drift between them.
    // Only the files whose bytes were READ reach this floor. The unreadable
    // ones are already in `file_failures` above; handing them to
    // `send_local_files` as well would report each of them twice under the same
    // noun.
    let (delivered_files, refused_files) =
        super::delivery::send_local_files(session_id, bot, chat, thread_id, &readable_files).await;
    if !delivered_files.is_empty() {
        let mut s = streaming.lock().unwrap_or_else(|e| e.into_inner());
        s.delivered_image_paths
            .extend(delivered_files.iter().map(|f| f.path.clone()));
    }
    plain = crate::utils::append_file_failure_notice(&plain, &refused_files);
    // The documents whose bytes could not be read never reached either send, so
    // their notice is owed here too — same list, same noun (#1918).
    plain = crate::utils::append_file_failure_notice(&plain, &file_failures);

    // Resolve fences here too (#1142 parity): when the rich path rejected the
    // message, the HTML fallback must still render the diagram instead of
    // shipping raw fence text. Identical to markdown_to_telegram_html when
    // the feature is off or no fence is present.
    let html = super::rich::markdown_to_html_mermaid(&plain).await;
    if html.is_empty() {
        return false;
    }
    let mut sent_ids: Vec<MessageId> = Vec::new();
    for chunk in split_message(&html, 4096) {
        match send_html_or_plain(bot, chat, thread_id, chunk, "turn", None).await {
            Ok(id) => {
                tg.note_bot_bubble(chat.0, id.0);
                sent_ids.push(id);
            }
            Err(e) => {
                tracing::warn!("Telegram: rich-intermediate send failed ({e})");
                return false;
            }
        }
    }
    let mut s = streaming.lock().unwrap_or_else(|e| e.into_inner());
    // ONE text, N ids — the HTML plane chunks a single bubble across as many
    // messages as 4096 chars requires. Pairing them here is the whole point of
    // `SentBubble`: two parallel vectors could not say which ids carry which
    // text, which is what made the #620 over-delete unfixable at the cleanup.
    s.sent_bubbles.push(SentBubble {
        text: text.to_string(),
        ids: sent_ids,
        // #1939: on this plane each document shipped as its OWN bubble from the
        // file floor above, so the fallback leg cannot derive their addresses
        // from this bubble's ids — they travel here instead.
        delivered_files: delivered_files
            .iter()
            .map(|file| (file.path.clone(), file.message_id))
            .collect(),
    });
    true
}

/// Threshold for treating a Telegram 429 as a "long rate-limit" (#1110).
///
/// When Telegram returns `Retry-After: N` where N > this threshold, the chat
/// is flood-banned for hours (28442s = 7.9 hours observed). Retrying the
/// send ladder burns 90 seconds (3 × 30s clamped wait) for no gain: the
/// window won't clear in that time. Instead, bail immediately and let the
/// caller surface the rate-limit to the user.
///
/// One hour is the boundary: typical flood windows (placeholder-edit churn,
/// command bursts) are seconds and stay under the inline cap. Anything over
/// an hour is a multi-hour ban, not a throttle.
const LONG_RATE_LIMIT_THRESHOLD: std::time::Duration = std::time::Duration::from_secs(3600);

/// Run a Telegram send, waiting out `RetryAfter` (429) up to 3 attempts.
///
/// Command replies are programmatic: a per-chat rate limit (typically a
/// streaming turn editing its placeholder into the same chat) must DELAY
/// them, never drop them. The command branches used a bare `.await?`, so
/// the 429 propagated out of the handler and the reply vanished with a
/// single error log line — /models looked "stuck" while a turn streamed
/// and worked right after it completed (#297). Non-429 errors and
/// exhausted retries still propagate to the caller.
///
/// Long rate-limits (>1 hour) bail immediately without retrying (#1110).
pub(crate) async fn send_retrying_rate_limit<T, F, Fut>(
    what: &str,
    mut send: F,
) -> std::result::Result<T, teloxide::RequestError>
where
    F: FnMut() -> Fut,
    Fut: std::future::IntoFuture<Output = std::result::Result<T, teloxide::RequestError>>,
{
    const MAX_RETRIES: u32 = 3;
    let mut attempt = 0u32;
    loop {
        match send().await {
            Err(teloxide::RequestError::RetryAfter(secs)) => {
                let requested = secs.duration();
                // Long rate-limit (>1 hour): bail immediately, don't retry (#1110).
                // The chat is flood-banned for hours; retrying burns 90s for no gain.
                if requested > LONG_RATE_LIMIT_THRESHOLD {
                    tracing::error!(
                        "Telegram: {what} long rate-limit ({}s > {}s threshold) — bailing immediately, \
                         no retry ladder (#1110)",
                        requested.as_secs(),
                        LONG_RATE_LIMIT_THRESHOLD.as_secs()
                    );
                    return Err(teloxide::RequestError::RetryAfter(secs));
                }
                if attempt < MAX_RETRIES {
                    attempt += 1;
                    let outcome = super::rate_limit::wait_out(
                        what,
                        requested,
                        &format!(" (attempt {attempt}/{MAX_RETRIES})"),
                        None,
                    )
                    .await;
                    if matches!(outcome, super::rate_limit::WaitOutcome::Deferred) {
                        return Err(teloxide::RequestError::RetryAfter(secs));
                    }
                } else {
                    tracing::error!(
                        "Telegram: {what} still rate-limited after {MAX_RETRIES} retries ({}s) — giving up",
                        requested.as_secs()
                    );
                    return Err(teloxide::RequestError::RetryAfter(secs));
                }
            }
            // No success line here (review F1): the wrapper is generic and
            // has no correlation fields, so its line carried nothing the
            // chokepoint telemetry doesn't already say with full fields.
            other => return other,
        }
    }
}

pub(crate) async fn send_html_or_plain(
    bot: &Bot,
    chat_id: ChatId,
    thread_id: Option<teloxide::types::ThreadId>,
    html: &str,
    origin: &str,
    reply_to: Option<i32>,
) -> std::result::Result<MessageId, teloxide::RequestError> {
    // G3 send pacing (#1211): the universal outbox ladder funnels cron
    // deliveries, tool sends and chunked replies through here, so the
    // ~1/s + 18/min per-forum pacer applies at this one seam. DMs pass
    // through untouched; pacing delays, never drops (#297).
    super::governor::pace_send(chat_id).await;
    // Correlation telemetry (#1085 P1a, review F8): this is the chokepoint
    // carrying chunked final replies, command acks and error notices.
    // `origin` is threaded by the caller (turn | tool | cron | system) so
    // an outbox/cron send is never mislabeled "turn". Every exit logs;
    // metadata only, never content.
    let thread = thread_id.map(|t| t.0.0);
    let hash8 = super::telemetry::content_hash8(html);
    let len = html.len();
    let log_ok = |path: &str, m: &MessageId, len: usize, hash8: &str| {
        super::telemetry::log_send_success(
            origin,
            "-",
            "-",
            "html_or_plain",
            path,
            chat_id.0,
            thread,
            m.0,
            len,
            hash8,
        );
    };
    // HTML rides the shared retry ladder (#1085 P1b R1): up to 3 attempts
    // with `rate_limit::wait_out` between them (#297 delay-never-drop),
    // matching every other send path — previously this hand-rolled a single
    // retry. Only a final failure falls back to plain text, and the
    // fallback rides the same ladder so a 429 cannot drop it either.
    // `reply_to` (optional) attaches Telegram reply_parameters so the same
    // seam carries tool-reply targeting without a separate writer (#1230).
    match send_retrying_rate_limit("HTML send", || {
        let mut req = message_in_thread(bot, chat_id, thread_id, html);
        if let Some(mid) = reply_to {
            req = req.reply_parameters(ReplyParameters::new(MessageId(mid)));
        }
        req.parse_mode(ParseMode::Html)
    })
    .await
    {
        Ok(m) => {
            log_ok("html", &m.id, len, &hash8);
            Ok(m.id)
        }
        Err(e) => {
            tracing::warn!("Telegram: HTML send failed after retries ({e}), sending as plain text");
            let plain = strip_html_tags(html);
            // Review F2: hash and len must describe the text that actually
            // landed on the wire (the stripped plain text), not the HTML
            // source — a duplicate-correlation query must match payloads.
            let plain_hash8 = super::telemetry::content_hash8(&plain);
            let plain_len = plain.len();
            match send_retrying_rate_limit("plain fallback", || {
                let mut req = message_in_thread(bot, chat_id, thread_id, plain.as_str());
                if let Some(mid) = reply_to {
                    req = req.reply_parameters(ReplyParameters::new(MessageId(mid)));
                }
                req
            })
            .await
            {
                Ok(m) => {
                    log_ok("plain_fallback", &m.id, plain_len, &plain_hash8);
                    Ok(m.id)
                }
                Err(e2) => {
                    super::telemetry::log_send_failure(
                        origin,
                        "-",
                        "-",
                        "html_or_plain",
                        "plain_fallback",
                        chat_id.0,
                        thread,
                        plain_len,
                        &plain_hash8,
                        &e2.to_string(),
                    );
                    Err(e2)
                }
            }
        }
    }
}
