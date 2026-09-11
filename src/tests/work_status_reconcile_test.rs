//! Boot reconciliation of detached-command status files (#111 follow-up, Part D).
//!
//! `report_interrupted` accounts for a restart-killed command through its DB
//! row and notifies the owning session, but it never rewrites the status FILE.
//! Every restart-killed command therefore left a file reading `Running`
//! forever, and every file reader (`tasks_list`, the waiter sweeps) saw work
//! that no longer existed. `reconcile_stale_commands` finalizes those files at
//! boot. At boot no command can be live, so a `Running` command file is stale
//! by construction.

use crate::brain::agent::service::work_status::{
    CommandExit, WorkState, WorkStatus, test_override,
};

/// Point the status dir at a throwaway temp dir for one test.
///
/// The override is thread-local, so the test must stay single-threaded — a
/// plain `#[test]`, never `#[tokio::test]` (a worker thread would see the real
/// dir and the assertions would run against the live filesystem).
struct TempStatusDir {
    dir: tempfile::TempDir,
}

impl TempStatusDir {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("temp dir");
        test_override::set(dir.path().to_path_buf());
        Self { dir }
    }
}

impl Drop for TempStatusDir {
    fn drop(&mut self) {
        test_override::clear();
        drop(self.dir);
    }
}

#[test]
fn running_command_file_is_finalized_and_completed_one_is_spared() {
    let _guard = TempStatusDir::new();

    // A command killed by a restart: written Running, process now gone.
    WorkStatus::new_command("stale-cmd", "sess-1", "gh run watch", "gh run watch 1")
        .expect("write running command");

    // A command that finished normally must be left untouched.
    WorkStatus::new_command("done-cmd", "sess-2", "true", "true").expect("write command");
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
        },
    )
    .expect("finish command");

    let finalized = WorkStatus::reconcile_stale_commands();
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
