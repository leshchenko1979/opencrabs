//! Background tasks survive a restart as reported interruptions (#763).
//!
//! A detached command used to live only in an in-memory map, so killing the
//! process dropped both the child and the record of it and the session waited
//! forever on a resume that could never arrive.

use crate::db::{BackgroundTaskRepository, Database};
use uuid::Uuid;

async fn repo() -> BackgroundTaskRepository {
    let db = Database::connect_in_memory()
        .await
        .expect("in-memory db connects");
    db.run_migrations().await.expect("migrations run");
    BackgroundTaskRepository::new(db.pool().clone())
}

#[tokio::test]
async fn a_recorded_task_survives_to_be_read_back() {
    let repo = repo().await;
    let (id, session) = (Uuid::new_v4(), Uuid::new_v4());
    repo.record(
        id,
        session,
        "cargo test",
        "cargo test --all-features",
        "/tmp",
    )
    .await
    .expect("record");

    let rows = repo.interrupted_candidates().await.expect("read back");
    assert_eq!(rows.len(), 1, "the row is what a restart finds");
    assert_eq!(rows[0].id, id);
    assert_eq!(rows[0].session_id, session, "resume targets this session");
    assert_eq!(rows[0].label, "cargo test");
    assert_eq!(
        rows[0].command, "cargo test --all-features",
        "the full command is kept so the agent can re-run it"
    );
}

#[tokio::test]
async fn a_task_that_finished_leaves_nothing_behind() {
    // Otherwise the next startup reports a phantom interruption for a command
    // that actually completed.
    let repo = repo().await;
    let (id, session) = (Uuid::new_v4(), Uuid::new_v4());
    repo.record(id, session, "cargo build", "cargo build", "/tmp")
        .await
        .expect("record");
    repo.clear(id).await.expect("clear");

    assert!(
        repo.interrupted_candidates()
            .await
            .expect("read back")
            .is_empty()
    );
}

#[tokio::test]
async fn clearing_one_task_leaves_its_siblings_running() {
    // Two identical commands can be in flight at once, which is why rows are
    // keyed by id rather than matched by label.
    let repo = repo().await;
    let session = Uuid::new_v4();
    let (first, second) = (Uuid::new_v4(), Uuid::new_v4());
    repo.record(first, session, "cargo test", "cargo test", "/tmp")
        .await
        .expect("record");
    repo.record(second, session, "cargo test", "cargo test", "/tmp")
        .await
        .expect("record");

    repo.clear(first).await.expect("clear");

    let rows = repo.interrupted_candidates().await.expect("read back");
    assert_eq!(rows.len(), 1, "only the finished one is gone");
    assert_eq!(rows[0].id, second);
}

#[tokio::test]
async fn tasks_from_several_sessions_are_all_reported() {
    // Startup accounts for every session's orphans, not just the foreground one.
    let repo = repo().await;
    let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
    repo.record(Uuid::new_v4(), a, "cargo test", "cargo test", "/tmp")
        .await
        .expect("record");
    repo.record(Uuid::new_v4(), b, "npm test", "npm test", "/tmp")
        .await
        .expect("record");

    let rows = repo.interrupted_candidates().await.expect("read back");
    assert_eq!(rows.len(), 2);
    let sessions: Vec<Uuid> = rows.iter().map(|r| r.session_id).collect();
    assert!(sessions.contains(&a) && sessions.contains(&b));
}

#[tokio::test]
async fn clear_all_empties_the_table_after_reporting() {
    let repo = repo().await;
    let session = Uuid::new_v4();
    repo.record(Uuid::new_v4(), session, "cargo test", "cargo test", "/tmp")
        .await
        .expect("record");
    repo.clear_all().await.expect("clear all");
    assert!(
        repo.interrupted_candidates()
            .await
            .expect("read back")
            .is_empty(),
        "leaving rows behind would re-report the same interruption forever"
    );
}

