//! #974 — the per-turn call journal at the dispatch seam.
//!
//! The defect these legs pin is an ORDERING one: `record()` writes a call's
//! `tool_executions` row in a spawned task AFTER the tool returned, so a turn
//! killed mid-call leaves no row at all — the exact case a journal exists for.
//! The loop therefore opens a `started` row BEFORE dispatch
//! (`turn_journal::open_call`) and closes it with the outcome
//! (`turn_journal::close_call`). These tests drive that seam with real tools
//! against an in-memory database, so a regression that moves the write back
//! after the call, or that leaves the row reading `started`, fails here.
//!
//! Mutants killed:
//!
//! * journaling AFTER dispatch — an interrupted call leaves no row, and the
//!   first test's "the row exists while the call is in flight" assertion goes
//!   red;
//! * completing with `record()` instead of the UPDATE — `INSERT OR IGNORE`
//!   leaves the row reading `started` forever, which the unknown-reconcile
//!   leg catches;
//! * journaling every call — the read-only leg finds a row where there must
//!   be none.

use crate::brain::agent::service::turn_journal::{close_call, open_call};
use crate::brain::tools::read::ReadTool;
use crate::brain::tools::write::WriteTool;
use crate::brain::tools::{Tool, ToolExecutionContext};
use crate::db::Database;
use crate::db::repository::{STATUS_STARTED, ToolExecutionRepository};
use rusqlite::OptionalExtension;
use tempfile::TempDir;
use uuid::Uuid;

async fn make_db() -> Database {
    let db = Database::connect_in_memory().await.unwrap();
    db.run_migrations().await.unwrap();
    db
}

/// Rows in `tool_executions`, whatever their status.
async fn row_count(db: &Database) -> i64 {
    db.pool()
        .get()
        .await
        .unwrap()
        .interact(|conn| {
            conn.query_row("SELECT COUNT(*) FROM tool_executions", [], |r| {
                r.get::<_, i64>(0)
            })
        })
        .await
        .unwrap()
        .unwrap()
}

/// The stored status of one row, `None` when the row does not exist.
async fn row_status(db: &Database, id: &str) -> Option<String> {
    let id = id.to_string();
    db.pool()
        .get()
        .await
        .unwrap()
        .interact(move |conn| {
            conn.query_row(
                "SELECT status FROM tool_executions WHERE id = ?1",
                rusqlite::params![id],
                |r| r.get::<_, String>(0),
            )
            .optional()
        })
        .await
        .unwrap()
        .unwrap()
}

/// Acceptance leg 1: one mutating call, one row, carrying the final outcome.
#[tokio::test]
async fn a_mutating_call_leaves_exactly_one_row_with_its_final_outcome() {
    let db = make_db().await;
    let repo = ToolExecutionRepository::new(db.pool().clone());
    let temp = TempDir::new().unwrap();
    let session_id = Uuid::new_v4();
    let turn_id = Uuid::new_v4();
    let message_id = Uuid::new_v4();
    let ctx =
        ToolExecutionContext::new(session_id).with_working_directory(temp.path().to_path_buf());
    let input = serde_json::json!({"path": "notes.txt", "content": "hello"});

    // The loop journals BEFORE dispatch.
    let row_id = open_call(&repo, message_id, session_id, turn_id, "write_file", &input)
        .await
        .expect("a write is side-effecting and must be journaled");

    // The row is already there while the call is in flight — this is the
    // assertion the after-dispatch mutant cannot pass.
    assert_eq!(
        row_status(&db, &row_id).await.as_deref(),
        Some(STATUS_STARTED),
        "the journal row must exist BEFORE the tool runs"
    );
    assert_eq!(
        repo.unknown_for_turn(&turn_id.to_string())
            .await
            .unwrap()
            .len(),
        1,
        "an in-flight call reads as unresolved"
    );

    // Drive the real tool, then close the row with its outcome.
    let result = WriteTool.execute(input.clone(), &ctx).await.unwrap();
    assert!(result.success);
    close_call(
        &repo,
        &row_id,
        result.success,
        result.exit_code(),
        Some("test-provider"),
        Some("test-model"),
        Some(3),
    )
    .await;

    assert_eq!(
        row_count(&db).await,
        1,
        "one call must leave exactly one row — the journal row IS the analytics row"
    );
    assert_eq!(
        row_status(&db, &row_id).await.as_deref(),
        Some("success"),
        "the row must carry the call's FINAL outcome, not `started`"
    );
    assert!(
        repo.unknown_for_turn(&turn_id.to_string())
            .await
            .unwrap()
            .is_empty(),
        "a completed call is not unknown"
    );
}

