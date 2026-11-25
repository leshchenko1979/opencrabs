//! Restart reports reach the session that owns the work (#1037).
//!
//! Recovery runs during startup, before any channel registers a route, so a
//! report produced then has nobody to deliver to yet. It must park rather than
//! fall back to whoever booted, leave the moment a route appears, and still be
//! delivered locally if no route ever does.

use std::sync::Arc;
use std::sync::Mutex;

use uuid::Uuid;

use crate::brain::agent::QueuedUserMessage;
use crate::brain::agent::service::MessageEnqueueCallback;
use crate::brain::agent::service::restart_recovery::{
    RESUME_CONTINUATION_BASE, deliver_or_park, flush_parked, interrupted_message, parked_count,
    referenced_subjects, resumed_turn_prompt, subagent_interrupted_message, test_guard,
};
use crate::brain::agent::service::session_routes::register_session_route;
use crate::brain::agent::service::work_status::{CommandPaths, WorkStatus, test_override};
use crate::db::BackgroundTaskRow;

/// What a recording callback saw: (session, display text) per delivery.
type Seen = Arc<Mutex<Vec<(Uuid, String)>>>;

/// A callback that records what it was handed.
fn recorder() -> (MessageEnqueueCallback, Seen) {
    let seen: Arc<Mutex<Vec<(Uuid, String)>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&seen);
    let cb: MessageEnqueueCallback = Arc::new(move |id: Uuid, msg: QueuedUserMessage| {
        if let Ok(mut v) = sink.lock() {
            v.push((id, msg.display_text));
        }
    });
    (cb, seen)
}

fn msg(text: &str) -> QueuedUserMessage {
    QueuedUserMessage {
        context_text: text.to_string(),
        display_text: text.to_string(),
        origin: crate::brain::agent::PushOrigin::Other,
        bg_meta: None,
        history_text: None,
    }
}

#[test]
fn a_report_with_no_route_parks_instead_of_going_out() {
    let _guard = test_guard();
    let session = Uuid::new_v4();

    let delivered = deliver_or_park(session, msg("interrupted"));

    assert!(!delivered, "nothing claims this session yet");
    assert_eq!(parked_count(), 1);
}

#[test]
fn registering_a_route_hands_over_what_was_parked() {
    let _guard = test_guard();
    let session = Uuid::new_v4();
    deliver_or_park(session, msg("interrupted"));

    let (cb, seen) = recorder();
    register_session_route(session, cb);

    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 1, "the channel gets what it missed");
    assert_eq!(seen[0].0, session);
    assert_eq!(seen[0].1, "interrupted");
    assert_eq!(parked_count(), 0);
}

#[test]
fn a_route_that_already_exists_receives_immediately() {
    let _guard = test_guard();
    let session = Uuid::new_v4();
    let (cb, seen) = recorder();
    register_session_route(session, cb);

    let delivered = deliver_or_park(session, msg("later report"));

    assert!(delivered);
    assert_eq!(parked_count(), 0, "nothing needed parking");
    assert_eq!(seen.lock().unwrap().len(), 1);
}

#[test]
fn one_sessions_route_does_not_drain_anothers_reports() {
    // The whole point is per-session delivery: claiming one session must not
    // hand it work belonging to a different one.
    let _guard = test_guard();
    let mine = Uuid::new_v4();
    let theirs = Uuid::new_v4();
    deliver_or_park(mine, msg("mine"));
    deliver_or_park(theirs, msg("theirs"));

    let (cb, seen) = recorder();
    register_session_route(mine, cb);

    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].1, "mine");
    assert_eq!(parked_count(), 1, "the other session's report still waits");
}

#[test]
fn the_flush_delivers_locally_rather_than_losing_the_report() {
    // A channel that never comes up in this run must not strand the report
    // forever; the local surface is the last resort, not the first choice.
    let _guard = test_guard();
    let orphan = Uuid::new_v4();
    deliver_or_park(orphan, msg("nobody claimed me"));

    let (local, seen) = recorder();
    let flushed = flush_parked(&local);

    assert_eq!(flushed, 1);
    assert_eq!(parked_count(), 0);
    let seen = seen.lock().unwrap();
    assert_eq!(seen[0].0, orphan);
    assert_eq!(seen[0].1, "nobody claimed me");
}

