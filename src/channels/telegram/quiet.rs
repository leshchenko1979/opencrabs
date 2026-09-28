//! Quiet mode for groups (#679).
//!
//! A human group reads its scrollback as an archive to be scanned, not a place
//! to watch an agent work. In such a room the bot's output should not dominate
//! the room it shares with people, so quiet mode changes two things, both
//! scoped to the opting group:
//!
//! * **no intermediate promotion** — mid-turn narration folds into the
//!   collapsed processing log instead of opening its own bubble (the gate lives
//!   in `delivery::handle_intermediate`, which owns the media carve-out);
//! * **the previous turn's answer is folded in place** when the next turn
//!   starts — this module.
//!
//! Net effect: **at most one open bot message per topic**. The one still-open
//! message is the one the reader just asked for; everything before it is one
//! tap away.
//!
//! Folding is an EDIT, never a delete. `deleteMessage` is capped at 48 h — no
//! bot exception, admins included — while editing one's own message has no time
//! limit. So history is foldable exactly because it is edited rather than
//! removed, and nothing is lost.
//!
//! Both planes are covered: the classic HTML plane by `<blockquote
//! expandable>`, the native rich plane by `<details>` / `RichBlockDetails`
//! (`is_open = false`). The wrapper is chosen from [`BubbleBody`] — the plane
//! the answer was actually delivered ON — because re-rendering a rich answer as
//! HTML would flatten its tables (#679).

use std::sync::Arc;

use teloxide::payloads::EditMessageTextSetters;
use teloxide::prelude::*;
use teloxide::types::{ChatId, InlineKeyboardMarkup, ParseMode, ThreadId};
use uuid::Uuid;

use super::state::{BubbleBody, TelegramState};

/// Longest summary line a folded rich message keeps visible. The collapsed
/// view IS the bot's footprint in the room, so it is one short line by design.
const SUMMARY_MAX_CHARS: usize = 60;

/// An empty inline keyboard — strips whatever buttons the folded message
/// carries. The same idiom the suggestion merge already uses to clear its own
/// dead controls (`PickRewrite`), and the same reasoning applies here: a new
/// turn has already dropped the previous turn's follow-up stash (#597), so
/// those buttons are dead on arrival and a collapsed message holding them would
/// be the worst of both.
fn strip_keyboard() -> serde_json::Value {
    serde_json::json!({ "inline_keyboard": [] })
}

/// Wrap an already-delivered classic-HTML body in the collapse primitive.
///
/// `<blockquote expandable>` is the same primitive the flow block has shipped
/// on since #451, so this adds no new rendering surface — a folded answer
/// collapses on exactly the clients the processing log already collapses on.
pub(crate) fn fold_html(body: &str) -> String {
    format!("<blockquote expandable>{body}</blockquote>")
}

/// Wrap an already-delivered markdown answer in the rich collapse primitive.
///
/// The blank line before `</details>` is deliberate and load-bearing: a body
/// ending on a `>` quote run would otherwise let a lazy continuation swallow
/// the closer, leaving the element unmatched — the failure mode #552 measured
/// as `RICH_MESSAGE_CONTENT_REQUIRED` rejections. A blank line terminates the
/// quote run and closes the element cleanly.
pub(crate) fn fold_markdown(body: &str) -> String {
    format!(
        "<details>\n<summary>{}</summary>\n\n{body}\n\n</details>",
        summary_line(body)
    )
}

/// Whether a delivered answer should be folded.
///
/// One guard, from the owner's scope for #679: a **one-line** answer is left
/// alone, because folding it would add an "edited" marker and an API call for
/// no visual gain. Lines are counted **non-blank** — a body of `"OK\n\n"` is
/// one line to a reader, and folding it would contradict the scope it exists to
/// honour.
///
/// There is deliberately no separate guard for buttons: the fold strips any
/// keyboard as part of the same edit, so a buttoned answer folds correctly
/// rather than being excluded from the feature.
pub(crate) fn should_fold(body: &str) -> bool {
    body.lines().filter(|l| !l.trim().is_empty()).count() > 1
}

/// A short, render-safe summary for a folded rich message: the answer's first
/// non-blank line, shorn of the block markers that would render as literal
/// markup inside a `<summary>` and capped to one visible line.
pub(crate) fn summary_line(body: &str) -> String {
    let first = body
        .lines()
        .map(|l| l.trim())
        .find(|l| !l.is_empty())
        .unwrap_or("");
    let shorn = first.trim_start_matches(|c: char| {
        c.is_whitespace() || matches!(c, '#' | '*' | '-' | '>' | '|' | '+' | '_' | '`')
    });
    let collapsed = shorn.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.is_empty() {
        return "Answer".to_string();
    }
    let capped: String = collapsed.chars().take(SUMMARY_MAX_CHARS).collect();
    if collapsed.chars().count() > SUMMARY_MAX_CHARS {
        format!("{capped}…")
    } else {
        capped
    }
}

/// Fold the previous turn's answer bubble in place, if one is retained for this
/// session (#679).
///
/// Returns whether a fold edit actually landed. Every early return is a silent
/// no-fold rather than an error: a missing answer, a one-liner and a failed
/// edit all leave the room exactly as it was, which is the safe direction —
/// folding is cosmetic, so it must never be able to lose or mangle an answer.
///
/// The retained bubble is TAKEN up front and put back if the edit fails, so a
/// transient API error costs a retry on the next turn rather than the reference
/// itself.
pub(crate) async fn fold_previous_answer(
    bot: &Bot,
    chat: ChatId,
    thread_id: Option<ThreadId>,
    session_id: Uuid,
    tg: &Arc<TelegramState>,
) -> bool {
    let Some(flow_state) = tg.flow_state_for(session_id).await else {
        return false;
    };
    let bubble = {
        let mut s = flow_state.lock().unwrap_or_else(|e| e.into_inner());
        s.published_answer.take()
    };
    let Some(bubble) = bubble else {
        return false;
    };

    let mid = bubble.message_id;
    let (folded, is_rich) = match &bubble.body {
        BubbleBody::Html(html) => {
            if !should_fold(html) {
                return false;
            }
            (fold_html(html), false)
        }
        BubbleBody::Markdown(md) => {
            if !should_fold(md) {
                return false;
            }
            (fold_markdown(md), true)
        }
    };

    let outcome = if is_rich {
        let strip = strip_keyboard();
        super::rich::api::edit_rich_markdown(
            bot.api_url().as_str(),
            bot.token(),
            chat.0,
            mid.0,
            &folded,
            Some(&strip),
            "quiet",
            "fold-prev-answer",
        )
        .await
    } else {
        bot.edit_message_text(chat, mid, &folded)
            .parse_mode(ParseMode::Html)
            .reply_markup(InlineKeyboardMarkup::default())
            .await
            .map(|_| ())
            .map_err(|e| anyhow::anyhow!(e.to_string()))
    };

    match outcome {
        Ok(()) => {
            tracing::info!(
                "Telegram quiet mode (#679): folded previous answer msg {} (thread {:?})",
                mid.0,
                thread_id.map(|t| t.0.0)
            );
            true
        }
        Err(e) => {
            // Put the reference back: a dropped socket must not cost the fold
            // for every later turn. The next turn retries.
            let mut s = flow_state.lock().unwrap_or_else(|e| e.into_inner());
            s.published_answer = Some(bubble);
            tracing::warn!(
                "Telegram quiet mode (#679): fold of msg {} failed, reference retained: {e}",
                mid.0
            );
            false
        }
    }
}
