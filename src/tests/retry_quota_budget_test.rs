//! #346 — the bounded in-place budget for HARD quota / billing errors.
//!
//! Retryability and retry BUDGET are separable, and this file pins both
//! halves so a later change cannot quietly collapse them:
//!
//! * a quota error stays **non-retryable**, which is what makes
//!   `should_try_next_provider` hand the request to the fallback chain
//!   (#952's guarantee, asserted below);
//! * an opted-in aggregator gets a **separate, deliberately tiny** in-place
//!   budget, because the cap behind the 429 belongs to one upstream key and
//!   the same request may be served by another seconds later.
//!
//! The regression that matters most is the third test: a direct provider
//! with a genuine billing 429 must still fall back immediately, i.e. burn
//! zero in-place attempts. That is #952, and #346 must not reopen it.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use crate::brain::provider::error::{should_try_next_provider, ProviderError};
use crate::brain::provider::retry_policy::{self, RetryOverrides, QUOTA_RETRY_ATTEMPTS};
use crate::utils::retry::{retry_with_notify, RetryConfig, RetryableError};

/// A 429 whose body carries a HARD quota phrase — the aggregator case.
fn quota_429() -> ProviderError {
    ProviderError::ApiError {
        status: 429,
        message: "You exceeded your current quota, please check your plan and billing details."
            .to_string(),
        error_type: None,
    }
}

/// A 429 whose body carries no quota phrase — an ordinary transient throttle.
fn plain_429() -> ProviderError {
    ProviderError::ApiError {
        status: 429,
        message: "Too many requests, slow down".to_string(),
        error_type: None,
    }
}

// ---------------------------------------------------------------------------
// Classification: the two halves must stay separate.
// ---------------------------------------------------------------------------

#[test]
fn a_quota_429_is_not_retryable_in_place() {
    // #952: a hard cap will not lift inside a retry window, so the SAME
    // provider must not be retried.
    assert!(
        !quota_429().is_retryable(),
        "a hard quota 429 must stay non-retryable — this is #952"
    );
}

#[test]
fn a_quota_429_still_rolls_to_the_fallback_chain() {
    // The other half of #952: non-retryable here means "do not retry THIS
    // provider", never "give up". The next provider bills another account.
    assert!(
        should_try_next_provider(&quota_429()),
        "a hard quota must still fail over to the next provider"
    );
}

#[test]
fn a_non_quota_429_is_retryable_in_any_body_shape() {
    // Task 3's guarantee, restated here because this file is where the two
    // 429 kinds are told apart: a throttle with no recognizable envelope
    // still reaches the retry engine.
    assert!(
        plain_429().is_retryable(),
        "a transient 429 must be retryable whatever its body looks like"
    );
}

#[test]
fn the_quota_classification_agrees_across_inherent_and_trait_methods() {
    // The retry engine dispatches through the `RetryableError` trait, while
    // `should_try_next_provider` calls the inherent method. If the trait
    // override ever stopped delegating, the engine would see every error as
    // "not quota" and quietly spend the full attempt budget on a hard cap.
    let q = quota_429();
    assert!(q.is_quota_exhausted(), "inherent: quota-phrased 429");
    assert!(
        RetryableError::is_quota_exhausted(&q),
        "trait override must delegate to the inherent classifier"
    );

    let p = plain_429();
    assert!(!p.is_quota_exhausted(), "inherent: plain 429 is a throttle");
    assert!(
        !RetryableError::is_quota_exhausted(&p),
        "trait override must not over-classify a plain 429"
    );
}

// ---------------------------------------------------------------------------
// Budget selection.
// ---------------------------------------------------------------------------

#[test]
fn the_default_budget_for_a_quota_error_is_zero() {
    let cfg = RetryConfig::default();
    assert_eq!(
        cfg.retry_quota_attempts, 0,
        "the default must be #952's behaviour"
    );
    assert_eq!(cfg.attempt_budget_for(&quota_429()), 0);
}

