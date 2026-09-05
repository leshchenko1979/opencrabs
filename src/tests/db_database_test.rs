use crate::db::Database;
use tokio;

#[tokio::test]
async fn test_connect_in_memory() {
    let db = Database::connect_in_memory().await.unwrap();
    assert!(db.is_connected());
}

#[tokio::test]
async fn test_migrations() {
    let db = Database::connect_in_memory().await.unwrap();
    db.run_migrations().await.unwrap();
}

#[tokio::test]
async fn test_migrations_idempotent() {
    let db = Database::connect_in_memory().await.unwrap();
    db.run_migrations().await.unwrap();
    // Running migrations a second time should be a no-op
    db.run_migrations().await.unwrap();

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

/// Simulate upgrading from an sqlx-managed database: the _sqlx_migrations
/// table exists, all tables are already present, and user_version is 0.
/// run_migrations() must detect this and stamp the version without failing.
#[tokio::test]
async fn test_sqlx_upgrade_stamps_user_version() {
    let db = Database::connect_in_memory().await.unwrap();

    // 1. Run migrations normally to create all tables
    db.run_migrations().await.unwrap();

    // 2. Simulate a pre-existing sqlx DB: add _sqlx_migrations and reset user_version
    db.pool
        .get()
        .await
        .unwrap()
        .interact(|conn| {
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS _sqlx_migrations (
                        version INTEGER PRIMARY KEY,
                        description TEXT NOT NULL,
                        installed_on TEXT NOT NULL DEFAULT (datetime('now'))
                    );
                    PRAGMA user_version = 0;",
            )
        })
        .await
        .unwrap()
        .unwrap();

    // 3. run_migrations should detect sqlx and stamp, not fail
    db.run_migrations().await.unwrap();

    // 4. Verify user_version was set to MIGRATION_COUNT
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

/// Fresh database (no _sqlx_migrations, user_version=0) should run all
/// migrations normally and end at MIGRATION_COUNT.
#[tokio::test]
async fn test_fresh_db_runs_all_migrations() {
    let db = Database::connect_in_memory().await.unwrap();

    // Verify starts at 0
    let before: i64 = db
        .pool
        .get()
        .await
        .unwrap()
        .interact(|conn| conn.pragma_query_value(None, "user_version", |r| r.get(0)))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(before, 0);

    db.run_migrations().await.unwrap();

    let after: i64 = db
        .pool
        .get()
        .await
        .unwrap()
        .interact(|conn| conn.pragma_query_value(None, "user_version", |r| r.get(0)))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(after, Database::MIGRATION_COUNT as i64);
}

/// Order contract guard (#26 P2, 2026-09-05): `build_migrations` entries are
/// `user_version` slots — inserting a new migration anywhere but the end
/// renumbers every later migration and corrupts existing deployments' schema
/// versioning (this exact bug shipped in the P2 branch and broke the
/// migration-33 heal tests). The doc comment on `build_migrations` says
/// "never reorder … only append new ones"; this pin enforces it mechanically:
/// the filenames referenced in the list must be strictly sorted and the count
/// must equal `MIGRATION_COUNT`.
#[test]
fn migration_list_order_matches_filename_sort() {
    let source = include_str!("../db/database.rs");
    let mut names: Vec<&str> = Vec::new();
    let mut rest = source;
    while let Some(pos) = rest.find("../migrations/") {
        let start = pos + "../migrations/".len();
        let end = rest[start..]
            .find(".sql")
            .expect("migration reference ends with .sql");
        names.push(&rest[start..start + end + 4]);
        rest = &rest[start + end + 4..];
    }
    assert_eq!(names.len(), Database::MIGRATION_COUNT);
    let mut sorted = names.clone();
    sorted.sort();
    assert_eq!(names, sorted, "build_migrations must append, never reorder");
}