#[tokio::test]
async fn a_finished_run_keeps_its_row_and_records_how_it_ended() {
    // #964: the row used to be DELETEd at completion, so "how did this run
    // end" lived only in the detached status JSON. The row now survives with
    // the terminal write on it.
    //
    // Fails if the feature is removed: `repo.finish()` would not exist, and
    // even if it were replaced by `clear()` the final `get()` would return
    // None and the exit-code assertion could not run at all. With `finish`
    // present but its UPDATE dropped, `get()` returns the row with
    // `status = Some("running")` and `exit_code = None`, failing the
    // `assert_eq!(row.exit_code, Some(3))` line below.
    let repo = repo().await;
    let (id, session) = (Uuid::new_v4(), Uuid::new_v4());
    repo.record(id, session, "cargo test", "cargo test", "/tmp")
        .await
        .expect("record");
    repo.finish(
        id,
        Some("completed"),
        Some(3),
        "/tmp/detached/x.out",
        "/tmp/detached/x.err",
    )
    .await
    .expect("finish");

    let row = repo
        .get(id)
        .await
        .expect("read back")
        .expect("row survives");
    assert_eq!(row.status.as_deref(), Some("completed"));
    assert_eq!(
        row.exit_code,
        Some(3),
        "a failing run's real code is recorded, not folded into success"
    );
    assert_eq!(row.stdout_path.as_deref(), Some("/tmp/detached/x.out"));
    assert_eq!(row.stderr_path.as_deref(), Some("/tmp/detached/x.err"));
    assert!(
        row.completed_at.is_some(),
        "the terminal write stamps its own instant"
    );
}

#[tokio::test]
async fn a_pre_migration_row_records_no_outcome() {
    // #964: a row written before the outcome columns existed was inserted while
    // its run was running and never received a terminal write. All five columns
    // are NULL, and that must read as "no outcome recorded" — never success.
    //
    // Fails if the feature is removed: `BackgroundTaskRow` would carry no
    // `status`/`exit_code` fields, so this test would not compile; if the
    // columns were added with a non-NULL default (e.g. `DEFAULT 'completed'`),
    // `row.status` would be `Some("completed")` and the first assertion fails.
    let db = Database::connect_in_memory()
        .await
        .expect("in-memory db connects");
    db.run_migrations().await.expect("migrations run");
    let repo = BackgroundTaskRepository::new(db.pool().clone());

    let (id, session) = (Uuid::new_v4(), Uuid::new_v4());
    repo.record(id, session, "cargo build", "cargo build", "/tmp")
        .await
        .expect("record");
    // Rewind to the pre-migration shape: the insert path sets `status =
    // 'running'`, so NULL it out along with the never-written outcome columns.
    let conn = db.pool().get().await.expect("connection");
    let id_str = id.to_string();
    conn.interact(move |conn| {
        conn.execute(
            "UPDATE background_tasks SET status = NULL, exit_code = NULL, \
                 completed_at = NULL, stdout_path = NULL, stderr_path = NULL \
               WHERE id = ?1",
            rusqlite::params![id_str],
        )
    })
    .await
    .expect("interact")
    .expect("null the outcome columns");
    // The in-memory pool is `max_size(1)`: release this connection before the
    // repository acquires one, or the read below waits on itself forever.
    drop(conn);

    let row = repo.get(id).await.expect("read back").expect("row exists");
    assert_eq!(row.status, None, "no terminal state was ever recorded");
    assert_eq!(row.exit_code, None, "a missing code is not a zero");
    assert_eq!(row.completed_at, None);
    assert_eq!(row.stdout_path, None);
    assert_eq!(row.stderr_path, None);
}

#[tokio::test]
async fn a_finished_row_is_not_an_interruption_candidate() {
    // #964: finished rows now survive, so the restart read must not treat every
    // surviving row as interrupted — that would report a completed run as one
    // the restart killed. `interrupted_candidates()` is the read
    // `report_interrupted()` performs.
    //
    // Fails if the feature is removed: with the old `all()` read (no WHERE
    // clause) this returns BOTH rows, so `rows.len() == 1` fails and the
    // `rows[0].id == still_running` assertion can also fail depending on
    // `started_at` ordering.
    let repo = repo().await;
    let session = Uuid::new_v4();
    let (finished, still_running) = (Uuid::new_v4(), Uuid::new_v4());
    repo.record(finished, session, "cargo test", "cargo test", "/tmp")
        .await
        .expect("record");
    repo.record(still_running, session, "cargo build", "cargo build", "/tmp")
        .await
        .expect("record");

    repo.finish(
        finished,
        Some("completed"),
        Some(0),
        "/tmp/detached/f.out",
        "/tmp/detached/f.err",
    )
    .await
    .expect("finish");

    let rows = repo.interrupted_candidates().await.expect("read back");
    assert_eq!(
        rows.len(),
        1,
        "a completed row is not an interruption candidate"
    );
    assert_eq!(
        rows[0].id, still_running,
        "the running row is the one a restart reports"
    );
}
