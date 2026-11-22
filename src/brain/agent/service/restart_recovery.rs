//! Delivery of restart-recovery reports to the session that owns them (#1037).
//!
//! A report about work a previous process was doing must reach the surface
//! that started that work, not whichever surface happened to boot. Two things
//! stand in the way:
//!
//! - Recovery runs during startup, before any channel has called
//!   [`super::session_routes::register_session_route`], so the route map is
//!   empty at the moment the report is produced. Delivering immediately sends
//!   a channel session's report to the local surface, which is the bug #940
//!   fixed for the completion path and the restart path never got.
//! - A channel that never comes up in this run would strand the report
//!   forever if we simply waited for a route.
//!
//! So a report is parked when no route claims its session yet, delivered the
//! moment one does, and flushed to the local surface once the grace period
//! expires. Nothing is dropped on either branch.

use std::sync::Mutex;
use std::time::Duration;

use uuid::Uuid;

use super::types::{MessageEnqueueCallback, PushOrigin, QueuedUserMessage};

/// How long a parked report waits for its channel to register a route before
/// it is delivered locally instead. Long enough for channels to finish
/// connecting, short enough that a report is not lost to a user who is
/// looking at the local surface right now.
pub const ROUTE_GRACE: Duration = Duration::from_secs(30);

/// Reports whose session had no route when they were produced.
static PARKED: Mutex<Vec<(Uuid, QueuedUserMessage)>> = Mutex::new(Vec::new());

/// Sessions known to belong to a channel that has not claimed them yet.
///
/// Startup crash-recovery revives a session straight into a turn, bypassing
/// the ingress handlers where `claim_for_channel` lives, so the route map
/// says nothing about it while it runs. The fallbacks then treat it as
/// local: on a daemon that is a void, and under the TUI it is worse than a
/// void, because the answer is delivered to the wrong surface and looks
/// fine. Recovery knows which channel the session came from, so it records
/// that here and the fallbacks park instead of guessing (#1206).
static AWAITING_CHANNEL: Mutex<Option<std::collections::HashSet<Uuid>>> = Mutex::new(None);

/// Record that `session_id` belongs to a channel which has not claimed it.
///
/// Idempotent. Cleared by [`claim_session`] once the owning channel binds a
/// route, so this never outlives the gap it describes.
pub fn expect_channel_route(session_id: Uuid) {
    match AWAITING_CHANNEL.lock() {
        Ok(mut guard) => {
            guard
                .get_or_insert_with(std::collections::HashSet::new)
                .insert(session_id);
        }
        Err(e) => {
            // Without the mark this session's completions fall back to the
            // local surface, which is the mis-delivery this prevents.
            tracing::error!(
                target: "background_task",
                "Could not mark session {session_id} as channel-owned: {e}"
            );
        }
    }
}

/// Does `session_id` belong to a channel that has not claimed it yet?
///
/// While true, a completion for it must be parked rather than handed to
/// whichever surface happens to be executing.
pub fn awaits_channel_route(session_id: Uuid) -> bool {
    match AWAITING_CHANNEL.lock() {
        Ok(guard) => guard.as_ref().is_some_and(|s| s.contains(&session_id)),
        Err(e) => {
            tracing::error!(
                target: "background_task",
                "Could not read the channel-owned mark for session {session_id}: {e}"
            );
            false
        }
    }
}

/// Forget the channel-owned mark for `session_id`.
fn clear_channel_expectation(session_id: Uuid) {
    match AWAITING_CHANNEL.lock() {
        Ok(mut guard) => {
            if let Some(set) = guard.as_mut() {
                set.remove(&session_id);
            }
        }
        Err(e) => {
            // Only costs a redundant park on a session that now has a real
            // route, which the route itself takes precedence over.
            tracing::warn!(
                target: "background_task",
                "Could not clear the channel-owned mark for session {session_id}: {e}"
            );
        }
    }
}

/// Deliver `msg` to whoever owns `session_id`, parking it if nobody does yet.
///
/// Returns whether it went out immediately. A parked report is not lost: it
/// leaves on the next [`claim_session`] for that session, or on
/// [`flush_parked`] when the grace period ends.
pub fn deliver_or_park(session_id: Uuid, msg: QueuedUserMessage) -> bool {
    if let Some(route) = super::session_routes::session_route(session_id) {
        route(session_id, msg);
        return true;
    }
    match PARKED.lock() {
        Ok(mut parked) => parked.push((session_id, msg)),
        Err(e) => {
            // The report is about to vanish, which is the exact failure this
            // module exists to prevent, so it is an error rather than a warn.
            tracing::error!(
                target: "background_task",
                "Could not park restart report for session {session_id}, it is lost: {e}"
            );
        }
    }
    false
}

/// Hand a newly routed session everything parked for it.
///
/// Called when a surface registers a route, so a channel that connects after
/// startup still receives what its session missed. Returns how many went out.
pub fn claim_session(session_id: Uuid, route: &MessageEnqueueCallback) -> usize {
    // The gap this mark describes is over: a real route now owns the session.
    clear_channel_expectation(session_id);
    let mine = match PARKED.lock() {
        Ok(mut parked) => {
            let mut mine = Vec::new();
            parked.retain(|(id, msg)| {
                if *id == session_id {
                    mine.push(msg.clone());
                    false
                } else {
                    true
                }
            });
            mine
        }
        Err(e) => {
            tracing::error!(
                target: "background_task",
                "Could not read parked restart reports for session {session_id}: {e}"
            );
            return 0;
        }
    };
    let count = mine.len();
    for msg in &mine {
        route(session_id, msg.clone());
    }
    if count > 0 {
        tracing::info!(
            target: "background_task",
            "Delivered {count} parked restart report(s) to session {session_id}"
        );
        // Delivery happened: any durable copies of these reports are now
        // redundant. Fire-and-forget — a failed clear costs a duplicate
        // after the next restart, never a lost report (#73).
        clear_persisted_tombstones(session_id);
        // notify_queue twin (#111): clear ONLY the rows matching what was
        // just delivered — a blanket clear-for-session here could eat a
        // never-delivered mid-turn row for the same session.
        //
        // Ordering (#439/#366, shape A): these clears run AFTER the routes
        // above, and `clear_on_delivery` now WAITS for the row in its own
        // window — because `route` is fire-and-forget and defers its
        // `persist` into a spawn, the delete would otherwise reach the pool
        // before the INSERT and the row would survive to mint one fresh
        // duplicate at the next boot.
        for msg in &mine {
            super::notify_queue::clear_on_delivery(session_id, msg);
        }
    }
    count
}

