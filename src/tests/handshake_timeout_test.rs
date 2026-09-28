//! Regression tests for `handshake_timeout_for`.
//!
//! Locks in the 2026-04-25 fix where NVIDIA's wedged
//! `integrate.api.nvidia.com` ate ~3 minutes (3 × 90s) of retry budget
//! before falling back. Cloud HTTP now bails after 60s (bumped from 30s
//! on 2026-05-07 because routing proxies like dialagram legitimately
//! need 20-45s when upstream is slow), while local HTTP keeps the longer
//! 90s window for cold-loading models.

use crate::brain::agent::service::helpers::{
    handshake_timeout_for, provider_handshake_timeout_for,
};
use std::time::Duration;

#[test]
fn cli_providers_get_ten_minutes() {
    // Subprocess startup + auth refresh dominate; the URL — if any —
    // doesn't matter.
    assert_eq!(handshake_timeout_for(true, None), Duration::from_secs(600));
    assert_eq!(
        handshake_timeout_for(true, Some("http://localhost:1234/v1/chat/completions")),
        Duration::from_secs(600),
    );
    assert_eq!(
        handshake_timeout_for(true, Some("https://api.openai.com/v1/chat/completions")),
        Duration::from_secs(600),
    );
}

#[test]
fn local_http_gets_ninety_seconds() {
    // Cold-loading a large gguf or MLX checkpoint can take 30+ seconds.
    for url in [
        "http://localhost:1234/v1/chat/completions",
        "http://127.0.0.1:8080/v1/chat/completions",
        "http://[::1]:11434/api/chat",
        "http://0.0.0.0:8000/v1/chat/completions",
        "http://192.168.1.42:11434/api/chat",
        "http://10.0.0.5:8080/v1",
        "http://172.20.0.10:8000/v1",
        "http://my-rig.local:1234/v1/chat/completions",
    ] {
        assert_eq!(
            handshake_timeout_for(false, Some(url)),
            Duration::from_secs(90),
            "expected 90s for local URL: {}",
            url,
        );
    }
}

#[test]
fn cloud_http_gets_sixty_seconds() {
    // Routing proxies (dialagram, openrouter) can take 20-45s when
    // upstream is slow. Wedged servers are >120s, so 60s still catches
    // real hangs in under 3 min via the retry chain. Previous 30s killed
    // legitimate requests to slower-but-healthy providers.
    for url in [
        "https://integrate.api.nvidia.com/v1/chat/completions",
        "https://api.openai.com/v1/chat/completions",
        "https://api.anthropic.com/v1/messages",
        "https://api.z.ai/api/coding/paas/v4/chat/completions",
        "https://opencode.ai/zen/go/v1/chat/completions",
        "https://openrouter.ai/api/v1/chat/completions",
        "https://api.minimax.io/v1/chat/completions",
        "https://api.moonshot.ai/v1/chat/completions",
    ] {
        assert_eq!(
            handshake_timeout_for(false, Some(url)),
            Duration::from_secs(60),
            "expected 60s for cloud URL: {}",
            url,
        );
    }
}

#[test]
fn missing_base_url_defaults_to_cloud_timeout() {
    // Providers without a base_url (built-in Anthropic/Gemini that
    // hardcode their endpoints internally) are always cloud — they
    // can't be a local LM server.
    assert_eq!(handshake_timeout_for(false, None), Duration::from_secs(60));
}

// ---------------------------------------------------------------------------
// The PER-SEND budget (#680/#682).
//
// `handshake_timeout_for` is the class table. `provider_handshake_timeout_for`
// is what each HTTP provider applies at its own `.send()`, and it must NOT
// return a budget for a CLI provider: those spawn a subprocess rather than
// sending HTTP, so a send budget is meaningless and the caller keeps its
// process-startup wall at the class table instead.
// ---------------------------------------------------------------------------

#[test]
fn cli_providers_get_no_per_send_budget() {
    // None == "there is no HTTP send to bound". A value here would both be
    // meaningless and silently drop the 600s process-startup wall the caller
    // applies only while this is None.
    assert_eq!(provider_handshake_timeout_for(true, None), None);
    assert_eq!(
        provider_handshake_timeout_for(true, Some("https://api.openai.com/v1/chat/completions")),
        None,
    );
}

#[test]
fn http_providers_get_the_class_budget_per_send() {
    assert_eq!(
        provider_handshake_timeout_for(false, Some("https://api.openai.com/v1/chat/completions")),
        Some(Duration::from_secs(60)),
    );
    assert_eq!(
        provider_handshake_timeout_for(false, Some("http://127.0.0.1:8080/v1/chat/completions")),
        Some(Duration::from_secs(90)),
    );
    // No base_url: the built-in Anthropic/Gemini providers hardcode cloud
    // endpoints, so they take the cloud budget.
    assert_eq!(
        provider_handshake_timeout_for(false, None),
        Some(Duration::from_secs(60)),
    );
}
