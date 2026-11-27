//! #722 phase 2: the background-task manager runs a command detached and, on
//! completion, enqueues a system QueuedUserMessage into the originating session.

use crate::brain::agent::PushOrigin;
use crate::brain::agent::service::MessageEnqueueCallback;
use crate::brain::agent::service::QueuedUserMessage;
use crate::brain::agent::service::background_tasks::{
    BackgroundTaskManager, ClaudeTurnTasks, CmdResult, RunHandle, RunRequest, arm_run_notice_for,
    capture_line, claude_completion_message, claude_needs_survival_delivery, claude_task_label,
    cmd_result_from, completion_message, format_elapsed, notice_deadline, probe_group,
    run_notice_message, short_label, tail_lines, wake_message,
};
use crate::brain::agent::service::restart_recovery;
use crate::brain::agent::service::session_routes;
use crate::brain::agent::service::work_status::{GroupState, RunCause, SignalOrigin};
use crate::brain::tools::bash::handover_notice;
use std::sync::{Arc, Mutex};
use uuid::Uuid;

#[test]
fn tail_keeps_last_n_lines() {
    let text = (1..=100)
        .map(|i| i.to_string())
        .collect::<Vec<_>>()
        .join("\n");
    let tail = tail_lines(&text, 3);
    assert_eq!(tail, "98\n99\n100");
    // Fewer lines than n -> whole text.
    assert_eq!(tail_lines("a\nb", 10), "a\nb");
}

#[test]
fn completion_message_reflects_success_and_failure() {
    // #935: a completion states what its capture files hold, so the test gives
    // it real ones. `ok` writes the bytes its tail reports; `fail` leaves both
    // empty, which is the "produced nothing" case stated as a sentence.
    let dir = tempfile::tempdir().expect("tempdir");
    let ok_out = dir.path().join("ok.out");
    let ok_err = dir.path().join("ok.err");
    std::fs::write(&ok_out, "test result: ok. 5 passed\n").expect("write stdout");
    std::fs::write(&ok_err, "").expect("write stderr");
    let fail_out = dir.path().join("fail.out");
    let fail_err = dir.path().join("fail.err");
    std::fs::write(&fail_out, "error[E0001]\n").expect("write stdout");
    std::fs::write(&fail_err, "").expect("write stderr");

    let ok = completion_message(
        "11111111-2222-3333-4444-555555555555",
        "cargo test",
        "cargo test --all-features",
        &CmdResult {
            success: true,
            code: 0,
            cause: RunCause::Ordinary,
            output: "test result: ok. 5 passed".into(),
        },
        &ok_out,
        &ok_err,
        12.0,
    );
    assert!(ok.context_text.contains("exit 0 (success)"));
    assert!(ok.context_text.contains("cargo test --all-features"));
    assert!(ok.context_text.contains("Do not re-run"));
    // #752: the completion names the run, so a lane whose context was compacted
    // since the handover does not have to call `tasks_list` to find out WHICH
    // run just reported.
    assert!(
        ok.context_text
            .contains("Run id: 11111111-2222-3333-4444-555555555555"),
        "the run id must be in the context text: {}",
        ok.context_text
    );
    // #811: the display line is a headline — outcome ICON, label, duration.
    // The outcome WORD it used to carry was the same fact twice on the card.
    assert_eq!(ok.display_text, "\u{2705} cargo test \u{1f552} 12s");
    // #15: the typed receipt payload rides along for the echo card.
    let meta = ok.bg_meta.expect("bg completion carries BgTaskMeta");
    assert!(meta.success);
    assert_eq!(meta.label, "cargo test");
    assert_eq!(meta.elapsed_secs, 12.0);
    assert_eq!(meta.tail, "test result: ok. 5 passed");

    let fail = completion_message(
        "66666666-7777-8888-9999-000000000000",
        "build",
        "cargo build",
        &CmdResult {
            success: false,
            code: 101,
            cause: RunCause::Ordinary,
            output: "error[E0001]".into(),
        },
        &fail_out,
        &fail_err,
        3.0,
    );
    assert!(fail.context_text.contains("exit 101 (failure)"));
    assert!(
        fail.context_text
            .contains("Run id: 66666666-7777-8888-9999-000000000000"),
        "a FAILED run names itself too — that is when the id matters most: {}",
        fail.context_text
    );
    assert_eq!(fail.display_text, "\u{274c} build \u{1f552} 3s");
    let meta = fail.bg_meta.expect("failed completion still carries meta");
    assert!(!meta.success);
    assert_eq!(meta.elapsed_secs, 3.0);
}

/// #786 — the wake message is the surface a lane reads first, so it must not
/// report a ceiling kill as a failure of the command. `137` is produced by
/// three different events and the message has to say which one happened.
#[test]
fn completion_message_names_why_a_137_ended() {
    // #935: two empty capture FILES, so the body states "no output on either
    // stream" rather than rendering nothing at all.
    let dir = tempfile::tempdir().expect("tempdir");
    let out = dir.path().join("killed.out");
    let err = dir.path().join("killed.err");
    std::fs::write(&out, "").expect("write stdout");
    std::fs::write(&err, "").expect("write stderr");
    let make = |cause: RunCause, success: bool| {
        completion_message(
            "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee",
            "long build",
            "make -j8",
            &CmdResult {
                success,
                code: 137,
                cause,
                output: String::new(),
            },
            &out,
            &err,
            3600.0,
        )
    };

    let ceiling = make(RunCause::Ceiling { limit_secs: 3600 }, false);
    let oom = make(
        RunCause::Signalled {
            signal: 9,
            origin: SignalOrigin::Unknown,
            group: GroupState::Unknown,
        },
        false,
    );
    let literal = make(RunCause::Ordinary, false);

    // The ceiling kill: the one case where the command did nothing wrong.
    assert!(
        ceiling.context_text.contains("ceiling 3600s"),
        "a ceiling kill must name the ceiling: {}",
        ceiling.context_text
    );
    assert!(
        !ceiling.context_text.contains("exit 137 (failure)"),
        "a ceiling kill is not a command failure: {}",
        ceiling.context_text
    );
    assert!(ceiling.context_text.contains("did not fail"));
    // #811: the ICON is the only outcome signal on the headline, so a
    // ceiling kill and a command failure read alike there — the distinction
    // that matters lives in the context text above and on the card body's
    // first line (#808 cue 9). Asserting a word here would re-add it.
    assert_eq!(ceiling.display_text, "\u{274c} long build \u{1f552} 1h 0m");

    // A signal is a kill too — and #966 left the Unknown arm rendering #786's
    // text byte-for-byte, because this record does not say who sent it. The
    // signal number is still what tells an OOM kill (9) from a cancellation
    // (15).
    assert!(
        oom.context_text.contains("signal 9 (killed)"),
        "got: {}",
        oom.context_text
    );
    assert!(!oom.context_text.contains("ceiling"));

    // And a command that simply exited 137 still reads as a failure.
    assert!(literal.context_text.contains("exit 137 (failure)"));
    assert_eq!(literal.display_text, ceiling.display_text);

    // All three are different strings — the whole point of the field.
    assert_ne!(ceiling.context_text, oom.context_text);
    assert_ne!(ceiling.context_text, literal.context_text);
    assert_ne!(oom.context_text, literal.context_text);
}

