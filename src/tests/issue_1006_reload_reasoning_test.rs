//! Across-turn reasoning on DB reload (#1006).
//!
//! Qwen/DashScope: no hoist on reload → no historical `reasoning_content` on
//! plain assistant rows; request still carries `preserve_thinking: true`.
//! Moonshot/Kimi: hoist unchanged until live K3 verification.
//! In-loop tool-call rows skip `clean_rows_for_llm` and still replay thinking.

use crate::brain::agent::context::AgentContext;
use crate::brain::agent::service::context_rows::clean_rows_for_llm;
use crate::brain::provider::custom_openai_compatible::{
    OpenAIProvider, hoist_thinking_on_db_reload, needs_reasoning_content_for, preserves_thinking,
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

fn apply_reload_clean(rows: &mut [DbMessage], base_url: &str, model: &str) {
    let hoist = hoist_thinking_on_db_reload(Some(base_url), model);
    clean_rows_for_llm(rows, hoist);
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
fn clean_rows_without_hoist_drops_reasoning_and_strips_ledger() {
    let mut rows = vec![turn_one_assistant_persisted()];
    clean_rows_for_llm(&mut rows, false);
    assert!(rows[0].thinking.is_none());
    assert!(!rows[0].content.contains("tools-v2"));
    assert!(!rows[0].content.contains("chain of thought"));
    assert!(rows[0].content.contains("Turn-1 visible answer."));
}

#[test]
fn clean_rows_with_hoist_keeps_reasoning_in_thinking_column() {
    let mut rows = vec![turn_one_assistant_persisted()];
    clean_rows_for_llm(&mut rows, true);
    assert!(
        rows[0]
            .thinking
            .as_deref()
            .is_some_and(|t| t.contains("Turn-1 chain of thought"))
    );
    assert!(!rows[0].content.contains("tools-v2"));
    assert_eq!(rows[0].content.trim(), "Turn-1 visible answer.");
}

#[test]
fn hoist_predicate_excludes_qwen_includes_kimi() {
    assert!(hoist_thinking_on_db_reload(
        Some("https://api.moonshot.ai/v1"),
        "kimi-k2.6"
    ));
    assert!(!hoist_thinking_on_db_reload(
        Some("https://dashscope.aliyuncs.com/compatible-mode/v1"),
        "qwen3.8-max-preview"
    ));
    assert!(preserves_thinking(
        Some("https://dashscope.aliyuncs.com/compatible-mode/v1"),
        "qwen3.8-max-preview"
    ));
}

// ─── Two-turn reload fixture → OpenAI payload shape ─────────────────

#[test]
fn kimi_reload_still_re_emits_turn_one_reasoning_content() {
    let mut rows = vec![
        turn_one_assistant_persisted(),
        db_row("user", "Turn-2 follow-up question.", None),
    ];
    apply_reload_clean(&mut rows, "https://api.moonshot.ai/v1", "kimi-k2.6");

    let ctx = AgentContext::from_db_messages(Uuid::new_v4(), rows, 200_000);
    let req = LLMRequest::new("kimi-k2.6".to_string(), ctx.messages);
    let body = serde_json::to_value(moonshot_provider().to_openai_request(req)).unwrap();
    assert_eq!(
        reasoning_content_on_assistant(&body, 0).as_deref(),
        Some("Turn-1 chain of thought about the task.")
    );
}

#[test]
fn qwen_reload_omits_turn_one_reasoning_content_on_wire() {
    let mut rows = vec![
        turn_one_assistant_persisted(),
        db_row("user", "Turn-2 follow-up question.", None),
    ];
    apply_reload_clean(
        &mut rows,
        "https://dashscope.aliyuncs.com/compatible-mode/v1",
        "qwen3.8-max-preview",
    );

    let ctx = AgentContext::from_db_messages(Uuid::new_v4(), rows, 200_000);
    let req = LLMRequest::new("qwen3.8-max-preview".to_string(), ctx.messages);
    let body = serde_json::to_value(dashscope_qwen_provider().to_openai_request(req)).unwrap();
    assert!(
        body["messages"][0]
            .get("reasoning_content")
            .is_none_or(|v| v.is_null()),
        "Qwen reload must not replay historical reasoning_content: {}",
        body["messages"][0]
    );
    assert_eq!(body["preserve_thinking"].as_bool(), Some(true));
}

#[test]
fn kimi_and_qwen_reload_paths_diverge_on_historical_reasoning() {
    let moonshot_url = "https://api.moonshot.ai/v1";
    let dashscope_url = "https://dashscope.aliyuncs.com/compatible-mode/v1";

    let mut kimi_rows = vec![turn_one_assistant_persisted()];
    let mut qwen_rows = vec![turn_one_assistant_persisted()];
    apply_reload_clean(&mut kimi_rows, moonshot_url, "kimi-k2.6");
    apply_reload_clean(&mut qwen_rows, dashscope_url, "qwen3.8-max-preview");

    assert!(
        kimi_rows[0].thinking.is_some(),
        "Kimi still hoists on reload"
    );
    assert!(
        qwen_rows[0].thinking.is_none(),
        "Qwen must not hoist on reload"
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
    assert!(body["messages"][1]["tool_calls"].is_array());
    assert_eq!(
        reasoning_content_on_assistant(&body, 1).as_deref(),
        Some("Live-loop thinking before bash.")
    );
}

#[test]
fn qwen_in_loop_tool_call_still_emits_reasoning_content() {
    let req = LLMRequest::new(
        "qwen3.8-max-preview".to_string(),
        vec![
            Message::user("run bash".to_string()),
            assistant_tool_call_with_thinking("Live-loop qwen thinking."),
        ],
    );
    let body = serde_json::to_value(dashscope_qwen_provider().to_openai_request(req)).unwrap();
    assert!(body["messages"][1]["tool_calls"].is_array());
    assert_eq!(
        reasoning_content_on_assistant(&body, 1).as_deref(),
        Some("Live-loop qwen thinking.")
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

// ─── Load-site wiring ───────────────────────────────────────────────

#[test]
fn load_site_uses_hoist_thinking_on_db_reload_not_preserves_thinking() {
    const SRC: &str = include_str!("../brain/agent/service/tool_loop.rs");
    let call = SRC
        .find("context_rows::clean_rows_for_llm(&mut db_messages, hoist_thinking_on_reload)")
        .expect("tool_loop reload clean");
    let window = &SRC[call.saturating_sub(500)..call];
    assert!(
        window.contains("hoist_thinking_on_db_reload("),
        "reload hoist must use the Kimi-only gate"
    );
}

#[test]
fn qwen_request_preserves_thinking_flag_independent_of_reload_hoist() {
    assert!(preserve_thinking_for(
        "https://dashscope.aliyuncs.com/compatible-mode/v1",
        "qwen3.8-max-preview"
    ));
}
