//! Unified status files for detached work (#26 P1).
//!
#![allow(dead_code)]
// upstream rework (#1313-#1319) removed callers of the
// legacy/stale-status subset; kept intact for the forkwin port cycle to
// disposition (round-4/5 precedent: tag, don't delete).
//! One JSON status file per unit of detached work — both detached bash
//! commands (#1160) and spawned sub-agents (#1038) — at
//! `<opencrabs_home>/tmp/detached/<id>.json`. Before #26 this was two
//! copy-pasted modules (`service/detached_status.rs` for commands,
//! `tools/subagent/status.rs` for agents); the pattern is now shared: spawn
//! writes the record, the worker updates state/progress, and the terminal
//! transition stamps a `finish`. The main orchestrator can `read_file` these
//! at any time for real-time visibility — no `session_search` needed.
//!
//! Files older than 7 days are cleaned up on startup (both kinds now age out
//! — command files previously had no sweep at all).
//!
//! Legacy compat: pre-#26 binaries wrote agent files to `tmp/subagents/` in
//! the old schema. [`migrate_legacy_dir`] moves them into the unified dir at
//! startup, converting the schema, so an agent in flight across the binary
//! swap is still reconciled and reported (#1038). Legacy COMMAND files
//! already live in `tmp/detached/` but carry the old `DetachedTaskStatus`
//! schema and do not parse here; that is harmless — command interruption
//! recovery reads the DB row (#763), never the file — and they age out via
//! the mtime sweep.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::RwLock;
use std::time::{Duration, SystemTime};

/// Base directory for all detached-work status files.
pub fn status_dir() -> PathBuf {
    #[cfg(test)]
    {
        if let Some(p) = test_override::get() {
            return p;
        }
    }
    crate::config::opencrabs_home().join("tmp").join("detached")
}

/// Where pre-#26 binaries wrote sub-agent status files: always the
/// `subagents` sibling of [`status_dir`].
pub fn legacy_dir() -> PathBuf {
    status_dir().with_file_name("subagents")
}

#[cfg(test)]
pub(crate) mod test_override {
    use std::cell::RefCell;
    use std::path::PathBuf;
    thread_local! {
        static DIR: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
    }
    pub fn set(p: PathBuf) {
        DIR.with(|d| *d.borrow_mut() = Some(p));
    }
    pub fn get() -> Option<PathBuf> {
        DIR.with(|d| d.borrow().clone())
    }
    /// Reset the override so the production path is visible again — used by
    /// tests that assert on the real dir layout. Thread-local overrides
    /// otherwise persist across the test-runner's reused threads.
    pub fn clear() {
        DIR.with(|d| *d.borrow_mut() = None);
    }
}

/// Ensure the status directory exists.
pub fn ensure_dir() -> std::io::Result<()> {
    let dir = status_dir();
    if !dir.exists() {
        fs::create_dir_all(&dir)?;
    }
    Ok(())
}

/// Path to a specific work item's status file.
pub fn status_path(id: &str) -> PathBuf {
    status_dir().join(format!("{}.json", id))
}

/// Path to a sub-agent's progress ledger: an append-only sibling of the status
/// file (#852).
///
/// The extension is load-bearing. [`cleanup_stale`] sweeps on
/// `extension() == "json"`, so a `.progress.jsonl` is invisible to it and
/// `scanned`/`removed` keep meaning *status files* — a ledger is deleted with
/// its record, never on its own schedule, because its lifetime IS the agent's.
pub fn progress_path(id: &str) -> PathBuf {
    status_dir().join(format!("{}.progress.jsonl", id))
}

/// Remove a status file together with the progress ledger that belongs to it.
///
/// The ledger has no lifetime of its own (#852): it exists to narrate one agent,
/// so it goes with that agent's record. Left behind, it would accumulate
/// forever — every sweep in this module selects on `extension() == "json"` and
/// would never see a `.jsonl`.
///
/// One home for that pairing, because there are two sweepers now: the standalone
/// [`cleanup_stale`] and the merged boot pass in
/// `brain::tools::subagent::reconcile` (#858). A second copy of this block is
/// how one of them would quietly stop deleting ledgers.
pub fn sweep_status_file(path: &Path) -> std::io::Result<()> {
    fs::remove_file(path)?;
    if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
        let _ = fs::remove_file(progress_path(stem));
    }
    Ok(())
}

/// Path to a run's captured stream (#692).
///
/// A sibling stem of the status file — same `id` — so every surface prints one
/// handle for the whole run, while the FILE lives in `tmp/runs/` beside its
/// peers rather than among the status JSON. Both streams exist from the moment
/// the child is spawned: a reader that hits `ENOENT` cannot tell "nothing
/// written yet" from "never existed", and the second reading is the one that
/// gets mistaken for a dead run.
///
/// Captured separately rather than merged. The in-memory capture has always
/// merged them (`CmdResult.output`), and merging into a file is lossier than
/// merging at read time: once interleaved, the writer cannot be taken back
/// apart. A reader wanting the historical merged view concatenates them, which
/// is exactly what `CmdResult` does.
pub fn command_output_path(id: &str, stderr: bool) -> PathBuf {
    let ext = if stderr { "err" } else { "out" };
    runs_dir().join(format!("{id}.{ext}"))
}

/// Marker file recording that a run's capture hit its cap (#692, D7).
///
/// A separate file rather than a line in the stream on purpose: the stream is
/// the RUN's bytes, and appending harness bookkeeping to it means a reader that
/// has already consumed the cap cannot tell which trailing bytes were the
/// program's. Written once, on the first over-cap write, and swept with the
/// captures it describes — same directory, same leg, same window.
///
/// Per-stream, not per-run: stdout and stderr cap independently, and a single
/// marker would report a truncated run as truncated on a stream that never
/// reached the limit.
pub fn truncation_marker_path(id: &str, stderr: bool) -> PathBuf {
    let ext = if stderr { "err" } else { "out" };
    runs_dir().join(format!("{id}.{ext}.cap"))
}

/// What a `.cap` marker says about one stream's capture (#917).
///
/// Until this existed the marker was written and read by nobody: `task_output`
/// reported a cut capture exactly as it reported one that simply ended, and the
/// only hint was a note appended INTO the capped bytes — prose a caller has to
/// pattern-match, and which a `tail_lines` read can drop entirely. The body's
/// format lives here, beside the path function that names the file, so the
/// writer (`background_tasks`) and the reader (`task_output`) cannot drift
/// apart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TruncationMarker {
    /// The cap, in bytes, that the stream reached.
    pub cap: u64,
    /// When the cap was hit, RFC 3339 UTC. `None` for a marker written before
    /// the body carried an instant: it still reports the truncation, it just
    /// cannot date it.
    pub at: Option<String>,
}

impl TruncationMarker {
    /// The body as written to `<id>.<ext>.cap`.
    ///
    /// The instant sits INSIDE the existing sentence rather than on a second
    /// line so that a marker written by an older binary — which has no `at`
    /// clause — parses with the same reader: the instant is the only optional
    /// part of the format.
    pub fn body(&self) -> String {
        match &self.at {
            Some(at) => format!(
                "cap {} bytes reached at {at}; further bytes are not captured \
                 (the run itself keeps running)\n",
                self.cap
            ),
            None => format!(
                "cap {} bytes reached; further bytes are not captured \
                 (the run itself keeps running)\n",
                self.cap
            ),
        }
    }

