//! #394 — the two hot write paths #321's sweep missed must route through
//! `write_with_retry`.
//!
//! Both sites previously called `.interact(..)` directly, so a contended write
//! got ONE attempt and surfaced `database is locked` immediately. The fix
//! routes them through the same helper the other five repositories already
//! use.
//!
//! The discriminator is a GENUINE `SQLITE_BUSY`, produced the way
//! `db_retry_test.rs`'s `LockFixture` produces one: a second connection holds
//! the write lock on the SAME file. The repository's pool is pinned to
//! `busy_timeout = 0` here — deliberately, because the production pool carries
//! `busy_timeout = 30000` (`apply_pragmas`, database.rs), which would absorb
//! the contention inside SQLite for 30 s and leave the retry helper
//! structurally unreachable from a test.
//!
//! Two observables the fix introduces, both asserted below:
//!   * the error chain carries `write_with_retry`'s
//!     "Database operation failed after retries" frame, and
//!   * elapsed wall time shows the backoff actually RAN (>= the 250 ms initial
//!     delay), rather than the old path's single immediate attempt. The frame
//!     alone is not enough: `retry_db_anyhow` adds it even on a non-retryable
//!     single attempt, so only the elapsed time proves the retry loop engaged.
//!
//! Each test then releases the lock and asserts the write LANDS — the
//! contention must be survived, not merely reported.

use crate::brain::agent::{BgTaskMeta, PushOrigin};
use crate::db::Database;
use crate::db::repository::{NotifyQueueRepository, SessionSkillsRepository};
use deadpool_sqlite::{Config as PoolConfig, Hook, Runtime};
use std::time::{Duration, Instant};
use uuid::Uuid;

/// The frame `write_with_retry` adds, absent from the unwrapped path.
const RETRY_CONTEXT: &str = "Database operation failed after retries";

/// A temp-file database carrying the real schema, plus the means to hold its
/// write lock out of band.
struct ContendedDb {
    path: std::path::PathBuf,
    /// Must outlive the holder: dropping it deletes the database file.
    _dir: tempfile::TempDir,
}

impl ContendedDb {
    async fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("hot_write_paths.db");
        {
            // Build the schema with the production constructor, then drop it —
            // the file and its migrations outlive the connection.
            let db = Database::connect(&path).await.expect("connect");
            db.run_migrations().await.expect("migrations");
        }
        Self { path, _dir: dir }
    }

    /// A single-connection pool over the same file with `busy_timeout = 0`, so
    /// a held write lock fails FAST instead of queuing behind SQLite.
    fn fail_fast_pool(&self) -> crate::db::Pool {
        PoolConfig::new(self.path.to_string_lossy().as_ref())
            .builder(Runtime::Tokio1)
            .expect("pool config")
            .max_size(1)
            .post_create(Hook::async_fn(|conn, _| {
                Box::pin(async move {
                    conn.interact(|conn| conn.execute_batch("PRAGMA busy_timeout = 0;"))
                        .await
                        .map_err(|e| deadpool_sqlite::HookError::Message(e.to_string().into()))?
                        .map_err(|e| {
                            deadpool_sqlite::HookError::Message(e.to_string().into())
                        })?;
                    Ok(())
                })
            }))
            .build()
            .expect("pool")
    }

    /// Take the write lock on the same file and keep it until dropped.
    fn hold_write_lock(&self) -> rusqlite::Connection {
        let holder = rusqlite::Connection::open(&self.path).expect("open holder");
        holder
            .execute_batch("BEGIN IMMEDIATE;")
            .expect("holder takes the write lock");
        holder
    }
}

#[tokio::test]
async fn clear_matching_retries_a_contended_write() {
    let fx = ContendedDb::new().await;
    let repo = NotifyQueueRepository::new(fx.fail_fast_pool());
    let session = Uuid::new_v4();

    // Seed one delivered row for the clear to match.
    let meta = BgTaskMeta {
        success: true,
        label: "cargo test".into(),
        elapsed_secs: 3.0,
        tail: "test result: ok".into(),
    };
    repo.record(
        Uuid::new_v4(),
        session,
        "context body",
        "display line",
        PushOrigin::BackgroundTask,
        Some(&meta),
    )
    .await
    .expect("seed row");

    let holder = fx.hold_write_lock();
    let started = Instant::now();
    let contended = repo.clear_matching(session, "context body", "display line").await;
    let elapsed = started.elapsed();

    let err = contended.expect_err("a held write lock must not be silently absorbed");
    let rendered = format!("{err:#}");
    assert!(
        rendered.contains(RETRY_CONTEXT),
        "clear_matching must route through write_with_retry; chain: {rendered}"
    );
    assert!(
        elapsed >= Duration::from_millis(200),
        "the 250 ms initial backoff must actually run; elapsed {elapsed:?}"
    );

    // Release: the same call must now land, so the contention was SURVIVED.
    drop(holder);
    repo.clear_matching(session, "context body", "display line")
        .await
        .expect("the write must succeed once the lock is released");
    assert!(
        repo.all().await.expect("all").is_empty(),
        "the seeded row must be gone after the clear"
    );
}

#[tokio::test]
async fn session_skills_record_retries_a_contended_write() {
    let fx = ContendedDb::new().await;
    let repo = SessionSkillsRepository::new(fx.fail_fast_pool());
    let session = Uuid::new_v4();

    let holder = fx.hold_write_lock();
    let started = Instant::now();
    let contended = repo.record(session, "opencrabs-dev", 7).await;
    let elapsed = started.elapsed();

    let err = contended.expect_err("a held write lock must not be silently absorbed");
    let rendered = format!("{err:#}");
    assert!(
        rendered.contains(RETRY_CONTEXT),
        "session_skills::record must route through write_with_retry; chain: {rendered}"
    );
    assert!(
        elapsed >= Duration::from_millis(200),
        "the 250 ms initial backoff must actually run; elapsed {elapsed:?}"
    );

    // Release: the stamp must land, so the persistence survived contention.
    drop(holder);
    repo.record(session, "opencrabs-dev", 7)
        .await
        .expect("the write must succeed once the lock is released");
    let rows = repo.all().await.expect("all");
    assert_eq!(rows.len(), 1, "exactly the one recorded stamp");
    assert_eq!(rows[0].1, "opencrabs-dev");
    assert_eq!(rows[0].2, Some(7), "the epoch must be the one recorded");
}
