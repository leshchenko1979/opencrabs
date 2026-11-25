//! Wire-level gating for `reasoning_content` replay (#1005).
//!
//! Only Moonshot kimi and Qwen/DashScope (`preserves_thinking`) receive stored
//! thinking on the serialized request; every other family omits the field.

use crate::brain::provider::custom_openai_compatible::OpenAIProvider;
use crate::brain::provider::{ContentBlock, LLMRequest, Message, Role};

const THINKING: &str = "weighing options before the tool call";

fn assistant_tool_call_with_thinking() -> Message {
    Message {
        role: Role::Assistant,
        content: vec![
            ContentBlock::Thinking {
                thinking: THINKING.to_string(),
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

fn assistant_text_with_thinking() -> Message {
    Message {
        role: Role::Assistant,
        content: vec![
            ContentBlock::Thinking {
                thinking: THINKING.to_string(),
                signature: None,
            },
            ContentBlock::Text {
                text: "Here is the answer.".to_string(),
            },
        ],
    }
}

fn wire_reasoning_content(provider: OpenAIProvider, model: &str, msg: Message) -> Option<String> {
    let req = LLMRequest::new(
        model.to_string(),
        vec![Message::user("hello".to_string()), msg],
    );
    let body = serde_json::to_value(provider.to_openai_request(req)).expect("serialize");
    let asst = &body["messages"].as_array().expect("messages")[1];
    asst.get("reasoning_content")
        .and_then(|v| v.as_str())
        .map(str::to_string)
}

fn assert_present(provider: OpenAIProvider, model: &str, msg: Message) {
    assert_eq!(
        wire_reasoning_content(provider, model, msg),
        Some(THINKING.to_string()),
        "expected reasoning_content for {model}"
    );
}

fn assert_absent(provider: OpenAIProvider, model: &str, msg: Message) {
    let req = LLMRequest::new(
        model.to_string(),
        vec![Message::user("hello".to_string()), msg],
    );
    let body = serde_json::to_value(provider.to_openai_request(req)).expect("serialize");
    let asst = &body["messages"].as_array().expect("messages")[1];
    assert!(
        asst.get("reasoning_content").is_none_or(|v| v.is_null()),
        "expected no reasoning_content for {model}: {asst}"
    );
}

// ─── present: kimi ───────────────────────────────────────────────────

#[test]
fn kimi_tool_call_replays_reasoning_content() {
    let p = OpenAIProvider::with_base_url("k".into(), "https://api.moonshot.ai/v1".into())
        .with_name("kimi");
    assert_present(p, "kimi-k2.6", assistant_tool_call_with_thinking());
}

#[test]
fn opencode_kimi_tool_call_replays_reasoning_content() {
    let p = OpenAIProvider::with_base_url(
        "k".into(),
        "https://opencode.ai/zen/go/v1/chat/completions".into(),
    )
    .with_name("opencode-kimi");
    assert_present(p, "kimi-k2.6", assistant_tool_call_with_thinking());
}

// ─── present: qwen ───────────────────────────────────────────────────

#[test]
fn dashscope_qwen_tool_call_replays_reasoning_content() {
    let p = OpenAIProvider::with_base_url(
        "k".into(),
        "https://dashscope.aliyuncs.com/compatible-mode/v1".into(),
    )
    .with_name("qwen");
    assert_present(p, "qwen3.7-plus", assistant_tool_call_with_thinking());
}

#[test]
fn dashscope_qwen_text_assistant_replays_reasoning_content() {
    let p = OpenAIProvider::with_base_url(
        "k".into(),
        "https://dashscope.aliyuncs.com/compatible-mode/v1".into(),
    )
    .with_name("qwen");
    assert_present(p, "qwen3.8-max", assistant_text_with_thinking());
}

// ─── absent: other families ──────────────────────────────────────────

#[test]
fn deepseek_omits_reasoning_content_with_thinking() {
    let p = OpenAIProvider::with_base_url("k".into(), "https://api.deepseek.com/v1".into())
        .with_name("deepseek");
    assert_absent(
        p,
        "deepseek-v4.1-flash",
        assistant_tool_call_with_thinking(),
    );
}

#[test]
fn openai_omits_reasoning_content_with_thinking() {
    let p = OpenAIProvider::with_base_url("k".into(), "https://api.openai.com/v1".into())
        .with_name("openai");
    assert_absent(p, "gpt-4.1", assistant_tool_call_with_thinking());
}

#[test]
fn zai_glm_omits_reasoning_content_with_thinking() {
    let p = OpenAIProvider::with_base_url(
        "k".into(),
        "https://api.z.ai/api/coding/paas/v4/chat/completions".into(),
    )
    .with_name("zai");
    assert_absent(p, "glm-5.3-flash", assistant_tool_call_with_thinking());
}

#[test]
fn generic_proxy_omits_reasoning_content_with_thinking() {
    let p = OpenAIProvider::with_base_url("k".into(), "https://inferhub.example/v1".into())
        .with_name("combo");
    assert_absent(
        p,
        "cmc/inclusionai/ling-3.1-flash-1019",
        assistant_tool_call_with_thinking(),
    );
}

#[test]
fn openrouter_omits_reasoning_content_with_thinking() {
    let p = OpenAIProvider::with_base_url("k".into(), "https://openrouter.ai/api/v1".into())
        .with_name("openrouter");
    assert_absent(p, "z-ai/glm-5.3-flash", assistant_tool_call_with_thinking());
}
