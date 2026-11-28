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
/// This used to hold **vacuously** for a `systemctl --user restart`: SIGTERM had
/// no handler (the daemon installed `tokio::signal::ctrl_c()`, SIGINT only), so
/// the process took the default action and died without unwinding
/// `run_tool_loop`, the delete never ran, and the row survived the swap. That is
/// precisely *why* a mid-turn swap left a resumable row.
///
/// **Since #1086 the daemon does handle SIGTERM**, and that vacuously-true
/// paragraph is now load-bearing instead: `cli::shutdown::drain_and_release`
/// raises this flag as its FIRST step, before it cancels a single child token,
/// because cancelling first would make every child's exit block read a
/// shutdown kill as a deliberate parent stop — and the tool loop would then
/// delete the recovery rows of turns the operator never abandoned.
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

/// Serialises every test that mutates the process-global shutdown flag.
///
/// The flag is one `AtomicBool` for the whole test binary, and libtest runs
/// cases in parallel threads of one process. A lock owned by ONE test module
/// only excludes that module's cases from each other, so the moment a second
/// module exercises the flag (`keeps_recovery_row` and `is_deliberate_stop`
/// both do) the two sets interleave and one's `reset_for_test` lands inside the
/// other's assertion. The lock lives here, beside the flag it protects, so a
/// third module cannot reintroduce the same bug by forgetting to share.
#[cfg(test)]
pub(crate) fn test_guard() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}
