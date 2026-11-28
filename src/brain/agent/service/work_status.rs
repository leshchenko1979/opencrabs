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
//! Naming (#991): a COMMAND record written by this build is
//! `<session_id>__<id>.json`, so one session's runs are a prefix glob
//! ([`list_session_runs`]) instead of a scan of the whole directory. Sub-agent
//! records keep `<id>.json`, and every record written before #991 keeps its
//! name — [`status_path`] resolves an id to whichever of the two exists, so
//! readers and writers that know only the id are unchanged.
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

/// Legacy status-file name for a work item: `<id>.json` (#991).
///
/// Still the name of every record written before #991 — and of every SUB-AGENT
/// record, which #991 deliberately left alone.
fn legacy_status_path(id: &str) -> PathBuf {
    status_dir().join(format!("{}.json", id))
}

/// Path to a specific work item's status file — the single resolver every
/// reader and writer goes through (#991).
///
/// Two names exist on disk, and a caller knows only the id:
///
/// 1. `<id>.json` — every record written before #991, and every sub-agent
///    record since. Returned whenever it exists, so a legacy run keeps reading
///    and keeps being REWRITTEN in place.
/// 2. `<session_id>__<id>.json` — a command record written by #991, named for
///    the session that started it so "which runs belonged to session X" is a
///    prefix glob ([`list_session_runs`]) rather than a parse of every file in
///    a directory that reaches ~16k entries on a long-lived host.
///
/// With neither on disk the legacy name is returned: it is the not-found
/// answer, and the name a first write would use if it were not told otherwise.
pub fn status_path(id: &str) -> PathBuf {
    existing_status_path(id).unwrap_or_else(|| legacy_status_path(id))
}

/// Path for a NEW command run's status file (#991): `<session_id>__<id>.json`.
///
/// The writer's side of the resolver, and the reason a command file never
/// exists twice. It resolves an EXISTING file first — legacy `<id>.json` or a
/// session-qualified name this id already has — and only a genuinely new run
/// gets the session-qualified name. A record created before #991 therefore
/// keeps its name and is updated in place, which is what lets an in-flight run
/// survive the binary swap.
///
/// Agents do not come here: `new_agent` keeps the `<id>.json` naming, because
/// sub-agent files are addressed by session already (`parent_session_id` in the
/// record, plus the boot-built index in [`install_agent_index`]).
pub fn status_path_for(session_id: &str, id: &str) -> PathBuf {
    existing_status_path(id)
        .unwrap_or_else(|| status_dir().join(format!("{session_id}__{id}.json")))
}

/// The file already on disk for `id`, under either naming scheme.
fn existing_status_path(id: &str) -> Option<PathBuf> {
    let legacy = legacy_status_path(id);
    if legacy.exists() {
        return Some(legacy);
    }
    session_named_path(id)
}

/// Resolved session-qualified names for the CURRENT status dir (#991).
///
/// Keyed by directory exactly like [`AGENT_INDEX`], so a test that points
/// [`status_dir`] at a throwaway dir can never be answered out of another
/// test's snapshot. Only POSITIVE entries are cached and every hit is
/// stat-checked: a name that has since been swept falls through to a rebuild
/// instead of answering with a path that is gone, and a file created after the
/// last rebuild is found by the rebuild that a MISS triggers.
///
/// The index exists because the alternative is a `readdir` per lookup, and
/// lookups are not rare: `task_wait` re-reads the record on every `POLL_MS`
/// tick, so a live run would pay a walk of the whole status dir ten times a
/// second — the cost the sweeps in this module go to some length to avoid.
static SESSION_NAMES: RwLock<Option<(PathBuf, HashMap<String, PathBuf>)>> = RwLock::new(None);