#[test]
fn format_elapsed_buckets_match_the_spec() {
    assert_eq!(format_elapsed(0.4), "0s");
    assert_eq!(format_elapsed(42.0), "42s");
    assert_eq!(format_elapsed(59.6), "1m 0s"); // rounds up into the minute bucket
    assert_eq!(format_elapsed(185.0), "3m 5s");
    assert_eq!(format_elapsed(3599.4), "59m 59s");
    assert_eq!(format_elapsed(3720.0), "1h 2m");
}

#[test]
fn short_label_takes_the_command_after_cd() {
    assert_eq!(short_label("cd ~/proj && cargo test"), "cargo test");
    assert_eq!(short_label("cargo build"), "cargo build");
}

#[tokio::test]
// The test_guard serializes suites touching the process-global parked-queue
// state; holding it across the polling `.await`s below is the entire point —
// same shape as the session_notify suite (#22).
#[allow(clippy::await_holding_lock)]
async fn spawn_command_enqueues_on_completion() {
    // register_session_route → claim_session touches the process-global
    // parked-queue state, so serialize against other suites that do too.
    let _guard = restart_recovery::test_guard();
    #[allow(clippy::type_complexity)]
    let recorded: Arc<Mutex<Vec<(Uuid, QueuedUserMessage)>>> = Arc::new(Mutex::new(Vec::new()));
    let rec = recorded.clone();
    let enqueue: MessageEnqueueCallback = Arc::new(move |sid, msg| {
        rec.lock().unwrap().push((sid, msg));
    });

    let mgr = Arc::new(BackgroundTaskManager::new());
    let sid = Uuid::new_v4();
    // The manager no longer carries its own route (fork #19 — delivery goes
    // through the one gated route, which resolves the session's registered
    // route), so claim the session the way a channel would: register the
    // recording callback as its route.
    session_routes::register_session_route(sid, enqueue);
    let cwd = std::env::temp_dir();

    mgr.clone().spawn_command(RunRequest::new(
        sid,
        cwd,
        "echo probe".to_string(),
        "echo BG_DONE_MARKER".to_string(),
    ));

    // Wait (bounded) for the detached command to finish and enqueue.
    let mut waited = 0;
    while recorded.lock().unwrap().is_empty() && waited < 50 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        waited += 1;
    }

    let r = recorded.lock().unwrap();
    assert_eq!(r.len(), 1, "completion should have enqueued exactly once");
    assert_eq!(r[0].0, sid);
    assert!(r[0].1.context_text.contains("BG_DONE_MARKER"));
    assert!(r[0].1.context_text.contains("exit 0 (success)"));
    // Running count drops back to zero after completion.
    assert_eq!(mgr.running_for(sid), 0);
}

/// #692 gate 36516212395: a fire-and-forget spawn must reserve its roster row
/// on the CALLER's thread, before the call returns.
///
/// `spawn_command` is called by paths that read the roster synchronously right
/// afterwards — `build_goal_evidence` does, and so does `tasks_list`. A
/// reservation taken inside the spawned task is invisible to that read until
/// the task is first polled, so the run would exist but be unaddressable by the
/// id, the output paths and the cancel surface this change adds.
///
/// The read below is deliberately synchronous and is the assertion: adding an
/// `.await` between the call and the read would let the spawned task run and
/// hide exactly the regression this pins. That regression cost a CI gate once,
/// so the pin is worth more than the shape of the test looks.
#[tokio::test]
async fn spawn_command_reserves_the_run_before_it_returns() {
    let mgr = Arc::new(BackgroundTaskManager::new());
    let sid = Uuid::new_v4();

    mgr.clone().spawn_command(RunRequest::new(
        sid,
        std::env::temp_dir(),
        "reserve probe".to_string(),
        "sleep 5".to_string(),
    ));

    // No await between the call above and this read.
    let live = mgr.handles_for(sid);
    assert_eq!(
        live.len(),
        1,
        "the run must be addressable the instant spawn_command returns, got: {live:?}"
    );
    assert_eq!(live[0].label, "reserve probe");

    // One id addresses the whole run: it is the status-file stem and the stem of
    // both captured streams, which is what `tasks_list` prints and what the
    // cancel surface takes.
    assert!(
        live[0].output_out.to_string_lossy().contains(&live[0].id),
        "the run's stdout path must carry its id, got: {:?}",
        live[0].output_out
    );
    assert!(
        live[0].output_err.to_string_lossy().contains(&live[0].id),
        "the run's stderr path must carry its id, got: {:?}",
        live[0].output_err
    );

    // Reservation is two-phase by design: the row is taken before the child
    // exists, and the pid is back-filled by the task's first poll. A cancel in
    // this window refuses ("no recorded pid yet") rather than reporting a kill
    // it did not make. The task has not been polled here — no await above — so
    // the pid must still be absent.
    assert!(
        live[0].pid.is_none(),
        "a run reserved before its child exists carries no pid yet, got: {:?}",
        live[0].pid
    );
}

