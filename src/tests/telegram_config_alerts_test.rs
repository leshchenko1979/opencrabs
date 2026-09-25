use crate::channels::telegram::config_alerts::{
    ConfigAlertState, ConfigProblem, ConfigProblemKind, Severity, audit_config_problems,
    db_integrity_problem, format_config_alert, format_config_recovery,
    resolve_recipient_chat_ids,
};
use crate::config::Config;

#[test]
fn test_audit_keys_file_missing_and_parse_error() {
    let mut config = Config::default();
    config.providers.anthropic = Some(crate::config::ProviderConfig {
        enabled: true,
        api_key: Some("test-key".into()),
        ..Default::default()
    });

    // 1. Invalid keys.toml syntax
    let raw_config = "[agent]\ndefault_provider = \"anthropic\"\n";
    let raw_keys = "this is not valid toml = [[[";
    let problems = audit_config_problems(&config, raw_config, Some(raw_keys));
    assert!(
        problems
            .iter()
            .any(|p| matches!(p.kind, ConfigProblemKind::KeysFileParseError(_))),
        "Expected KeysFileParseError for malformed keys.toml"
    );

    // 2. Valid keys.toml
    let valid_keys = "[anthropic]\napi_key = \"test-key\"\n";
    let clean_problems = audit_config_problems(&config, raw_config, Some(valid_keys));
    assert!(
        !clean_problems
            .iter()
            .any(|p| matches!(p.kind, ConfigProblemKind::KeysFileParseError(_))),
        "Expected no KeysFileParseError for valid keys.toml"
    );
}

#[test]
fn test_audit_missing_provider_api_keys() {
    let mut config = Config::default();
    config.providers.anthropic = Some(crate::config::ProviderConfig {
        enabled: true,
        api_key: None,
        default_model: Some("claude-3-5-sonnet".into()),
        ..Default::default()
    });
    config.providers.openai = Some(crate::config::ProviderConfig {
        enabled: true,
        api_key: Some("sk-valid".into()),
        default_model: Some("gpt-4o".into()),
        ..Default::default()
    });

    let problems = audit_config_problems(&config, "", Some(""));
    let anthropic_missing = problems.iter().find(|p| {
        matches!(
            &p.kind,
            ConfigProblemKind::MissingProviderApiKey { provider, .. } if provider == "anthropic"
        )
    });
    assert!(
        anthropic_missing.is_some(),
        "Expected MissingProviderApiKey for anthropic"
    );
    assert_eq!(anthropic_missing.unwrap().severity, Severity::Error);

    let openai_missing = problems.iter().find(|p| {
        matches!(
            &p.kind,
            ConfigProblemKind::MissingProviderApiKey { provider, .. } if provider == "openai"
        )
    });
    assert!(
        openai_missing.is_none(),
        "OpenAI with valid key should not be reported missing"
    );
}

#[test]
fn test_audit_custom_provider_missing_base_url() {
    let mut config = Config::default();
    let mut custom = std::collections::BTreeMap::new();
    custom.insert(
        "local_ollama".to_string(),
        crate::config::ProviderConfig {
            enabled: true,
            base_url: Some("".into()),
            api_key: None,
            ..Default::default()
        },
    );
    config.providers.custom = Some(custom);

    let problems = audit_config_problems(&config, "", Some(""));
    assert!(
        problems.iter().any(|p| matches!(
            &p.kind,
            ConfigProblemKind::CustomProviderMissingBaseUrl { provider } if provider == "local_ollama"
        )),
        "Expected CustomProviderMissingBaseUrl for local_ollama"
    );
}

