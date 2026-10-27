//! Vision request budget tests — the omit-`max_tokens` shape.
//!
//! `analyze_image` used to send a fixed `"max_tokens": 1024` on every call. A
//! reasoning vision model shares that budget with its reasoning tokens, so a
//! hard question spent all 1024 thinking and returned an empty `content` the
//! tool reported as a flat failure. The fix: send NO cap by default, let the
//! endpoint pick, and retry once with a generous cap only when the endpoint
//! itself refuses a capless request.

mod request_body {
    use crate::brain::tools::provider_vision::vision_request_body;

    /// Mutant killed: a body that always carries `max_tokens` (the old fixed
    /// 1024, or an explicit `null` some gateways reject).
    #[test]
    fn no_cap_means_the_field_is_absent() {
        let body =
            vision_request_body("some-vision-model", "what is this?", "https://x/i.png", None);
        let obj = body.as_object().expect("body must be a JSON object");
        assert!(
            !obj.contains_key("max_tokens"),
            "a capless request must OMIT max_tokens, not send a default or a null: {body}"
        );
        assert_eq!(obj["model"], "some-vision-model");
        assert_eq!(obj["messages"][0]["content"][1]["type"], "image_url");
    }

    /// Mutant killed: dropping the cap on the retry path, which would loop the
    /// same refusal forever.
    #[test]
    fn an_explicit_cap_is_sent_as_a_number() {
        let body = vision_request_body("m", "q", "https://x/i.png", Some(65000));
        assert_eq!(body["max_tokens"], 65000);
        assert!(
            body["max_tokens"].is_u64(),
            "the cap must be a number, never a string"
        );
    }

    /// Mutant killed: a cap small enough to reproduce the original bug.
    #[test]
    fn the_retry_cap_is_generous() {
        use crate::brain::tools::provider_vision::VISION_RETRY_MAX_TOKENS;
        assert!(
            VISION_RETRY_MAX_TOKENS > 1024,
            "the retry cap exists to escape the 1024 ceiling, not to restate it"
        );
    }
}

mod retry_predicate {
    use crate::brain::tools::provider_vision::needs_max_tokens;

    /// Mutant killed: a predicate that never fires, so a capless refusal is
    /// returned as a hard failure with no retry.
    #[test]
    fn a_refusal_that_names_the_cap_triggers_the_retry() {
        assert!(needs_max_tokens(
            "Vision API error 400: {\"error\":{\"message\":\"max_tokens is required\"}}"
        ));
        assert!(needs_max_tokens(
            "Vision API error 400: max_completion_tokens: field required"
        ));
    }

    /// Mutant killed: a predicate that fires on everything, which would send a
    /// second request — and a 65000-token cap — after any 401 or timeout.
    #[test]
    fn unrelated_failures_do_not_trigger_the_retry() {
        assert!(!needs_max_tokens("Vision API error 401: invalid api key"));
        assert!(!needs_max_tokens("Vision request failed: timed out"));
        assert!(!needs_max_tokens("Vision response was not JSON: eof"));
    }
}

mod empty_content {
    use crate::brain::tools::provider_vision::empty_content_reason;
    use serde_json::json;

    /// Mutant killed: the old flat "No text response from vision model", which
    /// could not distinguish "spent the budget thinking" from "provider sent
    /// nothing" — the leg that left the 2026-10-19 burst undiagnosed.
    #[test]
    fn a_budget_exhausted_answer_names_the_reason_and_the_tokens() {
        let body = json!({
            "choices": [{ "message": { "content": "" }, "finish_reason": "length" }],
            "usage": { "completion_tokens_details": { "reasoning_tokens": 1024 } }
        });
        let reason = empty_content_reason(&body);
        assert!(reason.contains("finish_reason: length"), "{reason}");
        assert!(reason.contains("reasoning_tokens: 1024"), "{reason}");
    }

    /// Mutant killed: unwrapping `usage`, which panics on the providers that
    /// omit it entirely.
    #[test]
    fn a_missing_usage_block_still_reports_the_finish_reason() {
        let body = json!({
            "choices": [{ "message": { "content": "" }, "finish_reason": "stop" }]
        });
        let reason = empty_content_reason(&body);
        assert!(reason.contains("finish_reason: stop"), "{reason}");
        assert!(!reason.contains("reasoning_tokens"), "{reason}");
    }

    /// Mutant killed: panicking on a body with no `choices` at all.
    #[test]
    fn a_choice_less_body_is_reported_as_unknown() {
        let reason = empty_content_reason(&json!({ "error": "upstream said nothing" }));
        assert!(reason.contains("finish_reason: unknown"), "{reason}");
    }
}
