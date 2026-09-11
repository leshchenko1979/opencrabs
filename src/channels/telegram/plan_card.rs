//! Persistent per-session plan card (#580): a single Telegram message that
//! shows the plan title + checklist and the Approve/Discard keyboard, edited in
//! place across the creation/execution/completion turns instead of re-rendered
//! inside each per-turn flow block. Tracked cross-turn on [`TelegramState`], so
//! there is exactly one card at a time rather than one checklist per turn.

use super::TelegramState;
use super::flow_chrome::{
    GoalSection, PlanKb, ProseSection, load_goal_section, load_plan_prose, load_plan_sections,
};
use super::handler::escape_html;
use super::send::message_in_thread;
use crate::brain::agent::AgentService;
use crate::config::Config;
use crate::utils::truncate_chars;
use std::sync::Arc;
use teloxide::prelude::*;
use teloxide::types::{MessageId, ParseMode, ThreadId};
use uuid::Uuid;

/// Total character budget for prose bodies on the classic card. The card
/// carries the title, checklist rows, and keyboard inside Telegram's 4096-char
/// message cap; sections past the budget are dropped (full prose via
/// /show-plan). The rich path (`sendRichMessage`, 32K chars) needs no budget.
const CARD_PROSE_BUDGET: usize = 2400;

/// Goal text budget (chars) on the classic card. The goal renders as a
/// collapsed expandable (ADR 0005 Decision 12), so the cap only trims the
/// expanded body, never the visible chrome.
const GOAL_TEXT_CAP: usize = 600;

/// Collapsible wrapper style for prose sections and goals.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum CollapsibleStyle {
    /// Classic `sendMessage` (4096 chars): `<blockquote expandable>`,
    /// prose truncated to `CARD_PROSE_BUDGET`.
    BlockquoteExpandable,
    /// Rich `sendRichMessage` (32K chars): `<details><summary>`, no truncation.
    /// Production callers migrated to the markdown+media dialect; the test-only
    /// `render_plan_card_rich_html` is the remaining constructor.
    #[cfg_attr(not(test), expect(dead_code))]
    DetailsSummary,
}

/// One section of the card, before it is committed to a target's HTML dialect.
///
/// Sections describe WHAT they are; the serializer decides how they are
/// separated. Letting each section pick its own line break is what collapsed
/// the checklist into a single line (#941): prose was converted to `<p>` for
/// the rich target and the title, checklist rows and goal separator were left
/// on bare `\n`, which the rich renderer treats as ordinary whitespace.
enum CardBlock {
    /// Inline content that must occupy its own line (the title, a checklist
    /// row). The serializer supplies whatever the target needs to break here.
    Line(String),
    /// Already block-level markup (`<details>`, `<blockquote>`, `<p>`-wrapped
    /// prose) that must not be wrapped again — `<p>` cannot contain `<details>`.
    Block(String),
    /// A blank line before the goal on the classic card. The rich serializer
    /// ignores it: block-level elements already space themselves.
    ClassicGap,
}

/// Commit the card's sections to one target's HTML dialect.
///
/// This is the ONLY place a line-break convention is chosen. Adding a section
/// cannot get it wrong, because sections no longer decide.
fn serialize_card(style: CollapsibleStyle, blocks: &[CardBlock]) -> String {
    let mut out = String::new();
    match style {
        // Classic `ParseMode::Html` is Telegram's limited dialect: a bare
        // newline IS the line break and there is no `<p>`. One newline between
        // blocks; `ClassicGap` contributes the second.
        CollapsibleStyle::BlockquoteExpandable => {
            for b in blocks {
                match b {
                    CardBlock::Line(s) | CardBlock::Block(s) => {
                        if !out.is_empty() {
                            out.push('\n');
                        }
                        out.push_str(s);
                    }
                    CardBlock::ClassicGap => {
                        if !out.is_empty() {
                            out.push('\n');
                        }
                    }
                }
            }
        }
        // `sendRichMessage` renders real HTML, where a newline is whitespace
        // and collapses. Every standalone line needs its own block-level
        // wrapper; the tags provide the spacing, so blocks join with nothing.
        CollapsibleStyle::DetailsSummary => {
            for b in blocks {
                match b {
                    CardBlock::Line(s) => {
                        out.push_str("<p>");
                        out.push_str(s);
                        out.push_str("</p>");
                    }
                    CardBlock::Block(s) => out.push_str(s),
                    CardBlock::ClassicGap => {}
                }
            }
        }
    }
    out
}

