//! #996 — summarise then cut: the ordering invariant, driven through the real
//! entry point.
//!
//! The defect was an ORDER, so a test of the pieces would have passed while it
//! was live: `plan_head_chunk` and `compact_head_with_summary` are only correct
//! together, and only if `enforce_context_budget` calls them before anything
//! drops a message. Measured 2026-11-24T18:57Z: the 90% floor truncated first,
//! the summariser was then handed a window that no longer contained the
//! messages it was supposed to describe, and a 2,979-character "Obligation
//! status: UNKNOWN" stub was persisted as the session's compaction summary.
//!
//! So these tests assert the property that was violated:
//!
//! - the provider receives the OLDEST messages (the ones the old code dropped
//!   before the call ever happened), and receives a non-empty list;
//! - the outcome is `Summarised`, not `Truncated`;
//! - the newest messages survive the visit verbatim;
//! - and when the summariser fails outright, the safety net is what drops
//!   messages, with the `Truncated` marker saying so.
//!
//! `AgentService::new_for_test` is NOT used: it hardcodes `Config::default()`,
//! whose `background_compaction` is `true`, and that branch spawns a background
//! summariser instead of running the synchronous head-cut loop under test here.
//! The service is built with `AgentService::new` and the same default config
//! with `background_compaction = false`.

use crate::brain::agent::context::AgentContext;
use crate::brain::agent::service::AgentService;
use crate::brain::agent::service::compaction::{BudgetPhase, CompactionOutcome};
use crate::brain::provider::{
    ContentBlock, LLMRequest, LLMResponse, Provider, ProviderStream, Role, StopReason, TokenUsage,
};
use crate::db::Database;
use crate::services::ServiceContext;
use async_trait::async_trait;
use std::sync::{Arc, Mutex};
use uuid::Uuid;

/// A message body of a size the fixture can rely on: ~10 chars per token, so a
/// window can be filled with a handful of messages and the fill is predictable
/// without pinning a tokenizer version.
fn filler() -> String {
    "the quick brown fox jumps over the lazy dog ".repeat(200)
}

/// Fill a context until it is over 95% of `window`, labelling each message so
/// the test can say WHICH ones the summariser saw.
fn context_over_95(window: usize) -> (AgentContext, Vec<crate::brain::provider::Message>) {
    let mut context = AgentContext::new(Uuid::new_v4(), window);
    let mut originals = Vec::new();
    let mut i = 0usize;
    while context.usage_percentage() <= 95.0 {
        let msg = crate::brain::provider::Message::user(format!("msg-{i} {}", filler()));
        context.add_message(msg.clone());
        originals.push(msg);
        i += 1;
        assert!(i < 200, "fixture must reach the 90% floor");
    }
    assert!(
        context.usage_percentage() > 90.0,
        "the fixture must trip the old 90% floor, or it proves nothing about the order"
    );
    assert!(
        context.messages.len() > 4,
        "the fixture needs a head AND a tail, or the cut has nothing to keep"
    );
    (context, originals)
}

