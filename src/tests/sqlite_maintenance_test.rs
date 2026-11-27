//! Concurrency and safety tests for SQLite maintenance (#273, #298).

use crate::db::{Database, MaintenanceKnobs, WAL_JOURNAL_SIZE_LIMIT, execute_safe_maintenance};
use rusqlite::Connection;
use std::time::{Duration, Instant};
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

/// Serializes the tests that drive the PROCESS-WIDE blocked-TRUNCATE streak.
///
/// `WAL_TRUNCATE_BLOCKED_STREAK` is one global, and nextest runs the tests in a
/// single process on several threads. A sweep that completes RESETS it to 0, so
/// a sibling's successful sweep landing between this test's increments and its
/// assertion would zero the counter under it. Any test that calls
/// `execute_safe_maintenance` with a low `wal_truncate_min_bytes` can reach the
/// TRUNCATE path and must hold this lock.
///
/// `tokio::sync::Mutex`, not `std::sync::Mutex`: a std guard held across the
/// `.await` points below is `clippy::await_holding_lock`, which this gate
/// promotes to a build failure with `-D warnings`.
static WAL_STREAK_LOCK: std::sync::LazyLock<tokio::sync::Mutex<()>> =
    std::sync::LazyLock::new(|| tokio::sync::Mutex::new(()));

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
    let _streak = WAL_STREAK_LOCK.lock().await;
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

// ─── #354: the WAL sidecar is capped per connection, not reclaimed per sweep ──

/// #354 step 6(a): the cap must be on **every** pooled connection.
///
/// `PRAGMA journal_size_limit` is per-connection, so reading it back from a
/// single connection passes even when only that one carries it — the trap the
/// scratch probe found (`set on A` → read back on a new B → `-1`). Two
/// connections are therefore held SIMULTANEOUSLY, which forces the pool to
/// hand out two distinct ones.
#[tokio::test]
async fn test_pragmas_set_journal_size_limit() {
    let dir = tempdir().expect("tempdir");
    let db = Database::connect(dir.path().join("jsl.db"))
        .await
        .expect("connect");

    let first = db.pool().get().await.expect("first connection");
    let second = db.pool().get().await.expect("second connection");

    for (label, conn) in [("first", &first), ("second", &second)] {
        let limit = conn
            .interact(|conn| {
                conn.query_row("PRAGMA journal_size_limit;", [], |row| row.get::<_, i64>(0))
            })
            .await
            .expect("interact")
            .expect("read journal_size_limit");
        assert_eq!(
            limit, WAL_JOURNAL_SIZE_LIMIT as i64,
            "{label} pooled connection must carry the cap: the pragma is per-connection, \
             so one read-back is not evidence that every connection is covered"
        );
    }
}

/// #354 step 6(b): the cap — not the checkpoint, and not a `VACUUM` — is what
/// bounds the sidecar.
///
/// A 4 MiB limit is deliberate: the mechanism is size-independent and a 64 MiB
/// fixture is ~20 000 inserts, i.e. a CI timeout rather than a test. Test (a)
/// is what pins the production constant.
///
/// Both halves are asserted. The end state alone passes against a `VACUUM`-based
/// fix, which is the behaviour this change replaces; "unchanged after PASSIVE"
/// is what distinguishes `journal_size_limit` from it.
#[tokio::test]
async fn test_passive_checkpoint_caps_wal_at_journal_size_limit() {
    const TEST_LIMIT: u64 = 4 * 1024 * 1024;

    let dir = tempdir().expect("tempdir");
    let (conn, wal_path) = seed_wal_backlog(&dir, "test_wal_cap.db");
    conn.execute_batch(&format!("PRAGMA journal_size_limit = {TEST_LIMIT};"))
        .expect("set the test cap");

    let payload = "x".repeat(4096);
    for _ in 0..600 {
        conn.execute("INSERT INTO test (val) VALUES (?1)", [&payload])
            .expect("insert");
    }
    let grown = wal_len(&wal_path);
    assert!(
        grown > TEST_LIMIT,
        "fixture must grow the sidecar past the cap: {grown} <= {TEST_LIMIT}"
    );

    conn.execute_batch("PRAGMA wal_checkpoint(PASSIVE);")
        .expect("passive checkpoint");
    let after_checkpoint = wal_len(&wal_path);
    assert_eq!(
        after_checkpoint, grown,
        "a PASSIVE checkpoint alone must not shrink the file — the trim lands on \
         the next WRITE, not on the checkpoint ({grown} -> {after_checkpoint})"
    );

    conn.execute("INSERT INTO test (val) VALUES ('after')", [])
        .expect("write after the checkpoint");
    let after_write = wal_len(&wal_path);
    assert!(
        after_write <= TEST_LIMIT,
        "the next write must truncate to the cap: {after_write} > {TEST_LIMIT}"
    );
}