/// Deliver everything still parked to `local`, whatever session it belongs to.
///
/// The last resort: the owning channel never came up in this run, and holding
/// the report indefinitely would be the same silent loss as never producing
/// it. Returns how many were flushed.
pub fn flush_parked(local: &MessageEnqueueCallback) -> usize {
    let remaining = match PARKED.lock() {
        Ok(mut parked) => std::mem::take(&mut *parked),
        Err(e) => {
            tracing::error!(
                target: "background_task",
                "Could not flush parked restart reports: {e}"
            );
            return 0;
        }
    };
    let count = remaining.len();
    for (session_id, msg) in remaining {
        local(session_id, msg.clone());
        // Delivered locally: the durable copies are redundant now (#73).
        clear_persisted_tombstones(session_id);
        // notify_queue twin (#111) — content-exact clear of what just went
        // out; a stale row on clear failure is a duplicate, not a loss.
        super::notify_queue::clear_on_delivery(session_id, &msg);
    }
    if count > 0 {
        tracing::info!(
            target: "background_task",
            "No route claimed {count} restart report(s) within the grace period, delivered locally"
        );
    }
    count
}

/// Schedule [`flush_parked`] to run once the grace period is over.
pub fn schedule_flush(local: MessageEnqueueCallback) {
    tokio::spawn(async move {
        tokio::time::sleep(ROUTE_GRACE).await;
        flush_parked(&local);
    });
}

/// How many reports are waiting for a route. Exposed for tests and for a
/// surface that wants to say something is still pending.
pub fn parked_count() -> usize {
    PARKED.lock().map(|p| p.len()).unwrap_or(0)
}

/// The destination for a process that has no local surface of its own (#1206).
///
/// A headless daemon's only surfaces are its channels. It still builds the
/// TUI's enqueue callback and hands it out as the fallback for a session no
/// channel has claimed, so a completion for such a session was pushed into a
/// `TuiEvent` channel that nothing in a daemon ever drains, and the send
/// error was discarded. Nothing resumed, nothing logged.
///
/// Sessions revived by startup crash-recovery are exactly that case: they run
/// a turn without ever passing through an ingress handler, and
/// `claim_for_channel` is only reached from one, so they stay unclaimed until
/// their channel sees the next inbound message.
///
/// Parking keeps them until a channel claims the session, at which point
/// [`claim_session`] flushes them to the surface that actually owns it. This
/// is a fallback destination, never a route: a session a channel HAS claimed
/// still takes the fast path and never reaches here.
pub fn parking_route() -> MessageEnqueueCallback {
    std::sync::Arc::new(|session_id, msg: QueuedUserMessage| {
        // Worth one line each time: a parked message is not lost, but it is
        // also not delivered yet, and that difference is invisible otherwise.
        tracing::warn!(
            target: "background_task",
            "No surface claims session {session_id} and this process has no local one; \
             parking until its channel claims it: {}",
            msg.display_text
        );
        // Durable twin (#111): the in-memory park dies with the process, the
        // row survives it. Best-effort, fire-and-forget.
        super::notify_queue::persist(session_id, &msg);
        deliver_or_park(session_id, msg);
    })
}

#[cfg(test)]
pub(crate) fn clear_parked_for_test() {
    if let Ok(mut parked) = PARKED.lock() {
        parked.clear();
    }
    if let Ok(mut awaiting) = AWAITING_CHANNEL.lock()
        && let Some(set) = awaiting.as_mut()
    {
        set.clear();
    }
}

/// Serialize tests that touch the process-global parked queue or the
/// channel-owned set, and start each from a clean slate.
///
/// The lock lives beside the state it guards rather than in one suite,
/// because more than one suite exercises that state: a second suite with its
/// own lock does not serialize against the first, and the two interleave
/// their parks. Found exactly that way (#1206).
#[cfg(test)]
pub(crate) fn test_guard() -> std::sync::MutexGuard<'static, ()> {
    static TEST_LOCK: Mutex<()> = Mutex::new(());
    let guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear_parked_for_test();
    guard
}

/// Account for background tasks that were running when a previous process
/// died, then clear them.
///
/// Every surviving row belonged to a process that no longer exists, so there
/// is no in-memory handle left to wait on. The CHILD is a different question:
/// detached runs are `setsid()`ed and outlive the daemon by design, so each row
/// is reported into its session with whatever the surviving surfaces can
/// actually say about it — a measured interruption where a status file records
/// the run, and an explicitly unknown outcome where nothing does (#763, #913).
///
/// Returns the run ids it accounted for — which is also the index the status
/// FILE leg needs, since the row id and the file id are the same uuid
/// (#848). It is returned rather than re-read by the caller because this
/// function clears each row as it goes, so a later read would find nothing.
pub async fn report_interrupted() -> Vec<Uuid> {
    let Some(repo) = super::background_tasks::task_repo() else {
        return Vec::new();
    };
    let rows = match repo.all().await {
        Ok(rows) => rows,
        Err(e) => {
            tracing::error!(target: "background_task", "Failed to read background tasks: {e:#}");
            return Vec::new();
        }
    };
    if rows.is_empty() {
        return Vec::new();
    }
    let mut reported = Vec::with_capacity(rows.len());
    for row in rows {
        let row_id = row.id;
        tracing::warn!(
            target: "background_task",
            "Background task '{}' for session {} was interrupted by a restart",
            row.label,
            row.session_id
        );
        // By session, never by whoever booted. This path used to take the
        // caller's callback directly, so a channel session's interruption
        // landed on the local surface — the shape #940 fixed for completions
        // and left standing here. Startup runs before channels register, so
        // an unclaimed session parks rather than mis-delivers (#1037).
        deliver_or_park(row.session_id, interrupted_message(&row));
        reported.push(row_id);

        // Clear per row, only after it is accounted for. clear_all() used to
        // run regardless, so a row whose report never got produced was
        // dropped from the table anyway and its session never heard anything.
        if let Err(e) = repo.clear(row_id).await {
            // A surviving row re-reports the same interruption next start,
            // which is noisy but recoverable; the report itself already
            // landed, so this is not fatal.
            tracing::error!(
                target: "background_task",
                "Failed to clear background task '{}' after reporting it: {e:#}",
                row.label
            );
        }
    }
    reported
}

