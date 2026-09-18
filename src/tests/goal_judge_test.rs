//! Tests for the goal judge (#299).
//!
//! Two halves: the retry/fail-open plumbing of `judge_goal`, and the #299
//! contract itself — the hardened prompt and the authoritative per-criterion
//! aggregate that overrules the model's own holistic verdict.

use crate::brain::goal::judge::{JudgeOutcome, judge_goal};
use crate::brain::goal::types::{GoalVerdict, JudgeDecision};
use crate::brain::provider::error::ProviderError;
use crate::brain::provider::{
    ContentBlock, LLMRequest, LLMResponse, Provider, ProviderStream, StopReason, TokenUsage,
};
use async_trait::async_trait;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

// ---------------------------------------------------------------------------
// Mock provider
// ---------------------------------------------------------------------------

/// Returns pre-configured responses and captures every request handed to
/// `complete()`. Responses are `Option` so they can be taken without requiring
/// `Clone` on `ProviderError`.
struct MockProvider {
    responses: Mutex<Vec<Option<Result<LLMResponse, ProviderError>>>>,
    call_count: AtomicUsize,
    requests: Mutex<Vec<LLMRequest>>,
}

impl MockProvider {
    fn new(responses: Vec<Result<LLMResponse, ProviderError>>) -> Self {
        Self {
            responses: Mutex::new(responses.into_iter().map(Some).collect()),
            call_count: AtomicUsize::new(0),
            requests: Mutex::new(Vec::new()),
        }
    }

    fn call_count(&self) -> usize {
        self.call_count.load(Ordering::SeqCst)
    }

    fn captured_requests(&self) -> Vec<LLMRequest> {
        self.requests.lock().unwrap().clone()
    }
}

#[async_trait]
impl Provider for MockProvider {
    async fn complete(&self, request: LLMRequest) -> crate::brain::provider::Result<LLMResponse> {
        self.requests.lock().unwrap().push(request);
        self.call_count.fetch_add(1, Ordering::SeqCst);
        let mut responses = self.responses.lock().unwrap();
        match responses.first_mut() {
            Some(slot) => {
                let taken = slot.take();
                responses.retain(|s| s.is_some());
                taken.unwrap_or(Err(ProviderError::Internal(
                    "no more mock responses".to_string(),
                )))
            }
            None => Err(ProviderError::Internal(
                "no more mock responses".to_string(),
            )),
        }
    }

    async fn stream(&self, _request: LLMRequest) -> crate::brain::provider::Result<ProviderStream> {
        unimplemented!("judge_goal does not use streaming")
    }

    fn name(&self) -> &str {
        "mock-judge"
    }

    fn default_model(&self) -> &str {
        "mock-model"
    }

    fn supported_models(&self) -> Vec<String> {
        vec!["mock-model".to_string()]
    }

    fn context_window(&self, _model: &str) -> Option<u32> {
        Some(128_000)
    }