/// #692: a `wake_on_output` match must not read like a result.
///
/// The agent asked to be told when a line appeared, NOT when the run ended. A
/// message that could be mistaken for a completion would stop it watching the
/// thing it asked about — the run is still going, and the two verbs that
/// continue watching it have to be named.
#[test]
fn wake_message_says_the_run_is_still_going() {
    let msg = wake_message(
        "cargo build",
        "run-42",
        "the first error line",
        &["error[E0308]: mismatched types".to_string()],
        false,
    );
    assert!(
        msg.context_text.contains("the first error line"),
        "the caller's reason must come back verbatim, got: {}",
        msg.context_text
    );
    assert!(
        msg.context_text.contains("STILL RUNNING"),
        "a wake must not read as a completion, got: {}",
        msg.context_text
    );
    assert!(
        msg.context_text.contains("error[E0308]"),
        "the matching line is the point of the wake, got: {}",
        msg.context_text
    );
    // The verbs that continue watching, so the agent does not have to re-derive
    // them or (worse) re-run the command.
    assert!(msg.context_text.contains("task_output"));
    assert!(msg.context_text.contains("task_wait"));
    assert!(
        msg.context_text.contains("Do not re-run"),
        "a wake must not invite a re-run of a command that is still running"
    );
    // stderr vs stdout is reported, because the two mean different things.
    let err = wake_message("b", "run-43", "r", &["boom".to_string()], true);
    assert!(err.context_text.contains("Matched on stderr"));
}

/// #692 D7: the spawn normalisation reaches the command line, and a caller's
/// own value still wins.
///
/// These four are a PROMISE to the model — no run will ever block on a pager or
/// on a credential prompt it cannot answer — and a promise with no reader is a
/// comment. `apply_context_env` is `pub(crate)` for this test alone.
///
/// `GIT_TERMINAL_PROMPT=0` is the one that changes behaviour rather than
/// formatting: without it a git command on a pty prompts for a username the
/// harness will never supply, and the run burns its whole deadline at the
/// prompt instead of failing in a second with git's own message.
#[test]
fn apply_context_env_sets_the_noninteractive_defaults() {
    use crate::brain::tools::ToolExecutionContext;
    use crate::brain::tools::bash::apply_context_env;
    use std::ffi::OsStr;
    use tokio::process::Command;

    let mut ctx = ToolExecutionContext::new(Uuid::new_v4());
    let mut cmd = Command::new("true");
    apply_context_env(&mut cmd, &ctx);

    let envs: Vec<(String, Option<String>)> = cmd
        .as_std()
        .get_envs()
        .map(|(k, v)| {
            (
                k.to_string_lossy().into_owned(),
                v.map(|v| v.to_string_lossy().into_owned()),
            )
        })
        .collect();

    for key in ["PAGER", "GIT_PAGER", "SYSTEMD_PAGER", "GIT_TERMINAL_PROMPT"] {
        assert!(
            envs.iter().any(|(k, _)| k == key),
            "{key} must be set on every spawn, got {envs:?}"
        );
    }
    assert!(
        envs.iter()
            .any(|(k, v)| k == "GIT_TERMINAL_PROMPT" && v.as_deref() == Some("0")),
        "GIT_TERMINAL_PROMPT must be 0, got {envs:?}"
    );

    // A caller that deliberately sets one of these still wins: normalisation is
    // a default, not a lock. Pinned because the ordering inside
    // `apply_context_env` is what makes it true, and an `env_vars` loop moved
    // above the four defaults would silently override every caller.
    ctx.env_vars.insert("PAGER".to_string(), "less".to_string());
    let mut cmd2 = Command::new("true");
    apply_context_env(&mut cmd2, &ctx);
    assert!(
        cmd2.as_std()
            .get_envs()
            .any(|(k, v)| k == OsStr::new("PAGER") && v == Some(OsStr::new("less"))),
        "a caller's own PAGER must survive the defaults"
    );
}

/// #1748: `spawn_command_with_hook` hands the outcome to the hook INSTEAD of
/// the generic session delivery; a hook that replaces the process on success
/// would orphan an in-memory enqueue mid-flight. Same lifecycle (timer, status
/// file, DB accounting), different completion route.
#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn spawn_command_with_hook_replaces_generic_delivery() {
    // register_session_route → claim_session touches the process-global
    // parked-queue state, so serialize against other suites that do too.
    let _guard = restart_recovery::test_guard();
    #[allow(clippy::type_complexity)]
    let recorded: Arc<Mutex<Vec<(Uuid, QueuedUserMessage)>>> = Arc::new(Mutex::new(Vec::new()));
    let rec = recorded.clone();
    let enqueue: MessageEnqueueCallback = Arc::new(move |sid, msg| {
        rec.lock().unwrap().push((sid, msg));
    });

    let mgr = Arc::new(BackgroundTaskManager::new());
    let sid = Uuid::new_v4();
    session_routes::register_session_route(sid, enqueue);

    // The hook records what it learned; the session route must stay silent.
    let hooked: Arc<Mutex<Option<(bool, bool)>>> = Arc::new(Mutex::new(None));
    let hk = hooked.clone();
    let hook: crate::brain::agent::service::background_tasks::CompletionHook = Box::new(
        move |ctx: crate::brain::agent::service::background_tasks::HookContext| {
            Box::pin(async move {
                *hk.lock().unwrap() = Some((ctx.result.success, ctx.elapsed_secs >= 0.0));
            })
        },
    );

    mgr.clone().spawn_command_with_hook(
        RunRequest::new(
            sid,
            std::env::temp_dir(),
            "hook probe".to_string(),
            "echo HOOK_DONE_MARKER".to_string(),
        ),
        hook,
    );

    // Wait (bounded) for the detached command to finish and fire the hook.
    let mut waited = 0;
    while hooked.lock().unwrap().is_none() && waited < 50 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        waited += 1;
    }

    let h = hooked.lock().unwrap();
    let (success, elapsed_ok) = h.expect("hook must fire on completion");
    assert!(success, "echo must report success to the hook");
    assert!(elapsed_ok, "hook carries the command runtime");

    // The generic delivery did NOT run: the session route got nothing.
    assert!(
        recorded.lock().unwrap().is_empty(),
        "a hook replaces generic session delivery"
    );
    // Running count drops back to zero after completion.
    assert_eq!(mgr.running_for(sid), 0);
}

// --- #1776 seam 2: mirrored claude-cli task lifecycle ---

#[test]
fn claude_task_label_carries_source_tag() {
    assert_eq!(claude_task_label("btglunt18"), "claude-cli btglunt18");
}