/// Unified plan card renderer. The two surfaces (classic sendMessage and rich
/// sendRichMessage) share title, checklist, and goal logic; only the
/// collapsible tag pair, the truncation budget, and the HTML dialect differ.
///
/// Returns `None` when the session has no plan content (no title and no
/// checklist) — the caller removes the card in that case.
///
/// Async because the rich arms render prose through the mermaid-aware
/// converter (`markdown_to_html_mermaid_p`), so a mermaid fence in plan
/// prose resolves to an embedded image exactly like one in a final reply
/// (#1142). The classic arms stay on the sync converter: classic
/// `sendMessage` HTML has no `<img>` tag, so a resolved image would get the
/// whole card rejected — a fence there stays a readable source code block.
async fn render_plan_card(
    style: CollapsibleStyle,
    title: Option<&str>,
    checklist: Option<&[String]>,
    prose: Option<&[ProseSection]>,
    goal: Option<&GoalSection>,
) -> Option<String> {
    let mut blocks: Vec<CardBlock> = Vec::new();

    // Title: identical for both styles.
    if let Some(t) = title.map(str::trim).filter(|t| !t.is_empty()) {
        blocks.push(CardBlock::Line(format!("📋 <b>{}</b>", escape_html(t))));
    }

    // Prose: style-dependent collapsible tags + truncation budget.
    // Locked order: title, prose expandables, checklist rows, goal.
    if let Some(sections) = prose.filter(|s| !s.is_empty()) {
        let mut budget: Option<usize> = match style {
            CollapsibleStyle::BlockquoteExpandable => Some(CARD_PROSE_BUDGET),
            CollapsibleStyle::DetailsSummary => None,
        };
        for sec in sections {
            if budget == Some(0) {
                break;
            }
            // Truncate raw text BEFORE HTML conversion so the collapsible
            // tags are always well-formed (truncating rendered HTML can
            // cut mid-tag, causing Telegram to strip rich formatting).
            let (body, chars_used) = match budget {
                Some(remaining) => {
                    let truncated = truncate_chars(&sec.body, remaining);
                    (truncated, truncated.chars().count())
                }
                None => (sec.body.as_str(), 0),
            };
            budget = budget.map(|b| b.saturating_sub(chars_used));
            // Every arm yields block-level markup: a collapsible element, or
            // prose the renderer has already wrapped for its target. The rich
            // arms go through the mermaid-aware converter (#1142); the classic
            // arms stay sync (classic HTML cannot embed <img>).
            blocks.push(CardBlock::Block(match (&sec.heading, style) {
                (Some(h), CollapsibleStyle::BlockquoteExpandable) => format!(
                    "<blockquote expandable><b>{}</b>\n{}</blockquote>",
                    escape_html(h),
                    super::rich::markdown_to_html(body),
                ),
                (Some(h), CollapsibleStyle::DetailsSummary) => format!(
                    "<details><summary><b>{}</b></summary>{}</details>",
                    escape_html(h),
                    super::rich::markdown_to_html_mermaid_p(body).await,
                ),
                (None, CollapsibleStyle::DetailsSummary) => {
                    super::rich::markdown_to_html_mermaid_p(body).await
                }
                (None, CollapsibleStyle::BlockquoteExpandable) => {
                    super::rich::markdown_to_html(body)
                }
            }));
        }
    }

    // Checklist: identical for both styles. Each row is its own line, and the
    // serializer decides what that means for the target — these rows rendering
    // as one run-on paragraph is the bug this structure prevents (#941).
    if let Some(rows) = checklist {
        for row in rows {
            blocks.push(CardBlock::Line(escape_html(row)));
        }
    }

    // Goal dart marker (owner option B, #77 — word dropped): truncation on classic only.
    if let Some(g) = goal {
        let text = g.text.trim();
        if !text.is_empty() {
            let has_prose = prose.is_some_and(|p| !p.is_empty());
            if checklist.is_some() || has_prose {
                blocks.push(CardBlock::ClassicGap);
            }
            blocks.push(CardBlock::Block(match style {
                CollapsibleStyle::BlockquoteExpandable => {
                    let capped = escape_html(truncate_chars(text, GOAL_TEXT_CAP));
                    format!(
                        "<blockquote expandable>{} {capped}</blockquote>",
                        g.prefix(true)
                    )
                }
                CollapsibleStyle::DetailsSummary => format!(
                    "<details><summary>{}</summary>\n{}</details>",
                    g.prefix(true),
                    escape_html(text)
                ),
            }));
        }
    }

    let out = serialize_card(style, &blocks);
    (!out.is_empty()).then_some(out)
}

/// Classic sendMessage card: `<blockquote expandable>` collapsibles, 4096-char
/// budget with per-section prose truncation.
pub(crate) async fn render_plan_card_html(
    title: Option<&str>,
    checklist: Option<&[String]>,
    prose: Option<&[ProseSection]>,
    goal: Option<&GoalSection>,
) -> Option<String> {
    render_plan_card(
        CollapsibleStyle::BlockquoteExpandable,
        title,
        checklist,
        prose,
        goal,
    )
    .await
}

/// Rich `sendRichMessage` card: `<details><summary>` collapsibles, 32K-char
/// limit, no truncation — prose renders in full.
/// Production callers migrated to the markdown+media dialect (dd70fdd6);
/// kept for test coverage of the card-assembly invariants.
#[cfg(test)]
pub(crate) async fn render_plan_card_rich_html(
    title: Option<&str>,
    checklist: Option<&[String]>,
    prose: Option<&[ProseSection]>,
    goal: Option<&GoalSection>,
) -> Option<String> {
    render_plan_card(
        CollapsibleStyle::DetailsSummary,
        title,
        checklist,
        prose,
        goal,
    )
    .await
}