#[test]
fn test_audit_invalid_fallback_chain() {
    let mut config = Config::default();
    config.providers.fallback = Some(crate::config::types::FallbackProviderConfig {
        enabled: true,
        providers: vec!["unconfigured_vendor".into(), "openai".into()],
        ..Default::default()
    });
    config.providers.openai = Some(crate::config::ProviderConfig {
        enabled: true,
        api_key: Some("sk-valid".into()),
        ..Default::default()
    });

    let problems = audit_config_problems(&config, "", Some(""));
    let unconfigured = problems.iter().find(|p| {
        matches!(
            &p.kind,
            ConfigProblemKind::InvalidFallbackProvider { provider, .. } if provider == "unconfigured_vendor"
        )
    });
    assert!(
        unconfigured.is_some(),
        "Expected InvalidFallbackProvider for unconfigured_vendor"
    );
    assert_eq!(unconfigured.unwrap().severity, Severity::Warning);

    let openai_fb = problems.iter().find(|p| {
        matches!(
            &p.kind,
            ConfigProblemKind::InvalidFallbackProvider { provider, .. } if provider == "openai"
        )
    });
    assert!(
        openai_fb.is_none(),
        "Configured fallback provider should not be reported invalid"
    );
}

#[test]
fn test_audit_ignored_and_typo_keys() {
    let config = Config::default();
    let raw_config =
        "[agent]\ndefault_provider = \"anthropic\"\n[my_typo_section]\nfoo = \"bar\"\n";

    let problems = audit_config_problems(&config, raw_config, Some(""));
    let typo_problem = problems.iter().find(|p| {
        matches!(&p.kind, ConfigProblemKind::IgnoredOrTypoConfigKeys(keys) if keys.contains(&"my_typo_section".to_string()))
    });
    assert!(
        typo_problem.is_some(),
        "Expected IgnoredOrTypoConfigKeys for unrecognized section"
    );
}

#[test]
fn test_alert_formatting_and_length_truncation() {
    let mut problems = Vec::new();
    for i in 0..50 {
        problems.push(ConfigProblem {
            kind: ConfigProblemKind::MissingProviderApiKey {
                provider: format!("provider_{i}"),
                model: format!("model_{i}"),
            },
            severity: Severity::Error,
            remediation: format!("Fix credentials for provider_{i}"),
        });
    }

    let alert = format_config_alert(&problems, Some("ops"));
    assert!(
        alert.contains("OpenCrabs Configuration Alert") && alert.contains("(profile: ops)"),
        "Header should include profile tag"
    );
    assert!(
        alert.len() <= 4000,
        "Alert formatted text must be safely bounded under Telegram's 4096 char limit, was {}",
        alert.len()
    );
    assert!(
        alert.contains("additional issues"),
        "Expected truncation note when many problems exist"
    );

    let recovery = format_config_recovery(Some("ops"));
    assert!(
        recovery.contains("OpenCrabs Configuration Restored")
            && recovery.contains("(profile: ops)"),
        "Recovery message should be cleanly formatted"
    );
}

#[test]
fn test_command_catalog_overflow_alert_formatting() {
    let problem = ConfigProblem {
        kind: ConfigProblemKind::CommandCatalogOverflow { count: 105 },
        severity: Severity::Warning,
        remediation: "Keep skill descriptions concise".into(),
    };

    let alert = format_config_alert(&[problem], Some("ops"));
    assert!(alert.contains("Too Many Commands / Skills"));
    assert!(alert.contains("105 commands"));
    assert!(alert.contains("exceeding Telegram's hard limit of 100 commands"));
}