#[test]
fn mirror_registers_then_removes() {
    let mgr = Arc::new(BackgroundTaskManager::new());
    let sid = Uuid::new_v4();
    let label = claude_task_label("btglunt18");

    // Before start: nothing running for the session.
    assert_eq!(mgr.running_for(sid), 0);

    mgr.mirror_started(sid, &label);
    assert_eq!(mgr.running_for(sid), 1);
    let rows = mgr.running_tasks(sid);
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].label, "claude-cli btglunt18",
        "row carries the source tag"
    );

    // The completed notification removes the shadow row.
    mgr.mirror_finished(sid, &label);
    assert_eq!(mgr.running_for(sid), 0);
    assert!(mgr.running_tasks(sid).is_empty());
}

#[test]
fn mirror_finish_is_noop_for_unknown_task() {
    let mgr = Arc::new(BackgroundTaskManager::new());
    let sid = Uuid::new_v4();

    // A notification for a task we never saw started (mid-session attach):
    // must not panic and must not conjure a row.
    mgr.mirror_finished(sid, &claude_task_label("ghost01"));
    assert_eq!(mgr.running_for(sid), 0);
}

#[test]
fn mirror_finish_removes_oldest_on_duplicate_labels() {
    let mgr = Arc::new(BackgroundTaskManager::new());
    let sid = Uuid::new_v4();
    let label = claude_task_label("btglunt18");

    // Two tasks with the same id-shaped label: the manager cannot tell them
    // apart (same rule as spawned commands with identical labels), so the
    // first finish drops the OLDEST row and the survivor stays visible.
    mgr.mirror_started(sid, &label);
    std::thread::sleep(std::time::Duration::from_millis(5));
    mgr.mirror_started(sid, &label);
    assert_eq!(mgr.running_for(sid), 2);

    mgr.mirror_finished(sid, &label);
    assert_eq!(mgr.running_for(sid), 1, "oldest row removed, survivor kept");

    mgr.mirror_finished(sid, &label);
    assert_eq!(mgr.running_for(sid), 0, "second finish clears the survivor");
}

#[test]
fn survivor_delivery_fires_only_for_tasks_not_started_this_turn() {
    let sid = Uuid::new_v4();
    let other = Uuid::new_v4();
    let mut turn_started = ClaudeTurnTasks::default();

    // Empty marker set: every notification is a post-exit survivor.
    assert!(claude_needs_survival_delivery(
        &turn_started,
        sid,
        "bAAA1111"
    ));

    // Started THIS turn: silent, claude sees the notification natively.
    turn_started.insert((sid, "bAAA1111".to_string()));
    assert!(!claude_needs_survival_delivery(
        &turn_started,
        sid,
        "bAAA1111"
    ));

    // Session isolation: the same task id under another session, or another
    // task under this session, is still a survivor.
    turn_started.insert((other, "bAAA1111".to_string()));
    turn_started.insert((sid, "bBBB2222".to_string()));
    assert!(claude_needs_survival_delivery(
        &turn_started,
        other,
        "bCCC3333"
    ));
    assert!(claude_needs_survival_delivery(
        &turn_started,
        sid,
        "bDDD4444"
    ));
}

#[test]
fn claude_completion_message_carries_status_and_summary_mechanically() {
    let msg = claude_completion_message("bAAA1111", "completed", Some("tests passed"));
    assert!(
        msg.context_text.contains("claude-cli bAAA1111"),
        "label in context"
    );
    assert!(msg.context_text.contains("Status: completed"));
    assert!(msg.context_text.contains("Summary: tests passed"));
    assert!(msg.context_text.contains("Do not re-run the task"));
    assert_eq!(msg.display_text, "\u{2705} claude-cli bAAA1111");
    assert_eq!(msg.origin, PushOrigin::BackgroundTask, "#1221 echo tag");
    assert!(msg.bg_meta.is_none(), "no CmdResult, no fabricated receipt");
}

#[test]
fn claude_completion_message_failed_wording_and_summary_fallback() {
    let msg = claude_completion_message("bBBB2222", "failed", None);
    assert!(msg.context_text.contains("Status: failed"));
    assert!(msg.context_text.contains("Summary: (no summary provided)"));
    assert_eq!(msg.display_text, "\u{274c} claude-cli bBBB2222");
}
// --- #877: REGISTERED is not DETACHED ---

/// A run that is merely registered (reserved before its grace wait) must not
/// reach the footer's `N ⏏️` segment: nothing has left the turn, so no roll row
/// can carry the icon either. The roster keeps answering "what is running" —
/// that question is unchanged — but the detach count answers a narrower one.
#[test]
fn a_registered_run_is_not_counted_as_detached() {
    let mgr = Arc::new(BackgroundTaskManager::new());
    let sid = Uuid::new_v4();
    mgr.started_run(RunHandle {
        id: "run-877-inline".to_string(),
        session_id: sid,
        label: "sleep 30".to_string(),
        command: "sleep 30".to_string(),
        cwd: std::env::temp_dir(),
        started: std::time::Instant::now(),
        detached: false,
        pid: None,
        output_out: std::path::PathBuf::new(),
        output_err: std::path::PathBuf::new(),
    });

    assert_eq!(
        mgr.running_tasks(sid).len(),
        1,
        "the roster still answers what is registered"
    );
    assert_eq!(
        mgr.detached_count_for(sid),
        0,
        "a live inline command is not a detached task"
    );

    // The handover flips the SAME row: no second registration, no new id.
    mgr.mark_detached("run-877-inline");
    assert_eq!(mgr.detached_count_for(sid), 1);
    assert_eq!(mgr.running_tasks(sid).len(), 1);

    // Finishing drops it from both.
    mgr.finish_run("run-877-inline");
    assert_eq!(mgr.detached_count_for(sid), 0);
    assert!(mgr.running_tasks(sid).is_empty());
}