/// Markdown-mode rich card (#134 family): the same blocks as the rich HTML
/// variant, but committed to raw markdown — details/summary collapsibles
/// inline (the markdown input dialect parses them natively, live-Bot-API
/// probe J/K 2026-09-10) and checklist rows as plain lines. Mermaid fences
/// in prose are NOT resolved here: the caller resolves the ASSEMBLED body
/// once through `resolve_markdown_media`, so every card re-render reuses
/// one diagN numbering per body instead of per-section (reviewer major #1).
pub(crate) async fn render_plan_card_markdown(
    title: Option<&str>,
    checklist: Option<&[String]>,
    prose: Option<&[ProseSection]>,
    goal: Option<&GoalSection>,
) -> Option<String> {
    let mut blocks: Vec<CardBlock> = Vec::new();

    if let Some(t) = title.map(str::trim).filter(|t| !t.is_empty()) {
        blocks.push(CardBlock::Line(format!("📋 <b>{}</b>", escape_html(t))));
    }

    if let Some(sections) = prose.filter(|s| !s.is_empty()) {
        for sec in sections {
            // Markdown mode: details/summary inline, prose body raw (the
            // markdown dialect renders md formatting + native tables) — with
            // media tags neutralised first: a LIVE `<img>`/`<video>`/`<audio>`
            // tag in model-authored prose is a whole-message rejection when
            // Telegram cannot resolve it against the media array (#134 root
            // cause, live-probe verified).
            let body = super::rich::mermaid::neutralize_prose_media_html(&sec.body);
            blocks.push(CardBlock::Block(match &sec.heading {
                Some(h) => format!(
                    "<details><summary><b>{}</b></summary>\n{}\n</details>",
                    escape_html(h),
                    body
                ),
                None => body,
            }));
        }
    }

    if let Some(rows) = checklist {
        for row in rows {
            blocks.push(CardBlock::Line(escape_html(row)));
        }
    }

    if let Some(g) = goal {
        let text = g.text.trim();
        if !text.is_empty() {
            let has_prose = prose.is_some_and(|p| !p.is_empty());
            if checklist.is_some() || has_prose {
                blocks.push(CardBlock::ClassicGap);
            }
            blocks.push(CardBlock::Block(format!(
                "<details><summary>{}</summary>\n{}\n</details>",
                g.prefix(true),
                escape_html(text)
            )));
        }
    }

    // Markdown serializer: same single-variant shape as the DetailsSummary
    // serializer but with markdown line semantics — Lines need a real
    // newline between them, Blocks carry their own collapsibles.
    let mut out = String::new();
    for b in &blocks {
        match b {
            CardBlock::Line(s) => {
                if !out.is_empty() {
                    out.push_str("\n\n");
                }
                out.push_str(s);
            }
            CardBlock::Block(s) => {
                // Skip the separator when the output already ends with a
                // blank line (an explicit ClassicGap) — otherwise the gap
                // doubles into two empty lines before the block.
                if !out.is_empty() && !out.ends_with("\n\n") {
                    out.push('\n');
                }
                out.push_str(s);
            }
            CardBlock::ClassicGap => {
                if !out.is_empty() {
                    out.push_str("\n\n");
                }
            }
        }
    }
    (!out.is_empty()).then_some(out)
}

/// Result of a plan card edit attempt.
enum EditOutcome {
    /// Card saved successfully (or content unchanged).
    Saved,
    /// Rate-limited: card writes suppressed for a duration.
    Suppressed,
    /// Card gone/unusable: caller should try creating fresh.
    Gone,
}

/// Classify a plan card edit failure and take the appropriate state action.
/// Handles "message is not modified" (silent success) and rate-limiting
/// (suppress future writes). Returns `Gone` when the card needs recreating.
async fn handle_edit_failure(
    error: &str,
    state: &TelegramState,
    session_id: Uuid,
    chat: ChatId,
    thread_id: Option<ThreadId>,
    signature: &str,
    mid: MessageId,
) -> EditOutcome {
    if error.contains("message is not modified") {
        state
            .set_plan_card(session_id, chat, thread_id, mid, signature.to_string())
            .await;
        return EditOutcome::Saved;
    }
    if let Some(wait) = super::rate_limit::parse_retry_after(error) {
        tracing::warn!(
            "Telegram plan card edit throttled for session {session_id}: {error} — \
             pausing card writes for {}s",
            wait.as_secs()
        );
        state
            .suppress_plan_card(session_id, wait + super::rate_limit::RETRY_MARGIN)
            .await;
        return EditOutcome::Suppressed;
    }
    tracing::debug!("Telegram plan card edit failed ({mid:?}): {error} — recreating");
    state.take_plan_card(session_id).await;
    EditOutcome::Gone
}

/// Classify a plan card create failure. Suppresses future writes on rate-limit,
/// warns on other errors.
async fn handle_create_failure(error: &str, state: &TelegramState, session_id: Uuid) {
    if let Some(wait) = super::rate_limit::parse_retry_after(error) {
        tracing::warn!(
            "Telegram plan card create throttled for session {session_id}: {error} — \
             pausing card writes for {}s",
            wait.as_secs()
        );
        state
            .suppress_plan_card(session_id, wait + super::rate_limit::RETRY_MARGIN)
            .await;
    } else {
        tracing::warn!("Telegram plan card create failed: {error}");
    }
}

/// Spawn label for the plan-review worker (#155). The subagent spawn path
/// keys its single write-grant exception on this exact label, so the two
/// sides must never drift — both read this one constant.
pub(crate) const PLAN_REVIEW_LABEL: &str = crate::brain::tools::subagent::PLAN_REVIEW_LABEL;

/// Card footer while a review subagent is rewriting the plan (#155).
pub(crate) const PLAN_REVIEW_RUNNING_NOTE: &str = "🔍 Review subagent rewriting plan…";

/// Cap on the one-line review delta rendered into the card footer. The delta
/// is a card line, not a report: the review worker is collected through
/// `wait_agent`, so its full report is deliberately NOT echoed into the
/// session — the delta is the only thing the owner is shown.
const PLAN_REVIEW_DELTA_CAP: usize = 200;

/// Marker the review brief asks the worker to end its report with, so the
/// card delta is the worker's own summary rather than a scraped prose line.
const PLAN_REVIEW_DELTA_MARKER: &str = "DELTA:";

/// The keyboard a plan card should show, given whether a review is running
/// (#155). Only the Editing keyboard grays out — a running review must not
/// invent a keyboard for the checklist or absent states.
pub(crate) fn plan_review_effective_kb(plan_kb: PlanKb, reviewing: bool) -> PlanKb {
    if reviewing && plan_kb == PlanKb::ApproveDiscard {
        PlanKb::ReviewingApproveDiscard
    } else {
        plan_kb
    }
}

/// Footer note for the card, if any (#155). The running note wins over a
/// stale delta, so the owner never reads a previous review's summary while a
/// new one is mid-flight.
pub(crate) fn plan_review_footer_note(
    plan_kb: PlanKb,
    reviewing: bool,
    delta: Option<String>,
) -> Option<String> {
    // The footer belongs to the EDITING card only. The same renderer draws the
    // Active checklist card (Discard-only) and the None card, and a review
    // delta or a "reviewing…" note left over on either of those would describe
    // a plan state that no longer exists.
    if !matches!(
        plan_kb,
        PlanKb::ApproveDiscard | PlanKb::ReviewingApproveDiscard
    ) {
        return None;
    }
    if reviewing {
        return Some(PLAN_REVIEW_RUNNING_NOTE.to_string());
    }
    delta.filter(|d| !d.trim().is_empty())
}

