//! #778: a summariser that answers with NO document is asked once more.
//!
//! A reasoning model spends the summariser's whole output allowance thinking
//! and returns nothing usable. Measured 2026-10-15: 42 `MaxTokens` stops in a
//! single day, 8 of them empty. An empty document is worse than no document —
//! `enforce_summary_budget` can only re-attach the `context-manifest` fence and
//! the §0 obligation line from a document that already carries them, so a stub
//! lands fence-less and the resuming turn loses its manifest. One session
//! compacted four times in two minutes because an empty summary bought it no
//! room.
//!
//! The fallback chain never sees this failure: a provider that answers with
//! nothing is a SUCCESS to it. So the retry has to sit above the walk, in
//! `complete_compaction_request_with_retry` — which is what these tests pin.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use crate::brain::agent::service::AgentService;
use crate::brain::provider::{
    ContentBlock, ContentDelta, LLMRequest, LLMResponse, Message, Provider, ProviderStream, Role,
    StopReason, StreamEvent, StreamMessage, TokenUsage,
};

/// Long enough that no mock in this file can reach it: these tests are about
/// how many times the summariser is asked, not how long one attempt is given.
const ATTEMPT_DEADLINE: std::time::Duration = std::time::Duration::from_secs(300);

/// Answers `empties` documents with no text, then produces one. `usize::MAX`
/// models a provider that never produces a document at all.
struct EmptyFirstMock {
    name: String,
    empties: usize,
    calls: Arc<AtomicUsize>,
}

impl EmptyFirstMock {
    fn new(name: &str, empties: usize) -> Self {
        Self {
            name: name.to_string(),
            empties,
            calls: Arc::new(AtomicUsize::new(0)),
        }
    }

    fn call_counter(&self) -> Arc<AtomicUsize> {
        self.calls.clone()
    }

    /// Counts the call and reports whether THIS one comes back empty.
    fn is_empty_call(&self) -> bool {
        self.calls.fetch_add(1, Ordering::SeqCst) < self.empties
    }
}

#[async_trait]
impl Provider for EmptyFirstMock {
    async fn complete(
        &self,
        request: LLMRequest,
    ) -> crate::brain::provider::error::Result<LLMResponse> {
        let empty = self.is_empty_call();
        Ok(LLMResponse {
            id: format!("{}-response", self.name),
            model: request.model,
            content: if empty {
                Vec::new()
            } else {
                vec![ContentBlock::Text {
                    text: format!("summary from {}", self.name),
                }]
            },
            // The measured shape: the allowance was spent, the document was not
            // written. An empty stop is what the retry exists for.
            stop_reason: empty.then_some(StopReason::MaxTokens),
            usage: TokenUsage::default(),
            streaming_active_secs: None,
            tool_text_leak: false,
        })
    }

    /// Compaction asks a non-CLI provider through the stream path and folds the
    /// events into one response (#1519), so the mock answers both the same way.
    /// An empty document is a stream that never emits a text delta.
    async fn stream(
        &self,
        request: LLMRequest,
    ) -> crate::brain::provider::error::Result<ProviderStream> {
        let empty = self.is_empty_call();
        let mut events = vec![Ok(StreamEvent::MessageStart {
            message: StreamMessage {
                id: format!("{}-response", self.name),
                model: request.model,
                role: Role::Assistant,
                usage: TokenUsage::default(),
            },
        })];
        if !empty {
            events.push(Ok(StreamEvent::ContentBlockDelta {
                index: 0,
                delta: ContentDelta::TextDelta {
                    text: format!("summary from {}", self.name),
                },
            }));
        }
        events.push(Ok(StreamEvent::MessageStop));
        Ok(Box::pin(futures::stream::iter(events)))
    }

    fn name(&self) -> &str {
        &self.name
    }

    fn default_model(&self) -> &str {
        "mock-default"
    }

    fn supported_models(&self) -> Vec<String> {
        Vec::new()
    }

    fn context_window(&self, _model: &str) -> Option<u32> {
        Some(200_000)
    }

    fn calculate_cost(&self, _model: &str, _input_tokens: u32, _output_tokens: u32) -> f64 {
        0.0
    }
}

fn request(model: &str) -> LLMRequest {
    LLMRequest::new(model.to_string(), vec![Message::user("compact me")])
}

/// The production text extractor is `pub(super)` inside the service module, so
/// mirror it here rather than widening its visibility for a test.
fn text_of(response: &LLMResponse) -> String {
    response
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// The headline regression: the summariser answers with nothing, and the
/// wrapper asks again instead of handing an empty document downstream.
#[tokio::test]
async fn empty_document_is_retried_once() {
    let mock = EmptyFirstMock::new("cr-empty-first", 1);
    let calls = mock.call_counter();
    let primary: Arc<dyn Provider> = Arc::new(mock);

    let response = AgentService::complete_compaction_request_with_retry(
        &primary,
        &[],
        request("primary-model"),
        &CancellationToken::new(),
        ATTEMPT_DEADLINE,
        None,
    )
    .await
    .expect("#778: the retry must surface the second attempt's document");

    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "an empty document must be retried exactly once"
    );
    assert_eq!(
        text_of(&response),
        "summary from cr-empty-first",
        "the retry's document is the one that must reach the compaction path"
    );
}

/// The retry is for EMPTY documents only. A document that arrived on the first
/// attempt must not cost a second request — a summariser that answers costs the
/// user real tokens and real seconds.
#[tokio::test]
async fn non_empty_document_is_not_retried() {
    let mock = EmptyFirstMock::new("cr-first-try", 0);
    let calls = mock.call_counter();
    let primary: Arc<dyn Provider> = Arc::new(mock);

    let response = AgentService::complete_compaction_request_with_retry(
        &primary,
        &[],
        request("primary-model"),
        &CancellationToken::new(),
        ATTEMPT_DEADLINE,
        None,
    )
    .await
    .expect("a first-attempt document must be returned as-is");

    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "a non-empty document must not trigger the retry"
    );
    assert_eq!(text_of(&response), "summary from cr-first-try");
}

/// A provider that is simply broken must not be asked forever: exactly one
/// retry, then whatever came back is surfaced. The caller still has to decide
/// what an empty document means — silence here would be worse.
#[tokio::test]
async fn persistently_empty_document_stops_after_one_retry() {
    let mock = EmptyFirstMock::new("cr-never-answers", usize::MAX);
    let calls = mock.call_counter();
    let primary: Arc<dyn Provider> = Arc::new(mock);

    let response = AgentService::complete_compaction_request_with_retry(
        &primary,
        &[],
        request("primary-model"),
        &CancellationToken::new(),
        ATTEMPT_DEADLINE,
        None,
    )
    .await
    .expect("an empty document is not an error — it is returned, not raised");

    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "one retry, never a loop"
    );
    assert!(
        text_of(&response).is_empty(),
        "a persistently empty summariser surfaces the empty document it got"
    );
}
