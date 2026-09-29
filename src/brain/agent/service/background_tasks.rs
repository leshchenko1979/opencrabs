//! Background task manager (#722).
//!
//! Runs a genuinely long command detached (so it doesn't churn the bash 600s
//! cap) and, on completion, enqueues a synthetic `QueuedUserMessage` into the
//! originating session via the surface enqueue callback. The tool loop drains
//! that at the next iteration boundary — injected mid-turn if the agent is still
//! working, or starting a fresh turn if it went idle.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

use uuid::Uuid;

use super::types::{BgTaskMeta, PushOrigin, QueuedUserMessage};

/// Result of a finished background command.
#[derive(Debug, Clone)]
pub struct CmdResult {
    pub success: bool,
    pub code: i32,
    pub output: String,
}

/// One in-flight background command.
#[derive(Debug, Clone)]
pub struct RunningTask {
    /// Short label for the command, e.g. `cargo test`.
    pub label: String,
    /// When it was spawned, for the elapsed time a surface displays.
    pub started: std::time::Instant,
    /// Stable run id (#692) — the handle `tasks_list`, `task_output`,
    /// `task_wait` and `task_cancel` all address the run by.
    pub id: String,
}

/// A live run's addressable state (#692) — the missing primitive.
///
/// `spawn_command` used to generate a `task_id`, write a status file under it,
/// and then drop that id into the closure. The process existed and had an
/// identity on disk, but the manager could neither name it nor signal it, so a
/// run could not be listed with its output, polled while it was still
/// producing, or cancelled. Every gap in the parity ask traces back to this
/// struct's absence — one omission, three symptoms.
#[derive(Debug, Clone)]
pub struct RunHandle {
    /// Stable id; also the status-file stem and the output-file stem, so a
    /// reader addresses the whole run by one string.
    pub id: String,
    pub session_id: Uuid,
    pub label: String,
    pub command: String,
    pub cwd: PathBuf,
    pub started: std::time::Instant,
    /// The spawned shell's pid. `detach_session_pre_exec` calls `setsid()`, so
    /// this pid IS a process-group id: the group form of a signal reaches the
    /// whole tree, including children that re-parent away mid-sweep — the case
    /// `kill_process_tree` documents missing and that was measured on #692,
    /// where the sweep ran, found nothing, and the command outlived the turn.
    pub pid: Option<u32>,
    /// Live capture of the run's stdout.
    pub output_out: PathBuf,
    /// Live capture of the run's stderr. Same lifetime and addressing as
    /// [`Self::output_out`].
    pub output_err: PathBuf,
}

/// Manages background commands and resumes their sessions on completion.
pub struct BackgroundTaskManager {
    /// Live runs, keyed by run id (#692).
    ///
    /// One map rather than a vec per session: the run id is the handle every
    /// surface and tool addresses, and the session is an attribute of a run
    /// rather than its key. The previous shape could not tell two runs of the
    /// same command apart either — `mark_finished` removed the oldest entry
    /// whose *label* matched, so a second `cargo test` cleared the first one's
    /// running badge and left a phantom on the roster.
    ///
    /// Holds the whole handle, not just a count, because a surface has to be
    /// able to say WHAT is running and for how long: a detached task takes the
    /// turn idle, so without this the TUI has nothing at all to draw while a
    /// long build runs and the wait looks like a hang (#762).
    runs: Mutex<HashMap<String, RunHandle>>,
}

use super::work_status::{CommandExit, CommandPaths, WorkStatus};

impl BackgroundTaskManager {
    pub fn new() -> Self {
        Self {
            runs: Mutex::new(HashMap::new()),
        }
    }

    /// How many background tasks are currently running for `session_id`.
    pub fn running_for(&self, session_id: Uuid) -> usize {
        self.runs
            .lock()
            .map(|m| m.values().filter(|r| r.session_id == session_id).count())
            .unwrap_or(0)
    }