/// What happened to one interrupted boot row, so that clearing it is a
/// decision rather than a blanket wipe (#481).
///
/// Every arm of the boot loop has to account for the row it is holding. Naming
/// the outcome is what lets the log say *why* the row went, and what keeps the
/// `NotDispatched` case — a row nothing could be done with — from looking
/// exactly like a handed-off one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowOutcome {
    /// The hand-off was attempted and returned: a system push was re-delivered,
    /// or a user turn was resumed. The row has been accounted for.
    HandedOff,
    /// Another row for the same session was already handled this boot, so this
    /// duplicate had no work of its own left to do.
    Superseded,
    /// No hand-off could be attempted: the row's `session_id` is not a UUID, so
    /// there is no session to deliver it to. Reported and LEFT IN PLACE — it is
    /// the only evidence that session was ever interrupted, and erasing a row no
    /// boot can resume would hide the one fact worth keeping.
    NotDispatched,
}

impl RowOutcome {
    /// The stable spelling used in log lines and asserted by tests.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::HandedOff => "handed_off",
            Self::Superseded => "superseded",
            Self::NotDispatched => "not_dispatched",
        }
    }
}

/// A value with its newlines escaped, for use as a structured log field.
///
/// `id`/`session_id`/`origin` are read straight out of the database, and a
/// `session_id` that does not parse as a UUID is precisely the row this module
/// now has to account for — so the value is not assumed well-formed. An
/// embedded newline would split the record and detach every field after it
/// onto an orphan line.
fn log_field(value: &str) -> String {
    value.replace('\n', "\\n").replace('\r', "\\r")
}

/// Clear ONE interrupted row, once its disposition is known (#481).
///
/// The boot path used to wipe the whole table before it had resumed anything,
/// so a row whose hand-off never produced a report was already gone: the work
/// disappeared with nothing left to resume and nothing said about it. Deleting
/// per row, after the arm has accounted for it, is the discipline the
/// background-task leg already applies ([`report_interrupted`]).
///
/// Takes the repository rather than holding one: the boot loop owns the
/// connection, and recovery must not open a second one.
///
/// Emits exactly ONE line per row, carrying the row's identity and its outcome,
/// so a boot's dispositions are readable afterwards from the log alone.
/// `NotDispatched` is the exception to the deleting: it reports and leaves the
/// row, and says so. No free-form text goes into the fields — the row's
/// `user_message` is the whole agent input and does not belong on this line.
///
/// Returns whether the row was cleared, so a caller can tell a disposal from a
/// report without re-querying the table.
pub async fn dispose_pending_row(
    repo: &crate::db::repository::pending_request::PendingRequestRepository,
    row: &crate::db::repository::pending_request::PendingRequest,
    outcome: RowOutcome,
) -> bool {
    if outcome == RowOutcome::NotDispatched {
        // The never-resumable arm: no hand-off was possible, so the row is the
        // only record that this session was interrupted. Reported, not erased.
        tracing::warn!(
            target: "restart_recovery",
            row_id = %log_field(&row.id),
            session_id = %log_field(&row.session_id),
            origin = %log_field(&row.origin),
            outcome = outcome.as_str(),
            "boot recovery could not dispatch this row — session_id is not a UUID; left in place"
        );
        return false;
    }

    match repo.delete_ids(vec![row.id.clone()]).await {
        Ok(()) => {
            tracing::info!(
                target: "restart_recovery",
                row_id = %log_field(&row.id),
                session_id = %log_field(&row.session_id),
                origin = %log_field(&row.origin),
                outcome = outcome.as_str(),
                "boot recovery accounted for an interrupted row"
            );
            true
        }
        Err(e) => {
            // The row stays for the next boot, which is the safe direction: the
            // hand-off already happened, so re-seeing it is noise, not loss.
            tracing::error!(
                target: "restart_recovery",
                row_id = %log_field(&row.id),
                session_id = %log_field(&row.session_id),
                origin = %log_field(&row.origin),
                outcome = outcome.as_str(),
                error = %e,
                "failed to clear an accounted-for interrupted row — it will be seen again next boot"
            );
            false
        }
    }
}

/// The continuation prompt a boot-resumed `user` turn is framed with (#481).
///
/// One home for the wording: both hand-off arms that resume a pending row frame
/// their wake with it, so the surface contract below cannot drift between them.
/// The boot classifier's `spawn_resumes` uses this const too, but the *bare*
/// const — its wake belongs to the classifier rather than to a pending row, and
/// its signature takes `&'static str`, so a per-session id cannot be threaded
/// through it.
///
/// **The wording is a contract, not copy.** *"Do not mention the restart or any
/// interruption"* must survive: a resumed turn that announces the restart shows
/// the user a restart notice on every swap.
pub(crate) const RESUME_CONTINUATION_BASE: &str = "[System: A restart just occurred while you were processing a request. Read the conversation \
context and continue where you left off naturally. Do not mention the restart or any \
interruption — just pick up seamlessly.]";

/// Frame a boot-resumed `user` turn: the base continuation, then the one fact the
/// agent cannot read off a transcript that looks complete (#481).
///
/// A turn killed mid-flight leaves a transcript whose last entry is frequently a
/// finished tool call, so nothing in it separates *"the work completed"* from
/// *"the process died between the work and the report"*. The appended clause
/// states that ambiguity and puts the check on the side-effecting steps, where
/// repeating one is expensive or destructive.
pub(crate) fn resumed_turn_prompt(session_id: Uuid) -> String {
    format!(
        "{} The previous attempt (session {}) was killed mid-turn and did NOT finish; part of the \
work may already have been completed. Before repeating a side-effecting step — a send, a write, \
an install, a commit — verify from the world whether it already landed.",
        RESUME_CONTINUATION_BASE,
        &session_id.simple().to_string()[..8],
    )
}

/// What survives a restart about one detached run, read from the surfaces that
/// actually recorded it (#913).
///
/// The DB row this report is built from carries `id, session_id, label,
/// command, started_at` — no pid, no capture path, no exit code — so the row
/// alone cannot say what happened to the run. The status FILE written at spawn
/// (`tmp/detached/<id>.json`) does: it carries `pid` and both capture paths,
/// and its `finish.code` once the child was reaped. That file exists precisely
/// so a reader can tell live from dead across a restart, and this is the reader.
struct RunEvidence {
    /// The spawned shell's pid, when a surface recorded one.
    pid: Option<u32>,
    /// Absolute path of the run's captured stdout, when one is recorded.
    output_out: Option<String>,
    /// Absolute path of the run's captured stderr, when one is recorded.
    output_err: Option<String>,
    /// The exit code the child was reaped with, when it finished before the
    /// restart — an exit status measured, not inferred.
    exit_code: Option<i32>,
}

