//! Tier-0 offload progress + Telegram notice (#1020).

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use uuid::Uuid;

use crate::brain::agent::context::AgentContext;
use crate::brain::agent::service::compaction::{BudgetPhase, CompactionOutcome};
use crate::brain::agent::service::compaction_offload::OFFLOAD_MIN_RESULT_BYTES;
use crate::brain::agent::service::{AgentService, ProgressCallback, ProgressEvent};
use crate::brain::provider::{
    ContentBlock, ContentDelta, LLMRequest, LLMResponse, Message, MessageDelta, Provider,
    ProviderStream, Role, StopReason, StreamEvent, StreamMessage, TokenUsage,
};
use crate::channels::telegram::flow::{
    ToolOffloadNotice, compacted_flow_line, tool_offload_notice,
};
use crate::channels::telegram::intermediates::is_compaction_card;
use crate::db::Database;
use crate::services::ServiceContext;

fn tool_body() -> String {
    "line\n".repeat(OFFLOAD_MIN_RESULT_BYTES / 4 + 400)
}

fn user_tool(id: &str, body: &str) -> Message {
    Message {
        role: Role::User,
        content: vec![ContentBlock::ToolResult {
            tool_use_id: id.to_string(),
            content: body.to_string(),
            is_error: None,
        }],
    }
}

fn progress_recorder() -> (ProgressCallback, Arc<Mutex<Vec<ProgressEvent>>>) {
    let log = Arc::new(Mutex::new(Vec::new()));
    let sink = log.clone();
    let cb: ProgressCallback = Arc::new(move |_sid, event| {
        sink.lock().unwrap().push(event);
    });
    (cb, log)
}

async fn service_no_background(provider: Arc<dyn Provider>) -> AgentService {
    let db = Database::connect_in_memory().await.unwrap();
    db.run_migrations().await.unwrap();
    let mut config = crate::config::Config::default();
    config.agent.background_compaction = false;
    AgentService::new(provider, ServiceContext::new(db.pool().clone()), &config).await
}

/// Provider that answers summariser calls and counts how many times it ran.
struct CountingSummariser {
    calls: Arc<Mutex<usize>>,
}

impl CountingSummariser {
    fn new() -> (Arc<Self>, Arc<Mutex<usize>>) {
        let calls = Arc::new(Mutex::new(0));
        (
            Arc::new(Self {
                calls: calls.clone(),
            }),
            calls,
        )
    }
}

