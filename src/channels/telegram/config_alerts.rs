//!
//! Proactive DM configuration problem alerting to `bot_owner` (#263).
//!
//! Inspects configuration and credentials integrity during Telegram startup and
//! background config reload cycles, dedupes alerts via stateful hashing, and
//! delivers actionable diagnostics directly to the owner in Telegram DM.

use crate::brain::provider::factory::{
    NameResolution, normalized_fallback_chain, resolve_provider_name,
};
use crate::config::Config;
use serde::{Deserialize, Serialize};
use std::sync::Mutex;
use teloxide::prelude::*;

/// Problem severity classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Severity {
    Warning,
    Error,
}

/// Category and structured data of a detected configuration defect.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ConfigProblemKind {
    KeysFileNotFound,
    KeysFileReadError(String),
    KeysFileParseError(String),
    MissingProviderApiKey { provider: String, model: String },
    CustomProviderMissingBaseUrl { provider: String },
    DefaultProviderDisabledOrMissing { provider: String },
    InvalidFallbackProvider { provider: String, reason: String },
    EmbeddingKeyMissing { provider: String },
    IgnoredOrTypoConfigKeys(Vec<String>),
    NoValidBotOwnerConfigured,
    CommandCatalogOverflow { count: usize },
}

impl ConfigProblemKind {
    /// Short human-readable title.
    pub fn title(&self) -> &'static str {
        match self {
            Self::KeysFileNotFound => "keys.toml Not Found",
            Self::KeysFileReadError(_) => "keys.toml Read Error",
            Self::KeysFileParseError(_) => "keys.toml Syntax / Parse Error",
            Self::MissingProviderApiKey { .. } => "Missing Provider API Key",
            Self::CustomProviderMissingBaseUrl { .. } => "Custom Provider Missing Base URL",
            Self::DefaultProviderDisabledOrMissing { .. } => "Default Provider Disabled or Missing",
            Self::InvalidFallbackProvider { .. } => "Invalid Fallback Provider",
            Self::EmbeddingKeyMissing { .. } => "Memory Embedding API Key Missing",
            Self::IgnoredOrTypoConfigKeys(_) => "Unrecognized / Typo Config Keys",
            Self::NoValidBotOwnerConfigured => "No Valid Telegram bot_owner Configured",
            Self::CommandCatalogOverflow { .. } => "Too Many Commands / Skills",
        }
    }

    /// Detailed diagnostic message.
    pub fn details(&self) -> String {
        match self {
            Self::KeysFileNotFound => "keys.toml file does not exist. Provider API keys may be missing.".into(),
            Self::KeysFileReadError(e) => format!("Failed to read keys.toml: {e}"),
            Self::KeysFileParseError(e) => format!("Failed to parse keys.toml as TOML: {e}"),
            Self::MissingProviderApiKey { provider, model } => {
                format!("Provider '{provider}' is enabled (model: {model}) but has no API key configured.")
            }
            Self::CustomProviderMissingBaseUrl { provider } => {
                format!("Custom provider '{provider}' is enabled but has an empty base_url.")
            }
            Self::DefaultProviderDisabledOrMissing { provider } => {
                format!("Default provider '{provider}' is not configured or disabled.")
            }
            Self::InvalidFallbackProvider { provider, reason } => {
                format!("Fallback provider '{provider}' is invalid: {reason}.")
            }
            Self::EmbeddingKeyMissing { provider } => {
                format!("Vector memory embeddings are enabled with '{provider}' but no API key is set.")
            }
            Self::IgnoredOrTypoConfigKeys(keys) => {
                format!("The following unrecognized keys in config.toml are silently ignored: {}", keys.join(", "))
            }
            Self::NoValidBotOwnerConfigured => {
                "No numeric Telegram user ID was found in channels.telegram.bot_owner or allowed_users.".into()
            }
            Self::CommandCatalogOverflow { count } => {
                format!(
                    "Command & skill catalog has {count} commands, exceeding Telegram's hard limit of 100 commands per scope."
                )
            }
        }
    }
}

