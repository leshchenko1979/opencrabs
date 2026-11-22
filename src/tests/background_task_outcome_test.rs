//! #964: the `background_tasks` row carries its own outcome.
//!
//! Before this, the terminal state of a detached command lived only in its
//! per-run status JSON under `tmp/runs/<id>.json`, and the completion path
//! DELETED the row. A reader holding the row — `tasks_list`, an operator query,
//! a lane auditing its own history — could not say whether the run finished,
//! failed or never started, and could not locate the output without knowing the
//! file-naming convention.
//!
//! The row is now stamped at completion and kept; pre-migration rows keep all
//! five columns `NULL` and render `unknown`, never `success`.
//!
//! ## Why part of this is a source sentinel
//!
//! The completion write resolves its repository through the process-wide
//! `global_pool()` `OnceLock`, which only a real `Database::connect` sets — no
//! test can install one (the same limitation `notify_queue_reap_order_test`
//! documents). So the data layer is pinned behaviourally here (a real process
//! → the real derivation → a real row), and the one line that connects the
//! completion path to it is pinned as source, because a dropped call would
//! otherwise be silent: the row would simply stay `running` forever.

use crate::brain::agent::service::background_tasks::{
    CmdResult, cmd_result_from, task_outcome_record,
};
use crate::brain::agent::service::work_status::{self, RunCause, SignalOrigin};
use crate::brain::tools::tasks_list::resolve_state;
use crate::db::{BackgroundTaskOutcome, BackgroundTaskRepository, Database};
use tempfile::TempDir;
use uuid::Uuid;

const SERVICE_SRC: &str = include_str!("../brain/agent/service/background_tasks.rs");

/// A migrated in-memory store plus its repository.
///
/// The `Database` is returned and held by the caller: it owns the pool the
/// repository borrows from.
async fn repo_and_db() -> (BackgroundTaskRepository, Database) {
    let db = Database::connect_in_memory()
        .await
        .expect("in-memory db connects");
    db.run_migrations().await.expect("migrations run");
    let repo = BackgroundTaskRepository::new(db.pool().clone());
    (repo, db)
}