/// Append the footer to a rendered card body (#155). Deliberately outside the
/// renderers: both production arms of `refresh_plan_card` call one renderer
/// each, so appending here covers rich and classic alike — without threading a
/// fifth parameter through every renderer and its many test call sites.
pub(crate) fn plan_card_with_footer(body: String, footer: Option<&str>) -> String {
    let Some(note) = footer.map(str::trim).filter(|n| !n.is_empty()) else {
        return body;
    };
    format!("{body}\n\n{note}")
}

/// Spawn input for the plan-review worker (#155). Pure so the test asserts the
/// exact contract the production path sends instead of a copy of it.
pub(crate) fn plan_review_spawn_input(session_id: Uuid, brief: String) -> serde_json::Value {
    serde_json::json!({
        "prompt": brief,
        "label": PLAN_REVIEW_LABEL,
        "plan_session": session_id.to_string(),
        "read_only": false,
    })
}

/// Child agent id from a `spawn_agent` tool result (#155). The spawn returns
/// `Spawned sub-agent '<label>' with id: <id>`; parse the id rather than
/// treating that acknowledgement as the worker's report — the defect the first
/// attempt shipped.
pub(crate) fn plan_review_agent_id(spawn_output: &str) -> Option<String> {
    let rest = spawn_output.split_once("with id: ")?.1;
    let id = rest.split_whitespace().next().unwrap_or_default();
    (!id.is_empty()).then(|| id.to_string())
}

/// One-line card delta from a finished review's report (#155). Reads the
/// worker's own `DELTA:` line; falls back to a status line so the card always
/// says something true about what happened.
pub(crate) fn plan_review_delta(report: Option<&str>) -> String {
    let Some(report) = report else {
        return "✨ Review finished but returned no report.".to_string();
    };
    let summary = report
        .lines()
        .rev()
        .map(str::trim)
        .find_map(|line| line.strip_prefix(PLAN_REVIEW_DELTA_MARKER))
        .map(str::trim)
        .filter(|s| !s.is_empty());
    match summary {
        Some(s) => format!("✨ Review: {}", clamp_review_delta(s)),
        None => "✨ Review finished (no summary line returned).".to_string(),
    }
}

/// Clamp a review summary to the footer budget, marking the cut with an
/// ellipsis — a silently shortened sentence reads as a complete one, and the
/// owner would never know the worker had more to say.
fn clamp_review_delta(s: &str) -> String {
    let cut = truncate_chars(s, PLAN_REVIEW_DELTA_CAP);
    if cut.len() == s.len() {
        s.to_string()
    } else {
        format!("{cut}…")
    }
}

