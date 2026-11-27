//! Context window configuration tests.
//!
//! Tests that the context_window field on ProviderConfig is properly wired
//! through to OpenAIProvider, overriding model-name heuristics for custom/local
//! providers whose models aren't recognized by name.

use crate::brain::TokenUsage;
use crate::brain::provider::custom_openai_compatible::OpenAIProvider;
use crate::brain::provider::factory::{create_provider, create_provider_by_name};
use crate::brain::{LLMResponse, Provider};
use crate::config::{Config, ProviderConfig, ProviderConfigs};
use std::collections::BTreeMap;

// ── Helper ──────────────────────────────────────────────────────

fn config_with_context_window(context_window: Option<u32>) -> Config {
    let mut custom_map = BTreeMap::new();
    custom_map.insert(
        "lmstudio".to_string(),
        ProviderConfig {
            enabled: true,
            api_key: None,
            base_url: Some("http://localhost:1234/v1".to_string()),
            default_model: Some("my-local-model".to_string()),
            context_window,
            ..Default::default()
        },
    );
    Config {
        providers: ProviderConfigs {
            custom: Some(custom_map),
            ..Default::default()
        },
        ..Default::default()
    }
}

// ── OpenAIProvider: with_context_window builder ─────────────────

#[test]
fn with_context_window_overrides_unknown_model() {
    let provider = OpenAIProvider::with_base_url(
        String::new(),
        "http://localhost:1234/v1/chat/completions".to_string(),
    )
    .with_name("local")
    .with_context_window(32_000);

    // Unknown model would normally return None, but configured value takes priority
    assert_eq!(provider.context_window("my-custom-model"), Some(32_000));
}

#[test]
fn with_context_window_overrides_known_model() {
    let provider = OpenAIProvider::with_base_url(
        String::new(),
        "http://localhost:1234/v1/chat/completions".to_string(),
    )
    .with_context_window(64_000);

    // Even known models (gpt-4o = 128k) are overridden by explicit config
    assert_eq!(provider.context_window("gpt-4o"), Some(64_000));
}

#[test]
fn without_context_window_returns_none_for_unknown() {
    let provider = OpenAIProvider::with_base_url(
        String::new(),
        "http://localhost:1234/v1/chat/completions".to_string(),
    );

    // No configured value, unknown model → None
    assert_eq!(provider.context_window("my-custom-model"), None);
}

#[test]
fn without_context_window_returns_heuristic_for_known() {
    let provider = OpenAIProvider::with_base_url(
        String::new(),
        "http://localhost:1234/v1/chat/completions".to_string(),
    );

    // No configured value, known model → heuristic
    assert_eq!(provider.context_window("gpt-4o"), Some(128_000));
}

// ── Factory: context_window wired through config ────────────────

#[tokio::test]
async fn factory_passes_context_window_to_provider() {
    let config = config_with_context_window(Some(200_000));
    let result = create_provider(&config).await;
    assert!(result.is_ok());
    let provider = result.unwrap();
    assert_eq!(
        provider.context_window("my-local-model"),
        Some(200_000),
        "Factory should pass context_window from config to provider"
    );
}

#[tokio::test]
async fn factory_no_context_window_returns_none_for_unknown() {
    let config = config_with_context_window(None);
    let result = create_provider(&config).await;
    assert!(result.is_ok());
    let provider = result.unwrap();
    assert_eq!(
        provider.context_window("my-local-model"),
        None,
        "Without context_window in config, unknown model should return None"
    );
}

#[tokio::test]
async fn factory_by_name_passes_context_window() {
    let config = config_with_context_window(Some(16_384));
    let result = create_provider_by_name(&config, "custom:lmstudio").await;
    assert!(result.is_ok());
    let provider = result.unwrap();
    assert_eq!(provider.context_window("whatever-model"), Some(16_384));
}

// ── ProviderConfig serialization ────────────────────────────────

