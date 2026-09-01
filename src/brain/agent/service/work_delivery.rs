//! ONE completion path for detached work — both kinds (design #26, items 4+5).
//!
//! Two builders used to live on opposite sides of the tree
//! (`background_tasks::completion_message` for commands, `subagent::spawn::
//! completion_message` for agents) with two copies of the delivery match that
//! turned a [`Delivery`] into log lines. This module owns both jobs once:
//!
//! * [`work_completion`] — the one completion builder, parameterized by kind:
//!   tail size (50 lines vs 4000 chars), framing per kind, origin per kind.
//! * [`deliver_work_result`] — the one delivery function: every completion
//!   routes through [`deliver_to_session`] with `interrupt=true` (fork #13: a
//!   completion is the origin's own awaited work) and exactly one match that
//!   turns the outcome into log lines. Boot-time interrupted reports keep
//!   `restart_recovery::deliver_or_park` — a pre-route delivery with
//!   park-not-misdeliver semantics (#940/#1037) that completions must not
//!   inherit.

use uuid::Uuid;

use super::background_tasks::{CmdResult, tail_lines};
use super::session_routes::{Delivery, deliver_to_session};
use super::types::{BgTaskMeta, PushOrigin, QueuedUserMessage};

/// Which kind of detached work a completion belongs to (the `background_tasks.
/// kind` column vocabulary, mirrored so callers never hand-write the strings).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WorkKind {
    Command,
    Agent,
}

impl WorkKind {
    /// The DB `kind` value for this kind.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            WorkKind::Command => crate::db::KIND_COMMAND,
            WorkKind::Agent => crate::db::KIND_AGENT,
        }
    }

    /// The log subject this kind's completions report under.
    fn subject<'a>(self, label: &'a str, id: &'a str) -> String {
        match self {
            WorkKind::Command => format!("Background task '{label}' completion"),
            WorkKind::Agent => format!("Sub-agent {id}'s result"),
        }
    }
}

/// Everything a finished piece of detached work reports, per kind. The builder
/// matches on this instead of taking two separate functions, so the framing
/// rules have exactly one home.
#[derive(Debug, Clone)]
pub(crate) enum WorkPayload {
    /// A finished `sh -c` command (#722).
    Command {
        label: String,
        command: String,
        result: CmdResult,
        elapsed_secs: f32,
    },
    /// A finished sub-agent: output on success, error on failure.
    Agent {
        label: String,
        agent_id: String,
        outcome: std::result::Result<String, String>,
    },
}

/// How much of a sub-agent's output the pushed message carries. Enough to act
/// on, short of pasting a long transcript into the parent's context.
const PUSHED_OUTPUT_LIMIT: usize = 4000;

/// Keep the tail of a long output: the conclusion matters more than the
/// opening, same as the detached-command completion path.
fn truncate_output(output: &str) -> String {
    if output.chars().count() <= PUSHED_OUTPUT_LIMIT {
        return output.to_string();
    }
    let skip = output.chars().count() - PUSHED_OUTPUT_LIMIT;
    let tail: String = output.chars().skip(skip).collect();
    format!("…(truncated)\n{tail}")
}

/// Build the completion message for a finished piece of detached work — the
/// ONE builder for both kinds (#26 item 5). Pure, so the framing is
/// unit-testable without spawning anything.
///
/// Per kind:
/// * `Command` — status line + exit code, last 50 output lines, a typed
///   [`BgTaskMeta`] payload (#15) so the Telegram echo renders a receipt card,
///   origin [`PushOrigin::BackgroundTask`] (#1221 bubble policy).
/// * `Agent` — output or error, 4000-char tail with a full-report hint,
///   origin [`PushOrigin::SubAgent`] (parent reports; no echo bubble).
pub(crate) fn work_completion(payload: WorkPayload) -> QueuedUserMessage {
    match payload {
        WorkPayload::Command {
            label,
            command,
            result,
            elapsed_secs,
        } => command_completion(&label, &command, &result, elapsed_secs),
        WorkPayload::Agent {
            label,
            agent_id,
            outcome,
        } => agent_completion(&label, &agent_id, outcome.as_deref()),
    }
}

