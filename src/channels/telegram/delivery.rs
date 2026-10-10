//! Final-response delivery: the ONE path every Telegram turn ends on
//! (#471 phases 2-4). Live turns (handle_message) and crash-recovery
//! resumes both call deliver_final_response; the post-loop display drain
//! lives here with it.

use super::TelegramState;
use super::flow::{
    DisplayItem, StreamingState, append_intermediate_to_flow, append_system_to_flow,
    append_tool_group, folded_duplicates_final, last_folded_text, settle_options_reclaim,
    take_folded_final,
};
use super::handler::{fire_reaction, map_to_allowed_reaction};
use super::intermediates::send_html_or_plain;
use super::markdown::{markdown_to_telegram_html, split_message};
use super::rich::mermaid::{MediaEntry, MediaKind};
use super::send::{
    TelegramMediaKind, TelegramVideoKind, VIDEO_FORMAT_HEAD_BYTES, best_effort_delete,
    document_in_thread, message_in_thread, photo_in_thread, sniff_video_format,
    telegram_media_kind, telegram_video_media_kind, video_in_thread, voice_in_thread,
};
use crate::brain::agent::AgentService;
use crate::db::ChannelMessageRepository;
use crate::db::models::ChannelMessage as DbChannelMessage;
use crate::utils::image::MediaSource;
use crate::utils::sanitize::{redact_secrets, redact_secrets_scoped};
use crate::utils::{LocalImageFailure, LocalImageFailureReason};
use std::path::PathBuf;
use std::sync::Arc;
use teloxide::prelude::*;
use teloxide::types::{InputFile, MessageId, ParseMode};
use uuid::Uuid;

/// Whether a turn carrying a `<<react:…>>` directive is react-ONLY, given the
/// text that remains once the directive is stripped.
///
/// The prompt teaches the model two shapes (see `handler.rs`, "Reaction
/// directive"): output ONLY the directive to react-only, or put the directive
/// at the START and answer after it to react AND respond. Emptiness of the
/// remaining text is therefore the whole question, and the answer is on the
/// wire rather than inferred.
///
/// Deliberately takes no other input. #928 also consulted whether the turn ran
/// tools and suppressed the text when it had not, which destroyed completions
/// that simply needed no tools (#1009). Tool state does not tell you what the
/// content channel holds, so it is not a parameter here.
pub(crate) fn is_react_only(text_after_directive: &str) -> bool {
    text_after_directive.trim().is_empty()
}

/// Whether the turn is genuinely react-only, as opposed to merely text-empty.
///
/// Emptiness alone does not answer it. A final suppressed by dedup (#1152) is
/// also text-empty at this point, but it was already delivered as intermediate
/// bubbles, so treating it as react-only fires the early return and the #546
/// incomplete-turn notice on top of work the user already received.
///
/// Named rather than inlined at the call site so the distinction has one
/// definition and a test can exercise it: asserting the expression by
/// rebuilding it in a test body proves nothing about the delivery path.
pub(crate) fn is_react_only_turn(suppressed_final: bool, text_after_directive: &str) -> bool {
    !suppressed_final && is_react_only(text_after_directive)
}

/// Whether the turn ends on a suggest_options surface (#1226 K): the
/// progress handler stashes the options mid-turn (progress.rs), so by
/// delivery time a Some here means an option surface is armed and the
/// flow block ends on the suggest_options Tool entry — the reclaim calls
/// below must then look BEFORE that trailing tool for the answer.
fn options_pending(streaming: &Arc<std::sync::Mutex<StreamingState>>) -> bool {
    streaming
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .pending_suggestions
        .is_some()
}

/// True when a rich-send failure means Telegram could not fetch the embedded
/// media (the mermaid.ink diagram) — a transient renderer or network window
/// that a single re-send can sail (#tg-mermaid-delivery-hardening). Our
/// resolve runs seconds before Telegram's own server-side refetch, so
/// `RICH_MESSAGE_PHOTO_NO_MEDIA_FOUND` is a race with a flaky renderer, not a
/// structural rejection. Structural 400s (schema, content) are never retried.
pub(crate) fn is_no_media_found(e: &anyhow::Error) -> bool {
    format!("{e:#}").contains("RICH_MESSAGE_PHOTO_NO_MEDIA_FOUND")
}

/// Drain all pending intermediate texts from the streaming state's display
/// queue and send them immediately. Called by the follow-up-question callback
/// BEFORE posting the question message, so the user sees contextual text
/// above the buttons instead of below (race reported in issue #142).
///
/// Applies the same sanitize/redact/dedup/split/send chain as the edit loop.
/// Deliver the final agent response for a turn: marker
/// extraction, sanitize, react directive, dedup, reaction-only handling,
/// folded-final reclaim, footer, rich-first send with HTML fallback,
/// chunked sends, history recording, TTS. Extracted VERBATIM from
/// handle_message (#471 phase 2) — the only edit is early
/// `return Ok(())` becoming `return Ok(false)` so the caller can
/// preserve handle_message's original control flow exactly.
///
/// Shared by live turns and the crash-recovery resume path (#471 phase 3); the
/// only difference is `reply_to`, which is `None` on resume because the original
/// message id is lost across a restart (#771).
/// Background-task indicator for the settled flow footer (#1054): the first
/// task's label when exactly one is running, a count when several, `None`
/// when nothing is detached or no manager is wired (#722). A settled turn
/// that ends with detached work looks identical to a complete one without
/// this, and the typing indicator staying alive is too easy to miss.
///
/// Returns the footer label **and** the numeric count (the settled header
/// needs the number to read "Waiting for N background task(s)" — #1144).
/// Both come from the single `running_tasks(session_id)` read so settle does
/// not hit the manager twice.
pub(crate) fn bg_indicator_for(
    agent: &AgentService,
    session_id: Uuid,
) -> (Option<String>, Option<usize>) {
    let Some(bm) = agent.background_manager() else {
        return (None, None);
    };
    let tasks = bm.running_tasks(session_id);
    match tasks.len() {
        0 => (None, Some(0)),
        1 => (Some(format!("{} running", tasks[0].label)), Some(1)),
        n => (Some(format!("{n} tasks running")), Some(n)),
    }
}

