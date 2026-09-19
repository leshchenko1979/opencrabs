//! Concurrency and safety tests for SQLite maintenance (#273, #298).

use crate::db::{Database, MaintenanceKnobs, execute_safe_maintenance};
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
    let vacuumed = execute_safe_maintenance(&conn, "test.db", MaintenanceKnobs::default())
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
    let vacuumed = execute_safe_maintenance(
        &conn,
        "test_trigger.db",
        MaintenanceKnobs {
            min_freelist_pages: 0,
            ..Default::default()
        },
    )
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
    execute_safe_maintenance(
        &conn,
        "test_wal_truncate.db",
        MaintenanceKnobs {
            min_freelist_pages: i64::MAX,
            wal_truncate_min_bytes: 4096,
            ..Default::default()
        },
    )
    .expect("maintenance");

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

    execute_safe_maintenance(
        &conn,
        "test_wal_skip.db",
        MaintenanceKnobs {
            min_freelist_pages: i64::MAX,
            wal_truncate_min_bytes: u64::MAX,
            ..Default::default()
        },
    )
    .expect("maintenance");

    let wal_after = wal_len(&wal_path);
    assert!(
        wal_after >= wal_before,
        "expected the size guard to skip truncation, WAL went {wal_before} -> {wal_after}"
    );
}

/// #321 Part A1: one maintenance sweep converts a file database to incremental
/// auto-vacuum, which is what lets the reclaim step below it be bounded.
#[tokio::test]
async fn test_maintenance_converts_to_incremental_autovacuum() {
    let dir = tempdir().expect("tempdir");
    let db_path = dir.path().join("test_autovacuum.db");
    let conn = Connection::open(&db_path).expect("open connection");

    conn.execute_batch(
        "PRAGMA journal_mode = WAL;
         CREATE TABLE test (id INTEGER PRIMARY KEY, val TEXT);
         INSERT INTO test (val) VALUES ('hello');",
    )
    .expect("setup table");

    // Deterministic fixture: pin the file to NONE whatever the build's
    // SQLITE_DEFAULT_AUTOVACUUM is, so the SWEEP is what must make the change.
    conn.execute_batch("PRAGMA auto_vacuum = NONE; VACUUM;")
        .expect("pin fixture to auto_vacuum=NONE");

    let before: i64 = conn
        .query_row("PRAGMA auto_vacuum;", [], |row| row.get(0))
        .expect("read auto_vacuum before");
    assert_eq!(before, 0, "fixture must start at NONE");

    execute_safe_maintenance(&conn, "test_autovacuum.db", MaintenanceKnobs::default())
        .expect("first sweep");

    let after: i64 = conn
        .query_row("PRAGMA auto_vacuum;", [], |row| row.get(0))
        .expect("read auto_vacuum after");
    assert_eq!(after, 2, "expected INCREMENTAL auto-vacuum after one sweep");

    // The conversion is one-time: a second sweep leaves it converted.
    execute_safe_maintenance(&conn, "test_autovacuum.db", MaintenanceKnobs::default())
        .expect("second sweep");

    let after_second: i64 = conn
        .query_row("PRAGMA auto_vacuum;", [], |row| row.get(0))
        .expect("read auto_vacuum after second sweep");
    assert_eq!(after_second, 2, "conversion must be one-time, not re-run");
}

/// #321 Part A2: the reclaim is bounded by the per-sweep budget — it frees
/// exactly `reclaim_pages_per_sweep` pages and never the whole freelist.
#[tokio::test]
async fn test_maintenance_reclaim_is_bounded_by_budget() {
    const BUDGET: i64 = 100;

    let dir = tempdir().expect("tempdir");
    let db_path = dir.path().join("test_bounded_reclaim.db");
    let conn = Connection::open(&db_path).expect("open connection");

    // Convert before any table exists, so the fixture is already INCREMENTAL
    // and the sweep's own conversion step (exercised separately) is a no-op.
    conn.execute_batch(
        "PRAGMA page_size = 4096;
         PRAGMA journal_mode = WAL;
         PRAGMA auto_vacuum = INCREMENTAL;
         VACUUM;
         CREATE TABLE bloat (id INTEGER PRIMARY KEY, val TEXT);",
    )
    .expect("setup");

    // Allocate far more than the budget in pages, then free every one of them.
    let payload = "x".repeat(4000);
    for _ in 0..600 {
        conn.execute("INSERT INTO bloat (val) VALUES (?1)", [&payload])
            .expect("insert");
    }
    conn.execute("DELETE FROM bloat", []).expect("delete");

    let freelist_before: i64 = conn
        .query_row("PRAGMA freelist_count;", [], |row| row.get(0))
        .expect("freelist before");
    assert!(
        freelist_before > BUDGET * 2,
        "fixture must leave the reclaim limited by the budget, not the freelist: {freelist_before}"
    );

    execute_safe_maintenance(
        &conn,
        "test_bounded_reclaim.db",
        MaintenanceKnobs {
            min_freelist_pages: 0,
            reclaim_pages_per_sweep: BUDGET,
            ..Default::default()
        },
    )
    .expect("maintenance");

    let freelist_after: i64 = conn
        .query_row("PRAGMA freelist_count;", [], |row| row.get(0))
        .expect("freelist after");

    assert_eq!(
        freelist_before - freelist_after,
        BUDGET,
        "reclaim must be capped at the budget: freelist {freelist_before} -> {freelist_after}"
    );
}