/// Create or update the session's plan card to reflect the live plan state,
/// carrying `plan_kb`. Removes the card when the plan is gone.
///
/// When `rich_messages` is enabled, the card is sent via `sendRichMessage`
/// (32K char limit, native `<details><summary>` collapsibles). On any rich
/// API failure, falls back to the classic HTML `sendMessage` path (4096 chars,
/// `<blockquote expandable>`).
pub(crate) async fn refresh_plan_card(
    bot: &Bot,
    chat: ChatId,
    thread_id: Option<ThreadId>,
    state: &Arc<TelegramState>,
    agent: &AgentService,
    session_id: Uuid,
    plan_kb: PlanKb,
) {
    // Telegram asked us to wait. The card is chrome, so skipping an update
    // beats renewing the flood-control window on every refresh (#814).
    // Checked BEFORE taking the lock, so a throttled session releases waiters
    // immediately instead of queueing them behind a write that will not happen.
    if state.plan_card_suppressed(session_id).await {
        return;
    }

    // Serialise everything below (#822). The sequence is check-whether-a-card-
    // is-tracked, decide edit-or-post, record the id — and with nothing held
    // across it two concurrent refreshes both saw no card, both posted, and the
    // second id overwrote the first. The loser was left visible in the chat but
    // untracked, so it could never be edited or deleted again.
    //
    // Held across the API calls, not just the map reads: releasing before the
    // create is exactly what leaves the window open.
    let card_lock = state.plan_card_lock(session_id).await;
    let _guard = card_lock.lock().await;
    let (title, checklist) = load_plan_sections(session_id).await;
    let prose = load_plan_prose(session_id).await;
    // Goal scoping (owner rule 2026-09-03, #84): a completed goal belongs to
    // the plan it was set under and renders only in the finalize chrome's ✅
    // card. The live card drops `completed` rows — the row survives in
    // goal_state after the turn-end judge marks it done, and without this
    // filter every NEWLY primed plan's card re-rendered the old goal text
    // (live instance: session 2fae1230, 2026-09-03). The flow path keeps its
    // own turn-local retained_goal guard for same-turn sighting.
    let goal = if checklist.is_some() {
        load_goal_section(agent, session_id)
            .await
            .filter(|&(_, completed)| !completed)
            .map(|(text, _)| GoalSection {
                text,
                completed: false,
            })
    } else {
        None
    };
    let use_rich = Config::current().channels.telegram.rich_messages;

    // #155: a plan review in flight grays the Review button and explains
    // itself in the footer; a finished review leaves its one-line delta there.
    let reviewing = state.is_plan_reviewing(session_id).await;
    let plan_kb = plan_review_effective_kb(plan_kb, reviewing);
    let footer_note = plan_review_footer_note(
        plan_kb,
        reviewing,
        state.plan_review_delta(session_id).await,
    );

    // Try rich path first when enabled: sendRichMessage (32K) as the
    // markdown+media dialect (#134 family) — raw markdown prose with inline
    // details/summary collapsibles (probe-proven, 2026-09-10) and rendered
    // mermaid diagrams riding the media array instead of a bare HTML string
    // that drops every diagram (the defect HQ's card exposed). The assembled
    // body resolves through `resolve_markdown_media` exactly ONCE, so diagN
    // numbering is stable per body.
    if use_rich
        && let Some(md) = render_plan_card_markdown(
            title.as_deref(),
            checklist.as_deref(),
            prose.as_deref(),
            goal.as_ref(),
        )
        .await
    {
        let (rich_md, media) = super::rich::mermaid::resolve_markdown_media(&md).await;
        // A `tg://photo?id=X` reference with no matching media entry is a
        // whole-message 400 (`RICH_MESSAGE_PHOTO_INVALID`); prose can carry
        // one as an example. Neutralise orphans AFTER resolution so the refs
        // the resolver just created keep resolving.
        let rich_md = super::rich::mermaid::neutralize_orphan_photo_refs(&rich_md, &media);
        let rich_md = super::rich::normalize_tables(&rich_md);
        // #155 footer rides the body, so it lands inside the signature below —
        // a footer-only change (review started, or a new delta) must re-render.
        let rich_md = plan_card_with_footer(rich_md, footer_note.as_deref());
        let kb_val = plan_kb
            .keyboard()
            .and_then(|m| serde_json::to_value(m).ok());
        // Sig covers the media array too: identical prose with a changed
        // diagram (re-render bytes/url) must not sig-skip.
        let rich_sig = format!(
            "richmd:{rich_md}\u{1}{:?}\u{1}{plan_kb:?}",
            media
                .iter()
                .map(|m| (
                    m.id.clone(),
                    m.url.clone(),
                    m.bytes.as_ref().map(|b| b.len())
                ))
                .collect::<Vec<_>>()
        );
        if let Some((mid, last_sig)) = state.plan_card(session_id).await {
            if last_sig == rich_sig {
                return;
            }
            // G2 flood governor (#1211): plan-card refreshes are FINAL class —
            // never dropped. When the edit bucket is empty the payload queues
            // latest-wins and the governor's drainer lands it on refill; the
            // tracked signature is saved now so identical later refreshes skip
            // (a permanently failed queue drain self-heals on the next
            // differing-content plan change). Media rides the queued final via
            // the media-bearing admission variant (owner law: extend, never
            // bypass the governor).
            let admitted = super::governor::edit_admission_media(
                bot,
                chat,
                mid,
                super::governor::EditClass::Final,
                rich_md.clone(),
                true,
                media.clone(),
            )
            .await;
            if !admitted {
                state
                    .set_plan_card(session_id, chat, thread_id, mid, rich_sig)
                    .await;
                return;
            }
            match super::rich::api::edit_rich_markdown_media(
                bot.api_url().as_str(),
                bot.token(),
                chat.0,
                mid.0,
                &rich_md,
                &media,
                kb_val.as_ref(),
                "turn",
                "-",
            )
            .await
            {
                Ok(()) => {
                    state
                        .set_plan_card(session_id, chat, thread_id, mid, rich_sig)
                        .await;
                    return;
                }
                Err(e) => {
                    let outcome = handle_edit_failure(
                        &e.to_string(),
                        state,
                        session_id,
                        chat,
                        thread_id,
                        &rich_sig,
                        mid,
                    )
                    .await;
                    match outcome {
                        EditOutcome::Saved | EditOutcome::Suppressed => return,
                        EditOutcome::Gone => { /* fall through to create */ }
                    }
                }
            }
        }
        // No live card or edit failed: create fresh via rich API.
        // G3 send pacing (#1211): a fresh card is a full message post.
        super::governor::pace_send(chat).await;
        match super::rich::api::send_rich_markdown_media_target_id(
            bot.api_url().as_str(),
            bot.token(),
            chat.0,
            thread_id,
            None,
            &rich_md,
            &media,
            kb_val.as_ref(),
            "turn",
            "-",
        )
        .await
        {
            Ok(mid) => {
                state
                    .set_plan_card(session_id, chat, thread_id, MessageId(mid), rich_sig)
                    .await;
                return;
            }
            Err(e) => {
                tracing::warn!("Rich plan card create failed: {e} — falling back to HTML");
            }
        }
    }

    // Classic HTML path (sendMessage, 4096 chars, <blockquote expandable>).
    let Some(html) = render_plan_card_html(
        title.as_deref(),
        checklist.as_deref(),
        prose.as_deref(),
        goal.as_ref(),
    )
    .await
    else {
        // No live plan. If THIS settle just archived the plan, the completion
        // arrived on a path that didn't run the handler settle gate (a
        // flood-delayed settle via resume.rs/stream_loop drives refresh_plan_card
        // directly). Finalize the completed card and re-stick it to the bottom
        // instead of silently deleting it (#1231). Otherwise it's a genuine
        // plan-gone/discard removal.
        if crate::utils::plan_files::peek_plan_just_archived(session_id).await {
            // Finalize consumes the flag itself once the notice lands (#16);
            // a failed finalize leaves it in place for the next settle's retry.
            finalize_plan_card_locked(bot, chat, thread_id, state, session_id).await;
        } else {
            remove_plan_card_locked(bot, chat, state, session_id).await;
        }
        return;
    };
    // #155: footer rides the body, so it lands inside `signature` below — a
    // footer-only change (review started, or a new delta) must re-render.
    let html = plan_card_with_footer(html, footer_note.as_deref());
    let kb = plan_kb.keyboard();
    let signature = format!("{html}\u{1}{plan_kb:?}");

    if let Some((mid, last_sig)) = state.plan_card(session_id).await {
        if last_sig == signature {
            return;
        }
        // G2 flood governor (#1211): same FINAL contract as the rich path —
        // queue latest-wins when the edit bucket is empty, never drop.
        let admitted = super::governor::edit_admission(
            bot,
            chat,
            mid,
            super::governor::EditClass::Final,
            html.clone(),
            false,
        )
        .await;
        if !admitted {
            state
                .set_plan_card(session_id, chat, thread_id, mid, signature)
                .await;
            return;
        }
        let mut req = bot
            .edit_message_text(chat, mid, html.clone())
            .parse_mode(ParseMode::Html);
        if let Some(ref k) = kb {
            req = req.reply_markup(k.clone());
        }
        match req.await {
            Ok(_) => {
                state
                    .set_plan_card(session_id, chat, thread_id, mid, signature)
                    .await;
                return;
            }
            Err(e) => {
                let outcome = handle_edit_failure(
                    &e.to_string(),
                    state,
                    session_id,
                    chat,
                    thread_id,
                    &signature,
                    mid,
                )
                .await;
                match outcome {
                    EditOutcome::Saved | EditOutcome::Suppressed => return,
                    EditOutcome::Gone => { /* fall through to create */ }
                }
            }
        }
    }

    // No live card (or it was unusable): post a fresh one at the bottom.
    // G3 send pacing (#1211): a fresh card is a full message post.
    super::governor::pace_send(chat).await;
    let mut req = message_in_thread(bot, chat, thread_id, html).parse_mode(ParseMode::Html);
    if let Some(ref k) = kb {
        req = req.reply_markup(k.clone());
    }
    match req.await {
        Ok(m) => {
            state
                .set_plan_card(session_id, chat, thread_id, m.id, signature)
                .await
        }
        Err(e) => {
            handle_create_failure(&e.to_string(), state, session_id).await;
        }
    }
}

