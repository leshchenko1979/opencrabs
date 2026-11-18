//! Embedding API key resolution from keys.toml (#1066).
//!
//! `EmbeddingConfig::api_key` documents that it is "also loaded from keys.toml
//! under `[providers.memory_embedding]`". The `[memory]` readers in
//! [`super::settings`] parse config.toml directly and never pass through
//! `Config::load()` where `merge_provider_keys` runs, so the fallback lives
//! here, at the one place that consumes it.

use url::{Host, Url};

/// Whether config.toml left the embedding key for keys.toml to supply.
///
/// A whitespace-only value counts as absent: it reaches the provider as an
/// empty bearer token and 401s exactly like no key at all, so treating it as
/// "configured" would keep the keys.toml fallback locked out for the one user
/// who most needs it.
pub(crate) fn needs_embedding_key(configured: Option<&str>) -> bool {
    configured.is_none_or(|k| k.trim().is_empty())
}

/// Whether this embedding endpoint actually REQUIRES a credential.
///
/// A loopback / `localhost` endpoint is a local daemon (llama.cpp, Ollama,
/// LM Studio): it serves without auth, so an absent key is not a defect.
/// Distinct from [`needs_embedding_key`], which asks whether a usable key
/// exists, not whether one is required.
///
/// An absent or unparseable url keeps the old behaviour (`true`): the
/// endpoint is unknown, so a missing credential is still worth reporting.
pub(crate) fn embedding_key_required(url: Option<&str>) -> bool {
    let Some(raw) = url else {
        return true;
    };
    let Ok(parsed) = Url::parse(raw) else {
        return true;
    };
    match parsed.host() {
        // `Url::parse` lowercases the host, but the comparison is explicit so
        // the intent survives a reader who does not know that.
        Some(Host::Domain(host)) => !host.eq_ignore_ascii_case("localhost"),
        Some(Host::Ipv4(ip)) => !ip.is_loopback(),
        Some(Host::Ipv6(ip)) => !ip.is_loopback(),
        // No host at all (`file:`, `unix:`) cannot be a remote API endpoint,
        // and today's behaviour is to ask for the key anyway.
        None => true,
    }
}

/// `[providers.memory_embedding].api_key` from keys.toml, when it holds a real
/// credential. Mirrors the `__EXISTING_KEY__` sentinel guard that
/// `merge_provider_keys` applies, so the placeholder `/models` writes
/// internally is never mistaken for a key.
pub(super) fn memory_embedding_key_from_keys_file() -> Option<String> {
    let keys_path = crate::config::opencrabs_home().join("keys.toml");
    let content = std::fs::read_to_string(&keys_path).ok()?;
    memory_embedding_key_in(&content)
}

/// The parsing half of the above, split out so the contract can be tested
/// without a keys.toml on disk.
///
/// The section is read as a raw toml table rather than through
/// `ProviderConfigs`, which has no `memory_embedding` field: adding one would
/// not help, because this module parses config.toml directly and never passes
/// through `Config::load()` where `merge_provider_keys` runs.
pub(crate) fn memory_embedding_key_in(keys_toml: &str) -> Option<String> {
    let table = keys_toml.parse::<toml::Table>().ok()?;
    let key = table
        .get("providers")?
        .get("memory_embedding")?
        .get("api_key")?
        .as_str()?;
    crate::config::stored_key::real_key(key).map(str::to_string)
}
