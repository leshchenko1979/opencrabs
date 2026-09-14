//! Startup reconciliation of sub-agent status files (#1038, #192).
//!
//! A sub-agent runs inside a `tokio::spawn`ed task, so it dies with the
//! process. Its status file does not: it stays on disk in whatever state it
//! last wrote, which for a killed agent is `Pending` or `Running` forever.
//! Anything reading those files then sees a live agent that no longer exists.
//!
//! Prior to #192, this pass read legacy `<home>/tmp/subagents`, but sub-agents
//! write unified status files to `<home>/tmp/detached` since #26.
//!
//! This pass runs once at startup, before any new agent is spawned. Every
//! non-terminal sub-agent file belongs to a dead process by definition, so each
//! is moved to [`WorkState::Interrupted`] and returned to the caller, which
//! decides how to report it into the originating session.

use std::time::Duration;

use crate::brain::agent::service::work_status::{self, WorkKind, WorkStatus};

/// How long a terminal status file is kept before the sweep removes it.
/// Matches the 7 days the module has always documented.
pub const STALE_AFTER: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// Move every non-terminal sub-agent status file to `Interrupted`, then sweep
/// files that have aged out.
///
/// Returns the agents that were interrupted, oldest file first, so the caller
/// can tell their sessions. An agent whose file cannot be re-written is still
/// returned: the session deserves the report even when the file is stuck, and
/// the write failure is logged rather than swallowed.
///
/// Ordering matters. Reconciliation runs before the sweep so an interrupted
/// agent gets its `completed_at` stamped first and ages out on the same
/// schedule as any other terminal state, instead of lingering as an
/// unexplained file.
pub fn reconcile_orphaned_agents() -> Vec<WorkStatus> {
    let interrupted = mark_orphans_interrupted();

    match work_status::cleanup_stale(STALE_AFTER) {
        Ok((scanned, removed)) if removed > 0 => {
            tracing::info!(
                target: "subagent",
                "Swept {removed} stale status file(s) of {scanned} scanned"
            );
        }
        Ok(_) => {}
        Err(e) => {
            // Costs disk, not correctness, so startup continues.
            tracing::warn!(target: "subagent", "Status sweep failed: {e}");
        }
    }

    interrupted
}

/// Walk the unified status directory and interrupt sub-agents still mid-flight.
fn mark_orphans_interrupted() -> Vec<WorkStatus> {
    let dir = work_status::status_dir();
    if !dir.exists() {
        return Vec::new();
    }

    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(e) => {
            tracing::warn!(
                target: "subagent",
                "Could not read status dir {}: {e}",
                dir.display()
            );
            return Vec::new();
        }
    };

    let mut orphans: Vec<WorkStatus> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|e| e != "json") {
            continue;
        }
        // A file we cannot read or parse is not silently skipped: it may be a
        // stranded agent, and going quiet here is how the original bug hid.
        let data = match std::fs::read_to_string(&path) {
            Ok(data) => data,
            Err(e) => {
                tracing::warn!(
                    target: "subagent",
                    "Could not read status file {}: {e}",
                    path.display()
                );
                continue;
            }
        };
        let mut status: WorkStatus = match serde_json::from_str(&data) {
            Ok(status) => status,
            Err(e) => {
                tracing::warn!(
                    target: "subagent",
                    "Could not parse status file {}: {e}",
                    path.display()
                );
                continue;
            }
        };
        if status.kind != WorkKind::Agent || status.state.is_terminal() {
            continue;
        }
        if let Err(e) = status.mark_interrupted() {
            tracing::error!(
                target: "subagent",
                "Sub-agent {} was interrupted by a restart but its status could not be updated, \
                 so it will keep reading as running: {e}",
                status.id
            );
        }
        tracing::warn!(
            target: "subagent",
            "Sub-agent '{}' ({}) for session {} was interrupted by a restart",
            status.label,
            status.id,
            status.parent_session_id.as_deref().unwrap_or(&status.session_id)
        );
        orphans.push(status);
    }

    orphans.sort_by(|a, b| a.spawned_at.cmp(&b.spawned_at));
    orphans
}