    /// Parse a marker body.
    ///
    /// `None` means this is not a truncation record — an empty or foreign file
    /// must not be reported as a cut capture, because a false "truncated" is
    /// worse than no answer at all: it sends a reader looking for bytes that
    /// were never produced.
    pub fn parse(body: &str) -> Option<Self> {
        let rest = body.split_once("cap ")?.1;
        let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
        let cap = digits.parse().ok()?;
        let at = rest
            .split_once(" at ")
            .and_then(|(_, tail)| tail.split(';').next())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        Some(Self { cap, at })
    }
}

/// Directory holding captured run streams (#692).
///
/// Deliberately separate from [`status_dir`]: the status JSON is a few hundred
/// bytes of metadata, while streams grow with the run's output and need their
/// own retention. Keeping them together would put unbounded files under a
/// directory the sweeper was never told about — the exact shape of the 183-file
/// accumulation measured on #692.
pub fn runs_dir() -> PathBuf {
    #[cfg(test)]
    {
        if let Some(p) = test_override::get() {
            return p;
        }
    }
    crate::config::opencrabs_home().join("tmp").join("runs")
}

/// Ensure the stream directory exists.
pub fn ensure_runs_dir() -> std::io::Result<()> {
    let dir = runs_dir();
    if !dir.exists() {
        fs::create_dir_all(&dir)?;
    }
    Ok(())
}

/// Map an empty path string to `None` (#692).
///
/// A record that names no file is honest; one that names `""` reads as a path
/// and fails at open time with an error indistinguishable from a missing run.
/// Agent records have no stream paths, and pre-#692 command records have none
/// either — both must be `None`, never an empty string.
fn recorded_path(p: String) -> Option<String> {
    if p.is_empty() { None } else { Some(p) }
}

// ── Status data types ────────────────────────────────────────────────

/// What produced one unit of detached work.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WorkKind {
    /// A detached bash command (#1160). Never enters
    /// [`WorkState::AwaitingInput`].
    Command,
    /// A spawned sub-agent (#1038).
    Agent,
}

impl Default for WorkKind {
    /// A file without a `kind` field was written by pre-#26 sub-agent code —
    /// those are all agents.
    fn default() -> Self {
        WorkKind::Agent
    }
}

/// Shared state machine for both kinds of detached work (#26).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum WorkState {
    Pending,
    Running,
    /// Parked at a round boundary awaiting follow-up input (#1183): the
    /// round's work is done and its output is collectable, but the agent has
    /// not terminated. Written so a parked file stops reading `Running`,
    /// which misled every consumer into waiting on work that was already
    /// finished. Not terminal — [`Self::is_terminal`] is false. Agents only;
    /// commands never enter this state.
    AwaitingInput,
    Completed,
    Failed,
    /// The process that owned this work died before it reached a terminal
    /// state. Distinct from `Failed`: the work did not fail, it never
    /// finished, and nothing knows how far it got. Callers must not read it
    /// as either success or failure.
    Interrupted,
}

impl WorkState {
    /// Whether this state means the work is no longer doing anything.
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            WorkState::Completed | WorkState::Failed | WorkState::Interrupted
        )
    }

    /// The word a reader sees for this state.
    ///
    /// Lives on the enum rather than at each render site: `task_output` and
    /// `tasks_list` both name a run's state, and a second copy of this match
    /// would let the two tools describe the same run differently (#752).
    pub fn label(&self) -> &'static str {
        match self {
            WorkState::Pending => "pending",
            WorkState::Running => "running",
            WorkState::AwaitingInput => "awaiting input",
            WorkState::Completed => "completed",
            WorkState::Failed => "failed",
            // Not an error: the owning process died before the run reached a
            // terminal state, so how far it got is genuinely unknown. Reporting
            // it as "failed" would assert the command went wrong; reporting it
            // as "completed" would assert it went right. Neither is known.
            WorkState::Interrupted => {
                "interrupted (the process died mid-run; how far it got is unknown)"
            }
        }
    }
}

/// Snapshot of the latest tool-use event in a running sub-agent.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProgressSnapshot {
    #[serde(default = "usize::default")]
    pub iteration: usize,
    #[serde(default = "usize::default")]
    pub tool_count: usize,
    #[serde(default)]
    pub last_tool: Option<String>,
    #[serde(default)]
    pub last_event: Option<String>,
    #[serde(default)]
    pub updated_at: Option<String>,
}

/// Why a detached command ended (#786).
///
/// An exit code alone cannot answer this. `137` is what a SIGKILL encodes, and
/// three materially different events produce it: the operator's lifetime
/// ceiling killed the run, the kernel OOM-killed it, or the command ran
/// `exit 137` itself. Before this the harness destroyed the discriminator at
/// the source — `background_tasks::exit_code` read
/// `status.signal()` and folded it into `128 + signal`, discarding the signal.
///
/// `Ordinary` is the `Default` so a status file written before this change
/// reads back exactly as it renders today: no migration is owed, and an old
/// record is never silently re-described as a kill.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RunCause {
    /// The process exited on its own, with or without success.
    #[default]
    Ordinary,
    /// A signal ended it. `code` is the shell encoding (`128 + signal`).
    Signalled {
        /// The signal number, kept rather than folded into the exit code.
        signal: i32,
    },
    /// The operator's `agent.run_max_lifetime_secs` ceiling SIGKILLed the group.
    ///
    /// This is the only ending the HARNESS caused, so it is the one a caller
    /// most needs named: the run did not fail, it was stopped.
    Ceiling {
        /// The ceiling that was exceeded, in seconds.
        limit_secs: u64,
    },
    /// The child was never awaited, so nothing ran and no code is the command's.
    NotAwaited,
}

impl RunCause {
    /// The one-line rendering both readers emit.
    ///
    /// `code` is passed rather than stored because `Ordinary` is the only arm
    /// that needs it, and a second copy of the code inside this enum could
    /// disagree with the recorded one.
    pub fn describe(&self, code: i32) -> String {
        match self {
            Self::Ordinary => format!("exit {code}"),
            Self::Signalled { signal } => format!("signal {signal} (killed)"),
            Self::Ceiling { limit_secs } => format!("ceiling {limit_secs}s"),
            Self::NotAwaited => "not awaited (nothing ran)".to_string(),
        }
    }
}

/// Terminal outcome of one unit of detached work.
///
/// Commands fill the exit side (`success`/`code`/`elapsed_secs`/
/// `output_bytes`, #1160), agents the outcome side (`error`/
/// `output_full`, #147). `completed_at` is shared and is what the
/// stale sweep ages files out by.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkFinish {
    /// RFC3339 time of the terminal transition.
    pub completed_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub success: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<i32>,
    /// Why the run ended (#786). Absent in records written before the field
    /// existed, and absent for agents — both read as [`RunCause::Ordinary`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cause: Option<RunCause>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub elapsed_secs: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_bytes: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// The COMPLETE final output of a finished agent (#147). Replaces the
    /// 200-char `output_summary` stub — zero production code paths ever
    /// read the summary's content, so the full report replaces it as the
    /// only persisted record. `None` for commands and legacy files.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_full: Option<String>,
}