/// The `<session>__<id>.json` file for `id`, if one exists.
///
/// The separator is `__` because a session id is a hyphenated UUID: `_` would
/// be legal inside neither half but `__` also cannot be produced by joining two
/// UUIDs accidentally, and — the part that matters to readers — it leaves the
/// stem's FIRST `.` as the extension boundary that `sweep_stale_runs` splits
/// captures on.
fn session_named_path(id: &str) -> Option<PathBuf> {
    let dir = status_dir();
    {
        let cache = SESSION_NAMES.read().unwrap_or_else(|e| e.into_inner());
        if let Some((cached_dir, names)) = cache.as_ref()
            && *cached_dir == dir
            && let Some(path) = names.get(id)
            && path.exists()
        {
            return Some(path.clone());
        }
    }

    // Rebuild from ONE readdir: every `<session>__<id>.json` name in the dir,
    // keyed by the run id. Deterministic on the (never-observed) collision —
    // the lexicographically smaller path wins — so two hosts with the same
    // bytes answer the same way.
    let mut names: HashMap<String, PathBuf> = HashMap::new();
    if let Ok(entries) = fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            let Some(stem) = name.strip_suffix(".json") else {
                continue;
            };
            let Some((_session, run_id)) = stem.split_once("__") else {
                continue;
            };
            if run_id.is_empty() {
                continue;
            }
            let run_id = run_id.to_string();
            let prev = names.get(&run_id).cloned();
            if prev.as_ref().is_none_or(|p| path < *p) {
                names.insert(run_id, path);
            }
        }
    }
    let found = names.get(id).cloned();
    let mut cache = SESSION_NAMES.write().unwrap_or_else(|e| e.into_inner());
    *cache = Some((dir, names));
    found
}

/// Record a name this process just wrote, so the next resolve is a cache hit.
fn remember_session_name(id: &str, path: &Path) {
    let dir = status_dir();
    let mut cache = SESSION_NAMES.write().unwrap_or_else(|e| e.into_inner());
    // An index for another directory is not an index this write can extend:
    // replace it rather than let one dir's names answer for another's.
    if cache
        .as_ref()
        .is_none_or(|(cached_dir, _)| *cached_dir != dir)
    {
        *cache = Some((dir, HashMap::new()));
    }
    if let Some((_, names)) = cache.as_mut() {
        names.insert(id.to_string(), path.to_path_buf());
    }
}