/// Acceptance leg 2: a read-only call writes nothing at all.
#[tokio::test]
async fn a_read_only_call_leaves_no_row() {
    let db = make_db().await;
    let repo = ToolExecutionRepository::new(db.pool().clone());
    let temp = TempDir::new().unwrap();
    let session_id = Uuid::new_v4();
    let turn_id = Uuid::new_v4();
    let message_id = Uuid::new_v4();
    std::fs::write(temp.path().join("notes.txt"), "hello").unwrap();
    let ctx =
        ToolExecutionContext::new(session_id).with_working_directory(temp.path().to_path_buf());
    let input = serde_json::json!({"path": "notes.txt"});

    let journaled = open_call(&repo, message_id, session_id, turn_id, "read_file", &input).await;
    assert!(
        journaled.is_none(),
        "read-only work must not be journaled — re-reading cannot duplicate an effect"
    );

    // Drive the real tool: the call happens, it simply leaves no receipt.
    let result = ReadTool.execute(input, &ctx).await.unwrap();
    assert!(result.success);

    assert_eq!(
        row_count(&db).await,
        0,
        "a read-only call must leave ZERO rows"
    );
    assert!(
        repo.unknown_for_turn(&turn_id.to_string())
            .await
            .unwrap()
            .is_empty()
    );
}

/// The journal's reason for existing: a call interrupted between open and
/// close is reconciled to `unknown`, and a completed sibling stays `success`.
#[tokio::test]
async fn an_interrupted_call_reads_unknown_after_boot_reconcile() {
    let db = make_db().await;
    let repo = ToolExecutionRepository::new(db.pool().clone());
    let session_id = Uuid::new_v4();
    let turn_id = Uuid::new_v4();
    let message_id = Uuid::new_v4();

    let finished = open_call(
        &repo,
        message_id,
        session_id,
        turn_id,
        "write_file",
        &serde_json::json!({"path": "done.txt", "content": "x"}),
    )
    .await
    .expect("journaled");
    close_call(&repo, &finished, true, Some(0), None, None, None).await;

    let interrupted = open_call(
        &repo,
        message_id,
        session_id,
        turn_id,
        "write_file",
        &serde_json::json!({"path": "half.txt", "content": "y"}),
    )
    .await
    .expect("journaled");
    // The process "dies" here: no close_call for this one.

    // Before boot the row still reads `started`; `unknown_for_turn` already
    // treats it as unresolved, so the replay decision never depends on the
    // reconciler having run first.
    assert_eq!(
        row_status(&db, &interrupted).await.as_deref(),
        Some(STATUS_STARTED)
    );

    let reconciled = repo.reconcile_started_to_unknown().await.unwrap();
    assert_eq!(reconciled, 1, "exactly the unfinished row is flipped");
    assert_eq!(
        row_status(&db, &interrupted).await.as_deref(),
        Some(crate::db::repository::STATUS_UNKNOWN)
    );
    assert_eq!(
        row_status(&db, &finished).await.as_deref(),
        Some("success"),
        "a completed row must never be re-stamped"
    );

    let unknown = repo.unknown_for_turn(&turn_id.to_string()).await.unwrap();
    assert_eq!(unknown.len(), 1);
    assert_eq!(unknown[0].id, interrupted);
    assert_eq!(unknown[0].tool_name, "write_file");
}