/// A mirrored external task is background work by definition, so it counts —
/// and it carries the label the roll draws its `⏏️ <label>` row from. Both
/// reads are session-scoped.
#[test]
fn a_mirrored_task_counts_as_detached_and_has_a_label() {
    let mgr = Arc::new(BackgroundTaskManager::new());
    let sid = Uuid::new_v4();
    let other = Uuid::new_v4();
    let label = claude_task_label("btglunt18");

    mgr.mirror_started(sid, &label);
    mgr.mirror_started(other, "other session task");

    assert_eq!(mgr.detached_count_for(sid), 1);
    assert_eq!(mgr.mirror_labels_for(sid), vec![label.clone()]);
    assert_eq!(
        mgr.mirror_labels_for(other),
        vec!["other session task".to_string()],
        "labels are session-scoped"
    );
    assert!(
        mgr.mirror_labels_for(Uuid::new_v4()).is_empty(),
        "a session with no mirror has no rows"
    );

    // The completion notification removes the row, and the label with it, so a
    // counted run can never outlive the row it points at.
    mgr.mirror_finished(sid, &label);
    assert_eq!(mgr.detached_count_for(sid), 0);
    assert!(mgr.mirror_labels_for(sid).is_empty());
}

// --- #935: an empty tail is a FACT, not an absence ---

/// The completion notice carries an output tail, and an empty tail used to
/// render as nothing at all — so a lane could not tell "the command produced
/// nothing", "output exists but has not been flushed to the capture file yet"
/// (Python block-buffers to a file) and "the run died before flushing" apart.
/// Byte counts are what discriminate them, so the line states both counts and
/// both paths, verbatim.
#[test]
fn capture_line_states_both_byte_counts_and_both_paths() {
    let dir = tempfile::tempdir().expect("tempdir");
    let out = dir.path().join("run.out");
    let err = dir.path().join("run.err");
    std::fs::write(&out, "hello!\n").expect("write stdout"); // 7 bytes
    std::fs::write(&err, "oops\n").expect("write stderr"); // 5 bytes

    assert_eq!(
        capture_line(&out, &err, false, "cargo test --lib"),
        format!(
            "output: 7 bytes stdout \u{00b7} 5 bytes stderr \u{00b7} captures: {} {}",
            out.display(),
            err.display()
        ),
        "the line must state both counts and both paths, in that shape"
    );
}

/// #959 (second half): a byte count is a claim about WHOSE output it counts.
///
/// A lane read `19 bytes` as "the command produced 19 bytes" while the command
/// had streamed 1,671 bytes across 40 filenames and `head` capped the capture.
/// The number was right and the sentence was wrong. When the command ends in a
/// stage that truncates its input, the line says so.
#[test]
fn capture_line_names_the_stage_that_capped_the_stream() {
    let dir = tempfile::tempdir().expect("tempdir");
    let out = dir.path().join("capped.out");
    let err = dir.path().join("capped.err");
    std::fs::write(&out, "file-one\nfile-two\n").expect("write stdout"); // 19 bytes
    std::fs::write(&err, "").expect("write stderr");

    let capped = capture_line(&out, &err, false, "find . -name '*.rs' | head -20");
    assert!(
        capped.contains("pipeline ends in `head`"),
        "a capped capture must name the stage that capped it, got: {capped}"
    );
    assert!(
        capped.contains("not the full stream"),
        "the clause must say what the count is NOT, got: {capped}"
    );

    // The same capture from a command with no pipeline is the stream's own
    // total, and must not borrow the clause.
    let uncapped = capture_line(&out, &err, false, "cargo test --lib");
    assert!(
        !uncapped.contains("pipeline ends in"),
        "a command with no pipe has no capping stage to name, got: {uncapped}"
    );
    assert_eq!(
        capped.lines().count(),
        1,
        "the clause rides on the same line, got: {capped}"
    );

    // A stage outside the truncating vocabulary is not claimed: `| tee log`
    // passes the whole stream through, so naming it would be the same
    // overstatement the fix exists to remove.
    let pass_through = capture_line(&out, &err, false, "cargo test --lib | tee /tmp/log");
    assert!(
        !pass_through.contains("pipeline ends in"),
        "a pass-through stage did not cap anything, got: {pass_through}"
    );
}

/// Two files that exist and hold nothing is the one case where "no output" is
/// a measurement rather than a guess — so it gets its own sentence instead of
/// a zero-byte count that reads like a measurement of nothing.
#[test]
fn capture_line_says_no_output_when_both_captures_are_empty() {
    let dir = tempfile::tempdir().expect("tempdir");
    let out = dir.path().join("run.out");
    let err = dir.path().join("run.err");
    std::fs::write(&out, "").expect("write stdout");
    std::fs::write(&err, "").expect("write stderr");

    assert_eq!(
        capture_line(&out, &err, false, "cargo test --lib"),
        format!(
            "no output on either stream (0 bytes) \u{00b7} captures: {} {}",
            out.display(),
            err.display()
        ),
        "both captures present and empty is stated as a sentence"
    );
}

/// A capture path that does not exist is NOT an empty capture: the run may
/// never have created it, or it may have been swept. Rendering it as `0 bytes`
/// would state a measurement nobody took, so it says `capture missing`.
#[test]
fn capture_line_marks_a_missing_capture_instead_of_calling_it_empty() {
    let dir = tempfile::tempdir().expect("tempdir");
    let out = dir.path().join("run.out");
    std::fs::write(&out, "line\n").expect("write stdout"); // 5 bytes
    // Never created.
    let err = dir.path().join("never-created.err");

    let line = capture_line(&out, &err, false, "cargo test --lib");
    assert!(
        line.contains("capture missing"),
        "an absent capture must be named as absent, got: {line}"
    );
    assert!(
        !line.contains("no output on either stream"),
        "a missing capture is not the same fact as a silent run, got: {line}"
    );
    assert!(
        line.contains(&out.display().to_string()),
        "the stream that DOES exist is still named by path, got: {line}"
    );
    assert!(
        line.contains("output: 5 bytes stdout"),
        "the present stream's count is still reported, got: {line}"
    );
}

