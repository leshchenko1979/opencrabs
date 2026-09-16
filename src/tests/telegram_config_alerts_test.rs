use crate::channels::telegram::config_alerts::{
    ConfigAlertState, ConfigProblem, ConfigProblemKind, Severity, audit_config_problems,
    format_config_alert, format_config_recovery, resolve_recipient_chat_ids,
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
