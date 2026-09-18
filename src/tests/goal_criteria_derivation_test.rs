//! Tests for criteria derivation (#299).
//!
//! The contract: derivation is one model call, it never panics, and every
//! failure path yields "no criteria" rather than an error. The stored form is
//! JSON text that round-trips back to the same list.

use crate::brain::goal::criteria::{
    MAX_CRITERIA, derive_criteria, derive_criteria_if_needed, parse_criteria, parse_criteria_reply,
    serialize_criteria,
};
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

/// Serves canned responses in order and records every request.
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
        unimplemented!("derive_criteria does not use streaming")
    }

    fn name(&self) -> &str {
        "mock-criteria"
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

fn ok(text: &str) -> Result<LLMResponse, ProviderError> {
    Ok(LLMResponse {
        id: "test-1".to_string(),
        model: "mock-model".to_string(),
        content: vec![ContentBlock::Text {
            text: text.to_string(),
        }],
        stop_reason: Some(StopReason::EndTurn),
        usage: TokenUsage::default(),
        streaming_active_secs: None,
        tool_text_leak: false,
    })
}

fn err() -> Result<LLMResponse, ProviderError> {
    Err(ProviderError::Internal("provider exploded".to_string()))
}

fn strings(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| s.to_string()).collect()
}

fn user_prompt(req: &LLMRequest) -> String {
    match &req.messages[0].content[0] {
        ContentBlock::Text { text } => text.clone(),
        _ => panic!("expected text block"),
    }
}

// ---------------------------------------------------------------------------
// derive_criteria — fail-open
// ---------------------------------------------------------------------------

/// Acceptance: a provider error yields an empty Vec and never panics.
#[tokio::test]
async fn provider_error_yields_empty_vec() {
    let provider = MockProvider::new(vec![err()]);
    let criteria = derive_criteria(&provider, "mock-model", "ship the thing").await;
    assert!(criteria.is_empty());
    // One call only — derivation does not retry; the caller owns the fallback.
    assert_eq!(provider.call_count(), 1);
}

/// An empty model reply is a failure too, not an empty goal.
#[tokio::test]
async fn empty_response_yields_empty_vec() {
    let provider = MockProvider::new(vec![ok(""), ok("")]);
    let criteria = derive_criteria(&provider, "mock-model", "ship the thing").await;
    assert!(criteria.is_empty());
    assert_eq!(provider.call_count(), 1);
}

/// Unparseable prose yields an empty Vec rather than a garbage criterion.
#[tokio::test]
async fn unparseable_reply_yields_empty_vec() {
    let provider = MockProvider::new(vec![ok("I could not think of any criteria.")]);
    let criteria = derive_criteria(&provider, "mock-model", "ship the thing").await;
    assert!(criteria.is_empty());
}

