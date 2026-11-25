//! Wire-level checks for `LLMRequest::disable_reasoning` (#1007).

use crate::brain::provider::custom_openai_compatible::OpenAIProvider;
use crate::brain::provider::{LLMRequest, Message};

const DASHSCOPE: &str = "https://dashscope.aliyuncs.com/compatible-mode/v1";
const DEEPSEEK: &str = "https://api.deepseek.com/v1";
const ZAI: &str = "https://api.z.ai/api/paas/v4/chat/completions";
const MOONSHOT: &str = "https://api.moonshot.ai/v1";
const COMBO: &str = "https://inferhub.example/v1";

fn wire(provider: OpenAIProvider, model: &str) -> serde_json::Value {
    let req = LLMRequest::new(model.to_string(), vec![Message::user("summarise")])
        .with_reasoning_disabled();
    serde_json::to_value(provider.to_openai_request(req)).expect("serialize")
}

#[test]
fn deepseek_disable_sends_thinking_disabled() {
    let p = OpenAIProvider::with_base_url("key".into(), DEEPSEEK.into()).with_name("deepseek");
    let body = wire(p, "deepseek-v4.1-flash");
    assert_eq!(body["thinking"], serde_json::json!({ "type": "disabled" }));
    assert!(body.get("enable_thinking").is_none());
}

#[test]
fn qwen_hybrid_disable_sends_enable_thinking_false() {
    let p = OpenAIProvider::with_base_url("key".into(), DASHSCOPE.into()).with_name("qwen");
    let body = wire(p, "qwen3.7-plus");
    assert_eq!(body["enable_thinking"], serde_json::json!(false));
}

#[test]
fn qwen_tiered_disable_sends_reasoning_effort_none() {
    let p = OpenAIProvider::with_base_url("key".into(), DASHSCOPE.into()).with_name("qwen");
    let body = wire(p, "qwen3.8-max");
    assert_eq!(body["reasoning_effort"], serde_json::json!("none"));
    assert!(body.get("enable_thinking").is_none());
}

#[test]
fn kimi_k26_disable_sends_thinking_disabled() {
    let p = OpenAIProvider::with_base_url("key".into(), MOONSHOT.into()).with_name("kimi");
    let body = wire(p, "kimi-k2.6");
    assert_eq!(body["thinking"], serde_json::json!({ "type": "disabled" }));
}

#[test]
fn glm_52_disable_sends_thinking_disabled() {
    let p = OpenAIProvider::with_base_url("key".into(), ZAI.into()).with_name("zai");
    let body = wire(p, "glm-5.2");
    assert_eq!(body["thinking"], serde_json::json!({ "type": "disabled" }));
}

#[test]
fn glm_53_disable_sends_lowest_effort_not_off() {
    let p = OpenAIProvider::with_base_url("key".into(), ZAI.into()).with_name("zai");
    let body = wire(p, "glm-5.3");
    assert_eq!(body["reasoning_effort"], serde_json::json!("low"));
    assert_eq!(
        body["thinking"]["type"],
        serde_json::json!("enabled"),
        "GLM-5.3+ cannot turn thinking off on the wire"
    );
}

#[test]
fn unknown_combo_disable_sends_generic_off_pair() {
    let p = OpenAIProvider::with_base_url("key".into(), COMBO.into()).with_name("combo");
    let body = wire(p, "cmc/inclusionai/ling-3.1-flash-1019");
    assert_eq!(body["enable_thinking"], serde_json::json!(false));
    assert_eq!(body["thinking"], serde_json::json!({ "type": "disabled" }));
}
