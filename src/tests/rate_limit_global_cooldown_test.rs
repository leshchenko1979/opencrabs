//! Unit tests for the process-wide Telegram 429 cooldown lock and the
//! `Retry after N` parser (#262, #556).
//!
//! These live here rather than inline in `channels/telegram/rate_limit.rs` so
//! that all tests stay under `src/tests/` per the test-isolation rule.
use std::time::Duration;

use crate::channels::telegram::governor::test_support;
use crate::channels::telegram::rate_limit::{
    exceeds_inline_bound, is_global_cooldown_active, parse_retry_after, record_global_429,
    reset_global_cooldown, wait_global_cooldown, wait_out, WaitOutcome, MAX_INLINE_RATE_LIMIT_WAIT,
};

#[test]
fn test_parse_retry_after() {
    assert_eq!(
        parse_retry_after("Too Many Requests: retry after 5"),
        Some(Duration::from_secs(5))
    );
    assert_eq!(
        parse_retry_after("Retry after 12 seconds"),
        Some(Duration::from_secs(12))
    );
    assert_eq!(parse_retry_after("Retry after 0"), None);
    assert_eq!(parse_retry_after("Other error"), None);
}

#[test]
fn test_inline_bound_defers_only_windows_over_sixty_seconds() {
    assert_eq!(MAX_INLINE_RATE_LIMIT_WAIT, Duration::from_secs(60));
    assert!(!exceeds_inline_bound(Duration::from_secs(60)));
    assert!(exceeds_inline_bound(Duration::from_secs(61)));
}

#[tokio::test]
async fn test_global_429_lock_cooldown() {
    let _guard = test_support::registry_guard().await;
    test_support::reset(0);
    reset_global_cooldown();

    assert!(!is_global_cooldown_active());
    assert!(wait_global_cooldown(MAX_INLINE_RATE_LIMIT_WAIT).await);

    // Record 5s cooldown -> total wait is 5s + 2s margin = 7s.
    record_global_429(Duration::from_secs(5), Some(-1001));
    assert!(is_global_cooldown_active());

    // A bound that covers the deadline clears it.
    assert!(wait_global_cooldown(MAX_INLINE_RATE_LIMIT_WAIT).await);
    assert!(!is_global_cooldown_active());
    reset_global_cooldown();
}

#[tokio::test]
async fn test_global_429_lock_extension_monotonic() {
    let _guard = test_support::registry_guard().await;
    test_support::reset(0);
    reset_global_cooldown();

    // 10s cooldown -> 12s total.
    record_global_429(Duration::from_secs(10), Some(-1001));
    // A smaller 3s cooldown must not shorten the 12s deadline.
    record_global_429(Duration::from_secs(3), Some(-1001));

    assert!(wait_global_cooldown(MAX_INLINE_RATE_LIMIT_WAIT).await);
    assert!(!is_global_cooldown_active());
    reset_global_cooldown();
}

#[tokio::test]
async fn test_global_429_deadline_is_not_truncated_by_inline_bound() {
    let _guard = test_support::registry_guard().await;
    test_support::reset(0);
    reset_global_cooldown();

    // A 61s retry-after arms a 63s deadline. A bounded 60s wait must defer,
    // not make the deadline disappear and permit an early retry.
    record_global_429(Duration::from_secs(61), Some(-1001));
    assert!(!wait_global_cooldown(MAX_INLINE_RATE_LIMIT_WAIT).await);
    assert!(is_global_cooldown_active());

    assert!(wait_global_cooldown(MAX_INLINE_RATE_LIMIT_WAIT).await);
    assert!(!is_global_cooldown_active());
    reset_global_cooldown();
}

#[tokio::test(start_paused = true)]
async fn long_window_defers_and_sleeps_nothing() {
    let _guard = test_support::registry_guard().await;
    test_support::reset(0);
    reset_global_cooldown();

    // #556: a window over the inline bound is NOT slept. The pre-#556 cap slept
    // 30s of an 8288s ban and then retried inside it; now nothing is slept and
    // the armed deadline carries the retry.
    assert_eq!(
        wait_out("test", Duration::from_secs(8_288), "", Some(-1001)).await,
        WaitOutcome::Deferred
    );
    assert!(
        is_global_cooldown_active(),
        "deferring must leave the armed deadline in place"
    );
    reset_global_cooldown();
}

#[tokio::test(start_paused = true)]
async fn in_bound_window_is_slept_rather_than_deferred() {
    let _guard = test_support::registry_guard().await;
    test_support::reset(0);
    reset_global_cooldown();

    // 45s is the top of every window measured over five days (#556): inside the
    // 60s bound, so the caller sleeps the real window and retries — the
    // truncation that fired the retry inside the ban is gone.
    assert_eq!(
        wait_out("test", Duration::from_secs(45), "", Some(-1001)).await,
        WaitOutcome::Slept
    );
    reset_global_cooldown();
}