/// Delete the session's plan card and stop tracking it. Used both as terminal
/// removal (discard / plan gone) and — followed by a later [`refresh_plan_card`]
/// — as a re-stick so the next card posts fresh at the bottom of the
/// conversation, keeping exactly one card visible as it follows the turns down.
/// Completed-plan finalization (#1158, #1231): when a turn settles right after
/// the session's plan was archived, turn the card into its completed form and
/// RE-STICK IT TO THE BOTTOM of the thread: ✅ header, final checklist, keyboard
/// stripped (explicit EMPTY `inline_keyboard`; omitting `reply_markup` leaves
/// stale buttons attached per Bot API semantics), footer noting the archive.
/// The buried tracked card is deleted and a fresh completed card is posted at
/// the conversation's current position, so the finished plan lands where the
/// user is reading, not far up in history.
///
/// Flood-safe: the fresh card is posted FIRST (paced through G3); only if the
/// post fails do we fall back to editing the tracked card in place, so the
/// completed form is never lost on a flood-controlled settle. A successful
/// post then deletes the old card best-effort — a delete failure leaves the
/// stale card visible, never a duplicate (the new one is what users see).
///
/// One-shot (#809 lesson): success UNTRACKS the card, so no later refresh or
/// restart ever re-renders an archived plan as live. The "just archived THIS
/// settle" stamp is a durable flag; tool_loop archives at EVERY settling
/// plan-turn, so without that stamp a later settle would wrongly finalize an
/// unrelated archive forever.
///
/// Outcome-gated consumption (#16): finalize itself consumes the flag — but
/// only AFTER the completed card's post/edit is confirmed landed (or is
/// terminally impossible). A failed finalize leaves the flag in place, so
/// the NEXT settle retries instead of losing the completion notice forever.
/// Pre-#16 the gate site consumed the flag BEFORE calling finalize; when a
/// G3 pacing hold aborted the settle mid-await, finalize died before any API
/// call and any failure branch — zero logs, flag gone, notice lost. Returns
/// `true` when the notice landed or was terminally settled, `false` when the
/// flag was kept for retry.
pub(crate) async fn finalize_plan_card(
    bot: &Bot,
    chat: ChatId,
    thread_id: Option<ThreadId>,
    state: &Arc<TelegramState>,
    session_id: Uuid,
) -> bool {
    // Same lock discipline as refresh/remove (#822): held across API calls.
    let card_lock = state.plan_card_lock(session_id).await;
    let _guard = card_lock.lock().await;
    finalize_plan_card_locked(bot, chat, thread_id, state, session_id).await
}

/// Drop-guard WARN for a mid-flight finalize abort (#16): when the settle
/// task is cancelled while finalize awaits (the G3 pacing hold never
/// resolving before the settle context drops was the 2026-08-28 incident),
/// the future is dropped and NO code below the await runs — the abort was
/// forensic-silent. Drop is the one hook that still fires; a normal exit
/// disarms the guard first.
struct FinalizeAbortGuard {
    session_id: Uuid,
    armed: bool,
}

impl Drop for FinalizeAbortGuard {
    fn drop(&mut self) {
        if self.armed {
            tracing::warn!(
                "Telegram plan card finalize aborted mid-flight for session {} \
                 (task cancelled while awaiting — e.g. unresolved G3 pacing hold); \
                 just-archived flag retained, next settle retries",
                self.session_id
            );
        }
    }
}