impl CountingSummariser {
    fn answer(&self, request: LLMRequest) -> LLMResponse {
        *self.calls.lock().unwrap() += 1;
        LLMResponse {
            id: "offload-notice-test".to_string(),
            model: request.model,
            resolved_model: None,
            content: vec![ContentBlock::Text {
                text: "## 1. Summary\n\nTest compaction document.".to_string(),
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
impl Provider for CountingSummariser {
    async fn complete(&self, request: LLMRequest) -> crate::brain::provider::Result<LLMResponse> {
        Ok(self.answer(request))
    }

    async fn stream(&self, request: LLMRequest) -> crate::brain::provider::Result<ProviderStream> {
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
        "counting-mock"
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

    fn calculate_cost(&self, _model: &str, _input: u32, _output: u32) -> f64 {
        0.0
    }
}

fn context_offload_alone() -> AgentContext {
    let session_id = Uuid::new_v4();
    let window = 32_000usize;
    let mut ctx = AgentContext::new(session_id, window);
    let body = tool_body();
    for i in 0..45 {
        ctx.add_message(Message::assistant(format!("step {i}")));
        ctx.add_message(user_tool(&format!("call_{i}"), &body));
    }
    assert!(
        ctx.usage_percentage() > 65.0,
        "fixture must start over the trigger, got {:.1}%",
        ctx.usage_percentage()
    );
    ctx
}

fn filler() -> String {
    "the quick brown fox jumps over the lazy dog ".repeat(80)
}

fn context_offload_then_summarise() -> AgentContext {
    let session_id = Uuid::new_v4();
    let window = 48_000usize;
    let mut ctx = AgentContext::new(session_id, window);
    let body = tool_body();
    let pad = filler().repeat(12);
    for i in 0..10 {
        ctx.add_message(Message::assistant(format!("step {i}")));
        ctx.add_message(user_tool(&format!("call_{i}"), &body));
        ctx.add_message(Message::user(format!("pad-{i} {pad}")));
    }
    assert!(
        ctx.usage_percentage() > 65.0,
        "fixture must start over the trigger, got {:.1}%",
        ctx.usage_percentage()
    );
    ctx
}

fn had_tool_offload(events: &[ProgressEvent]) -> bool {
    events
        .iter()
        .any(|e| matches!(e, ProgressEvent::ToolOutputOffloaded { .. }))
}

fn had_compacting(events: &[ProgressEvent]) -> bool {
    events
        .iter()
        .any(|e| matches!(e, ProgressEvent::Compacting { .. }))
}

#[test]
fn offload_notice_is_visually_distinct_from_summariser_card() {
    let notice = tool_offload_notice(&ToolOffloadNotice {
        blocks_offloaded: 4,
        tokens_freed: 12_000,
        before_pct: 71.0,
        after_pct: 58.0,
        before_tokens: 71_000,
        after_tokens: 58_000,
        ctx_max: 100_000,
        cache_hint: None,
    });
    assert!(notice.starts_with("📎 offloaded:"));
    assert!(notice.contains("tool-output offloaded"));
    assert!(!notice.contains("| scope |"));
    assert!(!notice.contains("post-compaction"));
    assert!(
        is_compaction_card(&notice),
        "offload notice must promote as a compaction receipt (#1023)"
    );
    let summary_head = compacted_flow_line(71.0, 58.0, std::time::Duration::from_secs(1));
    assert!(summary_head.starts_with("🧹 compact:"));
    assert_ne!(notice.lines().next().unwrap(), summary_head.as_str());
}

#[tokio::test]
async fn offload_alone_emits_tool_output_offloaded_without_summariser() {
    let (provider, calls) = CountingSummariser::new();
    let service = service_no_background(provider).await;
    let mut ctx = context_offload_alone();
    let session_id = ctx.session_id;
    let (cb, log) = progress_recorder();

    let outcome = service
        .enforce_context_budget(
            session_id,
            &mut ctx,
            "mock-model",
            None,
            &Some(cb),
            BudgetPhase::MidLoop,
        )
        .await;

    assert!(
        ctx.usage_percentage() <= 65.0,
        "offload alone must bring fill back under the trigger, got {:.1}%",
        ctx.usage_percentage()
    );
    assert!(
        !matches!(outcome, Some(CompactionOutcome::Summarised(_))),
        "summariser must not run when offload suffices"
    );
    assert_eq!(*calls.lock().unwrap(), 0, "summariser must not run");
    let events = log.lock().unwrap();
    assert!(had_tool_offload(&events));
    assert!(!had_compacting(&events));
    let ProgressEvent::ToolOutputOffloaded {
        blocks_offloaded,
        tokens_freed,
        before_pct,
        after_pct,
        ..
    } = events
        .iter()
        .find(|e| matches!(e, ProgressEvent::ToolOutputOffloaded { .. }))
        .expect("ToolOutputOffloaded event")
    else {
        unreachable!()
    };
    assert!(*blocks_offloaded >= 1);
    assert!(*tokens_freed > 0);
    assert!(*before_pct > *after_pct);
}

#[tokio::test]
async fn infeasible_gated_offload_leaves_summariser_notice_only() {
    let (provider, calls) = CountingSummariser::new();
    let service = service_no_background(provider).await;
    let mut ctx = context_offload_then_summarise();
    let session_id = ctx.session_id;
    let (cb, log) = progress_recorder();

    let outcome = service
        .enforce_context_budget(
            session_id,
            &mut ctx,
            "mock-model",
            None,
            &Some(cb),
            BudgetPhase::MidLoop,
        )
        .await;

    assert!(
        matches!(outcome, Some(CompactionOutcome::Summarised(_))),
        "fixture must still need the summariser"
    );
    assert!(*calls.lock().unwrap() >= 1);
    let events = log.lock().unwrap();
    assert!(
        !had_tool_offload(&events),
        "#1010 all-or-nothing gated offload must not partially run"
    );
    assert!(had_compacting(&events));
}
