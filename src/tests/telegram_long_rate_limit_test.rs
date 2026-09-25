//! Regression test for Telegram long rate-limit guard (#1110, #556).
//!
//! When Telegram returns `Retry-After: N` where N > 1 hour, the chat is
//! flood-banned for hours (28442s = 7.9 hours observed in Adi's audit).
//! Retrying the send ladder is pointless, so long rate-limits bail
//! immediately.
//!
//! #556 turned the inline wait from a 30s cap into a 60s BOUND: a window
//! within the bound is slept in full, and a window over it is not slept at
//! all — the call defers and the armed cooldown carries the retry. Both arms
//! sleep less than the pre-#556 ladder, which truncated the wait to 30s and
//! then retried inside a ban that was still running.
//!
//! The two retrying tests run on a paused clock (#1532). They used to sleep
//! those 90 seconds for real — between them roughly 87% of the suite's wall
//! clock, and both tripped the harness 60-second warning. Under
//! `start_paused` tokio auto-advances whenever nothing is runnable, so the
//! waits resolve instantly and `Instant::elapsed` still reports the virtual
//! time. That turns the wait from a cost into an assertion: measuring it is
//! what pins the sleep policy, since attempt counts alone cannot tell a full
//! in-bound wait from a deferred window.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;
use teloxide::RequestError;
use teloxide::types::Seconds;

/// In-bound wait per retry (a 30s window sits inside the 60s
/// `MAX_INLINE_RATE_LIMIT_WAIT` bound, so it is slept in full) times
/// `MAX_RETRIES`. Named so the expectation below reads as the rule, not as a
/// coincidentally equal number.
const LADDER_WAIT: Duration = Duration::from_secs(90);

/// Long rate-limit (>1 hour) bails immediately without retrying.
#[tokio::test]
async fn long_rate_limit_bails_immediately() {
    let attempts = Arc::new(AtomicU32::new(0));
    let attempts_clone = attempts.clone();

    // Mock a send that always returns a 7.9-hour rate-limit
    let result = crate::channels::telegram::intermediates::send_retrying_rate_limit(
        "test send",
        move || {
            let attempts = attempts_clone.clone();
            async move {
                attempts.fetch_add(1, Ordering::SeqCst);
                // 28442 seconds = 7.9 hours (observed in Adi's audit)
                Err::<(), _>(RequestError::RetryAfter(Seconds::from_seconds(28442)))
            }
        },
    )
    .await;

    // Should fail immediately without retrying
    assert!(result.is_err(), "Long rate-limit should return error");
    assert_eq!(
        attempts.load(Ordering::SeqCst),
        1,
        "Long rate-limit should bail after 1 attempt, not retry"
    );
}

/// Short rate-limit (<1 hour) retries normally up to 3 attempts.
#[tokio::test(start_paused = true)]
async fn short_rate_limit_retries_normally() {
    let attempts = Arc::new(AtomicU32::new(0));
    let attempts_clone = attempts.clone();
    let start = tokio::time::Instant::now();

    // Mock a send that returns a 30-second rate-limit 3 times, then succeeds
    let result = crate::channels::telegram::intermediates::send_retrying_rate_limit(
        "test send",
        move || {
            let attempts = attempts_clone.clone();
            async move {
                let count = attempts.fetch_add(1, Ordering::SeqCst);
                if count < 3 {
                    // 30 seconds (typical flood window)
                    Err::<(), _>(RequestError::RetryAfter(Seconds::from_seconds(30)))
                } else {
                    Ok(())
                }
            }
        },
    )
    .await;

    // Should succeed after retries
    assert!(
        result.is_ok(),
        "Short rate-limit should succeed after retries"
    );
    assert_eq!(
        attempts.load(Ordering::SeqCst),
        4,
        "Short rate-limit should make 4 attempts (1 initial + 3 retries)"
    );
    // A 30s window sits exactly ON the inline cap, so it is waited in full
    // rather than clamped. Same 90s total as the capped case below, reached
    // the other way round.
    assert_eq!(
        start.elapsed(),
        LADDER_WAIT,
        "three 30s windows waited in full"
    );
}

/// A rate-limit at exactly 1 hour (3600s) is NOT long enough to bail on the
/// `LONG_RATE_LIMIT_THRESHOLD` guard, but it is far over the 60s inline bound
/// (#556) — so the ladder defers on its first pass instead of sleeping.
///
/// This is the stronger form of the #1064 guarantee: the pre-#556 ladder slept
/// 3 x 30s of a 3600s ban and then retried 3540s inside it, and the elapsed
/// assertion below is what made that visible. Post-#556 nothing is slept at
/// all, so the assertion now pins zero rather than a clamped 90s.
#[tokio::test(start_paused = true)]
async fn rate_limit_at_threshold_defers_without_sleeping() {
    let attempts = Arc::new(AtomicU32::new(0));
    let attempts_clone = attempts.clone();
    let start = tokio::time::Instant::now();

    // Mock a send that returns exactly 3600s (1 hour) rate-limit
    let result = crate::channels::telegram::intermediates::send_retrying_rate_limit(
        "test send",
        move || {
            let attempts = attempts_clone.clone();
            async move {
                attempts.fetch_add(1, Ordering::SeqCst);
                // Exactly 1 hour (at threshold, not over)
                Err::<(), _>(RequestError::RetryAfter(Seconds::from_seconds(3600)))
            }
        },
    )
    .await;

    // At threshold the long-rate-limit guard does not fire, but 3600s exceeds
    // the inline bound, so the first `wait_out` defers and the ladder stops.
    assert!(
        result.is_err(),
        "a window over the inline bound must surface, not be slept"
    );
    assert_eq!(
        attempts.load(Ordering::SeqCst),
        1,
        "3600s exceeds the 60s inline bound: defer, never retry inside the ban (#556)"
    );
    assert_eq!(
        start.elapsed(),
        Duration::ZERO,
        "nothing is slept for a window over the inline bound (#1064)"
    );
}