    /// What is running for `session_id`, oldest first, for surfaces that show
    /// progress. Returns owned data so the caller never holds the lock.
    ///
    /// Sorted on the clock rather than on insertion: the map is unordered, and
    /// a surface that drew the roster in hash order would shuffle the rows of
    /// a running build between renders.
    pub fn running_tasks(&self, session_id: Uuid) -> Vec<RunningTask> {
        let mut tasks: Vec<RunningTask> = self
            .runs
            .lock()
            .map(|m| {
                m.values()
                    .filter(|r| r.session_id == session_id)
                    .map(|r| RunningTask {
                        label: r.label.clone(),
                        started: r.started,
                        id: r.id.clone(),
                    })
                    .collect()
            })
            .unwrap_or_default();
        tasks.sort_by_key(|t| t.started);
        tasks
    }

    /// Register a run and return its id (#692).
    ///
    /// This is the point at which a spawned process becomes addressable. The
    /// id is returned rather than kept private so the caller that owns the
    /// turn can hand it back to the agent in the same result.
    pub fn started_run(&self, handle: RunHandle) -> String {
        let id = handle.id.clone();
        if let Ok(mut m) = self.runs.lock() {
            m.insert(id.clone(), handle);
        }
        id
    }

    /// Drop a finished run from the roster. Keyed by id, never by label.
    pub fn finish_run(&self, id: &str) {
        if let Ok(mut m) = self.runs.lock() {
            m.remove(id);
        }
    }

    /// A live run's handle, cloned out so the caller never holds the lock.
    pub fn handle(&self, id: &str) -> Option<RunHandle> {
        self.runs.lock().ok()?.get(id).cloned()
    }