/// Command framing, moved verbatim from `background_tasks::completion_message`
/// (#722, #15, #1221).
fn command_completion(
    label: &str,
    command: &str,
    result: &CmdResult,
    elapsed_secs: f32,
) -> QueuedUserMessage {
    let status = if result.success {
        "exit 0 (success)".to_string()
    } else {
        format!("exit {} (failure)", result.code)
    };
    let tail = tail_lines(&result.output, 50);
    let context = format!(
        "[System: the background task you started has finished.\n\
         Task: {label}\n\
         Command: {command}\n\
         Status: {status}\n\
         Output (last 50 lines):\n{tail}\n\n\
         Report the result to the user and continue anything that was waiting on it. \
         Do not re-run the command — this IS its result.]"
    );
    let display = format!(
        "🔧 background task {}: {label}",
        if result.success { "finished" } else { "failed" }
    );
    let mut msg = QueuedUserMessage::system(context, display);
    // #1221: marks this delivery for the Telegram collapsible echo bubble.
    msg.origin = PushOrigin::BackgroundTask;
    // #15: typed receipt payload — the echo renders the card from this,
    // never from the `[System: ...]` context text.
    msg.bg_meta = Some(BgTaskMeta {
        success: result.success,
        label: label.to_string(),
        elapsed_secs,
        tail,
    });
    msg
}

/// Agent framing, moved verbatim from `subagent::spawn::completion_message`.
fn agent_completion(
    label: &str,
    agent_id: &str,
    outcome: std::result::Result<&str, &str>,
) -> QueuedUserMessage {
    let (context_text, display_text) = match outcome {
        Ok(output) => {
            let full_report_hint = if output.chars().count() > PUSHED_OUTPUT_LIMIT {
                format!(
                    "Preview truncated - the FULL untruncated report is available via the \
                     wait_agent tool with agent id {agent_id}.\n"
                )
            } else {
                String::new()
            };
            (
                format!(
                    "[System: the sub-agent you spawned has finished.\n\
                     Agent: {label} (id {agent_id})\n\
                     Status: completed\n\
                     Output:\n{}\n\n\
                     {full_report_hint}\
                     Report the result to the user and continue anything that was waiting on it. \
                     Do not re-spawn the agent — this IS its result.]",
                    truncate_output(output)
                ),
                format!("🤖 sub-agent finished: {label}"),
            )
        }
        Err(error) => (
            format!(
                "[System: the sub-agent you spawned has failed.\n\
                 Agent: {label} (id {agent_id})\n\
                 Status: failed\n\
                 Error: {error}\n\n\
                 Report the failure to the user and decide what to do about it. Do not assume the \
                 work was completed.]"
            ),
            format!("🤖 sub-agent failed: {label}"),
        ),
    };
    crate::brain::agent::QueuedUserMessage {
        context_text,
        display_text,
        origin: crate::brain::agent::PushOrigin::SubAgent,
        bg_meta: None,
    }
}

/// Deliver a finished work result to the session that started it — the ONE
/// delivery function for both kinds (#26 item 4). Routes through the gated
/// [`deliver_to_session`] with `interrupt=true` (fork #13: a completion is the
/// origin's own awaited work — it must reach the session even mid-turn) and
/// turns the outcome into log lines exactly once, under the kind's subject.
///
/// `target` scopes the log lines (`background_task` vs the agent lane's
/// unscoped target) so existing log filters keep working.
pub(crate) fn deliver_work_result(
    session_id: Uuid,
    kind: WorkKind,
    label: &str,
    agent_id: &str,
    target: &'static str,
    msg: QueuedUserMessage,
) -> Delivery {
    let subject = kind.subject(label, agent_id);
    // interrupt=true (fork #13): see doc. Channel-ownership, mid-turn and
    // redirect decisions live in exactly one place instead of being re-derived
    // per surface (#940, #17, #19).
    let outcome = deliver_to_session(session_id, msg, true);
    match &outcome {
        Delivery::Delivered => {
            tracing::info!(target, "{subject} reached session {session_id}");
        }
        Delivery::Redirected { to } => {
            // The origin was replaced on its channel while the work ran
            // (fork #17), so the result was REDIRECTED to the session that
            // owns the channel now (#19) — delivered, not dropped.
            tracing::info!(
                target,
                "{subject} for session {session_id} was redirected to session {to}, which now \
                 owns its channel"
            );
        }
        Delivery::Parked => {
            // Not lost: it leaves when the owning channel claims the session.
            tracing::info!(
                target,
                "{subject} for session {session_id} is parked until its channel claims the \
                 session"
            );
        }
        Delivery::NoRoute => {
            // The session is waiting on this either way, so say so rather
            // than returning quietly.
            tracing::warn!(
                target,
                "{subject} for session {session_id} had nowhere to go; the session will not \
                 hear about it"
            );
        }
        Delivery::RefusedInFlight { redirected_to } => {
            // Unreachable by construction: interrupt=true is passed above and
            // the fork #13 gate refuses only when interrupt is unset. Arm kept
            // explicit so a future call-site change cannot drop the outcome
            // silently (port seam: upstream's match has no catch-all).
            tracing::warn!(
                target,
                "{subject} for session {session_id} was refused by the mid-turn gate despite \
                 interrupt=true (redirected to {redirected_to:?})"
            );
        }
    }
    outcome
}
