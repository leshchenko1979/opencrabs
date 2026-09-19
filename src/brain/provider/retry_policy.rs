//! Retry-policy resolution for OpenAI-compatible providers (#346).
//!
//! Before #346 the retry engine's knobs were hard-wired: `retry_config()`
//! returned a family preset for qwen / OpenRouter / `:free` models and
//! [`RetryConfig::default`] for everything else, with no way for an operator
//! to raise the attempts on a flaky gateway. The plumbing to override it
//! (`with_retry_config`) existed but had no production caller.
//!
//! This module owns the resolution, in ONE place, so the provider struct and
//! the factory cannot drift apart:
//!
//! 1. per-provider `retry_*` keys on the provider's own config section,
//! 2. the global `[retry]` block,
//! 3. the built-in family preset ([`family_preset`]),
//! 4. [`RetryConfig::default`].
//!
//! Layers 1 and 2 are merged once at provider-build time into
//! [`RetryOverrides`]. Layer 3 depends on the *request* — `:free` is a model
//! suffix — so it is computed per call and the overrides are layered on top
//! field by field. That ordering is what keeps the family preset a default
//! and never a lock: an operator who raises only `max_attempts` still keeps
//! the preset's tuned delays.

use std::time::Duration;

use crate::config::{ProviderConfig, RetrySection};
use crate::utils::retry::RetryConfig;

/// Bounded in-place attempts granted to a HARD quota / billing error when the
/// provider opted in with `retry_quota_exhausted = true` (#346).
///
/// Deliberately small. The aggregator case is "one upstream key is capped and
/// another may answer in seconds" — not "wait out a billing window".
pub const QUOTA_RETRY_ATTEMPTS: u32 = 2;

/// Delay between quota retries. Short for the same reason: the point is to
/// let the aggregator re-route, not to back off exponentially.
pub const QUOTA_RETRY_DELAY: Duration = Duration::from_secs(2);

/// Operator-supplied retry values, merged from the provider's own section and
/// the global `[retry]` block.
///
/// Every numeric field stays an `Option` on purpose: resolution is
/// field-by-field, so a key the operator did not set falls through to the
/// family preset rather than to a hard-coded zero.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RetryOverrides {
    pub max_attempts: Option<u32>,
    pub initial_delay_secs: Option<f64>,
    pub max_delay_secs: Option<f64>,
    pub backoff_multiplier: Option<f64>,
    pub jitter: Option<f64>,
    /// Aggregator opt-in (#346). `true` grants a HARD quota error a bounded
    /// number of in-place attempts ([`QUOTA_RETRY_ATTEMPTS`]) before the
    /// request rolls to the fallback chain.
    pub quota_exhausted: bool,
}

impl RetryOverrides {
    /// Merge the provider's own section over the global `[retry]` block.
    /// The per-provider key wins wherever both are set.
    pub fn from_config(provider: &ProviderConfig, global: &RetrySection) -> Self {
        Self {
            max_attempts: provider.retry_max_attempts.or(global.max_attempts),
            initial_delay_secs: provider
                .retry_initial_delay_secs
                .or(global.initial_delay_secs),
            max_delay_secs: provider.retry_max_delay_secs.or(global.max_delay_secs),
            backoff_multiplier: provider
                .retry_backoff_multiplier
                .or(global.backoff_multiplier),
            jitter: provider.retry_jitter.or(global.jitter),
            quota_exhausted: provider.retry_quota_exhausted.unwrap_or(false),
        }
    }

    /// True when nothing at all was configured, i.e. the historical behaviour
    /// is already in force and no override needs installing.
    pub fn is_empty(&self) -> bool {
        self.max_attempts.is_none()
            && self.initial_delay_secs.is_none()
            && self.max_delay_secs.is_none()
            && self.backoff_multiplier.is_none()
            && self.jitter.is_none()
            && !self.quota_exhausted
    }

    /// Layer these values onto `base`, field by field. Unset keys leave
    /// `base` untouched.
    pub fn apply_to(&self, base: &mut RetryConfig) {
        if let Some(v) = self.max_attempts {
            base.max_attempts = v;
        }
        if let Some(v) = self.initial_delay_secs {
            base.initial_delay = secs(v);
        }
        if let Some(v) = self.max_delay_secs {
            base.max_delay = secs(v);
        }
        if let Some(v) = self.backoff_multiplier {
            base.backoff_multiplier = v;
        }
        if let Some(v) = self.jitter {
            base.jitter = v;
        }
    }
}

/// Seconds → [`Duration`], guarding against a negative or non-finite value
/// (a hand-edited `config.toml` can carry `-1.0`) becoming a panic.
fn secs(v: f64) -> Duration {
    if !v.is_finite() || v <= 0.0 {
        Duration::ZERO
    } else {
        Duration::from_secs_f64(v)
    }
}

/// The built-in retry preset for a (provider name, base URL, model) triple.
///
/// qwen / OpenRouter / `:free` models share tight per-minute windows that
/// reopen within seconds, so they retry in place with a 3s initial delay
/// instead of bouncing to the fallback chain on the first 429. Everything
/// else gets [`RetryConfig::default`].
pub fn family_preset(name: &str, base_url: &str, model: &str) -> RetryConfig {
    let openrouter = base_url.to_lowercase().contains("openrouter");
    if name == "qwen" || openrouter || model.ends_with(":free") {
        RetryConfig::qwen_cli_match()
    } else {
        RetryConfig::default()
    }
}

/// Full resolution for one request: family preset, then the operator's
/// overrides on top.
pub fn resolve(
    name: &str,
    base_url: &str,
    model: &str,
    overrides: &RetryOverrides,
) -> RetryConfig {
    let mut config = family_preset(name, base_url, model);
    overrides.apply_to(&mut config);
    config
}