#[test]
fn test_custom_provider_prefix_variations() {
    let mut config = Config::default();
    let mut custom = std::collections::BTreeMap::new();
    custom.insert(
        "llm-gateway".to_string(),
        crate::config::ProviderConfig {
            enabled: true,
            base_url: Some("http://localhost:8000/v1".into()),
            api_key: Some("test-key".into()),
            ..Default::default()
        },
    );
    config.providers.custom = Some(custom);

    // Test default_provider with "custom.", "custom:", "custom/", and bare "llm-gateway"
    for prefix in &["custom.", "custom:", "custom/", ""] {
        config.agent.default_provider = Some(format!("{prefix}llm-gateway"));
        let problems = audit_config_problems(&config, "", Some(""));
        let missing = problems.iter().find(|p| {
            matches!(
                &p.kind,
                ConfigProblemKind::DefaultProviderDisabledOrMissing { .. }
            )
        });
        assert!(
            missing.is_none(),
            "Expected default_provider with prefix '{prefix}' to resolve cleanly to custom.llm-gateway"
        );
    }

    // Test fallback providers with custom prefixes
    config.agent.default_provider = None;
    config.providers.fallback = Some(crate::config::types::FallbackProviderConfig {
        enabled: true,
        providers: vec![
            "custom.llm-gateway".into(),
            "custom:llm-gateway".into(),
            "custom/llm-gateway".into(),
        ],
        ..Default::default()
    });

    let problems = audit_config_problems(&config, "", Some(""));
    let invalid_fb = problems
        .iter()
        .find(|p| matches!(&p.kind, ConfigProblemKind::InvalidFallbackProvider { .. }));
    assert!(
        invalid_fb.is_none(),
        "Expected all custom prefix variations in fallback.providers to resolve cleanly"
    );
}

#[test]
fn test_alert_deduplication_lifecycle() {
    let state = ConfigAlertState::new();

    let problem_a = ConfigProblem {
        kind: ConfigProblemKind::KeysFileNotFound,
        severity: Severity::Warning,
        remediation: "Create keys.toml".into(),
    };
    let problem_b = ConfigProblem {
        kind: ConfigProblemKind::MissingProviderApiKey {
            provider: "anthropic".into(),
            model: "claude".into(),
        },
        severity: Severity::Error,
        remediation: "Set API key".into(),
    };

    // 1. Initial problem set -> should alert
    let set1 = vec![problem_a.clone()];
    assert!(state.should_alert(&set1));
    state.record_alert(&set1);

    // 2. Same problem set -> should suppress
    assert!(!state.should_alert(&set1));

    // 3. New problem added -> should alert
    let set2 = vec![problem_a.clone(), problem_b.clone()];
    assert!(state.should_alert(&set2));
    state.record_alert(&set2);

    // 4. Same two problems -> should suppress
    assert!(!state.should_alert(&set2));

    // 5. Problems cleared -> should alert for recovery
    let empty_set: Vec<ConfigProblem> = Vec::new();
    assert!(state.should_alert(&empty_set));

    // 6. Recovery handled and reset
    assert!(state.check_and_reset_recovery());
    assert!(!state.should_alert(&empty_set));
}

#[test]
fn test_recipient_chat_id_resolution() {
    let mut config = Config::default();
    config.channels.telegram.bot_owner = vec!["12345".into(), "invalid_str".into(), "67890".into()];
    config.channels.telegram.allowed_users = vec!["99999".into()];

    let recipients = resolve_recipient_chat_ids(&config, Some(12345));
    // 12345 (owner) and 67890 (owner) resolved, invalid_str filtered, 12345 deduplicated
    assert_eq!(recipients, vec![12345, 67890]);

    // Fallback to allowed_users when bot_owner is empty
    config.channels.telegram.bot_owner = Vec::new();
    let recipients_fb = resolve_recipient_chat_ids(&config, None);
    assert_eq!(recipients_fb, vec![99999]);
}

// ---------------------------------------------------------------------------
// Shared fixtures for the provider-resolution audits (#353).
//
// Both audits now ask ONE question through `resolve_provider_name`, so these
// tests pin the answer per class of name rather than re-deriving it per ladder.
// Each test states whether it DISCRIMINATES (its assertion fails on the pre-fix
// tree) or is a lock-in (it holds on both sides of the change).
// ---------------------------------------------------------------------------

/// A `[providers.<id>]` section shaped the way a factory expects a usable one.
fn provider(enabled: bool, api_key: Option<&str>) -> crate::config::ProviderConfig {
    crate::config::ProviderConfig {
        enabled,
        api_key: api_key.map(str::to_string),
        ..Default::default()
    }
}

