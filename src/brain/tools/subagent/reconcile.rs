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
//! the pass walks belongs to this process and is left alone (#858) — and so does
//! one WRITTEN while it walks, which is how a child that boot recovery revived
//! or that a parent resumed stops being clobbered (#1081).

use std::time::Duration;

use crate::brain::agent::service::work_status::{self, StaleWindows, WorkKind, WorkStatus};

/// One pass over the status directory: interrupt every sub-agent still
/// mid-flight, and sweep the files that have aged out.
///
/// # The window comes from config, per KIND (#849)
///
/// This pass used to sweep on one hard-coded 7-day window. It now
/// builds [`StaleWindows`] from the live config, so a finished detached
/// command's record ages out on `[agent.retention].command_runs` (default 2
/// days) while a sub-agent's keeps `[agent.retention].tool_output_files`
/// (default 7). The split matters because the two populations are read
/// differently: an agent's record backs revived routing and must outlive the
/// session that may come back for it, while a finished command's record is read
/// once, by this pass, and never again — 51.3 % of 30,112 such files were
/// already past two days on the host that filed #849.
///
/// A resolved value of `0` means "keep forever" and becomes a cutoff at
/// `SystemTime::UNIX_EPOCH`, which nothing is older than.
///
/// The split applies to FINISHED records only. A file with no finish, or one
/// that will not parse, takes the long window — see [`work_status::is_stale`].
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
///
/// # Last writer wins, and this pass must not be it (#1081)
///
/// A record is only interrupted when nothing has written it since the pass
/// began: [`written_during_pass`] checks the spawn stamp, the progress heartbeat
/// and the file's mtime, and the mtime is re-checked immediately before the
/// write so a child that lands its own update while this pass is deciding is
/// left running rather than overwritten with `Interrupted`.
pub fn reconcile_orphaned_agents() -> Vec<WorkStatus> {
    reconcile_with(stale_windows_from_config())
}