impl RunEvidence {
    /// Gather what the restart left behind for one run: the status file first,
    /// then the captures, which are deterministic from the run id alone.
    fn for_row(row: &crate::db::BackgroundTaskRow) -> Self {
        use super::work_status::{WorkStatus, command_output_path};

        let id = row.id.to_string();
        let status = WorkStatus::read(&id);
        let recorded = |p: Option<String>| p.filter(|p| !p.is_empty());
        // A derived path is evidence only when the file is really there: a
        // path that 404s cannot describe the run, and a fabricated one is
        // indistinguishable from a run whose captures were swept.
        let existing = |stderr: bool| {
            let path = command_output_path(&id, stderr);
            if path.exists() {
                Some(path.to_string_lossy().into_owned())
            } else {
                None
            }
        };

        Self {
            pid: status.as_ref().and_then(|s| s.pid),
            output_out: recorded(status.as_ref().and_then(|s| s.output_out.clone()))
                .or_else(|| existing(false)),
            output_err: recorded(status.as_ref().and_then(|s| s.output_err.clone()))
                .or_else(|| existing(true)),
            exit_code: status
                .as_ref()
                .and_then(|s| s.finish.as_ref())
                .and_then(|f| f.code),
        }
    }

    /// Whether anything here can describe the run at all.
    ///
    /// `false` means no pid, no exit code and no capture: nothing measured
    /// what became of the run, so the report must not claim it was killed or
    /// that it produced no result. A detached run is `setsid()`ed and outlives
    /// the daemon by design, so "the row survived a restart" says nothing
    /// about the child — a report built on it would be a claim of absence that
    /// the row cannot support (#913).
    fn is_empty(&self) -> bool {
        self.pid.is_none()
            && self.exit_code.is_none()
            && self.output_out.is_none()
            && self.output_err.is_none()
    }

    /// The capture that actually holds bytes, stdout first, as `(path, bytes)`.
    ///
    /// The one filesystem read this report makes, and it is a `stat` on the
    /// run's OWN capture — the artifact whose contents the report is about, not
    /// a guess about the process. A path is not a result: a recorded capture
    /// that is empty and one that already holds the deploy's output are the
    /// same `Option<String>` until somebody looks (#936).
    fn captured_output(&self) -> Option<(&str, u64)> {
        let candidates = [self.output_out.as_deref(), self.output_err.as_deref()];
        candidates.into_iter().flatten().find_map(|path| {
            let bytes = std::fs::metadata(path).ok()?.len();
            (bytes > 0).then_some((path, bytes))
        })
    }

    /// A recorded capture that exists and is measurably empty — a zero-byte
    /// file, which is the only shape in which "no result" is an observation.
    fn empty_capture(&self) -> Option<&str> {
        let candidates = [self.output_out.as_deref(), self.output_err.as_deref()];
        candidates
            .into_iter()
            .flatten()
            .find(|path| std::fs::metadata(*path).is_ok_and(|m| m.len() == 0))
    }

    /// What the run left in its capture, phrased as a clause the report states.
    ///
    /// `produced no result` survives only where the absence was MEASURED — a
    /// zero-byte capture. Where a capture holds bytes the report names the
    /// count and the path, because the bytes are the result and a reader told
    /// "no result" throws them away. Where no capture was recorded at all it
    /// says it does not know: an absent file is not an absent result, since a
    /// detached run writes its own captures and may still be writing them
    /// (#936).
    ///
    /// No WHEN is stated: the count is what the file holds at report time, and
    /// a detached run outlives the restart, so bytes present now may have been
    /// written after it. "produced N bytes before the restart" would be one
    /// more claim this path did not take (#936).
    fn result_clause(&self) -> String {
        if let Some((path, bytes)) = self.captured_output() {
            return format!("its capture at {path} holds {bytes} bytes; read them there");
        }
        match self.empty_capture() {
            Some(path) => format!("produced no result — its capture at {path} is empty"),
            None => {
                "no capture was recorded, so whether it produced output is not known".to_string()
            }
        }
    }
}

/// The subjects a shell command names, read from its text alone (#936).
///
/// A boot row records the pid of the *spawned shell* — the watcher — not of
/// whatever that shell was watching. Row `d594a148` ran `while kill -0
/// 2349610; do sleep 30; done` and carried `pid: 2366240`; 2349610 was the
/// chain that mattered, and no column in the row answered the only question a
/// lane had. It re-derived the answer by hand four times, then started a
/// second chain because it could not rule the first one out.
///
/// This names what the command references and stops there. A liveness probe
/// would answer "alive at the moment of writing" — and the lane reads the
/// report hours later, when that answer is a new fact rather than this one. So
/// the report states what was not checked and leaves the check to the reader.
///
/// **Pure string scan.** No `/proc`, no `ps`, no `systemctl`, no subprocess:
/// this runs on the boot path, where a probe would block a restart on the very
/// processes it is reporting about.
pub(crate) fn referenced_subjects(command: &str) -> Vec<String> {
    // Shell punctuation is stripped from the ends of each word so `2349610;`,
    // `"$pid"` and `$(pgrep -f foo)` yield their operands. Nothing is
    // expanded: a variable cannot be resolved without the shell.
    let words: Vec<String> = command
        .split_whitespace()
        .map(|w| {
            w.trim_matches(|c: char| {
                matches!(
                    c,
                    '\'' | '"' | '`' | '(' | ')' | ';' | '|' | '&' | '$' | '{' | '}' | '<' | '>'
                )
            })
            .to_string()
        })
        .filter(|w| !w.is_empty())
        .collect();

    let mut subjects: Vec<String> = Vec::new();
    let mut i = 0;
    while i < words.len() {
        match words[i].as_str() {
            // `kill -0 <pid>`: signal 0 IS a liveness check, so its operands
            // are exactly the pids a reader will want to re-check.
            "kill" => {
                let mut j = i + 1;
                let mut zero = false;
                while j < words.len() {
                    match words[j].as_str() {
                        "-0" | "-s0" | "--signal=0" => {
                            zero = true;
                            j += 1;
                        }
                        "-s" => {
                            if words.get(j + 1).is_some_and(|s| s.as_str() == "0") {
                                zero = true;
                                j += 2;
                            } else {
                                break;
                            }
                        }
                        w if w.starts_with('-') => j += 1,
                        _ => break,
                    }
                }
                if zero {
                    while j < words.len() && is_pid(&words[j]) {
                        note_subject(&mut subjects, format!("pid {}", words[j]));
                        j += 1;
                    }
                }
                i = j;
            }
            // `ps -p <pid>`, `ps -p<pid>` and `ps --pid=<pid>`.
            "ps" => {
                let mut j = i + 1;
                while j < words.len() {
                    let w = words[j].as_str();
                    if w == "-p" || w == "--pid" {
                        j += 1;
                        while j < words.len() && is_pid(&words[j]) {
                            note_subject(&mut subjects, format!("pid {}", words[j]));
                            j += 1;
                        }
                        continue;
                    }
                    if let Some(pid) = w.strip_prefix("--pid=")
                        && is_pid(pid)
                    {
                        note_subject(&mut subjects, format!("pid {pid}"));
                    } else if let Some(pid) = w.strip_prefix("-p")
                        && is_pid(pid)
                    {
                        note_subject(&mut subjects, format!("pid {pid}"));
                    } else if !w.starts_with('-') {
                        // A bare operand is a process name, not a `-p` subject.
                        break;
                    }
                    j += 1;
                }
                i = j;
            }
            // `pgrep ... <pattern>`: the pattern is the subject.
            "pgrep" => {
                let mut j = i + 1;
                while j < words.len() {
                    let w = words[j].as_str();
                    if w.starts_with('-') {
                        j += if PGREP_VALUE_FLAGS.contains(&w) { 2 } else { 1 };
                        continue;
                    }
                    note_subject(&mut subjects, format!("pattern {w}"));
                    break;
                }
                i = j;
            }
            w => {
                // `systemd-run ... --unit=<name>`, wherever it appears.
                if let Some(name) = w.strip_prefix("--unit=")
                    && !name.is_empty()
                {
                    note_subject(&mut subjects, format!("unit {name}"));
                } else if w == "--unit"
                    && let Some(name) = words.get(i + 1)
                {
                    note_subject(&mut subjects, format!("unit {name}"));
                }
                i += 1;
            }
        }
    }
    subjects
}

