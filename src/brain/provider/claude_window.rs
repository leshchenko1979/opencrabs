//! The Claude context-window table, in one place (#1045).
//!
//! Before this module the table lived inside the `anthropic` provider, so only
//! a model reached through the native Anthropic API could resolve its window.
//! The same model reached through a custom OpenAI-compatible gateway matched no
//! family arm in that provider at all and fell through to the agent default —
//! a 200K budget for a 1M model — while the `claude` CLI provider answered a
//! flat 200K for every model it could drive.
//!
//! The three providers now delegate here, and so does the tokenizer's family
//! test, so one model id resolves one window wherever it is served from and
//! "is this a Claude model?" has a single answer rather than a second,
//! hand-maintained list.

/// The context window for a Claude model id, when this tree knows one.
///
/// Single source of truth: the `anthropic` provider, the `claude` CLI provider
/// and the OpenAI-compatible gateway all delegate here, so a model reached
/// through a gateway resolves the same window as the same model reached
/// natively. `None` means "not a Claude model this tree recognises" — the
/// caller keeps its own fallback.
pub fn context_window_for_claude(model: &str) -> Option<u32> {
    window_for_bare_id(model).or_else(|| bare_model_id(model).and_then(window_for_bare_id))
}

/// A gateway may spell a model id with a routing prefix
/// (`anthropic/claude-...`, `openrouter/anthropic/claude-...`). The table is
/// keyed on the bare id, so the segment after the last `/` is tried when the
/// whole string misses. The table itself stays exact-match — this adds no
/// heuristic of its own, it only strips routing the gateway prepends.
fn bare_model_id(model: &str) -> Option<&str> {
    model.rsplit_once('/').map(|(_, tail)| tail)
}

/// The table itself: exact ids only, no heuristics.
fn window_for_bare_id(model: &str) -> Option<u32> {
    match model {
        // 1M-context models (Fable 5, Opus 4.6+, Sonnet 4.6).
        "claude-fable-5" => Some(1_000_000),
        "claude-opus-4-8" => Some(1_000_000),
        "claude-opus-4-7" => Some(1_000_000),
        "claude-opus-4-6" => Some(1_000_000),
        "claude-sonnet-4-6" => Some(1_000_000),
        "claude-sonnet-4-5-20250929" => Some(200_000),
        "claude-haiku-4-5-20251001" => Some(200_000),
        "claude-3-5-sonnet-20241022" => Some(200_000),
        "claude-3-5-haiku-20241022" => Some(200_000),
        "claude-3-opus-20240229" => Some(200_000),
        "claude-3-sonnet-20240229" => Some(200_000),
        "claude-3-5-sonnet-20240620" => Some(200_000),
        "claude-3-haiku-20240307" => Some(200_000),
        _ => None,
    }
}
