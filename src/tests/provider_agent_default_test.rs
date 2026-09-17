//! `[agent] default_provider` is the user's stated choice for every session
//! and every cron job (#269).
//!
//! Before the fix the key was consulted with its raw spelling, so a profile
//! whose config said `default_provider = "custom.llm_gateway"` missed the
//! `[providers.custom.llm-gateway]` section and fell through to the
//! enabled-provider priority list — the live `family` profile ran on a
//! built-in while its config named a custom provider.
//!
//! Two things had to agree for the key to resolve: every spelling of a custom
//! name (`custom.`, `custom:`, `custom/`, bare, and the underscore form the
//! loader normalises away) must reach the section, and the name that comes out
//! must be the section's own key. These cases drive the lookup helpers, the
//! factory and the cron resolver together, because a fix in one and not the
//! others is exactly the half-resolution that hid the bug.

use crate::brain::provider::factory::{is_known_provider_name, provider_config_by_name};
use crate::brain::provider::{create_provider, create_provider_with_warning};
use crate::brain::provider_spec::{ProviderKey, normalize_in};
use crate::config::profile::with_home_override_async;
use crate::config::{Config, ProviderConfig, ProviderConfigs};
use crate::cron::scheduler::{resolve_cron_provider_pair, resolve_or_create_cron_session};
use crate::db::Database;
use crate::db::models::CronJob;
use crate::services::{ServiceContext, SessionService};
use std::collections::BTreeMap;

/// A config whose only provider is `[providers.custom.llm-gateway]`: the
/// section key carries the hyphen, the `[agent]` value the user writes carries
/// the underscore — the spelling the live profiles use.
fn config_with_custom() -> Config {
    let mut custom = BTreeMap::new();
    custom.insert(
        "llm-gateway".to_string(),
        ProviderConfig {
            enabled: true,
            api_key: Some("sk-test-key".to_string()),
            base_url: Some("https://example.invalid/v1".to_string()),
            default_model: Some("auto".to_string()),
            models: vec!["auto".to_string()],
            ..Default::default()
        },
    );
    Config {
        providers: ProviderConfigs {
            custom: Some(custom),
            ..Default::default()
        },
        ..Default::default()
    }
}

fn job(name: &str) -> CronJob {
    CronJob::new(
        name.to_string(),
        "0 9 * * *".to_string(),
        "UTC".to_string(),
        "do things".to_string(),
        None,
        None,
        "off".to_string(),
        true,
        None,
        None,
    )
}

/// Every spelling a user may write for one custom provider resolves to the
/// same section, through the factory helpers and through `Config`'s own
/// declared/healthy pre-filters.
#[test]
fn every_custom_spelling_reaches_the_section() {
    let config = config_with_custom();
    for spelling in [
        "custom.llm-gateway",
        "custom.llm_gateway",
        "custom/llm-gateway",
        "custom:llm-gateway",
        "llm-gateway",
    ] {
        assert!(
            provider_config_by_name(&config, spelling).is_some(),
            "provider_config_by_name missed '{spelling}'"
        );
        assert!(
            is_known_provider_name(&config, spelling),
            "is_known_provider_name missed '{spelling}'"
        );
        assert!(
            config.providers.is_declared(spelling),
            "is_declared missed '{spelling}'"
        );
        assert!(
            config.providers.is_healthy(spelling),
            "is_healthy missed '{spelling}'"
        );
    }
}

/// The resolved name is the SECTION key, not the spelling the user wrote: the
/// underscore form is normalised by the loader, so `custom.llm_gateway` must
/// come back as `llm-gateway`. Returning `llm_gateway` would construct a
/// provider no config section names.
#[test]
fn the_underscore_spelling_resolves_to_the_section_key() {
    let config = config_with_custom();
    let pair = normalize_in(&config, ProviderKey::AGENT, "custom.llm_gateway", None);
    assert_eq!(pair.provider, "llm-gateway");
    assert!(
        pair.note.is_some(),
        "dropping the prefix must be reportable so the log teaches the canonical form"
    );
}

/// The key wins over the enabled-provider priority list.
#[tokio::test]
async fn agent_default_provider_wins_over_registrations() {
    let mut config = config_with_custom();
    config.agent.default_provider = Some("custom.llm_gateway".to_string());
    let provider = create_provider(&config).await.unwrap();
    assert_eq!(provider.name(), "llm-gateway");
}