/// Every COMMAND run recorded for `session_id` (#991).
///
/// The question the naming exists to answer, asked as a glob instead of a scan:
/// new command records are `<session_id>__<id>.json`, so the session's runs are
/// the directory entries carrying that prefix. Returns the run ids, sorted.
///
/// Legacy `<id>.json` records are NOT returned — their names carry no session,
/// and attributing them would mean parsing every file in the directory, which
/// is the cost this replaces. A caller that must see them (the `/tasks` disk
/// arm) keeps its bounded newest-first scan.
pub fn list_session_runs(session_id: &str) -> std::io::Result<Vec<String>> {
    let dir = status_dir();
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let prefix = format!("{session_id}__");
    let mut ids = Vec::new();
    for entry in fs::read_dir(&dir)? {
        let entry = entry?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if let Some(rest) = name.strip_prefix(&prefix)
            && let Some(id) = rest.strip_suffix(".json")
            && !id.is_empty()
        {
            ids.push(id.to_string());
        }
    }
    ids.sort();
    Ok(ids)
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
/// One home for that pairing. It had two callers when the boot pass still swept
/// (#858); since #1081 moved retention off boot it has one, [`cleanup_stale`],
/// and a second copy of this block is how that one would quietly stop deleting
/// ledgers.
pub fn sweep_status_file(path: &Path) -> std::io::Result<()> {
    fs::remove_file(path)?;
    if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
        // A command record's stem is `<session>__<id>` since #991 and no ledger
        // is ever written for a command, so this lookup is a no-op there; for a
        // sub-agent the stem IS the id and the pairing is unchanged.
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
    /// Stopped by the parent through `stop_agent`. Terminal — nothing is
    /// executing this work any more — and unlike `Interrupted` the stop was
    /// deliberate and was recorded by the process that did it.
    Cancelled,
    /// The process that owned this work died before it reached a terminal
    /// state. Distinct from `Failed`: the work did not fail, it never
    /// finished, and nothing knows how far it got. Callers must not read it
    /// as either success or failure.
    Interrupted,
}

impl WorkState {
    /// Whether this state means the work is no longer doing anything.
    ///
    /// `Cancelled` MUST be here. The boot pass (`reconcile`) marks every
    /// non-terminal agent file `Interrupted` at startup, so a stop missing from
    /// this list is re-reported to its session as "interrupted by a restart" on
    /// the next start — a deliberate act rewritten as an accident.
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            WorkState::Completed
                | WorkState::Failed
                | WorkState::Cancelled
                | WorkState::Interrupted
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
            // Deliberate, and recorded by the process that did it — the one
            // terminal state a reader must not mistake for a fault.
            WorkState::Cancelled => "cancelled (stopped by the parent)",
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
    /// A signal ended the direct child. `code` is the shell encoding
    /// (`128 + signal`).
    ///
    /// #966: the signal number alone left the reader to guess the SENDER, and
    /// the rendering it produced — `signal 15 (killed)` — reads like a crash
    /// whether a lane's own housekeeping kill, a restart, or something outside
    /// the harness sent it. `origin` carries who, as far as the writer knows.
    ///
    /// #977: the signal's target is the shell WRAPPER, so even a confirmed
    /// signal death says nothing about the work the wrapper ran. `group`
    /// carries whether the work's process group was still alive when the
    /// harness probed it. The two fields answer different questions and
    /// neither implies the other: `origin` says WHO the harness can
    /// substantiate as the sender, `group` says whether the WORK had ended
    /// when the harness looked — a surviving group can coexist with a known
    /// sender, and an empty group with an unknown one.
    Signalled {
        /// The signal number, kept rather than folded into the exit code.
        signal: i32,
        /// Who sent it, as far as the harness can tell.
        ///
        /// Defaulted so every pre-change record and every site that does not
        /// know reads as [`SignalOrigin::Unknown`] — a site that cannot
        /// distinguish must not acquire a claim (#966).
        #[serde(default)]
        origin: SignalOrigin,
        /// Whether the WORK had ended when the harness probed its process
        /// group (#977).
        ///
        /// The direct child a run's wait observed is a shell wrapper; its
        /// death on a signal says nothing about the work it wrapped. This
        /// field carries the probe's answer: [`GroupState::Survived`] when
        /// members were still in the group at probe time, [`GroupState::Exited`]
        /// when the group was empty, [`GroupState::Unknown`] when nobody
        /// probed. Defaulted so every pre-#977 record reads as `Unknown` — a
        /// record that never looked must not acquire the claim that the work
        /// ended.
        #[serde(default)]
        group: GroupState,
    },
    /// A lifetime ceiling SIGKILLed the group (#752).
    ///
    /// **No producer in this build (#1056).** The hard ceiling was deleted: a
    /// run that outlives its deadline now only REPORTS itself, and stopping it
    /// stays a decision made by the agent or the operator. The variant is kept
    /// because records written by an older build still carry it, and a stored
    /// record is history, not dead code — deleting the variant would fail their
    /// deserialization.
    ///
    /// Its text still describes what it always described — a stop rather than
    /// a failure, because the run did not fail. `describe` keeps the stored
    /// wording (`ceiling 3600s`); the completion body names who stopped it,
    /// and with the harness's own sender gone that is the operator's command.
    Ceiling {
        /// The ceiling that was exceeded, in seconds.
        limit_secs: u64,
    },
    /// The child was never awaited, so nothing ran and no code is the command's.
    NotAwaited,
}

/// Who sent the signal that ended a run (#966).
///
/// The complaint that filed #966 was that a lane's own housekeeping
/// `kill -TERM -- -<pgid>` reads as a crash, because `signal 15 (killed)` was
/// the one rendering for every sender. The approved design (#966) is the four
/// variants below. What each can be POPULATED with is a narrower question, and
/// the answer is stated per variant rather than implied — a variant nobody can
/// substantiate is still useful as a name for a future writer, but only as long
/// as no site pretends to know it today.
///
/// * [`Self_`](SignalOrigin::Self_) — no write site today. A lane's own kill is
///   a shell command the lane runs; the harness sees it as an ordinary signal
///   from outside this process and keeps no record of having watched the lane
///   issue one, so it cannot be told from an operator's kill or an OOM kill.
/// * [`Restart`](SignalOrigin::Restart) — no write site today. A restart kills
///   the writer along with the group, so nothing in-process can stamp it, and
///   the boot-time reader (`restart_recovery::report_interrupted`) can say only
///   that the run was in flight, not who ended it.
/// * [`External`](SignalOrigin::External) — stamped at the one construction
///   site, `background_tasks::cmd_result_from`. Substantiable there, and
///   STRONGER since #1056: the harness used to have exactly one sender to a
///   detached run's process group, reached from the ceiling arm — and that
///   sender, along with the ceiling itself, no longer exists. The harness now
///   has NO sender to a detached run group at all, so every signal observed at
///   that site necessarily came from outside this process — exactly what the
///   variant claims, and no more.
/// * [`Unknown`](SignalOrigin::Unknown) — the `Default`, so an absent origin
///   (every record written before #966) and any site that cannot tell read as
///   it. It renders #786's string byte-for-byte: a record that does not say who
///   sent the signal must not acquire the claim on the way out.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SignalOrigin {
    /// Nobody recorded who sent it — every pre-#966 record, and any site that
    /// cannot tell.
    #[default]
    Unknown,
    /// The lane killed its own run as housekeeping. No write site today (see
    /// the type doc).
    #[serde(rename = "self")]
    Self_,
    /// A daemon restart took the process group down. No write site today (see
    /// the type doc).
    Restart,
    /// A sender outside this process — the only origin the harness can
    /// substantiate, and the one it stamps.
    External,
}

