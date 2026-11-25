//! Wire knobs when [`LLMRequest::disable_reasoning`] is set (#778, #1007).
//!
//! The compaction summariser sets this flag. Clearing family resolvers to
//! `None` leaves server-default thinking on for gateways that default to on
//! (e.g. ling-3.1-flash via OpenAI-compatible combos). This module spells
//! "off" per family, mirroring each resolver's own disable path.

use super::deepseek_reasoning;
use super::glm_reasoning;
use super::kimi_reasoning;
use super::qwen;
use super::qwen_reasoning;

/// Outgoing thinking fields after a disable-reasoning override.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct DisabledReasoningWire {
    pub include_reasoning: Option<bool>,
    pub reasoning_effort: Option<String>,
    pub thinking: Option<serde_json::Value>,
    pub enable_thinking: Option<bool>,
    pub preserve_thinking: Option<bool>,
}

/// Resolve explicit off signals for `base_url` + `model` when the caller
/// requested reasoning disabled.
pub(crate) fn wire_for_disabled(base_url: &str, model: &str) -> DisabledReasoningWire {
    let base = base_url.to_ascii_lowercase();
    let openrouter_shaped = base.contains("openrouter")
        || base.contains("openrouter.ai")
        || std::env::var("OPENCRABS_ENABLE_REASONING").is_ok();

    if deepseek_reasoning::serves_deepseek(base_url, model) {
        let knobs = deepseek_reasoning::resolve(None, Some(false));
        return DisabledReasoningWire {
            include_reasoning: openrouter_shaped.then_some(false),
            reasoning_effort: knobs.effort.map(str::to_string),
            thinking: Some(serde_json::json!({
                "type": if knobs.enabled { "enabled" } else { "disabled" }
            })),
            enable_thinking: None,
            preserve_thinking: None,
        };
    }

    if qwen::serves_qwen_remotely(base_url, model) && qwen_reasoning::family(model).is_some() {
        let knobs = qwen_reasoning::resolve(model, None, Some(false));
        return DisabledReasoningWire {
            include_reasoning: openrouter_shaped.then_some(false),
            reasoning_effort: knobs.reasoning_effort,
            thinking: None,
            enable_thinking: knobs.enable_thinking,
            preserve_thinking: None,
        };
    }

    if kimi_reasoning::is_kimi_model(model) {
        let (effort, thinking) = kimi_reasoning::resolve_fields(model, "off");
        return DisabledReasoningWire {
            include_reasoning: openrouter_shaped.then_some(false),
            reasoning_effort: effort,
            thinking,
            enable_thinking: None,
            preserve_thinking: None,
        };
    }

    if glm_reasoning::serves_zai(base_url)
        && let Some(version) = glm_reasoning::glm_version(model)
    {
        if version >= (5, 3) {
            // GLM-5.3+ cannot turn thinking off — shallowest rung only.
            return DisabledReasoningWire {
                include_reasoning: openrouter_shaped.then_some(false),
                reasoning_effort: Some("low".to_string()),
                thinking: Some(serde_json::json!({
                    "type": "enabled",
                    "clear_thinking": false
                })),
                enable_thinking: None,
                preserve_thinking: None,
            };
        }
        if version >= (5, 0) {
            return DisabledReasoningWire {
                include_reasoning: openrouter_shaped.then_some(false),
                reasoning_effort: None,
                thinking: Some(serde_json::json!({ "type": "disabled" })),
                enable_thinking: None,
                preserve_thinking: None,
            };
        }
    }

    // OpenAI-compatible gateways with no family resolver (inferhub combo models,
    // ling-3.1-flash, etc.): send the safest generic off pair the codebase already
    // uses elsewhere. Deliberately omit `reasoning_effort` here — many gateways
    // reject unknown effort values with 400, while `enable_thinking` + `thinking`
    // are widely ignored when unsupported rather than rejected.
    DisabledReasoningWire {
        include_reasoning: openrouter_shaped.then_some(false),
        reasoning_effort: None,
        thinking: Some(serde_json::json!({ "type": "disabled" })),
        enable_thinking: Some(false),
        preserve_thinking: None,
    }
}