/// Finalization body, for callers already holding the per-session card lock.
///
/// `refresh_plan_card` runs the same lock and, on its no-live-plan path (the
/// plan was just archived there), finalizes instead of deleting — calling the
/// lock-taking `finalize_plan_card` from inside that locked scope would
/// deadlock (the lock is not reentrant; same split as
/// `remove_plan_card_locked`).
async fn finalize_plan_card_locked(
    bot: &Bot,
    chat: ChatId,
    thread_id: Option<ThreadId>,
    state: &Arc<TelegramState>,
    session_id: Uuid,
) -> bool {
    let Some((mid, _sig)) = state.plan_card(session_id).await else {
        // Nothing tracked: finalized once already, or never posted (card
        // tracking is in-memory — a restart empties it). Either way
        // deliberately NOT reposting is what kills resurrection. Consume
        // the flag (#16) so a stale stamp cannot gate every later settle.
        // Logged (#16 round 2): this consume drops the completion notice
        // permanently — it must never be forensic-silent again.
        tracing::warn!(
            "Telegram plan card finalize for session {session_id}: no tracked \
             card (already finalized or never posted) — consuming just-archived \
             flag, completion notice NOT posted"
        );
        crate::utils::plan_files::take_plan_just_archived(session_id).await;
        return true;
    };
    let Some(doc) = crate::utils::plan_files::latest_archived_plan(session_id).await else {
        // No archived document to render from: terminal, consume (#16).
        // Logged (#16 round 2): the stem-drift bug routed EVERY finalize
        // through this branch (reader prefix never matched the dot-less
        // archive names) — it must never be forensic-silent again.
        tracing::warn!(
            "Telegram plan card finalize for session {session_id}: no archived \
             plan document found — consuming just-archived flag, completion \
             notice NOT posted"
        );
        crate::utils::plan_files::take_plan_just_archived(session_id).await;
        return true;
    };
    // #16: armed across every await below; a normal exit disarms it. If the
    // enclosing task is cancelled mid-await instead, its Drop logs the abort
    // the old code never could.
    let mut abort_guard = FinalizeAbortGuard {
        session_id,
        armed: true,
    };
    let (title, checklist) = super::flow_chrome::plan_document_sections(&doc);
    let empty_kb = serde_json::json!({ "inline_keyboard": [] });

    let use_rich = Config::current().channels.telegram.rich_messages;

    // Completed forms, mirrored dual-path as in refresh_plan_card. The rich
    // form rides the markdown dialect now (#134 family) — checklist-only
    // body, no media; the ✅/notice chrome is markdown-bold + plain italic.
    let rich = if use_rich {
        render_plan_card_markdown(title.as_deref(), checklist.as_deref(), None, None)
            .await
            .map(|mut r| {
                r = r.replacen("📋", "✅", 1);
                r.push_str("\n*Plan completed and archived.*");
                r
            })
    } else {
        None
    };
    let mut html = render_plan_card_html(title.as_deref(), checklist.as_deref(), None, None)
        .await
        .unwrap_or_else(|| "<b>Plan</b>".to_string());
    html = html.replacen("📋", "✅", 1);
    html.push_str("\n<i>Plan completed and archived.</i>");

    // Restick-to-bottom (#1231): post the completed card fresh at the bottom
    // FIRST, so a post failure can fall back to the card already present — the
    // completed form is never lost.
    let mut posted: Option<MessageId> = None;
    if use_rich && let Some(rich) = &rich {
        // G3 send pacing (#1211): a fresh card is a full message post.
        super::governor::pace_send(chat).await;
        match super::rich::api::send_rich_markdown_media_target_id(
            bot.api_url().as_str(),
            bot.token(),
            chat.0,
            thread_id,
            None,
            rich,
            &[],
            None,
            "turn",
            "-",
        )
        .await
        {
            Ok(mid) => posted = Some(MessageId(mid)),
            Err(e) => tracing::warn!("Telegram plan card rich restick failed: {e}"),
        }
    }
    if posted.is_none() {
        // G3 send pacing (#1211): a fresh card is a full message post.
        super::governor::pace_send(chat).await;
        let req = message_in_thread(bot, chat, thread_id, html.clone()).parse_mode(ParseMode::Html);
        match req.await {
            Ok(m) => posted = Some(m.id),
            Err(e) => tracing::warn!("Telegram plan card restick post failed ({mid:?}): {e}"),
        }
    }

    match posted {
        Some(new_mid) => {
            // The fresh completed card is at the bottom: the completion
            // notice has LANDED — consume the flag now, not before (#16).
            crate::utils::plan_files::take_plan_just_archived(session_id).await;
            tracing::info!(
                "Telegram plan card finalized for session {session_id}: \
                 completed card posted ({new_mid:?})"
            );
            // Delete the buried tracked card best-effort; a delete failure
            // leaves a stale card visible, never a duplicate. Both outcomes
            // are logged — a vanished card must stay forensic (#16).
            if let Some((mid, _)) = state.plan_card(session_id).await
                && new_mid != mid
            {
                match bot.delete_message(chat, mid).await {
                    Ok(_) => {
                        tracing::info!("Telegram plan card restick deleted stale card ({mid:?})")
                    }
                    Err(e) => {
                        tracing::warn!("Telegram plan card restick delete failed ({mid:?}): {e}")
                    }
                }
            }
            state.take_plan_card(session_id).await;
            abort_guard.armed = false;
            true
        }
        None => {
            // Post failed (flood/API): fall back to editing the tracked card
            // in place so the completed form is still shown.
            let Some((mid, _)) = state.plan_card(session_id).await else {
                // Tracked card vanished mid-finalize: nothing left to edit,
                // and reposting from here would resurrect a deliberately
                // removed card. Terminal — consume the flag (#16). Logged
                // (#16 round 2): a vanished card plus a dropped notice must
                // stay forensic.
                tracing::warn!(
                    "Telegram plan card finalize for session {session_id}: \
                     tracked card vanished mid-finalize — consuming \
                     just-archived flag, completion notice NOT posted"
                );
                crate::utils::plan_files::take_plan_just_archived(session_id).await;
                abort_guard.armed = false;
                return true;
            };
            let mut edited = false;
            if use_rich && let Some(rich) = &rich {
                match super::rich::api::edit_rich_markdown_media(
                    bot.api_url().as_str(),
                    bot.token(),
                    chat.0,
                    mid.0,
                    rich,
                    &[],
                    Some(&empty_kb),
                    "turn",
                    "-",
                )
                .await
                {
                    Ok(()) => edited = true,
                    Err(e) => tracing::warn!(
                        "Telegram plan card rich finalize edit failed ({mid:?}): {e}"
                    ),
                }
            }
            if !edited {
                match bot
                    .edit_message_text(chat, mid, html.clone())
                    .parse_mode(teloxide::types::ParseMode::Html)
                    .reply_markup(super::suggest_options::empty_keyboard())
                    .await
                {
                    Ok(_) => edited = true,
                    Err(e) => {
                        tracing::warn!("Telegram plan card finalize edit failed ({mid:?}): {e}")
                    }
                }
            }
            abort_guard.armed = false;
            if edited {
                // In-place edit LANDED: the completion notice is visible —
                // consume the flag and untrack (#16).
                crate::utils::plan_files::take_plan_just_archived(session_id).await;
                tracing::info!(
                    "Telegram plan card finalized in place ({mid:?}) for session \
                     {session_id} after post failure"
                );
                state.take_plan_card(session_id).await;
                true
            } else {
                // Post AND edit failed: keep the flag so the NEXT settle
                // retries finalize (#16). The stamp is durable; this WARN is
                // the visible retry trail until one lands.
                tracing::warn!(
                    "Telegram plan card finalize FAILED for session {session_id} \
                     (post + edit both failed) — just-archived flag retained, \
                     next settle retries"
                );
                false
            }
        }
    }
}

