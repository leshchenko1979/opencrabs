//! Regression for #813: the `send_message` / channel path cleans persisted
//! reasoning out of the context it sends.
//!
//! `run_tool_loop` has stripped `<!-- reasoning -->` blocks and
//! `<!-- tools-v2 -->` ledgers from loaded rows since #1522. The OTHER
//! production assembly site, `prepare_message_context_with_display`, did not:
//! it went straight from `load_messages_since_last_compaction` to
//! `AgentContext::from_db_messages`, so every channel entry point that reaches
//! it (model-switch confirmations, reaction guidance, queued mid-turn
//! feedback, interaction replies) replayed the session's whole reasoning and
//! tool ledger to the provider as prompt text.
//!
//! The measurement behind the issue: one Telegram session held 710 rows and
//! 47.1 MB of persisted reasoning, 73.2% of its bytes, and reasoning was 79.3%
//! of the tokens in the window the provider received.

use std::sync::Arc;

use async_trait::async_trait;

use crate::brain::agent::service::builder::AgentService;
use crate::brain::provider::{
    ContentBlock, LLMRequest, LLMResponse, Provider, ProviderStream, StopReason, TokenUsage,
};
use crate::db::Database;
use crate::services::{MessageService, ServiceContext, SessionService};

/// Records the request the service hands the provider, so the assertion is
/// about what actually would have been sent rather than about internals.
struct CapturingProvider {
    captured: Arc<tokio::sync::Mutex<Option<LLMRequest>>>,
}

impl CapturingProvider {
    fn new() -> (Self, Arc<tokio::sync::Mutex<Option<LLMRequest>>>) {
        let captured = Arc::new(tokio::sync::Mutex::new(None));
        (
            Self {
                captured: Arc::clone(&captured),
            },
            captured,
        )
    }
}

#[async_trait]
impl Provider for CapturingProvider {
    async fn complete(&self, request: LLMRequest) -> crate::brain::provider::Result<LLMResponse> {
        *self.captured.lock().await = Some(request);
        Ok(LLMResponse {
            id: "captured".to_string(),
            model: "capture-model".to_string(),
            resolved_model: None,
            content: vec![ContentBlock::Text {
                text: "ack".to_string(),
            }],
            stop_reason: Some(StopReason::EndTurn),
            usage: TokenUsage {
                input_tokens: 5,
                output_tokens: 1,
                ..Default::default()
            },
            streaming_active_secs: None,
            tool_text_leak: false,
        })
    }

    async fn stream(&self, request: LLMRequest) -> crate::brain::provider::Result<ProviderStream> {
        // `send_message` takes the non-streaming path; capture for symmetry.
        self.complete(request).await?;
        let events: Vec<crate::brain::provider::Result<crate::brain::provider::StreamEvent>> =
            Vec::new();
        Ok(Box::pin(futures::stream::iter(events)))
    }

    fn name(&self) -> &str {
        "capturing"
    }

    fn default_model(&self) -> &str {
        "capture-model"
    }

    fn supported_models(&self) -> Vec<String> {
        vec!["capture-model".to_string()]
    }

    fn context_window(&self, _model: &str) -> Option<u32> {
        Some(200_000)
    }

    fn calculate_cost(&self, _model: &str, _input: u32, _output: u32) -> f64 {
        0.0
    }
}

/// Every text block the provider would receive, joined.
fn prompt_text(request: &LLMRequest) -> String {
    request
        .messages
        .iter()
        .flat_map(|m| m.content.iter())
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            ContentBlock::Thinking { thinking, .. } => Some(thinking.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// An assistant row the way the tool loop persists it: a reasoning block, the
/// visible answer, and a replay ledger far larger than both.
fn persisted_reasoning_row() -> String {
    let ledger_entry = r#"{"d":"bash: cargo test","i":{"command":"cargo test"},"o":"ok"}"#;
    let ledger = format!(
        "<!-- tools-v2: [{}] -->",
        std::iter::repeat_n(ledger_entry, 20)
            .collect::<Vec<_>>()
            .join(",")
    );
    let reasoning = format!(
        "<!-- reasoning -->\n{}\n<!-- /reasoning -->\n\n",
        "The user asked for the suite before the edit lands. ".repeat(40)
    );
    format!("{reasoning}Suite green, landing the edit now.\n\n{ledger}")
}

#[tokio::test]
async fn send_message_does_not_replay_persisted_reasoning() {
    let db = Database::connect_in_memory().await.unwrap();
    db.run_migrations().await.unwrap();
    let context = ServiceContext::new(db.pool().clone());

    let session_service = SessionService::new(context.clone());
    let session = session_service
        .create_session(Some("Reasoning Replay".to_string()))
        .await
        .unwrap();

    let message_service = MessageService::new(context.clone());
    let persisted = persisted_reasoning_row();
    message_service
        .create_message(session.id, "user".to_string(), "run the suite".to_string())
        .await
        .unwrap();
    message_service
        .create_message(session.id, "assistant".to_string(), persisted.clone())
        .await
        .unwrap();

    let (provider, captured) = CapturingProvider::new();
    let agent_service = AgentService::new_for_test(Arc::new(provider), context).await;

    agent_service
        .send_message(session.id, "and the next step?".to_string(), None)
        .await
        .unwrap();

    let request = captured
        .lock()
        .await
        .clone()
        .expect("the provider was called");
    let text = prompt_text(&request);

    assert!(
        !text.contains("<!-- reasoning -->") && !text.contains("<!-- /reasoning -->"),
        "persisted reasoning markers must not reach the provider; got {} chars",
        text.len()
    );
    assert!(
        !text.contains("The user asked for the suite before the edit lands."),
        "the reasoning BODY must be gone too, not just its markers"
    );
    assert!(
        !text.contains("<!-- tools-v2"),
        "the tool replay ledger must not reach the provider"
    );
    assert!(
        text.contains("Suite green, landing the edit now."),
        "the visible answer must survive the clean"
    );
    assert!(
        text.contains("and the next step?"),
        "the turn's own user message must still be in the prompt"
    );

    // Rows are append-only: the clean is in-memory only, so replay (TUI,
    // session reload) still has its ledgers.
    let stored = message_service
        .list_messages_for_session(session.id)
        .await
        .expect("rows readable");
    let assistant = stored
        .iter()
        .find(|m| m.role == "assistant" && m.content.contains("Suite green"))
        .expect("the persisted assistant row is still there");
    assert!(
        assistant.content.contains("<!-- reasoning -->"),
        "the stored row must keep its reasoning — only the in-memory copy is cleaned"
    );
    assert_eq!(
        assistant.content, persisted,
        "the clean must not rewrite the persisted row"
    );
}