/// A concrete configuration problem with its severity and recommended remediation.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ConfigProblem {
    pub kind: ConfigProblemKind,
    pub severity: Severity,
    pub remediation: String,
}

/// Audit configuration and credentials, returning all detected problems.
pub fn audit_config_problems(
    config: &Config,
    raw_config_toml: &str,
    raw_keys_toml: Option<&str>,
) -> Vec<ConfigProblem> {
    let mut problems = Vec::new();

    // 1. Audit keys.toml
    match raw_keys_toml {
        None => {
            let keys_path = crate::config::seed::keys_path();
            if !keys_path.exists() {
                problems.push(ConfigProblem {
                    kind: ConfigProblemKind::KeysFileNotFound,
                    severity: Severity::Warning,
                    remediation:
                        "Create keys.toml with provider credentials via /onboard or manual edit"
                            .into(),
                });
            } else {
                problems.push(ConfigProblem {
                    kind: ConfigProblemKind::KeysFileReadError("could not read file".into()),
                    severity: Severity::Error,
                    remediation: "Check permissions on keys.toml".into(),
                });
            }
        }
        Some(content) => {
            if let Err(e) = toml::from_str::<toml::Value>(content) {
                problems.push(ConfigProblem {
                    kind: ConfigProblemKind::KeysFileParseError(e.to_string()),
                    severity: Severity::Error,
                    remediation: "Correct syntax error in keys.toml".into(),
                });
            }
        }
    }

    // 2. Audit enabled built-in providers
    let built_in_providers = [
        ("anthropic", &config.providers.anthropic),
        ("openai", &config.providers.openai),
        ("gemini", &config.providers.gemini),
        ("openrouter", &config.providers.openrouter),
        ("minimax", &config.providers.minimax),
        ("zai", &config.providers.zai),
        ("moonshot", &config.providers.moonshot),
        ("xiaomi", &config.providers.xiaomi),
        ("qwen", &config.providers.qwen),
    ];

    for (name, provider_opt) in built_in_providers {
        if let Some(p) = provider_opt
            && p.enabled
            && p.api_key.as_ref().is_none_or(|k| k.trim().is_empty())
        {
            problems.push(ConfigProblem {
                kind: ConfigProblemKind::MissingProviderApiKey {
                    provider: name.to_string(),
                    model: p
                        .default_model
                        .as_deref()
                        .unwrap_or("(not set)")
                        .to_string(),
                },
                severity: Severity::Error,
                remediation: format!(
                    "Set API key for [{name}] in keys.toml or run /onboard:{name}"
                ),
            });
        }
    }

    // 3. Audit custom providers
    if let Some(ref custom) = config.providers.custom {
        for (name, p) in custom {
            if p.enabled && p.base_url.as_ref().is_none_or(|u| u.trim().is_empty()) {
                problems.push(ConfigProblem {
                    kind: ConfigProblemKind::CustomProviderMissingBaseUrl {
                        provider: name.clone(),
                    },
                    severity: Severity::Error,
                    remediation: format!(
                        "Set base_url under [providers.custom.{name}] in config.toml"
                    ),
                });
            }
        }
    }

    // 4. Audit default provider
    if let Some(dp) = config
        .agent
        .default_provider
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty() && *s != "none")
    {
        // Resolve the name exactly as `create_fallback` would, then apply the
        // UNCHANGED enabled-only predicate (Decision D1). Widening it here to
        // demand credentials would emit new owner-visible warnings for
        // enabled-but-keyless keyed providers - a different defect, tracked
        // separately rather than folded into this change.
        let is_valid = match resolve_provider_name(config, dp) {
            Some(NameResolution::Builtin { cfg, .. }) => cfg.enabled,
            Some(NameResolution::Custom(cfg)) => {
                cfg.enabled && cfg.base_url.as_ref().is_some_and(|u| !u.trim().is_empty())
            }
            None => false,
        };

        if !is_valid {
            problems.push(ConfigProblem {
                kind: ConfigProblemKind::DefaultProviderDisabledOrMissing {
                    provider: dp.to_string(),
                },
                severity: Severity::Error,
                remediation: format!(
                    "Enable provider '{dp}' or update agent.default_provider in config.toml"
                ),
            });
        }
    }

    // 5. Audit fallback providers
    if let Some(ref fb) = config.providers.fallback
        && fb.enabled
    {
        // Audit the list the runtime actually builds (Decision D2):
        // normalized_fallback_chain drops the custom: prefix, rewrites
        // provider/model to provider, and dedupes in order - so the audit
        // cannot flag a spelling the chain builder already corrected.
        for p_name in normalized_fallback_chain(config) {
            let is_valid = match resolve_provider_name(config, &p_name) {
                Some(NameResolution::Builtin {
                    cfg,
                    requires_api_key,
                }) => {
                    // The canonical credential rule (no key required, or a key
                    // present) with the audit's own STRICTER test kept: a blank
                    // key is not a credential, even though the loader's
                    // api_key.is_some() would accept Some("").
                    cfg.enabled
                        && (!requires_api_key
                            || cfg.api_key.as_ref().is_some_and(|k| !k.trim().is_empty()))
                }
                Some(NameResolution::Custom(cfg)) => {
                    cfg.enabled && cfg.base_url.as_ref().is_some_and(|u| !u.trim().is_empty())
                }
                None => false,
            };

            if !is_valid {
                problems.push(ConfigProblem {
                    kind: ConfigProblemKind::InvalidFallbackProvider {
                        provider: p_name.clone(),
                        reason: "provider is not configured, disabled, or lacks credentials".into(),
                    },
                    severity: Severity::Warning,
                    remediation: format!("Configure provider '{p_name}' or remove it from fallback.providers in config.toml"),
                });
            }
        }
    }

    // 6. Audit memory embeddings
    if config.memory.vector_enabled
        && let Some(ref emb) = config.memory.embedding
        && emb.url.is_some()
        && emb.api_key.as_ref().is_none_or(|k| k.trim().is_empty())
    {
        problems.push(ConfigProblem {
            kind: ConfigProblemKind::EmbeddingKeyMissing {
                provider: "memory.embedding".into(),
            },
            severity: Severity::Warning,
            remediation: "Set api_key for [memory.embedding] in config.toml or [providers.memory_embedding] in keys.toml".into(),
        });
    }

    // 7. Audit unrecognized / typo config keys
    if !raw_config_toml.trim().is_empty()
        && let Ok(ignored) = crate::config::sections::ignored_key_paths(raw_config_toml)
        && !ignored.is_empty()
    {
        problems.push(ConfigProblem {
            kind: ConfigProblemKind::IgnoredOrTypoConfigKeys(ignored.clone()),
            severity: Severity::Warning,
            remediation: format!(
                "Remove or correct unrecognized config keys in config.toml: {}",
                ignored.join(", ")
            ),
        });
    }

    // 8. Audit Telegram bot_owner
    if config.channels.telegram.enabled {
        let tg = &config.channels.telegram;
        let has_numeric_owner = tg.bot_owner.iter().any(|u| u.trim().parse::<i64>().is_ok());
        let has_numeric_allowed = tg
            .allowed_users
            .iter()
            .any(|u| u.trim().parse::<i64>().is_ok());
        if !has_numeric_owner && !has_numeric_allowed {
            problems.push(ConfigProblem {
                kind: ConfigProblemKind::NoValidBotOwnerConfigured,
                severity: Severity::Warning,
                remediation: "Set a numeric Telegram user ID under channels.telegram.bot_owner in config.toml".into(),
            });
        }
    }

    // 9. Audit Command & Skill catalog count limit (Telegram max 100 commands per scope)
    if config.channels.telegram.enabled {
        let skills = crate::brain::skills::load_all_skills();
        let brain_path = crate::brain::BrainLoader::resolve_path();
        let loader = crate::brain::CommandLoader::from_brain_path(&brain_path);
        let user_commands = loader.load();

        // Built-ins (21 base commands) + user commands + skills
        let total_count = 21 + user_commands.len() + skills.len();

        if total_count > 100 {
            problems.push(ConfigProblem {
                kind: ConfigProblemKind::CommandCatalogOverflow {
                    count: total_count,
                },
                severity: Severity::Warning,
                remediation: "Reduce the number of loaded skills or user commands to 100 or fewer to fit Telegram's menu limit.".into(),
            });
        }
    }

    problems
}

