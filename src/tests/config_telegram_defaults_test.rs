use crate::config::{Config, TelegramConfig};

#[test]
fn test_telegram_config_default_struct_booleans() {
    let cfg = TelegramConfig::default();
    assert!(cfg.rich_messages, "rich_messages must default to true");
    assert!(cfg.mermaid_render, "mermaid_render must default to true");
    assert!(
        cfg.silence_group_start,
        "silence_group_start must default to true"
    );
    assert!(!cfg.enabled, "enabled must default to false");
    assert!(cfg.token.is_none(), "token must default to None");
    assert!(
        cfg.allowed_users.is_empty(),
        "allowed_users must default to empty"
    );
    assert!(
        cfg.allowed_channels.is_empty(),
        "allowed_channels must default to empty"
    );
    assert!(cfg.bot_owner.is_empty(), "bot_owner must default to empty");
    assert!(cfg.groups.is_empty(), "groups must default to empty");
    assert!(
        cfg.rate_limiter.enabled,
        "rate_limiter must default to enabled"
    );
}

#[test]
fn test_config_missing_channels_table_toml() {
    let toml_str = r#"
        # Complete absence of [channels] and [channels.telegram]
        [agent]
        max_tool_iterations = 10
    "#;
    let cfg: Config = toml::from_str(toml_str).expect("config must parse without channels table");
    let tg = cfg.channels.telegram;
    assert!(
        tg.rich_messages,
        "rich_messages must be true when [channels] is omitted"
    );
    assert!(
        tg.mermaid_render,
        "mermaid_render must be true when [channels] is omitted"
    );
    assert!(
        tg.silence_group_start,
        "silence_group_start must be true when [channels] is omitted"
    );
}

#[test]
fn test_config_missing_channels_telegram_table_toml() {
    let toml_str = r#"
        [channels]
        # [channels.telegram] omitted
    "#;
    let cfg: Config = toml::from_str(toml_str).expect("config must parse with empty [channels]");
    let tg = cfg.channels.telegram;
    assert!(
        tg.rich_messages,
        "rich_messages must be true when [channels.telegram] is omitted"
    );
    assert!(
        tg.mermaid_render,
        "mermaid_render must be true when [channels.telegram] is omitted"
    );
    assert!(
        tg.silence_group_start,
        "silence_group_start must be true when [channels.telegram] is omitted"
    );
}

#[test]
fn test_config_empty_channels_telegram_table_toml() {
    let toml_str = r#"
        [channels.telegram]
        # no keys specified inside table
    "#;
    let cfg: Config =
        toml::from_str(toml_str).expect("config must parse with bare [channels.telegram]");
    let tg = cfg.channels.telegram;
    assert!(
        tg.rich_messages,
        "rich_messages must be true when [channels.telegram] is empty"
    );
    assert!(
        tg.mermaid_render,
        "mermaid_render must be true when [channels.telegram] is empty"
    );
    assert!(
        tg.silence_group_start,
        "silence_group_start must be true when [channels.telegram] is empty"
    );
}

#[test]
fn test_config_explicit_false_overrides_respected() {
    let toml_str = r#"
        [channels.telegram]
        rich_messages = false
        mermaid_render = false
        silence_group_start = false
    "#;
    let cfg: Config =
        toml::from_str(toml_str).expect("config must parse with explicit false overrides");
    let tg = cfg.channels.telegram;
    assert!(
        !tg.rich_messages,
        "explicit rich_messages = false must be respected"
    );
    assert!(
        !tg.mermaid_render,
        "explicit mermaid_render = false must be respected"
    );
    assert!(
        !tg.silence_group_start,
        "explicit silence_group_start = false must be respected"
    );
}

#[test]
fn test_config_partial_override_preserves_other_defaults() {
    let toml_str = r#"
        [channels.telegram]
        rich_messages = false
    "#;
    let cfg: Config = toml::from_str(toml_str).expect("config must parse with partial override");
    let tg = cfg.channels.telegram;
    assert!(!tg.rich_messages, "rich_messages = false must be applied");
    assert!(tg.mermaid_render, "mermaid_render must remain true default");
    assert!(
        tg.silence_group_start,
        "silence_group_start must remain true default"
    );
}

#[test]
fn test_config_mermaid_theme_and_bg_default_to_auto() {
    let d = TelegramConfig::default();
    assert_eq!(d.mermaid_theme, "auto", "mermaid_theme must default to auto");
    assert_eq!(d.mermaid_bg, "auto", "mermaid_bg must default to auto");

    // [channels] omitted entirely.
    let cfg: Config = toml::from_str("[agent]\nmax_tool_iterations = 10\n")
        .expect("config must parse without channels table");
    let tg = cfg.channels.telegram;
    assert_eq!(
        tg.mermaid_theme, "auto",
        "mermaid_theme must default to auto when [channels] is omitted"
    );
    assert_eq!(
        tg.mermaid_bg, "auto",
        "mermaid_bg must default to auto when [channels] is omitted"
    );

    // Bare [channels.telegram] table.
    let cfg: Config = toml::from_str("[channels.telegram]\n")
        .expect("config must parse with a bare [channels.telegram]");
    let tg = cfg.channels.telegram;
    assert_eq!(
        tg.mermaid_theme, "auto",
        "mermaid_theme must default to auto when [channels.telegram] is empty"
    );
    assert_eq!(
        tg.mermaid_bg, "auto",
        "mermaid_bg must default to auto when [channels.telegram] is empty"
    );
}

#[test]
fn test_config_mermaid_theme_and_bg_explicit_values_respected() {
    let cfg: Config = toml::from_str(
        "[channels.telegram]\nmermaid_theme = \"forest\"\nmermaid_bg = \"none\"\n",
    )
    .expect("config must parse with explicit mermaid styling");
    let tg = cfg.channels.telegram;
    assert_eq!(
        tg.mermaid_theme, "forest",
        "explicit mermaid_theme must be respected"
    );
    assert_eq!(
        tg.mermaid_bg, "none",
        "explicit mermaid_bg must be respected"
    );

    // A partial override must not disturb the other new key.
    let cfg: Config = toml::from_str("[channels.telegram]\nmermaid_theme = \"neutral\"\n")
        .expect("config must parse with a partial mermaid override");
    let tg = cfg.channels.telegram;
    assert_eq!(
        tg.mermaid_theme, "neutral",
        "mermaid_theme = neutral must be applied"
    );
    assert_eq!(
        tg.mermaid_bg, "auto",
        "mermaid_bg must remain the auto default"
    );
}
