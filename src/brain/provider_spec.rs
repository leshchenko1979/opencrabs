//! Normalise an `[agent] *_provider` config value into the provider/model pair
//! it names (#1314, #1316).
//!
//! Every such key takes a provider name: the config section name, so
//! `[providers.custom.moonshotai]` is `"moonshotai"`. Users reasonably write
//! `"custom:moonshotai"` (the table path) or `"zhipu/glm-5.3"` (the pair they
//! see in `/models`), and a value taken verbatim then fails to build a
//! provider. This module recognises those spellings against the providers that
//! are actually configured and says what it corrected, naming the key it was
//! correcting, so the log teaches the canonical form.
//!
//! [`normalize`] is pure: the two predicates are injected so every branch is
//! testable without a `Config`. [`normalize_in`] binds them to a live config.

use crate::config::Config;

/// Which `[agent]` key pair a value came from, for the correction note.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProviderKey {
    pub provider: &'static str,
    pub model: &'static str,
}

impl ProviderKey {
    pub const SELF_IMPROVEMENT: Self = Self {
        provider: "self_improvement_provider",
        model: "self_improvement_model",
    };
    /// `[agent] default_provider` / `default_model` — the pair every session and
    /// every cron job inherits when nothing more specific is configured.
    pub const AGENT: Self = Self {
        provider: "default_provider",
        model: "default_model",
    };
    pub const SUBAGENT: Self = Self {
        provider: "subagent_provider",
        model: "subagent_model",
    };
    pub const PLAN: Self = Self {
        provider: "plan_provider",
        model: "plan_model",
    };
    pub const EXECUTE: Self = Self {
        provider: "execute_provider",
        model: "execute_model",
    };
    /// One entry of `[providers.fallback] providers`. A fallback provider
    /// runs on its own `default_model`; there is no per-entry model key.
    pub const FALLBACK_PROVIDERS: Self = Self {
        provider: "[providers.fallback] providers",
        model: "that provider's own default_model",
    };
    /// One entry of `[providers.fallback] vision`; the model is the
    /// provider's `vision_model`.
    pub const FALLBACK_VISION: Self = Self {
        provider: "[providers.fallback] vision",
        model: "that provider's own vision_model",
    };
}

/// The provider and optional model a key resolves to, after normalisation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderPair {
    pub provider: String,
    pub model: Option<String>,
    /// What was corrected, for a one-line warning; `None` when the value was
    /// already canonical.
    pub note: Option<String>,
}

/// Normalise `spec` (the raw `<key>_provider` value) with `model_key` (the raw
/// `<key>_model` value).
///
/// `is_custom` answers whether a name is a `[providers.custom.<name>]` key;
/// `is_declared` whether a name is any configured provider, custom or
/// built-in. Rules, in order:
///
/// 1. A `custom:` / `custom.` / `custom/` prefix is dropped.
/// 2. If what remains is `<a>/<b>` or `<a>:<b>` and `<a>` is a declared
///    provider, it is split: `<a>` is the provider and `<b>` the model. The
///    split happens only when the head is a known provider, because model ids
///    themselves contain both separators (`anthropic/claude-…`, `qwen:7b`).
/// 3. An explicit `<key>_model` wins over a model found by (2).
pub fn normalize(
    key: ProviderKey,
    spec: &str,
    model_key: Option<&str>,
    is_custom: impl Fn(&str) -> bool,
    is_declared: impl Fn(&str) -> bool,
) -> ProviderPair {
    let raw = spec.trim();
    let mut notes: Vec<String> = Vec::new();

    let mut name = raw;
    if let Some((prefix, rest)) = crate::config::strip_custom_prefix(name) {
        name = rest.trim();
        notes.push(format!(
            "dropped the '{prefix}' prefix: the provider name is the \
             [providers.custom.<name>] section name, so write \"{name}\""
        ));
    }

    let mut model_from_spec: Option<String> = None;
    if let Some(idx) = name.find(['/', ':']) {
        let (head, tail) = (name[..idx].trim(), name[idx + 1..].trim());
        if !head.is_empty() && !tail.is_empty() && (is_custom(head) || is_declared(head)) {
            notes.push(format!(
                "split \"{name}\" into provider \"{head}\" and model \"{tail}\": \
                 {} takes a provider name only; the model goes in {}",
                key.provider, key.model
            ));
            model_from_spec = Some(tail.to_string());
            name = head;
        }
    }

    let model = match (
        model_key.map(str::trim).filter(|m| !m.is_empty()),
        model_from_spec,
    ) {
        (Some(explicit), Some(found)) if explicit != found => {
            notes.push(format!(
                "{} = \"{explicit}\" wins over the \"{found}\" found in {}",
                key.model, key.provider
            ));
            Some(explicit.to_string())
        }
        (Some(explicit), _) => Some(explicit.to_string()),
        (None, found) => found,
    };

    ProviderPair {
        provider: name.to_string(),
        model,
        note: (!notes.is_empty()).then(|| notes.join("; ")),
    }
}

/// [`normalize`] against the providers `config` declares.
pub fn normalize_in(
    config: &Config,
    key: ProviderKey,
    spec: &str,
    model_key: Option<&str>,
) -> ProviderPair {
    let mut pair = normalize(
        key,
        spec,
        model_key,
        |name| {
            config
                .providers
                .custom
                .as_ref()
                .is_some_and(|m| m.contains_key(name))
        },
        |name| config.providers.is_declared(name),
    );
    pair.provider = canonical_provider_name(config, &pair.provider);
    pair
}

/// The name a resolved provider actually has: when `name` (in any accepted
/// spelling) names a `[providers.custom.<name>]` entry, the key that entry is
/// stored under; otherwise `name` unchanged.
///
/// The custom map is keyed by `normalize_toml_key`, so
/// `default_provider = "custom.llm_gateway"` against
/// `[providers.custom.llm-gateway]` resolves through `custom_provider_key` to
/// an entry stored as `llm-gateway`. Returning the user's spelling instead
/// would construct the provider — and stamp the session row — under a name no
/// config section carries, leaving the two to meet only through the same
/// fallback lookup.
pub fn canonical_provider_name(config: &Config, name: &str) -> String {
    let canonical = crate::config::custom_provider_key(name);
    if config
        .providers
        .custom
        .as_ref()
        .is_some_and(|m| m.contains_key(&canonical))
    {
        canonical
    } else {
        name.to_string()
    }
}
