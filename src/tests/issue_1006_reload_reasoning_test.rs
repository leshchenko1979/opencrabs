//! Investigation tests for #1006: across-turn reasoning after DB reload.
//!
//! Pins the reload path:
//! - `clean_rows_for_llm` hoists `<!-- reasoning -->` into `thinking` when
//!   `preserve_thinking` is true (Moonshot/Kimi + Qwen/DashScope today).
//! - `strip_llm_artifacts` removes `<!-- tools-v2 -->` ledgers and reasoning
//!   blocks when hoisting is off.
//! - `from_db_messages` → `to_openai_request` emits `reasoning_content` only
//!   when a `ContentBlock::Thinking` is present (not from the Moonshot blank
//!   fallback on plain-text rows).
//!
//! In-loop tool-call rows are unaffected: they never pass through
//! `clean_rows_for_llm` mid-turn, and the encoder still requires
//! `reasoning_content` when `tool_calls` are present on Kimi routes.

use crate::brain::agent::context::AgentContext;
use crate::brain::agent::service::context_rows::clean_rows_for_llm;
use crate::brain::provider::custom_openai_compatible::{
    OpenAIProvider, needs_reasoning_content_for, preserves_thinking,
};
use crate::brain::provider::qwen_reasoning::preserve_thinking_for;
use crate::brain::provider::{ContentBlock, LLMRequest, Message, Role};
use crate::db::models::Message as DbMessage;
use crate::utils::sanitize::strip_llm_artifacts;
use chrono::Utc;
use uuid::Uuid;

fn db_row(role: &str, content: &str, thinking: Option<&str>) -> DbMessage {
    DbMessage {
        id: Uuid::new_v4(),
        session_id: Uuid::new_v4(),
        role: role.to_string(),
        content: content.to_string(),
        sequence: 0,
        created_at: Utc::now(),
        token_count: None,
        cost: None,
        input_tokens: None,
        cache_creation_tokens: None,
        cache_read_tokens: None,
        thinking: thinking.map(String::from),
        duration_secs: None,
    }
}

/// Turn-1 assistant row as persisted by the tool loop: reasoning markers,
/// visible answer, and a tools-v2 replay ledger (stripped before the provider
/// sees the row again on the next user turn).
fn turn_one_assistant_persisted() -> DbMessage {
    let ledger = r#"<!-- tools-v2: [{"d":"bash","o":"ok"}] -->"#;
    let body = format!(
        "<!-- reasoning -->\nTurn-1 chain of thought about the task.\n<!-- /reasoning -->\n\n\
         Turn-1 visible answer.\n\n{ledger}"
    );
    db_row("assistant", &body, None)
}

fn moonshot_provider() -> OpenAIProvider {
    OpenAIProvider::with_base_url(
        "test-key".to_string(),
        "https://api.moonshot.ai/v1/chat/completions".to_string(),
    )
    .with_name("moonshot")
}

fn dashscope_qwen_provider() -> OpenAIProvider {
    OpenAIProvider::with_base_url(
        "test-key".to_string(),
        "https://dashscope.aliyuncs.com/compatible-mode/v1".to_string(),
    )
    .with_name("dashscope")
}

fn assistant_tool_call_with_thinking(thinking: &str) -> Message {
    Message {
        role: Role::Assistant,
        content: vec![
            ContentBlock::Thinking {
                thinking: thinking.to_string(),
                signature: None,
            },
            ContentBlock::ToolUse {
                id: "call_1".to_string(),
                name: "bash".to_string(),
                input: serde_json::json!({"command": "echo hi"}),
            },
        ],
    }
}

fn reasoning_content_on_assistant(body: &serde_json::Value, index: usize) -> Option<String> {
    body["messages"][index]["reasoning_content"]
        .as_str()
        .map(String::from)
}

// ─── strip_llm_artifacts / hoist contract ───────────────────────────

#[test]
fn strip_llm_artifacts_removes_tools_v2_and_reasoning_without_hoist() {
    let raw = turn_one_assistant_persisted().content;
    let out = strip_llm_artifacts(&raw);
    assert!(!out.contains("tools-v2"), "ledger must be stripped: {out}");
    assert!(
        !out.contains("Turn-1 chain of thought"),
        "reasoning prose must not survive strip: {out}"
    );
    assert_eq!(out.trim(), "Turn-1 visible answer.");
}

#[test]
fn clean_rows_without_preserve_thinking_drops_reasoning_and_strips_ledger() {
    let mut rows = vec![turn_one_assistant_persisted()];
    clean_rows_for_llm(&mut rows, false);
    assert!(rows[0].thinking.is_none());
    assert!(!rows[0].content.contains("tools-v2"));
    assert!(!rows[0].content.contains("chain of thought"));
    assert!(rows[0].content.contains("Turn-1 visible answer."));
}

#[test]
fn clean_rows_with_preserve_thinking_hoists_reasoning_and_strips_ledger() {
    let mut rows = vec![turn_one_assistant_persisted()];
    clean_rows_for_llm(&mut rows, true);
    assert!(
        rows[0]
            .thinking
            .as_deref()
            .is_some_and(|t| t.contains("Turn-1 chain of thought"))
    );
    assert!(!rows[0].content.contains("tools-v2"));
    assert!(!rows[0].content.contains("<!--"));
    assert_eq!(rows[0].content.trim(), "Turn-1 visible answer.");
}

// ─── Two-turn reload fixture → OpenAI payload shape ─────────────────