/// Format detected configuration problems into a rich Markdown Telegram alert.
pub fn format_config_alert(problems: &[ConfigProblem], profile_name: Option<&str>) -> String {
    let profile_tag = match profile_name {
        Some(p) if !p.is_empty() => format!(" (profile: {p})"),
        _ => String::new(),
    };

    let mut msg = format!("⚠️ **OpenCrabs Configuration Alert**{profile_tag}\n\n");
    msg.push_str("The following configuration issues were detected:\n\n");

    let mut truncated = 0;
    for (idx, prob) in problems.iter().enumerate() {
        let icon = match prob.severity {
            Severity::Error => "🛑",
            Severity::Warning => "⚠️",
        };
        let block = format!(
            "{icon} **{}**\n• *Details*: {}\n• *Fix*: `{}`\n\n",
            prob.kind.title(),
            prob.kind.details(),
            prob.remediation
        );

        if msg.len() + block.len() > 3800 {
            truncated = problems.len() - idx;
            break;
        }
        msg.push_str(&block);
    }

    if truncated > 0 {
        msg.push_str(&format!(
            "... and {truncated} additional issues (run `/doctor` for complete diagnostics).\n"
        ));
    } else {
        msg.push_str("💡 Run `/doctor` to view full runtime diagnostics.");
    }

    msg
}