#[test]
fn flushing_with_nothing_parked_is_a_no_op() {
    let _guard = test_guard();
    let (local, seen) = recorder();

    assert_eq!(flush_parked(&local), 0);
    assert!(seen.lock().unwrap().is_empty());
}

// ---- #481: the prompt a boot-resumed `user` turn is framed with -------------

/// The base continuation is a user-facing contract, and the clause a hand-off
/// appends is the only place the agent is told the transcript may be lying about
/// what finished.
#[test]
fn resumed_turn_prompt_keeps_the_base_and_states_the_loss() {
    let session = Uuid::from_u128(0x0123_4567_89ab_cdef_0123_4567_89ab_cdef);
    let prompt = resumed_turn_prompt(session);

    // The base survives verbatim as a prefix. This is what stops a swap from
    // announcing itself: without it the user sees a restart notice every time.
    assert!(
        prompt.starts_with(RESUME_CONTINUATION_BASE),
        "the resumed prompt must open with the base continuation; got: {prompt}"
    );
    assert!(
        RESUME_CONTINUATION_BASE.contains("Do not mention the restart"),
        "the surface contract must stay in the base, or the restart becomes visible"
    );

    // The appended clause names the ambiguity and the check that resolves it.
    assert!(prompt.contains("did NOT finish"), "{prompt}");
    assert!(
        prompt.contains("verify from the world whether it already landed"),
        "{prompt}"
    );

    // The session id is derived rather than frozen: same id, same prompt; a
    // different id, a different prompt. Neither assertion can pass on a build
    // that dropped the substitution.
    let short = &session.simple().to_string()[..8];
    assert!(
        prompt.contains(short),
        "expected short id {short} in: {prompt}"
    );
    assert_eq!(prompt, resumed_turn_prompt(session));
    assert_ne!(prompt, resumed_turn_prompt(Uuid::from_u128(1)));
}

// ---- #913: a boot report may only claim what it measured -------------------

/// Point both the status dir and the capture dir at a throwaway temp dir.
///
/// The override is thread-local, so each test using it must stay
/// single-threaded — a plain `#[test]`, never `#[tokio::test]`.
struct TempRunDirs {
    dir: Option<tempfile::TempDir>,
}

impl TempRunDirs {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("temp dir");
        test_override::set(dir.path().to_path_buf());
        Self { dir: Some(dir) }
    }

    fn path(&self) -> &std::path::Path {
        self.dir.as_ref().expect("temp dir alive").path()
    }
}

impl Drop for TempRunDirs {
    fn drop(&mut self) {
        test_override::clear();
        // `Drop` cannot move a field out of `&mut self` (E0507).
        let _ = self.dir.take();
    }
}

fn row_for(id: Uuid) -> BackgroundTaskRow {
    BackgroundTaskRow {
        id,
        session_id: Uuid::new_v4(),
        label: "oc-deploy ship".to_string(),
        command: "oc-deploy ship --wait".to_string(),
        started_at: 0,
    }
}

/// The defect this pins (#913): a row with no pid, no exit code and no capture
/// cannot support a verdict, so the report must say UNKNOWN rather than "was
/// killed and produced no result". Acting on that false claim — "it never ran,
/// run it again" — has already come close to double-shipping a live swap.
#[test]
fn a_row_with_no_pid_exit_or_capture_reports_the_outcome_as_unknown() {
    let _dirs = TempRunDirs::new();
    let msg = interrupted_message(&row_for(Uuid::new_v4()));

    assert!(
        msg.context_text.contains("OUTCOME UNKNOWN"),
        "no evidence must read as unknown; got: {}",
        msg.context_text
    );
    assert!(
        !msg.context_text.contains("was killed"),
        "nothing measured a kill, so the report must not assert one: {}",
        msg.context_text
    );
    assert!(
        !msg.context_text.contains("produced no result"),
        "nothing measured the result, so the report must not assert its absence: {}",
        msg.context_text
    );
    // It names what is missing and what the agent can check instead.
    for needle in [
        "pid",
        "exit code",
        "captured output",
        "ps -p",
        "task_output",
    ] {
        assert!(
            msg.context_text.contains(needle),
            "the unknown report must name {needle:?}; got: {}",
            msg.context_text
        );
    }
    assert!(
        msg.display_text.contains("unknown"),
        "the surface line must not announce a kill either: {}",
        msg.display_text
    );
}