/// `pgrep` flags that consume the following word, so it is not the pattern.
const PGREP_VALUE_FLAGS: [&str; 8] = ["-u", "-U", "-g", "-G", "-P", "-t", "-d", "-F"];

/// Record `subject` once, in the order the command names it.
fn note_subject(subjects: &mut Vec<String>, subject: String) {
    if !subjects.contains(&subject) {
        subjects.push(subject);
    }
}

/// Whether `word` is a bare process id. A shell variable (`$PID`) is not:
/// resolving it would need the shell this scan deliberately does not run.
fn is_pid(word: &str) -> bool {
    !word.is_empty() && word.chars().all(|c| c.is_ascii_digit())
}

/// The pids among `subjects`, in the order they were named.
fn referenced_pids(subjects: &[String]) -> Vec<u32> {
    subjects
        .iter()
        .filter_map(|s| s.strip_prefix("pid ").and_then(|p| p.parse().ok()))
        .collect()
}

/// What the agent is told about a command a restart interrupted. Deliberately
/// states what is known and hands the decision back, rather than re-running
/// something expensive on the agent's behalf.
///
/// With evidence on disk this keeps the long-standing interruption report.
/// Without any — the row is all the restart left — it reports the outcome as
/// UNKNOWN and names what to check, because "was killed and produced no
/// result" is a measurement this path never took (#913).
///
/// What it will NOT say is that the work died. The recorded pid belongs to the
/// spawned shell, which is a watcher whenever the command itself watches a
/// process (`while kill -0 …`) or waits on a unit, and those outlive the row:
/// one lane was told "killed" about a `systemd-run --wait` gate that was still
/// active 55.7 minutes later and exited rc=1 106 minutes after the row buried
/// it, and another started a SECOND ship chain because it could not rule out
/// that the first was running (#936). So the report names the subjects the
/// command references, says their liveness was not checked, and states what
/// the capture measured — and claims nothing else.
pub(crate) fn interrupted_message(row: &crate::db::BackgroundTaskRow) -> QueuedUserMessage {
    let evidence = RunEvidence::for_row(row);
    if evidence.is_empty() {
        return unknown_outcome_message(row);
    }
    let subjects = referenced_subjects(&row.command);
    let mut context_text = format!(
        "[BACKGROUND TASK INTERRUPTED] `{}` was in flight when OpenCrabs restarted. The command \
         was:\n\n```\n{}\n```",
        row.label, row.command
    );
    if !subjects.is_empty() {
        context_text.push_str(&format!(
            "\n\ncommand references: {} — liveness not checked",
            subjects.join(" · ")
        ));
    }
    if let Some(pid) = evidence.pid {
        let referenced = referenced_pids(&subjects);
        // The row's pid is the spawned shell's. Where the command names other
        // pids, that shell was watching them — and a reader who mistakes one
        // for the other acts on the wrong process (#936).
        if !referenced.is_empty() && !referenced.contains(&pid) {
            context_text.push_str(&format!("\n\nrow pid {pid} is the watcher, not the work"));
        }
    }
    context_text.push_str("\n\n");
    context_text.push_str(&evidence.result_clause());
    context_text.push_str(
        "\n\nNothing here says whether the work finished, and nothing above was checked for \
         liveness: the subjects named are what the command was watching or waiting on, and that \
         check is yours to make. Decide whether to run it again based on what you were doing; do \
         not assume it passed or failed, and do not re-run something that is still live.",
    );
    QueuedUserMessage {
        context_text,
        display_text: format!("⚠️ Background task interrupted by restart: {}", row.label),
        origin: PushOrigin::Recovery,
        bg_meta: None,
    }
}