/// Format recovery notification when all problems are resolved.
pub fn format_config_recovery(profile_name: Option<&str>) -> String {
    let profile_tag = match profile_name {
        Some(p) if !p.is_empty() => format!(" (profile: {p})"),
        _ => String::new(),
    };
    format!(
        "✅ **OpenCrabs Configuration Restored**{profile_tag}\n\nAll previously reported configuration issues have been resolved."
    )
}

/// Stateful deduplication tracker for proactive alerts.
#[derive(Default)]
pub struct ConfigAlertState {
    last_alert_hash: Mutex<Option<u64>>,
    last_alert_count: Mutex<usize>,
}

impl ConfigAlertState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Compute deterministic hash of problems list.
    pub fn compute_hash(problems: &[ConfigProblem]) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        let mut rep: Vec<String> = problems
            .iter()
            .map(|p| format!("{:?}:{:?}:{}", p.kind, p.severity, p.remediation))
            .collect();
        rep.sort();
        for r in rep {
            r.hash(&mut hasher);
        }
        hasher.finish()
    }

    /// Check whether an alert should be sent given the current problems list.
    pub fn should_alert(&self, problems: &[ConfigProblem]) -> bool {
        let guard = self.last_alert_hash.lock().unwrap();
        match *guard {
            Some(last) => {
                if problems.is_empty() {
                    true // recovery notice
                } else {
                    last != Self::compute_hash(problems)
                }
            }
            None => !problems.is_empty(),
        }
    }

    /// Record that an alert (or clearance) was sent.
    pub fn record_alert(&self, problems: &[ConfigProblem]) {
        let hash = if problems.is_empty() {
            None
        } else {
            Some(Self::compute_hash(problems))
        };
        if let Ok(mut g) = self.last_alert_hash.lock() {
            *g = hash;
        }
        if let Ok(mut g) = self.last_alert_count.lock() {
            *g = problems.len();
        }
    }

    /// Check if recovery state should be notified (was Some, now empty) and clear state.
    pub fn check_and_reset_recovery(&self) -> bool {
        let mut guard = match self.last_alert_hash.lock() {
            Ok(g) => g,
            Err(e) => e.into_inner(),
        };
        if guard.is_some() {
            *guard = None;
            if let Ok(mut c) = self.last_alert_count.lock() {
                *c = 0;
            }
            true
        } else {
            false
        }
    }
}

