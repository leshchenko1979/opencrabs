//! Whether this process is on its way down (#1462).
//!
//! A turn that ends carrying `AgentError::Cancelled` can mean two opposite
//! things, and the recovery ticket must be handled differently for each:
//!
//! * **The user stopped it** — Esc twice, `/stop`, `/discard`, a stop word.
//!   They abandoned the work, so the tracking row is deleted and the turn is
//!   never replayed.
//! * **The app is quitting under it** — Ctrl+C twice cancels the in-flight
//!   token before setting `should_quit` (`tui/app/state.rs`). The user asked
//!   for the process to end, not for the turn to be thrown away, so the row
//!   must survive for the next boot to resume.
//!
//! `CancellationToken` carries no reason, so the shutdown paths raise this
//! flag before cancelling and the tool loop reads it when deciding whether to
//! delete the row. Process-global on purpose: it describes the process, and
//! the reader is several layers below the TUI that sets it.
//!
//! Not needed for `/restart`, `/exit`, `/quit` or `TuiEvent::Quit`: those end
//! the process without cancelling, so the row already survives. They set the
//! flag anyway, so the meaning stays "this process is going down" rather than
//! "one particular key combination was pressed".

use std::sync::atomic::{AtomicBool, Ordering};

static SHUTTING_DOWN: AtomicBool = AtomicBool::new(false);

/// Mark the process as shutting down. Called before any shutdown path
/// cancels an in-flight turn.
///
/// **Invariant (#481) — call this BEFORE cancelling, never after.** The tool
/// loop reads the flag at its delete site (`tool_loop.rs:901-910`) to tell
/// *"the user abandoned this turn"* from *"the app is quitting under it"*. A
/// cancellation that arrives with the flag still clear is indistinguishable
/// from a user stop, so the turn's recovery row is deleted as abandoned and
/// the work is lost — the exact failure #481 exists to prevent.
///
/// This holds **vacuously** for a `systemctl --user restart` today: SIGTERM
/// has no handler (the daemon installs `tokio::signal::ctrl_c()`, SIGINT
/// only), so the process takes the default action and dies without unwinding
/// `run_tool_loop`, the delete never runs, and the row survives the swap.
/// That is precisely *why* a mid-turn swap leaves a resumable row. The
/// paragraph is here so it stays true the day someone adds a SIGTERM handler
/// that cancels in-flight turns.
pub(crate) fn mark_shutting_down() {
    SHUTTING_DOWN.store(true, Ordering::SeqCst);
}

/// True once a shutdown has begun.
pub(crate) fn is_shutting_down() -> bool {
    SHUTTING_DOWN.load(Ordering::SeqCst)
}

/// Should this turn's recovery row survive?
///
/// Generic over the success type so the decision can be exercised directly
/// without building an `AgentResponse`. Only a cancellation that coincides
/// with a shutdown keeps the row: a user-initiated stop still deletes it, and
/// a turn that merely *finished* during a shutdown deletes it too, or the next
/// boot would replay work that was already answered.
pub(crate) fn keeps_recovery_row<T>(
    result: &Result<T, crate::brain::agent::error::AgentError>,
) -> bool {
    matches!(
        result,
        Err(crate::brain::agent::error::AgentError::Cancelled)
    ) && is_shutting_down()
}

/// Test-only reset so cases cannot leak the flag into each other.
#[cfg(test)]
pub(crate) fn reset_for_test() {
    SHUTTING_DOWN.store(false, Ordering::SeqCst);
}