#[test]
fn current_reload_path_re_emits_turn_one_reasoning_for_moonshot() {
    let mut rows = vec![
        turn_one_assistant_persisted(),
        db_row("user", "Turn-2 follow-up question.", None),
    ];
    clean_rows_for_llm(&mut rows, true);

    let ctx = AgentContext::from_db_messages(Uuid::new_v4(), rows, 200_000);
    let req = LLMRequest::new("kimi-k2.6".to_string(), ctx.messages);
    let body = serde_json::to_value(moonshot_provider().to_openai_request(req)).unwrap();
    let asst = &body["messages"][0];
    assert_eq!(asst["role"], "assistant");
    assert!(
        asst.get("tool_calls").is_none_or(|v| v.is_null()),
        "reloaded turn-1 row must be plain text, not tool_calls: {asst}"
    );
    assert_eq!(
        reasoning_content_on_assistant(&body, 0).as_deref(),
        Some("Turn-1 chain of thought about the task.")
    );
}

#[test]
fn drop_path_omits_turn_one_reasoning_for_moonshot_plain_text() {
    let mut rows = vec![
        turn_one_assistant_persisted(),
        db_row("user", "Turn-2 follow-up question.", None),
    ];
    clean_rows_for_llm(&mut rows, false);

    let ctx = AgentContext::from_db_messages(Uuid::new_v4(), rows, 200_000);
    let req = LLMRequest::new("kimi-k2.6".to_string(), ctx.messages);
    let body = serde_json::to_value(moonshot_provider().to_openai_request(req)).unwrap();
    assert!(
        body["messages"][0]
            .get("reasoning_content")
            .is_none_or(|v| v.is_null()),
        "no Thinking block → no reasoning_content on plain assistant: {}",
        body["messages"][0]
    );
}

#[test]
fn current_reload_path_re_emits_turn_one_reasoning_for_qwen() {
    let mut rows = vec![
        turn_one_assistant_persisted(),
        db_row("user", "Turn-2 follow-up question.", None),
    ];
    clean_rows_for_llm(&mut rows, true);

    let ctx = AgentContext::from_db_messages(Uuid::new_v4(), rows, 200_000);
    let req = LLMRequest::new("qwen3.8-max-preview".to_string(), ctx.messages);
    let body = serde_json::to_value(dashscope_qwen_provider().to_openai_request(req)).unwrap();
    assert_eq!(
        reasoning_content_on_assistant(&body, 0).as_deref(),
        Some("Turn-1 chain of thought about the task.")
    );
    assert_eq!(body["preserve_thinking"].as_bool(), Some(true));
}

#[test]
fn drop_path_omits_turn_one_reasoning_for_qwen_plain_text() {
    let mut rows = vec![
        turn_one_assistant_persisted(),
        db_row("user", "Turn-2 follow-up question.", None),
    ];
    clean_rows_for_llm(&mut rows, false);

    let ctx = AgentContext::from_db_messages(Uuid::new_v4(), rows, 200_000);
    let req = LLMRequest::new("qwen3.8-max-preview".to_string(), ctx.messages);
    let body = serde_json::to_value(dashscope_qwen_provider().to_openai_request(req)).unwrap();
    assert!(
        body["messages"][0]
            .get("reasoning_content")
            .is_none_or(|v| v.is_null()),
        "plain reloaded assistant without hoist: {}",
        body["messages"][0]
    );
}

// ─── In-loop tool-call guard (must not drop reasoning mid-turn) ─────

#[test]
fn moonshot_in_loop_tool_call_still_emits_reasoning_content() {
    let req = LLMRequest::new(
        "kimi-k2.6".to_string(),
        vec![
            Message::user("run bash".to_string()),
            assistant_tool_call_with_thinking("Live-loop thinking before bash."),
        ],
    );
    let body = serde_json::to_value(moonshot_provider().to_openai_request(req)).unwrap();
    let asst = &body["messages"][1];
    assert!(asst["tool_calls"].is_array());
    assert_eq!(
        reasoning_content_on_assistant(&body, 1).as_deref(),
        Some("Live-loop thinking before bash.")
    );
}

#[test]
fn moonshot_tool_call_without_thinking_uses_non_empty_placeholder() {
    let mut msg = assistant_tool_call_with_thinking("x");
    msg.content
        .retain(|b| !matches!(b, ContentBlock::Thinking { .. }));
    let req = LLMRequest::new(
        "kimi-k2.6".to_string(),
        vec![Message::user("run bash".to_string()), msg],
    );
    let body = serde_json::to_value(moonshot_provider().to_openai_request(req)).unwrap();
    let rc = body["messages"][1]["reasoning_content"]
        .as_str()
        .expect("kimi tool_call row must carry reasoning_content");
    assert!(!rc.is_empty(), "Moonshot treats empty as missing");
    assert!(needs_reasoning_content_for(
        "https://api.moonshot.ai/v1/chat/completions",
        "kimi-k2.6"
    ));
}

// ─── Load-site wiring (production still hoists today) ───────────────

#[test]
fn load_site_still_passes_preserves_thinking_into_clean_rows() {
    const SRC: &str = include_str!("../brain/agent/service/tool_loop.rs");
    let call = SRC
        .find("context_rows::clean_rows_for_llm(&mut db_messages, preserve_thinking)")
        .expect("tool_loop reload clean");
    let window = &SRC[call.saturating_sub(500)..call + 80];
    assert!(
        window.contains("preserves_thinking("),
        "preserve_thinking must gate the hoist"
    );
    assert!(
        preserves_thinking(Some("https://api.moonshot.ai/v1"), "kimi-k2.6")
            && preserves_thinking(
                Some("https://dashscope.aliyuncs.com/compatible-mode/v1"),
                "qwen3.8-max-preview"
            ),
        "both families are preserve_thinking targets today"
    );
}

#[test]
fn qwen_request_preserves_thinking_flag_independent_of_hoist() {
    assert!(preserve_thinking_for(
        "https://dashscope.aliyuncs.com/compatible-mode/v1",
        "qwen3.8-max-preview"
    ));
}