/// Resolve candidate Telegram recipient Chat IDs from config and state.
pub fn resolve_recipient_chat_ids(config: &Config, owner_chat_id: Option<i64>) -> Vec<i64> {
    let mut targets: Vec<i64> = Vec::new();
    let tg = &config.channels.telegram;

    for owner in &tg.bot_owner {
        if let Ok(id) = owner.trim().parse::<i64>()
            && !targets.contains(&id)
        {
            targets.push(id);
        }
    }

    if targets.is_empty() {
        for user in &tg.allowed_users {
            if let Ok(id) = user.trim().parse::<i64>()
                && !targets.contains(&id)
            {
                targets.push(id);
                break;
            }
        }
    }

    if let Some(id) = owner_chat_id
        && !targets.contains(&id)
    {
        targets.push(id);
    }

    targets
}

/// Run proactive config audit and dispatch DM alerts if new problems exist.
pub async fn run_proactive_config_audit(
    bot: &Bot,
    config: &Config,
    state: &super::state::TelegramState,
) {
    let raw_config = {
        let p = crate::config::opencrabs_home().join("config.toml");
        if p.exists() {
            std::fs::read_to_string(&p).ok().unwrap_or_default()
        } else {
            String::new()
        }
    };

    let raw_keys = {
        let keys_path = crate::config::keys_path();
        if keys_path.exists() {
            std::fs::read_to_string(&keys_path).ok()
        } else {
            None
        }
    };

    let problems = audit_config_problems(config, &raw_config, raw_keys.as_deref());
    let alert_state = state.config_alert_state();

    if !alert_state.should_alert(&problems) {
        return;
    }

    let owner_chat = state.owner_chat_id().await;
    let recipients = resolve_recipient_chat_ids(config, owner_chat);

    if recipients.is_empty() {
        tracing::warn!(
            "Telegram config alert: no valid numeric bot_owner or allowed_users ChatId found"
        );
        alert_state.record_alert(&problems);
        return;
    }

    let profile = crate::config::profile::active_profile().map(ToString::to_string);
    let is_recovery = problems.is_empty() && alert_state.check_and_reset_recovery();

    let msg = if is_recovery {
        format_config_recovery(profile.as_deref())
    } else if !problems.is_empty() {
        format_config_alert(&problems, profile.as_deref())
    } else {
        return;
    };

    let hash8 = format!("{:08x}", ConfigAlertState::compute_hash(&problems) as u32);

    for &chat_id in &recipients {
        match bot
            .send_message(teloxide::types::ChatId(chat_id), &msg)
            .parse_mode(teloxide::types::ParseMode::MarkdownV2)
            .await
        {
            Ok(m) => {
                super::telemetry::log_send_success(
                    "system",
                    "config_alert",
                    "-",
                    "owner_dm",
                    "send",
                    chat_id,
                    None,
                    m.id.0,
                    msg.len(),
                    &hash8,
                );
            }
            Err(_) => {
                // Fallback without parse_mode in case Markdown formatting had escaping quirks
                match bot
                    .send_message(teloxide::types::ChatId(chat_id), &msg)
                    .await
                {
                    Ok(m) => {
                        super::telemetry::log_send_success(
                            "system",
                            "config_alert",
                            "-",
                            "owner_dm",
                            "send_plain",
                            chat_id,
                            None,
                            m.id.0,
                            msg.len(),
                            &hash8,
                        );
                    }
                    Err(e) => {
                        super::telemetry::log_send_failure(
                            "system",
                            "config_alert",
                            "-",
                            "owner_dm",
                            "send",
                            chat_id,
                            None,
                            msg.len(),
                            &hash8,
                            &e.to_string(),
                        );
                    }
                }
            }
        }
    }

    alert_state.record_alert(&problems);
}