/// Exit information for a detached command (#1160). Bundled so
/// [`WorkStatus::finish_command`] stays under the clippy argument limit —
/// same shape as the pre-#26 `DetachedFinish`.
#[derive(Debug, Clone, Copy)]
pub struct CommandExit {
    pub success: bool,
    pub code: i32,
    /// Why the run ended (#786). Stamped by the arm that knows, never inferred
    /// from `code` at read time — `137` alone cannot tell a ceiling kill from
    /// an OOM kill from a command that exited `137`.
    pub cause: RunCause,
    pub elapsed_secs: f32,
    pub output_bytes: usize,
}

/// Where a command run's live artifacts are, plus its pid (#692).
///
/// Bundled rather than passed as three more arguments for the same reason
/// [`CommandExit`] is: it keeps the constructors under the clippy argument
/// limit. Everything here is known at spawn, before the child has produced a
/// byte — which is the point, because a reader that arrives mid-run must find
/// the paths already recorded rather than wait for the run to finish and
/// describe itself.
#[derive(Debug, Clone, Default)]
pub struct CommandPaths {
    /// Absolute path of the run's captured stdout stream.
    pub output_out: String,
    /// Absolute path of the run's captured stderr stream.
    pub output_err: String,
    /// The spawned shell's pid, when the platform gave us one.
    pub pid: Option<u32>,
}

impl CommandPaths {
    /// No recorded artifacts.
    ///
    /// The honest shape for a record created before #692, or for a caller with
    /// no streams to point at — an empty path would read as a path and fail at
    /// open time with an error indistinguishable from a missing run.
    pub fn none() -> Self {
        Self::default()
    }
}

fn finish_now() -> WorkFinish {
    WorkFinish {
        completed_at: now_rfc3339(),
        success: None,
        code: None,
        cause: None,
        elapsed_secs: None,
        output_bytes: None,
        error: None,
        output_full: None,
    }
}

/// What a sub-agent was permitted to do, recorded so the grant survives the
/// process (#852).
///
/// Before this, the grant lived only in the in-memory `SubAgent` and a
/// rehydrated agent therefore came back with whatever the rebuilding path
/// assumed. That is a privilege-widening bug in the making: a `read_only`
/// child restored as writable looks exactly like a normal resume. Recording
/// the three flags at spawn is what makes rehydration safe.
///
/// Deliberately has NO `Default`: a zeroed value reads `allow_nested: false`,
/// so any legacy record deserialised through a default would silently strip
/// nesting from an agent that had it. Absence is expressed as
/// `Option<AgentGrant>` — see [`WorkStatus::grant`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentGrant {
    pub read_only: bool,
    pub include_brain: bool,
    pub allow_nested: bool,
}

impl AgentGrant {
    pub fn new(read_only: bool, include_brain: bool, allow_nested: bool) -> Self {
        Self {
            read_only,
            include_brain,
            allow_nested,
        }
    }

    /// The pre-#852 grant, assumed for records written before the field
    /// existed. Widest of the three flags off, nesting on — i.e. exactly what
    /// `SubAgent::new` used to fall back to.
    pub fn legacy() -> Self {
        Self::new(false, false, true)
    }
}

/// Persisted status of one unit of detached work.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkStatus {
    pub id: String,
    #[serde(default)]
    pub kind: WorkKind,
    /// The session recorded at spawn. For agents this is what recovery
    /// routes the interruption report by (the pre-#26 `parent_session_id`
    /// value); for commands, the owning session.
    pub session_id: String,
    /// The session that spawned this agent (#110). `None` for legacy files
    /// written before the field existed and for detached commands. The
    /// boot-resume path uses it to deliver a revived agent's result to the
    /// session that is waiting on it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session_id: Option<String>,
    pub label: String,
    /// The work itself: the shell command for [`WorkKind::Command`], the
    /// prompt for [`WorkKind::Agent`].
    pub task: String,
    /// RFC3339 time of spawn.
    pub spawned_at: String,
    pub state: WorkState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub progress: Option<ProgressSnapshot>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finish: Option<WorkFinish>,
    /// The permissions the agent was spawned with (#852). `None` for
    /// commands and for records written before the field existed — read it
    /// through [`Self::effective_grant`], never directly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grant: Option<AgentGrant>,
    /// Absolute path of the run's captured stdout (#692). `None` for agents
    /// and for records written before the field existed.
    ///
    /// The path is what makes a live run reachable: before it, a detached
    /// command had a status file under a UUID the agent was never told, so
    /// nothing could read the output while it was still being produced.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_out: Option<String>,
    /// Absolute path of the run's captured stderr (#692). Same contract as
    /// [`Self::output_out`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_err: Option<String>,
    /// The spawned shell's pid (#692), so a reader can tell a live run from a
    /// dead one without a registry lookup — the registry is in-memory and does
    /// not survive a restart, the status file does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
}

impl WorkStatus {
    /// Create a sub-agent status in `Pending` state and write the JSON file
    /// (#1038).
    pub fn new_agent(
        id: &str,
        label: &str,
        session_id: &str,
        prompt: &str,
        parent_session_id: Option<&str>,
        grant: Option<&AgentGrant>,
    ) -> std::io::Result<Self> {
        ensure_dir()?;
        let status = Self {
            id: id.to_string(),
            kind: WorkKind::Agent,
            session_id: session_id.to_string(),
            parent_session_id: parent_session_id.map(str::to_string),
            label: label.to_string(),
            task: prompt.to_string(),
            spawned_at: now_rfc3339(),
            state: WorkState::Pending,
            progress: None,
            finish: None,
            grant: grant.copied(),
            output_out: None,
            output_err: None,
            pid: None,
        };
        status.write()?;
        Ok(status)
    }

    /// Create a detached-command status in `Running` state and write the
    /// JSON file (#1160). A command goes straight to work — no Pending
    /// phase — and it never enters `AwaitingInput`.
    pub fn new_command(
        id: &str,
        session_id: &str,
        label: &str,
        command: &str,
        paths: CommandPaths,
    ) -> std::io::Result<Self> {
        ensure_dir()?;
        // The stream directory is created on the spawn path, not by the first
        // reader, so the recorded paths are valid from the instant they are
        // recorded. An absent file and an empty one are indistinguishable to a
        // reader, and only one of them means "this run never existed" (#692).
        ensure_runs_dir()?;
        let status = Self {
            id: id.to_string(),
            kind: WorkKind::Command,
            session_id: session_id.to_string(),
            label: label.to_string(),
            task: command.to_string(),
            spawned_at: now_rfc3339(),
            state: WorkState::Running,
            progress: None,
            finish: None,
            grant: None,
            output_out: recorded_path(paths.output_out),
            output_err: recorded_path(paths.output_err),
            pid: paths.pid,
            parent_session_id: None,
        };
        status.write()?;
        Ok(status)
    }