/// Alive sub-agent counts for the settled header (#1183): how many of THIS
/// session's children are still working vs parked awaiting collection. The
/// sub-agent registry is separate from `BackgroundTaskManager`, so the #1144
/// header gate never saw it — a turn ending with agents mid-work still read
/// "✅ Finished". Empty when no manager is wired or every child already
/// terminated; the header then falls back to the background-task-only (or
/// plain Finished) form.
pub(crate) fn subagent_counts_for(
    agent: &AgentService,
    session_id: Uuid,
) -> super::flow::SubagentCounts {
    let Some(mgr) = agent.subagent_manager() else {
        return super::flow::SubagentCounts::default();
    };
    let (working, awaiting) = mgr.alive_counts_for(session_id);
    super::flow::SubagentCounts { working, awaiting }
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn deliver_final_response(
    bot: &Bot,
    chat_id: ChatId,
    // The message this turn answers, for the reaction target. None on the
    // crash-recovery resume path, where the original message id is lost across
    // restarts — reactions strip without firing. Only the ID travels: the chat
    // facts the file-link leg needs come from `chat_id`, so a resumed turn can
    // still address the bubbles it delivered (#771).
    reply_to: Option<MessageId>,
    thread_id: Option<teloxide::types::ThreadId>,
    streaming: &Arc<std::sync::Mutex<StreamingState>>,
    session_id: Uuid,
    agent: &Arc<AgentService>,
    telegram_state: &Arc<TelegramState>,
    channel_msg_repo: &ChannelMessageRepository,
    voice_config: &crate::config::VoiceConfig,
    is_voice: bool,
    is_dm: bool,
    chat_title: &str,
    mut streaming_msg_id: Option<MessageId>,
    result: Result<crate::brain::agent::AgentResponse, crate::brain::agent::AgentError>,
) -> ResponseResult<bool> {
    match result {
        Ok(response) => {
            // Merge candidate captured below: whichever bubble carried the final
            // response (classic HTML edit/send, or table-free rich message).
            // render_suggestions attaches its keyboard to THIS bubble when Some.
            let mut final_bubble: Option<super::state::MergeBubble> = None;
            // Collect the reply's image references — `<<IMG:path>>` markers,
            // local markdown links, and REMOTE links — into one attachment
            // list. Remote targets are fetched here, so a link the model wrote
            // ships as native media instead of arriving as bare markdown
            // (#286).
            let image_cwd = agent.get_working_directory_for_session(session_id);
            // #465: the video family reads the SAME two reference forms the
            // image family does (`<<VID:…>>` and the markdown reference), so
            // exactly one of them must own each reference — the invariant the
            // image plane's own docs state. The video scan runs FIRST and
            // consumes what it claims, so the image scan below never sees a
            // reference the video plane is about to deliver or report. Without
            // this order the image plane classifies a video's bytes as an
            // unsupported image and answers the same reference with a second,
            // false notice ("Image not attached") on the turn the clip arrives.
            let video_scan =
                crate::utils::extract_local_videos(&response.content, Some(image_cwd.as_path()));
            let image_scan = crate::utils::resolve_remote_images(
                crate::utils::extract_local_images(&video_scan.text, Some(image_cwd.as_path())),
            )
            .await;
            // #1916: the file scan runs LAST, on the text the image plane left
            // behind, so a reference one plane already claimed is never seen by
            // this one — the same one-owner-per-reference invariant the video
            // leg states above. #1918 gives the family the rich-plane TWIN it
            // lacked — the `tg://document` media kind — so the #487
            // plane-ownership split now applies to it exactly as it does to the
            // other two: the rich plane inlines the document AT its reference,
            // and the send floor below is gated on that ownership. This scan
            // still owns the HTML plane: the `📎 <label>` marker the reader
            // falls back to, and the failure list that notice names.
            let file_scan = crate::utils::extract_local_files(
                &image_scan.text,
                Some(image_cwd.as_path()),
            );
            // `text_only` is the FILE scan's output: it ran last, on the text
            // the image plane already emptied, so it is the only one that has
            // seen every reference. Taking the image scan's text here would
            // leave each delivered link in the body AND ship it as a document.
            // The scan itself outlives this line (#1918): its `attachments`
            // carry each marker's span and its `text` is the buffer those spans
            // index into, so the link pass at the file floor below needs the
            // whole record, not one field of it. The three reads that follow
            // take copies for that reason — each is a handful of small values.
            let (text_only, img_paths) = (file_scan.text.clone(), image_scan.attachments);
            // A picture this turn already delivered as a promoted intermediate
            // must not ship twice (#502). Filtering the ATTACHMENT list — not
            // the text — is what makes the final leg ship exactly one photo
            // when the closing answer repeats the reference: the text still
            // dedups by the existing normalized-text rule, untouched.
            let img_paths: Vec<crate::utils::image::LocalImage> = {
                let delivered = {
                    let s = streaming.lock().unwrap_or_else(|e| e.into_inner());
                    s.delivered_image_paths.clone()
                };
                img_paths
                    .into_iter()
                    .filter(|i| !delivered.contains(&i.path))
                    .collect()
            };
            // References that never became attachments: rejected local paths,
            // failed downloads, and — appended to below — images the channel
            // itself refused. Drives the honest notice and the regen nudge.
            let mut image_failures: Vec<LocalImageFailure> = image_scan.failures;
            // #1916: the same #502 dedup for files. `delivered_image_paths` is
            // the turn's delivered-MEDIA list — a path is either already in the
            // chat or it is not, whichever family put it there — so a file a
            // promoted intermediate already sent is not sent a second time.
            let file_paths: Vec<crate::utils::image::LocalFile> = {
                let delivered = {
                    let s = streaming.lock().unwrap_or_else(|e| e.into_inner());
                    s.delivered_image_paths.clone()
                };
                file_scan
                    .attachments
                    .iter()
                    .filter(|f| !delivered.contains(&f.path))
                    .cloned()
                    .collect()
            };
            // Rejected file links, and — appended to below — files the channel
            // refused. Its OWN vector, not folded into `image_failures`: the
            // notice names the family, so a missing document must not be
            // answered with "Image not attached".
            let mut file_failures: Vec<LocalImageFailure> = file_scan.failures.clone();
            // #465: the same #502 rule for video. `delivered_image_paths` is the
            // turn's delivered-MEDIA list — a path is either already in the chat
            // or it is not, whichever family put it there — so video rides the
            // same one, and a clip a promoted intermediate already sent is not
            // sent a second time here.
            let vid_paths: Vec<crate::utils::image::LocalVideo> = {
                let delivered = {
                    let s = streaming.lock().unwrap_or_else(|e| e.into_inner());
                    s.delivered_image_paths.clone()
                };
                video_scan
                    .attachments
                    .into_iter()
                    .filter(|v| !delivered.contains(&v.path))
                    .collect()
            };
            // Rejected video references, and — appended to below — clips the
            // channel refused. Its OWN vector, not folded into `image_failures`:
            // the notice names the family, so a missing clip must not be
            // answered with "Image not attached".
            let mut video_failures: Vec<LocalImageFailure> = video_scan.failures;
            // Strip LLM-hallucinated artifacts (<!-- tools-v2 -->, XML tool blocks)
            let text_only = crate::utils::sanitize::strip_llm_artifacts(&text_only);
            let text_only = redact_secrets(&text_only);

            // #487: the rich plane rebuilds a local image reference IN PLACE —
            // a `tg://photo?id=imgN` reference at the original offset, its bytes
            // in the media array — so the picture lands where it was written and
            // Telegram renders its markdown title as the caption. The extraction
            // leg does neither: it tears the reference out and ships the file as
            // a standalone bubble, detached from the text that introduced it.
            // Exactly ONE leg may own a reply's images, so the decision is made
            // here, once, from the same rewrite the rich send will carry. The
            // bytes are read here rather than at the send so a read that fails
            // between validation and delivery joins the honest notice below.
            // The rich plane is fed from the RAW content, so the
            // `<<react:emoji>>` directive the text plane strips below still
            // rides here. Stripped only there, it is delivered as literal
            // text on every rich send — which is what an image-bearing
            // react turn takes (#487), so strip it for this plane too.
            let rich_source = crate::utils::extract_react_marker(&response.content).0;
            let rich_source =
                redact_secrets(&crate::utils::sanitize::strip_llm_artifacts(&rich_source));
            let (rich_rw, rich_vw, rich_fw) = {
                let delivered = {
                    let s = streaming.lock().unwrap_or_else(|e| e.into_inner());
                    s.delivered_image_paths.clone()
                };
                // #465: the VIDEO family walks first, and the image family
                // runs on the video family's rich form. The order is
                // load-bearing and it is the order the text floor below already
                // uses (`extract_local_videos` then `extract_local_images`):
                //
                //  * Both families read the markdown reference form, and the
                //    image walk CONSUMES a reference whose bytes fail image
                //    validation (`Rewriter::file` pushes the failure and
                //    returns `true`). A clip is exactly that — `UnsupportedFormat`
                //    — so an image-first order eats a `![clip](x.mp4)` reference
                //    before the video family can claim it: the clip is never
                //    delivered and the reader is told "Image not attached"
                //    instead. Claiming it here means the image walk never sees
                //    it.
                //  * The video walk is the SELECTIVE one — it declines a
                //    reference that is not video-ish by content, and leaves
                //    every `<<IMG:…>>` marker, picture reference and `tg://`
                //    ref verbatim — so running it first cannot starve the image
                //    family of anything it owns.
                //
                // Running the image walk on `vw.rich` (not `vw.stripped`) keeps
                // the rich body whole: a `![video](tg://video?id=vidN)`
                // reference classifies as a media ref, so the image walk leaves
                // it verbatim and BOTH families' references survive in
                // `rich_rw.rich` for the media array to answer. `vid` is its own
                // id prefix: entries are matched to references BY ID inside one
                // message's media array, so a shared prefix would let an image
                // entry answer a video reference.
                let vw = crate::utils::image::rewrite_local_videos(
                    &rich_source,
                    Some(image_cwd.as_path()),
                    crate::utils::VID_ID_PREFIX,
                    &delivered,
                );
                let rw = crate::utils::image::rewrite_local_images(
                    &vw.rich,
                    Some(image_cwd.as_path()),
                    "img",
                    &delivered,
                );
                // #1918: the FILE walk runs LAST, on the image family's rich
                // form — the same one-owner-per-reference order the text floor
                // uses (`extract_local_videos` → `extract_local_images` →
                // `extract_local_files`). It must run here rather than over
                // `rich_source` for the same reason the image walk runs on
                // `vw.rich`: a reference an earlier family already claimed must
                // not be judged twice. A `![pic](tg://photo?id=img0)` reference
                // the image walk produced classifies as a media ref, so this
                // walk leaves it verbatim and the two families' references both
                // survive in `rich_fw.rich` for the media array to answer.
                // `doc` is its own id prefix for the reason `vid` is: entries
                // are matched to references BY ID inside one message's media
                // array, so a shared prefix would let a document entry answer a
                // picture reference.
                let fw = crate::utils::image::rewrite_local_files(
                    &rw.rich,
                    Some(image_cwd.as_path()),
                    crate::utils::DOC_ID_PREFIX,
                    &delivered,
                    // #1968: the markdown opt-out is applied INSIDE the walk, so
                    // a `.md` reference comes back as the plain marker and no
                    // entry is recorded for it — which is what keeps the rich
                    // plane from inlining it and hands it to the floor below.
                    crate::config::Config::current()
                        .channels
                        .telegram
                        .inline_markdown_documents,
                );
                (rw, vw, fw)
            };
            // #487/#465: images and videos share ONE media array, so the entries
            // are built into the same vector and the ownership decisions below
            // are read off it.
            let (mut rich_media, failures, _) = read_media_entries(
                &rich_rw.entries,
                MediaKind::Photo,
                "local image for the rich plane",
                |_| None,
            )
            .await;
            image_failures.extend(failures);
            // #465: video entries ride the same array, each carrying its own
            // kind so the builder emits the string that matches the bytes. A
            // read that fails here joins the VIDEO refusal list, not the image
            // one — the notice names the family, and this is a clip.
            let (media, failures, _) = read_media_entries(
                &rich_vw.entries,
                MediaKind::Video,
                "local video for the rich plane",
                |_| None,
            )
            .await;
            rich_media.extend(media);
            video_failures.extend(failures);
            // #1918: the documents ride the SAME array, each carrying its own
            // kind so the builder emits the string that matches the bytes. The
            // entry's part name is the file's OWN name (Step 1's
            // `document_part_name`), because bytes alone cannot recover it and
            // Telegram shows the part name as the file name in the chat — the
            // bare `file` the probe found on bubble `91048`. A read that fails
            // here joins the FILE refusal list, not the image one: the notice
            // names the family, and this is a document.
            //
            // The reference that answers these entries is `tg://document?id=docN`,
            // which `rich_fw.rich` carries and the text plane's marker does not:
            // this is the leg that renders the document AT the reference instead
            // of as a detached bubble, which is the whole point of the kind.
            // #1968: with `inline_markdown_documents` off the FILE WALK has
            // already left every markdown document as a plain marker and
            // recorded no entry for it, so nothing here needs to filter — this
            // carries exactly the documents the rich plane owns, and the
            // detached file floor delivers the ones it does not.
            let (media, failures, _) = read_media_entries(
                &rich_fw.entries,
                MediaKind::Document,
                "local file for the rich plane",
                |entry| Some(document_part_name(&entry.file.path)),
            )
            .await;
            rich_media.extend(media);
            file_failures.extend(failures);
            // #487: an image-only body takes the rich plane too. The rich send
            // carries the REWRITTEN markdown, so the reference itself is the
            // body's content — there is no "text bubble" needed to carry it,
            // and nothing is dropped: measured against the live API, an
            // image-only body renders inline WITH its caption. The extraction
            // leg below remains the floor when the rich send fails.
            //
            // #465: video rides the same decision, but the two families keep
            // SEPARATE ownership flags and the rich send is gated on their
            // union. The array is shared, so `!rich_media.is_empty()` alone
            // would make a video-only body claim to own IMAGES and suppress the
            // image floor for a reply that has none — and symmetrically, an
            // image-only body would suppress the video floor. The floors below
            // are per-family; only the send is joint.
            let rich_plane_ok = super::rich::should_send_native_rich_for_media(&rich_source, true);
            let ownership = super::rich::mermaid::rich_media_ownership(rich_plane_ok, &rich_media);
            let rich_owns_images = ownership.images;
            let rich_owns_videos = ownership.videos;
            let rich_owns_documents = ownership.documents;
            let rich_owns_media = ownership.any();

            // Drop an echoed plan title (#837). The reminder shows the model
            // the title every turn and it opens by repeating it, directly
            // under the card that already renders it.
            //
            // Covers Editing as well as Active: #621 folded title and prose
            // into the card in BOTH states, so limiting this to Active left
            // the duplicate visible for every plan still being drafted.
            let text_only = match crate::utils::plan_files::load_plan(session_id).await {
                Some(plan) if !plan.title.trim().is_empty() => {
                    strip_echoed_plan_title(&text_only, &plan.title)
                }
                _ => text_only,
            };

            // Extract <<react:emoji>> directive — the LLM outputs this to
            // signal a reaction-only response (no text bubble). If the
            // response is ONLY a reaction, the emoji is sent as a Telegram
            // reaction on the user's message and text delivery is skipped.
            let (text_only, react_emoji) = crate::utils::extract_react_marker(&text_only);

            // Dedup: strip text that was already sent as intermediate messages
            // to avoid duplicating content on Telegram. An intermediate chunk
            // that already carries the final answer (e.g. "Done. Uploaded to
            // Drive: https://…") will otherwise be repeated when the
            // streaming placeholder is edited with the final response.
            // Intermediates stay visible as-is; only the streaming
            // placeholder's final text is pruned.
            // The bubble TEXTS, for the comparison below. The ids a bubble
            // occupies are read separately at the supersession site (#620), so
            // the two never have to be index-aligned by hand.
            let sent = {
                let s = streaming.lock().unwrap_or_else(|e| e.into_inner());
                s.sent_bubbles
                    .iter()
                    .map(|b| b.text.clone())
                    .collect::<Vec<_>>()
            };
            tracing::info!(
                "Telegram dedup: response.content len={}, sent_bubbles count={}",
                text_only.len(),
                sent.len(),
            );
            let pre_dedup_text = text_only.clone();
            // Whitespace-normalized comparison. The predicate has ONE home
            // (#620) — `normalize_for_dedup` — shared with the supersession
            // cleanup below: if the two arms normalized differently, one could
            // delete a bubble the other never matched, which is the same silent
            // loss this fix removes.
            let mut suppressed_final = false;
            let text_only = if !sent.is_empty() {
                let norm_final = super::intermediates::normalize_for_dedup(&text_only);
                if sent
                    .iter()
                    .any(|i| super::intermediates::normalize_for_dedup(i) == norm_final)
                {
                    tracing::info!(
                        "Telegram dedup: match found among {} intermediates (normalized) — suppressing final response",
                        sent.len()
                    );
                    suppressed_final = true;
                    String::new()
                } else {
                    text_only
                }
            } else {
                text_only
            };

            // Reclaim a folded answer BEFORE the reaction decision (#478):
            // the completion can stream mid-turn as IntermediateText and
            // fold into the flow block. When the final text is empty, the
            // trailing folded run IS the answer — pulling it out here means
            // a closing react directive becomes a reaction ON TOP of the
            // delivered completion, instead of the reaction-only skip
            // imprisoning the answer inside the Processing log block.
            //
            // UNLESS the final was just suppressed by dedup (#1152): the
            // trailing folded run then belongs to the SAME answer, which
            // already went out as intermediate bubbles. Reclaiming it here
            // re-ships an orphan duplicate fragment (62-char tail under the
            // full answer). `folded_duplicates_final` cannot arbitrate this:
            // streaming may leave a SUFFIX chunk folded in the block, and
            // that predicate deliberately only matches prefix overlap. So
            // when suppression happened, strip the folded copy from the
            // block silently — it is already delivered.
            //
            // #31: tracks whether THIS block already reclaimed the answer, so
            // the main reclaim below can tell "nothing left to reclaim" from
            // "the reclaim found nothing that was ever there" in its K warn.
            let mut pre_reclaimed = false;
            // `mut` so the image-delivery notice below can append to the final
            // text once the sends have actually been attempted (#286).
            let mut text_only = if text_only.trim().is_empty() {
                if suppressed_final {
                    let (discarded, discarded_trailer) =
                        take_folded_final(bot, chat_id, streaming, options_pending(streaming))
                            .await;
                    if discarded.is_some() || discarded_trailer.is_some() {
                        tracing::info!(
                            "Telegram: final suppressed by dedup — dropping {}(+{}) folded \
                             chars already delivered as intermediates (#1152)",
                            discarded.as_ref().map(|t| t.len()).unwrap_or(0),
                            discarded_trailer.as_ref().map(|t| t.len()).unwrap_or(0),
                        );
                    }
                    text_only
                } else {
                    let (reclaimed, trailer) =
                        take_folded_final(bot, chat_id, streaming, options_pending(streaming))
                            .await;
                    if let Some(t) = trailer {
                        let trailer_len = t.len();
                        // #31: the post-halt sign-off rides AFTER the buttons —
                        // stash it for render_suggestions (keep-never-discard).
                        streaming
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .pending_trailer = Some(t);
                        tracing::info!(
                            "Telegram: stashed {} trailer chars reclaimed before the react \
                             decision (#31)",
                            trailer_len
                        );
                    }
                    match reclaimed {
                        Some(reclaimed) => {
                            pre_reclaimed = true;
                            tracing::info!(
                                "Telegram: reclaimed folded final ({} chars) before react \
                                 decision (#478)",
                                reclaimed.len()
                            );
                            reclaimed
                        }
                        None => text_only,
                    }
                }
            } else {
                text_only
            };

            // Reaction directive: if the LLM included <<react:emoji>>, send
            // a reaction on the user's message. For reaction-only responses
            // (empty text after stripping the directive), skip all text/TTS
            // delivery and just react — but ONLY when the turn did no tool
            // work (#439): a turn that executed tools and ended with a bare
            // reaction dropped its whole completion (issues were closed and
            // commented, the user saw only 🔥). For a work turn, empty final
            // text is a failure mode, never a deliberate ack.
            let turn_ran_tools = {
                let s = streaming.lock().unwrap_or_else(|e| e.into_inner());
                !s.tool_msgs.is_empty()
            };
            if let Some(ref emoji) = react_emoji {
                let mapped = map_to_allowed_reaction(emoji);
                let reaction = teloxide::types::ReactionType::Emoji {
                    emoji: mapped.clone(),
                };
                super::telemetry::log_request(
                    "turn",
                    "delivery reaction",
                    &session_id.to_string(),
                    "reaction",
                    "setMessageReaction",
                    chat_id.0,
                    None,
                    reply_to.map(|id| i64::from(id.0)),
                );
                let react_result = match reply_to {
                    Some(id) => bot
                        .set_message_reaction(chat_id, id)
                        .reaction(vec![reaction])
                        .is_big(false)
                        .await
                        .map(|_| ()),
                    // Resume path: the original message is gone, nothing to
                    // react to — treat as delivered so no fallback fires.
                    None => Ok(()),
                };
                if let Err(ref e) = react_result {
                    tracing::warn!("Telegram: failed to set reaction ({mapped}): {}", e);
                }
                // `!suppressed_final` (#1152): a final suppressed by dedup was
                // not dropped — it shipped early as intermediate bubbles. That
                // is delivery, not the failure mode #439 guards against, so no
                // synthetic "Done — X/Y tool calls" summary on top of it.
                // `!rich_owns_images` (#487): an image-bearing react turn is
                // not summary-only. Returning here would drop the picture
                // before the rich plane can rebuild it, and the image IS the
                // answer — there is no prose summary to lose.
                if text_only.trim().is_empty()
                    && turn_ran_tools
                    && !suppressed_final
                    && !rich_owns_images
                {
                    // Work turn with no completion text (#439): the model
                    // replaced its summary with a reaction. Deliver a
                    // fallback completion so the work is reported — the
                    // reaction already landed above.
                    tracing::warn!(
                        "Telegram: turn executed tools but produced no completion text — \
                         delivering fallback summary instead of reaction-only skip (#439)"
                    );
                    let fallback = {
                        let s = streaming.lock().unwrap_or_else(|e| e.into_inner());
                        let done = s
                            .tool_msgs
                            .iter()
                            .filter(|t| t.completed == Some(true))
                            .count();
                        format!(
                            "Done — {done}/{} tool calls completed. (The model ended the turn \
                             without a summary; see the log above for what ran.)",
                            s.tool_msgs.len()
                        )
                    };
                    if let Err(e) = message_in_thread(bot, chat_id, thread_id, &fallback).await {
                        tracing::error!("Telegram: fallback completion send failed: {}", e);
                    }
                    if let Some(mid) = streaming_msg_id {
                        best_effort_delete(bot, chat_id, mid, "fallback send cleanup").await;
                    }
                    return Ok(false);
                }
                // React-only means exactly what the prompt says it means: ONLY
                // the directive, nothing after it. The contract taught to the
                // model (handler.rs, "Reaction directive") states both shapes:
                //
                //   react-only        -> output ONLY the directive
                //   react AND respond -> directive at the START, then the text
                //
                // so text after the directive IS the documented second shape,
                // and delivering it is the contract being honoured rather than
                // violated.
                //
                // #928 suppressed that text whenever the turn ran no tools, on
                // the theory that a react turn has no answer in it and anything
                // present must be reasoning spilled into the content channel.
                // The tool state was standing in for "is this reasoning", and
                // it does not answer that question: a turn that answers from
                // analysis alone runs no tools, which is the ordinary shape of
                // an explanation or a follow-up. #1009 is four consecutive
                // completions destroyed that way, none of which contained any
                // reasoning at all.
                //
                // Reasoning that reaches the content channel is a provider-side
                // defect (#760) and belongs where it originates. Delivery must
                // not delete a completion on suspicion, because that failure is
                // silent and total while a leak is merely ugly.
                //
                // `!suppressed_final` (#1152, fork #293): a final suppressed by
                // dedup was already delivered as intermediate bubbles. That is
                // delivery, not a react-only turn — do not trigger the
                // react-only early return or the spurious #546 incomplete-turn
                // notice on top of delivered work.
                // `!rich_owns_images` (#487): a react turn that carries an
                // image has content to deliver, so it is not react-only in
                // spirit — falling through lets the rich plane rebuild it.
                if is_react_only_turn(suppressed_final, &text_only) && !rich_owns_images {
                    // Never-silent guard (#353): a reaction-only turn whose
                    // reaction FAILED must degrade to text, not to nothing.
                    if react_result.is_err() {
                        tracing::warn!(
                            "Telegram: reaction-only turn with failed reaction — \
                             delivering the emoji as text instead"
                        );
                        if let Err(e) =
                            message_in_thread(bot, chat_id, thread_id, emoji.as_str()).await
                        {
                            tracing::error!("Telegram: emoji text fallback also failed: {}", e);
                        }
                    } else {
                        // A genuine ack (praise, "got it") completes in seconds.
                        // A react-only turn that instead took MINUTES engaged
                        // with real work, ran no tools, and then bailed with
                        // only a reaction — a dropped request, not an ack (#546:
                        // "executing a task for 5m, it reacts and just stops").
                        // #439 only guards the tools-ran case; this catches the
                        // reasoned-but-no-tools case. Delivery can't re-run the
                        // model, so surface it as an incomplete turn instead of
                        // a silent drop, so the user knows to re-send.
                        let elapsed = {
                            let s = streaming.lock().unwrap_or_else(|e| e.into_inner());
                            s.turn_started_at.elapsed()
                        };
                        // This branch is now reached only with an empty content
                        // channel, so the turn produced a reaction and nothing
                        // else. #928's carve-out for prose landing here is gone
                        // with the suppression it guarded (#1009): prose no
                        // longer routes into this branch at all, it is
                        // delivered.
                        if elapsed >= std::time::Duration::from_secs(60) {
                            tracing::warn!(
                                "Telegram: react-only after {}s with no tools and no text — \
                                 surfacing as an incomplete turn, not a silent drop (#546)",
                                elapsed.as_secs()
                            );
                            let notice = "⚠️ I ended that turn with only a reaction and did not \
                                          complete the request. Please re-send it if you needed \
                                          me to act.";
                            if let Err(e) = message_in_thread(bot, chat_id, thread_id, notice).await
                            {
                                tracing::error!(
                                    "Telegram: #546 incomplete-turn notice failed: {}",
                                    e
                                );
                            }
                        } else {
                            tracing::info!(
                                "Telegram: reaction-only response ({}), skipping text delivery",
                                mapped
                            );
                        }
                    }
                    // A react-only turn ran no tools (the tools-with-no-text
                    // case returned above, #439), so any open processing-log
                    // block is header-only — the model's thinking preview plus
                    // persistent plan chrome. Left up, it reads as a "Processing
                    // log" bubble with no answer, i.e. a dropped request (#544).
                    // Remove it and clear its state so ONLY the reaction remains;
                    // the plan state persists independently (/show-plan still
                    // works).
                    let flow_mid = {
                        let mut s = streaming.lock().unwrap_or_else(|e| e.into_inner());
                        s.flow_entries.clear();
                        s.open_group_msg_id.take()
                    };
                    if let Some(mid) = flow_mid {
                        best_effort_delete(bot, chat_id, mid, "flow teardown").await;
                        // #1377: the card is gone — drop the fold handle so a
                        // later completion falls back to the bubble lane
                        // instead of editing a deleted message.
                        telegram_state.clear_flow_state(session_id).await;
                    }
                    if let Some(mid) = streaming_msg_id {
                        best_effort_delete(bot, chat_id, mid, "streaming teardown").await;
                    }
                    return Ok(false);
                }
            }

            // Context budget footer is display-only chrome: it rides the
            // settled flow message as a section, never the final answer, and
            // is NOT stored in the session/messages table or used for TTS.
            let ctx_max = agent.context_limit_for_session(session_id);
            let footer = crate::utils::format_ctx_footer(
                response.context_tokens,
                ctx_max,
                response.tokens_per_second,
            );
            // Quiet ❕ while the #909 pressure hint is active (#29): the
            // settled footer mirrors the nudge that's in the prompt. Cleared
            // on compaction success; re-arms below the 55% floor.
            let footer = if !footer.is_empty() && agent.pressure_warning_active(session_id) {
                format!("{footer} ❕")
            } else {
                footer
            };
            {
                let mut s = streaming.lock().unwrap_or_else(|e| e.into_inner());
                s.sections.ctx = (!footer.is_empty()).then(|| footer.clone());
            }

            // Send each attachment through the SHARED loop (#502), so the
            // promotion fallback and this leg cannot drift on kind selection,
            // captions, failure reasons or telemetry. The paths it reports as
            // delivered are already filtered above (a picture the promotion
            // path put in the chat is not re-sent here), so the first element
            // is deliberately ignored — only the channel's own refusals join
            // the notice.
            // #487: when the rich plane owns this reply's images it rebuilds
            // each reference in place, so this leg must NOT also send them —
            // that would ship every picture twice, one copy of it detached.
            if !rich_owns_images {
                let (_, refused) =
                    send_local_images(session_id, bot, chat_id, thread_id, &img_paths).await;
                image_failures.extend(refused);
            }
            // #465: the video floor, symmetric to the image one above and
            // gated on its OWN flag. A rich plane that owns this reply's images
            // has said nothing about its videos, and vice versa — the two lists
            // are separate all the way down, so a clip must not be suppressed
            // because a picture was inlined.
            if !rich_owns_videos {
                let (_, refused) =
                    send_local_videos(session_id, bot, chat_id, thread_id, &vid_paths).await;
                video_failures.extend(refused);
            }
            // #1918: the file floor, symmetric to the image and video floors
            // above and gated on its OWN flag. A document-bearing body the rich
            // plane owns carries the file inline AT its reference, so this leg
            // must NOT also send it — that would ship every document twice, one
            // copy of it detached from the text that introduced it. The floor is
            // a document's only delivery leg when the rich plane does not own it
            // (the flag off, or a body the rich plane declines), and `delivered`
            // is then empty because nothing was sent.
            //
            // #1968: the suppression is now per-FILE, not per-family. With
            // `inline_markdown_documents` off a markdown document is deliberately absent
            // from the rich plane's entries, so a reply that also carries, say,
            // a PDF would read `rich_owns_documents == true` and this leg would
            // ship nothing — leaving the markdown file with no delivery leg at
            // all. The floor therefore sends exactly the files the rich plane
            // did NOT take. Normally the two sets coincide (the plane owns all
            // of them or none), so every other kind behaves as before.
            let files_the_rich_plane_skipped: Vec<crate::utils::image::LocalFile> =
                if rich_owns_documents {
                    file_paths
                        .iter()
                        .filter(|f| !rich_fw.entries.iter().any(|e| e.file.path == f.path))
                        .cloned()
                        .collect()
                } else {
                    Vec::new()
                };
            let (delivered, refused) = if rich_owns_documents {
                if files_the_rich_plane_skipped.is_empty() {
                    (Vec::new(), Vec::new())
                } else {
                    send_local_files(
                        session_id,
                        bot,
                        chat_id,
                        thread_id,
                        &files_the_rich_plane_skipped,
                    )
                    .await
                }
            } else {
                send_local_files(session_id, bot, chat_id, thread_id, &file_paths).await
            };
            file_failures.extend(refused);
            // #1918: now that each document has a bubble of its own, the marker
            // standing in for it can point at that bubble — where the chat has a
            // message-link form at all. That is a property of the CHAT ID, not of
            // the message that happened to arrive, so a resumed turn builds the
            // same links a live one does (#771): the old `inbound` gate made a
            // fact about the chat hostage to a fact about the message. A path
            // absent from `delivered` is absent here too, so a refused file
            // keeps its plain marker.
            let mut file_links: Vec<(std::path::PathBuf, String)> =
                delivered_file_links(&delivered, chat_id.0, thread_id);
            // #1939: a document delivered by a promoted INTERMEDIATE is in the
            // chat but absent from `delivered` — the rich plane owned it, so
            // this floor sent nothing and there is no id here to build a link
            // from. The intermediate's own bubble IS its address, and its
            // `delivered_files` records the pair. Merged into the SAME list so
            // the one `link_file_markers` pass below links both origins by one
            // rule. A path the final leg DID send already has its own (more
            // precise) id, so it is skipped here rather than overwritten.
            // #771: no `inbound` gate — the link form comes from `chat_id`, so
            // a resumed turn recovers its intermediate links too.
            let from_intermediates = {
                let s = streaming.lock().unwrap_or_else(|e| e.into_inner());
                super::intermediates::intermediate_file_links(&s.sent_bubbles, &pre_dedup_text)
            };
            for (path, message_id) in from_intermediates {
                if file_links.iter().any(|(p, _)| p == &path) {
                    continue;
                }
                if let Some(link) = file_message_link(chat_id.0, thread_id, message_id) {
                    file_links.push((path, link));
                }
            }
            // Rewriting `text_only` here reaches both planes that derive from it
            // below: the HTML edit/send and the mermaid path. The rich body does
            // NOT derive from it (it is `rich_fw.rich`), which carries the same
            // anchors in their inline form. When the rich plane owns the
            // documents this floor sends nothing, so `delivered` is empty — but
            // #1939 fills `file_links` from the intermediate that DID deliver
            // them, so the marker still points at the bubble that holds the
            // document. Only where no such address exists (a DM, or an
            // intermediate that delivered nothing) does the call leave the plain
            // marker, which is honest: the marker names the document and the
            // reader's own bubble sits directly below it.
            text_only = link_file_markers(&text_only, &file_scan, &file_links);

            // An image the reply announced must not vanish silently when the
            // send fails: the reply says plainly which one is missing. This is
            // the honest floor — the post-delivery re-entry ladder (#286) may
            // replace it with a model-authored line where a budget is available.
            text_only = crate::utils::append_failure_notice(&text_only, &image_failures);
            // #465: and the same honest floor for a clip, under its own noun.
            // Folding video refusals into the image notice would tell the user
            // "Image not attached" about a video that is missing (#502's
            // failure mode, one media family over).
            text_only = crate::utils::append_video_failure_notice(&text_only, &video_failures);
            // #1916: and the honest floor for a document, under its own noun.
            // Folding file refusals into the image notice would tell the user
            // "Image not attached" about a PDF that is missing — #502's failure
            // mode, one media family over.
            text_only = crate::utils::append_file_failure_notice(&text_only, &file_failures);

            // Rich fallback: when all content was sent as HTML intermediates
            // during streaming, the dedup step strips text_only to empty. If
            // the original response had rich structure (tables, headings,
            // lists), replace the HTML intermediates with a single native rich
            // message so Telegram renders proper tables and blocks.
            // #46: this arm decides on `pre_dedup_text` BEFORE the options
            // reclaim below can restore the host, so it must consult
            // options_pending itself — same gate as the final-answer site
            // (#45) — or a fully-deduped buttons turn stays plain.
            // #1939: does an intermediate that ALREADY carries this exact body
            // hold it in a bubble that cannot be deleted? A bubble that carried
            // media records EMPTY ids (#617), so the delete set below cannot
            // select it — nothing would be deleted, and the fallback's re-send
            // would leave the reader with the body twice while orphaning the
            // `📎` markers (the documents rode the intermediate, so the second
            // copy's markers point at a bubble that holds nothing). Skipping the
            // arm keeps the intermediates the reader already has; `text_only` is
            // empty here, so no reply text is lost — only the duplicate.
            let fallback_dup = {
                let s = streaming.lock().unwrap_or_else(|e| e.into_inner());
                super::intermediates::fallback_would_duplicate(&s.sent_bubbles, &pre_dedup_text)
            };
            let text_only = if text_only.is_empty()
                && !sent.is_empty()
                && !fallback_dup
                && super::rich::should_send_native_rich_for(
                    &pre_dedup_text,
                    options_pending(streaming),
                ) {
                // #1918: the rich body does not derive from `text_only`, so the
                // marker rewrite the floor applied there is applied here too —
                // same spans, same links, and the same buffer those spans index
                // into, which is what the rewrite checks before it cuts.
                let rich_md = link_file_markers(&pre_dedup_text, &file_scan, &file_links);
                // Deliberately NOT guarded (#500): this arm REPLACES the
                // intermediates it deletes, so a suppression here would answer
                // with the id of an intermediate the block below then deletes,
                // losing the content. #620 makes that premise true BY
                // CONSTRUCTION: the delete set below is exactly the bubbles
                // whose text this message re-sends, so it cannot leave a
                // duplicate behind. Bubbles whose text this message does NOT
                // carry are now spared — they hold the only copy of that prose,
                // and deleting them was the silent loss #620 fixes.
                match super::rich::send_rich_with_mermaid_id(
                    bot.api_url().as_str(),
                    bot.token(),
                    chat_id.0,
                    thread_id,
                    &rich_md,
                    None,
                    "turn",
                    "-",
                )
                .await
                {
                    Ok(rich_msg_id) => {
                        // Delete ONLY the intermediates this rich message
                        // re-sends (#620). The former code deleted every id it
                        // had recorded for the turn, on the premise that the
                        // rich message supersedes all of them — it supersedes
                        // the bubbles whose TEXT it actually repeats. A
                        // narration bubble it does not repeat is the only copy
                        // of that prose.
                        let superseded = {
                            let s = streaming.lock().unwrap_or_else(|e| e.into_inner());
                            super::intermediates::superseded_ids(&s.sent_bubbles, &rich_md)
                        };
                        for mid in &superseded {
                            best_effort_delete(bot, chat_id, *mid, "intermediate cleanup").await;
                        }
                        tracing::info!(
                            "Telegram: rich fallback delivered ({} chars), deleted {} superseded HTML intermediates",
                            rich_md.len(),
                            superseded.len()
                        );
                        // Merge candidate (#tg-suggest-merge): every rich
                        // bubble can carry the suggestion controls now —
                        // table-bearing answers ride the markdown plane,
                        // whose server-side render keeps tables intact
                        // (#79 piece 4; the html plane flattened them,
                        // ex-upstream adolfousier/opencrabs#679, which is
                        // why they used to be excluded).
                        final_bubble = Some(super::state::MergeBubble {
                            message_id: teloxide::types::MessageId(rich_msg_id),
                            body: super::state::BubbleBody::Markdown(pre_dedup_text.clone()),
                        });
                        // Store bot reply in channel_messages even though
                        // text_only is empty (dedup stripped it). The rich
                        // fallback already sent pre_dedup_text, so the next
                        // turn's recent() query sees the bot's side of the
                        // conversation. Without this, the agent "talks to
                        // itself in the dark" after every rich fallback.
                        if !is_dm {
                            let bot_display_name = telegram_state
                                .bot_username()
                                .await
                                .map(|u| format!("@{}", u))
                                .unwrap_or_else(|| "OpenCrabs".to_string());
                            let thread_id_str = thread_id.map(|t| t.0.to_string());
                            let resolved_topic_name = match &thread_id_str {
                                Some(tid) => channel_msg_repo
                                    .latest_topic_name("telegram", &chat_id.0.to_string(), tid)
                                    .await
                                    .ok()
                                    .flatten(),
                                None => None,
                            };
                            let cm = DbChannelMessage::new(
                                "telegram".to_string(),
                                chat_id.0.to_string(),
                                Some(chat_title.to_string()),
                                "bot:opencrabs".to_string(),
                                bot_display_name,
                                pre_dedup_text.clone(),
                                "text".to_string(),
                                Some(rich_msg_id.to_string()),
                            )
                            .with_thread(thread_id_str, resolved_topic_name);
                            if let Err(e) = channel_msg_repo.insert(&cm).await {
                                tracing::warn!(
                                    "Telegram: rich fallback: failed to record bot reply: {}",
                                    e
                                );
                            }
                        }
                        text_only
                    }
                    Err(e) => {
                        tracing::warn!(
                            "Telegram: rich fallback failed, keeping HTML intermediates: {e}"
                        );
                        text_only
                    }
                }
            } else {
                text_only
            };

            // #300 follow-up: ALWAYS check if the trailing folded text matches the
            // final answer and remove it to prevent duplication. For CLI providers,
            // the final answer arrives as a trailing IntermediateText folded into
            // the collapsed block while response.content comes back empty, so we
            // reclaim it. For other providers (or CLI turns where the answer stayed
            // in content), if the same text ended up both folded and in the final
            // response, remove the folded copy to avoid showing it twice.
            //
            // #31: with options pending the reclaim returns BOTH runs — the
            // answer as host, the post-halt sign-off run as trailer — and
            // `settle_options_reclaim` arbitrates. The options arm runs FIRST:
            // the stock trailing-duplicate strip used to pop the trailer run
            // and drop the host with it (the smoke-v4 abandonment).
            let (text_only, reclaimed_trailer) = if text_only.trim().is_empty() {
                // CLI provider case: no separate answer, reclaim the folded final
                let (host, trailer) =
                    take_folded_final(bot, chat_id, streaming, options_pending(streaming)).await;
                settle_options_reclaim(text_only, host, trailer)
            } else if options_pending(streaming) {
                // #1226 flow-fold (K) + #31 trailer: the turn halted on the
                // suggest_options surface — the substantive pre-options answer
                // is the Text run BEFORE the Tool entry, the sign-off ack the
                // run AFTER it. Reclaim both; the answer becomes the final
                // bubble, the ack rides after the buttons.
                let (host, trailer) = take_folded_final(bot, chat_id, streaming, true).await;
                if host.is_none() && trailer.is_none() && !pre_reclaimed {
                    tracing::warn!(
                        "Telegram: options pending but flow reclaim returned nothing — \
                         answer stuck in flow block (#1226 K)"
                    );
                }
                let (text, trailer) = settle_options_reclaim(text_only, host, trailer);
                if let Some(t) = &trailer {
                    tracing::info!(
                        "Telegram: options reclaim settled — {} trailer chars ride after \
                         the buttons (#31)",
                        t.len()
                    );
                }
                (text, trailer)
            } else {
                // Non-CLI case: check if the trailing folded text matches the final
                // answer and remove it to prevent duplication
                let trailing_matches = {
                    let s = streaming.lock().unwrap_or_else(|e| e.into_inner());
                    // Past any chrome appended after the answer (#1253): a
                    // provider switch landing late must not hide the folded
                    // copy and let it render twice.
                    match last_folded_text(&s.flow_entries) {
                        Some(folded) => folded_duplicates_final(folded, &text_only),
                        None => false,
                    }
                };
                if trailing_matches {
                    // Remove the duplicate from the block
                    take_folded_final(bot, chat_id, streaming, options_pending(streaming)).await;
                    (text_only, None)
                } else {
                    (text_only, None)
                }
            };

            // #31: hand the trailer to render_suggestions — rich merges it as
            // a paragraph after the in-body button rows, classic delivers it
            // as its own bubble. Stashed here so both render sites (handler
            // turn end + resume) pick it up identically.
            if let Some(trailer) = reclaimed_trailer {
                streaming
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .pending_trailer = Some(trailer);
            }

            // #690: re-expand any table the model collapsed onto one line so it
            // renders (native rich or <pre> grid) instead of raw pipes. Applied
            // once here before the rich-vs-HTML branching, so both the
            // should_send_native_rich detection below and the HTML render see the
            // reflowed table. Idempotent on well-formed tables.
            let text_only = super::rich::reflow_collapsed_tables(&text_only);

            // Deliver final response — prefer editing the streaming message in-place
            // to avoid the delete+send race that causes duplicates.
            let html = markdown_to_telegram_html(&text_only);
            // Final answers stay clean prose: the ctx footer lives on the
            // settled flow message, not here.
            let mut display_html = html.clone();
            tracing::info!(
                "Telegram deliver: html.len={}, ctx footer on flow='{}'",
                html.len(),
                footer
            );
            // Telegram message_id of the FINAL reply bubble. Captured across the
            // delivery paths (rich send, in-place edit, chunked send) so we can
            // persist it and later recover the EXACT message a user replies to,
            // instead of guessing "the most recent bot message" (#234 follow-up).
            let mut sent_reply_id: Option<i32> = None;
            // Merge candidate (#tg-suggest-merge): id + exact HTML of the bubble
            // the classic HTML path delivered the final response in. Captured in
            // every success arm below; handed to StreamingState after delivery so
            // suggest_options can attach its keyboard to THIS bubble instead of
            // posting a separate "Suggested next" message. Rich and voice paths
            // deliberately leave it None — their bubbles are not re-editable as
            // plain HTML without breaking their rendering.
            // #487: an image-only body has an EMPTY `display_html` (the
            // reference was lifted into the rewrite), so the rich block must be
            // reachable on the image decision alone — otherwise the rich plane
            // can never own a picture that has no prose around it. #465 widens
            // the same reachability to video: a video-only body has an empty
            // `display_html` too, and gating on the image flag alone would send
            // it down the HTML path where its `tg://video` reference is dead
            // visible markdown. `rich_owns_media` is the union; the per-family
            // flags below still decide what each floor does.
            if !display_html.is_empty() || rich_owns_media {
                // Rich-first delivery: a structured reply (tables / headings /
                // lists / math) is delivered as a native Telegram rich message
                // regardless of length — Telegram renders the raw markdown into
                // real tables and blocks. Edit the streamed placeholder in place
                // if we have one, otherwise send a fresh message. The ctx footer
                // is plain text, appended as-is. On ANY failure we fall through
                // to the HTML chunking path below, so the streaming path is
                // never regressed. Plain prose skips rich entirely so Telegram's
                // parser never reinterprets incidental characters — EXCEPT prose
                // that ends on a suggest_options surface (#45): the tap rewrite
                // preserves the host plane, so button-bearing prose rides rich
                // too and the pick record edits back in rendered form.
                // ex-upstream adolfousier/opencrabs#679: for TABLE
                // messages, skip only the doomed native-BLOCKS
                // attempt — Telegram's InputRichBlock rejects our header/rows/align
                // shape (its schema wants cells/size), so a table always 400s the
                // block send and wastes a round-trip. But the rich-MARKDOWN send
                // renders tables correctly, so route tables straight to it instead
                // of skipping the whole rich branch (which #651 did, sending tables
                // to the HTML path where they showed as bare markup). Non-table
                // rich content still tries blocks first (clean fences) then falls
                // back to markdown.
                let mut delivered_rich = (rich_owns_media
                    || super::rich::should_send_native_rich_for(
                        &text_only,
                        // #45: `options_pending` is true when the turn stashed a
                        // suggest_options set mid-turn (#1226 K helper) — force the
                        // rich plane for prose so buttons never live on a plain host.
                        options_pending(streaming),
                    ))
                    && {
                    // #487: an image-bearing body is sent from the REWRITTEN
                    // markdown, whose `tg://photo` references only resolve
                    // against the media array sent with it; every other body
                    // keeps today's text, byte for byte.
                    // #465/#1918: `rich_fw.rich` is the LAST walk's output —
                    // the video walk's form fed through the image walk, then
                    // through the file walk — so it carries `tg://photo`,
                    // `tg://video` AND `tg://document` references and is the
                    // correct body for any of the three families. It must be
                    // the final walk's form, not a named family's: the earlier
                    // walk's output is missing the later family's references,
                    // and the array below is shared, so a body whose references
                    // are not in it would ship dead markdown. When no family
                    // claimed anything it is byte-identical to `rich_rw.rich`,
                    // which is what the pre-#1918 line sent — so the image and
                    // video planes are unchanged.
                    let rich_md = if rich_owns_media {
                        rich_fw.rich.clone()
                    } else {
                        text_only.clone()
                    };
                    let rich_media: &[super::rich::mermaid::MediaEntry] = if rich_owns_media {
                        rich_media.as_slice()
                    } else {
                        &[]
                    };
                    // Send a FRESH rich message rather than editing the streamed
                    // placeholder into rich. Editing a normal message into a rich
                    // one glitches the client render — overlap during the
                    // transition, and a stale pre-edit (HTML) version after a
                    // refresh / chat switch. A fresh sendRichMessage renders clean.
                    //
                    // Delete the placeholder FIRST so the fresh rich message is
                    // the LAST thing added to the chat — deleting it AFTER the
                    // send pulls the content up and leaves the view mid-chat
                    // instead of scrolling to the bottom on completion. `.take()`
                    // clears the id so the HTML fallback below sends a fresh
                    // message (not an edit of a deleted one) if the rich send fails.
                    if let Some(mid) = streaming_msg_id.take() {
                        best_effort_delete(bot, chat_id, mid, "pre-rich-fallback cleanup").await;
                    }
                    // Native BLOCKS first (#476 path B) for NON-table content: the
                    // block value is sent as-is, so code fences render natively with
                    // no server-side parser to mangle them into <code> artifacts. A
                    // table would only 400 here (schema mismatch), so skip blocks
                    // entirely when one is present and let the markdown send below
                    // render it. On any block rejection we also fall through to
                    // markdown, so worst case is exactly the rich-markdown render.
                    // Straight to rich-markdown (#871). The native-blocks
                    // attempt ran 68 times across two days and returned 400
                    // (RICH_MESSAGE_CONTENT_REQUIRED) all 68 times, never once
                    // succeeding, so every rich message already arrived via the
                    // markdown fallback below. Keeping it cost a guaranteed
                    // round-trip and a guaranteed error before every delivery.
                    //
                    // Markdown is also the only mode that renders tables: the
                    // rich HTML input mode returns 200 and then flattens a table
                    // into a run-on paragraph, which is why telegram_send now
                    // uses this same call rather than its own.
                    {
                        // Rich MARKDOWN renders tables correctly; mermaid fences
                        // are routed to the rich-HTML image path inside the sender
                        // (#1044), everything else stays on markdown.
                        // #tg-mermaid-delivery-hardening: retry once when
                        // Telegram's server-side refetch of the embedded media
                        // (mermaid.ink) died — `RICH_MESSAGE_PHOTO_NO_MEDIA_FOUND`
                        // is a race against a flaky renderer (our resolve ran
                        // seconds earlier), not a structural rejection. The sender
                        // re-resolves media on every call, so the retry naturally
                        // re-fetches from the renderer. Structural 400s are never
                        // retried.
                        let mut rich_send = super::delivery_dedup::send_rich_turn_guarded(
                            session_id,
                            bot,
                            chat_id,
                            thread_id,
                            &rich_md,
                            // #487: empty unless the rich plane owns this
                            // body's images, in which case the references in
                            // `rich_md` resolve against exactly these entries.
                            rich_media,
                        )
                        .await;
                        if rich_send.is_err() && is_no_media_found(rich_send.as_ref().unwrap_err())
                        {
                            tracing::warn!(
                                "Telegram: rich send hit NO_MEDIA_FOUND (renderer flake?) — retrying once"
                            );
                            // Nothing was recorded for the failed attempt, so the
                            // guard lets this retry through (#500).
                            rich_send = super::delivery_dedup::send_rich_turn_guarded(
                                session_id,
                                bot,
                                chat_id,
                                thread_id,
                                &rich_md,
                                rich_media,
                            )
                            .await;
                        }
                        match rich_send {
                            Ok(id) => {
                                // Success was silent, which is why an
                                // unformatted table could not be traced (#860).
                                tracing::info!(
                                    "Telegram: rich markdown delivered as msg {id} ({} chars)",
                                    rich_md.len()
                                );
                                sent_reply_id = Some(id);
                                // Merge candidate (#tg-suggest-merge): the
                                // controls can ride this bubble — table-bearing
                                // answers included: the merge edit goes back out
                                // on the markdown plane, whose server-side
                                // render keeps tables intact (#79 piece 4; the
                                // old html-plane merge flattened them,
                                // ex-upstream adolfousier/opencrabs#679, which is
                                // why tables used to be excluded).
                                final_bubble = Some(super::state::MergeBubble {
                                    message_id: teloxide::types::MessageId(id),
                                    body: super::state::BubbleBody::Markdown(rich_md.clone()),
                                });
                                true
                            }
                            Err(e) => {
                                tracing::warn!("Telegram: rich delivery failed, using HTML: {e}");
                                false
                            }
                        }
                    }
                };

                if !delivered_rich {
                    // #487: the rich plane owned this body's images and did not
                    // deliver them. The extraction leg is the floor — without it
                    // the picture would vanish with no notice at all, which is
                    // exactly the #502 failure mode (silent in both directions).
                    // Its notice joins the fallback HTML here because the shared
                    // append above ran before the rich outcome was known.
                    if rich_owns_images && !img_paths.is_empty() {
                        let (_, refused) =
                            send_local_images(session_id, bot, chat_id, thread_id, &img_paths)
                                .await;
                        if !refused.is_empty() {
                            display_html =
                                crate::utils::append_failure_notice(&display_html, &refused);
                        }
                    }
                    // #465: the video twin of the floor above, under its own
                    // flag and its own notice noun. A reply whose rich send
                    // failed has delivered neither family, so the clip would
                    // vanish with no notice at all — the same silent loss, one
                    // media type over.
                    if rich_owns_videos && !vid_paths.is_empty() {
                        let (_, refused) =
                            send_local_videos(session_id, bot, chat_id, thread_id, &vid_paths)
                                .await;
                        if !refused.is_empty() {
                            display_html =
                                crate::utils::append_video_failure_notice(&display_html, &refused);
                        }
                    }
                    // #1918: and the file twin, under its own noun. The floor
                    // above was suppressed because the rich plane owned the
                    // documents — but this arm is reached precisely when that
                    // rich send did NOT deliver, so the suppression would leave
                    // every document with no delivery leg at all and the reader
                    // with a marker pointing at nothing. `file_paths` is still
                    // the undelivered set (the #502 dedup ran on it, and nothing
                    // has shipped since), so this is the same list the floor
                    // would have taken.
                    if rich_owns_documents && !file_paths.is_empty() {
                        let (_, refused) =
                            send_local_files(session_id, bot, chat_id, thread_id, &file_paths)
                                .await;
                        if !refused.is_empty() {
                            display_html =
                                crate::utils::append_file_failure_notice(&display_html, &refused);
                        }
                    }
                    // #tg-mermaid-delivery-hardening: last-chance mermaid render
                    // before degrading to chunks — the classic chunked HTML path
                    // cannot embed `<img>`, so when the source carried a diagram
                    // try the rich HTML dialect once more (it supports images).
                    // If the renderer recovered in the meantime the diagram still
                    // lands inline instead of raw fence text; if it is still down
                    // the resolve yields a legible failure block (renderer note +
                    // source) rather than a bare code dump.
                    if super::rich::mermaid::should_render_mermaid(&text_only) {
                        let fallback_html =
                            super::rich::markdown_to_html_mermaid_p(&text_only).await;
                        match super::rich::api::send_rich_html_id(
                            bot.api_url().as_str(),
                            bot.token(),
                            chat_id.0,
                            thread_id,
                            &fallback_html,
                            None,
                            "turn",
                            "-",
                        )
                        .await
                        {
                            Ok(id) => {
                                tracing::info!(
                                    "Telegram: rich-html mermaid fallback delivered as msg {id}"
                                );
                                sent_reply_id = Some(id);
                                delivered_rich = true;
                            }
                            Err(e2) => {
                                tracing::warn!(
                                    "Telegram: rich-html mermaid fallback failed ({e2}); degrading to chunks"
                                );
                            }
                        }
                    }
                }
                // #487: there is nothing to chunk when the body was only an
                // image and the rich send failed. The floor above already
                // shipped the picture, and `split_message("")` yields a single
                // EMPTY chunk that would otherwise be sent as an empty message.
                if !delivered_rich && !display_html.is_empty() {
                    let chunks: Vec<String> = split_message(&display_html, 4096)
                        .into_iter()
                        .map(|s| s.to_string())
                        .collect();

                    // If single chunk and we have a streaming message, edit it in-place
                    if chunks.len() == 1
                        && let Some(mid) = streaming_msg_id
                    {
                        match bot
                            .edit_message_text(chat_id, mid, &chunks[0])
                            .parse_mode(ParseMode::Html)
                            .await
                        {
                            Ok(_) => {
                                // Edited in place — the reply bubble keeps `mid`.
                                // The final visible text is what a duplicate or
                                // chatty-agent investigation needs: one line
                                // closes "what actually landed in the mutated
                                // message" (#1085 post-review). Failure arms
                                // already log via the fallback sends' telemetry.
                                super::telemetry::log_send_success(
                                    "turn",
                                    "-",
                                    &session_id.to_string(),
                                    "stream_edit_final",
                                    "in_place_edit",
                                    chat_id.0,
                                    thread_id.map(|t| t.0.0),
                                    mid.0,
                                    chunks[0].len(),
                                    &super::telemetry::content_hash8(&chunks[0]),
                                );
                                sent_reply_id = Some(mid.0);
                                // Merge candidate (#tg-suggest-merge): the
                                // answer bubble suggest_options can ride on.
                                final_bubble = Some(super::state::MergeBubble {
                                    message_id: mid,
                                    body: super::state::BubbleBody::Html(chunks[0].clone()),
                                });
                            }
                            Err(teloxide::RequestError::RetryAfter(secs)) => {
                                // #556: a window over the inline bound is not slept
                                // and is not retried — the next pass re-renders.
                                if matches!(
                                    super::rate_limit::wait_out(
                                        "edit",
                                        secs.duration(),
                                        "",
                                        Some(chat_id.0)
                                    )
                                    .await,
                                    super::rate_limit::WaitOutcome::Deferred
                                ) {
                                    tracing::warn!(
                                        "Telegram: edit deferred by long 429 window chat={}",
                                        chat_id.0
                                    );
                                } else {
                                match bot
                                    .edit_message_text(chat_id, mid, &chunks[0])
                                    .parse_mode(ParseMode::Html)
                                    .await
                                {
                                    Ok(_) => {
                                        sent_reply_id = Some(mid.0);
                                        final_bubble = Some(super::state::MergeBubble {
                                            message_id: mid,
                                            body: super::state::BubbleBody::Html(chunks[0].clone()),
                                        });
                                    }
                                    Err(e) => {
                                        tracing::warn!(
                                            "Telegram: edit retry failed ({e}), falling back to delete+send"
                                        );
                                        best_effort_delete(
                                            bot,
                                            chat_id,
                                            mid,
                                            "edit-retry fallback",
                                        )
                                        .await;
                                        // Never silent (#1019): this is the LAST fallback.
                                        // The edit already failed and was logged; if the
                                        // resend fails too the message is gone entirely,
                                        // so the recovery path must not be the quiet one.
                                        if let Ok(sent) = send_html_or_plain(
                                            bot, chat_id, thread_id, &chunks[0], "turn", None,
                                        )
                                        .await
                                        {
                                            sent_reply_id = Some(sent.0);
                                            final_bubble = Some(super::state::MergeBubble {
                                                message_id: sent,
                                                body: super::state::BubbleBody::Html(
                                                    chunks[0].clone(),
                                                ),
                                            });
                                        } else {
                                            tracing::error!(
                                                "Telegram: delete+send fallback failed in chat {chat_id}, \
                                                 the reply was lost"
                                            );
                                        }
                                    }
                                }
                                }
                            }
                            Err(e) => {
                                tracing::warn!(
                                    "Telegram: edit final failed ({e}), falling back to delete+send"
                                );
                                best_effort_delete(bot, chat_id, mid, "edit-final fallback").await;
                                if let Ok(sent) = send_html_or_plain(
                                    bot, chat_id, thread_id, &chunks[0], "turn", None,
                                )
                                .await
                                {
                                    sent_reply_id = Some(sent.0);
                                    final_bubble = Some(super::state::MergeBubble {
                                        message_id: sent,
                                        body: super::state::BubbleBody::Html(chunks[0].clone()),
                                    });
                                }
                            }
                        }
                    } else {
                        // Multi-chunk or no streaming message — delete old, send new
                        if let Some(mid) = streaming_msg_id {
                            best_effort_delete(bot, chat_id, mid, "multi-chunk swap").await;
                        }
                        for chunk in &chunks {
                            // Last chunk wins — that's the bubble a user replies to.
                            if let Ok(sent) =
                                send_html_or_plain(bot, chat_id, thread_id, chunk, "turn", None)
                                    .await
                            {
                                sent_reply_id = Some(sent.0);
                                final_bubble = Some(super::state::MergeBubble {
                                    message_id: sent,
                                    body: super::state::BubbleBody::Html(chunk.clone()),
                                });
                            }
                        }
                    }
                }
            } else if let Some(mid) = streaming_msg_id {
                // Empty final text: all content was already delivered as
                // intermediate messages. The ctx budget rides the settled
                // flow message now, so just remove the now-empty streaming
                // placeholder.
                best_effort_delete(bot, chat_id, mid, "empty-final placeholder").await;
            }

            // Hand the merge candidate to the turn state (#tg-suggest-merge):
            // handler.rs reads it immediately after this returns and passes it
            // into render_suggestions. None (rich / voice / suppressed paths)
            // means suggestions fall back to their standalone block as before.
            if final_bubble.is_some() {
                let mut s = streaming.lock().unwrap_or_else(|e| e.into_inner());
                // #679: retain a SECOND copy. `render_suggestions` TAKES
                // `final_bubble`, so from the next turn only this field still
                // describes the answer whose text quiet mode needs to fold.
                s.published_answer = final_bubble.clone();
                s.final_bubble = final_bubble;
            }

            // Record the bot's text reply into channel_messages.
            //
            // Groups: needed so the recent() query that builds conversation
            // context on the NEXT turn sees both sides — without it the bot
            // loads a one-sided transcript and talks to itself in the dark.
            //
            // Both group AND DM persist the Telegram message_id (when captured)
            // so a later reply to THIS bubble can be recovered EXACTLY by id —
            // Telegram delivers rich bot messages with empty text, so the reply
            // handler can't read the quoted content from the update and must
            // look it up by id (#234 follow-up). DMs are stored only when we
            // have an id (lookup-only; DM conversation context still comes from
            // the session messages table, not channel_messages).
            let pmid = sent_reply_id.map(|i| i.to_string());
            if !text_only.trim().is_empty() && (!is_dm || pmid.is_some()) {
                let bot_display_name = telegram_state
                    .bot_username()
                    .await
                    .map(|u| format!("@{}", u))
                    .unwrap_or_else(|| "OpenCrabs".to_string());
                let thread_id_str = thread_id.map(|t| t.0.to_string());
                let resolved_topic_name = match &thread_id_str {
                    Some(tid) => channel_msg_repo
                        .latest_topic_name("telegram", &chat_id.0.to_string(), tid)
                        .await
                        .ok()
                        .flatten(),
                    None => None,
                };
                let cm = DbChannelMessage::new(
                    "telegram".to_string(),
                    chat_id.0.to_string(),
                    Some(chat_title.to_string()),
                    "bot:opencrabs".to_string(),
                    bot_display_name,
                    text_only.clone(),
                    "text".to_string(),
                    pmid.clone(),
                )
                .with_thread(thread_id_str, resolved_topic_name);
                if let Err(e) = channel_msg_repo.insert(&cm).await {
                    tracing::warn!(
                        "Telegram: failed to record bot reply in channel_messages: {}",
                        e
                    );
                }
            }

            // If input was voice AND TTS is enabled, also send voice note after text
            if is_voice && voice_config.tts_enabled {
                tracing::info!(
                    "Telegram: TTS requested — synthesizing response text (len={})",
                    response.content.len()
                );
                match crate::channels::voice::synthesize(&response.content, voice_config).await {
                    Ok(audio_bytes) => {
                        tracing::info!(
                            "Telegram: TTS succeeded — {} bytes of audio, sending to chat {}",
                            audio_bytes.len(),
                            chat_id
                        );
                        match voice_in_thread(
                            bot,
                            chat_id,
                            thread_id,
                            InputFile::memory(audio_bytes),
                        )
                        .await
                        {
                            Ok(m) => {
                                tracing::info!(
                                    "Telegram: voice message delivered (msg_id={})",
                                    m.id
                                );
                                // Record the delivered voice message ID in
                                // the isolated voice_msg_ids list. Cleanup
                                // paths do not touch this list. See the
                                // field doc on StreamingState.
                                let mut s = streaming.lock().unwrap_or_else(|e| e.into_inner());
                                s.voice_msg_ids.push(m.id);
                            }
                            Err(e) => {
                                tracing::error!("Telegram: send_voice failed — {}: {:?}", e, e);
                            }
                        }
                    }
                    Err(e) => {
                        tracing::error!("Telegram: TTS synthesis failed: {:#}", e);
                    }
                }
            }

            // ── Post-delivery image re-entry (#286) ──────────────────────────
            // Extraction can succeed and the CHANNEL still refuse the image:
            // the file vanished between validation and send, the upload
            // failed, or Telegram rejected the payload. None of that is
            // visible to the model — the reply it wrote announces a picture
            // that never arrived. Hand it ONE synthetic correction turn
            // through the detached-work queue (the #227 machinery, which
            // resumes the turn WITH the tool registry) so it can tell the user
            // plainly which image is missing.
            //
            // The latch is what keeps this finite: the correction turn
            // delivers through this same path, so an unlatched re-entry would
            // re-arm on its own failure and chain turns forever.
            // `try_spend_image_reentry` returns false once spent, and the
            // latch re-arms when the user sends their NEXT message
            // (handler.rs), so a genuine failure in a later turn still heals.
            //
            // The guard line is deliberately NOT latched: the user is told
            // about every failed attachment, however many turns it takes to
            // get there.
            if !image_failures.is_empty() {
                let alert = format!(
                    "{} image attachment(s) could not be delivered",
                    image_failures.len()
                );
                let guard_line = format!("🛡️ guard: {alert}");
                append_system_to_flow(bot, chat_id, thread_id, streaming, &guard_line).await;
                if telegram_state.try_spend_image_reentry(session_id) {
                    let nudge =
                        crate::brain::agent::service::nudge::local_image_delivery_failure_nudge(
                            &image_failures,
                        );
                    tracing::info!(
                        "Telegram: post-delivery image re-entry queued for session {session_id} \
                         ({} failure(s))",
                        image_failures.len()
                    );
                    telegram_state.enqueue_detached_result(
                        session_id,
                        crate::brain::agent::QueuedUserMessage::system(
                            nudge,
                            format!("🖼️ {alert} — asking the model to report them"),
                        ),
                    );
                } else {
                    tracing::info!(
                        "Telegram: post-delivery image re-entry latch already spent for session \
                         {session_id}; notice only"
                    );
                }
            }
            // #1916: the same post-delivery ladder for a file the channel
            // refused. It shares the ONE re-entry latch with the image leg
            // above — the latch is what keeps this finite, and a turn gets one
            // synthetic correction, not one per media family. When both
            // families fail in the same turn the image leg spends it first and
            // this leg is notice-only; the honest floor above still tells the
            // user which file is missing, so nothing vanishes silently either
            // way.
            if !file_failures.is_empty() {
                let alert = format!(
                    "{} file attachment(s) could not be delivered",
                    file_failures.len()
                );
                let guard_line = format!("🛡️ guard: {alert}");
                append_system_to_flow(bot, chat_id, thread_id, streaming, &guard_line).await;
                if telegram_state.try_spend_image_reentry(session_id) {
                    let nudge =
                        crate::brain::agent::service::nudge::local_file_delivery_failure_nudge(
                            &file_failures,
                        );
                    tracing::info!(
                        "Telegram: post-delivery file re-entry queued for session {session_id} \
                         ({} failure(s))",
                        file_failures.len()
                    );
                    telegram_state.enqueue_detached_result(
                        session_id,
                        crate::brain::agent::QueuedUserMessage::system(
                            nudge,
                            format!("📄 {alert} — asking the model to report them"),
                        ),
                    );
                } else {
                    tracing::info!(
                        "Telegram: post-delivery file re-entry latch already spent for session \
                         {session_id}; notice only"
                    );
                }
            }
        }
        Err(ref e) if matches!(e, crate::brain::agent::AgentError::Cancelled) => {
            tracing::info!("Telegram: agent call cancelled for session {}", session_id);
            // Silently clean up — user already received "Operation cancelled." from /stop
            if let Some(mid) = streaming_msg_id {
                best_effort_delete(bot, chat_id, mid, "cancel cleanup").await;
            }
        }
        Err(e) => {
            tracing::error!("Telegram: agent error: {}", e);
            // Translate via the shared helper so the message tells the
            // user WHAT self-heal already tried + what to do next,
            // instead of leaking the raw `API error (502)` shape that
            // confused users into thinking the agent silently dropped
            // their request. See `brain::agent::format_user_error` for
            // the pattern matchers (5xx exhausted / 429 / context too
            // large / stream broken / repetition loop / etc.).
            let user_msg = format!("❌ Error\n\n{}", crate::brain::agent::format_user_error(&e));
            if let Some(mid) = streaming_msg_id {
                if let Err(e) = bot.edit_message_text(chat_id, mid, user_msg).await {
                    tracing::warn!(
                        target: "telegram::send",
                        chat_id = chat_id.0,
                        message_id = mid.0,
                        error = %e,
                        "final-error edit failed"
                    );
                }
            } else {
                message_in_thread(bot, chat_id, thread_id, user_msg).await?;
            }
        }
    }
    Ok(true)
}

/// Send each resolved image as its own bubble, routing by the photo ceiling
/// exactly as the final leg does (#286). Returns the paths that landed and the
/// failures the channel itself refused.
///
/// One home for the loop the final-response leg and the promotion fallback both
/// need (#502): they must not drift on kind selection, captions, failure
/// reasons or telemetry, and a copy-pasted second version is how they would.
/// Both halves of the outcome are returned because the callers need both and
/// they are set in different arms — inferring one from the other would couple
/// two independent facts.
pub(crate) async fn send_local_images(
    session_id: Uuid,
    bot: &Bot,
    chat_id: ChatId,
    thread_id: Option<teloxide::types::ThreadId>,
    images: &[crate::utils::image::LocalImage],
) -> (Vec<std::path::PathBuf>, Vec<LocalImageFailure>) {
    let mut delivered: Vec<std::path::PathBuf> = Vec::new();
    let mut failures: Vec<LocalImageFailure> = Vec::new();

    // A picture above the 10 MB photo ceiling would be rejected by `sendPhoto`,
    // so it ships as a document instead — an un-previewable image beats a
    // missing one (#286).
    for image in images {
        let img_path = &image.path;
        let bytes = match tokio::fs::read(img_path).await {
            Ok(bytes) => bytes,
            Err(e) => {
                tracing::error!(
                    "Telegram: failed to read image {}: {}",
                    img_path.display(),
                    e
                );
                failures.push(LocalImageFailure {
                    raw: img_path.display().to_string(),
                    resolved: Some(img_path.clone()),
                    reason: LocalImageFailureReason::Unreadable,
                });
                continue;
            }
        };
        let len = bytes.len();
        let kind = telegram_media_kind(len as u64);
        let sent = match kind {
            TelegramMediaKind::Photo => photo_in_thread(
                bot,
                chat_id,
                thread_id,
                InputFile::memory(bytes),
                image.caption.clone(),
            )
            .await
            .map(|m| m.id.0),
            TelegramMediaKind::Document => document_in_thread(
                bot,
                chat_id,
                thread_id,
                InputFile::memory(bytes),
                image.caption.clone(),
            )
            .await
            .map(|m| m.id.0),
        };
        match sent {
            Ok(mid) => {
                delivered.push(img_path.clone());
                let reference = img_path.display().to_string();
                // Match the outbox media receipt: len is sent bytes and
                // hash8 identifies the path, so one audit predicate covers both legs.
                super::telemetry::log_send_success(
                    "turn",
                    "-",
                    &session_id.to_string(),
                    "delivery_media",
                    match kind {
                        TelegramMediaKind::Photo => "image_photo",
                        TelegramMediaKind::Document => "image_document",
                    },
                    chat_id.0,
                    thread_id.map(|t| t.0.0),
                    mid,
                    len,
                    &super::telemetry::content_hash8(&reference),
                );
            }
            Err(e) => {
                tracing::error!(
                    "Telegram: failed to send image {} as {}: {}",
                    img_path.display(),
                    match kind {
                        TelegramMediaKind::Photo => "photo",
                        TelegramMediaKind::Document => "document",
                    },
                    e
                );
                failures.push(LocalImageFailure {
                    raw: img_path.display().to_string(),
                    resolved: Some(img_path.clone()),
                    reason: LocalImageFailureReason::DeliveryFailed,
                });
            }
        }
    }

    (delivered, failures)
}

/// The filename Telegram should display for a document read from `path`.
///
/// `InputFile::memory` carries no name, and teloxide's own fallback returns an
/// empty string for a `Bytes` payload — Telegram then labels the document
/// `file` and drops the MIME, because the extension never travelled either.
/// A path whose final component is absent (`/`, `..`) keeps that empty
/// fallback, so the call site is unchanged for it.
pub(crate) fn document_part_name(path: &std::path::Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Read the bytes for one family's resolved entries into a media array, once
/// per site that builds one.
///
/// All six sites that fill a `MediaEntry` array — the final leg's image, video
/// and document loops, and the intermediate leg's three — did the same three
/// things in the same order: `tokio::fs::read` the entry's path, push an entry
/// carrying the family's kind and the bytes, and on failure warn under the
/// family's noun and push a [`LocalImageFailure`]. The bodies were identical
/// per family and the only variation was the noun and the failure list, so one
/// helper serves them all.
///
/// The returned tuple is `(entries, failures, read_paths)`:
/// - `entries` are appended to the caller's shared media array, in order;
/// - `failures` are appended to the caller's per-family failure list (the
///   notice names the family, so an image's failure must not be answered under
///   a document's noun);
/// - `read_paths` are the paths whose bytes actually READ. A caller that
///   records what it delivered needs exactly that set — a path whose read
///   failed was never handed to the API and must not be recorded as sent.
///
/// `warn_what` names the family in the failure log, so every site's line reads
/// `Telegram: failed to read <warn_what> <path>: <error>` (`local image for the
/// rich plane`, `promoted video`, …). `name_fn` supplies the entry's part name
/// for the family that has one: a document sets it to the file's own name
/// (`document_part_name`) because bytes alone cannot recover it, while the photo
/// and video families pass a closure returning `None`, whose part name is
/// derived from their bytes at the send site.
pub(crate) async fn read_media_entries<E, F>(
    entries: &[E],
    kind: MediaKind,
    warn_what: &str,
    name_fn: F,
) -> (Vec<MediaEntry>, Vec<LocalImageFailure>, Vec<PathBuf>)
where
    E: MediaSource,
    F: Fn(&E) -> Option<String>,
{
    let mut media: Vec<MediaEntry> = Vec::with_capacity(entries.len());
    let mut failures: Vec<LocalImageFailure> = Vec::new();
    let mut read_paths: Vec<PathBuf> = Vec::with_capacity(entries.len());
    for entry in entries {
        let path = entry.media_path();
        match tokio::fs::read(path).await {
            Ok(bytes) => {
                media.push(MediaEntry {
                    kind,
                    id: entry.media_id().to_string(),
                    url: None,
                    bytes: Some(bytes),
                    name: name_fn(entry),
                });
                read_paths.push(path.to_path_buf());
            }
            Err(e) => {
                tracing::warn!(
                    "Telegram: failed to read {} {}: {}",
                    warn_what,
                    path.display(),
                    e
                );
                failures.push(LocalImageFailure {
                    raw: path.display().to_string(),
                    resolved: Some(path.to_path_buf()),
                    reason: LocalImageFailureReason::Unreadable,
                });
            }
        }
    }
    (media, failures, read_paths)
}

/// A local file that landed in the chat, and the message it landed in (#1918).
///
/// The id is what lets the rich plane link a file's `📎 <label>` marker back to
/// the bubble that carries it: a marker that names a document the reader can
/// scroll to is the whole point of the marker, and without the id the rewrite
/// would have to guess which bubble belongs to which link. The path travels
/// with it because the caller looks the file up BY PATH — the id alone would
/// not say which file it carried.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DeliveredFile {
    /// The file that was sent.
    pub path: std::path::PathBuf,
    /// Telegram's id for the message the send produced.
    pub message_id: i32,
}

/// The `t.me` message link for a delivered file's bubble, when its chat has one
/// (#1918).
///
/// A message link exists only for a supergroup or a channel, and both are
/// identified by the `-100` prefix on the chat id — the SAME marker the
/// `t.me/c/` path below strips. Deriving it from `chat_id` rather than taking a
/// `ChatKind` is what lets a resume-path turn build the link at all: the chat id
/// survives a restart, the inbound message does not (#771).
///
/// `t.me/c/…` addresses a chat by its INTERNAL id — the chat id with the `-100`
/// marker stripped — and gains a middle segment with the topic id when the
/// message sits in a forum topic, so the reader lands on the bubble inside the
/// right topic rather than at the top of a thread they then have to search.
///
/// A private chat and a basic group have no message-link form at all, so they
/// get `None` and the caller leaves the marker as plain text: a link that goes
/// nowhere is worse than no link, and the marker's job — pointing at the file —
/// is done by the label either way.
pub(crate) fn file_message_link(
    chat_id: i64,
    thread_id: Option<teloxide::types::ThreadId>,
    message_id: i32,
) -> Option<String> {
    // `t.me/c/` takes the id WITHOUT the `-100` marker that identifies a
    // supergroup or channel. An id lacking the prefix is a private chat or a
    // basic group, neither of which has a message-link form, so `None` falls out
    // of the same strip that produces the internal id.
    let chat = chat_id.to_string();
    let internal = chat.strip_prefix("-100")?;
    Some(match thread_id {
        Some(thread) => format!("https://t.me/c/{internal}/{}/{message_id}", thread.0.0),
        None => format!("https://t.me/c/{internal}/{message_id}"),
    })
}

/// The `(path, t.me link)` pairs for the files a delivery leg actually sent
/// (#771).
///
/// Built from the delivered ids alone. It used to be written inline at the
/// delivery site behind `inbound.map(…)`, which made a fact about the CHAT — the
/// link form its id implies — hostage to a fact about the MESSAGE, so a resumed
/// turn built no links at all. One helper so the final leg and any future caller
/// share the rule, and so the resume-path case has a unit test of its own.
pub(crate) fn delivered_file_links(
    delivered: &[DeliveredFile],
    chat_id: i64,
    thread_id: Option<teloxide::types::ThreadId>,
) -> Vec<(std::path::PathBuf, String)> {
    delivered
        .iter()
        .filter_map(|file| {
            file_message_link(chat_id, thread_id, file.message_id)
                .map(|link| (file.path.clone(), link))
        })
        .collect()
}

/// Rewrite each file's `📎 <label>` marker into a markdown link to the bubble
/// that carries that file (#1918, link part 4).
///
/// `links` names the files that were BOTH delivered and reachable by a link —
/// `(path, url)` — so a file the channel refused, a file a promoted
/// intermediate already sent, or one whose chat kind has no message-link form
/// is simply absent and keeps the plain marker. The marker never points at a
/// bubble that does not exist, and it never disappears: the label is the
/// reader's anchor either way.
///
/// `text` is the buffer being rewritten, which STARTS as a copy of `scan.text`
/// and is then run through the artifact strip, the secret redaction and the
/// dedup ladder — any of which can move a marker. `scan` therefore travels with
/// the call, and a span is cut only while the bytes it names still match the
/// ones the scanner wrote there: a rewrite that moved the text degrades to
/// today's plain marker instead of splicing a link over unrelated words.
///
/// Spans are cut from the LAST to the FIRST, so a link's extra bytes can never
/// invalidate a span that has not been cut yet.
pub(crate) fn link_file_markers(
    text: &str,
    scan: &crate::utils::image::LocalFileScan,
    links: &[(std::path::PathBuf, String)],
) -> String {
    if links.is_empty() {
        return text.to_string();
    }
    let mut spans: Vec<(usize, usize, &str, &str)> = Vec::new();
    for file in &scan.attachments {
        let Some(span) = file.marker_span.as_ref() else {
            // Not from a scan: nothing to cut.
            continue;
        };
        let (start, end) = (span.start, span.end);
        let Some(marker) = text.get(start..end) else {
            continue;
        };
        if scan.text.get(start..end) != Some(marker) {
            // The text moved under the span — the marker stays plain.
            continue;
        }
        let Some((_, link)) = links
            .iter()
            .find(|(path, _)| path.as_path() == file.path.as_path())
        else {
            continue;
        };
        spans.push((start, end, marker, link.as_str()));
    }
    if spans.is_empty() {
        return text.to_string();
    }
    // The scanner emits markers in order, so this only guards a caller that
    // reordered them: cutting back-to-front is what keeps the earlier spans
    // valid, and that needs the order to be known.
    spans.sort_by_key(|(start, _, _, _)| *start);
    let mut linked = text.to_string();
    for (start, end, marker, link) in spans.iter().rev() {
        linked.replace_range(*start..*end, &format!("[{marker}]({link})"));
    }
    linked
}

/// Send each resolved local file as its own document bubble (#1916).
///
/// Deliberately NOT a generalization of [`send_local_images`]: a file has no
/// kind decision to make. Telegram's clients render no document preview, so
/// there is no photo/document ceiling to route by — every file ships through
/// `sendDocument`, and the one size gate that matters lives upstream in
/// [`crate::utils::image::validate_local_file`], where exceeding it can be
/// reported to the MODEL as a rejection reason before delivery is attempted
/// instead of surfacing here as a refusal the model never saw coming. Returns
/// the files that landed — each with the message id the send produced — and the
/// failures the channel itself refused, for the same reason its siblings do: the
/// two halves are set in different arms, and inferring one from the other would
/// couple two independent facts.
pub(crate) async fn send_local_files(
    session_id: Uuid,
    bot: &Bot,
    chat_id: ChatId,
    thread_id: Option<teloxide::types::ThreadId>,
    files: &[crate::utils::image::LocalFile],
) -> (Vec<DeliveredFile>, Vec<LocalImageFailure>) {
    let mut delivered: Vec<DeliveredFile> = Vec::new();
    let mut failures: Vec<LocalImageFailure> = Vec::new();

    for file in files {
        let path = &file.path;
        let bytes = match tokio::fs::read(path).await {
            Ok(bytes) => bytes,
            Err(e) => {
                tracing::error!("Telegram: failed to read file {}: {}", path.display(), e);
                failures.push(LocalImageFailure {
                    raw: path.display().to_string(),
                    resolved: Some(path.clone()),
                    reason: LocalImageFailureReason::Unreadable,
                });
                continue;
            }
        };
        let len = bytes.len();
        let sent = document_in_thread(
            bot,
            chat_id,
            thread_id,
            InputFile::memory(bytes).file_name(document_part_name(path)),
            file.caption.clone(),
        )
        .await
        .map(|m| m.id.0);
        match sent {
            Ok(mid) => {
                delivered.push(DeliveredFile {
                    path: path.clone(),
                    message_id: mid,
                });
                let reference = path.display().to_string();
                // Match the outbox media receipt: len is sent bytes and hash8
                // identifies the path, so one audit predicate covers every leg.
                super::telemetry::log_send_success(
                    "turn",
                    "-",
                    &session_id.to_string(),
                    "delivery_media",
                    "file_document",
                    chat_id.0,
                    thread_id.map(|t| t.0.0),
                    mid,
                    len,
                    &super::telemetry::content_hash8(&reference),
                );
            }
            Err(e) => {
                tracing::error!(
                    "Telegram: failed to send file {} as document: {}",
                    path.display(),
                    e
                );
                failures.push(LocalImageFailure {
                    raw: path.display().to_string(),
                    resolved: Some(path.clone()),
                    reason: LocalImageFailureReason::DeliveryFailed,
                });
            }
        }
    }

    (delivered, failures)
}

/// Send each resolved video as its own bubble, routing by the video ceiling and
/// the container exactly as the final leg does (#465).
///
/// The video twin of [`send_local_images`], and deliberately not a
/// generalization of it: the kind decision differs in SHAPE, not just in
/// numbers — an image's kind is a property of its byte length alone, while a
/// video's is a property of `(byte length, container)`, because Telegram's
/// clients play MPEG-4 and will take anything else only as a document. So the
/// bytes are sniffed here (`VIDEO_FORMAT_HEAD_BYTES` of them) and the result
/// feeds [`telegram_video_media_kind`]. Returns the paths that landed and the
/// failures the channel itself refused, for the same reason its twin does: the
/// two halves are set in different arms and inferring one from the other would
/// couple two independent facts.
pub(crate) async fn send_local_videos(
    session_id: Uuid,
    bot: &Bot,
    chat_id: ChatId,
    thread_id: Option<teloxide::types::ThreadId>,
    videos: &[crate::utils::image::LocalVideo],
) -> (Vec<std::path::PathBuf>, Vec<LocalImageFailure>) {
    let mut delivered: Vec<std::path::PathBuf> = Vec::new();
    let mut failures: Vec<LocalImageFailure> = Vec::new();

    for video in videos {
        let vid_path = &video.path;
        let bytes = match tokio::fs::read(vid_path).await {
            Ok(bytes) => bytes,
            Err(e) => {
                tracing::error!(
                    "Telegram: failed to read video {}: {}",
                    vid_path.display(),
                    e
                );
                failures.push(LocalImageFailure {
                    raw: vid_path.display().to_string(),
                    resolved: Some(vid_path.clone()),
                    reason: LocalImageFailureReason::Unreadable,
                });
                continue;
            }
        };
        let len = bytes.len();
        // The container is read from the file's OWN leading bytes, never from
        // its extension: a `.mp4` name over an AVI payload would otherwise take
        // the video arm and settle as a bubble that never plays.
        let format = sniff_video_format(&bytes[..VIDEO_FORMAT_HEAD_BYTES.min(len)]);
        let kind = telegram_video_media_kind(len as u64, format);
        let sent = match kind {
            TelegramVideoKind::Video => video_in_thread(
                bot,
                chat_id,
                thread_id,
                InputFile::memory(bytes),
                video.caption.clone(),
            )
            .await
            .map(|m| m.id.0),
            TelegramVideoKind::Document => document_in_thread(
                bot,
                chat_id,
                thread_id,
                InputFile::memory(bytes),
                video.caption.clone(),
            )
            .await
            .map(|m| m.id.0),
        };
        match sent {
            Ok(mid) => {
                delivered.push(vid_path.clone());
                let reference = vid_path.display().to_string();
                // Same audit predicate as the image twin: len is sent bytes and
                // hash8 identifies the path, so one predicate covers both legs.
                super::telemetry::log_send_success(
                    "turn",
                    "-",
                    &session_id.to_string(),
                    "delivery_media",
                    match kind {
                        TelegramVideoKind::Video => "video",
                        TelegramVideoKind::Document => "video_document",
                    },
                    chat_id.0,
                    thread_id.map(|t| t.0.0),
                    mid,
                    len,
                    &super::telemetry::content_hash8(&reference),
                );
            }
            Err(e) => {
                tracing::error!(
                    "Telegram: failed to send video {} as {}: {}",
                    vid_path.display(),
                    match kind {
                        TelegramVideoKind::Video => "video",
                        TelegramVideoKind::Document => "document",
                    },
                    e
                );
                failures.push(LocalImageFailure {
                    raw: vid_path.display().to_string(),
                    resolved: Some(vid_path.clone()),
                    reason: LocalImageFailureReason::DeliveryFailed,
                });
            }
        }
    }

    (delivered, failures)
}

/// The ONE pipeline a mid-turn intermediate goes through — shared by the live
/// edit loop and the post-loop drain, which is why it lives here rather than in
/// either caller (#502, #470).
///
/// The order of these steps IS the fix. The image scan used to run AFTER the
/// strip and its result was discarded, so by the time the promotion gate
/// decided, the reference was already gone and the gate could not see an
/// attachment that had been removed 21 lines earlier. Scanning first means the
/// gate sees the picture, and the rewritten text carries it into the bubble the
/// model wrote.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn handle_intermediate(
    session_id: Uuid,
    bot: &Bot,
    chat: ChatId,
    thread_id: Option<teloxide::types::ThreadId>,
    streaming: &Arc<std::sync::Mutex<StreamingState>>,
    tg: &TelegramState,
    cwd: &std::path::Path,
    react_target: Option<MessageId>,
    raw: &str,
) {
    // 1. Sanitize: strip LLM artifacts, then redact secrets at the session's
    //    own scope (a DM keeps them, a group scrubs them — #677).
    let text = crate::utils::sanitize::strip_llm_artifacts(raw);
    // One short lock for both reads, released before any await.
    let (is_dm, delivered) = {
        let s = streaming.lock().unwrap_or_else(|e| e.into_inner());
        (s.is_dm, s.delivered_image_paths.clone())
    };
    let text = redact_secrets_scoped(&text, is_dm);

    // 2. Extract the react directive BEFORE the rewrite, so both output forms
    //    are marker-free by construction rather than by a second pass. The
    //    sole behavioural delta is an image whose ALT TEXT contains a
    //    `<<react:…>>` directive; no real input has one.
    let (text, react_emoji) = crate::utils::extract_react_marker(&text);
    // A resumed turn has no inbound message to react to: the marker is stripped
    // above but nothing fires (#261).
    if let Some(ref emoji) = react_emoji
        && let Some(target) = react_target
    {
        fire_reaction(bot, chat, target, emoji).await;
    }

    // 3. TWO walks, VIDEO first. The order is load-bearing, and it is the same
    //    order the delivery floor uses (`extract_local_videos` then
    //    `extract_local_images`): both families read the markdown reference
    //    form, so a reference left to the image walk is judged
    //    `UnsupportedFormat`, CONSUMED, and lost — the clip would never be
    //    delivered and the reader would be told "Image not attached" instead.
    //    Claiming it here means the image walk never sees it. What the video
    //    walk deliberately leaves — a markdown reference to a picture, an
    //    `<<IMG:…>>` marker, a remote target — is exactly the image walk's
    //    input. #732: the image walk runs TWICE, on the video family's rich and
    //    stripped forms, because a `tg://video` reference survives verbatim in
    //    both (a media ref is never consumed) and the two planes need opposite
    //    things from it — `rw_rich.rich` keeps both families' references for the
    //    media array, `rw_stripped.stripped` drops both for the HTML plane and
    //    the dedup record.
    //
    //    `vid` is its own id prefix: entries are matched to references BY ID
    //    inside one message's media array, so a shared prefix would let an image
    //    entry answer a `tg://video` reference (#732 lifts the clips into that
    //    same array).
    let vw = crate::utils::image::rewrite_local_videos(
        &text,
        Some(cwd),
        crate::utils::VID_ID_PREFIX,
        &delivered,
    );
    // `Some(cwd)` is the session working directory — the same base the final
    // leg resolves against.
    //
    // #732: TWO image rewrites, mirroring the final leg's own pipeline. The
    // video walk leaves a `tg://video` reference verbatim in BOTH its buffers,
    // so a single walk cannot separate them:
    //
    //  * `rw_rich` — built on `vw.rich` — keeps both families' references, which
    //    is what the rich media array needs in order to answer the clip.
    //  * `rw_stripped` — built on `vw.stripped` — carries NEITHER family's
    //    reference, which is what the HTML plane (no media array) and the dedup
    //    record need. It is byte-identical to the single walk this path used to
    //    build, so the HTML and dedup behaviour are unchanged.
    let rw_rich = crate::utils::image::rewrite_local_images(&vw.rich, Some(cwd), "img", &delivered);
    let rw_stripped =
        crate::utils::image::rewrite_local_images(&vw.stripped, Some(cwd), "img", &delivered);
    // #1918: the FILE family walks LAST, on the image family's rich form — the
    // same one-owner-per-reference order the final leg uses. Its entries ride
    // the promoted bubble's media array as `MediaKind::Document`, so the
    // document renders AT its reference instead of as a bare `📎 <label>` (the
    // hole the #1918 probe found on rich bubble `90894`). `doc` is its own id
    // prefix for the reason `vid` is: entries are matched to references BY ID
    // inside one message's media array.
    let fw_rich = crate::utils::image::rewrite_local_files(
        &rw_rich.rich,
        Some(cwd),
        crate::utils::DOC_ID_PREFIX,
        &delivered,
        // #1968: same gate as the final leg — a markdown document promoted
        // with an intermediate must not be inlined either, or the bubble the
        // reader keeps would carry the un-openable form.
        crate::config::Config::current()
            .channels
            .telegram
            .inline_markdown_documents,
    );

    // 4. A fresh picture is report-shaped content on its own (#502); anything
    //    else keeps folding, with the failure notice carried along so a broken
    //    reference is named instead of vanishing.
    //
    //    #679: a quiet group suppresses the REPORT arm only. The room scans its
    //    scrollback, so a mid-turn status report is noise there and folds into
    //    the collapsed block instead of opening a bubble. The MEDIA arm still
    //    promotes even when quiet: the fold path strips image references, so
    //    suppressing it would lose the picture outright (#502) — and a picture
    //    is not narration. Passing 0 for `fresh_images` is exact here: media is
    //    decided by the arm above, so the report predicate is asked about text
    //    alone.
    let quiet = crate::config::Config::current()
        .channels
        .telegram
        .is_quiet_for(&chat.0.to_string());
    let has_fresh_media = !(rw_stripped.entries.is_empty()
        && vw.entries.is_empty()
        // #1918: a fresh DOCUMENT is media too. Leaving it out would fold a
        // document-only intermediate into the collapsed block, where the file
        // walk's `tg://document` reference has no media array to answer it —
        // the exact detached-bubble shape this task removes.
        && fw_rich.entries.is_empty());
    let promote = has_fresh_media
        || (!quiet && super::intermediates::should_promote_intermediate(&rw_stripped.stripped, 0));
    if promote {
        super::intermediates::deliver_intermediate_message(
            session_id,
            bot,
            chat,
            thread_id,
            streaming,
            tg,
            cwd,
            super::intermediates::IntermediateMedia {
                rich_images: &rw_rich,
                stripped_images: &rw_stripped,
                videos: &vw,
                rich_files: &fw_rich,
            },
        )
        .await;
    } else {
        // The FOLDED form is the image walk's stripped output, not the video
        // walk's: `vw.stripped` has the clips' references gone but still
        // carries every picture reference and `<<IMG:…>>` marker verbatim,
        // because the image walk has not run on it. `rw_stripped.stripped` is
        // the one form with both families' references removed — the only correct
        // text for a plane that carries no media at all (#465).
        //
        // #1918: the file pass runs on it too. The flow block is a surface the
        // reader sees, and a stripped local-file link there is the same defect
        // the promoted bubble had — the reference gone, its position unmarked.
        // (The promoted path runs the same pass inside
        // `deliver_intermediate_message`, over both reflowed forms.)
        let file_scan = crate::utils::extract_local_files(&rw_stripped.stripped, Some(cwd));
        let folded = crate::utils::append_failure_notice(
            &file_scan.text,
            &intermediate_failures(&rw_stripped, &vw),
        );
        append_intermediate_to_flow(bot, chat, thread_id, streaming, &folded).await;
    }
}