/// Whether the WORK inside a signalled run had ended when the harness probed
/// its process group (#977).
///
/// A detached run's direct child is a shell wrapper (`sh -c …`). The wait that
/// observed a signal death observed THE WRAPPER. The measurement that filed
/// #977: a notice read `signal 15 (killed)` and, in the same turn, `ps -g`
/// still listed the `grep` the wrapper had spawned — a terminal kill stamped
/// for work the harness had never looked at. This enum is the probe's answer,
/// stamped next to the signal:
///
/// * [`Unknown`](GroupState::Unknown) — nobody probed: every pre-#977 record,
///   and every platform that cannot tell.
/// * [`Exited`](GroupState::Exited) — probed, and the group was empty: the
///   work ended with the wrapper, so the kill reading was true.
/// * [`Survived`](GroupState::Survived) — probed, and members remained. The
///   `procs` count is `None` when the platform can tell the group is alive
///   but not how many members it has (the `kill(-pgid, 0)` platforms).
///
/// `origin` and `group` are different questions and neither implies the
/// other: `origin` says WHO sent the signal (a claim about the sender),
/// `group` says whether the work had ENDED when the harness looked (a claim
/// about the work). A surviving group can coexist with a fully attributed
/// sender, and an empty group with an unknown one.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GroupState {
    /// Nobody probed — every pre-#977 record, and any site or platform that
    /// cannot tell. Renders exactly as before this field existed, so no
    /// record acquires a claim on the way out.
    #[default]
    Unknown,
    /// The probe found no living member in the recorded group.
    Exited,
    /// The probe found the group still holding at least one member. `procs`
    /// is the count when the platform reports one, `None` when it can only
    /// say the group is reachable (`kill(-pgid, 0)` succeeded or said EPERM).
    Survived {
        /// The process group the probe looked at — the run's recorded pid,
        /// which doubles as the pgid because the child was spawned under
        /// `setsid`.
        pgid: u32,
        /// Living members found, or `None` when the platform cannot count.
        procs: Option<u32>,
    },
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
            // `Unknown` origin returns #786's exact wording. Every other
            // origin arm names the sender, which is the whole point of the
            // field (#966).
            //
            // #977: the group probe is matched FIRST, because it answers a
            // different question than the origin. A surviving group means the
            // wait only observed the wrapper, so no `killed` rendering may be
            // produced for it — not even the sender-attributed ones. `Unknown`
            // (nobody probed: every pre-#977 record) and `Exited` (probed,
            // group empty) keep the per-origin rendering byte for byte.
            Self::Signalled {
                signal,
                origin,
                group,
            } => match group {
                GroupState::Survived { pgid, procs } => match procs {
                    Some(count) => format!(
                        "signal {signal} — watcher stopped, work may still be running: \
                         {count} procs in group {pgid}"
                    ),
                    None => format!(
                        "signal {signal} — watcher stopped, work may still be running \
                         in group {pgid}"
                    ),
                },
                GroupState::Unknown | GroupState::Exited => match origin {
                    SignalOrigin::Unknown => format!("signal {signal} (killed)"),
                    SignalOrigin::Self_ => signalled_by(*signal, "self"),
                    SignalOrigin::Restart => signalled_by(*signal, "restart"),
                    SignalOrigin::External => signalled_by(*signal, "external process"),
                },
            },
            Self::Ceiling { limit_secs } => format!("ceiling {limit_secs}s"),
            Self::NotAwaited => "not awaited (nothing ran)".to_string(),
        }
    }
}