/// Open a second connection on `db_path` and pin a read snapshot below the
/// current WAL frame — the state that makes a `TRUNCATE` checkpoint fail (#354).
fn pin_straggler(db_path: &std::path::Path) -> Connection {
    let straggler = Connection::open(db_path).expect("open straggler");
    straggler.execute_batch("BEGIN").expect("begin read txn");
    let _pinned: i64 = straggler
        .query_row("SELECT count(*) FROM test", [], |row| row.get(0))
        .expect("pin snapshot");
    straggler
}

/// Consecutive blocked `TRUNCATE` attempts, read through the test-only accessor.
fn blocked_streak() -> u32 {
    crate::db::database::wal_truncate_blocked_streak()
}

/// #354 steps 6(c) and 4b: a doomed `TRUNCATE` costs the short timeout, the
/// 30 s default is restored, and a streak of blocked attempts is counted and
/// then reset by a completed checkpoint.
///
/// A pinned straggler reader is what makes the `TRUNCATE` fail: it holds a
/// snapshot below the current WAL frame, and waiting does not help — so the
/// full default only delays the sweep's other legs. Measured 2.0-3.4 s at the
/// 2 s timeout against 30.2 s at the default, so 10 s is a 3x discriminator
/// with CI headroom.
///
/// ONE test, deliberately: timing and streak describe the SAME blocked sweep, so
/// splitting them would double the 2 s cost and let a sibling's successful
/// checkpoint zero the counter between this test's increments and its assertion.
/// It holds `WAL_STREAK_LOCK` because `test_execute_safe_maintenance_truncates_large_wal`
/// drives a sweep that COMPLETES and therefore resets the same process-wide
/// global; a `std::sync::Mutex` guard held across `.await` would instead be
/// `clippy::await_holding_lock`, which this gate promotes to a build failure.
#[tokio::test]
async fn test_truncate_uses_short_busy_timeout_and_counts_blocked_streak() {
    let _streak = WAL_STREAK_LOCK.lock().await;
    let dir = tempdir().expect("tempdir");
    let db_path = dir.path().join("test_wal_busy.db");
    let (conn, _wal_path) = seed_wal_backlog(&dir, "test_wal_busy.db");
    let knobs = MaintenanceKnobs {
        min_freelist_pages: i64::MAX,
        wal_truncate_min_bytes: 4096,
        ..Default::default()
    };

    // The production pool runs with a 30 s busy timeout; pin the fixture to the
    // same value so the timing assertion discriminates against 30 s rather than
    // against whatever `Connection::open` defaults to.
    conn.execute_batch("PRAGMA busy_timeout = 30000;")
        .expect("pin the fixture busy timeout");

    // Baseline: a sweep with no straggler completes its checkpoint, so the
    // process-wide counter starts from zero.
    let vacuumed = execute_safe_maintenance(&conn, "test_wal_busy.db", knobs).expect("sweep");
    assert!(!vacuumed, "the reclaim must stay skipped (no VACUUM)");
    assert_eq!(
        blocked_streak(),
        0,
        "a completed checkpoint must reset the blocked streak"
    );

    let straggler = pin_straggler(&db_path);
    // Frames above the pinned snapshot, so the TRUNCATE has something it
    // cannot discard.
    let payload = "x".repeat(4096);
    for _ in 0..16 {
        conn.execute("INSERT INTO test (val) VALUES (?1)", [&payload])
            .expect("write above the pin");
    }

    for attempt in 1u32..=3 {
        let started = Instant::now();
        let vacuumed =
            execute_safe_maintenance(&conn, "test_wal_busy.db", knobs).expect("blocked sweep");
        let elapsed = started.elapsed();

        assert!(!vacuumed, "the reclaim must stay skipped (no VACUUM)");
        assert!(
            elapsed < Duration::from_secs(10),
            "attempt {attempt}: a doomed TRUNCATE must not pay the 30 s default, took {elapsed:?}"
        );
        let busy_timeout: i64 = conn
            .query_row("PRAGMA busy_timeout;", [], |row| row.get(0))
            .expect("read busy_timeout");
        assert_eq!(
            busy_timeout, 30000,
            "attempt {attempt}: the 30 s busy timeout must be restored"
        );
        assert_eq!(
            blocked_streak(),
            attempt,
            "attempt {attempt}: every blocked TRUNCATE must count toward the streak"
        );
    }

    straggler.execute_batch("COMMIT").expect("release the pin");
    drop(straggler);

    let vacuumed = execute_safe_maintenance(&conn, "test_wal_busy.db", knobs).expect("sweep");
    assert!(!vacuumed, "the reclaim must stay skipped (no VACUUM)");
    assert_eq!(
        blocked_streak(),
        0,
        "an unblocked checkpoint must reset the streak to 0"
    );
}
