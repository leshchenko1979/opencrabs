use crate::db::retry::*;
use std::time::Duration;

#[test]
fn test_retry_config_defaults() {
    let config = DbRetryConfig::default();
    assert_eq!(config.max_attempts, 5);
    assert_eq!(config.initial_delay, Duration::from_millis(50));
    assert_eq!(config.max_delay, Duration::from_secs(5));
}

#[test]
fn test_retry_config_aggressive() {
    let config = DbRetryConfig::aggressive();
    assert_eq!(config.max_attempts, 10);
    assert_eq!(config.initial_delay, Duration::from_millis(100));
}

#[test]
fn test_calculate_delay() {
    let config = DbRetryConfig {
        initial_delay: Duration::from_millis(50),
        max_delay: Duration::from_secs(5),
        backoff_multiplier: 2.0,
        max_attempts: 5,
    };

    let delay0 = config.calculate_delay(0);
    assert_eq!(delay0, Duration::from_millis(50));

    let delay1 = config.calculate_delay(1);
    assert_eq!(delay1, Duration::from_millis(100));

    let delay2 = config.calculate_delay(2);
    assert_eq!(delay2, Duration::from_millis(200));

    // Should cap at max_delay
    let delay10 = config.calculate_delay(10);
    assert_eq!(delay10, Duration::from_secs(5));
}

#[test]
fn test_is_database_locked() {
    // Non-lock error should return false
    let err = rusqlite::Error::QueryReturnedNoRows;
    assert!(!is_database_locked(&err));
}

#[tokio::test]
async fn test_retry_success_immediate() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};

    let config = DbRetryConfig::default();
    let call_count = Arc::new(AtomicU32::new(0));
    let call_count_clone = call_count.clone();

    let result = retry_db_operation(
        move || {
            let count = call_count_clone.clone();
            async move {
                count.fetch_add(1, Ordering::SeqCst);
                Ok::<_, String>(42)
            }
        },
        &config,
    )
    .await;

    assert!(result.is_ok());
    assert_eq!(result.unwrap(), 42);
    assert_eq!(call_count.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn test_retry_success_after_retries() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};

    let config = DbRetryConfig::new(3, Duration::from_millis(10));
    let call_count = Arc::new(AtomicU32::new(0));
    let call_count_clone = call_count.clone();

    let result = retry_db_operation(
        move || {
            let count = call_count_clone.clone();
            async move {
                let current = count.fetch_add(1, Ordering::SeqCst) + 1;
                if current < 3 {
                    Err("database is locked".to_string())
                } else {
                    Ok::<_, String>(42)
                }
            }
        },
        &config,
    )
    .await;

    assert!(result.is_ok());
    assert_eq!(result.unwrap(), 42);
    assert_eq!(call_count.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn test_retry_max_attempts_exceeded() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};

    let config = DbRetryConfig::new(2, Duration::from_millis(10));
    let call_count = Arc::new(AtomicU32::new(0));
    let call_count_clone = call_count.clone();

    let result = retry_db_operation(
        move || {
            let count = call_count_clone.clone();
            async move {
                count.fetch_add(1, Ordering::SeqCst);
                Err::<i32, _>("database is locked".to_string())
            }
        },
        &config,
    )
    .await;

    assert!(result.is_err());
    assert_eq!(call_count.load(Ordering::SeqCst), 3); // Initial + 2 retries
}

#[tokio::test]
async fn test_retry_non_retryable_error() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};

    let config = DbRetryConfig::default();
    let call_count = Arc::new(AtomicU32::new(0));
    let call_count_clone = call_count.clone();

    let result = retry_db_operation(
        move || {
            let count = call_count_clone.clone();
            async move {
                count.fetch_add(1, Ordering::SeqCst);
                Err::<i32, _>("constraint violation".to_string())
            }
        },
        &config,
    )
    .await;

    assert!(result.is_err());
    assert_eq!(call_count.load(Ordering::SeqCst), 1); // Should not retry
}

// ---------------------------------------------------------------------------
// #321 Part B — the retryability decision must see through `.context(..)`.
//
// The defect: `retry_db_anyhow` inherited `retry_db_operation`'s textual
// test, which lowercases `E::to_string()`. For `anyhow::Error` that string is
// ONLY the outermost `.context(..)` frame, so a `rusqlite` SQLITE_BUSY buried
// underneath never matched `"locked"`/`"busy"` — the operation was declared
// non-retryable and given up on the first attempt.
//
// These tests use a GENUINE SQLITE_BUSY produced by two real connections on
// one temp file database (not a hand-made string), wrapped in `.context(..)`
// exactly as the repository methods wrap it.
// ---------------------------------------------------------------------------

/// A temp file DB with a live write lock held by a second connection, so a
/// fresh `SQLITE_BUSY` can be produced on demand — one error per call, since
/// `anyhow::Error` is not `Clone`.
struct LockFixture {
    path: std::path::PathBuf,
    /// Must outlive the fixture: dropping it releases the write lock.
    _holder: rusqlite::Connection,
    /// Must outlive the fixture: dropping it deletes the database file.
    _dir: tempfile::TempDir,
}