/// The pass itself, with its windows handed in (#849).
///
/// Split out so a test can pin an age without depending on the host's
/// `[agent.retention]`: the predicate is the thing under test, and reading the
/// live config to answer it would make a correct pass fail on an operator who
/// simply set a longer window.
pub fn reconcile_with(windows: StaleWindows) -> Vec<WorkStatus> {
    let started = std::time::Instant::now();
    let dir = work_status::status_dir();

    let mut interrupted: Vec<WorkStatus> = Vec::new();
    let mut scanned = 0usize;
    let mut removed = 0usize;

    // A file that did not exist when this pass began cannot belong to a dead
    // process — and now that the pass runs detached (#858), a new agent CAN
    // start while it walks. Interrupting a live run's own status file would
    // corrupt it, so the boundary is this pass's start instant. Nothing can
    // spawn a sub-agent before `recover()` runs on any surface, so a file older
    // than this instant belongs to a previous process by construction.
    //
    // The boundary is checked against three clocks, not one (#1081): the pass
    // used to read only `spawned_at`, which a REVIVED or RESUMED child keeps
    // from its original spawn. Boot recovery can hand such a child back to a
    // live session while this pass walks, and the pass then stamped
    // `Interrupted` over a child that had already delivered its report —
    // measured on the host that filed #1081, a 12,000-character report
    // survived on disk under a record reading `Interrupted`. A rewrite always
    // moves the file's mtime and any progress update restamps
    // `progress.updated_at`, so those two signals cover the revival the spawn
    // stamp cannot.
    let pass_started = PassStart::now();

    if dir.exists() {
        match std::fs::read_dir(&dir) {
            Ok(entries) => {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.extension().is_none_or(|e| e != "json") {
                        continue;
                    }
                    scanned += 1;

                    // The mtime of the bytes about to be read (#1081). Two
                    // uses: one of the three "written after this pass began"
                    // signals below, and the guard that stops a rewrite landing
                    // between this read and our own write from being clobbered.
                    let read_mtime = path.metadata().and_then(|m| m.modified()).ok();

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
                            && !written_during_pass(s, read_mtime, &pass_started)
                    );
                    if is_orphan {
                        if let Some(mut orphan) = status.take() {
                            // Last look before the write (#1081). The decision
                            // above came from bytes read a moment ago, and a
                            // revived child can rewrite its record inside that
                            // window — the same clobber, only narrower. An
                            // unchanged mtime means nobody wrote since the read,
                            // so the decision still describes the file on disk.
                            if changed_since_read(&path, read_mtime) {
                                tracing::info!(
                                    target: "subagent",
                                    "Sub-agent {} was rewritten while the reconcile pass walked, \
                                     so it is live and was left alone",
                                    orphan.id
                                );
                                continue;
                            }
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

                    if work_status::is_stale(status.as_ref(), &path, windows)
                        && !changed_since_read(&path, read_mtime)
                    {
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

/// The two staleness windows, resolved from the live config (#849).
///
/// `Config::current()` is an `Arc` clone of the in-memory mirror — zero disk IO
/// — so this is safe on the blocking pool the pass now runs on, and it picks up
/// a config the watcher refreshed before boot reached here.
///
/// A resolved `0` means "keep forever". [`StaleWindows::from_ages`] expresses
/// that as a cutoff at `UNIX_EPOCH`, which nothing is older than, so a zero is
/// an operator saying "do not delete" rather than "delete everything".
fn stale_windows_from_config() -> StaleWindows {
    let config = crate::config::Config::current();
    let command_age = retention_age(config.agent.command_run_retention_days());
    let agent_age = retention_age(config.agent.tool_output_retention_days());
    StaleWindows::from_ages(command_age, agent_age)
}

/// `days` as an age, or `None` for "keep forever" when it is zero.
fn retention_age(days: u32) -> Option<Duration> {
    match days {
        0 => None,
        days => Some(Duration::from_secs(u64::from(days) * 24 * 60 * 60)),
    }
}

/// The instant this pass began, on both clocks a record can be compared against
/// (#1081).
///
/// One `now()` read, two representations: the status records carry RFC3339
/// strings and the filesystem carries a `SystemTime` mtime. Taking them from a
/// single clock read rather than two is what keeps the boundary consistent — a
/// second `now()` would place the wall-clock boundary microseconds after the
/// mtime boundary, and which way a record falls between them decides whether it
/// is clobbered.
struct PassStart {
    wall: chrono::DateTime<chrono::Utc>,
    mtime: std::time::SystemTime,
}

impl PassStart {
    fn now() -> Self {
        let mtime = std::time::SystemTime::now();
        Self {
            wall: chrono::DateTime::<chrono::Utc>::from(mtime),
            mtime,
        }
    }
}

/// Did anything write this record after the pass began?
///
/// Three signals, any one of which is enough to leave the file alone, because
/// each covers a case the others cannot (#1081):
///
/// - **`spawned_at`** — a live agent spawned while the pass walks (#858). The
///   original guard, and the only one that can see a file whose spawn stamp is
///   ahead of a coarse filesystem clock.
/// - **`progress.updated_at`** — the heartbeat of a run that was already going
///   when the pass started: [`WorkStatus::update_progress`] restamps it on every
///   tool-loop iteration, so a child revived by boot recovery and still working
///   carries a stamp from after the pass began.
/// - **the file's own mtime** — every state transition rewrites the record
///   ([`WorkStatus::mark_completed`] and friends), so a child that FINISHED
///   during the pass has a fresh mtime even when neither timestamp above moved.
///   This is the arm that covers the case #1081 measured: a child that delivered
///   a 12,000-character report kept its original `spawned_at`, and its record
///   was stamped `Interrupted` anyway.
///
/// An unparseable stamp is not evidence of freshness, so it falls through to the
/// next signal rather than answering for it. A record that is unreadable in all
/// three senses counts as OLD — the pre-#858 answer: a file we cannot date is
/// more likely stranded than live, and a wrong guess costs one boot, never a
/// lost run.
fn written_during_pass(
    status: &WorkStatus,
    mtime: Option<std::time::SystemTime>,
    pass_started: &PassStart,
) -> bool {
    spawned_during_pass(status, &pass_started.wall)
        || progress_during_pass(status, &pass_started.wall)
        || mtime.is_some_and(|mtime| mtime >= pass_started.mtime)
}

/// Was this agent spawned after the pass began?
///
/// The #858 guard, unchanged in meaning: a file spawned during the pass belongs
/// to this process. It is NOT sufficient on its own (#1081) — a resumed child
/// keeps the stamp of its original spawn — which is why
/// [`written_during_pass`] is what the pass asks.
fn spawned_during_pass(
    status: &WorkStatus,
    pass_started_at: &chrono::DateTime<chrono::Utc>,
) -> bool {
    chrono::DateTime::parse_from_rfc3339(&status.spawned_at)
        .map(|spawned| spawned.with_timezone(&chrono::Utc) >= *pass_started_at)
        .unwrap_or(false)
}

/// Did this agent report progress after the pass began?
///
/// The signal a revived-but-still-working child leaves (#1081): it has not
/// reached a terminal state, so nothing restamps `spawned_at`, and its record
/// may already exist with an old one. A record with no progress snapshot has no
/// answer here and falls through to the mtime arm.
fn progress_during_pass(
    status: &WorkStatus,
    pass_started_at: &chrono::DateTime<chrono::Utc>,
) -> bool {
    status
        .progress
        .as_ref()
        .and_then(|p| p.updated_at.as_deref())
        .and_then(|ts| chrono::DateTime::parse_from_rfc3339(ts).ok())
        .is_some_and(|updated| updated.with_timezone(&chrono::Utc) >= *pass_started_at)
}

/// Did the file change between this pass reading it and this pass writing it?
///
/// The narrow form of the same race (#1081). [`written_during_pass`] answers
/// "was this record written after the pass STARTED", which a child that
/// rewrites its record in the microseconds between this pass's read and its own
/// write slips past: the read saw `Running`, the child wrote `Completed`, and
/// the pass would then stamp `Interrupted` over a finished report.
///
/// Only the COMPARISON is trusted here, never the value: both readings are of
/// the same file, so two `None`s (a path that cannot be stat-ed) compare equal
/// and the decision taken from the bytes stands. A file this pass could not
/// date before touching it is one whose only writer is this pass.
fn changed_since_read(path: &std::path::Path, read_mtime: Option<std::time::SystemTime>) -> bool {
    path.metadata().and_then(|m| m.modified()).ok() != read_mtime
}