#[test]
fn context_window_serializes_when_set() {
    let cfg = ProviderConfig {
        enabled: true,
        context_window: Some(128_000),
        ..Default::default()
    };
    let toml_str = toml::to_string(&cfg).expect("serialize");
    assert!(
        toml_str.contains("context_window = 128000"),
        "context_window should appear in serialized TOML: {}",
        toml_str
    );
}

#[test]
fn context_window_omitted_when_none() {
    let cfg = ProviderConfig {
        enabled: true,
        context_window: None,
        ..Default::default()
    };
    let toml_str = toml::to_string(&cfg).expect("serialize");
    assert!(
        !toml_str.contains("context_window"),
        "context_window should be omitted when None: {}",
        toml_str
    );
}

#[test]
fn context_window_deserializes_from_toml() {
    let toml_str = r#"
enabled = true
context_window = 32000
"#;
    let cfg: ProviderConfig = toml::from_str(toml_str).expect("deserialize");
    assert_eq!(cfg.context_window, Some(32_000));
}

#[test]
fn context_window_defaults_to_none_when_missing() {
    let toml_str = r#"
enabled = true
"#;
    let cfg: ProviderConfig = toml::from_str(toml_str).expect("deserialize");
    assert_eq!(cfg.context_window, None);
}

// ── Multiple custom providers with different context windows ────

#[tokio::test]
async fn multiple_customs_each_get_own_context_window() {
    let mut custom_map = BTreeMap::new();
    custom_map.insert(
        "nvidia".to_string(),
        ProviderConfig {
            enabled: true,
            base_url: Some("https://integrate.api.nvidia.com/v1".to_string()),
            default_model: Some("llama-70b".to_string()),
            context_window: Some(128_000),
            ..Default::default()
        },
    );
    custom_map.insert(
        "ollama".to_string(),
        ProviderConfig {
            enabled: false,
            base_url: Some("http://localhost:11434/v1".to_string()),
            default_model: Some("phi3".to_string()),
            context_window: Some(4_096),
            ..Default::default()
        },
    );
    let config = Config {
        providers: ProviderConfigs {
            custom: Some(custom_map),
            ..Default::default()
        },
        ..Default::default()
    };

    // nvidia is the active (enabled) one
    let provider = create_provider(&config).await.unwrap();
    assert_eq!(provider.context_window("llama-70b"), Some(128_000));

    // ollama via by_name
    let ollama = create_provider_by_name(&config, "custom:ollama")
        .await
        .unwrap();
    assert_eq!(ollama.context_window("phi3"), Some(4_096));
}

#[test]
fn context_window_zero_is_valid() {
    // Edge case: user sets context_window = 0 (unusual but shouldn't crash)
    let provider = OpenAIProvider::with_base_url(
        String::new(),
        "http://localhost:1234/v1/chat/completions".to_string(),
    )
    .with_context_window(0);
    assert_eq!(provider.context_window("any-model"), Some(0));
}

#[test]
fn context_window_large_value() {
    // Very large context windows (e.g. 1M+ tokens)
    let provider = OpenAIProvider::with_base_url(
        String::new(),
        "http://localhost:1234/v1/chat/completions".to_string(),
    )
    .with_context_window(2_000_000);
    assert_eq!(provider.context_window("any-model"), Some(2_000_000));
}

// ── #1046: family-keyed decisions read the SERVED id ─────────────────────────

