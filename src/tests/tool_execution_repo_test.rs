//! Tests for `ToolExecutionRepository`.
//!
//! Pins the empty-`tool_name` guard: on 2026-04-28 a model emitted 44
//! `tool_use` blocks with no name field; each dispatch errored but the
//! failure still got recorded with `tool_name = ""`, producing a blank
//! row in the /usage dashboard's "Core Tools" card. The repository now
//! refuses to insert empties.

use crate::db::Database;
use crate::db::repository::ToolExecutionRepository;

async fn make_db() -> Database {
    let db = Database::connect_in_memory().await.unwrap();
    db.run_migrations().await.unwrap();
    db
}

#[tokio::test]
async fn record_skips_empty_tool_name() {
    let db = make_db().await;
    let repo = ToolExecutionRepository::new(db.pool().clone());
    repo.record("id-empty", "msg-1", "sess-1", "", "error", None, None, None, None)
        .await
        .expect("empty record returns Ok, just skips the insert");
    let stats = repo.stats_by_tool(None).await.unwrap();
    assert!(stats.is_empty(), "empty tool_name must not land in DB");
}

#[tokio::test]
async fn record_skips_whitespace_only_tool_name() {
    let db = make_db().await;
    let repo = ToolExecutionRepository::new(db.pool().clone());
    repo.record(
        "id-ws", "msg-1", "sess-1", "   \t  ", "error", None, None, None,        None,
    )
    .await
    .expect("whitespace-only is treated as empty");
    let stats = repo.stats_by_tool(None).await.unwrap();
    assert!(stats.is_empty());
}

#[tokio::test]
async fn record_accepts_normal_tool_name() {
    let db = make_db().await;
    let repo = ToolExecutionRepository::new(db.pool().clone());
    repo.record(
        "id-bash",
        "msg-1",
        "sess-1",
        "bash",
        "completed",
        None,
        None,
        None,        None,
    )
    .await
    .unwrap();
    repo.record(
        "id-grep",
        "msg-2",
        "sess-1",
        "grep",
        "completed",
        None,
        None,
        None,        None,
    )
    .await
    .unwrap();
    let stats = repo.stats_by_tool(None).await.unwrap();
    assert_eq!(stats.len(), 2);
    let names: Vec<&str> = stats.iter().map(|s| s.tool_name.as_str()).collect();
    assert!(names.contains(&"bash"));
    assert!(names.contains(&"grep"));
}

#[tokio::test]
async fn stats_by_tool_filters_legacy_empty_rows() {
    let db = make_db().await;
    let repo = ToolExecutionRepository::new(db.pool().clone());
    // Insert directly via the raw pool, bypassing the guard, to simulate
    // legacy pre-guard rows that already live in users' production DBs.
    db.pool()
        .get()
        .await
        .unwrap()
        .interact(|conn| {
            conn.execute(
                "INSERT INTO tool_executions (id, message_id, session_id, tool_name, status) \
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                rusqlite::params!["legacy-1", "msg-1", "sess-1", "", "error"],
            )
        })
        .await
        .unwrap()
        .unwrap();
    repo.record(
        "id-bash",
        "msg-2",
        "sess-1",
        "bash",
        "completed",
        None,
        None,
        None,        None,
    )
    .await
    .unwrap();

    let stats = repo.stats_by_tool(None).await.unwrap();
    assert_eq!(
        stats.len(),
        1,
        "the legacy empty-name row must be filtered out by the SELECT"
    );
    assert_eq!(stats[0].tool_name, "bash");
}

// #687: garbage tool names from phantom tool calls must be rejected.
#[tokio::test]
async fn record_skips_garbage_tool_name_with_xml_fragment() {
    let db = make_db().await;
    let repo = ToolExecutionRepository::new(db.pool().clone());
    let garbage = "i_apologize... i'll_call_write_opencrabs_file...";
    repo.record(
        "id-garbage",
        "msg-1",
        "sess-1",
        garbage,
        "error",
        None,
        None,
        None,        None,
    )
    .await
    .expect("garbage record returns Ok, just skips the insert");
    let stats = repo.stats_by_tool(None).await.unwrap();
    assert!(stats.is_empty(), "garbage tool_name must not land in DB");
}

#[tokio::test]
async fn record_skips_tool_name_with_uppercase() {
    let db = make_db().await;
    let repo = ToolExecutionRepository::new(db.pool().clone());
    repo.record(
        "id-upper", "msg-1", "sess-1", "Bash", "error", None, None, None,        None,
    )
    .await
    .unwrap();
    let stats = repo.stats_by_tool(None).await.unwrap();
    assert!(stats.is_empty(), "uppercase tool_name must be rejected");
}

#[tokio::test]
async fn record_skips_overlong_tool_name() {
    let db = make_db().await;
    let repo = ToolExecutionRepository::new(db.pool().clone());
    let long_name = "a".repeat(65);
    repo.record(
        "id-long", "msg-1", "sess-1", &long_name, "error", None, None, None,        None,
    )
    .await
    .unwrap();
    let stats = repo.stats_by_tool(None).await.unwrap();
    assert!(stats.is_empty(), "tool_name >64 chars must be rejected");
}

#[tokio::test]
async fn record_accepts_tool_name_with_digits_and_underscores() {
    let db = make_db().await;
    let repo = ToolExecutionRepository::new(db.pool().clone());
    repo.record(
        "id-ok",
        "msg-1",
        "sess-1",
        "web_search_v2",
        "success",
        None,
        None,
        None,        None,
    )
    .await
    .unwrap();
    let stats = repo.stats_by_tool(None).await.unwrap();
    assert_eq!(stats.len(), 1);
    assert_eq!(stats[0].tool_name, "web_search_v2");
}

/// #787: a detached run's row is written twice — once when the run is SPAWNED
/// (no code yet) and once when it ENDS. The second write is the whole point:
/// without it every detached run stays recorded as a successful `bash` call
/// with no exit code, whatever it went on to do.
#[tokio::test]
async fn update_outcome_writes_the_runs_real_result() {
    let db = make_db().await;
    let repo = ToolExecutionRepository::new(db.pool().clone());
    // The spawn-time row, exactly as `tool_loop` writes it: the run has not
    // exited, so there is no code, and the spawn itself succeeded.
    repo.record("run-1", "msg-1", "sess-1", "bash", "success", None, None, None, None)
        .await
        .unwrap();

    let touched = repo
        .update_outcome("run-1", Some(7), "success")
        .await
        .expect("update succeeds");
    assert_eq!(touched, 1, "the row exists, so exactly one row is touched");

    let pool = db.pool().clone();
    let row: (Option<i32>, String) = pool
        .get()
        .await
        .unwrap()
        .interact(move |conn| {
            conn.query_row(
                "SELECT exit_code, status FROM tool_executions WHERE id = 'run-1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
        })
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.0, Some(7), "the run's real exit code lands on the row");
    assert_eq!(row.1, "success", "#763's split is not restated here");
}

/// The insert is spawned and can lose the race against a fast run, so a zero
/// is a RETRY signal, not a verdict — the caller must be able to tell the two
/// apart, which is why the count is returned rather than swallowed.
#[tokio::test]
async fn update_outcome_on_a_missing_row_reports_zero() {
    let db = make_db().await;
    let repo = ToolExecutionRepository::new(db.pool().clone());
    let touched = repo
        .update_outcome("never-inserted", Some(0), "success")
        .await
        .expect("a missing row is not an error");
    assert_eq!(touched, 0, "no row yet is reported, never invented");
}