    fn calculate_cost(&self, _model: &str, _input: u32, _output: u32) -> f64 {
        0.0
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn make_response(text: &str, stop: StopReason) -> LLMResponse {
    LLMResponse {
        id: "test-1".to_string(),
        model: "mock-model".to_string(),
        content: vec![ContentBlock::Text {
            text: text.to_string(),
        }],
        stop_reason: Some(stop),
        usage: TokenUsage::default(),
        streaming_active_secs: None,
        tool_text_leak: false,
    }
}

fn ok(text: &str) -> Result<LLMResponse, ProviderError> {
    Ok(make_response(text, StopReason::EndTurn))
}

fn eval_json(id: &str, status: &str) -> String {
    format!(
        r#"{{"id":"{id}","criterion":"criterion {id}","status":"{status}","evidence":"receipt {id}"}}"#
    )
}

/// A judge reply: the per-criterion array emitted before the aggregate verdict.
fn reply(criteria: &[(&str, &str)], verdict: &str, reason: &str) -> String {
    let arr = criteria
        .iter()
        .map(|(id, s)| eval_json(id, s))
        .collect::<Vec<_>>()
        .join(",");
    format!(r#"{{"criteria":[{arr}],"verdict":"{verdict}","reason":"{reason}"}}"#)
}

/// Run the judge with the given declared criteria against a canned response.
async fn run(provider: &MockProvider, criteria: &[&str], response: &str) -> JudgeOutcome {
    let criteria: Vec<String> = criteria.iter().map(|c| c.to_string()).collect();
    judge_goal(
        provider,
        "mock-model",
        "the goal",
        &criteria,
        "no evidence",
        response,
    )
    .await
}

fn user_prompt(req: &LLMRequest) -> String {
    match &req.messages[0].content[0] {
        ContentBlock::Text { text } => text.clone(),
        _ => panic!("expected text block"),
    }
}

// ---------------------------------------------------------------------------
// The aggregate contract (#299)
// ---------------------------------------------------------------------------

/// Every criterion MET → Verified.
#[tokio::test]
async fn all_criteria_met_verifies() {
    let provider = MockProvider::new(vec![ok(&reply(
        &[("c1", "MET"), ("c2", "MET")],
        "VERIFIED",
        "both receipts present",
    ))]);
    let outcome = run(&provider, &["a", "b"], "done").await;
    assert_eq!(outcome.verdict, GoalVerdict::Verified);
    assert_eq!(outcome.model_verdict, GoalVerdict::Verified);
    assert_eq!(outcome.criteria.len(), 2);
    assert!(!outcome.parse_failed);
    assert_eq!(provider.call_count(), 1);
}

/// The #299 regression: a model claiming VERIFIED while a criterion is UNMET
/// must be overruled by the aggregate.
#[tokio::test]
async fn model_optimism_cannot_outrank_an_unmet_criterion() {
    let provider = MockProvider::new(vec![ok(&reply(
        &[("c1", "UNMET")],
        "VERIFIED",
        "looks done to me",
    ))]);
    let outcome = run(&provider, &["a"], "still working on it").await;
    assert_eq!(outcome.verdict, GoalVerdict::Rejected);
    assert_eq!(outcome.model_verdict, GoalVerdict::Verified);
}

/// The overrule runs in both directions: a model that says REJECTED while every
/// criterion is MET cannot withhold a Verified aggregate either.
#[tokio::test]
async fn model_pessimism_is_overruled_too() {
    let provider = MockProvider::new(vec![ok(&reply(&[("c1", "MET")], "REJECTED", "unclear"))]);
    let outcome = run(&provider, &["a"], "done").await;
    assert_eq!(outcome.verdict, GoalVerdict::Verified);
    assert_eq!(outcome.model_verdict, GoalVerdict::Rejected);
}

/// NO_EVIDENCE is the honest third state: nothing contradicted, nothing proven.
#[tokio::test]
async fn no_evidence_is_uncertain_not_verified() {
    let provider = MockProvider::new(vec![ok(&reply(
        &[("c1", "MET"), ("c2", "NO_EVIDENCE")],
        "VERIFIED",
        "one receipt missing",
    ))]);
    let outcome = run(&provider, &["a", "b"], "progress report").await;
    assert_eq!(outcome.verdict, GoalVerdict::Uncertain);
}

/// A partial reply (fewer evaluations than declared) cannot verify: the
/// unevaluated remainder counts as NO_EVIDENCE.
#[tokio::test]
async fn partial_reply_is_uncertain() {
    let provider = MockProvider::new(vec![ok(&reply(&[("c1", "MET")], "VERIFIED", "done"))]);
    let outcome = run(&provider, &["a", "b", "c"], "done").await;
    assert_eq!(outcome.verdict, GoalVerdict::Uncertain);
}

/// A goal that declared no criteria can never be verified — "no criteria" is
/// not a licence to terminate.
#[tokio::test]
async fn no_declared_criteria_is_uncertain() {
    let provider = MockProvider::new(vec![ok(&reply(&[], "VERIFIED", "nothing to check"))]);
    let outcome = run(&provider, &[], "done").await;
    assert_eq!(outcome.verdict, GoalVerdict::Uncertain);
}

// ---------------------------------------------------------------------------
// The hardened prompt (#299)
// ---------------------------------------------------------------------------

/// The retired sycophancy licence must not survive in the prompt, and the three
/// load-bearing rules must be present.
#[tokio::test]
async fn system_prompt_is_hardened() {
    let provider = MockProvider::new(vec![ok(&reply(&[("c1", "MET")], "VERIFIED", "ok"))]);
    run(&provider, &["a"], "done").await;
    let system = provider.captured_requests()[0].system.clone().unwrap();
    assert!(
        !system.contains("Be generous"),
        "the sycophancy licence is the #299 root cause and must never return"
    );
    assert!(system.contains("Anti-verbosity rule"));
    assert!(system.contains("Prose is NOT evidence"));
    assert!(system.contains("in-progress state is never MET"));
}

/// The user prompt numbers the declared criteria with stable ids, carries the
/// evidence pack and the last response — and says so explicitly when the goal
/// declared no criteria, so the judge does not invent any.
#[tokio::test]
async fn user_prompt_carries_criteria_evidence_and_response() {
    let provider = MockProvider::new(vec![ok(&reply(&[("c1", "MET")], "VERIFIED", "ok"))]);
    let criteria = vec!["tests pass".to_string(), "PR filed".to_string()];
    judge_goal(
        &provider,
        "mock-model",
        "ship it",
        &criteria,
        "task 1: completed",
        "done!",
    )
    .await;
    let prompt = user_prompt(&provider.captured_requests()[0]);
    assert!(prompt.contains("c1: tests pass"));
    assert!(prompt.contains("c2: PR filed"));
    assert!(prompt.contains("task 1: completed"));
    assert!(prompt.contains("done!"));

    let provider = MockProvider::new(vec![ok(&reply(&[], "UNCERTAIN", "ok"))]);
    judge_goal(&provider, "mock-model", "ship it", &[], "none", "done!").await;
    let prompt = user_prompt(&provider.captured_requests()[0]);
    assert!(prompt.contains("none declared"));
}

// ---------------------------------------------------------------------------
// Retry and fail-open plumbing
// ---------------------------------------------------------------------------

/// First response empty, second valid: retries and succeeds on attempt 2.
#[tokio::test]
async fn empty_response_retries_and_succeeds() {
    let provider = MockProvider::new(vec![
        ok(""),
        ok(&reply(&[("c1", "MET")], "VERIFIED", "recovered")),
    ]);
    let outcome = run(&provider, &["a"], "response").await;
    assert_eq!(outcome.verdict, GoalVerdict::Verified);
    assert_eq!(provider.call_count(), 2);
}

/// Both attempts empty → fail-open UNCERTAIN, flagged for the auto-pause.
#[tokio::test]
async fn consecutive_empty_fails_open_uncertain() {
    let provider = MockProvider::new(vec![ok(""), ok("")]);
    let outcome = run(&provider, &["a"], "response").await;
    assert_eq!(outcome.verdict, GoalVerdict::Uncertain);
    assert!(outcome.parse_failed);
    assert!(outcome.reason.contains("empty response"));
    assert_eq!(provider.call_count(), 2);
}

/// Unparseable JSON on the first attempt, valid on the second.
#[tokio::test]
async fn parse_failure_retries_and_succeeds() {
    let provider = MockProvider::new(vec![
        ok("not json at all"),
        ok(&reply(&[("c1", "MET")], "VERIFIED", "kept going")),
    ]);
    let outcome = run(&provider, &["a"], "response").await;
    assert_eq!(outcome.verdict, GoalVerdict::Verified);
    assert_eq!(provider.call_count(), 2);
}

/// Both attempts unparseable → fail-open UNCERTAIN, flagged.
#[tokio::test]
async fn consecutive_parse_failures_fail_open_uncertain() {
    let provider = MockProvider::new(vec![ok("garbage1"), ok("garbage2")]);
    let outcome = run(&provider, &["a"], "response").await;
    assert_eq!(outcome.verdict, GoalVerdict::Uncertain);
    assert!(outcome.parse_failed);
    assert!(outcome.reason.contains("judge parse error"));
    assert_eq!(provider.call_count(), 2);
}

/// First call is an API error, second succeeds.
#[tokio::test]
async fn api_error_retries_and_succeeds() {
    let provider = MockProvider::new(vec![
        Err(ProviderError::Internal("transient".to_string())),
        ok(&reply(&[("c1", "MET")], "VERIFIED", "fixed")),
    ]);
    let outcome = run(&provider, &["a"], "response").await;
    assert_eq!(outcome.verdict, GoalVerdict::Verified);
    assert_eq!(provider.call_count(), 2);
}

/// Both attempts are API errors → fail-open UNCERTAIN, flagged.
#[tokio::test]
async fn consecutive_api_errors_fail_open_uncertain() {
    let provider = MockProvider::new(vec![
        Err(ProviderError::Internal("err1".to_string())),
        Err(ProviderError::Internal("err2".to_string())),
    ]);
    let outcome = run(&provider, &["a"], "response").await;
    assert_eq!(outcome.verdict, GoalVerdict::Uncertain);
    assert!(outcome.parse_failed);
    assert!(outcome.reason.contains("judge call error"));
    assert_eq!(provider.call_count(), 2);
}

/// Every attempt carries max_tokens=4096 — the retry must not drift.
#[tokio::test]
async fn max_tokens_is_4096_on_every_attempt() {
    let provider = MockProvider::new(vec![ok(""), ok(&reply(&[("c1", "MET")], "VERIFIED", "ok"))]);
    run(&provider, &["a"], "response").await;
    let requests = provider.captured_requests();
    assert_eq!(requests.len(), 2);
    assert!(requests.iter().all(|r| r.max_tokens == Some(4096)));
}

/// A MaxTokens stop reason with valid JSON still parses, with no retry.
#[tokio::test]
async fn max_tokens_stop_reason_with_valid_json_no_retry() {
    let provider = MockProvider::new(vec![Ok(make_response(
        &reply(&[("c1", "NO_EVIDENCE")], "UNCERTAIN", "hit token limit"),
        StopReason::MaxTokens,
    ))]);
    let outcome = run(&provider, &["a"], "response").await;
    assert_eq!(outcome.verdict, GoalVerdict::Uncertain);
    assert_eq!(outcome.reason, "hit token limit");
    assert_eq!(provider.call_count(), 1);
}

/// A long last-response is truncated before it reaches the judge.
#[tokio::test]
async fn long_response_is_truncated_in_the_prompt() {
    let provider = MockProvider::new(vec![ok(&reply(&[("c1", "MET")], "VERIFIED", "ok"))]);
    let long_response = "x".repeat(10_000);
    run(&provider, &["a"], &long_response).await;
    let prompt = user_prompt(&provider.captured_requests()[0]);
    assert!(
        prompt.len() < 10_000,
        "prompt should be truncated, got len={}",
        prompt.len()
    );
}

/// #300: a 4000-byte-from-end cut landing inside a multi-byte codepoint must
/// not panic. Emoji are 4 bytes, so real assistant text hits this routinely.
#[tokio::test]
async fn truncation_is_char_boundary_safe() {
    let provider = MockProvider::new(vec![ok(&reply(&[("c1", "MET")], "VERIFIED", "ok"))]);

    // 100 ASCII bytes, a 4-byte emoji at bytes 100..104, then 3998 ASCII bytes
    // → len = 4102, so `len - 4000` = 102, INSIDE the emoji.
    let mut response = "A".repeat(100);
    response.push('🔺');
    response.push_str(&"B".repeat(3998));
    assert_eq!(response.len(), 4102);
    assert!(
        !response.is_char_boundary(102),
        "fixture must land mid-codepoint"
    );

    run(&provider, &["a"], &response).await;

    let prompt = user_prompt(&provider.captured_requests()[0]);
    assert!(
        prompt.ends_with(&"B".repeat(3998)),
        "truncation should keep the ASCII tail"
    );
}

/// #300: the fail-open warn path truncates the raw reply to 200 bytes. A
/// non-ASCII reply whose 200-byte cut lands mid-codepoint must not panic.
#[test]
fn parse_or_continue_truncates_non_ascii_without_panicking() {
    // 199 ASCII bytes, then a 3-byte arrow at bytes 199..202, then junk that
    // fails JSON parsing → the warn path slices at byte 200.
    let mut raw = "X".repeat(199);
    raw.push('→');
    raw.push_str("not json");
    assert_eq!(raw.len(), 210);
    assert!(
        !raw.is_char_boundary(200),
        "fixture must land mid-codepoint"
    );

    let decision = JudgeDecision::parse_or_continue(&raw);
    assert_eq!(decision.verdict, GoalVerdict::Uncertain);
}