/// The other half of the contract: where a capture WAS recorded the run has
/// evidence behind it, so the verdict stays the interruption report it has
/// always been. A build that reported UNKNOWN unconditionally would pass the
/// test above and fail this one.
#[test]
fn a_row_with_a_recorded_capture_keeps_the_interrupted_verdict() {
    let dirs = TempRunDirs::new();
    let id = Uuid::new_v4();
    let capture = dirs.path().join(format!("{id}.out"));
    std::fs::write(&capture, "deployed abc1234\n").expect("write capture");

    let msg = interrupted_message(&row_for(id));

    assert!(
        !msg.context_text.contains("OUTCOME UNKNOWN"),
        "a run with output on disk is not evidence-free; got: {}",
        msg.context_text
    );
    assert!(
        msg.context_text.contains("BACKGROUND TASK INTERRUPTED"),
        "got: {}",
        msg.context_text
    );
}

/// A recorded pid is evidence too, even with no capture file — the status file
/// carries it precisely so a reader can tell live from dead across a restart.
#[test]
fn a_row_with_a_recorded_pid_is_not_unknown() {
    let _dirs = TempRunDirs::new();
    let id = Uuid::new_v4();
    WorkStatus::new_command(
        &id.to_string(),
        "sess-1",
        "oc-deploy ship",
        "oc-deploy ship --wait",
        CommandPaths {
            pid: Some(std::process::id()),
            ..CommandPaths::none()
        },
    )
    .expect("write status file");

    let msg = interrupted_message(&row_for(id));

    assert!(
        !msg.context_text.contains("OUTCOME UNKNOWN"),
        "a recorded pid is evidence; got: {}",
        msg.context_text
    );
}

// ---- #936: the row names what the command references, and claims nothing --

/// A row for a command of the test's choosing.
fn row_with(id: Uuid, command: &str) -> BackgroundTaskRow {
    BackgroundTaskRow {
        id,
        session_id: Uuid::new_v4(),
        label: "oc-ship chain wait".to_string(),
        command: command.to_string(),
        started_at: 0,
    }
}

/// The row from #936, verbatim: the command watches pid 2349610 and the row's
/// recorded pid is the WATCHER that runs the loop.
///
/// Pre-fix the report said "was killed and produced no result" — a claim about
/// work nothing had measured — and never named the pid the lane needed. That
/// lane re-derived the answer by hand four times and then started a SECOND
/// ship chain, because it could not rule out that the first was still running.
#[test]
fn an_interrupted_row_names_the_pid_it_watched_and_calls_its_own_pid_the_watcher() {
    let _dirs = TempRunDirs::new();
    let id = Uuid::new_v4();
    let command = "while kill -0 2349610; do sleep 30; done; echo \"=== CHAIN EXITED ===\"";
    WorkStatus::new_command(
        &id.to_string(),
        "sess-1",
        "oc-ship chain wait",
        command,
        CommandPaths {
            pid: Some(2_366_240),
            ..CommandPaths::none()
        },
    )
    .expect("write status file");

    let msg = interrupted_message(&row_with(id, command));

    assert!(
        msg.context_text
            .contains("command references: pid 2349610 — liveness not checked"),
        "the report must name the pid the command watched, and say it did not check it: {}",
        msg.context_text
    );
    assert!(
        msg.context_text
            .contains("row pid 2366240 is the watcher, not the work"),
        "the row's own pid is the watcher's, and a reader who confuses the two acts on \
         the wrong process: {}",
        msg.context_text
    );
    assert!(
        !msg.context_text.contains("killed"),
        "nothing observed the work dead, so the report must not say it was killed: {}",
        msg.context_text
    );
}