/// (a) A migrated store's `background_tasks` schema carries all five columns.
///
/// Read from `sqlite_master`, which is the text `.schema background_tasks`
/// prints — an `ALTER TABLE ADD COLUMN` rewrites that entry, so this is the
/// operator-visible schema and not a second-hand summary of it.
#[tokio::test]
async fn migrated_store_lists_all_five_outcome_columns() {
    let (_repo, db) = repo_and_db().await;

    let schema: String = db
        .pool()
        .get()
        .await
        .unwrap()
        .interact(|conn| {
            conn.query_row(
                "SELECT sql FROM sqlite_master WHERE type='table' AND name='background_tasks'",
                [],
                |r| r.get(0),
            )
        })
        .await
        .unwrap()
        .unwrap();

    for column in [
        "status",
        "exit_code",
        "completed_at",
        "stdout_path",
        "stderr_path",
    ] {
        assert!(
            schema.contains(column),
            "`.schema background_tasks` is missing {column}; was: {schema}"
        );
    }
    // The original six are still there: the outcome is ADDED to the record, not
    // a replacement for it.
    for column in ["id", "session_id", "label", "command", "cwd", "started_at"] {
        assert!(
            schema.contains(column),
            "`.schema background_tasks` lost {column}; was: {schema}"
        );
    }

    // And the columns are real, in the exact spelling the writers use.
    let names: Vec<String> = db
        .pool()
        .get()
        .await
        .unwrap()
        .interact(|conn| {
            conn.prepare("SELECT name FROM pragma_table_info('background_tasks')")?
                .query_map([], |r| r.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()
        })
        .await
        .unwrap()
        .unwrap();
    for column in [
        "status",
        "exit_code",
        "completed_at",
        "stdout_path",
        "stderr_path",
    ] {
        assert!(
            names.iter().any(|n| n == column),
            "column {column} missing from pragma_table_info; found {names:?}"
        );
    }
}

/// (b) A completed run's row carries its REAL exit code and both capture paths.
///
/// The result comes from a real process through [`cmd_result_from`], the same
/// derivation the completion path applies — not a hand-written `CmdResult` the
/// assertion was then written against.
#[tokio::test]
async fn a_completed_run_records_its_real_exit_code_and_both_capture_paths() {
    let dir = TempDir::new().unwrap();
    work_status::test_override::set(dir.path().to_path_buf());

    let (repo, _db) = repo_and_db().await;
    let (id, session) = (Uuid::new_v4(), Uuid::new_v4());
    repo.record(id, session, "probe", "sh -c 'exit 3'", "/tmp")
        .await
        .expect("record");

    let output = std::process::Command::new("sh")
        .args(["-c", "exit 3"])
        .output()
        .expect("spawn sh");
    let result = cmd_result_from(&output);
    assert_eq!(
        result.code, 3,
        "the fixture must produce the code the row is checked against"
    );
    assert!(!result.success);

    let id_str = id.to_string();
    let written = repo
        .finish(id, task_outcome_record(&id_str, &result))
        .await
        .expect("stamp the row");
    assert_eq!(
        written, 1,
        "the completed run's row must still exist to be stamped"
    );

    let row = repo
        .outcome(&id_str)
        .await
        .expect("read the row")
        .expect("the row SURVIVES completion (#964)");

    let expected_out = work_status::command_output_path(&id_str, false)
        .display()
        .to_string();
    let expected_err = work_status::command_output_path(&id_str, true)
        .display()
        .to_string();

    // Cleared before the assertions: a failing assert must not leave this
    // thread's override pointing at a TempDir that is about to be deleted.
    work_status::test_override::clear();

    assert_eq!(row.status.as_deref(), Some("failed"), "was: {row:?}");
    assert_eq!(
        row.exit_code,
        Some(3),
        "the row must carry the command's OWN exit code; was: {row:?}"
    );
    assert_eq!(
        row.stdout_path.as_deref(),
        Some(expected_out.as_str()),
        "the row alone must locate stdout; was: {row:?}"
    );
    assert_eq!(
        row.stderr_path.as_deref(),
        Some(expected_err.as_str()),
        "the row alone must locate stderr; was: {row:?}"
    );
    assert!(
        row.completed_at.is_some(),
        "the terminal transition is stamped; was: {row:?}"
    );
    assert_eq!(row.state_label(), "failed");

    // A completed row is history, not an orphan: the next boot must not report
    // it as interrupted (the phantom interruption this table exists to avoid).
    assert!(
        repo.all().await.expect("read back").is_empty(),
        "a completed row must not be re-reported as interrupted at the next boot"
    );
}

/// (b cont.) `exit_code` is the COMMAND's own code — `NULL` when the number is
/// the harness's.
///
/// A ceiling kill, a signalled death and a never-awaited child all fold a
/// harness number into `code` (137 for SIGKILL). Writing that into an
/// `exit_code` column would be the #938 defect in a new column.
#[tokio::test]
async fn a_signalled_death_records_no_exit_code_of_its_own() {
    let dir = TempDir::new().unwrap();
    work_status::test_override::set(dir.path().to_path_buf());

    let id = Uuid::new_v4().to_string();
    let ceiling = CmdResult {
        success: false,
        code: 137,
        cause: RunCause::Ceiling { limit_secs: 900 },
        output: String::new(),
    };
    let signalled = CmdResult {
        success: false,
        code: 143,
        // #966 made `origin` a required field; this result is hand-built, so the
        // honest origin is `Unknown` (nobody recorded the sender), not the
        // `External` the live construction site stamps.
        cause: RunCause::Signalled {
            signal: 15,
            origin: SignalOrigin::Unknown,
        },
        output: String::new(),
    };
    let not_awaited = CmdResult {
        success: false,
        code: -1,
        cause: RunCause::NotAwaited,
        output: String::new(),
    };

    let outcomes = [
        task_outcome_record(&id, &ceiling),
        task_outcome_record(&id, &signalled),
        task_outcome_record(&id, &not_awaited),
    ];
    work_status::test_override::clear();

    for outcome in &outcomes {
        assert_eq!(
            outcome.exit_code, None,
            "a harness code must not be recorded as an exit code; was: {outcome:?}"
        );
    }

    // #966 says a signalled death and a ceiling kill are not the command
    // failing. If this recorded `failed`, the row would carry the exact
    // misreading that issue removed from the rendered text.
    assert_eq!(
        outcomes[0].status.as_deref(),
        Some("killed"),
        "ceiling kill"
    );
    assert_eq!(
        outcomes[1].status.as_deref(),
        Some("killed"),
        "signalled death"
    );
    // Nothing ran here, so it is not a success — but it is not a kill either.
    assert_eq!(
        outcomes[2].status.as_deref(),
        Some("failed"),
        "never awaited"
    );

    // The label must survive the reader, not just the writer: `killed` is
    // terminal, so a row that was stopped reports how it ended.
    assert_eq!(outcomes[1].state_label(), "killed");
    assert_eq!(
        outcomes[1].terminal_label(),
        Some("killed"),
        "a stopped run is a terminal state, or the reader falls through to unknown"
    );
}

/// (c) A pre-migration row (all five `NULL`) renders `unknown`, never `success`.
#[tokio::test]
async fn a_pre_migration_row_renders_unknown_never_success() {
    let (repo, db) = repo_and_db().await;
    let (id, session) = (Uuid::new_v4(), Uuid::new_v4());

    // Exactly the row a store written BEFORE the migration holds: the six
    // original columns and nothing else.
    let (id_s, session_s) = (id.to_string(), session.to_string());
    db.pool()
        .get()
        .await
        .unwrap()
        .interact(move |conn| {
            conn.execute(
                "INSERT INTO background_tasks \
                     (id, session_id, label, command, cwd, started_at) \
                     VALUES (?1, ?2, 'cargo test', 'cargo test --lib', '/tmp', 1)",
                rusqlite::params![id_s, session_s],
            )
        })
        .await
        .unwrap()
        .unwrap();

    let row = repo
        .outcome(&id.to_string())
        .await
        .expect("read the row")
        .expect("row exists");
    assert!(
        row.is_pre_migration(),
        "all five outcome columns must read back NULL; was: {row:?}"
    );
    assert_eq!(
        row.state_label(),
        "unknown",
        "NULL outcome must render unknown; was: {row:?}"
    );
    assert_ne!(
        row.state_label(),
        "success",
        "a defaulted success is the lie #938 removed"
    );
    assert_eq!(
        resolve_state(Some(&row), None),
        "unknown",
        "with the status JSON gone, a pre-migration row must not become a success"
    );

    // It is still IN FLIGHT for restart accounting: a NULL status is a row whose
    // owning process is as gone as any other, and its session must still hear
    // about the interruption.
    assert_eq!(
        repo.all().await.expect("read back").len(),
        1,
        "a pre-migration row must still be reported at the next boot"
    );
}

/// The read order: the row is preferred, the JSON is the pre-migration fallback.
#[test]
fn resolve_state_prefers_the_row_and_falls_back_to_the_json() {
    let done = BackgroundTaskOutcome {
        status: Some("success".to_string()),
        ..Default::default()
    };
    // The row answers even with no JSON: that is the whole point of #964.
    assert_eq!(resolve_state(Some(&done), None), "success");
    // A terminal row outranks a stale JSON.
    assert_eq!(resolve_state(Some(&done), Some("running")), "success");

    // A pre-migration row has no answer, so the JSON is what is left.
    let pre = BackgroundTaskOutcome::default();
    assert_eq!(resolve_state(Some(&pre), Some("completed")), "completed");
    // Neither record: unknown — never a defaulted success.
    assert_eq!(resolve_state(Some(&pre), None), "unknown");
    assert_eq!(resolve_state(None, None), "unknown");

    // The spawn-time `running` is a statement the writer made; it is reported
    // as such, and only from the row.
    let live = BackgroundTaskOutcome {
        status: Some("running".to_string()),
        ..Default::default()
    };
    assert_eq!(resolve_state(Some(&live), None), "running");
}

/// (d) The migration is registered in `MIGRATION_SQL` exactly once, and last.
///
/// The list is applied by INDEX, so an entry inserted anywhere but the end is
/// skipped on every database already stamped past that index (#1401 class).
#[test]
fn the_outcome_migration_is_registered_exactly_once() {
    let list = crate::db::database::MIGRATION_SQL;
    let hits: Vec<usize> = list
        .iter()
        .enumerate()
        .filter(|(_, sql)| {
            sql.contains("ADD COLUMN stdout_path") && sql.contains("ADD COLUMN stderr_path")
        })
        .map(|(index, _)| index)
        .collect();
    assert_eq!(
        hits.len(),
        1,
        "the #964 outcome migration must appear exactly once in MIGRATION_SQL, found at {hits:?}"
    );
    assert_eq!(
        hits[0],
        list.len() - 1,
        "it must be APPENDED: the list is applied by index, so an inserted entry is skipped \
         on every store already stamped past it"
    );
    assert_eq!(
        crate::db::Database::MIGRATION_COUNT,
        list.len(),
        "the count is the list's length"
    );
}

/// The detached completion path really stamps the row, and the inline path
/// still clears its own.
///
/// A source sentinel because the completion write resolves `global_pool()`,
/// which no test can install (see this file's header). Without it, deleting the
/// call would leave every completed run's row at `running` forever and no test
/// above would notice — the derivation and the store would both stay green.
#[test]
fn the_detached_completion_path_stamps_the_row() {
    assert_eq!(
        SERVICE_SRC
            .matches("finish_task_row(&id, &label, &result).await;")
            .count(),
        1,
        "the detached completion must stamp its row exactly once"
    );
    assert_eq!(
        SERVICE_SRC
            .matches("clear_row(run_uuid, &label).await;")
            .count(),
        1,
        "the INLINE arm must still drop its row: only a handed-over run keeps one"
    );
}