/// `signal 15 (SIGTERM) — signalled by restart`, or without the name when the
/// number is not one [`signal_name`] knows.
fn signalled_by(signal: i32, who: &str) -> String {
    match signal_name(signal) {
        Some(name) => format!("signal {signal} ({name}) — signalled by {who}"),
        None => format!("signal {signal} — signalled by {who}"),
    }
}

/// The conventional short name of a signal, for the arms that name a sender.
///
/// A table rather than `strsignal(3)`: the text is then identical on every
/// platform, cannot pick up a locale, and cannot differ between the binary that
/// wrote a status file and the one that reads it.
fn signal_name(signal: i32) -> Option<&'static str> {
    Some(match signal {
        1 => "SIGHUP",
        2 => "SIGINT",
        3 => "SIGQUIT",
        6 => "SIGABRT",
        9 => "SIGKILL",
        11 => "SIGSEGV",
        13 => "SIGPIPE",
        15 => "SIGTERM",
        _ => return None,
    })
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
    /// Branch of the private worktree cut for this agent (#960), if one was
    /// cut. `None` for commands, for agents that ran in the parent's directory,
    /// and for records written before the field existed.
    ///
    /// A parent pays for a child's work and cannot otherwise review it while it
    /// is in progress: the branch and path were derived at spawn and then
    /// discarded, so the only way to find the tree was to guess the naming
    /// scheme.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree_branch: Option<String>,
    /// Absolute path of the worktree named by [`Self::worktree_branch`] (#960).
    /// Same contract.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree_path: Option<String>,
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
            worktree_branch: None,
            worktree_path: None,
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
            worktree_branch: None,
            worktree_path: None,
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
            worktree_branch: None,
            worktree_path: None,
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

    /// Record the private worktree cut for this agent (#960).
    ///
    /// Written at spawn, because the tree exists from the child's first turn
    /// and a parent may want to read it while the child is still working. The
    /// branch and path come from [`super::super::tools::subagent::worktree::paths`]
    /// at the spawn site, so this method stores them without re-deriving them.
    pub fn record_worktree(&mut self, branch: &str, path: &std::path::Path) -> std::io::Result<()> {
        self.worktree_branch = Some(branch.to_string());
        self.worktree_path = Some(path.display().to_string());
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

    /// Mark the work stopped by its parent through `stop_agent`.
    ///
    /// Deliberately writes no `output_full`: a stopped round's fragment is not
    /// a result, and storing it would let `task_output` hand a caller the very
    /// piece of work it just cancelled. No `error` either — nothing went
    /// wrong, so asserting a fault would be a second lie. The finish stamp is
    /// what puts the file on the terminal retention schedule instead of the
    /// unfinished one.
    pub fn mark_cancelled(&mut self) -> std::io::Result<()> {
        self.state = WorkState::Cancelled;
        self.finish = Some(finish_now());
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
    ///
    /// The NAME is chosen once per run (#991). A command's first write puts the
    /// session in the file name; every later write — including one that follows
    /// a legacy `<id>.json` an older binary wrote — lands on the path that
    /// already exists, so a run never ends up with two records. [`status_path_for`]
    /// is the only place that decides, and it decides by looking at the disk.
    fn write(&self) -> std::io::Result<()> {
        let path = match self.kind {
            WorkKind::Command => status_path_for(&self.session_id, &self.id),
            WorkKind::Agent => status_path(&self.id),
        };
        ensure_dir()?;
        let tmp = path.with_extension("json.tmp");
        let data = serde_json::to_string_pretty(self).map_err(std::io::Error::other)?;
        let mut f = fs::File::create(&tmp)?;
        f.write_all(data.as_bytes())?;
        f.sync_all()?;
        fs::rename(&tmp, &path)?;
        // Only a session-qualified name belongs in the index; a legacy path is
        // found by the resolver's direct hit before the index is ever consulted.
        if matches!(self.kind, WorkKind::Command) && path != legacy_status_path(&self.id) {
            remember_session_name(&self.id, &path);
        }
        Ok(())
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
                && !matches!(
                    status.state,
                    WorkState::Completed | WorkState::Failed | WorkState::Cancelled
                ))
            .then_some(status);
        }
        let ids = Self::list_all().ok()?;
        ids.into_iter().filter_map(|id| Self::read(&id)).find(|s| {
            matches!(s.kind, WorkKind::Agent)
                && s.session_id == session_id
                && !matches!(
                    s.state,
                    WorkState::Completed | WorkState::Failed | WorkState::Cancelled
                )
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
            worktree_branch: None,
            worktree_path: None,
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

/// The two staleness windows, resolved from the live config (#849, #1081).
///
/// `Config::current()` is an `Arc` clone of the in-memory mirror — zero disk IO
/// — so this is safe on the blocking pool the sweep runs on, and it picks up a
/// config the watcher refreshed since the process started.
///
/// A resolved `0` means "keep forever". [`StaleWindows::from_ages`] expresses
/// that as a cutoff at `UNIX_EPOCH`, which nothing is older than, so a zero is
/// an operator saying "do not delete" rather than "delete everything".
///
/// # Why it lives here and not at the call site (#1081)
///
/// Retention used to be resolved inside the boot reconcile pass, which was the
/// only caller. Now that the periodic maintenance sweep owns the sweep, a second
/// copy of this resolution would be a second place for the two windows to drift
/// — and the pair is load-bearing: a command record read once and an agent
/// record read on revival age out on deliberately different schedules.
pub fn stale_windows_from_config() -> StaleWindows {
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

/// Remove status files that have aged out: a finished file on the window its
/// KIND gets, and a file with no finish (or no parse) on its mtime.
///
/// Scope is the STATUS directory (`tmp/detached`) only. Captured streams in
/// `tmp/runs` belong to [`sweep_stale_runs`], whose single owner is
/// `logging::cleanup_old_temp_files` (#848) — see the note at the end of this
/// function for why the duplicate came out.
///
/// # Who calls this (#1081)
///
/// The periodic maintenance sweep
/// ([`crate::services::maintenance::MaintenanceService`]), not the boot
/// reconcile pass. Retention does not depend on the process having just
/// started, and boot used to be its only trigger — so a daemon that stayed up
/// grew `tmp/detached` without bound and every restart paid the whole backlog
/// (294 s over 16,913 files on the host that filed #1081) while competing with
/// the boot recovery that had just revived children in the same directory.
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

        // One parse, one decision. This was the same predicate the boot pass
        // applied while it still swept (#858); since #1081 the boot pass only
        // reconciles, so this is now the sole caller — and still the only place
        // the decision is made.
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
            Err(e) => tracing::warn!("Failed to remove stale run capture {}: {e}", path.display()),
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
    /// the one that matters most: the run did not fail, it was STOPPED.
    ///
    /// #1056: nothing in this build produces `Ceiling` any more — a run past
    /// its deadline is now reported, never stopped — so this is a rendering pin
    /// for a record an older build wrote, and it has to keep rendering as a
    /// stop rather than as a failure.
    #[test]
    fn ceiling_oom_and_literal_137_are_three_distinct_renderings() {
        let ceiling = RunCause::Ceiling { limit_secs: 3600 }.describe(137);
        let oom = RunCause::Signalled {
            signal: 9,
            origin: SignalOrigin::External,
            group: GroupState::Unknown,
        }
        .describe(137);
        let literal = RunCause::Ordinary.describe(137);

        assert_eq!(ceiling, "ceiling 3600s");
        assert_eq!(oom, "signal 9 (SIGKILL) — signalled by external process");
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
            RunCause::Signalled {
                signal: 15,
                origin: SignalOrigin::Unknown,
                group: GroupState::Unknown,
            },
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

    /// #966 — the plan's Done-when, literally. A signalled death used to render
    /// `signal 15 (killed)` for every sender, which reads like a crash and made
    /// a lane's own housekeeping `kill -TERM` look like a fault. Two origins,
    /// one signal, two strings — and the unknown arm keeps #786's wording
    /// byte-for-byte, because a record that does not say who sent the signal
    /// must not acquire the claim on the way out.
    #[test]
    fn a_signal_names_who_signalled_it() {
        let by_self = RunCause::Signalled {
            signal: 15,
            origin: SignalOrigin::Self_,
            group: GroupState::Unknown,
        }
        .describe(143);
        let by_restart = RunCause::Signalled {
            signal: 15,
            origin: SignalOrigin::Restart,
            group: GroupState::Unknown,
        }
        .describe(143);
        let by_external = RunCause::Signalled {
            signal: 15,
            origin: SignalOrigin::External,
            group: GroupState::Unknown,
        }
        .describe(143);
        let unknown = RunCause::Signalled {
            signal: 15,
            origin: SignalOrigin::Unknown,
            group: GroupState::Unknown,
        }
        .describe(143);

        assert_eq!(by_self, "signal 15 (SIGTERM) — signalled by self");
        assert_eq!(by_restart, "signal 15 (SIGTERM) — signalled by restart");
        assert_eq!(
            by_external, "signal 15 (SIGTERM) — signalled by external process",
            "the one origin the harness substantiates must say what it claims"
        );
        // The pre-#966 string, byte-for-byte.
        assert_eq!(unknown, "signal 15 (killed)");

        // The assertion the plan asks for, and the one the negative control
        // breaks: same signal, different origin, different text.
        assert_ne!(
            by_restart, by_self,
            "who sent the signal is the whole point of the field"
        );
        // No two origins may collapse into one rendering, or the field is
        // decoration for the arms that never get populated either.
        let all = [unknown, by_self, by_restart, by_external];
        for (i, a) in all.iter().enumerate() {
            for b in &all[i + 1..] {
                assert_ne!(a, b, "two origins render alike: {a}");
            }
        }
    }

    /// A signal number outside the naming table still names its sender; only
    /// the parenthetical name is dropped.
    #[test]
    fn an_unnamed_signal_still_names_its_sender() {
        let rendered = RunCause::Signalled {
            signal: 40,
            origin: SignalOrigin::External,
            group: GroupState::Unknown,
        }
        .describe(168);
        assert_eq!(rendered, "signal 40 — signalled by external process");
        assert!(
            !rendered.contains('('),
            "no invented signal name: {rendered}"
        );
    }

    /// A record written before #966 carries no `origin`, and #786's own
    /// shipped records look exactly like this. It must read back as `Unknown`
    /// — never as a harness kill, which would be a claim the record cannot
    /// support — and render exactly as it did before the field existed.
    #[test]
    fn a_pre_change_signal_record_reads_as_unknown_origin() {
        let legacy = r#"{"kind":"signalled","signal":9}"#;
        let cause: RunCause = serde_json::from_str(legacy).expect("legacy signalled record parses");
        assert_eq!(
            cause,
            RunCause::Signalled {
                signal: 9,
                origin: SignalOrigin::Unknown,
                group: GroupState::Unknown,
            },
            "an absent origin is Unknown, not a harness kill"
        );
        assert_eq!(
            cause.describe(137),
            "signal 9 (killed)",
            "a legacy record must render byte-for-byte as it always has"
        );
        assert_eq!(SignalOrigin::default(), SignalOrigin::Unknown);
    }

    /// #977 — the plan's Done-when, literally. The issue's measured case: the
    /// wait observed only the shell WRAPPER dying (`signal 15`, while `ps -g`
    /// still listed the grep the wrapper had spawned), so a record whose probe
    /// found living members must not render as a kill of the work. Same
    /// signal, three probe answers, three strings — and the surviving one is
    /// the only one allowed to say the work may still be running.
    #[test]
    fn a_surviving_group_is_not_rendered_as_a_kill() {
        let survived = RunCause::Signalled {
            signal: 15,
            origin: SignalOrigin::External,
            group: GroupState::Survived {
                pgid: 59099,
                procs: Some(3),
            },
        }
        .describe(143);
        let exited = RunCause::Signalled {
            signal: 15,
            origin: SignalOrigin::External,
            group: GroupState::Exited,
        }
        .describe(143);
        let unknown = RunCause::Signalled {
            signal: 15,
            origin: SignalOrigin::External,
            group: GroupState::Unknown,
        }
        .describe(143);

        assert_eq!(
            survived,
            "signal 15 — watcher stopped, work may still be running: 3 procs in group 59099",
            "a surviving group reports the wrapper's stop, not a kill of the work"
        );
        assert!(
            !survived.contains("killed"),
            "the word `killed` must not appear for a surviving group: {survived}"
        );
        assert!(survived.contains("watcher stopped, work may still be running"));
        // The negative controls — the assertions a probe-less build fails:
        // same signal, different probe answer, different text.
        assert_ne!(survived, exited, "a probed-empty group is a different fact");
        assert_ne!(
            survived, unknown,
            "an unprobed record must not read as a probe's answer"
        );

        // The count-less form, for the platforms that can say alive but not
        // how many: no count clause, and still no `killed`.
        let count_unknown = RunCause::Signalled {
            signal: 15,
            origin: SignalOrigin::External,
            group: GroupState::Survived {
                pgid: 59099,
                procs: None,
            },
        }
        .describe(143);
        assert_eq!(
            count_unknown,
            "signal 15 — watcher stopped, work may still be running in group 59099"
        );
        assert!(!count_unknown.contains("killed"));
    }

    /// A record written before #977 carries no `group`, and #966's shipped
    /// records look exactly like this. Nobody probed for it, so it must read
    /// as `Unknown` — never as `Exited`, which would claim a probe nobody
    /// took — and render exactly as it did before the field existed.
    #[test]
    fn a_pre_change_signalled_record_probes_nothing() {
        // The wire shape mirrors `a_pre_change_signal_record_reads_as_unknown_origin`:
        // `kind` tags the variant, `signal` is the only carried field.
        let legacy = r#"{"kind":"signalled","signal":15}"#;
        let cause: RunCause = serde_json::from_str(legacy).expect("legacy signalled record parses");
        assert_eq!(
            cause,
            RunCause::Signalled {
                signal: 15,
                origin: SignalOrigin::Unknown,
                group: GroupState::Unknown,
            },
            "an absent group is Unknown, not a probe result nobody took"
        );
        assert_eq!(
            cause.describe(143),
            "signal 15 (killed)",
            "a legacy record must render byte-for-byte as it always has"
        );
        assert_eq!(GroupState::default(), GroupState::Unknown);
    }

    /// The wire names are part of the status-file format: a reader on a later
    /// binary has to recognise what an earlier one wrote.
    #[test]
    fn signal_origin_wire_names_are_stable() {
        for (origin, wire) in [
            (SignalOrigin::Unknown, "\"unknown\""),
            (SignalOrigin::Self_, "\"self\""),
            (SignalOrigin::Restart, "\"restart\""),
            (SignalOrigin::External, "\"external\""),
        ] {
            assert_eq!(
                serde_json::to_string(&origin).expect("serialize origin"),
                wire,
                "{origin:?} changed its wire name"
            );
        }
    }

    /// The enum round-trips through the status file, so a reader on a later
    /// binary sees the cause the finishing binary stamped — for every origin,
    /// not only the two the harness populates today.
    #[test]
    fn every_cause_round_trips_through_serde() {
        for cause in [
            RunCause::Ordinary,
            RunCause::Signalled {
                signal: 9,
                origin: SignalOrigin::Unknown,
                group: GroupState::Unknown,
            },
            // #977: the probe's answer rides the record too, so a surviving
            // group must survive the file as well — a reader on a later binary
            // has to see what the finishing binary measured.
            RunCause::Signalled {
                signal: 15,
                origin: SignalOrigin::External,
                group: GroupState::Survived {
                    pgid: 59099,
                    procs: Some(3),
                },
            },
            RunCause::Signalled {
                signal: 15,
                origin: SignalOrigin::Self_,
                group: GroupState::Unknown,
            },
            RunCause::Signalled {
                signal: 15,
                origin: SignalOrigin::Restart,
                group: GroupState::Unknown,
            },
            RunCause::Signalled {
                signal: 15,
                origin: SignalOrigin::External,
                group: GroupState::Unknown,
            },
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
