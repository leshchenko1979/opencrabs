//! Migration coverage for the safe-auto-resume turn journal.
//!
//! `20261122000002_add_tool_executions_turn_journal.sql` adds the two columns
//! the journal row needs (`turn_id`, `arguments`) and the `status` index the
//! boot reconciler reads. The repository tests in `tool_execution_repo_test`
//! prove the methods work; these tests prove the SCHEMA they run against is
//! the one a migrated database actually has, including the store that never
//! carried `arguments` because `20260415000003` rebuilt the table without it.

use crate::db::Database;

async fn migrated_db() -> Database {
    let db = Database::connect_in_memory().await.unwrap();
    db.run_migrations().await.unwrap();
    db
}

/// `(name, type, notnull)` for every `tool_executions` column.
async fn tool_execution_columns(db: &Database) -> Vec<(String, String, i64)> {
    db.pool()
        .get()
        .await
        .unwrap()
        .interact(|conn| {
            conn.prepare(
                "SELECT name, type, \"notnull\" FROM pragma_table_info('tool_executions')",
            )?
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()
        })
        .await
        .unwrap()
        .unwrap()
}

/// The journal row cannot be written without both columns, and both are
/// nullable: pre-existing rows have no `turn_id`, and the `arguments` column
/// is new to a table that lost it in the 20260415 rebuild.
#[tokio::test]
async fn migration_adds_turn_id_and_arguments_columns() {
    let db = migrated_db().await;
    let cols = tool_execution_columns(&db).await;

    for want in ["turn_id", "arguments"] {
        let col = cols
            .iter()
            .find(|(name, _, _)| name == want)
            .unwrap_or_else(|| panic!("tool_executions column {want} missing; got {cols:?}"));
        assert_eq!(col.1, "TEXT", "{want} must be TEXT");
        assert_eq!(
            col.2, 0,
            "{want} must be nullable: existing rows have no value and the journal must not \
             invent one"
        );
    }
}

/// The boot reconciler filters on `status` ALONE
/// (`UPDATE … WHERE status = 'started'`), the first such query in the tree (it
/// lands in the next step of this card; no existing read does), which is why
/// the two earlier migrations' "no index on status" ruling no longer applies.
/// On a 1.1 GB table an unindexed reconcile is a full scan at every boot.
#[tokio::test]
async fn migration_adds_the_status_index_the_boot_reconciler_reads() {
    let db = migrated_db().await;

    let (indexed_column, plan) = db
        .pool()
        .get()
        .await
        .unwrap()
        .interact(|conn| {
            let cols: Vec<String> = conn
                .prepare("SELECT name FROM pragma_index_info('idx_tool_executions_status')")?
                .query_map([], |r| r.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            let plan: Vec<String> = conn
                .prepare(
                    "EXPLAIN QUERY PLAN \
                     SELECT id FROM tool_executions WHERE status = 'started'",
                )?
                .query_map([], |r| r.get::<_, String>(3))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok::<_, rusqlite::Error>((cols, plan))
        })
        .await
        .unwrap()
        .unwrap();

    assert_eq!(
        indexed_column,
        vec!["status".to_owned()],
        "idx_tool_executions_status must index exactly (status)"
    );
    assert!(
        plan.iter()
            .any(|line| line.contains("idx_tool_executions_status")),
        "the reconciler's status-only filter must use the index; plan was {plan:?}"
    );
}

/// The migrated table accepts the journal insert shape: a nullable `turn_id`
/// and `arguments` alongside the existing required columns, with `created_at`
/// defaulted.
#[tokio::test]
async fn migrated_table_accepts_a_started_journal_row() {
    let db = migrated_db().await;

    db.pool()
        .get()
        .await
        .unwrap()
        .interact(|conn| {
            conn.execute(
                "INSERT INTO tool_executions \
                     (id, message_id, session_id, turn_id, tool_name, arguments, status) \
                     VALUES ('mig-call-1', 'msg-1', 'sess-1', 'turn-1', 'bash', \
                             '{\"command\":\"git push\"}', 'started')",
                [],
            )?;
            let row: (String, String, Option<String>, String) = conn.query_row(
                "SELECT turn_id, tool_name, arguments, status FROM tool_executions \
                 WHERE id = 'mig-call-1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )?;
            assert_eq!(row.0, "turn-1");
            assert_eq!(row.1, "bash");
            assert_eq!(row.2.as_deref(), Some("{\"command\":\"git push\"}"));
            assert_eq!(row.3, "started");
            Ok::<(), rusqlite::Error>(())
        })
        .await
        .unwrap()
        .unwrap();
}

/// A migrated database is stamped at the end of the list: appending this
/// migration must not leave `user_version` behind (the list is applied by
/// index, so a stamp drift here would skip the entry on the next open).
#[tokio::test]
async fn migration_lands_at_the_current_version() {
    let db = migrated_db().await;
    let version: i64 = db
        .pool
        .get()
        .await
        .unwrap()
        .interact(|conn| conn.pragma_query_value(None, "user_version", |r| r.get(0)))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(version, Database::MIGRATION_COUNT as i64);
}
