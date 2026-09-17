use opencrabs::brain::provider::create_provider_with_warning;
use opencrabs::config::Config;

#[tokio::test]
async fn test_agent_default_provider_honored_in_factory() {
    let mut config = Config::default();
    config.agent.default_provider = Some("anthropic".to_string());
    // anthropic has no api_key, so create_provider_by_name returns Err
    // create_provider_with_warning logs warning and falls back to placeholder
    let (provider, warning) = create_provider_with_warning(&config).await.unwrap();
    assert!(warning.is_some());
    assert!(warning
        .unwrap()
        .contains("Configured default provider 'anthropic' failed to initialize"));
    assert_eq!(provider.name(), "placeholder");
}

#[tokio::test]
async fn test_agent_default_provider_custom_honored_in_factory() {
    let mut config = Config::default();
    config.agent.default_provider = Some("custom:my-llm".to_string());
    config.providers.custom.insert(
        "my-llm".to_string(),
        opencrabs::config::types::CustomProviderConfig {
            base_url: "http://localhost:8000/v1".to_string(),
            api_key: Some("secret".to_string()),
            models: vec!["custom-model".to_string()],
            ..Default::default()
        },
    );

    let (provider, warning) = create_provider_with_warning(&config).await.unwrap();
    assert!(warning.is_none());
    assert_eq!(provider.name(), "custom:my-llm");
}

#[test]
fn test_agent_default_provider_cron_fallback_logic() {
    let mut config = Config::default();
    config.agent.default_provider = Some("custom:agent-llm".to_string());
    config.agent.default_model = Some("agent-model-1".to_string());

    // cron default is unset
    let effective_provider = config
        .cron
        .default_provider
        .clone()
        .or_else(|| config.agent.default_provider.clone());
    let effective_model = config
        .cron
        .default_model
        .clone()
        .or_else(|| config.agent.default_model.clone());

    assert_eq!(effective_provider, Some("custom:agent-llm".to_string()));
    assert_eq!(effective_model, Some("agent-model-1".to_string()));

    // cron default set overrides agent default
    config.cron.default_provider = Some("minimax".to_string());
    config.cron.default_model = Some("MiniMax-Text-01".to_string());

    let effective_provider2 = config
        .cron
        .default_provider
        .clone()
        .or_else(|| config.agent.default_provider.clone());
    let effective_model2 = config
        .cron
        .default_model
        .clone()
        .or_else(|| config.agent.default_model.clone());

    assert_eq!(effective_provider2, Some("minimax".to_string()));
    assert_eq!(effective_model2, Some("MiniMax-Text-01".to_string()));
}
