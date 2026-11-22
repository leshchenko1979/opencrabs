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

/// The count is derived from the list, so the only drift left is a `.sql`
/// on disk that the list forgot (#1354).
#[test]
fn test_every_migration_file_on_disk_is_registered() {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/src/migrations");
    let on_disk = std::fs::read_dir(dir)
        .expect("migrations dir")
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|x| x == "sql"))
        .count();
    assert_eq!(
        on_disk,
        Database::MIGRATION_COUNT,
        "a migration file exists that build_migrations does not include"
    );
}

/// The list is applied by INDEX, so two branches that each append "migration
/// N" and a merge that re-sorts them by date skip one of them on every
/// database already stamped N (#1401: `pending_requests.origin`). The only
/// place that can catch it before a merge is here: the files on disk in name
/// order must be the list, entry for entry.
#[test]
fn test_migration_list_is_the_on_disk_files_in_name_order() {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/src/migrations");
    let mut files: Vec<std::path::PathBuf> = std::fs::read_dir(dir)
        .expect("migrations dir")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "sql"))
        .collect();
    files.sort();
    let list = crate::db::database::MIGRATION_SQL;
    assert_eq!(files.len(), list.len(), "file count and list length differ");
    for (i, path) in files.iter().enumerate() {
        let on_disk = std::fs::read_to_string(path).expect("read migration");
        assert_eq!(
            on_disk,
            list[i],
            "list index {} is not {} : the list is out of name order, so a database stamped \
             past this index has skipped a migration",
            i + 1,
            path.file_name().unwrap().to_string_lossy()
        );
    }
}

/// Index names on `tool_executions`, read from `sqlite_master` after migration.
async fn tool_executions_indexes(db: &Database) -> Vec<String> {
    db.pool()
        .get()
        .await
        .unwrap()
        .interact(|conn| {
            conn.prepare(
                "SELECT name FROM sqlite_master \
                 WHERE type = 'index' AND tbl_name = 'tool_executions'",
            )?
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()
        })
        .await
        .unwrap()
        .unwrap()
}

/// #916: `tool_executions` was indexed on `message_id` only, so the
/// per-session self-read and the `created_at` windows full-scanned a 1.1 GB
/// table. Both indexes existed only as hand-built stopgaps on the live
/// profile, so a REBUILT store started bare — the schema must carry them.
#[tokio::test]
async fn tool_executions_carries_session_and_created_at_indexes() {
    let db = Database::connect_in_memory().await.unwrap();
    db.run_migrations().await.unwrap();

    let indexes = tool_executions_indexes(&db).await;
    for want in [
        "idx_tool_executions_session_id",
        "idx_tool_executions_created_at",
    ] {
        assert!(
            indexes.iter().any(|name| name == want),
            "index {want} missing after migrations; found {indexes:?}"
        );
    }

    // Each is a single-column index on the column it is named for: that is the
    // shape the per-session read and the `created_at` range plan against.
    for (index, column) in [
        ("idx_tool_executions_session_id", "session_id"),
        ("idx_tool_executions_created_at", "created_at"),
    ] {
        let cols: Vec<String> = db
            .pool()
            .get()
            .await
            .unwrap()
            .interact(move |conn| {
                conn.prepare("SELECT name FROM pragma_index_info(?1)")?
                    .query_map([index], |r| r.get::<_, String>(0))?
                    .collect::<rusqlite::Result<Vec<_>>>()
            })
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            cols,
            vec![column.to_owned()],
            "{index} must be an index on ({column})"
        );
    }
}

/// The 1-based version of the migration that creates the `tool_executions`
/// session/created_at indexes, found by its content.
///
/// Deriving this from `Database::MIGRATION_COUNT - 1` assumes the index
/// migration is always LAST, which holds only until the next migration is
/// appended — and then this test fails for a reason that has nothing to do
/// with indexes.
fn index_migration_version() -> usize {
    let pos = crate::db::database::MIGRATION_SQL
        .iter()
        .position(|sql| sql.contains("idx_tool_executions_session_id"))
        .expect("a migration creating idx_tool_executions_session_id must exist");
    pos + 1
}

/// #916: an existing store gains the indexes on its next open. A database
/// stamped at the version just below this migration carries neither of them
/// (that is the live shape with the hand-built copies absent), and
/// `run_migrations` must create them without a rebuild — `IF NOT EXISTS`
/// makes the same statement a no-op where they are already present.
#[tokio::test]
async fn existing_store_gains_tool_executions_indexes_on_next_open() {
    let db = Database::connect_in_memory().await.unwrap();
    // Anchor on the migration's own CONTENT, not on `MIGRATION_COUNT - 1`.
    // The count is the list's length, so appending any later migration moved
    // the "one version below" target off this one and broke an unrelated
    // test (#938's no-op comment migration did exactly that). The index
    // name identifies the migration unambiguously.
    let below = index_migration_version() - 1;
    db.pool()
        .get()
        .await
        .unwrap()
        .interact(move |conn| -> Result<(), String> {
            crate::db::database::build_migrations()
                .to_version(conn, below)
                .map_err(|e| e.to_string())
        })
        .await
        .unwrap()
        .unwrap();

    let before = tool_executions_indexes(&db).await;
    for want in [
        "idx_tool_executions_session_id",
        "idx_tool_executions_created_at",
    ] {
        assert!(
            !before.iter().any(|name| name == want),
            "index {want} must not exist one version below its migration; found {before:?}"
        );
    }

    db.run_migrations().await.unwrap();

    let after = tool_executions_indexes(&db).await;
    for want in [
        "idx_tool_executions_session_id",
        "idx_tool_executions_created_at",
    ] {
        assert!(
            after.iter().any(|name| name == want),
            "index {want} missing after the next open; found {after:?}"
        );
    }
}