/// A `[providers.custom.<name>]` section: usable only when enabled AND
/// carrying a non-empty base_url.
fn custom_provider(enabled: bool) -> crate::config::ProviderConfig {
    crate::config::ProviderConfig {
        enabled,
        base_url: Some("http://localhost:8000/v1".into()),
        api_key: Some("test-key".into()),
        ..Default::default()
    }
}

fn fallback_of(names: &[&str]) -> crate::config::types::FallbackProviderConfig {
    crate::config::types::FallbackProviderConfig {
        enabled: true,
        providers: names.iter().map(|n| (*n).to_string()).collect(),
        ..Default::default()
    }
}

/// Every provider the fallback audit reported invalid, in audit order.
fn invalid_fallback_providers(config: &Config) -> Vec<String> {
    audit_config_problems(config, "", Some(""))
        .into_iter()
        .filter_map(|p| match p.kind {
            ConfigProblemKind::InvalidFallbackProvider { provider, .. } => Some(provider),
            _ => None,
        })
        .collect()
}

/// Whether the default-provider audit flagged `name`.
fn default_provider_flagged(config: &Config, name: &str) -> bool {
    audit_config_problems(config, "", Some(""))
        .iter()
        .any(|p| {
            matches!(
                &p.kind,
                ConfigProblemKind::DefaultProviderDisabledOrMissing { provider } if provider.as_str() == name
            )
        })
}

/// #353's filed defect: a CLI / OAuth provider carries no API key, so the old
/// ladder reported every one of these names invalid — five through keyed arms,
/// `opencode` through the custom-only wildcard — while `create_fallback` builds
/// them all.
///
/// DISCRIMINATING: produced one row per name before #353.
#[test]
fn test_audit_fallback_cli_providers_need_no_api_key() {
    let mut config = Config::default();
    config.providers.fallback = Some(fallback_of(&[
        "claude-cli",
        "opencode-cli",
        "codex-cli",
        "command-code-cli",
        "codex",
    ]));
    // Enabled and deliberately keyless: the credential rule these ids declare
    // is "no key required".
    config.providers.claude_cli = Some(provider(true, None));
    config.providers.opencode_cli = Some(provider(true, None));
    config.providers.codex_cli = Some(provider(true, None));
    config.providers.command_code_cli = Some(provider(true, None));
    config.providers.codex = Some(provider(true, None));

    assert_eq!(
        invalid_fallback_providers(&config),
        Vec::<String>::new(),
        "CLI providers carry no API key; they must not be reported invalid"
    );
}

/// Every id in `REGISTRATIONS` resolves once its credential rule is satisfied,
/// which is the property the audit now inherits from the runtime.
///
/// LOCK-IN for the ten keyed ids; DISCRIMINATING for the seven keyless ones,
/// which the old ladder reached only through its custom-only wildcard.
#[test]
fn test_audit_fallback_all_registered_builtins_resolve() {
    let mut config = Config::default();
    // The ten registry rows that declare `requires_api_key = true`.
    config.providers.anthropic = Some(provider(true, Some("test-key")));
    config.providers.openai = Some(provider(true, Some("test-key")));
    config.providers.gemini = Some(provider(true, Some("test-key")));
    config.providers.openrouter = Some(provider(true, Some("test-key")));
    config.providers.minimax = Some(provider(true, Some("test-key")));
    config.providers.zai = Some(provider(true, Some("test-key")));
    config.providers.moonshot = Some(provider(true, Some("test-key")));
    config.providers.xiaomi = Some(provider(true, Some("test-key")));
    config.providers.qwen = Some(provider(true, Some("test-key")));
    config.providers.github = Some(provider(true, Some("test-key")));
    // The seven that declare `requires_api_key = false`.
    config.providers.ollama = Some(provider(true, None));
    config.providers.claude_cli = Some(provider(true, None));
    config.providers.opencode_cli = Some(provider(true, None));
    config.providers.codex_cli = Some(provider(true, None));
    config.providers.command_code_cli = Some(provider(true, None));
    config.providers.codex = Some(provider(true, None));
    config.providers.opencode = Some(provider(true, Some("test-key")));
    config.providers.fallback = Some(fallback_of(&[
        "anthropic",
        "openai",
        "gemini",
        "openrouter",
        "minimax",
        "zai",
        "moonshot",
        "xiaomi",
        "qwen",
        "github",
        "ollama",
        "claude-cli",
        "opencode-cli",
        "codex-cli",
        "command-code-cli",
        "codex",
        "opencode",
    ]));

    assert_eq!(
        invalid_fallback_providers(&config),
        Vec::<String>::new(),
        "every registered built-in must resolve once its credential rule is met"
    );
}