    /// Rewrite a command record with exit information (#1160). Falls back
    /// to a minimal fresh record if the spawn write never landed.
    pub fn finish_command(
        id: &str,
        session_id: &str,
        label: &str,
        command: &str,
        exit: CommandExit,
    ) -> std::io::Result<()> {
        let mut status = Self::read(id).unwrap_or_else(|| Self {
            id: id.to_string(),
            kind: WorkKind::Command,
            session_id: session_id.to_string(),
            label: label.to_string(),
            task: command.to_string(),
            spawned_at: now_rfc3339(),
            state: WorkState::Running,
            progress: None,
            finish: None,
            grant: None,
            // The spawn write may have failed (best-effort by design), and the
            // stream paths are not recoverable from the exit information. Left
            // `None` rather than guessed: a fabricated path that 404s is worse
            // than an explicit absence, because the reader cannot tell it apart
            // from a run whose files were swept (#692).
            output_out: None,
            output_err: None,
            pid: None,
            parent_session_id: None,
        });
        status.state = if exit.success {
            WorkState::Completed
        } else {
            WorkState::Failed
        };
        let mut finish = finish_now();
        finish.success = Some(exit.success);
        finish.code = Some(exit.code);
        // Stamped from the caller that KNEW, never re-derived from `code`:
        // `137` is produced by a ceiling kill, an OOM kill and a literal
        // `exit 137` alike (#786).
        finish.cause = Some(exit.cause);
        finish.elapsed_secs = Some(exit.elapsed_secs);
        finish.output_bytes = Some(exit.output_bytes);
        status.finish = Some(finish);
        status.write()
    }

    /// Transition to `Running`.
    pub fn mark_running(&mut self) -> std::io::Result<()> {
        self.state = WorkState::Running;
        self.write()
    }

    /// Park at a round boundary awaiting follow-up input (#1183). Mirrors
    /// `SubAgentManager::mark_awaiting_input`: before this the file kept
    /// reading `Running` with no finish while the agent sat idle, so audits
    /// kept waiting on work that was already finished
    /// (`state: "Running"` on a parked agent, #1183). No finish is stamped —
    /// this is not a terminal state, and flipping back via
    /// [`Self::mark_running`] when input arrives is a normal transition.
    pub fn mark_awaiting_input(&mut self) -> std::io::Result<()> {
        self.state = WorkState::AwaitingInput;
        self.write()
    }

    /// Update the progress snapshot after each tool-loop iteration or tool execution (#155).
    pub fn update_progress(
        &mut self,
        iteration: usize,
        last_tool: Option<String>,
        last_event: Option<String>,
    ) -> std::io::Result<()> {
        let prev_tool_count = self.progress.as_ref().map(|p| p.tool_count).unwrap_or(0);
        self.progress = Some(ProgressSnapshot {
            iteration,
            tool_count: prev_tool_count,
            last_tool,
            last_event,
            updated_at: Some(now_rfc3339()),
        });
        self.write()
    }

    /// Update the progress snapshot with explicit tool execution count (#155).
    pub fn update_progress_tool_count(
        &mut self,
        iteration: usize,
        tool_count: usize,
        last_tool: Option<String>,
        last_event: Option<String>,
    ) -> std::io::Result<()> {
        self.progress = Some(ProgressSnapshot {
            iteration,
            tool_count,
            last_tool,
            last_event,
            updated_at: Some(now_rfc3339()),
        });
        self.write()
    }

    /// Mark the work completed with the COMPLETE final output (#147).
    pub fn mark_completed(&mut self, output_full: String) -> std::io::Result<()> {
        self.state = WorkState::Completed;
        let mut finish = finish_now();
        finish.output_full = Some(output_full);
        self.finish = Some(finish);
        self.write()
    }

    /// Mark the work failed with an error message.
    pub fn mark_failed(&mut self, error: String) -> std::io::Result<()> {
        self.state = WorkState::Failed;
        let mut finish = finish_now();
        finish.error = Some(error);
        self.finish = Some(finish);
        self.write()
    }

    /// Mark the work interrupted by a restart.
    ///
    /// A finish is stamped so [`cleanup_stale`] can age the file out on the
    /// same schedule as any other terminal state. Its `error` carries the
    /// reason rather than being left empty, so anything reading the file can
    /// tell a restart apart from a genuine failure. The wording is
    /// kind-aware but keeps the exact agent sentence from before #26.
    pub fn mark_interrupted(&mut self) -> std::io::Result<()> {
        self.state = WorkState::Interrupted;
        let subject = match self.kind {
            WorkKind::Command => "this command",
            WorkKind::Agent => "this agent",
        };
        let mut finish = finish_now();
        finish.error = Some(format!(
            "OpenCrabs restarted while {subject} was running, so it was killed before finishing"
        ));
        self.finish = Some(finish);
        self.write()
    }

    /// Read the persisted status for a work item, if the file exists.
    pub fn read(id: &str) -> Option<Self> {
        let path = status_path(id);
        if !path.exists() {
            return None;
        }
        let data = fs::read_to_string(&path).ok()?;
        serde_json::from_str(&data).ok()
    }

    /// The grant this record carries, falling back to [`AgentGrant::legacy`]
    /// for records written before #852 (#852).
    ///
    /// One reader, so "no grant recorded" can never be mistaken for "no
    /// permissions": the fallback is the pre-#852 behaviour, which is what such
    /// a record was actually produced under.
    pub fn effective_grant(&self) -> AgentGrant {
        self.grant.unwrap_or_else(AgentGrant::legacy)
    }

    /// Re-read this handle from disk, returning whether the file was there
    /// (#187 D2).
    ///
    /// A `WorkStatus` is a SNAPSHOT, not a live view: `read` is called once and
    /// every later write re-serialises whatever that copy held. That becomes a
    /// clobber as soon as a second writer updates the same file — the sub-agent
    /// progress callback writes `tool_count` through its own fresh read, and
    /// the round-end `update_progress`, whose `prev_tool_count` comes from the
    /// spawn-time copy, reset it to 0 at every round boundary. Reload before
    /// such a write so the other writer's fields survive it.
    ///
    /// A vanished file leaves the handle untouched: the caller is mid-write on
    /// a status the sweep may have aged out, and blanking the handle would turn
    /// a missing file into a wrong one.
    pub fn reload(&mut self) -> bool {
        match Self::read(&self.id) {
            Some(fresh) => {
                *self = fresh;
                true
            }
            None => false,
        }
    }

    /// Seconds since this work item spawned (#155). Drives the plan-review
    /// progress clock — the same wall-clock shape the flow chrome footer
    /// already uses, so the review note reads like a turn's own footer rather
    /// than inventing a second time format. `None` when `spawned_at` is
    /// unparseable or lies in the future, so a caller renders NO clock instead
    /// of a wrong one.
    pub fn elapsed_secs(&self) -> Option<u64> {
        let spawned = chrono::DateTime::parse_from_rfc3339(&self.spawned_at).ok()?;
        let secs = (chrono::Utc::now() - spawned.with_timezone(&chrono::Utc)).num_seconds();
        u64::try_from(secs).ok()
    }