impl LockFixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("retry_lock.db");

        let holder = rusqlite::Connection::open(&path).expect("open holder");
        holder
            .execute_batch(
                "PRAGMA journal_mode=WAL;
                 CREATE TABLE t (v INTEGER);
                 BEGIN IMMEDIATE;
                 INSERT INTO t VALUES (1);",
            )
            .expect("holder takes the write lock");

        Self {
            path,
            _holder: holder,
            _dir: dir,
        }
    }

    /// A fresh genuine `SQLITE_BUSY`, wrapped in `.context(..)` the way the
    /// repository methods wrap it.
    fn wrapped_lock_error(&self) -> anyhow::Error {
        use anyhow::Context;

        let contender = rusqlite::Connection::open(&self.path).expect("open contender");
        // No patience, so the busy error surfaces immediately.
        contender
            .busy_timeout(Duration::from_millis(0))
            .expect("busy_timeout");
        let raw = contender
            .execute("INSERT INTO t VALUES (2)", [])
            .expect_err("contender must hit the lock");

        Err::<(), rusqlite::Error>(raw)
            .context("Failed to record feedback")
            .expect_err("the wrapped error is the subject of this fixture")
    }
}

#[test]
fn the_textual_predicate_is_blind_to_a_wrapped_lock() {
    let fixture = LockFixture::new();
    let err = fixture.wrapped_lock_error();

    // The old decision, unchanged: the rendered message carries only the
    // context frame, so the textual test returns false — the #321 defect.
    assert!(
        !display_says_locked(&err),
        "the textual predicate must be blind to a wrapped lock, else this \
         fixture is not exercising the defect; rendered: {err}"
    );

    // The chain-aware replacement sees the typed cause.
    assert!(
        is_busy_anyhow(&err),
        "the chain-aware predicate must classify a wrapped SQLITE_BUSY as retryable; \
         chain: {:#}",
        err
    );
}

#[tokio::test]
async fn the_old_textual_predicate_gives_up_where_the_chain_aware_one_retries() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};

    let fixture = LockFixture::new();
    let config = DbRetryConfig::new(3, Duration::from_millis(5));

    // Half 1 — the OLD predicate: one attempt, then "not retryable".
    let old_calls = Arc::new(AtomicU32::new(0));
    let old_result = {
        let calls = old_calls.clone();
        let fixture = &fixture;
        retry_db_with(
            move || {
                let calls = calls.clone();
                async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    Err::<i32, anyhow::Error>(fixture.wrapped_lock_error())
                }
            },
            &config,
            display_says_locked::<anyhow::Error>,
        )
        .await
    };
    assert!(old_result.is_err());
    assert_eq!(
        old_calls.load(Ordering::SeqCst),
        1,
        "the textual predicate must give up on the first attempt — this is the defect"
    );

    // Half 2 — the NEW predicate: same loop, same operation, same fixture.
    // max_attempts=3 means the initial call plus 3 retries.
    let new_calls = Arc::new(AtomicU32::new(0));
    let new_result = {
        let calls = new_calls.clone();
        let fixture = &fixture;
        retry_db_with(
            move || {
                let calls = calls.clone();
                async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    Err::<i32, anyhow::Error>(fixture.wrapped_lock_error())
                }
            },
            &config,
            is_busy_anyhow,
        )
        .await
    };
    assert!(new_result.is_err());
    assert_eq!(
        new_calls.load(Ordering::SeqCst),
        4,
        "the chain-aware predicate must retry the wrapped lock, not abandon it"
    );
}

#[tokio::test]
async fn retry_db_anyhow_retries_a_context_wrapped_lock() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};

    let fixture = LockFixture::new();
    let config = DbRetryConfig::new(3, Duration::from_millis(5));
    let calls = Arc::new(AtomicU32::new(0));

    let result: anyhow::Result<i32> = {
        let calls = calls.clone();
        let fixture = &fixture;
        retry_db_anyhow(
            move || {
                let calls = calls.clone();
                async move {
                    let attempt = calls.fetch_add(1, Ordering::SeqCst) + 1;
                    if attempt < 3 {
                        Err(fixture.wrapped_lock_error())
                    } else {
                        Ok(42)
                    }
                }
            },
            &config,
        )
        .await
    };

    assert_eq!(
        result.expect("the lock must be retried through, not surfaced"),
        42
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        3,
        "retry_db_anyhow must actually re-invoke the wrapped operation"
    );
}

#[tokio::test]
async fn retry_db_anyhow_still_surfaces_a_genuinely_non_retryable_error() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};

    let config = DbRetryConfig::new(3, Duration::from_millis(5));
    let calls = Arc::new(AtomicU32::new(0));

    let result: anyhow::Result<i32> = {
        let calls = calls.clone();
        retry_db_anyhow(
            move || {
                let calls = calls.clone();
                async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    Err(anyhow::anyhow!("no such table: t"))
                }
            },
            &config,
        )
        .await
    };

    assert!(result.is_err());
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "a non-lock error must not be retried"
    );
    // The context frame survives, and the cause is still in the chain.
    let rendered = format!("{:#}", result.unwrap_err());
    assert!(
        rendered.contains("Database operation failed after retries"),
        "the retry context must be present; got: {rendered}"
    );
    assert!(
        rendered.contains("no such table"),
        "the cause must remain in the chain; got: {rendered}"
    );
}
