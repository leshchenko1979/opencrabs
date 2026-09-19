//! #346 task 4 — the operator's retry tuning must reach the provider that
//! actually retries.
//!
//! `configure_openai_compatible` is the single choke point every
//! [OI]-compatible provider passes through: the primary provider, and each
//! fallback member built by `create_provider_by_name`. Before #346 the retry
//! knobs were hard-wired inside the provider and the `with_retry_config`
//! setter had no production caller at all (D2), so an operator had no way to
//! raise the attempts on a flaky gateway.
//!
//! Two things must hold, and they are asserted separately:
//!   1. the values written in `[retry]` / `retry_*` reach the provider the
//!      choke point builds, layered per KEY over the family preset, and
//!   2. they reach it through the FALLBACK path too — which returns a
//!      `dyn Provider`, so it can only be inspected through the trait.

use crate::brain::provider::factory::{configure_openai_compatible, create_provider_by_name};
use crate::brain::provider::OpenAIProvider;
use crate::config::{Config, ProviderConfig};
use crate::utils::retry::RetryConfig;

/// `[retry]` global + a per-provider override that must win over it.
const CONFIGURED: &str = r#"
[retry]
max_attempts = 7
jitter = 0.25

[providers.custom.tuned]
base_url = "https://example.invalid/v1"
api_key = "sk-test"
default_model = "tuned-model"
retry_max_attempts = 2
"#;

/// A qwen-named provider with ONE key set: the family preset must survive.
const PRESET_WITH_ONE_KEY: &str = r#"
[providers.custom.qwen]
base_url = "https://example.invalid/v1"
api_key = "sk-test"
default_model = "qwen3-coder"
retry_max_attempts = 6
"#;

/// Nothing configured at all — upgrading must change no behaviour.
const UNCONFIGURED: &str = r#"
[providers.custom.qwen]
base_url = "https://example.invalid/v1"
api_key = "sk-test"
default_model = "qwen3-coder"
"#;

fn parse(src: &str) -> Config {
    toml::from_str(src).expect("config must parse")
}

fn section(config: &Config, name: &str) -> ProviderConfig {
    config
        .providers
        .custom
        .as_ref()
        .and_then(|m| m.get(name))
        .unwrap_or_else(|| panic!("[providers.custom.{name}] must be present"))
        .clone()
}

/// Build a provider exactly the way the factory does, through the choke point.
fn build(config: &Config, name: &str) -> OpenAIProvider {
    configure_openai_compatible(
        OpenAIProvider::new("sk-test".to_string()).with_name(name),
        config,
        &section(config, name),
    )
}

#[test]
fn configured_retry_keys_reach_the_provider_the_choke_point_builds() {
    let config = parse(CONFIGURED);
    let resolved = build(&config, "tuned").retry_config("tuned-model");

    assert_eq!(
        resolved.max_attempts, 2,
        "the per-provider retry_max_attempts must beat the global [retry] block"
    );
    assert_eq!(
        resolved.jitter, 0.25,
        "the global [retry] block must reach the provider through the choke point"
    );
    // Keys the operator did not set keep the built-in default: overrides are
    // layered per KEY, never a wholesale replacement.
    let default = RetryConfig::default();
    assert_eq!(resolved.initial_delay, default.initial_delay, "initial_delay");
    assert_eq!(resolved.max_delay, default.max_delay, "max_delay");
    assert_eq!(
        resolved.backoff_multiplier, default.backoff_multiplier,
        "backoff_multiplier"
    );
}

#[tokio::test]
async fn the_fallback_path_resolves_the_same_retry_policy() {
    let config = parse(CONFIGURED);
    let provider = create_provider_by_name(&config, "custom:tuned")
        .await
        .expect("the custom provider must build");

    let resolved = provider.retry_config("tuned-model");
    assert_eq!(
        resolved.max_attempts, 2,
        "a fallback member must get the same policy as the primary"
    );
    assert_eq!(
        resolved.jitter, 0.25,
        "a fallback member must get the same policy as the primary"
    );
}

#[test]
fn a_partial_override_keeps_the_family_preset_underneath() {
    let config = parse(PRESET_WITH_ONE_KEY);
    let resolved = build(&config, "qwen").retry_config("qwen3-coder");
    let preset = RetryConfig::qwen_cli_match();

    assert_eq!(resolved.max_attempts, 6, "the operator's key wins");
    assert_eq!(
        resolved.initial_delay, preset.initial_delay,
        "the preset's tuned delay survives a partial override"
    );
    assert_eq!(
        resolved.jitter, preset.jitter,
        "the preset's tuned jitter survives a partial override"
    );
}

#[test]
fn an_unconfigured_provider_keeps_the_family_preset_exactly() {
    let config = parse(UNCONFIGURED);
    let resolved = build(&config, "qwen").retry_config("qwen3-coder");
    let preset = RetryConfig::qwen_cli_match();

    assert_eq!(resolved.max_attempts, preset.max_attempts, "max_attempts");
    assert_eq!(resolved.initial_delay, preset.initial_delay, "initial_delay");
    assert_eq!(resolved.max_delay, preset.max_delay, "max_delay");
    assert_eq!(
        resolved.backoff_multiplier, preset.backoff_multiplier,
        "backoff_multiplier"
    );
    assert_eq!(resolved.jitter, preset.jitter, "jitter");
}

#[test]
fn the_explicit_retry_config_override_still_replaces_the_policy() {
    let config = parse(CONFIGURED);
    let provider = build(&config, "tuned").with_retry_config(RetryConfig::no_retry());

    assert_eq!(
        provider.retry_config("tuned-model").max_attempts,
        0,
        "with_retry_config REPLACES the policy (RotatingQwenProvider relies on it)"
    );
}
