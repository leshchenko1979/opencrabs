//! #346 — retry policy configuration surface.
//!
//! Two properties are pinned here:
//!
//! 1. the `[retry]` block and the per-provider `retry_*` keys are strictly
//!    OPTIONAL: a `config.toml` written before #346 parses unchanged and
//!    resolves to exactly the built-in defaults, so no existing install
//!    changes behaviour on upgrade;
//! 2. the resolution order (per-provider key → global `[retry]` → built-in
//!    family preset → [`RetryConfig::default`]) holds per FIELD, including
//!    that an explicit value beats the qwen family preset while the keys the
//!    operator left alone keep the preset's tuned values.

use crate::brain::provider::retry_policy::{self, RetryOverrides};
use crate::config::{Config, ProviderConfig, RetrySection};
use crate::utils::retry::RetryConfig;

/// A `config.toml` exactly as it looked before #346 — no `[retry]` block and
/// no `retry_*` key anywhere.
const LEGACY: &str = r#"
[providers.openai]
api_key = "sk-test"
default_model = "gpt-4o-mini"
"#;

/// The same file with every new knob set globally.
const TUNED: &str = r#"
[retry]
max_attempts = 7
initial_delay_secs = 2.5
max_delay_secs = 45.0
backoff_multiplier = 3.0
jitter = 0.25

[providers.openai]
api_key = "sk-test"
default_model = "gpt-4o-mini"
"#;

/// The global block plus one per-provider key that must win over it.
const PROVIDER_OVERRIDE: &str = r#"
[retry]
max_attempts = 7
jitter = 0.25

[providers.openai]
api_key = "sk-test"
default_model = "gpt-4o-mini"
retry_max_attempts = 2
"#;

fn parse(toml_src: &str) -> Config {
    toml::from_str(toml_src).expect("config must parse")
}

fn openai_section(config: &Config) -> &ProviderConfig {
    config
        .providers
        .openai
        .as_ref()
        .expect("[providers.openai] must be present")
}

/// Field-by-field comparison against the built-in default. `RetryConfig`
/// carries no `PartialEq` of its own, and comparing fields keeps the failure
/// message naming the field that drifted.
fn assert_is_builtin_default(c: &RetryConfig) {
    let d = RetryConfig::default();
    assert_eq!(c.max_attempts, d.max_attempts, "max_attempts");
    assert_eq!(c.initial_delay, d.initial_delay, "initial_delay");
    assert_eq!(c.max_delay, d.max_delay, "max_delay");
    assert_eq!(c.backoff_multiplier, d.backoff_multiplier, "backoff_multiplier");
    assert_eq!(c.jitter, d.jitter, "jitter");
}

// ── the absent case: no [retry], no retry_* ─────────────────────────

#[test]
fn a_config_without_a_retry_section_still_parses() {
    let config = parse(LEGACY);
    assert!(
        config.retry.max_attempts.is_none()
            && config.retry.initial_delay_secs.is_none()
            && config.retry.max_delay_secs.is_none()
            && config.retry.backoff_multiplier.is_none()
            && config.retry.jitter.is_none(),
        "an absent [retry] block must leave every key unset"
    );

    let provider = openai_section(&config);
    assert!(
        provider.retry_max_attempts.is_none()
            && provider.retry_initial_delay_secs.is_none()
            && provider.retry_max_delay_secs.is_none()
            && provider.retry_backoff_multiplier.is_none()
            && provider.retry_jitter.is_none(),
        "an absent retry_* key must stay None, not default to a number"
    );
    assert_eq!(
        provider.retry_quota_exhausted, None,
        "the aggregator opt-in must default to unset, i.e. #952 behaviour"
    );
}

#[test]
fn an_absent_retry_section_resolves_to_the_builtin_default() {
    let config = parse(LEGACY);
    let overrides = RetryOverrides::from_config(openai_section(&config), &config.retry);

    assert!(
        overrides.is_empty(),
        "nothing configured must read as empty overrides: {overrides:?}"
    );
    assert!(
        !overrides.quota_exhausted,
        "quota retries are opt-in and default off"
    );

    let resolved = retry_policy::resolve(
        "openai",
        "https://api.openai.com/v1",
        "gpt-4o-mini",
        &overrides,
    );
    assert_is_builtin_default(&resolved);
}

#[test]
fn an_empty_retry_section_parses_to_defaults() {
    let section: RetrySection = toml::from_str("").expect("empty [retry] must parse");
    let config = parse(LEGACY);
    let overrides = RetryOverrides::from_config(openai_section(&config), &section);
    assert!(overrides.is_empty(), "a present-but-empty [retry] changes nothing");
}

// ── the global section ──────────────────────────────────────────────