/// The whole point of #935: three completions that used to be one blank body
/// must be three different bodies. (a) output was produced, (b) both captures
/// exist and are empty, (c) the captures are not there at all.
#[test]
fn completion_bodies_are_pairwise_distinguishable() {
    let dir = tempfile::tempdir().expect("tempdir");
    let make = |name: &str, output: &str, body: Option<&str>| {
        let out = dir.path().join(format!("{name}.out"));
        let err = dir.path().join(format!("{name}.err"));
        if let Some(body) = body {
            std::fs::write(&out, body).expect("write stdout");
            std::fs::write(&err, "").expect("write stderr");
        }
        completion_message(
            "93500000-0000-0000-0000-000000000000",
            "python3 gen.py",
            "python3 gen.py",
            &CmdResult {
                success: true,
                code: 0,
                cause: RunCause::Ordinary,
                output: output.to_string(),
            },
            &out,
            &err,
            1.0,
        )
        .context_text
    };

    let body = "test result: ok. 5 passed\n";
    let with_output = make("a", "test result: ok. 5 passed", Some(body));
    let empty = make("b", "", Some(""));
    // Never created: no `Some(..)`, so neither file exists.
    let missing = make("c", "", None);

    assert!(
        with_output.contains(&format!("output: {} bytes stdout", body.len())),
        "a run that produced output states how much, got: {with_output}"
    );
    assert!(
        with_output.contains("Last 50 lines of output:\ntest result: ok. 5 passed"),
        "the tail still rides along as evidence, got: {with_output}"
    );

    assert!(
        empty.contains("no output on either stream (0 bytes)"),
        "a run whose captures exist and are empty says so, got: {empty}"
    );
    assert!(
        !empty.contains("Last 50 lines of output:"),
        "an empty tail must not leave a dangling header, got: {empty}"
    );

    assert!(
        missing.contains("capture missing"),
        "a run whose captures are absent says that instead, got: {missing}"
    );

    assert_ne!(with_output, empty, "(a) and (b) must not read alike");
    assert_ne!(with_output, missing, "(a) and (c) must not read alike");
    assert_ne!(empty, missing, "(b) and (c) must not read alike");
}

/// #959: the same empty pair of captures means two different things depending
/// on WHEN the line is rendered, so the two surfaces must not print the same
/// sentence.
///
/// `completion_message` renders after the run has ended — there, "no output on
/// either stream" is a verdict the writer is entitled to. `handover_notice`
/// renders while the run is still going, and the same words there read as a
/// post-mortem: run `5f045946` was handed over at 0 bytes and finished with
/// 242,401 bytes at 333 s, and the lane that read the notice as a death
/// abandoned its scan and launched a duplicate.
///
/// This asserts the two states DIFFER for one identical pair of empty files.
/// If `live` stops being consulted, the first assertion fails.
#[test]
fn capture_line_distinguishes_a_live_run_from_a_finished_one() {
    let dir = tempfile::tempdir().expect("tempdir");
    let out = dir.path().join("same.out");
    let err = dir.path().join("same.err");
    std::fs::write(&out, "").expect("write stdout");
    std::fs::write(&err, "").expect("write stderr");

    let live = capture_line(&out, &err, true, "cargo test --lib");
    let finished = capture_line(&out, &err, false, "cargo test --lib");

    assert!(
        live.contains("no output yet"),
        "a run that is still going has produced nothing YET, got: {live}"
    );
    assert!(
        !live.contains("no output on either stream"),
        "a live run must not be described in the past tense, got: {live}"
    );
    assert!(
        finished.contains("no output on either stream (0 bytes)"),
        "a finished run with empty captures still states the verdict, got: {finished}"
    );
    assert!(
        !finished.contains("no output yet"),
        "a finished run has no 'yet' left to give, got: {finished}"
    );
    assert_ne!(
        live, finished,
        "the same empty pair must not read the same on a live and a terminal surface"
    );
}

/// The handover notice is the FIRST thing a caller reads about a detached run.
/// It used to omit the capture section entirely when both tails were empty, so
/// "this run has printed nothing yet" was indistinguishable from a notice that
/// forgot to mention output. It now states the emptiness, and keeps the tails
/// as evidence when there are any.
#[test]
fn handover_states_empty_captures_instead_of_omitting_them() {
    let dir = tempfile::tempdir().expect("tempdir");
    let out = dir.path().join("handed.out");
    let err = dir.path().join("handed.err");
    std::fs::write(&out, "").expect("write stdout");
    std::fs::write(&err, "").expect("write stderr");

    let notice = handover_notice(
        "sleep 900",
        "sleep 900",
        "after 600s",
        "run-935",
        Some(4242),
        &out,
        &err,
    );
    assert!(
        notice.contains("no output yet (0 bytes so far)"),
        "an empty handover must SAY it is empty SO FAR, got: {notice}"
    );
    assert!(
        !notice.contains("no output on either stream"),
        "a handover is a snapshot of a live run, not a post-mortem (#959), got: {notice}"
    );
    assert!(
        !notice.contains("stdout so far"),
        "an empty capture has no tail to show, got: {notice}"
    );
    assert!(
        notice.contains(&out.display().to_string()) && notice.contains(&err.display().to_string()),
        "both capture paths are still named so the caller can look for itself, got: {notice}"
    );

    // With bytes on the stream, the tail is still carried — the statement did
    // not replace the evidence, it precedes it.
    std::fs::write(&out, "step 1\n").expect("write stdout"); // 7 bytes
    let notice = handover_notice(
        "sleep 900",
        "sleep 900",
        "after 600s",
        "run-935",
        Some(4242),
        &out,
        &err,
    );
    assert!(
        notice.contains("output: 7 bytes stdout"),
        "a live capture is counted at render time, got: {notice}"
    );
    assert!(
        notice.contains("stdout so far (last 15 lines):\nstep 1"),
        "the tail is still shown when it has something to say, got: {notice}"
    );
}

/// #966 — the one production construction site stamps the origin, and the
/// origin it stamps is the only one this process can substantiate: a signal
/// that reaches `cmd_result_from` was sent by something outside the harness.
/// Since #1056 that is stronger than when the claim was written — the ceiling
/// that used to signal a run's group is gone, so the harness has no sender to
/// one at all.
///
/// A real child signals itself, so this exercises the whole path — spawn,
/// wait, read the wait status — rather than a hand-built `ExitStatus`, which
/// the std API does not let a test construct portably.
#[cfg(unix)]
#[test]
fn a_signalled_run_is_stamped_external_at_the_one_construction_site() {
    // SIGKILL cannot be caught or ignored, so the wait status is guaranteed to
    // carry a signal and no exit code.
    let output = std::process::Command::new("sh")
        .arg("-c")
        .arg("kill -KILL $$")
        .output()
        .expect("spawn sh");
    assert!(
        output.status.code().is_none(),
        "expected a signal death, got an exit code: {:?}",
        output.status
    );

    // `None`: no detached run's pgid is in scope here, so the probe is not
    // taken and the cause keeps `GroupState::Unknown` (#977). The origin
    // assertion below is what this test pins; the group probe has its own.
    let result = cmd_result_from(&output, None);
    assert_eq!(
        result.cause,
        RunCause::Signalled {
            signal: 9,
            origin: SignalOrigin::External,
            group: GroupState::Unknown,
        },
        "a signal this process did not send must be stamped External, not guessed"
    );
    // The reader's text, end to end: the defect #966 filed was that this read
    // like a crash.
    assert!(
        result
            .cause
            .describe(result.code)
            .contains("signalled by external"),
        "got: {}",
        result.cause.describe(result.code)
    );
}