/// The fallback audit reads `normalized_fallback_chain`, so the two spellings
/// the chain builder already corrects no longer produce phantom warnings.
///
/// DISCRIMINATING: both names produced a row before #353.
#[test]
fn test_audit_fallback_normalised_names_and_aliases() {
    let mut config = Config::default();
    config.providers.fallback = Some(fallback_of(&["openai/gpt-4o", "claude_cli"]));
    config.providers.openai = Some(provider(true, Some("sk-valid")));
    config.providers.claude_cli = Some(provider(true, None));

    assert_eq!(
        invalid_fallback_providers(&config),
        Vec::<String>::new(),
        "`provider/model` splits to `provider`, and `claude_cli` is a registered alias"
    );
}

/// A `[providers.custom.<name>]` section literally named after a built-in id
/// wins, exactly as `create_fallback` resolves it.
///
/// DISCRIMINATING IN BOTH DIRECTIONS: before #353 the built-in's own arm
/// decided `openai`, so a disabled custom section was ignored and an enabled
/// one was shadowed.
#[test]
fn test_audit_fallback_custom_section_beats_builtin_id() {
    // Custom disabled, built-in enabled and keyed -> the CUSTOM decides.
    let mut config = Config::default();
    let mut custom = std::collections::BTreeMap::new();
    custom.insert("openai".to_string(), custom_provider(false));
    config.providers.custom = Some(custom);
    config.providers.openai = Some(provider(true, Some("sk-valid")));
    config.providers.fallback = Some(fallback_of(&["openai"]));
    assert_eq!(
        invalid_fallback_providers(&config),
        vec!["openai".to_string()],
        "a disabled custom `openai` section shadows the built-in, as the runtime resolves it"
    );

    // Custom enabled, built-in disabled -> the CUSTOM decides again.
    let mut config = Config::default();
    let mut custom = std::collections::BTreeMap::new();
    custom.insert("openai".to_string(), custom_provider(true));
    config.providers.custom = Some(custom);
    config.providers.openai = Some(provider(false, Some("sk-valid")));
    config.providers.fallback = Some(fallback_of(&["openai"]));
    assert_eq!(
        invalid_fallback_providers(&config),
        Vec::<String>::new(),
        "an enabled custom `openai` section shadows the disabled built-in"
    );
}

