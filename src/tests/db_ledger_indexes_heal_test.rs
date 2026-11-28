//! The `usage_ledger` index heal added for #1076 — supplied by a post-pass
//! heal, NOT by a migration entry.
//!
//! **Why there is no migration.** A new entry appended to [`MIGRATION_SQL`]
//! bumps `Database::MIGRATION_COUNT`, and `needs_pre_migration_snapshot`
//! returns `true` for every profile whose `user_version` sits below the new
//! count. That re-arms the pre-migration `VACUUM INTO` — measured 331 s / 1.31
//! GB on the ops profile behind a 90 s CLI cap (#1034). An index is additive
//! and preserves every row, so it needs no snapshot at all: the heal is a
//! `sqlite_master` probe on a healthy boot and a `CREATE INDEX` only where one
//! is missing.
//!
//! `no_migration_carries_the_indexes` pins the first half of that decision so a
//! later merge cannot quietly re-add the entry and re-arm the snapshot; the
//! rest pins the heal's contract (create what is missing, report it, and be a
//! no-op on the second call).

use crate::db::Database;
use crate::db::database::MIGRATION_SQL;
use crate::db::migration_heal::{has_index, heal_usage_ledger_indexes};

/// #1076: the `usage_ledger` indexes may not come from a migration.
#[test]
fn no_migration_carries_the_indexes() {
    for marker in [
        "idx_usage_ledger_created",
        "idx_usage_ledger_session",
        "idx_usage_ledger_provider",
    ] {
        assert!(
            !MIGRATION_SQL.iter().any(|sql| sql.contains(marker)),
            "no migration may create {marker}: a new index raises MIGRATION_COUNT and \
             re-arms the pre-migration snapshot on every profile (#1076, #1034)"
        );
    }
}

async fn index_present(db: &Database, index: &str) -> bool {
    let name = index.to_string();
    db.pool()
        .get()
        .await
        .unwrap()
        .interact(move |conn| has_index(conn, &name))
        .await
        .unwrap()
        .unwrap()
}

async fn drop_index(db: &Database, index: &str) {
    let sql = format!("DROP INDEX IF EXISTS {index};");
    db.pool()
        .get()
        .await
        .unwrap()
        .interact(move |conn| conn.execute_batch(&sql))
        .await
        .unwrap()
        .unwrap();
}

async fn run_usage_heal(db: &Database) -> bool {
    db.pool()
        .get()
        .await
        .unwrap()
        .interact(|conn| heal_usage_ledger_indexes(conn))
        .await
        .unwrap()
        .unwrap()
}

/// A fresh database gains all three `usage_ledger` indexes from
/// `run_migrations` (the heal is in the post-pass block), and a second call is
/// a no-op that reports nothing created.
#[tokio::test]
async fn usage_ledger_indexes_exist_after_migrations_and_the_heal_is_idempotent() {
    let db = Database::connect_in_memory().await.unwrap();
    db.run_migrations().await.unwrap();

    for index in [
        "idx_usage_ledger_created",
        "idx_usage_ledger_session",
        "idx_usage_ledger_provider",
    ] {
        assert!(
            index_present(&db, index).await,
            "{index} must exist after run_migrations (the post-pass heal supplies it)"
        );
    }

    // Second call: nothing to create, so `false`.
    assert!(
        !run_usage_heal(&db).await,
        "a healthy database must make the heal a no-op"
    );

    // And a database that lost ONE index regains exactly that one, then reports
    // no-op again — the per-index probe, not one batch that would report
    // "nothing created" for the two still present.
    drop_index(&db, "idx_usage_ledger_provider").await;
    assert!(!index_present(&db, "idx_usage_ledger_provider").await);
    assert!(
        run_usage_heal(&db).await,
        "the heal must report true when it re-created a missing index"
    );
    assert!(index_present(&db, "idx_usage_ledger_provider").await);
    assert!(
        !run_usage_heal(&db).await,
        "second run over the same schema must be a no-op"
    );
}

/// A database whose table does not exist yet (an old image mid-migration) is
/// left alone rather than erroring: the heal early-outs on `has_table`.
#[tokio::test]
async fn heal_is_a_no_op_when_the_table_is_absent() {
    let db = Database::connect_in_memory().await.unwrap();
    // No `run_migrations`: the schema is empty.
    assert!(
        !run_usage_heal(&db).await,
        "no usage_ledger table means nothing to index"
    );
}