/// #977 — the probe must find what is actually IN a group, not just echo the
/// number it was handed. This process's own group holds at least this test's
/// process, so the live path runs against a group observable from the inside.
/// `getpgrp()` rather than `std::process::id()`: our pid is our pgid only when
/// we happen to be a group leader, which a test thread cannot assume.
///
/// Gated to Linux: the counting probe is the `/proc` walker. The
/// `kill(-pgid, 0)` platforms answer `Survived` with `procs: None`, which is
/// a different assertion and lives on a platform these tests do not run on.
///
/// If the probe were removed this fails on the `match` — `probe_group` would
/// no longer exist for the test to call, and the rendering tests would fail
/// on their `assert_ne!` negative controls.
#[cfg(target_os = "linux")]
#[test]
fn probing_our_own_group_finds_its_members() {
    // SAFETY: getpgrp(2) reports the caller's own process group; no state is
    // changed.
    let pgid = unsafe { libc::getpgrp() } as u32;

    match probe_group(Some(pgid)) {
        GroupState::Survived {
            pgid: found,
            procs: Some(count),
        } => {
            assert_eq!(
                found, pgid,
                "the probe must report the group it actually looked at"
            );
            assert!(
                count >= 1,
                "our own live group holds at least this test's process, got {count}"
            );
        }
        other => panic!("our own live group must read Survived with a count, got {other:?}"),
    }
}

/// #977 — the negative half: a group whose leader has been reaped and left no
/// members must read `Exited`, because that is the answer that lets a
/// signalled record keep its `killed` reading honestly.
///
/// The child is made its own group leader with `process_group(0)` — the same
/// shape (`setsid` → pid doubles as pgid) the detached path relies on — so
/// the probed pgid is exactly the child's, then it is waited to the end and
/// the group is probed afterwards.
///
/// What this pins is the `/proc` walk finding no member for a dead group.
/// It cannot pin the pgid-reuse race the probe itself documents: an unrelated
/// process handed this pgid between the reaping and the scan would flip the
/// answer, which is why the probe claims evidence and not proof.
///
/// If the probe were removed this fails at the `assert_eq!` — with no probe
/// there is no `Exited` to produce, and `cmd_result_from` would stamp
/// `Unknown` everywhere.
#[cfg(target_os = "linux")]
#[test]
fn probing_a_reaped_group_finds_nothing() {
    use std::os::unix::process::CommandExt as _;

    let mut cmd = std::process::Command::new("/bin/sh");
    cmd.arg("-c").arg("exit 0");
    cmd.process_group(0);
    let mut child = cmd.spawn().expect("spawn sh");
    let pgid = child.id();
    let status = child.wait().expect("wait sh");
    assert!(
        status.success(),
        "fixture must exit cleanly before the probe: {status:?}"
    );

    assert_eq!(
        probe_group(Some(pgid)),
        GroupState::Exited,
        "a reaped, empty group must read Exited"
    );
}

// ---- #1056: the deadline notice replaces the kill ----

