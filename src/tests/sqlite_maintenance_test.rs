//! Concurrency and safety tests for SQLite maintenance (#273).

use crate::db::{Database, execute_safe_maintenance};
use rusqlite::Connection;
use tempfile::tempdir;

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
    let vacuumed = execute_safe_maintenance(&conn, "test.db", 1024).expect("maintenance");
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
    let vacuumed = execute_safe_maintenance(&conn, "test_trigger.db", 0).expect("maintenance");
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