/// The report for a run the restart left no evidence about (#913).
///
/// Detached runs are `setsid()`ed, so they survive the daemon's death and keep
/// writing their captures; the old wording asserted a kill and an empty result
/// that nothing had checked. Acting on it has already cost real work: a lane
/// was told "killed, no result" while `ps -p` showed the process alive and the
/// deploy it ran had already advanced. The honest report is the one that says
/// it does not know, names what is missing, and points at the checks that can
/// answer.
fn unknown_outcome_message(row: &crate::db::BackgroundTaskRow) -> QueuedUserMessage {
    let mut context_text = format!(
        "[BACKGROUND TASK OUTCOME UNKNOWN] `{}` was in flight when OpenCrabs restarted, and no \
         pid, no exit code and no captured output were recorded for it — so whether it is still \
         running, already finished, or died with the old process is NOT KNOWN. It was not \
         observed to be killed, and it did not necessarily produce nothing. The command \
         was:\n\n```\n{}\n```\n\nIt ran detached (`setsid`), which means it can outlive the \
         daemon and keep writing its output. Before you re-run it, check what the world \
         says:\n- `ps -p <pid>` / `pgrep -af` for the command above, to see whether it is still \
         alive;\n- `task_output` for this run, if the harness still lists it;\n- the effect the \
         command would have (a file, a deployed sha, a sent message), to see whether it already \
         landed.\n\nRe-running a run that is still live or already finished can duplicate a \
         side-effecting step.",
        row.label, row.command
    );
    // The row recorded nothing, so the command TEXT is the only place the
    // subjects can come from — and they are what the checks above need to be
    // pointed at (#936).
    let subjects = referenced_subjects(&row.command);
    if !subjects.is_empty() {
        context_text.push_str(&format!(
            "\n\ncommand references: {} — liveness not checked",
            subjects.join(" · ")
        ));
    }
    QueuedUserMessage {
        context_text,
        display_text: format!(
            "⚠️ Background task outcome unknown after restart: {}",
            row.label
        ),
        origin: PushOrigin::Recovery,
        bg_meta: None,
    }
}

/// Account for everything a previous process was doing, and arrange for the
/// reports to reach the right sessions.
///
/// Run once per process start by every surface: the TUI, the daemon, and the
/// headless commands. It used to live only in the TUI's startup, so a daemon
/// start left interrupted work unreported and its rows in the table until
/// somebody happened to open the TUI, at which point work from an arbitrarily
/// old process was announced as if it had just died.
///
/// `local` is the surface doing the booting, used only as the last-resort
/// destination once the grace period expires. `None` when the process has no
/// local surface at all (a headless daemon, whose only surfaces are its
/// channels): there is nothing to flush TO, so parked reports keep waiting
/// for a channel to claim their session instead of being handed to a
/// destination that would discard them (#1206).
pub async fn recover(local: Option<MessageEnqueueCallback>) -> usize {
    // Migrate pre-#26 sub-agent status files into the unified detached dir
    // before reconciliation, so sub-agents in flight across upgrades are preserved (#1038, #192).
    let legacy = super::work_status::legacy_dir();
    let migrated = super::work_status::migrate_legacy_dir(&legacy);
    if migrated > 0 {
        tracing::info!(
            target: "background_task",
            count = migrated,
            from = %legacy.display(),
            "Migrated legacy subagent status files to unified detached directory"
        );
    }

    // Sub-agents first: they die with the process but their status files do
    // not, so every file still mid-flight is an agent that no longer exists.
    //
    // DETACHED (#858). The pass walks the whole status directory — 30k files on
    // a long-lived host — and boot used to sit on it before the A2A listener
    // could bind, which left the listener dead for the whole window. It now runs
    // on the blocking pool and the reports it produces are delivered from the
    // same task: `deliver_or_park` parks and `persist_tombstone` makes the report
    // durable, which already covers a route that is not ready yet — exactly the
    // state boot is in. A panic inside the pass arrives as a `JoinError` and is
    // logged; boot never aborts on it.
    //
    // The count of sub-agent reports is therefore no longer part of this
    // function's return value: nothing downstream waits for it (the caller logs
    // it), so the task logs its own.
    let _reconcile = tokio::spawn(async move {
        let orphans = match tokio::task::spawn_blocking(
            crate::brain::tools::subagent::reconcile::reconcile_orphaned_agents,
        )
        .await
        {
            Ok(orphans) => orphans,
            Err(e) => {
                tracing::error!(
                    target: "subagent",
                    "Status reconcile did not complete, so restart-killed sub-agents keep \
                     reading as live until the next boot: {e}"
                );
                return;
            }
        };

        let mut reported = 0usize;
        for mut orphan in orphans {
            match interrupted_report_target(&orphan) {
                Some(session_id) => {
                    let msg = subagent_interrupted_message(&orphan);
                    // Clone: the parked branch still needs the message content
                    // for the durable copy below.
                    if deliver_or_park(session_id, msg.clone()) {
                        // Delivered to a live route; nothing durable to keep.
                    } else {
                        // Parked in memory only — persist it so the restart that
                        // killed the agent cannot also kill its report (#73).
                        persist_tombstone(session_id, &msg).await;
                    }
                    reported += 1;
                }
                None => {
                    // Nothing to route to. Say so rather than dropping it, since
                    // the agent's parent is waiting on a result either way.
                    tracing::error!(
                        target: "background_task",
                        "Sub-agent '{}' has an unparseable parent session '{}', its \
                         interruption cannot be reported",
                        orphan.label,
                        orphan.parent_session_id.as_deref().unwrap_or(&orphan.session_id),
                    );
                    // Still finalize the file so it cannot zombie.
                    orphan.mark_interrupted().ok();
                }
            }
        }
        if reported > 0 {
            tracing::info!(
                target: "background_task",
                "Reported {reported} interrupted sub-agent(s) from a previous run, {} waiting \
                 for a session route",
                parked_count()
            );
        }
    });

    let mut reported = redeliver_persisted_tombstones().await;

    // Then detached commands, which keep their own table. The ids it returns
    // are the index the file leg below works from — handed over here because
    // `report_interrupted` clears each row as it accounts for it, so nothing
    // could re-read them afterwards.
    let interrupted_commands = report_interrupted().await;
    reported += interrupted_commands.len();
    // Their STATUS FILES are a separate surface: `report_interrupted` accounts
    // for the DB row but never rewrites the file, so a restart-killed command
    // left a forever-`Running` file that every reader saw as live work (#111
    // follow-up, Part D). Naming the files from the row ids keeps this leg off
    // the whole status dir, which is ~25k files on a long-lived host (#848).
    // No notice here — the row path already notified; this only makes the file
    // agree with reality.
    let interrupted_ids: Vec<String> = interrupted_commands
        .iter()
        .map(|id| id.to_string())
        .collect();
    let finalized = super::work_status::WorkStatus::reconcile_stale_commands(&interrupted_ids);
    if finalized > 0 {
        tracing::info!(
            target: "background_task",
            "Boot status reconcile: finalized={finalized} stale command status file(s)"
        );
    }

    // Finally the durable notify queue (#111): pushes parked in memory by a
    // process that died before their session claimed them. Re-offered AFTER
    // the crash-recovery reports so the death announcements go first.
    let redelivered = super::notify_queue::redeliver_persisted().await;
    if redelivered > 0 {
        tracing::info!(
            target: "background_task",
            "Boot notify queue: redelivered={redelivered}"
        );
    }

    // ...and keep doing it (#931). The pass above is the only thing that
    // enforces `MAX_ROW_AGE_SECS`, and it used to run exactly once per
    // process, so a daemon that stayed up for days reaped nothing: measured
    // live, rows 6.78h past the 72h ceiling on a host up 11.59h.
    //
    // The tick runs the WHOLE pass, never the bare age reap on its own. The
    // reap is correct only as the pass's tail, where every row it sees has
    // just been offered and re-parked; on a timer it would delete pushes for
    // perfectly live sessions after any long downtime — the one outcome this
    // module forbids. `run_periodic_pass` carries the single-flight guard so a
    // slow pass cannot overlap the next tick.
    let _periodic_notify_reap = tokio::spawn(async move {
        loop {
            // One full interval before the first tick: `recover` ran a pass
            // microseconds ago, so an immediate one would only repeat it.
            tokio::time::sleep(std::time::Duration::from_secs(
                super::notify_queue::NOTIFY_REAP_INTERVAL_SECS,
            ))
            .await;
            super::notify_queue::run_periodic_pass().await;
        }
    });

    if reported > 0 {
        tracing::info!(
            target: "background_task",
            "Recovered {reported} interrupted item(s) from a previous run, {} waiting for a \
             session route",
            parked_count()
        );
    }
    match local {
        Some(local) => schedule_flush(local),
        None => tracing::info!(
            target: "background_task",
            "No local surface to flush to; parked reports wait for a channel to claim their \
             session"
        ),
    }
    reported
}