/// A capture holding bytes is a RESULT, and the pre-fix report called it "no
/// result" over the deploy's own output (#936). The count and the path are
/// what let a reader act without re-deriving anything.
#[test]
fn a_capture_with_bytes_is_named_by_its_count_and_path() {
    let dirs = TempRunDirs::new();
    let id = Uuid::new_v4();
    let capture = dirs.path().join(format!("{id}.out"));
    std::fs::write(&capture, "deployed abc1234\n").expect("write capture");

    let msg = interrupted_message(&row_for(id));

    assert!(
        msg.context_text.contains("holds 17 bytes"),
        "the byte count is the measurement the row cannot otherwise carry: {}",
        msg.context_text
    );
    assert!(
        !msg.context_text.contains("before the restart"),
        "a detached run may have written these bytes after the restart, so the report \
         must not date them: {}",
        msg.context_text
    );
    assert!(
        msg.context_text.contains(&capture.display().to_string()),
        "and the path is how the reader gets at them: {}",
        msg.context_text
    );
    assert!(
        !msg.context_text.contains("produced no result"),
        "bytes on disk are a result: {}",
        msg.context_text
    );
}

// ---- #963: the sub-agent surface stops asserting a death it never observed --

/// A sub-agent status for the test's choosing.
fn agent_status(id: &str, label: &str, task: &str) -> WorkStatus {
    WorkStatus::new_agent(id, label, "sess-1", task, None, None).expect("write agent status")
}

/// #963, the sibling #936 missed. The sub-agent report used to say it *"was
/// killed and produced no result"* — two claims the boot-time reconciliation
/// never observed, on a surface where the work very often DOES survive (the
/// status file persists, `resume_agent` continues it, and any code it wrote is
/// still in its worktree and branch). A parent that believes "produced no
/// result" re-spawns work that is already done.
#[test]
fn an_interrupted_subagent_does_not_claim_a_kill_or_an_empty_result() {
    let _dirs = TempRunDirs::new();
    let msg = subagent_interrupted_message(&agent_status(
        "agent-963",
        "land-894",
        "land the row-leak repair",
    ));

    assert!(
        !msg.context_text.contains("produced no result"),
        "nothing measured the result, so the report must not assert its absence: {}",
        msg.context_text
    );
    assert!(
        !msg.context_text.contains("was killed"),
        "nothing observed a kill, so the report must not assert one: {}",
        msg.context_text
    );
    // It states what IS known, and what the reader can do about it.
    for needle in ["boot-time reconciliation", "NOT KNOWN", "resume_agent"] {
        assert!(
            msg.context_text.contains(needle),
            "the interrupted report must name {needle:?}; got: {}",
            msg.context_text
        );
    }
    assert!(
        msg.context_text.contains("land the row-leak repair"),
        "the task the sub-agent was given is what the decision is made about: {}",
        msg.context_text
    );
}

/// The two arms must stay distinguishable (#963): the sub-agent arm has no
/// measurement, so it may not borrow the vocabulary of the arm that DOES — a
/// rewrite that flattened both into one "no result" sentence would pass the
/// test above while deleting the only report backed by real evidence.
#[test]
fn a_measured_empty_capture_still_says_so_on_the_command_surface() {
    let _dirs = TempRunDirs::new();
    let id = Uuid::new_v4();
    let dirs = tempfile::tempdir().expect("temp dir");
    let out = dirs.path().join(format!("{id}.out"));
    std::fs::write(&out, "").expect("write empty capture");
    WorkStatus::new_command(
        &id.to_string(),
        "sess-1",
        "oc-deploy ship",
        "oc-deploy ship --wait",
        CommandPaths {
            output_out: out.display().to_string(),
            ..CommandPaths::none()
        },
    )
    .expect("write status file");

    let msg = interrupted_message(&row_for(id));
    assert!(
        msg.context_text.contains("produced no result"),
        "a capture that was READ and found empty is a measurement, and the command \
         surface must still state it: {}",
        msg.context_text
    );
}

/// The scan itself: every shape the poll's rows used, in command order, with a
/// pid named twice recorded once. Pure string work — a probe here would run on
/// the boot path against the very processes being reported.
#[test]
fn the_command_scan_names_pids_patterns_and_units_in_order() {
    let subjects = referenced_subjects(
        "ps -p 2349610 -o pid= && pgrep -af oc-ship-chain && \
         systemd-run --user --unit=gates-651-deadbeef --wait --pipe python3 tools/gate_select.py \
         && kill -0 2349610",
    );

    assert_eq!(
        subjects,
        vec![
            "pid 2349610".to_string(),
            "pattern oc-ship-chain".to_string(),
            "unit gates-651-deadbeef".to_string(),
        ],
        "order follows the command and the repeated pid is named once"
    );
}