/// The intermediate path's failure list: both families' refusals, in the order
/// the walks ran.
///
/// A merge rather than a filter, because the video walk runs FIRST: it claims
/// every reference it can deliver, so the image walk that follows never judges a
/// clip's bytes and never answers a reference the video plane owns. The two
/// lists are therefore disjoint by construction — an image failure is a
/// genuinely broken picture and a video failure a genuinely broken clip, which
/// is why the notice names the family and the reader is never told "Image not
/// attached" about a video (#465).
pub(crate) fn intermediate_failures(
    rw: &crate::utils::image::LocalImageRewrite,
    vw: &crate::utils::image::LocalVideoRewrite,
) -> Vec<LocalImageFailure> {
    let mut failures: Vec<LocalImageFailure> = vw.failures.clone();
    failures.extend(rw.failures.iter().cloned());
    failures
}

/// Drain the display items left queued after the edit loop stopped,
/// folding them into the processing-log flow. ONE shared implementation for
/// handle_message and resume_session (#470 / #462 item 1: the drains were
/// copy-pasted and could drift apart — parity is now structural).
/// `react_target` is the inbound message a folded `<<react:>>` directive
/// acknowledges; resume has none (the original message id is lost across
/// restarts), so the directive strips without firing (#261).
///
/// `cwd` and `tg` are threaded through for the shared intermediate pipeline
/// (#502): the drain resolves relative image references against the same
/// session working directory the live loop uses, and the promotion path needs
/// the Telegram state to note its own bubbles.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn drain_remaining_display(
    session_id: Uuid,
    bot: &Bot,
    chat: ChatId,
    thread_id: Option<teloxide::types::ThreadId>,
    streaming: &Arc<std::sync::Mutex<StreamingState>>,
    tg: &TelegramState,
    cwd: &std::path::Path,
    remaining: Vec<DisplayItem>,
    react_target: Option<MessageId>,
) {
    let mut tool_buffer: Vec<usize> = Vec::new();
    for item in remaining {
        match item {
            DisplayItem::NewTool(idx) => {
                tool_buffer.push(idx);
            }
            DisplayItem::Intermediate(text) => {
                // Fold or promote through the ONE shared pipeline (#502),
                // which sanitizes exactly like the live edit-loop path.
                append_tool_group(bot, chat, thread_id, streaming, &tool_buffer).await;
                tool_buffer.clear();
                handle_intermediate(
                    session_id,
                    bot,
                    chat,
                    thread_id,
                    streaming,
                    tg,
                    cwd,
                    react_target,
                    &text,
                )
                .await;
            }
            DisplayItem::System(text) => {
                // Chrome folds into the same block, in order, but skips the
                // model pipeline: it is our own text, so there is no artifact
                // to strip, no image marker, and no react directive to fire.
                // Secrets are still scrubbed — a self-healing alert embeds a
                // raw upstream error string (#1253).
                append_tool_group(bot, chat, thread_id, streaming, &tool_buffer).await;
                tool_buffer.clear();
                let text = redact_secrets(&text);
                append_system_to_flow(bot, chat, thread_id, streaming, &text).await;
            }
        }
    }
    // Flush any remaining tools into the open group (merges the final batch
    // into the running collapsible block instead of opening a new message).
    append_tool_group(bot, chat, thread_id, streaming, &tool_buffer).await;
}

