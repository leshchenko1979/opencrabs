//! Token counting using tiktoken BPE encodings.
//!
//! Two encodings are available, chosen by model family (#1045):
//!
//! - `cl100k_base` — the default, and what every pre-#1045 call site uses. It
//!   is the GPT-4 / GPT-3.5-turbo encoding, and the closest publicly available
//!   one to most providers' internal tokenizers.
//! - `o200k_base` — the GPT-5 / GPT-4.1 / o-series encoding, and the one the
//!   Claude family is counted with. It merges compound identifiers that
//!   `cl100k_base` splits, which is the shape a Claude turn over code and JSON
//!   is full of. The measured shortfall `cl100k_base` shows against a Claude
//!   provider's own count is documented in
//!   `src/tests/context_provider_anchor_test.rs` (~660k estimated against
//!   ~1.03M reported on a 2.4MB request): a budget that undercounts fills
//!   late, so the soft gate never fires and the turn hits the provider's hard
//!   limit instead.
//!
//! Family-aware callers use [`count_tokens_for_model`] /
//! [`count_message_tokens_for_model`]; the family-blind [`count_tokens`] and
//! [`count_message_tokens`] are unchanged, so no existing caller silently
//! shifts its numbers. The Claude family test is
//! `claude_window::context_window_for_claude` — the same table the providers
//! resolve windows from, so "is this a Claude model?" has one answer.
//!
//! Both tokenizers are initialized lazily via `once_cell` and reused.

use once_cell::sync::Lazy;
use tiktoken_rs::CoreBPE;

/// Global tokenizer instance — initialized once, reused everywhere.
/// cl100k_base is used by GPT-4, GPT-3.5-turbo, and text-embedding-ada-002.
/// It's the closest publicly available tokenizer to what Anthropic uses.
static TOKENIZER: Lazy<CoreBPE> =
    Lazy::new(|| tiktoken_rs::cl100k_base().expect("Failed to initialize cl100k_base tokenizer"));

/// The o200k_base tokenizer, for model families it matches better (#1045).
/// Lazily built, so an install that never counts for one of those families
/// never pays for the second vocabulary.
static O200K_TOKENIZER: Lazy<CoreBPE> =
    Lazy::new(|| tiktoken_rs::o200k_base().expect("Failed to initialize o200k_base tokenizer"));

/// Count tokens in a string using cl100k_base BPE encoding.
///
/// This is the single source of truth for token estimation across the entire
/// codebase. No more chars/3, chars/4, or any other heuristic.
///
/// # Returns
/// Actual BPE token count (minimum 1 for non-empty strings, 0 for empty).
pub fn count_tokens(text: &str) -> usize {
    if text.is_empty() {
        return 0;
    }
    let tokens = TOKENIZER.encode_ordinary(text);
    tokens.len().max(1)
}

/// Count tokens for a message with structural overhead.
///
/// Each message has ~4 tokens of overhead for role tags and separators.
pub fn count_message_tokens(text: &str) -> usize {
    count_tokens(text) + 4
}

/// Whether this model's family is counted with `o200k_base` rather than
/// `cl100k_base`.
///
/// The Claude table is the family test on purpose: one predicate shared with
/// the provider window resolution, not a second hand-maintained model list
/// that can drift from it.
pub fn uses_o200k(model: &str) -> bool {
    crate::brain::provider::claude_window::context_window_for_claude(model).is_some()
}

/// Count tokens in a string with the encoding that fits `model` (#1045).
///
/// Claude models are counted with `o200k_base`, which tracks their tokenizer
/// far more closely on code and JSON than `cl100k_base` does; every other
/// model keeps `cl100k_base`, identical to [`count_tokens`].
///
/// # Returns
/// Actual BPE token count (minimum 1 for non-empty strings, 0 for empty).
pub fn count_tokens_for_model(text: &str, model: &str) -> usize {
    if text.is_empty() {
        return 0;
    }
    if !uses_o200k(model) {
        return count_tokens(text);
    }
    O200K_TOKENIZER.encode_ordinary(text).len().max(1)
}

/// Count tokens for a message with structural overhead, family-aware (#1045).
///
/// Mirrors [`count_message_tokens`]: the family-appropriate text count plus the
/// same ~4 tokens of role/separator overhead.
pub fn count_message_tokens_for_model(text: &str, model: &str) -> usize {
    count_tokens_for_model(text, model) + 4
}
