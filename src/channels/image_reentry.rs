//! Post-delivery image re-entry, shared by Slack, Discord and WhatsApp (#319).
//!
//! #286 gave Telegram two correction ladders. The first — the in-loop
//! preflight [`crate::brain::agent::service::nudge::local_image_regen_nudge`] —
//! is already shared by every channel session. The second fires *after* the
//! turn ends, when an image passed extraction but the channel refused it at
//! send time (remote fetch failure, upload error, platform rejection, file
//! vanished between validation and send). Until #319 that ladder lived only in
//! the Telegram delivery path, so on the other three channels the user was told
//! about the failure while the model was not — it believed the image arrived.
//!
//! This module carries the shared half of that second ladder: the latch that
//! bounds it to one re-entry per user exchange, and the decision-plus-dispatch
//! that turns a failure list into a correction turn. Each channel supplies
//! three thin wirings (a latch field, a trigger site, a re-arm site) and no
//! logic of its own.
//!
//! The dispatch is a caller-supplied closure rather than the channel's agent
//! service, so the spend/refuse decision and the payload are unit-testable
//! without an agent, and this module stays free of agent dependencies.

use std::collections::HashSet;
use std::sync::Mutex;

use uuid::Uuid;

use crate::brain::agent::QueuedUserMessage;
use crate::utils::image::LocalImageFailure;

/// One post-delivery image re-entry per session per user exchange (#286).
///
/// The bound is what keeps the re-entry finite: the correction turn runs a full
/// tool loop and delivers through the same path, so an unlatched re-entry would
/// re-arm on its own failure. [`ImageReentryLatch::clear`] re-arms it for the
/// user's next message.
#[derive(Debug, Default)]
pub struct ImageReentryLatch {
    spent: Mutex<HashSet<Uuid>>,
}

impl ImageReentryLatch {
    pub fn new() -> Self {
        Self {
            spent: Mutex::new(HashSet::new()),
        }
    }

    /// Claim this session's ONE post-delivery image re-entry.
    ///
    /// Returns `true` the first time a session asks (and marks it spent),
    /// `false` afterwards.
    pub fn try_spend(&self, session_id: Uuid) -> bool {
        match self.spent.lock() {
            Ok(mut set) => set.insert(session_id),
            Err(e) => {
                // Poisoned: the safe answer is to refuse the re-entry rather
                // than risk an unbounded chain of synthetic turns. Same policy
                // as Telegram's latch (#286).
                tracing::error!(
                    "image re-entry latch unreadable for session {session_id}: {e}; \
                     refusing the re-entry"
                );
                false
            }
        }
    }

    /// Re-arm the post-delivery image re-entry for `session_id`.
    ///
    /// Called when the user sends their own next message: the previous exchange
    /// is over, so a delivery failure in the new one is a fresh failure and
    /// deserves its own correction turn.
    pub fn clear(&self, session_id: Uuid) {
        match self.spent.lock() {
            Ok(mut set) => {
                set.remove(&session_id);
            }
            Err(e) => tracing::error!(
                "could not re-arm the image re-entry latch for session {session_id}: {e}"
            ),
        }
    }
}

/// Decision plus dispatch for one post-delivery image failure (#319).
///
/// `true` when this call claimed the session's re-entry (the latch is spent
/// *before* the dispatch, so a failed dispatch cannot re-arm the chain).
/// `false` when there was nothing to report or the exchange already spent its
/// one re-entry — the caller then leaves it at the user-facing notice.
///
/// `dispatch` is the channel's enqueue path, normally
/// `|msg| agent.enqueue_session_message(session_id, msg)`.
pub fn enqueue_image_reentry(
    latch: &ImageReentryLatch,
    session_id: Uuid,
    failures: &[LocalImageFailure],
    dispatch: impl FnOnce(QueuedUserMessage) -> bool,
) -> bool {
    // Nothing failed → nothing to spend. Checked first so a clean turn never
    // burns the exchange's budget.
    if failures.is_empty() || !latch.try_spend(session_id) {
        return false;
    }

    let alert = format!(
        "{} image attachment(s) could not be delivered",
        failures.len()
    );
    let nudge =
        crate::brain::agent::service::nudge::local_image_delivery_failure_nudge(failures);
    let queued = dispatch(QueuedUserMessage::system(
        nudge,
        format!("🖼️ {alert} — asking the model to report them"),
    ));
    if queued {
        tracing::info!(
            "post-delivery image re-entry queued for session {session_id} \
             ({} failure(s))",
            failures.len()
        );
    } else {
        // The latch stays spent for this exchange (fail-closed, same bound as
        // Telegram); the next inbound user message re-arms it.
        tracing::warn!(
            "post-delivery image re-entry for session {session_id} could not be queued \
             ({} failure(s)); notice only this exchange",
            failures.len()
        );
    }
    true
}