/// Strip an echoed plan title from the start of the agent's response (#837).
///
/// The `[ACTIVE PLAN REMINDER]` shows the model `📋 Plan: "{title}"` every
/// turn, and the model opens its reply by repeating it. The plan card already
/// carries the title, so the text repeats what is rendered directly above it.
///
/// Pure, taking the title rather than loading it, so the matching is testable
/// without a session on disk — the first version compared an exact string and
/// could only be exercised end to end.
pub(crate) fn strip_echoed_plan_title(text: &str, plan_title: &str) -> String {
    let title = normalize_title_line(plan_title);
    if title.is_empty() {
        return text.to_string();
    }
    let trimmed = text.trim_start();
    let Some(first_line) = trimmed.lines().next() else {
        return text.to_string();
    };
    if normalize_title_line(first_line) != title {
        return text.to_string();
    }
    // Drop the line and the blank line that usually follows a heading.
    let rest = trimmed[first_line.len()..].trim_start_matches('\n');
    rest.to_string()
}

/// Reduce a line to the bare title for comparison.
///
/// Handles the shapes the model actually produces, which are the shapes the
/// reminder itself shows it: markdown headings and emphasis, the `📋` the
/// reminder and the card both use, a `Plan:` label, and surrounding quotes.
/// The earlier version trimmed only `#`, `*` and `~`, so an echo of the
/// reminder's own formatting — the most likely echo of all — never matched.
fn normalize_title_line(line: &str) -> String {
    let mut s = line.trim();
    // Leading blockquote / list / heading markers.
    s = s.trim_start_matches(['#', '>', '-', '*', '_', '~', ' ']);
    // Any leading non-alphanumeric run: emoji such as 📋, bullets, symbols.
    s = s.trim_start_matches(|c: char| !c.is_alphanumeric() && c != '"');
    // The reminder labels it `Plan: "…"`; the model copies the label too.
    for label in ["Plan:", "plan:", "PLAN:"] {
        if let Some(rest) = s.strip_prefix(label) {
            s = rest.trim_start();
            break;
        }
    }
    s = s.trim_matches(['"', '\'', '*', '~', '_', ' ']);
    s.trim().to_string()
}
