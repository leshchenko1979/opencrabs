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
//!
//! # Reconciliation only — retention is not a boot decision (#1081)
//!
//! This pass used to sweep aged-out records as it walked. That made boot the
//! ONLY retention trigger in the process, so a long-lived daemon grew the
//! directory without bound and every restart paid the whole bill: 294 s over
//! 16,913 files on the host that filed #1081, detached from boot so nothing
//! waited for it, competing with the recovery work that had just revived those
//! same children.
//!
//! Retention now runs on the periodic maintenance sweep
//! ([`crate::services::maintenance::MaintenanceService`]), which is the
//! scheduler this process already has. This pass marks genuinely live orphans
//! and nothing else.

use crate::brain::agent::service::work_status::{self, WorkKind, WorkStatus};

/// One pass over the status directory: interrupt every sub-agent still
/// mid-flight.
///
/// Returns the agents that were interrupted, oldest file first, so the caller
/// can tell their sessions. An agent whose file cannot be re-written is still
/// returned: the session deserves the report even when the file is stuck, and
/// the write failure is logged rather than swallowed.
///
/// # Retention moved out of this pass (#1081)
///
/// This used to sweep aged-out records in the same walk. Retention is not a
/// boot decision: it does not depend on the process having just started, and
/// making boot its only trigger meant a long-lived daemon never swept at all
/// while every restart paid for the whole backlog. The sweep now belongs to the
/// periodic maintenance pass, which resolves the same windows from the same
/// config (`[agent.retention].command_runs` / `tool_output_files`) via
/// [`work_status::cleanup_stale`].
///
/// # One pass, not two (#858, #1081)
///
/// This used to be two walks of the same directory: `mark_orphans_interrupted`
/// parsed every file, then `cleanup_stale` re-opened and re-parsed all of them
/// to reach the same struct. On a 30k-file status directory that was 30k extra
/// opens and parses on every boot, on the startup task. The two were merged into
/// one walk, and #1081 then took the retention half out of this pass entirely,
/// leaving the one read here.
///
/// # An interrupted agent is not swept in the same breath
///
/// [`WorkStatus::mark_interrupted`] stamps `completed_at = now`, which makes the
/// record fresh under the finish rule — so the retention sweep that follows this
/// pass on the maintenance schedule leaves it alone, and the agent ages out
/// later on the same schedule as any other terminal state. A failed re-write is
/// the case to keep in mind: the on-disk bytes and mtime are then still the
/// originals, so an old orphan that could not be updated stays old and is
/// removed by its mtime on the next sweep. That is the intended reading — a
/// record nobody could rewrite is a record nobody can report either.
///
/// # Last writer wins, and this pass must not be it (#1081)
///
/// A record is only interrupted when nothing has written it since the pass
/// began: [`written_during_pass`] checks the spawn stamp, the progress heartbeat
/// and the file's mtime, and the mtime is re-checked immediately before the
/// write so a child that lands its own update while this pass is deciding is
/// left running rather than overwritten with `Interrupted`.
pub fn reconcile_orphaned_agents() -> Vec<WorkStatus> {
    reconcile()
}