#[test]
fn the_quota_opt_in_grants_a_bounded_budget() {
    let cfg = RetryConfig::default().with_quota_attempts(QUOTA_RETRY_ATTEMPTS);
    assert_eq!(cfg.attempt_budget_for(&quota_429()), QUOTA_RETRY_ATTEMPTS);
    assert!(
        QUOTA_RETRY_ATTEMPTS > 0 && QUOTA_RETRY_ATTEMPTS <= 2,
        "the aggregator budget is deliberately tiny, not a second backoff ramp"
    );
}

#[test]
fn a_non_quota_error_keeps_the_full_attempt_budget() {
    // Opting in for quota retries must not shrink the ordinary budget.
    let cfg = RetryConfig::default().with_quota_attempts(QUOTA_RETRY_ATTEMPTS);
    assert_eq!(cfg.attempt_budget_for(&plain_429()), cfg.max_attempts);
}

// ---------------------------------------------------------------------------
// Resolution: the config flag reaches the resolved config.
// ---------------------------------------------------------------------------

#[test]
fn resolve_grants_the_quota_budget_only_when_opted_in() {
    let opted_in = RetryOverrides {
        quota_exhausted: true,
        ..Default::default()
    };
    let cfg = retry_policy::resolve("acme", "https://api.example.com", "gpt-4o", &opted_in);
    assert_eq!(cfg.retry_quota_attempts, QUOTA_RETRY_ATTEMPTS);

    let not_opted_in = RetryOverrides::default();
    let cfg = retry_policy::resolve("acme", "https://api.example.com", "gpt-4o", &not_opted_in);
    assert_eq!(
        cfg.retry_quota_attempts, 0,
        "a provider that did not opt in keeps #952 behaviour"
    );
}

// ---------------------------------------------------------------------------
// Behaviour: what the retry engine actually does with each budget.
// ---------------------------------------------------------------------------

/// Drive `retry_with_notify` with a fixed error and count the attempts.
async fn attempts_with(cfg: &RetryConfig, err: fn() -> ProviderError) -> u32 {
    let calls = Arc::new(AtomicU32::new(0));
    let c = calls.clone();

    let result: Result<(), ProviderError> = retry_with_notify(
        || {
            let c = c.clone();
            async move {
                c.fetch_add(1, Ordering::SeqCst);
                Err(err())
            }
        },
        cfg,
        |_, _, _| {},
    )
    .await;

    assert!(result.is_err(), "the operation always fails");
    calls.load(Ordering::SeqCst)
}

#[tokio::test]
async fn a_direct_provider_falls_back_immediately_on_a_billing_429() {
    // THE regression guard. Default config => zero quota budget => the very
    // first attempt is also the last one, so the caller's fallback walk can
    // hand the request to a provider with its own budget. If this ever
    // becomes 2, #346 has reopened #952.
    let calls = attempts_with(&RetryConfig::default(), quota_429).await;
    assert_eq!(
        calls, 1,
        "a hard billing 429 must burn ZERO in-place attempts by default"
    );
}

#[tokio::test]
async fn an_opted_in_aggregator_gets_bounded_in_place_attempts() {
    // With the opt-in the error is retried, but only within the tiny quota
    // budget — the first attempt plus QUOTA_RETRY_ATTEMPTS retries.
    let cfg = RetryConfig::default().with_quota_attempts(QUOTA_RETRY_ATTEMPTS);
    let calls = attempts_with(&cfg, quota_429).await;
    assert_eq!(
        calls,
        QUOTA_RETRY_ATTEMPTS + 1,
        "an opted-in aggregator gets exactly the bounded quota budget"
    );
}

#[tokio::test]
async fn a_transient_429_still_gets_the_full_attempt_budget() {
    // The opt-in must not touch the ordinary path: a plain throttle gets
    // every attempt the config allows.
    let cfg = RetryConfig::default().with_quota_attempts(QUOTA_RETRY_ATTEMPTS);
    let calls = attempts_with(&cfg, plain_429).await;
    assert_eq!(calls, cfg.max_attempts + 1);
}