/// An empty goal is never sent to the model.
#[tokio::test]
async fn empty_goal_text_skips_the_model() {
    let provider = MockProvider::new(vec![ok(r#"{"criteria":["nope"]}"#)]);
    let criteria = derive_criteria(&provider, "mock-model", "   \n  ").await;
    assert!(criteria.is_empty());
    assert_eq!(provider.call_count(), 0);
}

// ---------------------------------------------------------------------------
// derive_criteria — reply shapes
// ---------------------------------------------------------------------------

#[tokio::test]
async fn parses_plain_json_object() {
    let provider = MockProvider::new(vec![ok(r#"{"criteria":["tests pass","the PR is filed"]}"#)]);
    let criteria = derive_criteria(&provider, "mock-model", "ship it").await;
    assert_eq!(criteria, strings(&["tests pass", "the PR is filed"]));
}

/// Models wrap JSON in code fences routinely.
#[tokio::test]
async fn parses_fenced_json() {
    let provider = MockProvider::new(vec![ok("```json\n{\"criteria\":[\"tests pass\"]}\n```")]);
    let criteria = derive_criteria(&provider, "mock-model", "ship it").await;
    assert_eq!(criteria, strings(&["tests pass"]));
}

/// Prose before and after the object is tolerated.
#[tokio::test]
async fn parses_json_with_prose_around_it() {
    let provider = MockProvider::new(vec![ok(
        "Here are the criteria:\n{\"criteria\":[\"tests pass\"]}\nHope that helps!",
    )]);
    let criteria = derive_criteria(&provider, "mock-model", "ship it").await;
    assert_eq!(criteria, strings(&["tests pass"]));
}

/// A bare array, and an array of objects, both parse.
#[test]
fn parses_bare_array_and_object_entries() {
    assert_eq!(
        parse_criteria_reply(r#"["tests pass","PR filed"]"#),
        strings(&["tests pass", "PR filed"])
    );
    assert_eq!(
        parse_criteria_reply(
            r#"{"criteria":[{"criterion":"tests pass"},{"criterion":"PR filed"}]}"#
        ),
        strings(&["tests pass", "PR filed"])
    );
}

/// Blanks are dropped, entries are trimmed, and the list is capped.
#[test]
fn sanitizes_and_caps_the_list() {
    let raw = r#"{"criteria":["  a  ","","   ","b","c","d","e","f","g"]}"#;
    let parsed = parse_criteria_reply(raw);
    assert_eq!(parsed, strings(&["a", "b", "c", "d", "e"]));
    assert_eq!(parsed.len(), MAX_CRITERIA);
}

// ---------------------------------------------------------------------------
// derive_criteria — the request
// ---------------------------------------------------------------------------

/// The goal travels in the user prompt; the system prompt defines *checkable*
/// and forbids unverifiable criteria.
#[tokio::test]
async fn request_carries_goal_and_checkability_rules() {
    let provider = MockProvider::new(vec![ok(r#"{"criteria":["a"]}"#)]);
    derive_criteria(&provider, "mock-model", "finish the upstream merge process").await;

    let request = &provider.captured_requests()[0];
    let system = request.system.clone().unwrap();
    assert!(system.contains("CHECKABLE"));
    assert!(system.contains("observable evidence"));
    assert!(system.contains("must never be emitted"));
    assert!(
        !system.contains("Be generous"),
        "the sycophancy licence is the #299 root cause"
    );

    let prompt = user_prompt(request);
    assert!(prompt.contains("finish the upstream merge process"));
}

/// The derivation call is small and bounded — it must not inherit the judge's
/// 4096-token budget.
#[tokio::test]
async fn request_uses_a_small_token_budget() {
    let provider = MockProvider::new(vec![ok(r#"{"criteria":["a"]}"#)]);
    derive_criteria(&provider, "mock-model", "ship it").await;
    assert_eq!(provider.captured_requests()[0].max_tokens, Some(1024));
}

// ---------------------------------------------------------------------------
// derive_criteria_if_needed
// ---------------------------------------------------------------------------

/// Explicit criteria win outright — no model call at all.
#[tokio::test]
async fn explicit_criteria_win_without_a_model_call() {
    let provider = MockProvider::new(vec![ok(r#"{"criteria":["derived"]}"#)]);
    let explicit = strings(&["tests pass", "PR filed"]);
    let criteria = derive_criteria_if_needed(&provider, "mock-model", "ship it", &explicit).await;
    assert_eq!(criteria, explicit);
    assert_eq!(provider.call_count(), 0);
}

/// Blank explicit entries are not criteria; the model is still consulted.
#[tokio::test]
async fn blank_explicit_criteria_fall_through_to_derivation() {
    let provider = MockProvider::new(vec![ok(r#"{"criteria":["derived"]}"#)]);
    let criteria =
        derive_criteria_if_needed(&provider, "mock-model", "ship it", &strings(&["", "  "])).await;
    assert_eq!(criteria, strings(&["derived"]));
    assert_eq!(provider.call_count(), 1);
}

/// When derivation fails the goal text becomes the single criterion, so the
/// goal is never stored with an empty list.
#[tokio::test]
async fn falls_back_to_the_goal_text_when_derivation_fails() {
    let provider = MockProvider::new(vec![err()]);
    let criteria =
        derive_criteria_if_needed(&provider, "mock-model", "finish the merge", &[]).await;
    assert_eq!(criteria, strings(&["finish the merge"]));
}

/// No explicit criteria and no goal text → nothing to store.
#[tokio::test]
async fn no_explicit_criteria_and_no_goal_yields_empty() {
    let provider = MockProvider::new(vec![err()]);
    let criteria = derive_criteria_if_needed(&provider, "mock-model", "  ", &[]).await;
    assert!(criteria.is_empty());
}

// ---------------------------------------------------------------------------
// Storage round-trip
// ---------------------------------------------------------------------------

/// Acceptance: stored TEXT parses back to the same Vec<String>.
#[test]
fn criteria_json_round_trips() {
    let criteria = strings(&[
        "tests pass",
        "the PR is filed on upstream",
        "cargo clippy clean",
    ]);
    let stored = serialize_criteria(&criteria);
    assert_eq!(parse_criteria(&stored), criteria);
}

/// Non-ASCII criteria survive the round trip byte-for-byte.
#[test]
fn round_trip_preserves_unicode_and_punctuation() {
    let criteria = strings(&[
        "gate is GREEN → swap ran",
        "«criteria» quoted \"text\"",
        "ok ✅",
    ]);
    let stored = serialize_criteria(&criteria);
    assert_eq!(parse_criteria(&stored), criteria);
}

#[test]
fn empty_criteria_round_trip_to_empty() {
    assert_eq!(serialize_criteria(&[]), "[]");
    assert!(parse_criteria("[]").is_empty());
    assert!(parse_criteria("").is_empty());
    assert!(parse_criteria("   ").is_empty());
}

/// A hand-written (non-JSON) value is kept as one criterion, not dropped —
/// dropping it would silently cap the goal at UNCERTAIN.
#[test]
fn non_json_stored_value_is_kept_as_one_criterion() {
    assert_eq!(
        parse_criteria("tests pass and the PR is filed"),
        strings(&["tests pass and the PR is filed"])
    );
}