fn text_of(msg: &crate::brain::provider::Message) -> String {
    msg.content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// A provider that records every request it is sent and answers with a
/// continuation document — the shape `MockProvider` has, plus the ledger the
/// ordering assertion needs.
struct RecordingProvider {
    seen: Arc<Mutex<Vec<LLMRequest>>>,
}

impl RecordingProvider {
    fn new() -> Self {
        Self {
            seen: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn record(&self, request: &LLMRequest) {
        self.seen
            .lock()
            .expect("seen lock poisoned")
            .push(request.clone());
    }

    fn answer(&self, request: LLMRequest) -> LLMResponse {
        self.record(&request);
        LLMResponse {
            id: "head-cut-response-1".to_string(),
            model: "mock-model".to_string(),
            content: vec![ContentBlock::Text {
                text: "## 1. Summary\n\nThe oldest span of this session was summarised \
                       before it was cut (#996 test document)."
                    .to_string(),
            }],
            stop_reason: Some(StopReason::EndTurn),
            usage: TokenUsage {
                input_tokens: 10,
                output_tokens: 20,
                ..Default::default()
            },
            streaming_active_secs: None,
            tool_text_leak: false,
        }
    }
}

#[async_trait]
impl Provider for RecordingProvider {
    async fn complete(&self, request: LLMRequest) -> crate::brain::provider::Result<LLMResponse> {
        Ok(self.answer(request))
    }

    async fn stream(&self, request: LLMRequest) -> crate::brain::provider::Result<ProviderStream> {
        use crate::brain::provider::{ContentDelta, MessageDelta, StreamEvent, StreamMessage};

        let response = self.answer(request);
        let mut events = vec![Ok(StreamEvent::MessageStart {
            message: StreamMessage {
                id: response.id.clone(),
                model: response.model.clone(),
                role: Role::Assistant,
                usage: response.usage,
            },
        })];
        for (i, block) in response.content.iter().enumerate() {
            if let ContentBlock::Text { text } = block {
                events.push(Ok(StreamEvent::ContentBlockStart {
                    index: i,
                    content_block: ContentBlock::Text {
                        text: String::new(),
                    },
                }));
                events.push(Ok(StreamEvent::ContentBlockDelta {
                    index: i,
                    delta: ContentDelta::TextDelta { text: text.clone() },
                }));
                events.push(Ok(StreamEvent::ContentBlockStop { index: i }));
            }
        }
        events.push(Ok(StreamEvent::MessageDelta {
            delta: MessageDelta {
                stop_reason: response.stop_reason,
                stop_sequence: None,
            },
            usage: response.usage,
        }));
        events.push(Ok(StreamEvent::MessageStop));
        Ok(Box::pin(futures::stream::iter(events)))
    }

    fn name(&self) -> &str {
        "recording-mock"
    }

    fn default_model(&self) -> &str {
        "mock-model"
    }

    fn supported_models(&self) -> Vec<String> {
        vec!["mock-model".to_string()]
    }

    fn context_window(&self, _model: &str) -> Option<u32> {
        Some(32_000)
    }
}

/// A provider that always fails — the summariser is unavailable, so only the
/// safety net can move the floor.
struct AlwaysFailingProvider;

#[async_trait]
impl Provider for AlwaysFailingProvider {
    async fn complete(&self, _request: LLMRequest) -> crate::brain::provider::Result<LLMResponse> {
        Err(crate::brain::provider::ProviderError::ApiError {
            status: 500,
            message: "summariser unavailable (#996 test)".to_string(),
            error_type: None,
        })
    }

    async fn stream(&self, _request: LLMRequest) -> crate::brain::provider::Result<ProviderStream> {
        Err(crate::brain::provider::ProviderError::ApiError {
            status: 500,
            message: "summariser unavailable (#996 test)".to_string(),
            error_type: None,
        })
    }

    fn name(&self) -> &str {
        "always-failing-mock"
    }

    fn default_model(&self) -> &str {
        "mock-model"
    }

    fn supported_models(&self) -> Vec<String> {
        vec!["mock-model".to_string()]
    }

    fn context_window(&self, _model: &str) -> Option<u32> {
        Some(32_000)
    }
}

async fn service_with(provider: Arc<dyn Provider>) -> AgentService {
    let db = Database::connect_in_memory().await.unwrap();
    db.run_migrations().await.unwrap();
    let pool = db.pool().clone();
    let context = ServiceContext::new(pool);
    let mut config = crate::config::Config::default();
    // The synchronous head-cut loop is the path under test; backgrounding would
    // hand the summary to a spawned task and return before anything is cut.
    config.agent.background_compaction = false;
    AgentService::new(provider, context, &config).await
}

/// The regression #996 asks for: the summariser sees the OLDEST messages, the
/// outcome is a summary, and the newest messages survive verbatim.
#[tokio::test]
async fn the_summariser_sees_the_oldest_messages_before_anything_is_cut() {
    let provider = Arc::new(RecordingProvider::new());
    let seen = provider.seen.clone();
    let service = service_with(provider).await;

    let (mut context, originals) = context_over_95(32_000);
    let session_id = context.session_id;
    let before_len = context.messages.len();
    let newest = originals.last().cloned().expect("the fixture has a tail");

    let outcome = service
        .enforce_context_budget(
            session_id,
            &mut context,
            "mock-model",
            None,
            &None,
            BudgetPhase::MidLoop,
        )
        .await;

    // (b) a summary landed, not a truncation.
    match &outcome {
        Some(CompactionOutcome::Summarised(marker)) => {
            assert!(
                marker.contains("CONTEXT COMPACTION"),
                "the persisted marker is the one welded into the window (#1928)"
            );
        }
        other => panic!(
            "the visit must summarise, not truncate: {}",
            other
                .as_ref()
                .map(|o| o.marker(""))
                .unwrap_or_else(|| "no outcome at all".to_string())
        ),
    }

    // (a) the provider saw the oldest messages — the ones the old code dropped
    // BEFORE the call, which is the defect.
    let requests = seen.lock().expect("seen lock poisoned").clone();
    assert!(!requests.is_empty(), "the summariser must have been called");
    let first = &requests[0];
    assert!(
        !first.messages.is_empty(),
        "an empty summariser input is the 2026-11-24 stub, not a summary"
    );
    assert!(
        first.messages.iter().any(|m| text_of(m).contains("msg-0 ")),
        "the OLDEST message must reach the summariser; the old order truncated it away \
         before the call and left the model describing a window it had never seen"
    );

    // (c) the newest messages survive the visit verbatim.
    assert!(
        context.messages.len() < before_len,
        "the head must actually leave the window"
    );
    let kept_newest = context.messages.last().expect("the tail is not empty");
    assert_eq!(
        serde_json::to_string(kept_newest).unwrap(),
        serde_json::to_string(&newest).unwrap(),
        "the newest message must survive byte-identically"
    );
    assert!(
        !text_of(kept_newest).is_empty() && text_of(kept_newest).contains("msg-"),
        "and it must still be the conversation, not a marker"
    );
}

/// The other half: with no summariser, the safety net runs — and it says so.
#[tokio::test]
async fn a_failed_summariser_falls_through_to_the_truncation_marker() {
    let service = service_with(Arc::new(AlwaysFailingProvider)).await;

    let (mut context, _originals) = context_over_95(32_000);
    let session_id = context.session_id;
    let before_len = context.messages.len();

    let outcome = service
        .enforce_context_budget(
            session_id,
            &mut context,
            "mock-model",
            None,
            &None,
            BudgetPhase::MidLoop,
        )
        .await;

    assert!(
        matches!(outcome, Some(CompactionOutcome::Truncated)),
        "with no summary the visit must persist the truncation marker, not a summary"
    );
    assert!(
        context.messages.len() < before_len,
        "the safety net must be what drops the messages"
    );
    assert!(
        context.usage_percentage() <= 85.0,
        "the safety net cuts to 80% of the window (was {:.1}%)",
        context.usage_percentage()
    );
}