pub(crate) async fn remove_plan_card(
    bot: &Bot,
    chat: ChatId,
    state: &Arc<TelegramState>,
    session_id: Uuid,
) {
    // Same lock as refresh (#822). Removal clears tracking, so a refresh
    // interleaving here is guaranteed to see no card and post one, which is
    // the widest form of the race.
    //
    // Callers that ALREADY hold the lock must use remove_plan_card_locked
    // instead: the lock is not reentrant, so re-acquiring it deadlocks.
    let card_lock = state.plan_card_lock(session_id).await;
    let _guard = card_lock.lock().await;
    // #155: the card is going away, so the review footer goes with it — a
    // stale "✨ Review: …" must never resurface on a later plan in the same
    // session (the same stale-chrome family the goal-scoping filter fixed).
    state.clear_plan_review_delta(session_id).await;
    remove_plan_card_locked(bot, chat, state, session_id).await;
}

/// Settle-path plan-card tail (#69): the #62 re-stick block shared by BOTH
/// delivery paths. The normal settle runs it after final delivery; the
/// interrupt/replacement teardown (handle_message's cancel_token guard) runs
/// the SAME tail, because a preempted turn's `return Ok(())` used to skip the
/// re-stick entirely — during rapid-fire discussion the card stayed buried at
/// a stale position for as long as the interrupt chain lasted (#69 live
/// observation: zero normal settles for 2+ hours, resticks only from
/// stream_loop flow-burial). Same gates as the normal path, unchanged:
/// finalize-on-archive first, then the tracked-card claim on the shared
/// sticky-stack budget (#1150) — a cardless settle spends nothing and retries
/// on the next settle — then the in-place refresh riding G2, fresh post
/// riding G3 (#1211). No cooldown: the #814 legacy stays deleted.
///
/// `plan_kb` is passed by the caller: the normal path forwards the value its
/// settle render just computed (bit-identical to the pre-#69 block); the
/// interrupt path recomputes it for a finished turn (`load_plan_state_section`
/// with `turn_active == false`, mirroring the settle path's `refresh_sections`)
/// so the Approve/Discard keyboard rides the resticked card.
pub(crate) async fn restick_plan_card_after_turn(
    bot: &Bot,
    chat: ChatId,
    thread_id: Option<ThreadId>,
    state: &Arc<TelegramState>,
    agent: &AgentService,
    session_id: Uuid,
    plan_kb: PlanKb,
) {
    if crate::utils::plan_files::peek_plan_just_archived(session_id).await {
        // Plan completed THIS settle (#1158, #1231): finalize the tracked
        // card (✅ header, keyboard stripped, one-shot untrack) and re-stick
        // the completed card to the bottom of the thread instead of
        // re-sticking or refreshing a now-archived plan. Finalize consumes
        // the flag only after the notice LANDED (#16): a flood-aborted
        // finalize leaves it in place, so the next settle retries instead
        // of losing the completion notice forever.
        finalize_plan_card(bot, chat, thread_id, state, session_id).await;
    } else {
        // Gate the claim on a tracked card (in-memory only — cheap): a
        // cardless settle must not spend the sticky budget, or the
        // flow-block restick starves for 15s after EVERY settle (#62).
        if state.plan_card_cached(session_id).await.is_some()
            && state.claim_sticky_action(chat.0, TelegramState::STICKY_STACK_MIN_INTERVAL)
        {
            remove_plan_card(bot, chat, state, session_id).await;
        }
        refresh_plan_card(bot, chat, thread_id, state, agent, session_id, plan_kb).await;
    }
}

/// Removal body, for callers already holding the per-session card lock.
///
/// Split out because `refresh_plan_card` takes the lock and then needs to
/// remove on its no-content path. Calling the lock-taking version there
/// deadlocked the Telegram handler outright.
async fn remove_plan_card_locked(
    bot: &Bot,
    chat: ChatId,
    state: &Arc<TelegramState>,
    session_id: Uuid,
) {
    if let Some(mid) = state.take_plan_card(session_id).await {
        // #16: deleteMessage success used to be silent — a vanished card was
        // undetectable without cross-referencing a user report. Log both
        // outcomes so a removal is always forensic.
        match bot.delete_message(chat, mid).await {
            Ok(_) => {
                tracing::info!("Telegram plan card deleted ({mid:?}) for session {session_id}")
            }
            Err(e) => tracing::warn!("Telegram plan card delete failed ({mid:?}): {e}"),
        }
    }
}
