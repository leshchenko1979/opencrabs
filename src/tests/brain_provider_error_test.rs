use crate::brain::provider::error::*;

#[test]
fn test_error_retryable() {
    let rate_limit = ProviderError::RateLimitExceeded("Try again later".to_string());
    assert!(rate_limit.is_retryable());

    let invalid_key = ProviderError::InvalidApiKey;
    assert!(!invalid_key.is_retryable());

    let server_error = ProviderError::ApiError {
        status: 500,
        message: "Internal Server Error".to_string(),
        error_type: None,
    };
    assert!(server_error.is_retryable());

    let client_error = ProviderError::ApiError {
        status: 400,
        message: "Bad Request".to_string(),
        error_type: None,
    };
    assert!(!client_error.is_retryable());
}

#[test]
fn flaky_404_is_retryable_but_model_not_found_404_is_not() {
    // #748: a flaky provider's transient 404 must retry with backoff...
    let flaky = ProviderError::ApiError {
        status: 404,
        message: "Not Found".to_string(),
        error_type: None,
    };
    assert!(
        flaky.is_retryable(),
        "a non-model 404 is a transient infra hiccup and must retry"
    );

    // ...but a genuine model-not-found 404 stays permanent.
    let model_404 = ProviderError::ApiError {
        status: 404,
        message: "The model gpt-x is not found".to_string(),
        error_type: Some("model_not_found".to_string()),
    };
    assert!(
        !model_404.is_retryable(),
        "a model-not-found 404 is permanent, not retryable"
    );
}

#[test]
fn repetitive_tool_guardrail_500_is_not_retryable() {
    // #740: the repetition guardrail 500 will 500 again on retry with the same
    // poisoned history — surface it fast so the tool loop can prune and retry.
    let poison = ProviderError::ApiError {
        status: 500,
        message: "Repetitive tool calls detected in the conversation history. The same tool call \
                  with identical name and arguments has been repeated across multiple consecutive \
                  rounds."
            .to_string(),
        error_type: Some("invalid_request_error".to_string()),
    };
    assert!(
        !poison.is_retryable(),
        "the repetition guardrail 500 must not be retried in place"
    );

    // A plain 500 is still retryable.
    let plain = ProviderError::ApiError {
        status: 500,
        message: "Internal Server Error".to_string(),
        error_type: None,
    };
    assert!(plain.is_retryable());
}

#[test]
fn test_status_code() {
    let error = ProviderError::ApiError {
        status: 429,
        message: "Too many requests".to_string(),
        error_type: Some("rate_limit_error".to_string()),
    };
    assert_eq!(error.status_code(), Some(429));

    let invalid_key = ProviderError::InvalidApiKey;
    assert_eq!(invalid_key.status_code(), None);
}

/// #346 / D3 — a 429 is retryable REGARDLESS of its body shape.
///
/// Before the fix, only a 429 that had parsed into the rate-limit envelope
/// reached the retry engine. A bare `ApiError { status: 429 }` — an
/// infrastructure HTML error page, an unparseable body, an unrelated JSON
/// payload, or no body at all — fell through `is_retryable`'s catch-all
/// `false` arm, so the request burned ZERO in-place retries and bounced
/// straight to the fallback chain. The status code alone is the signal; the
/// body only ever added detail.
///
/// The hard-quota exemption is checked first (see the sibling test), so this
/// covers the genuine per-minute throttle in every one of its shapes.
#[test]
fn a_429_is_retryable_in_any_body_shape() {
    for body in [
        // The ordinary throttle message.
        "Too many requests",
        // An HTML error page from a CDN / load balancer — never parses into
        // the JSON envelope, so this is the shape that used to be lost.
        "<html><body>429 Too Many Requests</body></html>",
        // A JSON body with no recognizable error schema.
        r#"{"detail":"slow down"}"#,
        // No body at all.
        "",
    ] {
        let err = ProviderError::ApiError {
            status: 429,
            message: body.to_string(),
            error_type: None,
        };
        assert!(
            !err.is_quota_exhausted(),
            "test body must not read as a hard quota limit: {body:?}"
        );
        assert!(
            err.is_retryable(),
            "a 429 must retry in place whatever its body shape: {body:?}"
        );
    }
}

/// #952 must survive #346 — the explicit 429 arm must NOT resurrect a hard
/// quota / billing 429.
///
/// A monthly cap or an exhausted credit balance never lifts inside a retry
/// window, so the request has to bail straight to the fallback chain instead
/// of burning the whole backoff budget against a wall. `is_retryable` checks
/// `is_quota_exhausted()` before the match, and that ordering is the entire
/// guarantee.
#[test]
fn a_hard_quota_429_stays_non_retryable() {
    let err = ProviderError::ApiError {
        status: 429,
        message: "You exceeded your current quota, please check your plan and billing details."
            .to_string(),
        error_type: Some("insufficient_quota".to_string()),
    };
    assert!(
        err.is_quota_exhausted(),
        "the body must classify as a hard quota limit"
    );
    assert!(
        !err.is_retryable(),
        "#952: a hard quota 429 must bail to the fallback chain, not retry in place"
    );
}