/// The tombstone repository, when a pool exists.
///
/// Resolved per call through the global pool, same as the background-task
/// repo: recovery runs before sessions exist, so nothing threads a pool
/// here. `None` before the DB is initialized (early startup, tests), which
/// simply means durable parking is skipped and the report rides the
/// in-memory queue alone.
fn tombstone_repo() -> Option<crate::db::PendingTombstoneRepository> {
    crate::db::global_pool().map(|p| crate::db::PendingTombstoneRepository::new(p.clone()))
}

/// Persist an undelivered tombstone report so no restart can lose it (#73).
///
/// Best-effort by design: a failure here is logged loudly but never blocks
/// startup, because a report that only lives in memory still beats a report
/// that panics the boot. The row is cleared the moment the report actually
/// reaches a surface (see [`clear_persisted_tombstones`]).
pub(crate) async fn persist_tombstone(session_id: Uuid, msg: &QueuedUserMessage) {
    let Some(repo) = tombstone_repo() else {
        return;
    };
    if let Err(e) = repo
        .record(
            Uuid::new_v4(),
            session_id,
            &msg.context_text,
            &msg.display_text,
        )
        .await
    {
        tracing::error!(
            target: "background_task",
            "Could not persist sub-agent tombstone for session {session_id}: it rides the \
             in-memory queue alone and the next restart will lose it: {e:#}"
        );
    }
}

/// Deliver a boot-resumed sub-agent's outcome to the session that spawned it,
/// and finalize the status file either way (#110).
///
/// Boot-resume revives an interrupted child session, but the child has no
/// channel binding: the default surface route would drop the result where
/// nobody reads it. The status file carries the spawning session, so the
/// outcome goes there instead — the same delivery the live completion path
/// uses. If no live route exists the report is durably parked, so the
/// failure direction is a duplicate report, never a lost one.
///
/// Returns whether a status file was found and finalized (informational).
pub(crate) async fn deliver_revived_agent_outcome(
    status: &mut crate::brain::agent::service::work_status::WorkStatus,
    outcome: std::result::Result<&str, &str>,
) -> bool {
    use crate::brain::agent::service::session_routes::{Delivery, deliver_to_session};

    let Some(parent) = status
        .parent_session_id
        .as_deref()
        .and_then(|p| Uuid::parse_str(p).ok())
    else {
        // Legacy file without a parent: nothing to route to, but still
        // finalize so the file cannot zombie (upstream's fix — the pre-merge
        // version returned without stamping the terminal outcome).
        tracing::warn!(
            target: "background_task",
            "Revived sub-agent '{}' has no parent session recorded; finalizing status only",
            status.id
        );
        finalize_revived_status(status, outcome);
        return false;
    };

    let agent_id = status.id.clone();
    let label = status.label.clone();
    let msg = crate::brain::tools::subagent::spawn::completion_message(&label, &agent_id, outcome);
    let delivered = matches!(
        deliver_to_session(parent, msg.clone(), true),
        Delivery::Delivered
    );
    if !delivered {
        // Parked or unroutable: hand it to the park machinery now (upstream's
        // immediate-park semantics), then persist a tombstone so a later
        // restart cannot eat the report the same restart class produced
        // (#73 semantics). Worst case the report is delivered twice, never
        // zero times.
        deliver_or_park(parent, msg.clone());
        persist_tombstone(parent, &msg).await;
    }
    match outcome {
        Ok(output) => {
            // #147: full revived report persisted, not a 512-char tail.
            if let Err(e) = status.mark_completed(output.to_string()) {
                tracing::warn!(target: "background_task", "Could not finalize revived sub-agent status '{}': {e}", agent_id);
            }
        }
        Err(error) => {
            if let Err(e) = status.mark_failed(error.to_string()) {
                tracing::warn!(target: "background_task", "Could not finalize revived sub-agent status '{}': {e}", agent_id);
            }
        }
    }
    delivered
}

/// Re-offer every persisted tombstone from a previous process (#73).
///
/// Runs before the freshly reconciled reports are delivered, so older
/// deaths are announced first. A row whose report lands on a live route is
/// cleared; one that parks again keeps its row — memory is still not
/// durable, the DB row is, so the report keeps surviving restarts until it
/// is genuinely delivered.
async fn redeliver_persisted_tombstones() -> usize {
    let Some(repo) = tombstone_repo() else {
        return 0;
    };
    let rows = match repo.all().await {
        Ok(rows) => rows,
        Err(e) => {
            tracing::error!(
                target: "background_task",
                "Could not read persisted sub-agent tombstones: {e:#}"
            );
            return 0;
        }
    };
    let mut count = 0usize;
    for row in rows {
        let msg = QueuedUserMessage {
            context_text: row.context_text.clone(),
            display_text: row.display_text.clone(),
            origin: PushOrigin::Recovery,
            bg_meta: None,
        };
        if deliver_or_park(row.session_id, msg)
            && let Err(e) = repo.clear(row.id).await
        {
            // Worst case the report is delivered twice, never zero times.
            tracing::error!(
                target: "background_task",
                "Delivered persisted tombstone {} but could not clear its row; it may be \
                 re-delivered after the next restart: {e:#}",
                row.id
            );
        }
        count += 1;
    }
    if count > 0 {
        tracing::info!(
            target: "background_task",
            "Re-offered {count} persisted sub-agent tombstone(s) from a previous run"
        );
    }
    count
}

