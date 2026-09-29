//! #722 phase 2: the background-task manager runs a command detached and, on
//! completion, enqueues a system QueuedUserMessage into the originating session.

use crate::brain::agent::service::MessageEnqueueCallback;
use crate::brain::agent::service::QueuedUserMessage;
use crate::brain::agent::service::background_tasks::{
    BackgroundTaskManager, CmdResult, completion_message, format_elapsed, short_label, tail_lines,
};
use crate::brain::agent::service::restart_recovery;
use crate::brain::agent::service::session_routes;
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
    let ok = completion_message(
        "cargo test",
        "cargo test --all-features",
        &CmdResult {
            success: true,
            code: 0,
            output: "test result: ok. 5 passed".into(),
        },
        12.0,
    );
    assert!(ok.context_text.contains("exit 0 (success)"));
    assert!(ok.context_text.contains("cargo test --all-features"));
    assert!(ok.context_text.contains("Do not re-run"));
    assert!(ok.display_text.contains("finished"));
    // #15: the typed receipt payload rides along for the echo card.
    let meta = ok.bg_meta.expect("bg completion carries BgTaskMeta");
    assert!(meta.success);
    assert_eq!(meta.label, "cargo test");
    assert_eq!(meta.elapsed_secs, 12.0);
    assert_eq!(meta.tail, "test result: ok. 5 passed");

    let fail = completion_message(
        "build",
        "cargo build",
        &CmdResult {
            success: false,
            code: 101,
            output: "error[E0001]".into(),
        },
        3.0,
    );
    assert!(fail.context_text.contains("exit 101 (failure)"));
    assert!(fail.display_text.contains("failed"));
    let meta = fail.bg_meta.expect("failed completion still carries meta");
    assert!(!meta.success);
    assert_eq!(meta.elapsed_secs, 3.0);
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

    mgr.clone().spawn_command(
        sid,
        cwd,
        "echo probe".to_string(),
        "echo BG_DONE_MARKER".to_string(),
    );

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

    mgr.clone().spawn_command(
        sid,
        std::env::temp_dir(),
        "reserve probe".to_string(),
        "sleep 5".to_string(),
    );

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