/// Names whose verdict is NOT decided by the resolver: the P1/P2 divergences
/// tracked in issue #461, plus the two registry rows that have no factory.
#[test]
fn test_audit_fallback_predicate_divergences_are_locked_in() {
    // (b) `bedrock` / `vertex` have a `provider_registry()` row but no
    // `REGISTRATIONS` entry and no factory, so `create_fallback` rejects them
    // as an unknown provider — a key does not make them buildable.
    // LOCK-IN: reported before and after #353.
    let mut config = Config::default();
    config.providers.fallback = Some(fallback_of(&["bedrock", "vertex"]));
    config.providers.bedrock = Some(provider(true, Some("test-key")));
    config.providers.vertex = Some(provider(true, Some("test-key")));
    assert_eq!(
        invalid_fallback_providers(&config),
        vec!["bedrock".to_string(), "vertex".to_string()],
        "bedrock/vertex are declared but unbuildable; a key must not silence the warning"
    );

    // (i) P1 (issue #461): `cfg.enabled` is a false positive for the eleven
    // factories that never branch on it, so a disabled-but-keyed provider is
    // still reported. LOCK-IN: reported before and after #353.
    let mut config = Config::default();
    config.providers.fallback = Some(fallback_of(&["minimax", "opencode"]));
    config.providers.minimax = Some(provider(false, Some("test-key")));
    config.providers.opencode = Some(provider(false, Some("test-key")));
    assert_eq!(
        invalid_fallback_providers(&config),
        vec!["minimax".to_string(), "opencode".to_string()],
        "the audit's `enabled` conjunct is unchanged by #353 (P1, issue #461)"
    );

    // (j) P2 (issue #461): `opencode` is declared keyless in the registry but
    // its factory hard-requires a key, so an enabled-but-keyless `opencode`
    // now reads VALID while `create_fallback` skips it. #353 turns a
    // correct-by-luck warning into silence; the divergence is tracked, not
    // fixed here.
    // DISCRIMINATING: a row was emitted before #353, via the custom-only
    // wildcard, which happened to give the right answer for the wrong reason.
    let mut config = Config::default();
    config.providers.fallback = Some(fallback_of(&["opencode"]));
    config.providers.opencode = Some(provider(true, None));
    assert_eq!(
        invalid_fallback_providers(&config),
        Vec::<String>::new(),
        "P2 divergence (issue #461): the audit trusts the registry's requires_api_key flag"
    );
}

/// Boot-time DB integrity verdict renders through the config-alert path (#459).
///
/// DISCRIMINATING: `None` (a clean check) must yield no problem at all, so a
/// healthy boot stays silent rather than DMing the owner about a healthy DB.
#[test]
fn db_integrity_problem_renders_the_sqlite_detail() {
    // A clean check yields no problem, so a healthy boot stays silent.
    assert!(db_integrity_problem(None).is_none());

    let problem = db_integrity_problem(Some("Page 42 is never used"))
        .expect("a detail must produce a problem");
    assert_eq!(problem.severity, Severity::Error);
    assert!(matches!(
        problem.kind,
        ConfigProblemKind::DatabaseIntegrityFailed { .. }
    ));
    assert_eq!(problem.kind.title(), "Database Integrity Check Failed");
    assert!(
        problem.kind.details().contains("Page 42 is never used"),
        "details must carry SQLite's own message, got: {}",
        problem.kind.details()
    );

    let rendered = format_config_alert(std::slice::from_ref(&problem), Some("ops"));
    assert!(
        rendered.contains("Database Integrity Check Failed"),
        "the alert must render the corruption title, got: {rendered}"
    );
}

/// The default-provider audit resolves the name the same way, so the two
/// spellings it used to get wrong now agree with the runtime.
///
/// DISCRIMINATING on both counts: the old ladder carried a `claude-cli` arm
/// only, so the `claude_cli` alias fell through to a custom-only lookup; and it
/// carried explicit `bedrock`/`vertex` arms that reported them valid although
/// no factory builds them.
#[test]
fn test_audit_default_provider_alias_and_stub_deltas() {
    // (g) The underscore alias resolves to the built-in.
    let mut config = Config::default();
    config.agent.default_provider = Some("claude_cli".into());
    config.providers.claude_cli = Some(provider(true, None));
    assert!(
        !default_provider_flagged(&config, "claude_cli"),
        "`claude_cli` is a registered alias and must resolve like `claude-cli`"
    );

    // (h) A registry row without a factory is not a buildable default.
    for stub in ["bedrock", "vertex"] {
        let mut config = Config::default();
        config.agent.default_provider = Some(stub.into());
        match stub {
            "bedrock" => config.providers.bedrock = Some(provider(true, Some("test-key"))),
            _ => config.providers.vertex = Some(provider(true, Some("test-key"))),
        }
        assert!(
            default_provider_flagged(&config, stub),
            "{stub} has no factory, so it cannot serve as the default provider"
        );
    }
}