/// Drop every persisted tombstone for a session whose reports just reached a
/// surface — the durable copies are redundant the moment delivery happens.
///
/// Best-effort and fire-and-forget: a failed clear costs a duplicate report
/// after the next restart, never a lost one. No-op outside a tokio runtime.
fn clear_persisted_tombstones(session_id: Uuid) {
    if tokio::runtime::Handle::try_current().is_err() {
        return;
    }
    if let Some(repo) = tombstone_repo() {
        tokio::spawn(async move {
            if let Err(e) = repo.clear_for_session(session_id).await {
                tracing::warn!(
                    target: "background_task",
                    "Could not clear persisted tombstones for session {session_id} after \
                     delivery: they may be re-delivered after the next restart: {e:#}"
                );
            }
        });
    }
}

/// What the parent agent is told about a sub-agent a restart interrupted.
///
/// #963: this used to assert BOTH halves of a verdict it never observed — that
/// the sub-agent was killed, and that it produced no result — the same two
/// claims #936 removed from the detached-command surface, left standing on its
/// sibling. The boot-time
/// reconciliation has observed ONE thing: the harness restarted. It did not
/// observe the sub-agent's death, and it cannot know whether the sub-agent ran,
/// how far it got, or whether its work is on disk. A sub-agent's work survives a
/// restart far more readily than a command's: its status file persists by
/// design, `resume_agent` continues it across a restart (#852), and if it wrote
/// code that code sits in a worktree and branch nothing about the restart
/// removed. Telling the parent it "produced no result" pushes it toward
/// re-spawning work that is already there.
///
/// So the report states the observation, states what is unknown, names where the
/// work may be, and hands the decision back — the same shape as the
/// detached-command report, in the same vocabulary.
pub(crate) fn subagent_interrupted_message(
    status: &super::work_status::WorkStatus,
) -> QueuedUserMessage {
    let mut context_text = format!(
        "[SUB-AGENT INTERRUPTED] OpenCrabs restarted while the sub-agent `{}` (id {}) was \
         running, so this report comes from the boot-time reconciliation — not from the \
         sub-agent. Its task was:\n\n```\n{}\n```\n\n\
         Whether it ran, how far it got and whether it finished are NOT KNOWN: the restart \
         recorded no result for it, and nothing here was checked for liveness.",
        status.label, status.id, status.task
    );
    // Its own record is the first thing a reader can look at, and naming it
    // costs one path — the reader may never have been told the id's layout.
    let record = super::work_status::status_path(&status.id);
    if record.exists() {
        context_text.push_str(&format!(
            "\n\nIts status file is at {} — it carries the last progress the sub-agent reported \
             before the restart.",
            record.display()
        ));
    }
    if let Some(line) = subagent_capture_evidence(status) {
        context_text.push_str(&format!("\n\n{line}"));
    }
    context_text.push_str(
        "\n\nIts work may have survived on disk: `resume_agent` with this id continues it, and if \
         it wrote code, the worktree and branch named when it was spawned may hold that work. \
         Decide whether to resume it or to check what it produced, based on what you were doing; \
         do not assume it succeeded or failed, and do not re-run work that may already be done.",
    );
    QueuedUserMessage {
        context_text,
        display_text: format!("⚠️ Sub-agent interrupted by restart: {}", status.label),
        origin: PushOrigin::Recovery,
        bg_meta: None,
    }
}

/// What a sub-agent's captures hold, measured from the FILES (#963).
///
/// `WorkStatus::output_out`/`output_err` are recorded PATHS, and a recorded path
/// is not a file: the capture may never have been created, or may have been
/// swept. `RunEvidence::is_empty()` answers whether a path was recorded, not
/// whether the file holds bytes, so this stats instead and says nothing about a
/// capture it could not open — a byte count about a file nobody read is a
/// measurement nobody took.
fn subagent_capture_evidence(status: &super::work_status::WorkStatus) -> Option<String> {
    let mut parts = Vec::new();
    for (name, recorded) in [
        ("stdout", status.output_out.as_deref()),
        ("stderr", status.output_err.as_deref()),
    ] {
        let Some(path) = recorded.filter(|p| !p.is_empty()) else {
            continue;
        };
        if let Ok(meta) = std::fs::metadata(path) {
            parts.push(format!("{name} {path} ({} bytes)", meta.len()));
        }
    }
    if parts.is_empty() {
        None
    } else {
        Some(format!(
            "Its captures, measured now: {} — read them there.",
            parts.join(", ")
        ))
    }
}

/// Route an interrupted sub-agent's report to the session that spawned it.
///
/// The status file carries `parent_session_id` since #110; older files (and
/// any agent spawned before the binding existed) fall back to `session_id`,
/// the pre-#26 routing value this field replaced. Returns the session to
/// report to, if either field parses as a UUID.
fn interrupted_report_target(status: &super::work_status::WorkStatus) -> Option<Uuid> {
    status
        .parent_session_id
        .as_deref()
        .and_then(|p| Uuid::parse_str(p).ok())
        .or_else(|| Uuid::parse_str(&status.session_id).ok())
}

/// Stamp the terminal outcome onto a revived agent's status file.
fn finalize_revived_status(
    status: &mut crate::brain::agent::service::work_status::WorkStatus,
    outcome: std::result::Result<&str, &str>,
) {
    match outcome {
        Ok(output) => {
            // #147: full revived report persisted, not a 512-char tail.
            if let Err(e) = status.mark_completed(output.to_string()) {
                tracing::warn!(
                    target: "background_task",
                    "Could not finalize revived sub-agent status '{}': {e}",
                    status.id
                );
            }
        }
        Err(error) => {
            if let Err(e) = status.mark_failed(error.to_string()) {
                tracing::warn!(
                    target: "background_task",
                    "Could not finalize revived sub-agent status '{}': {e}",
                    status.id
                );
            }
        }
    }
}