/// `context_window_for_response` must resolve the family from the model the
/// backend actually served, and fall back to the requested id when the backend
/// reported none.
///
/// The defect this pins: a gateway combo route (`combo/route`) names a fan-out
/// rather than a model, so it matches no family arm at all. Resolving from the
/// requested id alone therefore yields `None` while the model that answered was
/// a known one — the budget then fell to the agent default regardless.
#[test]
fn context_window_for_response_prefers_the_served_model() {
    let provider = OpenAIProvider::new("test-key".to_string());

    // The requested id matches no arm; the served id names a known family.
    assert_eq!(provider.context_window("combo/route"), None);
    assert_eq!(
        provider.context_window("gpt-5.1"),
        Some(1_047_576),
        "precondition: the served id must resolve to a family"
    );

    let served = LLMResponse {
        id: "r1".to_string(),
        model: "combo/route".to_string(),
        resolved_model: Some("gpt-5.1".to_string()),
        content: vec![],
        stop_reason: None,
        usage: TokenUsage::default(),
        streaming_active_secs: None,
        tool_text_leak: false,
    };
    assert_eq!(
        provider.context_window_for_response(&served),
        Some(1_047_576),
        "the served id must win over the requested one"
    );

    // No served id reported: fall back to the requested id, as every
    // pre-#1046 path did.
    let unresolved = LLMResponse {
        id: "r2".to_string(),
        model: "gpt-5.1".to_string(),
        resolved_model: None,
        content: vec![],
        stop_reason: None,
        usage: TokenUsage::default(),
        streaming_active_secs: None,
        tool_text_leak: false,
    };
    assert_eq!(
        provider.context_window_for_response(&unresolved),
        Some(1_047_576),
        "absent a served id the requested id is still honoured"
    );
}

// ── #1045: a gateway-served Claude model resolves its real window ────────────

/// A Claude model reached through a custom OpenAI-compatible gateway must be
/// budgeted against the window its id names, not against the agent default.
///
/// The defect this pins: the Claude window table lived inside the `anthropic`
/// provider, so the gateway's `context_window` had no `claude` arm at all and
/// returned `None`. `context_limit_for_session` then fell through to
/// `agent.context_limit` — a 200K budget for a 1M model — while the very same
/// model served natively resolved 1M.
#[tokio::test]
async fn a_gateway_claude_resolves_its_real_window() {
    use crate::tests::agent_service_mocks::create_test_service_with_provider;
    use std::sync::Arc;

    // A gateway entry with NO `providers.<name>.context_window` — the shape
    // that exposed the defect.
    let provider = Arc::new(OpenAIProvider::new("test-key".to_string()));
    assert_eq!(
        provider.configured_context_window(),
        None,
        "precondition: nothing is explicitly configured"
    );

    // The provider now answers for the model, through the shared table.
    assert_eq!(
        provider.context_window("claude-opus-4-7"),
        Some(1_000_000),
        "the gateway must resolve a 1M Claude model's window"
    );
    assert_eq!(
        provider.context_window("claude-3-haiku-20240307"),
        Some(200_000),
        "and a 200K one's, from the same table"
    );

    let (svc, sid) = create_test_service_with_provider(provider).await;

    svc.set_session_model(sid, "claude-opus-4-7".to_string());
    assert_eq!(
        svc.context_limit_for_session(sid),
        1_000_000,
        "a 1M Claude model on a gateway must budget 1M, not the 200K default"
    );

    svc.set_session_model(sid, "claude-3-haiku-20240307".to_string());
    assert_eq!(
        svc.context_limit_for_session(sid),
        200_000,
        "a 200K Claude model on the same gateway budgets 200K"
    );
}

/// An explicit `providers.<name>.context_window` still outranks the model-keyed
/// window (#973 must not regress): the user's DECLARED budget wins over a value
/// derived from the model id.
#[tokio::test]
async fn an_explicit_context_window_still_overrides_the_model_window() {
    use crate::tests::agent_service_mocks::create_test_service_with_provider;
    use std::sync::Arc;

    let provider =
        Arc::new(OpenAIProvider::new("test-key".to_string()).with_context_window(300_000));

    let (svc, sid) = create_test_service_with_provider(provider).await;
    svc.set_session_model(sid, "claude-opus-4-7".to_string());

    assert_eq!(
        svc.context_limit_for_session(sid),
        300_000,
        "the declared window must win over the model-keyed 1M (#973)"
    );
}