/// The notice's whole job is to be ACTIONABLE: it must name the run, state that
/// nothing was killed, carry both capture paths, and hand over the exact stop
/// command. If any of those is missing the reader either cannot find the output
/// or believes the run was stopped for it.
#[test]
fn run_notice_names_the_run_the_captures_and_the_stop_command() {
    let dir = std::env::temp_dir().join(format!("oc-notice-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let out = dir.join("out.log");
    let err = dir.join("err.log");
    std::fs::write(&out, "compiling crate a\ncompiling crate b\n").expect("write out");
    std::fs::write(&err, "").expect("write err");

    let msg = run_notice_message(
        "run-123",
        "cargo build",
        3725.0,
        Some(4242),
        &out,
        &err,
        "compiling crate a\ncompiling crate b",
    );
    let text = &msg.context_text;

    assert!(text.contains("run-123"), "names the run: {text}");
    assert!(text.contains("cargo build"), "names the label: {text}");
    assert!(
        text.contains("NOTHING WAS KILLED"),
        "must not read as a post-mortem: {text}"
    );
    assert!(
        text.contains("kill -TERM -- -4242"),
        "carries the literal stop command with the pgid: {text}"
    );
    assert!(
        text.contains(&out.display().to_string()),
        "names the stdout capture path: {text}"
    );
    assert!(
        text.contains(&err.display().to_string()),
        "names the stderr capture path: {text}"
    );
    assert!(
        text.contains("compiling crate a"),
        "carries the output tail: {text}"
    );
    assert!(
        text.contains("1h 2m"),
        "states elapsed in the shared human-scale form: {text}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// A pid that could not be read must not produce a notice naming a pgid nobody
/// has: the placeholder says where to find it instead.
#[test]
fn run_notice_without_a_pid_names_a_placeholder() {
    let dir = std::env::temp_dir().join(format!("oc-notice-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let out = dir.join("out.log");
    let err = dir.join("err.log");

    let msg = run_notice_message("run-9", "sleep", 60.0, None, &out, &err, "");
    let text = &msg.context_text;

    assert!(
        text.contains("<pgid>"),
        "an unknown pgid renders as a placeholder, never a fabricated number: {text}"
    );
    assert!(
        text.contains("kill -TERM -- -<pgid>"),
        "the placeholder is still a runnable shape: {text}"
    );
    assert!(
        text.contains("still running"),
        "the notice still says the run is alive: {text}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// The notice is a deadline, not a hang detector: it must say so, or a reader
/// treats a working long run as stuck and stops it.
#[test]
fn run_notice_distinguishes_a_deadline_from_a_hang() {
    let dir = std::env::temp_dir().join(format!("oc-notice-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let out = dir.join("out.log");
    let err = dir.join("err.log");

    let msg = run_notice_message("run-7", "cargo test", 3600.0, Some(11), &out, &err, "");
    let text = &msg.context_text;

    assert!(
        text.contains("still writing output is working"),
        "must tell the reader that live output means healthy: {text}"
    );
    assert!(
        text.contains("task_output"),
        "must name the live progress surface: {text}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

// ---- #1056: the deadline NOTIFIES, it never kills ----

/// `run_notice_secs = 0` must arm NOTHING. A task armed for zero seconds would
/// fire on the first poll and report a run that has just started, which is the
/// opposite of the knob's meaning; the deadline computation is the place that
/// decision lives, and it is pure so it can be pinned without mutating the
/// process-wide config mirror every other test reads.
#[test]
fn a_zero_notice_knob_arms_no_deadline() {
    assert!(
        notice_deadline(0).is_none(),
        "0 disables the notice entirely — it must not mean 'fire immediately'"
    );
    assert_eq!(
        notice_deadline(3600),
        Some(std::time::Duration::from_secs(3600)),
        "a positive knob is passed through unchanged"
    );
}

/// A recorder for one session's deliveries, so the two tests below can count
/// what actually reached the session rather than what was constructed.
#[cfg(unix)]
fn notice_recorder(session: Uuid) -> (Arc<Mutex<Vec<String>>>, MessageEnqueueCallback) {
    let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&seen);
    let cb: MessageEnqueueCallback = Arc::new(move |id: Uuid, msg: QueuedUserMessage| {
        if id == session
            && let Ok(mut v) = sink.lock()
        {
            v.push(msg.context_text);
        }
    });
    (seen, cb)
}

/// A detached run that outlives its deadline must be REPORTED, once, and left
/// alive — the whole point of #1056 is that the old timer killed a run that was
/// still working. "Delivered" and "still running" are asserted together because
/// either one alone passes a broken implementation: a notice that also kills
/// the child, or a child left alive with nobody told, would each satisfy half.
#[cfg(unix)]
#[tokio::test]
// Holding the guard across the polling `.await`s below is the entire point —
// same shape as the sibling suites (#22, #1206).
#[allow(clippy::await_holding_lock)]
async fn run_notice_fires_once_and_leaves_the_run_alive() {
    // `register_session_route` reaches `claim_session`, which clears the
    // process-global parked queue and channel expectations for this session —
    // so serialize against every suite that touches that state, and start from
    // a clean slate. Without it this test wipes a sibling's fixture and the
    // failure lands in whichever suite happened to be running.
    let _guard = restart_recovery::test_guard();
    let session = Uuid::new_v4();
    let (seen, route) = notice_recorder(session);
    session_routes::register_session_route(session, route);

    // Its own process group, the same shape `setsid` gives a real detached run,
    // so the pgid in the notice is the group a stop command would reach.
    let mut cmd = tokio::process::Command::new("/bin/sh");
    cmd.arg("-c").arg("sleep 30");
    cmd.process_group(0);
    let mut child = cmd.spawn().expect("spawn sleep");
    let pgid = child.id();

    let dir = tempfile::tempdir().expect("tempdir");
    let out = dir.path().join("out.log");
    let err = dir.path().join("err.log");
    std::fs::write(&out, "working on it\n").expect("write out");
    std::fs::write(&err, "").expect("write err");

    let notice = arm_run_notice_for(
        "run-notice-alive".to_string(),
        "sleep 30".to_string(),
        session,
        std::time::Instant::now(),
        pgid,
        out.clone(),
        err.clone(),
        std::time::Duration::from_millis(150),
    );

    // Poll rather than sleep a fixed span: the assertion is "exactly one
    // delivery", and a fixed sleep would race the scheduler on a loaded box.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while seen.lock().map(|v| v.len()).unwrap_or(0) == 0 {
        assert!(
            std::time::Instant::now() < deadline,
            "the notice never reached the session"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }

    // Give a second delivery every chance to happen before counting.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let delivered = seen.lock().expect("notice log").clone();
    assert_eq!(
        delivered.len(),
        1,
        "one notice per run, not a repeating alarm: {delivered:?}"
    );
    let text = &delivered[0];
    assert!(
        text.contains("NOTHING WAS KILLED"),
        "the notice must not read as a post-mortem: {text}"
    );
    if let Some(pgid) = pgid {
        assert!(
            text.contains(&format!("kill -TERM -- -{pgid}")),
            "the stop command must name the run's own group: {text}"
        );
    }

    // The load-bearing half: the run is still alive. A notice that had killed
    // it would report the same text and fail here.
    assert!(
        child.try_wait().expect("try_wait").is_none(),
        "#1056: the deadline reports a run, it does not end one"
    );

    notice.abort();
    let _ = child.kill().await;
    let _ = child.wait().await;
}

/// The other half: a run that finishes inside its deadline must produce NO
/// notice. Without this, a notice armed for every run would spam a session with
/// "still running" about work that reported its completion minutes earlier.
#[cfg(unix)]
#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn run_notice_is_cancelled_by_completion() {
    // Same global state as the sibling test above, so the same lock.
    let _guard = restart_recovery::test_guard();
    let session = Uuid::new_v4();
    let (seen, route) = notice_recorder(session);
    session_routes::register_session_route(session, route);

    let mut cmd = tokio::process::Command::new("/bin/sh");
    cmd.arg("-c").arg("exit 0");
    cmd.process_group(0);
    let mut child = cmd.spawn().expect("spawn sh");
    let pgid = child.id();

    let dir = tempfile::tempdir().expect("tempdir");
    let out = dir.path().join("out.log");
    let err = dir.path().join("err.log");

    // A deadline long enough that the run below finishes first, short enough
    // that the "no notice arrived" window is a real window and not a shrug.
    let notice = arm_run_notice_for(
        "run-notice-cancelled".to_string(),
        "exit 0".to_string(),
        session,
        std::time::Instant::now(),
        pgid,
        out,
        err,
        std::time::Duration::from_millis(500),
    );

    let status = child.wait().await.expect("wait sh");
    assert!(status.success(), "fixture must exit cleanly: {status:?}");

    // The completion path aborts the notice; do the same thing the function
    // does, then wait PAST the deadline so a surviving task would have fired.
    notice.abort();
    tokio::time::sleep(std::time::Duration::from_millis(800)).await;

    assert_eq!(
        seen.lock().expect("notice log").len(),
        0,
        "a run that finished before its deadline must deliver no notice"
    );
}
