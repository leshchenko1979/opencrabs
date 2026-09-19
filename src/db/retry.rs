//! Database Retry Logic
//!
//! Handles SQLite lock errors and retries with exponential backoff.
//!
//! ## Features
//! - Automatic retry for SQLITE_BUSY errors
//! - Exponential backoff for lock contention
//! - Configurable retry attempts
//! - Logging for debugging lock issues
//!
//! ## #321 — retryability is decided by TYPE, not by rendered text
//!
//! `anyhow::Error`'s `Display` renders ONLY the outermost `.context(..)`
//! frame. A substring test for `"locked"` on that string therefore can never
//! see a `rusqlite::Error::SqliteFailure(DatabaseBusy)` buried one frame
//! down: it returned `false`, and the operation was reported as
//! non-retryable without a single retry. Every entry point below now decides
//! retryability through an explicit predicate instead:
//!
//! | error type | predicate | sees through `.context(..)`? |
//! |---|---|---|
//! | `rusqlite::Error` | [`is_database_locked`] | n/a — already typed |
//! | `anyhow::Error` | [`is_busy_anyhow`] | yes — walks `Error::chain()` |
//! | anything else | [`display_says_locked`] | no — text is all it has |

use anyhow::{Context, Result};
use std::future::Future;
use std::time::Duration;
use tokio::time::sleep;

/// Configuration for database retry logic
#[derive(Debug, Clone)]
pub struct DbRetryConfig {
    /// Maximum number of retry attempts
    pub max_attempts: u32,
    /// Initial delay before first retry
    pub initial_delay: Duration,
    /// Maximum delay between retries
    pub max_delay: Duration,
    /// Backoff multiplier (typically 2.0 for exponential)
    pub backoff_multiplier: f64,
}

impl Default for DbRetryConfig {
    fn default() -> Self {
        Self {
            max_attempts: 5,
            initial_delay: Duration::from_millis(50),
            max_delay: Duration::from_secs(5),
            backoff_multiplier: 2.0,
        }
    }
}

impl DbRetryConfig {
    /// Create a new retry config with custom settings
    pub fn new(max_attempts: u32, initial_delay: Duration) -> Self {
        Self {
            max_attempts,
            initial_delay,
            ..Default::default()
        }
    }

    /// Create config for aggressive retry (for high contention)
    pub fn aggressive() -> Self {
        Self {
            max_attempts: 10,
            initial_delay: Duration::from_millis(100),
            max_delay: Duration::from_secs(10),
            backoff_multiplier: 1.5,
        }
    }

    /// Calculate delay for a given attempt
    pub(crate) fn calculate_delay(&self, attempt: u32) -> Duration {
        let base_delay = self.initial_delay.as_millis() as f64;
        let exponential = base_delay * self.backoff_multiplier.powi(attempt as i32);
        let max_delay_ms = self.max_delay.as_millis() as f64;

        let delay = exponential.min(max_delay_ms);
        Duration::from_millis(delay as u64)
    }
}

/// Check if a rusqlite error is a SQLite busy/locked error
pub(crate) fn is_database_locked(err: &rusqlite::Error) -> bool {
    matches!(
        err,
        rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error {
                code: rusqlite::ffi::ErrorCode::DatabaseBusy,
                ..
            },
            _,
        ) | rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error {
                code: rusqlite::ffi::ErrorCode::DatabaseLocked,
                ..
            },
            _,
        )
    )
}

/// Chain-aware retryability test for `anyhow::Error`.
///
/// Walks [`anyhow::Error::chain`] and classifies the first typed
/// `rusqlite::Error` with [`is_database_locked`]. This is what makes a SQLite
/// lock buried under one or more `.context(..)` frames visible to the retry
/// loop (#321 Part B) — the textual test it replaces could not.
pub(crate) fn is_busy_anyhow(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| {
        cause
            .downcast_ref::<rusqlite::Error>()
            .is_some_and(is_database_locked)
    })
}

/// Textual retryability test, for error types that carry nothing but text.
///
/// ⚠️ Chain-blind by construction: it sees only the outermost `Display`
/// frame. Correct for a plain `String`- or `io::Error`-shaped error; WRONG for
/// `anyhow::Error`, which must use [`is_busy_anyhow`] instead.
pub(crate) fn display_says_locked<E: std::fmt::Display>(err: &E) -> bool {
    let msg = err.to_string().to_lowercase();
    msg.contains("locked") || msg.contains("busy")
}

/// The shared retry loop.
///
/// `is_retryable` is the ONLY part that differs between the public entry
/// points, so the backoff, the attempt accounting and the log lines have
/// exactly one home.
pub(crate) async fn retry_db_with<F, Fut, T, E, P>(
    mut operation: F,
    config: &DbRetryConfig,
    is_retryable: P,
) -> std::result::Result<T, E>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = std::result::Result<T, E>>,
    E: std::fmt::Display,
    P: Fn(&E) -> bool,
{
    let mut attempt = 0;

    loop {
        match operation().await {
            Ok(result) => {
                if attempt > 0 {
                    tracing::info!("Database operation succeeded after {} retries", attempt);
                }
                return Ok(result);
            }
            Err(err) => {
                if !is_retryable(&err) {
                    // `{:#}` so an anyhow chain reports its cause, not just
                    // the outermost context frame (#321 Part C).
                    tracing::debug!("Database error is not retryable: {:#}", err);
                    return Err(err);
                }

                if attempt >= config.max_attempts {
                    tracing::warn!(
                        "Max database retry attempts ({}) exceeded for lock error",
                        config.max_attempts
                    );
                    return Err(err);
                }

                let delay = config.calculate_delay(attempt);

                tracing::info!(
                    "Database locked (attempt {}/{}), retrying after {}ms",
                    attempt + 1,
                    config.max_attempts,
                    delay.as_millis()
                );

                sleep(delay).await;

                attempt += 1;
            }
        }
    }
}

/// Retry a database operation with exponential backoff.
///
/// Retryability is decided textually ([`display_says_locked`]) — the right
/// call for an error type whose `Display` genuinely carries the signal. An
/// `anyhow::Error` does not: use [`retry_db_anyhow`].
pub async fn retry_db_operation<F, Fut, T, E>(
    operation: F,
    config: &DbRetryConfig,
) -> std::result::Result<T, E>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = std::result::Result<T, E>>,
    E: std::fmt::Display,
{
    retry_db_with(operation, config, display_says_locked::<E>).await
}

/// Retry a database operation that returns anyhow::Result
///
/// Retryability is decided by [`is_busy_anyhow`], which walks the error
/// chain. The `Display`-based test this previously inherited saw only the
/// outermost `.context(..)` frame and therefore never fired on a wrapped
/// SQLite lock (#321 Part B).
pub async fn retry_db_anyhow<F, Fut, T>(operation: F, config: &DbRetryConfig) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T>>,
{
    retry_db_with(operation, config, is_busy_anyhow)
        .await
        .context("Database operation failed after retries")
}

/// Retry a database operation that returns rusqlite::Result
pub async fn retry_db_rusqlite<F, Fut, T>(
    operation: F,
    config: &DbRetryConfig,
) -> std::result::Result<T, rusqlite::Error>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = std::result::Result<T, rusqlite::Error>>,
{
    retry_db_with(operation, config, is_database_locked).await
}