    /// Persist status to disk. Uses atomic rename for crash safety.
    fn write(&self) -> std::io::Result<()> {
        let path = status_path(&self.id);
        ensure_dir()?;
        let tmp = path.with_extension("json.tmp");
        let data = serde_json::to_string_pretty(self).map_err(std::io::Error::other)?;
        let mut f = fs::File::create(&tmp)?;
        f.write_all(data.as_bytes())?;
        f.sync_all()?;
        fs::rename(tmp, path)
    }

    /// List all known status files (by work id).
    pub fn list_all() -> std::io::Result<Vec<String>> {
        let dir = status_dir();
        if !dir.exists() {
            return Ok(Vec::new());
        }
        let mut ids = Vec::new();
        for entry in fs::read_dir(&dir)? {
            let entry = entry?;
            if let Some(name) = entry.file_name().to_str()
                && let Some(id) = name.strip_suffix(".json")
            {
                ids.push(id.to_string());
            }
        }
        ids.sort();
        Ok(ids)
    }
}

/// What an installed index holds: the status dir it describes, plus
/// `child session id` → `status-file id`.
type AgentIndex = (PathBuf, HashMap<String, String>);

/// Boot-built index of mid-flight sub-agent files (#848).
///
/// The directory is part of the value on purpose. An index may only answer for
/// the directory it was built from, so a test that points [`status_dir`] at a
/// throwaway dir can never be answered out of another test's snapshot: a
/// mismatch reads as "no index", which falls back to the walk. That is what
/// keeps a process-global map safe beside the thread-local test override.
static AGENT_INDEX: RwLock<Option<AgentIndex>> = RwLock::new(None);

/// Record the session→run map the boot orphan pass built (#848).
///
/// Called once, by [`crate::brain::tools::subagent::reconcile`], which has just
/// read every agent file anyway — so the map costs nothing there and saves the
/// boot-resume path a second walk of the status dir. A duplicated child session
/// keeps the file the pass saw last; one session has one sub-agent in practice.
///
/// **The map must be COMPLETE for the status dir, not merely the files the
/// caller touched.** A miss is final once an index describes the current dir,
/// so an index built from a subset silently reports "no such run" for every
/// session outside it. [`crate::brain::tools::subagent::reconcile`] satisfies
/// this because its orphan pass collects every non-terminal agent file, which
/// is exactly the set [`WorkStatus::find_agent_by_session`] can match.
pub fn install_agent_index(index: HashMap<String, String>) {
    let mut slot = AGENT_INDEX.write().unwrap_or_else(|e| e.into_inner());
    *slot = Some((status_dir(), index));
}

/// The indexed run id for `session_id`, when an index describes the CURRENT
/// status dir.
///
/// `None` means "no index answers here, walk instead"; `Some(None)` means the
/// index answered and does not know this session.
fn indexed_agent_id(session_id: &str) -> Option<Option<String>> {
    let slot = AGENT_INDEX.read().unwrap_or_else(|e| e.into_inner());
    match slot.as_ref() {
        Some((dir, index)) if *dir == status_dir() => Some(index.get(session_id).cloned()),
        _ => None,
    }
}

impl WorkStatus {
    /// Find a sub-agent status whose **child** session is `session_id` and
    /// whose outcome is not yet decided (#110).
    ///
    /// The boot-resume path uses this to recognize a revived sub-agent
    /// session: its result must go to the spawning session, not to the
    /// recorded channel. `Interrupted` files qualify on purpose — upstream
    /// reconciliation marks the file at boot, before the resumed session
    /// finishes — as do `Running`/`Pending`/`AwaitingInput`. `None` for
    /// unknown, outcome-terminal (`Completed`/`Failed`), or non-agent work —
    /// those keep the existing resume behavior.
    ///
    /// Consults [`install_agent_index`]'s boot-built map first, so a resumed
    /// session costs one file read instead of a walk of the whole status dir
    /// (~25k files on a long-lived host, #848). The walk survives as the
    /// no-index path: it is what answers before boot recovery has run, and in
    /// tests that point the status dir at a throwaway directory.
    ///
    /// A miss is FINAL while an index describes this dir — no walk — which is
    /// sound for the only callers (`cli::ui`'s boot resume path, after
    /// [`crate::brain::tools::subagent::reconcile`]): every file such a row can
    /// name was written before the restart, so the boot pass saw it. A caller
    /// that runs after agents were created in this same process must re-install
    /// the index rather than read a miss as absence.
    pub fn find_agent_by_session(session_id: &str) -> Option<WorkStatus> {
        if let Some(indexed) = indexed_agent_id(session_id) {
            let status = Self::read(&indexed?)?;
            // Re-checked, never trusted: the file may have reached a terminal
            // state after the index was built.
            return (matches!(status.kind, WorkKind::Agent)
                && status.session_id == session_id
                && !matches!(status.state, WorkState::Completed | WorkState::Failed))
                .then_some(status);
        }
        let ids = Self::list_all().ok()?;
        ids.into_iter().filter_map(|id| Self::read(&id)).find(|s| {
            matches!(s.kind, WorkKind::Agent)
                && s.session_id == session_id
                && !matches!(s.state, WorkState::Completed | WorkState::Failed)
        })
    }

    /// Finalize the detached-COMMAND status files a restart left `Running`
    /// (#111 follow-up, Part D).
    ///
    /// [`crate::brain::agent::service::restart_recovery::report_interrupted`]
    /// accounts for a killed command through its DB row and notifies the
    /// owning session, but it never rewrites the status FILE — so every
    /// restart-killed command left a file reading `Running` forever, and
    /// every file reader (`tasks_list`, the waiter sweeps) saw work that no
    /// longer existed.
    ///
    /// `ids` is the index, and it is the reason this leg no longer reads the
    /// whole status dir (~25k files on a long-lived host, #848). A
    /// `background_tasks` row exists only while its command is believed to be
    /// running, so the rows that survive a boot ARE the commands the restart
    /// killed ([`crate::db::repository::background_task`]) — and the row id is
    /// the status-file id, because `background_tasks.rs` derives the file id
    /// from the same uuid it records.
    ///
    /// Two kinds of file are left alone by design: terminal files (nothing to
    /// fix), and a `Running` file whose row never landed. The spawn path logs
    /// that write failure as an error, and the 7-day status sweep bounds how
    /// long such a file can linger — the alternative is re-reading every file
    /// on every boot to catch a case the DB itself could not write.
    ///
    /// Deliberately emits NO notice: the DB-row path already reported the
    /// interruption, and a second notice would be a duplicate wake.
    /// Returns how many files were finalized.
    pub fn reconcile_stale_commands(ids: &[String]) -> usize {
        let mut finalized = 0usize;
        for id in ids {
            let Some(mut status) = Self::read(id) else {
                continue;
            };
            if status.kind == WorkKind::Command && status.state == WorkState::Running {
                match status.mark_interrupted() {
                    Ok(()) => finalized += 1,
                    Err(e) => tracing::warn!(
                        target: "background_task",
                        "Could not finalize stale command status '{id}': {e}"
                    ),
                }
            }
        }
        finalized
    }
}

