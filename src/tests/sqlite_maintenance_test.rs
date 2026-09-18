//! Concurrency and safety tests for SQLite maintenance (#273, #298).

use crate::db::{Database, WAL_TRUNCATE_MIN_BYTES, execute_safe_maintenance};
use rusqlite::Connection;
use tempfile::tempdir;

/// Open a WAL-mode database with auto-checkpoint disabled and insert enough
/// rows to grow the `-wal` sidecar well past a few pages.
///
/// Returns the connection and the `-wal` path. Auto-checkpoint is disabled so
/// the sidecar actually accumulates frames instead of being folded back into
/// the main file at SQLite's default 1000-page watermark.
fn seed_wal_backlog(dir: &tempfile::TempDir, stem: &str) -> (Connection, std::path::PathBuf) {
    let db_path = dir.path().join(stem);
    let wal_path = dir.path().join(format!("{stem}-wal"));
    let conn = Connection::open(&db_path).expect("open connection");

    conn.execute_batch(
        "PRAGMA page_size = 4096;
         PRAGMA journal_mode = WAL;
         PRAGMA wal_autocheckpoint = 0;
         CREATE TABLE test (id INTEGER PRIMARY KEY, val TEXT);",
    )
    .expect("setup table");

    let payload = "x".repeat(4096);
    for _ in 0..200 {
        conn.execute("INSERT INTO test (val) VALUES (?1)", [&payload])
            .expect("insert");
    }

    (conn, wal_path)
}

fn wal_len(wal_path: &std::path::Path) -> u64 {
    std::fs::metadata(wal_path).map(|m| m.len()).unwrap_or(0)
}

#[tokio::test]
async fn test_execute_safe_maintenance_freelist_skip() {
    let dir = tempdir().expect("tempdir");
    let db_path = dir.path().join("test.db");
    let conn = Connection::open(&db_path).expect("open connection");

    conn.execute_batch(
        "PRAGMA journal_mode = WAL;
         CREATE TABLE test (id INTEGER PRIMARY KEY, val TEXT);
         INSERT INTO test (val) VALUES ('hello');",
    )
    .expect("setup table");

    // With min_freelist_pages = 1024, an unfragmented DB skips VACUUM
    let vacuumed = execute_safe_maintenance(&conn, "test.db", 1024, WAL_TRUNCATE_MIN_BYTES)
        .expect("maintenance");
    assert!(!vacuumed, "Expected vacuum to skip on low freelist count");
}

#[tokio::test]
async fn test_execute_safe_maintenance_freelist_trigger() {
    let dir = tempdir().expect("tempdir");
    let db_path = dir.path().join("test_trigger.db");
    let conn = Connection::open(&db_path).expect("open connection");

    conn.execute_batch(
        "PRAGMA journal_mode = WAL;
         PRAGMA page_size = 4096;
         CREATE TABLE test (id INTEGER PRIMARY KEY, val TEXT);",
    )
    .expect("setup table");

    // Insert and delete rows to generate freelist pages
    for i in 0..500 {
        conn.execute(
            "INSERT INTO test (val) VALUES (?1)",
            [format!("large data payload string number {i}")],
        )
        .expect("insert");
    }
    conn.execute("DELETE FROM test WHERE id > 10", [])
        .expect("delete");

    // Setting min_freelist_pages = 0 forces vacuum attempt
    let vacuumed = execute_safe_maintenance(&conn, "test_trigger.db", 0, WAL_TRUNCATE_MIN_BYTES)
        .expect("maintenance");
    assert!(
        vacuumed,
        "Expected vacuum to execute when freelist threshold is 0"
    );
}

#[tokio::test]
async fn test_database_vacuum_database_integration() {
    let dir = tempdir().expect("tempdir");
    let db_path = dir.path().join("opencrabs_test.db");

    let db = Database::connect(&db_path).await.expect("connect");
    let vacuumed = db.vacuum_database().await.expect("vacuum_database");
    // New database has no freelist pages, so vacuum is skipped
    assert!(!vacuumed);
}

/// Regression (#298): a `-wal` sidecar past the threshold must be truncated by
/// the maintenance sweep, not left at its all-time high-water mark.
#[tokio::test]
async fn test_execute_safe_maintenance_truncates_large_wal() {
    let dir = tempdir().expect("tempdir");
    let (conn, wal_path) = seed_wal_backlog(&dir, "test_wal_truncate.db");

    let wal_before = wal_len(&wal_path);
    assert!(
        wal_before > 4096,
        "expected a WAL past the threshold, got {wal_before} bytes"
    );

    // min_freelist_pages = i64::MAX skips VACUUM so this isolates the WAL pass.
    execute_safe_maintenance(&conn, "test_wal_truncate.db", i64::MAX, 4096).expect("maintenance");

    let wal_after = wal_len(&wal_path);
    assert!(
        wal_after < wal_before,
        "expected the WAL to shrink from {wal_before} bytes, still {wal_after}"
    );
}

/// The size guard must leave a healthy (small) WAL alone (#298): a threshold
/// above the observed sidecar size skips the blocking TRUNCATE checkpoint.
#[tokio::test]
async fn test_execute_safe_maintenance_skips_small_wal() {
    let dir = tempdir().expect("tempdir");
    let (conn, wal_path) = seed_wal_backlog(&dir, "test_wal_skip.db");

    let wal_before = wal_len(&wal_path);
    assert!(wal_before > 0, "expected a non-empty WAL sidecar");

    execute_safe_maintenance(&conn, "test_wal_skip.db", i64::MAX, u64::MAX).expect("maintenance");

    let wal_after = wal_len(&wal_path);
    assert!(
        wal_after >= wal_before,
        "expected the size guard to skip truncation, WAL went {wal_before} -> {wal_after}"
    );
}