#[test]
fn the_global_retry_section_reaches_the_resolved_config() {
    let config = parse(TUNED);
    let overrides = RetryOverrides::from_config(openai_section(&config), &config.retry);

    let resolved = retry_policy::resolve(
        "openai",
        "https://api.openai.com/v1",
        "gpt-4o-mini",
        &overrides,
    );
    assert_eq!(resolved.max_attempts, 7);
    assert_eq!(resolved.initial_delay.as_secs_f64(), 2.5);
    assert_eq!(resolved.max_delay.as_secs_f64(), 45.0);
    assert_eq!(resolved.backoff_multiplier, 3.0);
    assert_eq!(resolved.jitter, 0.25);
}

#[test]
fn a_tuned_config_round_trips_through_serde() {
    let config = parse(TUNED);
    let rendered = toml::to_string(&config).expect("config serializes");
    let reparsed = parse(&rendered);
    assert_eq!(reparsed.retry.max_attempts, Some(7));
    assert_eq!(reparsed.retry.initial_delay_secs, Some(2.5));
    assert_eq!(
        openai_section(&reparsed).retry_max_attempts,
        None,
        "unset per-provider keys must not be serialized as nulls"
    );
}

// ── precedence ──────────────────────────────────────────────────────

#[test]
fn a_per_provider_key_beats_the_global_section() {
    let config = parse(PROVIDER_OVERRIDE);
    let overrides = RetryOverrides::from_config(openai_section(&config), &config.retry);

    assert_eq!(
        overrides.max_attempts,
        Some(2),
        "retry_max_attempts on the provider must win over [retry].max_attempts"
    );
    assert_eq!(
        overrides.jitter,
        Some(0.25),
        "a key set only globally must still come through"
    );
}

#[test]
fn an_explicit_value_beats_the_qwen_family_preset() {
    // The qwen preset is (4 attempts, 3s, 30s, 2.0, 0.2 jitter).
    let overrides = RetryOverrides {
        max_attempts: Some(9),
        ..Default::default()
    };
    let resolved =
        retry_policy::resolve("qwen", "https://dashscope.aliyuncs.com", "qwen-max", &overrides);

    assert_eq!(resolved.max_attempts, 9, "the explicit value must win");
    assert_eq!(
        resolved.initial_delay.as_secs_f64(),
        3.0,
        "a field the operator did not set must keep the preset value"
    );
    assert_eq!(
        resolved.jitter,
        0.2,
        "the preset is a default, not a lock — untouched fields survive"
    );
}

#[test]
fn the_family_preset_still_selects_on_name_url_and_model() {
    let none = RetryOverrides::default();
    let qwen = retry_policy::family_preset("qwen", "https://example.invalid", "qwen-max");
    let openrouter =
        retry_policy::family_preset("custom", "https://openrouter.ai/api/v1", "some-model");
    let free = retry_policy::family_preset("custom", "https://example.invalid", "llama:free");
    let plain = retry_policy::family_preset("openai", "https://api.openai.com/v1", "gpt-4o");

    for preset in [&qwen, &openrouter, &free] {
        assert_eq!(
            preset.initial_delay.as_secs_f64(),
            3.0,
            "qwen / openrouter / :free must keep the in-place-retry preset"
        );
    }
    assert_is_builtin_default(&plain);
    assert_eq!(
        retry_policy::resolve("openai", "https://api.openai.com/v1", "gpt-4o", &none).max_attempts,
        plain.max_attempts,
        "resolve with no overrides must be exactly the preset"
    );
}

// ── the aggregator opt-in ───────────────────────────────────────────

#[test]
fn the_quota_opt_in_is_false_unless_explicitly_set() {
    let config = parse(TUNED);
    let overrides = RetryOverrides::from_config(openai_section(&config), &config.retry);
    assert!(
        !overrides.quota_exhausted,
        "quota retries must never be enabled by a global default"
    );

    let opted_in: ProviderConfig =
        toml::from_str("retry_quota_exhausted = true").expect("provider section must parse");
    assert_eq!(opted_in.retry_quota_exhausted, Some(true));
    let overrides = RetryOverrides::from_config(&opted_in, &config.retry);
    assert!(overrides.quota_exhausted, "the opt-in must survive resolution");
    assert!(
        !overrides.is_empty(),
        "an opt-in alone is a real override, not an empty one"
    );
}

// ── hostile input ───────────────────────────────────────────────────

#[test]
fn a_negative_delay_does_not_panic() {
    let overrides = RetryOverrides {
        initial_delay_secs: Some(-1.0),
        max_delay_secs: Some(f64::NAN),
        ..Default::default()
    };
    let mut config = RetryConfig::default();
    overrides.apply_to(&mut config);
    assert_eq!(config.initial_delay, std::time::Duration::ZERO);
    assert_eq!(config.max_delay, std::time::Duration::ZERO);
}