// ── Legacy migration ─────────────────────────────────────────────────

/// On-disk schema of a pre-#26 sub-agent status file (`tmp/subagents/`).
#[derive(Deserialize)]
struct LegacySubagentStatus {
    id: String,
    label: String,
    parent_session_id: String,
    state: WorkState,
    prompt: String,
    started_at: String,
    #[serde(default)]
    progress: Option<ProgressSnapshot>,
    #[serde(default)]
    completed_at: Option<String>,
    #[serde(default)]
    error: Option<String>,
}

/// Move pre-#26 sub-agent status files from `legacy` into [`status_dir`],
/// converting them to the unified schema. Returns the number migrated.
///
/// Runs at startup before reconciliation, so an agent in flight across the
/// binary swap is still interrupted and reported (#1038): the dir changes
/// homes, the semantics must survive the upgrade.
pub fn migrate_legacy_dir(legacy: &Path) -> usize {
    let entries = match fs::read_dir(legacy) {
        Ok(entries) => entries,
        // No legacy dir = nothing to migrate — the common case.
        Err(_) => return 0,
    };
    let mut migrated = 0usize;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|e| e != "json") {
            continue;
        }
        let data = match fs::read_to_string(&path) {
            Ok(data) => data,
            Err(e) => {
                tracing::warn!(
                    target: "subagent",
                    "Could not read legacy sub-agent status file {}: {e}",
                    path.display()
                );
                continue;
            }
        };
        let old: LegacySubagentStatus = match serde_json::from_str(&data) {
            Ok(old) => old,
            Err(e) => {
                tracing::warn!(
                    target: "subagent",
                    "Could not parse legacy sub-agent status file {}: {e}",
                    path.display()
                );
                continue;
            }
        };
        if status_path(&old.id).exists() {
            // The unified dir already has a record for this id (written by
            // the new code): it is fresher by definition. Drop the duplicate.
            let _ = fs::remove_file(&path);
            continue;
        }
        let status = WorkStatus {
            id: old.id.clone(),
            kind: WorkKind::Agent,
            session_id: old.parent_session_id,
            parent_session_id: None,
            label: old.label,
            task: old.prompt,
            spawned_at: old.started_at,
            state: old.state,
            progress: old.progress,
            finish: old.completed_at.map(|completed_at| {
                let mut finish = WorkFinish {
                    completed_at,
                    success: None,
                    code: None,
                    elapsed_secs: None,
                    output_bytes: None,
                    error: None,
                    output_full: None,
                    // A legacy record predates #786 and never recorded why the
                    // run ended. `None` is "unknown", never "ordinary".
                    cause: None,
                };
                finish.error = old.error.clone();
                finish
            }),
            // A legacy record predates the persisted grant (#852). `None`
            // reads back as the pre-#852 behaviour via `effective_grant`, which
            // is exactly what this record was produced under.
            grant: None,
            // A legacy record is an AGENT's, and agents have neither capture
            // streams nor a process group (#692). `None`, not an empty string:
            // an empty path reads as a path and fails at open time with an
            // error indistinguishable from a swept run.
            output_out: None,
            output_err: None,
            pid: None,
        };
        match status.write() {
            Ok(()) => {
                let _ = fs::remove_file(&path);
                migrated += 1;
            }
            Err(e) => {
                // Leave the legacy file in place: a retry on the next boot
                // beats a silent loss of a possibly-stranded agent.
                tracing::warn!(
                    target: "subagent",
                    "Could not migrate legacy sub-agent status {} into the unified dir: {e}",
                    old.id
                );
            }
        }
    }
    if migrated > 0 {
        // Best effort: drop the legacy dir once it is empty.
        let _ = fs::remove_dir(legacy);
    }
    migrated
}

// ── Auto-cleanup ─────────────────────────────────────────────────────

/// The two retention windows the status sweep judges against (#849).
///
/// The split exists because the two kinds of record have different readers, not
/// because one is more valuable. A finished COMMAND's record is read at most
/// once — by the boot pass that finalizes it (#848) — and never again, so it
/// ages out on the short window. A sub-agent's record backs revived routing
/// ([`WorkStatus::find_agent_by_session`]) and is a rounding error in
/// population, so it keeps the long one.
///
/// A `None` age means "keep forever" and is expressed as a cutoff at
/// [`SystemTime::UNIX_EPOCH`]: nothing is older than the beginning of time, so
/// that kind is never swept. That is the same answer the row prunes give a `0`,
/// and it is the safe one — a zero is an operator saying "do not delete".
#[derive(Debug, Clone, Copy)]
pub struct StaleWindows {
    /// Files belonging to finished detached commands.
    pub command: SystemTime,
    /// Files belonging to sub-agents.
    pub agent: SystemTime,
}

impl StaleWindows {
    /// Build both cutoffs from two optional ages.
    pub fn from_ages(command_age: Option<Duration>, agent_age: Option<Duration>) -> Self {
        let now = SystemTime::now();
        let cutoff = |age: Option<Duration>| {
            age.and_then(|age| now.checked_sub(age))
                .unwrap_or(SystemTime::UNIX_EPOCH)
        };
        Self {
            command: cutoff(command_age),
            agent: cutoff(agent_age),
        }
    }

    /// One window for both kinds — the pre-#849 behaviour, and what a caller
    /// that is not about the split wants.
    pub fn uniform(age: Duration) -> Self {
        Self::from_ages(Some(age), Some(age))
    }

    /// The cutoff that applies to one kind of work.
    fn cutoff_for(&self, kind: &WorkKind) -> SystemTime {
        match kind {
            WorkKind::Command => self.command,
            WorkKind::Agent => self.agent,
        }
    }
}

/// Remove status files that have aged out: a finished file on the window its
/// KIND gets, and a file with no finish (or no parse) on its mtime.
///
/// Scope is the STATUS directory (`tmp/detached`) only. Captured streams in
/// `tmp/runs` belong to [`sweep_stale_runs`], whose single owner is
/// `logging::cleanup_old_temp_files` (#848) — see the note at the end of this
/// function for why the duplicate came out.
pub fn cleanup_stale(windows: StaleWindows) -> std::io::Result<(usize, usize)> {
    let dir = status_dir();
    if !dir.exists() {
        return Ok((0, 0));
    }

    let mut scanned = 0usize;
    let mut removed = 0usize;

    for entry in fs::read_dir(&dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().is_none_or(|e| e != "json") {
            continue;
        }
        scanned += 1;

        // One parse, one decision — `is_stale` is the same predicate the merged
        // boot pass uses (#858), so the two callers cannot drift apart.
        let should_delete = match fs::read_to_string(&path) {
            Ok(data) => match serde_json::from_str::<WorkStatus>(&data) {
                Ok(status) => is_stale(Some(&status), &path, windows),
                Err(_) => is_stale(None, &path, windows),
            },
            Err(_) => is_stale(None, &path, windows),
        };

        if should_delete {
            sweep_status_file(&path)?;
            removed += 1;
        }
    }

    // Captured streams of finished runs (#692) live in `runs_dir()`, a SIBLING
    // of the status dir, so the loop above never sees them — and the temp purge
    // deliberately does not recurse into subdirectories. They are NOT swept
    // here (#848): this leg used to run in BOTH this function and
    // `logging::cleanup_old_temp_files`, and both run on every boot
    // (`subagent/reconcile.rs` for this one, `main.rs` for the purge), so the
    // duplicate bought a second `readdir` of `tmp/runs` plus a `stat` per
    // capture on every start. That directory is the largest population on the
    // box (~52k files, 25.81 s cold on `ops`), which made the copy the most
    // expensive avoidable thing in a boot.
    //
    // `logging::cleanup_old_temp_files` is the remaining owner, and it is the
    // right one: it runs on every surface AND on every CLI invocation, so no
    // surface loses the leg. The reason this function used to carry it — that
    // it was "the TUI's startup path", with `cleanup_stale` reached from
    // `cli/ui.rs` — was never true: nothing in `cli/ui.rs` calls this function.
    Ok((scanned, removed))
}

