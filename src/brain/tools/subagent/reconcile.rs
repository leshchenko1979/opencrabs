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
//! This pass runs once per process start. Every non-terminal sub-agent file
//! written before the pass began belongs to a dead process by definition, so
//! each is moved to [`WorkState::Interrupted`] and returned to the caller, which
//! decides how to report it into the originating session. A file spawned WHILE
//! the pass walks belongs to this process and is left alone (#858).

use std::time::{Duration, SystemTime};

use crate::brain::agent::service::work_status::{self, WorkKind, WorkStatus};

/// How long a terminal status file is kept before the sweep removes it.
/// Matches the 7 days the module has always documented.
pub const STALE_AFTER: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// One pass over the status directory: interrupt every sub-agent still
/// mid-flight, and sweep the files that have aged out.
///
/// Returns the agents that were interrupted, oldest file first, so the caller
/// can tell their sessions. An agent whose file cannot be re-written is still
/// returned: the session deserves the report even when the file is stuck, and
/// the write failure is logged rather than swallowed.
///
/// # One pass, not two (#858)
///
/// This used to be two walks of the same directory: `mark_orphans_interrupted`
/// parsed every file, then `cleanup_stale` re-opened and re-parsed all of them
/// to reach the same struct. On a 30k-file status directory that was 30k extra
/// opens and parses on every boot, on the startup task. Each file is now read
/// and parsed once, and both decisions come from that one parse.
///
/// # Ordering is load-bearing
///
/// Interruption is decided before staleness, and a file interrupted in this pass
/// is never swept in this pass. [`WorkStatus::mark_interrupted`] stamps
/// `completed_at = now`, which makes the file fresh under the finish rule — but
/// the skip is explicit rather than incidental, so a failed re-write (where the
/// on-disk bytes and mtime are still the originals) cannot turn a
/// just-interrupted agent into a deletion. An interrupted agent ages out later
/// on the same schedule as any other terminal state.
pub fn reconcile_orphaned_agents() -> Vec<WorkStatus> {
    let started = std::time::Instant::now();
    let dir = work_status::status_dir();
    let cutoff = SystemTime::now()
        .checked_sub(STALE_AFTER)
        .unwrap_or(SystemTime::UNIX_EPOCH);

    let mut interrupted: Vec<WorkStatus> = Vec::new();
    let mut scanned = 0usize;
    let mut removed = 0usize;

    // A file that did not exist when this pass began cannot belong to a dead
    // process — and now that the pass runs detached (#858), a new agent CAN
    // start while it walks. Interrupting a live run's own status file would
    // corrupt it, so the boundary is this pass's start instant. Nothing can
    // spawn a sub-agent before `recover()` runs on any surface, so a file older
    // than this instant belongs to a previous process by construction.
    let pass_started_at = chrono::Utc::now();

    if dir.exists() {
        match std::fs::read_dir(&dir) {
            Ok(entries) => {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.extension().is_none_or(|e| e != "json") {
                        continue;
                    }
                    scanned += 1;

                    // A file we cannot read or parse is not silently skipped: it
                    // may be a stranded agent, and going quiet here is how the
                    // original bug hid. It stays eligible for the sweep by its
                    // mtime — a half-written file is not immortal.
                    let mut status = match std::fs::read_to_string(&path) {
                        Ok(data) => match serde_json::from_str::<WorkStatus>(&data) {
                            Ok(status) => Some(status),
                            Err(e) => {
                                tracing::warn!(
                                    target: "subagent",
                                    "Could not parse status file {}: {e}",
                                    path.display()
                                );
                                None
                            }
                        },
                        Err(e) => {
                            tracing::warn!(
                                target: "subagent",
                                "Could not read status file {}: {e}",
                                path.display()
                            );
                            None
                        }
                    };

                    let is_orphan = matches!(
                        status.as_ref(),
                        Some(s) if s.kind == WorkKind::Agent
                            && !s.state.is_terminal()
                            && !spawned_during_pass(s, &pass_started_at)
                    );
                    if is_orphan {
                        if let Some(mut orphan) = status.take() {
                            if let Err(e) = orphan.mark_interrupted() {
                                tracing::error!(
                                    target: "subagent",
                                    "Sub-agent {} was interrupted by a restart but its status could \
                                     not be updated, so it will keep reading as running: {e}",
                                    orphan.id
                                );
                            }
                            tracing::warn!(
                                target: "subagent",
                                "Sub-agent '{}' ({}) for session {} was interrupted by a restart",
                                orphan.label,
                                orphan.id,
                                orphan.parent_session_id.as_deref().unwrap_or(&orphan.session_id)
                            );
                            interrupted.push(orphan);
                        }
                        // Never sweep what this pass just interrupted.
                        continue;
                    }

                    if work_status::is_stale(status.as_ref(), &path, cutoff) {
                        match work_status::sweep_status_file(&path) {
                            Ok(()) => removed += 1,
                            Err(e) => {
                                // One stuck file must not abandon the rest of
                                // the pass: the interrupted reports still owe
                                // delivery.
                                tracing::warn!(
                                    target: "subagent",
                                    "Could not remove stale status file {}: {e}",
                                    path.display()
                                );
                            }
                        }
                    }
                }
            }
            Err(e) => {
                tracing::warn!(
                    target: "subagent",
                    "Could not read status dir {}: {e}",
                    dir.display()
                );
            }
        }
    }

    interrupted.sort_by(|a, b| a.spawned_at.cmp(&b.spawned_at));

    // Hand the session→run map to the boot-resume path (#848). This pass has
    // just read every agent file, so the map is free here — and it saves the
    // resume path a second walk of the same directory to answer one lookup.
    work_status::install_agent_index(
        interrupted
            .iter()
            .map(|s| (s.session_id.clone(), s.id.clone()))
            .collect(),
    );

    let elapsed_ms = started.elapsed().as_millis();
    let interrupted_count = interrupted.len();
    // Unconditional (#858). The old line spoke only when `removed > 0`, so a
    // boot that deleted nothing was silent *by construction* — which is why a
    // boot could show no sweep line and still have walked all 30k files. The
    // window that line sits in then had to be inferred from an absence. One
    // line per boot makes the pass attributable instead.
    tracing::info!(
        target: "subagent",
        "Status reconcile: {scanned} scanned, {removed} removed, \
         {interrupted_count} interrupted in {elapsed_ms} ms"
    );

    interrupted
}

/// Was this agent spawned after the pass began?
///
/// A file newer than the pass cannot belong to a process that has already died,
/// so it must not be interrupted — with the pass detached (#858), a live agent
/// can start while it walks. An unparseable `spawned_at` counts as OLD, which is
/// the pre-#858 answer: a file we cannot date is more likely stranded than live,
/// and a wrong guess costs one boot, never a lost run.
fn spawned_during_pass(
    status: &WorkStatus,
    pass_started_at: &chrono::DateTime<chrono::Utc>,
) -> bool {
    chrono::DateTime::parse_from_rfc3339(&status.spawned_at)
        .map(|spawned| spawned.with_timezone(&chrono::Utc) >= *pass_started_at)
        .unwrap_or(false)
}
