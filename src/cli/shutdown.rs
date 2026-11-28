//! The daemon's shutdown path (#1086).
//!
//! Before this module the daemon installed `tokio::signal::ctrl_c()` and
//! nothing else, which is **SIGINT only**. `systemctl --user restart` sends
//! **SIGTERM**, so the process took the kernel's default action and died
//! instantly, mid-tool:
//!
//! * no live child was cancelled, so every one of them was killed inside
//!   whatever tool call it happened to be in;
//! * no `JoinHandle` was ever collected — `take_join_handle` had no production
//!   caller at all — so nothing waited for a child to reach a clean stop;
//! * the terminal status write in a child's own exit block never ran, so its
//!   status file stayed `Running` forever and the next boot's reconcile had to
//!   infer `Interrupted` from a dead process.
//!
//! The drain is ordered, and the order is the point: raise the shutdown flag
//! **before** cancelling (a cancellation that arrives with the flag still clear
//! is indistinguishable from a user stop, and the tool loop deletes the
//! recovery row of a turn it believes was abandoned — #481), then cancel, then
//! wait within a bound, then record, then release the profile locks **last** so
//! a replacement process cannot claim the profile while this one still has
//! children unwinding.

use std::sync::Arc;
use std::time::Duration;

use crate::brain::tools::subagent::{DrainOutcome, SubAgentManager};

/// Which signal asked this process to stop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopSignal {
    /// `SIGTERM` — what `systemctl stop` / `restart` sends.
    Terminate,
    /// `SIGINT` — Ctrl+C, or a `kill -2`.
    Interrupt,
}

impl StopSignal {
    /// Name for the log line and the shutdown notice.
    pub fn as_str(self) -> &'static str {
        match self {
            StopSignal::Terminate => "SIGTERM",
            StopSignal::Interrupt => "SIGINT",
        }
    }
}

/// Wait for a request to stop: `SIGTERM` **or** `SIGINT` on unix.
///
/// Both are handled because they mean the same thing to the operator and only
/// one of them was ever caught. `signal()` is used rather than `ctrl_c()` even
/// for SIGINT: the two arms then come from the same mechanism, so there is no
/// chance of the SIGINT arm silently swallowing an error the SIGTERM arm would
/// have reported.
///
/// Registration happens when this future is FIRST POLLED, which is why the
/// caller awaits it directly rather than spawning it — a handler installed late
/// leaves a window where `systemctl restart` still kills the process outright.
///
/// A registration failure is logged and that arm is dropped, not propagated:
/// losing SIGTERM handling degrades to the old behaviour (the process dies),
/// while refusing to run the daemon at all would be a worse trade for a signal
/// that is only needed at exit.
#[cfg(unix)]
pub async fn wait_for_stop_signal() -> StopSignal {
    use tokio::signal::unix::{SignalKind, signal};

    let mut terminate = match signal(SignalKind::terminate()) {
        Ok(s) => Some(s),
        Err(e) => {
            tracing::error!(
                "Could not install a SIGTERM handler ({e}); a service restart will kill live \
                 sub-agents mid-tool with no terminal write"
            );
            None
        }
    };
    let mut interrupt = match signal(SignalKind::interrupt()) {
        Ok(s) => Some(s),
        Err(e) => {
            tracing::error!("Could not install a SIGINT handler ({e})");
            None
        }
    };

    // `select!` over `Option`s: an arm whose handler failed to install is
    // replaced by a future that never resolves, so the surviving arm still
    // works instead of the whole wait erroring out.
    tokio::select! {
        _ = async {
            match terminate.as_mut() {
                Some(sig) => { sig.recv().await; }
                None => std::future::pending::<()>().await,
            }
        } => StopSignal::Terminate,
        _ = async {
            match interrupt.as_mut() {
                Some(sig) => { sig.recv().await; }
                None => std::future::pending::<()>().await,
            }
        } => StopSignal::Interrupt,
    }
}

/// Non-unix fallback: Ctrl+C is the only portable stop request.
#[cfg(not(unix))]
pub async fn wait_for_stop_signal() -> StopSignal {
    let _ = tokio::signal::ctrl_c().await;
    StopSignal::Interrupt
}

/// The full ordered shutdown sequence, after a stop signal has arrived.
///
/// Steps, in this order and for these reasons:
///
/// 1. **Raise the shutdown flag** (`mark_shutting_down`) — before any cancel, so
///    a child's exit block reads "the process is going down" instead of "my
///    parent stopped me", and the tool loop keeps recovery rows it would
///    otherwise delete (#481, #1462).
/// 2. **Drain the children** — cancel, bounded wait, terminal records. See
///    [`SubAgentManager::drain_for_shutdown`]. With no live children this is a
///    single lock read and an immediate return: no sleep, no log line, no disk.
/// 3. **Release the profile locks** — last, because a replacement process that
///    claims the profile while children are still unwinding would start a
///    second set of children over the same session rows.
///
/// Returns the drain outcome for callers that want the counts. The drain logs
/// its own single-line summary when it had live children, so a caller that
/// ignores this return value loses nothing and duplicates nothing; with an
/// empty roster it logs nothing at all.
pub async fn drain_and_release(
    manager: Option<&Arc<SubAgentManager>>,
    deadline: Duration,
) -> DrainOutcome {
    // Step 1.
    crate::brain::agent::service::shutdown::mark_shutting_down();

    // Step 2.
    let outcome = match manager {
        Some(mgr) => mgr.drain_for_shutdown(deadline).await,
        // No manager was wired (a very early failure, or a build path that
        // never registers sub-agent tools). Nothing to drain; the flag is
        // still raised, which is what the tool loop and the child exit block
        // read.
        None => DrainOutcome::default(),
    };

    // Step 3.
    crate::config::profile::release_all_locks();

    outcome
}