/// Sweep expired captures (and their cap markers) out of [`runs_dir`] (#692).
///
/// Returns `(scanned, removed)`, counting only what this leg saw.
///
/// The status record is the authority on liveness, and that is the whole reason
/// this cannot be a plain mtime sweep: a run writes CONTINUOUSLY, so unlinking
/// a stream under a run that has not finished would silently truncate output the
/// agent is still reading — and it would do so on the one file class where the
/// writer never reopens the file to notice. A missing or unreadable status
/// record counts as NOT live, or a stream whose record was already aged out
/// would be stranded forever.
///
/// Cap markers are swept by the same rule and in the same pass: they share the
/// captures' directory and their run's liveness, and a marker that outlived its
/// capture would report a truncation for a stream that no longer exists.
///
/// # Order of the two tests (#847)
///
/// The **mtime test runs first**, and the status record is consulted only for a
/// file that is already stale. The order is not an optimisation of convenience:
/// a file that is not stale is not removed whether its run is live or not, so
/// reading its record first was work whose result could never change the
/// outcome.
///
/// Measured on the `ops` profile: a boot on 2026-11-15 spent **161.76 s** between
/// the config-load line and the purge line with 50,115 captures in the
/// directory; a census of that directory at 17:11:51Z found 50,349 files, of
/// which only **68** were older than the 7-day window. All but those 68 reads
/// and serde parses were therefore discarded, on every boot, before the database
/// opened.
///
/// The record is also read **once per run**, not once per file: `<id>.out`,
/// `<id>.err` and their `.cap` markers all resolve to the same id, so the cache
/// collapses a run's reads into one — up to four files per run (two streams plus
/// two truncation markers), two on the measured tree.
pub fn sweep_stale_runs(windows: StaleWindows) -> std::io::Result<(usize, usize)> {
    let runs = runs_dir();
    if !runs.exists() {
        return Ok((0, 0));
    }

    let mut scanned = 0usize;
    let mut removed = 0usize;

    // The cheap pre-filter (#847) uses the LATER of the two cutoffs, which is
    // the most permissive one: a later cutoff is a shorter age, so it admits
    // every file either window could take. A file it rejects is stale under
    // neither window and needs no status read. `max` also does the right thing
    // when one kind is "keep forever" — its cutoff is `UNIX_EPOCH`, the
    // earliest possible, so it cannot drag the gate down with it.
    let gate = windows.command.max(windows.agent);

    // The status record per run id, filled on demand (#847). Only runs with at
    // least one STALE file ever appear here, so a healthy tree leaves this empty
    // and the sweep costs one readdir plus one stat per file. The record is kept
    // whole rather than as a liveness bool since #849: the capture now follows
    // its record's KIND, so the sweep needs the kind as well as the finish.
    let mut status_by_run: HashMap<String, Option<WorkStatus>> = HashMap::new();

    for entry in fs::read_dir(&runs)? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        scanned += 1;

        // #847: the mtime test comes FIRST — see the doc block above.
        if !file_stale(&path, &gate) {
            continue;
        }

        // `<id>.out`, `<id>.err` and the markers `<id>.out.cap` / `<id>.err.cap`
        // all resolve to `<id>`: the stem is everything before the FIRST dot, so
        // a two-dot name still yields the run it belongs to. That is also why a
        // marker is written per stream rather than as one `<id>.cap` file —
        // `file_stem`-style derivation would collapse both streams onto one id
        // and the marker could not say which one truncated.
        let run_id = path
            .file_name()
            .and_then(|s| s.to_str())
            .map(|s| s.split('.').next().unwrap_or_default())
            .unwrap_or_default()
            .to_string();

        let status = status_by_run.entry(run_id.clone()).or_insert_with(|| {
            fs::read_to_string(status_path(&run_id))
                .ok()
                .and_then(|data| serde_json::from_str::<WorkStatus>(&data).ok())
        });

        // A run that has not finished is live, whatever its kind: unlinking a
        // stream under it would truncate output the agent is still reading.
        if status.as_ref().is_some_and(|s| s.finish.is_none()) {
            continue;
        }

        // The capture follows the KIND of its record (#849) — a finished
        // command's stream goes on the short window, a sub-agent's on the long
        // one. A record that is missing or unreadable cannot name a kind, so it
        // takes the long window: the same conservative answer `is_stale` gives
        // an unparsed file.
        let cutoff = match status.as_ref() {
            Some(status) => windows.cutoff_for(&status.kind),
            None => windows.agent,
        };
        if !file_stale(&path, &cutoff) {
            continue;
        }

        // #847: one unremovable capture must not abandon the rest of the leg.
        // Propagating here left every remaining capture unswept, and the caller
        // (`cleanup_old_temp_files`) logs a single warning and moves on — so the
        // next boot repeated the same work and failed on the same file. The
        // other three legs of that sweep already warn per file.
        match fs::remove_file(&path) {
            Ok(()) => removed += 1,
            Err(e) => tracing::warn!(
                "Failed to remove stale run capture {}: {e}",
                path.display()
            ),
        }
    }

    Ok((scanned, removed))
}

fn parse_completed_at(cutoff: &SystemTime, ts: &str) -> bool {
    // Naïve UTC parser — enough for RFC3339 without subseconds.
    let Ok(dt) = chrono::DateTime::parse_from_rfc3339(ts) else {
        return false; // can't parse — skip, let cleanup catch it later
    };
    let completed = SystemTime::UNIX_EPOCH
        .checked_add(Duration::from_secs(dt.timestamp() as u64))
        .unwrap_or(SystemTime::UNIX_EPOCH);
    completed < *cutoff
}

fn file_stale(path: &Path, cutoff: &SystemTime) -> bool {
    path.metadata()
        .and_then(|m| m.modified())
        .map(|mtime| mtime < *cutoff)
        .unwrap_or(true) // can't stat → delete to be safe
}

