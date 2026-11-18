//! Boot reconciliation of detached-command status files (#111 follow-up, Part D).
//!
//! `report_interrupted` accounts for a restart-killed command through its DB
//! row and notifies the owning session, but it never rewrites the status FILE.
//! Every restart-killed command therefore left a file reading `Running`
//! forever, and every file reader (`tasks_list`, the waiter sweeps) saw work
//! that no longer existed. `reconcile_stale_commands` finalizes those files at
//! boot, working from the run ids whose `background_tasks` row survived the
//! boot (#848) rather than from a scan of the whole status dir.

use crate::brain::agent::service::work_status::{
    CommandExit, CommandPaths, RunCause, WorkState, WorkStatus, test_override,
};

/// Point the status dir at a throwaway temp dir for one test.
///
/// The override is thread-local, so the test must stay single-threaded — a
/// plain `#[test]`, never `#[tokio::test]` (a worker thread would see the real
/// dir and the assertions would run against the live filesystem).
struct TempStatusDir {
    dir: Option<tempfile::TempDir>,
}

impl TempStatusDir {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("temp dir");
        test_override::set(dir.path().to_path_buf());
        Self { dir: Some(dir) }
    }
}

impl Drop for TempStatusDir {
    fn drop(&mut self) {
        test_override::clear();
        // A `Drop` impl cannot move a field out of `&mut self` (E0507), so
        // `take()` it instead. The dir still drops here — after the override
        // stops pointing at it, which is the ordering this guard exists for.
        let _ = self.dir.take();
    }
}

#[test]
fn running_command_file_is_finalized_and_completed_one_is_spared() {
    let _guard = TempStatusDir::new();

    // A command killed by a restart: written Running, process now gone.
    WorkStatus::new_command(
        "stale-cmd",
        "sess-1",
        "gh run watch",
        "gh run watch 1",
        CommandPaths::none(),
    )
    .expect("write running command");

    // A command that finished normally must be left untouched.
    WorkStatus::new_command("done-cmd", "sess-2", "true", "true", CommandPaths::none())
        .expect("write command");
    WorkStatus::finish_command(
        "done-cmd",
        "sess-2",
        "true",
        "true",
        CommandExit {
            success: true,
            code: 0,
            elapsed_secs: 0.1,
            output_bytes: 0,
            cause: RunCause::Ordinary,
        },
    )
    .expect("finish command");

    let indexed = ["stale-cmd".to_string(), "done-cmd".to_string()];
    let finalized = WorkStatus::reconcile_stale_commands(&indexed);
    assert_eq!(finalized, 1, "only the Running command file is finalized");

    let stale = WorkStatus::read("stale-cmd").expect("stale file present");
    assert_eq!(stale.state, WorkState::Interrupted);
    assert!(
        stale.state.is_terminal(),
        "a finalized file stops reading as live work"
    );

    let done = WorkStatus::read("done-cmd").expect("done file present");
    assert_eq!(
        done.state,
        WorkState::Completed,
        "terminal files are not rewritten"
    );
}

/// The leg works from the DB index, not from a directory scan (#848).
///
/// This is the behaviour change that makes the leg cheap, so it needs its own
/// test: a `Running` file the index does not name must survive untouched. If
/// someone reintroduces a `list_all()` scan here, this test fails — which is
/// the point, because the cost it removes is ~25k file reads per boot.
#[test]
fn a_running_file_the_index_does_not_name_is_left_alone() {
    let _guard = TempStatusDir::new();

    WorkStatus::new_command(
        "killed-cmd",
        "sess-1",
        "sleep 900",
        "sleep 900",
        CommandPaths::none(),
    )
    .expect("write running command");
    WorkStatus::new_command(
        "unnamed-cmd",
        "sess-2",
        "sleep 900",
        "sleep 900",
        CommandPaths::none(),
    )
    .expect("write second running command");

    let finalized = WorkStatus::reconcile_stale_commands(&["killed-cmd".to_string()]);
    assert_eq!(finalized, 1, "only the named run is finalized");

    assert_eq!(
        WorkStatus::read("killed-cmd").map(|s| s.state),
        Some(WorkState::Interrupted),
        "a named run is finalized"
    );
    assert_eq!(
        WorkStatus::read("unnamed-cmd").map(|s| s.state),
        Some(WorkState::Running),
        "an unnamed run is not found by scanning the dir — the index decides"
    );
}