/// Unset keeps the pre-#269 behaviour: nothing is pinned, so the priority
/// list decides (finding the enabled custom provider).
#[tokio::test]
async fn agent_default_provider_unset_keeps_the_old_behaviour() {
    let config = config_with_custom();
    let provider = create_provider(&config).await.unwrap();
    assert_eq!(provider.name(), "llm-gateway");
}

/// A key that names no provider must be reported, not swallowed — and the
/// warning must survive the fallback scan that follows it.
#[tokio::test]
async fn agent_default_provider_failure_is_reported_and_falls_back() {
    let mut config = Config::default();
    config.agent.default_provider = Some("no-such-provider".to_string());
    let (provider, warning) = create_provider_with_warning(&config).await.unwrap();
    assert_eq!(provider.name(), "none");
    let warning = warning.expect("an unresolvable default_provider must warn");
    assert!(
        warning.contains("no-such-provider"),
        "the warning must name the value that failed, got: {warning}"
    );
}

/// With nothing pinned, a cron job inherits the `[agent]` pair.
#[test]
fn cron_pair_inherits_the_agent_pair_when_nothing_is_pinned() {
    let mut config = config_with_custom();
    config.agent.default_provider = Some("custom.llm_gateway".to_string());
    config.agent.default_model = Some("auto".to_string());
    let (provider, model) = resolve_cron_provider_pair(&job("inherited"), &config);
    assert_eq!(provider, Some("llm-gateway".to_string()));
    assert_eq!(model, Some("auto".to_string()));
}

/// A `[cron]` default pins the provider, so the `[agent]` model must NOT ride
/// along: pairing provider A with a model belonging to provider B makes the
/// job's own pre-validation skip it.
#[test]
fn cron_pair_never_mixes_two_providers() {
    let mut config = config_with_custom();
    config.cron.default_provider = Some("minimax".to_string());
    config.agent.default_provider = Some("custom.llm_gateway".to_string());
    config.agent.default_model = Some("auto".to_string());
    let (provider, model) = resolve_cron_provider_pair(&job("cron-pinned"), &config);
    assert_eq!(provider, Some("minimax".to_string()));
    assert_eq!(
        model, None,
        "the [agent] model must not pair with [cron]'s provider"
    );
}

/// The job's own pin outranks both defaults.
#[test]
fn cron_pair_prefers_the_job_pin() {
    let mut config = config_with_custom();
    config.agent.default_provider = Some("custom.llm_gateway".to_string());
    config.agent.default_model = Some("auto".to_string());
    let mut pinned = job("job-pinned");
    pinned.provider = Some("zhipu".to_string());
    pinned.model = Some("glm-5.3".to_string());
    let (provider, model) = resolve_cron_provider_pair(&pinned, &config);
    assert_eq!(provider, Some("zhipu".to_string()));
    assert_eq!(model, Some("glm-5.3".to_string()));
}

/// End to end: the session row a cron job creates carries the inherited pair,
/// read back from a config.toml written exactly as a user would write it.
#[tokio::test]
async fn cron_session_row_carries_the_inherited_pair() {
    let dir = tempfile::tempdir().expect("tempdir");
    let home = dir.path().join(".opencrabs");
    std::fs::create_dir_all(&home).expect("create home");
    std::fs::write(
        home.join("config.toml"),
        r#"
[agent]
default_provider = "custom.llm_gateway"
default_model = "auto"

[providers.custom.llm-gateway]
enabled = true
base_url = "https://example.invalid/v1"
api_key = "sk-test-key"
default_model = "auto"
models = ["auto"]
"#,
    )
    .expect("write config.toml");

    with_home_override_async(home, async {
        let db = Database::connect_in_memory().await.expect("in-memory db");
        db.run_migrations().await.expect("migrations");
        let ctx = ServiceContext::new(db.pool().clone());

        let session_id = resolve_or_create_cron_session(&ctx, &job("inherited-job"))
            .await
            .expect("resolve cron session");

        let session = SessionService::new(ctx)
            .get_session(session_id)
            .await
            .expect("get session")
            .expect("session exists");
        assert_eq!(session.provider_name.as_deref(), Some("llm-gateway"));
        assert_eq!(session.model.as_deref(), Some("auto"));
    })
    .await;
}