/// Is this status file old enough to sweep?
///
/// Exactly three shapes, and they are not interchangeable (#858):
///
/// - **Parsed, with a finish** → judged on its `completed_at`. A run that ended
///   days ago ages out even when its mtime is fresh.
/// - **Parsed, without a finish** → judged on its mtime. Covers in-flight runs
///   and states that never stamped an end.
/// - **Not parsed** (`status == None`: unreadable or corrupt) → judged on its
///   mtime. A half-written file is not immortal.
///
/// A finish whose `completed_at` cannot be parsed is **not** stale — the same
/// answer the pre-#858 inline predicate gave, since an unparseable timestamp
/// short-circuits the mtime test instead of falling through to it.
///
/// `path`'s mtime is read HERE, so a caller that rewrites the file first must
/// pass the status it parsed from the ORIGINAL bytes: [`WorkStatus::mark_interrupted`]
/// restamps the file, and an mtime read after that rewrite is the rewrite's.
/// Passing the post-interrupt status is safe for a different reason — its finish
/// now carries `completed_at = now`, so the first arm answers and the mtime is
/// never consulted.
///
/// # The window is chosen by KIND, not by caller (#849)
///
/// A finished record is judged on the cutoff its own kind gets: a detached
/// command on the short window, a sub-agent on the long one. The kind is read
/// from the parsed struct, so a caller cannot pass the wrong window for a file
/// it has already read — the one thing it could get wrong is which windows it
/// resolved, and that is [`StaleWindows`]' own contract.
///
/// The other two shapes ignore the split and take the LONG window: neither a
/// record with no finish nor an unreadable one can be reasoned about, and the
/// long window is the conservative answer for both.
pub fn is_stale(status: Option<&WorkStatus>, path: &Path, windows: StaleWindows) -> bool {
    match status {
        Some(status) => match status.finish.as_ref() {
            Some(finish) => {
                parse_completed_at(&windows.cutoff_for(&status.kind), &finish.completed_at)
            }
            // No finish: the state never stamped an end, so the KIND is not
            // enough to shorten its life — a `Running` record of either kind is
            // judged on mtime against the LONG window. Only a finished record
            // has a reader profile we can reason about (#849).
            None => file_stale(path, &windows.agent),
        },
        // Unparsed: a corrupt file cannot name a kind, and being unreadable is
        // not evidence of being disposable. Long window, as before.
        None => file_stale(path, &windows.agent),
    }
}

fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339()
}

#[cfg(test)]
mod run_cause_tests {
    use super::*;

    /// #786 — the whole point of the field: three endings that ALL record `137`
    /// must render as three different strings.
    ///
    /// Before this, a caller reading `task_output` saw `exit code: 137` for a
    /// run the harness killed at the operator's ceiling, a run the kernel
    /// OOM-killed, and a command that simply ran `exit 137`. The ceiling case is
    /// the one that matters most: the run did not fail, it was STOPPED, and the
    /// fix is a longer ceiling rather than a code change.
    #[test]
    fn ceiling_oom_and_literal_137_are_three_distinct_renderings() {
        let ceiling = RunCause::Ceiling { limit_secs: 3600 }.describe(137);
        let oom = RunCause::Signalled { signal: 9 }.describe(137);
        let literal = RunCause::Ordinary.describe(137);

        assert_eq!(ceiling, "ceiling 3600s");
        assert_eq!(oom, "signal 9 (killed)");
        assert_eq!(literal, "exit 137");

        // The assertion that would have caught the original defect.
        assert_ne!(
            ceiling, literal,
            "a ceiling kill must not render like a command that exited 137"
        );
        assert_ne!(ceiling, oom, "a ceiling kill is not an OOM kill");
        assert_ne!(oom, literal, "a signal is not an ordinary exit");
    }

    /// `Ordinary` needs the code, and only `Ordinary` does — so the code is
    /// passed at render time rather than duplicated inside the enum, where it
    /// could disagree with the recorded one.
    #[test]
    fn only_ordinary_renders_the_code() {
        assert_eq!(RunCause::Ordinary.describe(0), "exit 0");
        assert_eq!(RunCause::Ordinary.describe(7), "exit 7");
        // A non-zero code must not leak into the other arms' text.
        for cause in [
            RunCause::Signalled { signal: 15 },
            RunCause::Ceiling { limit_secs: 60 },
            RunCause::NotAwaited,
        ] {
            let rendered = cause.describe(143);
            assert!(
                !rendered.contains("143"),
                "{cause:?} must describe its own cause, not echo the code: {rendered}"
            );
        }
    }

    /// A status file written BEFORE #786 carries no `cause`. It must read back
    /// as `Ordinary` — i.e. render exactly as it does today — rather than fail
    /// to parse or be silently re-described as a kill.
    #[test]
    fn a_pre_change_record_reads_as_ordinary() {
        let legacy = r#"{
            "completed_at": "2026-10-01T00:00:00Z",
            "success": false,
            "code": 137
        }"#;
        let finish: WorkFinish = serde_json::from_str(legacy).expect("legacy record must parse");
        assert_eq!(
            finish.cause, None,
            "an absent cause stays absent — it is not invented"
        );
        assert_eq!(
            finish.cause.unwrap_or_default(),
            RunCause::Ordinary,
            "an old record renders as it did before the field existed"
        );
    }

    /// The enum round-trips through the status file, so a reader on a later
    /// binary sees the cause the finishing binary stamped.
    #[test]
    fn every_cause_round_trips_through_serde() {
        for cause in [
            RunCause::Ordinary,
            RunCause::Signalled { signal: 9 },
            RunCause::Ceiling { limit_secs: 3600 },
            RunCause::NotAwaited,
        ] {
            let json = serde_json::to_string(&cause).expect("serialize");
            let back: RunCause = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(back, cause, "round-trip lost the cause: {json}");
        }
    }
}

#[cfg(test)]
mod truncation_marker_tests {
    use super::*;

    /// #917 — the marker body is now read by a consumer, so the format has two
    /// ends. A body written by THIS binary must round-trip, cap and instant
    /// both: a reader that lost the cap could not say how much was cut, and one
    /// that lost the instant could not say when.
    #[test]
    fn a_marker_body_round_trips_through_the_reader() {
        let marker = TruncationMarker {
            cap: 1_048_576,
            at: Some("2026-11-20T09:08:45Z".to_string()),
        };
        assert_eq!(TruncationMarker::parse(&marker.body()), Some(marker));
    }

    /// A marker written before the body carried an instant — which is every
    /// marker already on disk — still reports its truncation. It simply cannot
    /// date it, and saying so is better than refusing to read the file.
    #[test]
    fn a_body_without_an_instant_still_reports_the_cap() {
        let body = "cap 8192 bytes reached; further bytes are not captured \
                    (the run itself keeps running)\n";
        assert_eq!(
            TruncationMarker::parse(body),
            Some(TruncationMarker {
                cap: 8192,
                at: None
            })
        );
    }

    /// A file that is not a truncation record must read as "no marker", never
    /// as "truncated at 0 bytes": a false positive sends the caller hunting for
    /// bytes that were never produced.
    #[test]
    fn a_foreign_or_empty_body_is_not_a_truncation_record() {
        assert_eq!(TruncationMarker::parse(""), None);
        assert_eq!(TruncationMarker::parse("hello\n"), None);
        assert_eq!(TruncationMarker::parse("cap nope bytes\n"), None);
    }
}