    /// Every live run for `session_id`, oldest first (#692).
    ///
    /// Scoped to the caller's own session for the same reason `tasks_list` is
    /// (#191): the manager is process-global, so an unfiltered read reports
    /// other sessions' work as this caller's.
    pub fn handles_for(&self, session_id: Uuid) -> Vec<RunHandle> {
        let mut out: Vec<RunHandle> = self
            .runs
            .lock()
            .map(|m| {
                m.values()
                    .filter(|r| r.session_id == session_id)
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        out.sort_by_key(|r| r.started);
        out
    }

    /// Signal a run's whole process group (#692).
    ///
    /// Group-first, because `setsid` made the child a leader: `killpg` reaches
    /// every descendant, including one that re-parented to init before the
    /// signal — precisely the case a tree-walk by pid cannot see.
    ///
    /// `Ok(())` means the signal was *delivered*, not that the process is
    /// already gone; the caller re-reads the roster (or the status file) to
    /// confirm. A refusal is returned rather than swallowed: a cancel the agent
    /// believes landed but did not is worse than no cancel at all.
    pub fn cancel(&self, id: &str) -> std::result::Result<(), String> {
        let Some(handle) = self.handle(id) else {
            return Err(format!("no live run with id {id}"));
        };
        let Some(pid) = handle.pid else {
            return Err(format!("run {id} has no recorded pid yet"));
        };
        #[cfg(unix)]
        {
            // SAFETY: `pid` came from `Child::id()` for a child this process
            // spawned, so it is a live pid we are entitled to signal. Even if
            // it has since exited, `killpg` on a dead pid is `ESRCH` — reported
            // below rather than assumed away.
            let rc = unsafe { libc::killpg(pid as libc::pid_t, libc::SIGTERM) };
            if rc == 0 {
                Ok(())
            } else {
                Err(format!(
                    "killpg({pid}) failed: {}",
                    std::io::Error::last_os_error()
                ))
            }
        }
        #[cfg(not(unix))]
        {
            crate::utils::shell::kill_process_tree(pid);
            Ok(())
        }
    }

    /// Spawn `command` (via `sh -c`) in `cwd`, detached; on completion enqueue a
    /// system message into `session_id` summarizing the result. Returns
    /// immediately — the caller's turn is free to end.
    ///
    /// This is the "detach now" form: the caller has already decided the run
    /// outlives its turn (explicit `background: true`, or the long-command
    /// classifier). The grace handover is [`Self::run_or_detach`].
    pub fn spawn_command(
        self: std::sync::Arc<Self>,
        session_id: Uuid,
        cwd: PathBuf,
        label: String,
        command: String,
    ) {
        let cmd = build_command(&cwd, &command, session_id);
        let this = std::sync::Arc::clone(&self);
        tokio::spawn(async move {
            match this
                .run_or_detach(
                    session_id,
                    cwd,
                    label.clone(),
                    command.clone(),
                    cmd,
                    Some(std::time::Duration::ZERO),
                )
                .await
            {
                // A command that somehow finished within the zero-length window
                // would otherwise have its completion dropped on the floor: the
                // caller here is a detached task with nobody to hand a result
                // to. Deliver it down the same path the handover uses, so the
                // contract ("returns immediately, reports on completion") holds
                // for every duration, including zero.
                Ok(Handover::Inline(output)) => {
                    deliver_completion(session_id, &label, &command, &cmd_result_from(&output), 0.0);
                }
                Ok(Handover::Detached { .. }) => {}
                Err(e) => tracing::error!(
                    target: "background_task",
                    "Could not start detached run: {e:#}"
                ),
            }
        });
    }

    /// Spawn `cmd`, hold the caller's turn for at most `grace`, and hand the run
    /// to the background manager if it outlives that window (#692).
    ///
    /// The whole correctness of this handover rests on ONE property: the
    /// `Child` is owned in a scope that outlives the caller's turn. Every spawn
    /// site in the bash tool sets `kill_on_drop(true)` (#1046), so a handoff
    /// that merely *stopped waiting* would kill the command at the grace
    /// boundary and then write a status file claiming it had been detached —
    /// re-shipping the very defect (#692) this change exists to remove. The
    /// child is therefore never dropped while it is still wanted: the inline
    /// arm consumes it by waiting, and the detached arm MOVES it into the
    /// continuation task.
    ///
    /// `grace = None` never reaches here — it is the caller's signal that it
    /// keeps today's kill-at-deadline behaviour.
    pub async fn run_or_detach(
        self: std::sync::Arc<Self>,
        session_id: Uuid,
        cwd: PathBuf,
        label: String,
        command: String,
        mut cmd: tokio::process::Command,
        grace: Option<std::time::Duration>,
    ) -> std::io::Result<Handover> {
        let started = std::time::Instant::now();
        let run_uuid = Uuid::new_v4();
        let id = run_uuid.to_string();
        let output_out = crate::brain::agent::service::work_status::command_output_path(&id, false);
        let output_err = crate::brain::agent::service::work_status::command_output_path(&id, true);
        crate::brain::agent::service::work_status::ensure_runs_dir()?;

        // Streams are created AT SPAWN, never by the first reader: an absent
        // file and an empty one are indistinguishable to a reader, and only one
        // of them means "this run never existed" (#692).
        let out_file = tokio::fs::File::create(&output_out).await?;
        let err_file = tokio::fs::File::create(&output_err).await?;

        let mut child = cmd.spawn()?;
        let pid = child.id();
        let cap = run_output_cap_bytes();
        let out_reader = spawn_stream_reader(child.stdout.take(), out_file, cap);
        let err_reader = spawn_stream_reader(child.stderr.take(), err_file, cap);

        // Registered BEFORE the wait, so the run is addressable from its first
        // instant: a `task_cancel` issued moments after the spawn must find it.
        self.started_run(RunHandle {
            id: id.clone(),
            session_id,
            label: label.clone(),
            command: command.clone(),
            cwd: cwd.clone(),
            started,
            pid,
            output_out: output_out.clone(),
            output_err: output_err.clone(),
        });
        if let Err(e) = WorkStatus::new_command(
            &id,
            &session_id.to_string(),
            &label,
            &command,
            CommandPaths {
                output_out: output_out.display().to_string(),
                output_err: output_err.display().to_string(),
                pid,
            },
        ) {
            tracing::warn!(
                target: "background_task",
                "Could not write run status for {id}: {e}"
            );
        }
        tracing::info!(
            target: "background_task",
            "Run '{label}' started for session {session_id} (id={id}, pid={pid:?}, cwd={})",
            cwd.display()
        );

        // Persist BEFORE waiting: a restart mid-command must find a row to
        // report as interrupted, otherwise the session waits forever on a
        // resume that can no longer come (#763).
        if let Some(repo) = task_repo()
            && let Err(e) = repo
                .record(run_uuid, session_id, &label, &command, &cwd.to_string_lossy())
                .await
        {
            // Not fatal: the command still runs and still resumes the session
            // in this process. Only restart accounting is lost.
            tracing::error!(
                target: "background_task",
                "Failed to persist run '{label}': {e:#}"
            );
        }

        // A `None` grace means the caller never wanted a handover. Reached only
        // if a future caller passes it, and answered with the detached form
        // rather than by silently killing the child we already spawned.
        let Some(grace) = grace else {
            let this = std::sync::Arc::clone(&self);
            // `id.clone()`: the handle is returned to the caller AND handed to
            // the continuation, so both need it. The paths are re-derived there
            // from the id rather than passed, which keeps one construction site
            // for a run's layout.
            let task_id = id.clone();
            tokio::spawn(async move {
                continue_detached(
                    this,
                    run_uuid,
                    task_id,
                    session_id,
                    label,
                    command,
                    started,
                    child,
                    out_reader,
                    err_reader,
                )
                .await;
            });
            return Ok(Handover::Detached {
                id,
                output_out,
                output_err,
            });
        };

        match tokio::time::timeout(grace, child.wait()).await {
            Ok(Ok(status)) => {
                // Finished inside the window: the turn keeps its result and
                // nothing is handed over.
                let stdout = collect_capture(out_reader, &output_out).await;
                let stderr = collect_capture(err_reader, &output_err).await;
                let output = std::process::Output {
                    status,
                    stdout,
                    stderr,
                };
                let result = cmd_result_from(&output);
                let elapsed_secs = started.elapsed().as_secs_f32();
                write_finish(&id, &session_id, &label, &command, elapsed_secs, &result);
                self.finish_run(&id);
                clear_row(run_uuid, &label).await;
                Ok(Handover::Inline(output))
            }
            Ok(Err(e)) => {
                // The child was reaped but could not be awaited. Drop the
                // roster entry so a dead run is not listed as live.
                self.finish_run(&id);
                Err(e)
            }
            Err(_elapsed) => {
                // Outlived the window. The child MOVES into the continuation —
                // never dropped here, or `kill_on_drop` fires (see the doc
                // comment above).
                let this = std::sync::Arc::clone(&self);
                let task_id = id.clone();
                tokio::spawn(async move {
                    continue_detached(
                        this,
                        run_uuid,
                        task_id,
                        session_id,
                        label,
                        command,
                        started,
                        child,
                        out_reader,
                        err_reader,
                    )
                    .await;
                });
                Ok(Handover::Detached {
                    id,
                    output_out,
                    output_err,
                })
            }
        }
    }
}

/// What happened to a run the manager was asked to supervise (#692).
#[derive(Debug)]
pub enum Handover {
    /// Finished inside the grace window — the caller keeps the result and
    /// nothing was handed over.
    ///
    /// Carries a `std::process::Output` rather than a [`CmdResult`] so the bash
    /// tool can hand it to the same downstream it uses for every other path
    /// (byte-exact stdout/stderr, exit status) instead of re-deriving a
    /// string form and losing the separation. The manager's own consumers use
    /// [`CmdResult`] and convert where they need it.
    Inline(std::process::Output),
    /// Outlived the window; the run continues in the background.
    Detached {
        /// The run's handle for `tasks_list` / `task_output` / `task_wait` /
        /// `task_cancel`.
        id: String,
        /// Live stdout capture.
        output_out: PathBuf,
        /// Live stderr capture.
        output_err: PathBuf,
    },
}

/// Per-stream capture cap (#692), read from config.
///
/// `0` means "no cap", which is a legitimate operator choice on a box with
/// room — so it is honoured rather than treated as a mistake.
fn run_output_cap_bytes() -> u64 {
    crate::config::Config::current().agent.run_output_cap_bytes
}

/// Build the detached-run command: platform shell, session env, no controlling
/// TTY, piped stdio.
///
/// `detach_session_pre_exec`'s `setsid()` is why a run's recorded pid doubles
/// as a process-group id, which is in turn why [`BackgroundTaskManager::cancel`]
/// can reach a descendant that re-parented to init.
fn build_command(cwd: &std::path::Path, command: &str, session_id: Uuid) -> tokio::process::Command {
    use crate::utils::shell::PushShellCommand;
    use tokio::process::Command;
    let (shell, shell_arg) = crate::utils::shell::shell_pair();
    let mut cmd = Command::new(shell);
    cmd.push_shell_command(shell_arg, command)
        .current_dir(cwd)
        .env("OPENCRABS_SESSION_ID", session_id.to_string())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    // The SAME terminal detachment the inline path applies, and load-bearing
    // for cancellation rather than cosmetic: `setsid()` makes the child a
    // session and process-group leader, so the pid recorded on the run is a
    // PGID and one `killpg` reaches the whole tree — including a grandchild
    // that re-parented to init. Without it the recorded pid signals only the
    // shell, which is exactly the miss `kill_process_tree` documents.
    crate::brain::tools::bash::detach_session_pre_exec(&mut cmd);
    cmd
}

/// Copy a child's stream to `file` until EOF, honouring `cap`.
///
/// Two rules make this a live capture rather than a buffered one:
///
/// 1. The reader drains CONTINUOUSLY. A reader that stopped at the cap would
///    block the child the moment the pipe filled, turning a size limit into a
///    hang — so past the cap the bytes are read and discarded, and the fact
///    is recorded in the file rather than dropped in silence.
/// 2. Nothing is buffered in memory. The file is the store, so a reader
///    (`task_output`) sees bytes at the moment they are produced instead of
///    whenever the process happens to exit.
fn spawn_stream_reader<R>(
    reader: Option<R>,
    mut file: tokio::fs::File,
    cap: u64,
) -> tokio::task::JoinHandle<()>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    tokio::spawn(async move {
        let Some(mut reader) = reader else { return };
        let mut buf = vec![0u8; 8192];
        let mut written: u64 = 0;
        let mut truncated = false;
        loop {
            match reader.read(&mut buf).await {
                Ok(0) => break,
                Ok(n) => {
                    let within = cap == 0 || written < cap;
                    if within {
                        if file.write_all(&buf[..n]).await.is_err() {
                            break;
                        }
                        written += n as u64;
                    } else if !truncated {
                        truncated = true;
                        let note = format!(
                            "\n[output truncated: cap {cap} bytes reached; the run is still \
                             draining, further bytes are not captured]\n"
                        );
                        let _ = file.write_all(note.as_bytes()).await;
                    }
                }
                Err(_) => break,
            }
        }
        let _ = file.flush().await;
    })
}

/// Read a run's capture once its process is gone.
///
/// Bounded on purpose. `Child::wait()` returning means the process exited, but
/// a grandchild that inherited the pipe keeps it open — so waiting on the
/// reader unbounded would hang the caller's turn on exactly the runaway
/// process this feature exists to make visible. The reader is aborted and the
/// file is read instead, which is why the file (not the task's return value) is
/// the store.
async fn collect_capture(mut handle: tokio::task::JoinHandle<()>, path: &std::path::Path) -> Vec<u8> {
    // 500 ms is a flush deadline, not a read deadline: `Child::wait()` returning
    // means the child exited, and the reader then drains whatever is left and
    // hits EOF. A reader still running after that is a grandchild holding the
    // pipe open — the runaway this feature exists to expose — so it is aborted
    // and the file is read directly. The file is the store; the task is only a
    // copier, which is why aborting it cannot lose bytes already written.
    if tokio::time::timeout(std::time::Duration::from_millis(500), &mut handle)
        .await
        .is_err()
    {
        handle.abort();
    }
    tokio::fs::read(path).await.unwrap_or_default()
}

/// The [`CmdResult`] view of a captured run (#692).
///
/// stderr is kept separate on disk because interleaving is lossy and cannot be
/// undone; a consumer wanting the historical merged shape concatenates at read
/// time, which is what this does.
fn cmd_result_from(output: &std::process::Output) -> CmdResult {
    let mut combined = String::from_utf8_lossy(&output.stdout).into_owned();
    let err = String::from_utf8_lossy(&output.stderr);
    if !err.trim().is_empty() {
        if !combined.is_empty() {
            combined.push('\n');
        }
        combined.push_str(&err);
    }
    CmdResult {
        success: output.status.success(),
        code: exit_code(&output.status),
        output: combined,
    }
}

/// The exit code to record for a finished process (#692).
///
/// `ExitStatus::code()` is `None` for a process killed by a signal, and
/// reporting `-1` there would read as an ordinary error — indistinguishable
/// from the command failing on its own. A cancelled run is a different event
/// from a failing one, so it is encoded the way every shell encodes it:
/// `128 + signal`. SIGTERM therefore records `143`, SIGKILL `137`.
fn exit_code(status: &std::process::ExitStatus) -> i32 {
    if let Some(code) = status.code() {
        return code;
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt as _;
        if let Some(signal) = status.signal() {
            return 128 + signal;
        }
    }
    -1
}

/// Write a run's terminal status, best-effort.
fn write_finish(
    id: &str,
    session_id: &Uuid,
    label: &str,
    command: &str,
    elapsed_secs: f32,
    result: &CmdResult,
) {
    if let Err(e) = WorkStatus::finish_command(
        id,
        &session_id.to_string(),
        label,
        command,
        CommandExit {
            success: result.success,
            code: result.code,
            elapsed_secs,
            output_bytes: result.output.len(),
        },
    ) {
        tracing::warn!(
            target: "background_task",
            "Could not write run status for {id}: {e}"
        );
    }
}

/// Drop a finished run's restart-accounting row, loud on failure.
async fn clear_row(run_uuid: Uuid, label: &str) {
    if let Some(repo) = task_repo()
        && let Err(e) = repo.clear(run_uuid).await
    {
        // A stale row makes the NEXT startup report a phantom interruption, so
        // this must be visible even though the command itself succeeded.
        tracing::error!(
            target: "background_task",
            "Failed to clear background run '{label}' after completion: {e:#}"
        );
    }
}

/// Deliver a run's completion into its originating session.
///
/// The ONE gated route (fork #19): the same `deliver_to_session` that sub-agent
/// completions and the `session_notify` tool use, so channel ownership,
/// mid-turn and redirect decisions live in exactly one place instead of being
/// re-derived per surface. Resolves the owner by SESSION, never by whichever
/// service executed the command — a channel session driven from the TUI runs on
/// the TUI's service, and a direct resolve would answer into the TUI and leave
/// the channel that asked waiting on a reply that never comes (#940).
///
/// `interrupt=true`: a completion is the origin's own awaited work, exactly like
/// a sub-agent's; it must reach it even mid-turn (fork #13).
fn deliver_completion(session_id: Uuid, label: &str, command: &str, result: &CmdResult, elapsed_secs: f32) {
    let msg = completion_message(label, command, result, elapsed_secs);
    match super::session_routes::deliver_to_session(session_id, msg, true) {
        super::session_routes::Delivery::Redirected { to } => {
            tracing::info!(
                target: "background_task",
                "Run '{label}' completion for session {session_id} was redirected to \
                 session {to}, which now owns its channel"
            );
        }
        super::session_routes::Delivery::Parked => {
            tracing::info!(
                target: "background_task",
                "Run '{label}' completion for session {session_id} is parked until its \
                 channel claims the session"
            );
        }
        super::session_routes::Delivery::NoRoute => {
            tracing::warn!(
                target: "background_task",
                "Run '{label}' completion for session {session_id} had nowhere to go; the \
                 session will not hear about it"
            );
        }
        super::session_routes::Delivery::RefusedInFlight { .. } => {
            // Unreachable by construction: interrupt=true is passed above, so
            // the fork #13 gate cannot refuse. Kept explicit so a future change
            // to the flag cannot drop the outcome silently (port seam:
            // upstream's match has no catch-all).
            tracing::warn!(
                target: "background_task",
                "Run '{label}' completion for session {session_id} was refused by the \
                 mid-turn gate despite interrupt=true"
            );
        }
        super::session_routes::Delivery::Delivered => {}
    }
}

/// Await a handed-over run, then report it (#692).
///
/// Owns the `Child` for the whole run, which is what keeps `kill_on_drop` from
/// firing the moment the caller's turn ends.
#[allow(clippy::too_many_arguments)]
async fn continue_detached(
    this: std::sync::Arc<BackgroundTaskManager>,
    run_uuid: Uuid,
    id: String,
    session_id: Uuid,
    label: String,
    command: String,
    started: std::time::Instant,
    mut child: tokio::process::Child,
    out_reader: tokio::task::JoinHandle<()>,
    err_reader: tokio::task::JoinHandle<()>,
) {
    let out_path = crate::brain::agent::service::work_status::command_output_path(&id, false);
    let err_path = crate::brain::agent::service::work_status::command_output_path(&id, true);
    let result = match child.wait().await {
        Ok(status) => {
            let stdout = collect_capture(out_reader, &out_path).await;
            let stderr = collect_capture(err_reader, &err_path).await;
            cmd_result_from(&std::process::Output {
                status,
                stdout,
                stderr,
            })
        }
        Err(e) => {
            // Distinct from a command that ran and failed: nothing executed at
            // all, so the exit code below is not one the command produced.
            tracing::error!(
                target: "background_task",
                "Detached run '{label}' could not be awaited: {e}"
            );
            CmdResult {
                success: false,
                code: -1,
                output: format!("failed to await: {e}"),
            }
        }
    };
    // Capture ONCE: the log line, the status file and the receipt payload (#15)
    // must all report the same runtime.
    let elapsed_secs = started.elapsed().as_secs_f32();
    tracing::info!(
        target: "background_task",
        "Run '{label}' for session {session_id} finished (success={}, exit={}, elapsed={:.1}s)",
        result.success,
        result.code,
        elapsed_secs
    );
    write_finish(&id, &session_id, &label, &command, elapsed_secs, &result);
    clear_row(run_uuid, &label).await;
    // Clear the indicator BEFORE delivering, not after: the work is over the
    // moment the process exits, so a badge that outlived it would show the
    // agent reporting a finished task as still running.
    this.finish_run(&id);
    deliver_completion(session_id, &label, &command, &result, elapsed_secs);
}

/// The background-task repository, when a pool exists.
///
/// Resolved per call through the global pool rather than threaded through the
/// manager, because `spawn_command` is reached from the bash tool which has no
/// pool in its context. `None` before the DB is initialized (early startup,
/// tests), which simply means restart accounting is skipped.
pub(super) fn task_repo() -> Option<crate::db::BackgroundTaskRepository> {
    crate::db::global_pool().map(|p| crate::db::BackgroundTaskRepository::new(p.clone()))
}

/// A short human label for a command (first meaningful token sequence), for the
/// "running in the background" acknowledgement and the completion tag.
pub(crate) fn short_label(command: &str) -> String {
    let after_cd = crate::utils::command_label::command_label(command);
    let label: String = after_cd.chars().take(60).collect();
    if after_cd.chars().count() > 60 {
        format!("{label}…")
    } else {
        label
    }
}

/// Keep only the last `n` lines of `text`.
pub(crate) fn tail_lines(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.len().saturating_sub(n);
    lines[start..].join("\n")
}

/// Build the resume message from a finished background command (#722). Pure so
/// the framing is unit-testable without spawning anything. `elapsed_secs` is
/// the detached command's wall-clock runtime; it rides along in the typed
/// `BgTaskMeta` payload (#15) so the receipt card renders a duration without
/// parsing the context text.
pub(crate) fn completion_message(
    label: &str,
    command: &str,
    result: &CmdResult,
    elapsed_secs: f32,
) -> QueuedUserMessage {
    let status = if result.success {
        "exit 0 (success)".to_string()
    } else {
        format!("exit {} (failure)", result.code)
    };
    let tail = tail_lines(&result.output, 50);
    let context = format!(
        "[System: the background task you started has finished.\n\
         Task: {label}\n\
         Command: {command}\n\
         Status: {status}\n\
         Output (last 50 lines):\n{tail}\n\n\
         Report the result to the user and continue anything that was waiting on it. \
         Do not re-run the command — this IS its result.]"
    );
    let display = format!(
        "🔧 background task {}: {label}",
        if result.success { "finished" } else { "failed" }
    );
    let mut msg = QueuedUserMessage::system(context, display);
    // #1221: marks this delivery for the Telegram collapsible echo bubble.
    msg.origin = PushOrigin::BackgroundTask;
    // #15: typed receipt payload — the echo renders the card from this,
    // never from the `[System: ...]` context text.
    msg.bg_meta = Some(BgTaskMeta {
        success: result.success,
        label: label.to_string(),
        elapsed_secs,
        tail,
    });
    msg
}

/// Human duration for the receipt card (#15): `42s`, `3m 5s`, `1h 12m`.
/// Rounds to whole seconds; sub-second tasks show `0s`.
pub(crate) fn format_elapsed(secs: f32) -> String {
    let total = secs.max(0.0).round() as u64;
    if total < 60 {
        format!("{total}s")
    } else if total < 3600 {
        format!("{}m {}s", total / 60, total % 60)
    } else {
        format!("{}h {}m", total / 3600, (total % 3600) / 60)
    }
}