/// The pass itself.
///
/// Takes no windows any more (#1081): the pass decides only which records are
/// live, and the retention sweep that used to share this walk moved to the
/// periodic maintenance sweep. What is left here is a predicate over the
/// record's own timestamps and the file's mtime, which is why a test can drive
/// it with no config at all.
///
/// # It visits only the names that can answer it (#1081)
///
/// The directory holds two disjoint naming schemes, and only one of them can
/// hold a sub-agent: an agent record is `<id>.json`, a command record is
/// `<session>__<id>.json`. [`work_status::is_agent_status_file_name`] draws
/// that line from the name alone, so the pass skips command records without
/// opening them.
///
/// This is what bounds the boot. The host that filed #1081 logged
/// `Status reconcile: 16913 scanned, 286 removed, 7 interrupted in 294382 ms` —
/// every one of those files stat-ed, read and parsed, to interrupt 7 agents.
/// The rest of the directory is session-qualified command records
/// (`<session>__<id>.json`), which this pass can never act on, because only
/// agents are interrupted. The pass ran detached from boot, so nothing waited
/// for it while it competed with the recovery work that had just revived
/// children in the same directory. The few hundred remaining names are the ones
/// whose answer can change.
pub fn reconcile() -> Vec<WorkStatus> {
    let started = std::time::Instant::now();
    let dir = work_status::status_dir();

    let mut interrupted: Vec<WorkStatus> = Vec::new();
    // Two counts, because they answer different questions (#1081).
    //
    // `scanned` is files this pass actually OPENED — candidates, not directory
    // entries. It is what the `Status reconcile:` line reports, and that line's
    // shape is fixed because tooling parses it, so the count keeps its name and
    // its meaning: files visited, not files present. After #1081 it drops by
    // two orders of magnitude, which is the fix rather than a regression.
    //
    // `skipped` is everything the name gate turned away — session-qualified
    // command records plus the ledger and non-JSON entries — and it is logged
    // separately precisely because `scanned` shrank. The parsed line cannot
    // grow a field, so the difference is accounted for beside it rather than
    // silently dropped from it.
    let mut scanned = 0usize;
    let mut skipped = 0usize;

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
                    // Skip by NAME before touching the file (#1081). Only an
                    // agent record can answer this pass's question, and the
                    // naming says which those are without a stat, let alone a
                    // read: ~99 % of a long-lived directory is finished command
                    // records, and opening all of them was the 294 s boot.
                    //
                    // `skipped` counts them so the shrunken `scanned` below is
                    // accountable rather than merely smaller.
                    let name = entry.file_name();
                    // A name that is not UTF-8 cannot be an agent id — those are
                    // hex (`SubAgentManager::generate_id`) — so skipping it is
                    // the same decision the gate below would make.
                    let Some(name) = name.to_str() else {
                        continue;
                    };
                    if !work_status::is_agent_status_file_name(name) {
                        skipped += 1;
                        continue;
                    }
                    let path = entry.path();
                    scanned += 1;

                    // The mtime of the bytes about to be read (#1081). Two
                    // uses: one of the three "written after this pass began"
                    // signals below, and the guard that stops a rewrite landing
                    // between this read and our own write from being clobbered.
                    let read_mtime = path.metadata().and_then(|m| m.modified()).ok();

                    // A file we cannot read or parse is not silently skipped: it
                    // may be a stranded agent, and going quiet here is how the
                    // original bug hid. It stays eligible for the retention
                    // sweep by its mtime (#1081) — a half-written file is not
                    // immortal just because it never parses.
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
                    if is_orphan && let Some(mut orphan) = status.take() {
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

    // Why `scanned` is small now, on its own line (#1081). The line below is
    // parsed by tooling and cannot grow a field, so the accounting goes beside
    // it: without this, a boot that used to report ~17k scanned and now reports
    // ~200 reads like a pass that lost its files rather than one that stopped
    // opening the ones it could never act on.
    //
    // Worded for what the count actually holds — a session-qualified command
    // record AND a progress ledger both land here — so the line cannot be read
    // as claiming every skipped entry was a command.
    if skipped > 0 {
        tracing::info!(
            target: "subagent",
            "Status reconcile skipped {skipped} entries that cannot hold a sub-agent"
        );
    }
    // Unconditional (#858). The old line spoke only when something was removed,
    // so a boot that deleted nothing was silent *by construction* — which is why
    // a boot could show no sweep line and still have walked all 30k files. The
    // window that line sits in then had to be inferred from an absence. One line
    // per boot makes the pass attributable instead.
    //
    // `removed` is fixed at 0 (#1081): this pass no longer sweeps. The field
    // stays in the line because tooling parses it, and a boot that reports
    // "0 removed" is now a statement of fact rather than a hint.
    //
    // The shape of this line is load-bearing and must not change: tooling parses
    // it. Anything the pass wants to say that does not fit goes on its own line
    // above, which is why the skip count is reported separately.
    let removed = 0usize;
    tracing::info!(
        target: "subagent",
        "Status reconcile: {scanned} scanned, {removed} removed, \
         {interrupted_count} interrupted in {elapsed_ms} ms"
    );

    interrupted
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
