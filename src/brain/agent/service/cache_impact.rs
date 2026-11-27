//! Prompt-cache invalidation estimates for compaction actions (#1025).
//!
//! Providers cache the **prefix** of a request. Every compaction action that
//! rewrites a message at index `i` keeps the cache for `messages[0..i]` and
//! forces a re-read of everything from `i` on. #1000 tracks the account-wide
//! hit rate at the gateway; this module answers the per-action question the
//! `compaction_event` line could not: *how much of the next request did this
//! action just un-cache?*
//!
//! The estimate is deliberately local and cheap — the same `tiktoken`
//! estimator the budget already uses, summed over the message vector. It is
//! an ESTIMATE and is logged as one; the authoritative figure arrives later
//! from the provider's `cached_tokens` usage field and is compared against it
//! in a `cache_outcome` record.
//!
//! Scope is the message vector only. System prompt, brain files and tool
//! schemas are static for the life of a session and enter as
//! `static_overhead`, which is therefore *kept* on every action — they are
//! never the reason a cache breaks.
//!
//! Two mutation classes the issue's table lists are deliberately absent:
//!
//! * **Thinking drop** — [`super::strip_thinking_for_summariser`]
//!   rewrites the *summariser's input*, never the live context. The replayed
//!   prompt still carries every `Thinking` block, so the next request's prefix
//!   is untouched and there is nothing to estimate. Charging a cost here would
//!   invent one.
//! * **Emergency head cut** — it swaps the whole window for a marker, which is
//!   [`CacheMutation::FullApply`] exactly: the leading message changes, so the
//!   break is at 0 and a separate variant would only duplicate it.

use crate::brain::agent::context::{AgentContext, CompactionScope};
use crate::brain::provider::Message;

/// Where the prompt stops matching what the provider cached.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CacheMutation {
    /// Tier-0 offload stubbed the tool result at this message index.
    ToolOffload { first_offloaded: usize },
    /// A delta summary was appended; the frozen prefix is `snapshot_len` long.
    DeltaApply { snapshot_len: usize },
    /// The whole window was replaced by one summary — nothing survives.
    FullApply,
    /// Segment consolidation replaced the markers starting at this index.
    Consolidate { first_merged: usize },
}

impl CacheMutation {
    /// Earliest message index whose serialized body changes.
    ///
    /// `0` means the very first message changes, i.e. the entire message
    /// vector is re-read; only the static prefix survives.
    pub(crate) const fn break_at_seq(self) -> usize {
        match self {
            Self::ToolOffload { first_offloaded } => first_offloaded,
            Self::DeltaApply { snapshot_len } => snapshot_len,
            Self::FullApply => 0,
            Self::Consolidate { first_merged } => first_merged,
        }
    }

    /// Short stable label for the log line.
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::ToolOffload { .. } => "tool_offload",
            Self::DeltaApply { .. } => "delta_apply",
            Self::FullApply => "full_apply",
            Self::Consolidate { .. } => "consolidate",
        }
    }
}

/// What one action costs the next request's cache.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct CacheImpact {
    /// Earliest message index whose body changed.
    pub break_at_seq: usize,
    /// Static prefix + `messages[0..break_at_seq]` — the part the provider
    /// can still serve from cache.
    pub prefix_kept_tokens: usize,
    /// `messages[break_at_seq..]` — the part that must be re-read.
    pub invalidated_tokens: usize,
    /// `invalidated / (kept + invalidated) × 100`.
    pub invalidated_pct: f64,
}

impl CacheImpact {
    /// Total request size the estimate covers (message vector + static).
    pub(crate) const fn total_tokens(&self) -> usize {
        self.prefix_kept_tokens + self.invalidated_tokens
    }
}

/// Estimate the cache cost of `mutation` against the message vector the NEXT
/// request will carry — i.e. call this AFTER the mutation was applied.
///
/// The `kept` half is unaffected by the timing (nothing before
/// `break_at_seq` was touched); reading it post-apply is what makes
/// `invalidated` describe the prompt that will actually be sent, rather than
/// the one that just left.
///
/// `model` is the encoding the window itself is counted in (#1063). The hint
/// this produces is compared against what the provider reports it re-read, so
/// measuring it in a different vocabulary than the budget would make the two
/// disagree for reasons that have nothing to do with the cache.
pub(crate) fn estimate(
    messages: &[Message],
    mutation: CacheMutation,
    static_overhead: usize,
    model: &str,
) -> CacheImpact {
    // A break index past the end is the same as "nothing survives".
    let break_at = mutation.break_at_seq().min(messages.len());

    let kept_body: usize = messages[..break_at]
        .iter()
        .map(|m| AgentContext::estimate_tokens_static_for(m, model))
        .sum();
    let invalidated: usize = messages[break_at..]
        .iter()
        .map(|m| AgentContext::estimate_tokens_static_for(m, model))
        .sum();

    let prefix_kept_tokens = static_overhead.saturating_add(kept_body);
    let total = prefix_kept_tokens.saturating_add(invalidated);
    let invalidated_pct = if total > 0 {
        (invalidated as f64 / total as f64) * 100.0
    } else {
        0.0
    };

    CacheImpact {
        break_at_seq: break_at,
        prefix_kept_tokens,
        invalidated_tokens: invalidated,
        invalidated_pct,
    }
}

/// The estimate one mutating action left for the NEXT provider response to be
/// checked against (#1025).
///
/// Kept as its own type rather than reusing [`CacheImpact`]: this one is
/// stored per session between two turns, so it must be `Copy`, cheap and
/// self-describing (the label survives the action that produced it — by the
/// time the response lands, the caller no longer knows which action ran).
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct CacheEstimate {
    /// `tool_offload`, `delta_apply`, `full_apply`, `consolidate`.
    pub mutation: &'static str,
    /// `messages[break_at_seq..]` at the instant the action landed.
    pub estimated_invalidated_tokens: usize,
    /// `prefix_kept + invalidated` at that same instant — the request size the
    /// provider's `prompt_tokens` should land near.
    pub estimated_prompt_tokens: usize,
}

impl CacheEstimate {
    pub(crate) const fn from_impact(mutation: CacheMutation, impact: CacheImpact) -> Self {
        Self {
            mutation: mutation.label(),
            estimated_invalidated_tokens: impact.invalidated_tokens,
            estimated_prompt_tokens: impact.total_tokens(),
        }
    }
}

/// The mutation an APPLY just performed, read off the context it left behind.
///
/// Taken post-apply on purpose: the scope names the policy, but only the
/// resulting vector says WHERE the rewrite starts.
///
/// * `FullWindow` swaps the whole vector for one marker — break at 0.
/// * `DeltaSinceMarker` truncates at the previous marker and pushes a new one,
///   so the newly appended marker (the LAST one) is the first changed message;
///   every frozen segment above it survives verbatim.
/// * `SegmentConsolidation` truncates at the FIRST marker, so that index is
///   the break.
/// * A truncated pass appends a notice without dropping anything; it lands in
///   the delta case for free, because the notice is a new message and the
///   whole frozen prefix above it is byte-identical to what was cached.
pub(crate) fn mutation_after_apply(
    scope: CompactionScope,
    context: &AgentContext,
) -> CacheMutation {
    match scope {
        CompactionScope::FullWindow => CacheMutation::FullApply,
        CompactionScope::DeltaSinceMarker => CacheMutation::DeltaApply {
            snapshot_len: context.last_marker_index().unwrap_or(0),
        },
        CompactionScope::SegmentConsolidation => CacheMutation::Consolidate {
            first_merged: context.first_marker_index().unwrap_or(0),
        },
    }
}

/// `42_000` → `42k`, `1_200_000` → `1.2M`.
///
/// Lowercase on purpose: this rides a Telegram subline whose other figures are
/// printed by `format_token_count`, and a `42K`/`68k` mix in one line reads as
/// two different units.
fn short_tokens(tokens: usize) -> String {
    let t = tokens as f64;
    if t >= 1_000_000.0 {
        format!("{:.1}M", t / 1_000_000.0)
    } else if t >= 1_000.0 {
        format!("{:.0}k", t / 1_000.0)
    } else {
        format!("{tokens}")
    }
}

/// One-line hint appended to the `🧹 compact:` / `📎 offloaded:` /
/// `🧹 idle merge:` notices: `cache: ~42k of 68k re-read (62%)`.
///
/// The `~` is load-bearing — the figure is a local estimate, not a provider
/// count, and the notice must not read as a measurement it did not make.
pub(crate) fn format_hint(impact: &CacheImpact) -> String {
    format!(
        "cache: ~{} of {} re-read ({:.0}%)",
        short_tokens(impact.invalidated_tokens),
        short_tokens(impact.total_tokens()),
        impact.invalidated_pct
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::brain::provider::{ContentBlock, Role};

    fn msg(role: Role, text: &str) -> Message {
        Message {
            role,
            content: vec![ContentBlock::Text {
                text: text.to_string(),
            }],
        }
    }

    /// Every action's break index is the earliest index its own body touches.
    #[test]
    fn break_index_follows_the_action_that_caused_it() {
        assert_eq!(
            CacheMutation::ToolOffload { first_offloaded: 7 }.break_at_seq(),
            7
        );
        assert_eq!(
            CacheMutation::DeltaApply { snapshot_len: 12 }.break_at_seq(),
            12
        );
        assert_eq!(CacheMutation::FullApply.break_at_seq(), 0);
        assert_eq!(
            CacheMutation::Consolidate { first_merged: 3 }.break_at_seq(),
            3
        );
    }

    /// A break at 0 keeps the static prefix and nothing else.
    #[test]
    fn a_full_apply_keeps_only_the_static_prefix() {
        let messages = vec![msg(Role::User, "hello world"), msg(Role::Assistant, "hi")];
        let impact = estimate(&messages, CacheMutation::FullApply, 10_000, "");

        assert_eq!(impact.break_at_seq, 0);
        assert_eq!(impact.prefix_kept_tokens, 10_000);
        assert_eq!(
            impact.invalidated_tokens,
            messages
                .iter()
                .map(AgentContext::estimate_tokens_static)
                .sum::<usize>()
        );
        assert!(impact.invalidated_pct > 0.0 && impact.invalidated_pct < 100.0);
    }

    /// A break past the end invalidates nothing and keeps everything.
    #[test]
    fn a_break_past_the_end_invalidates_nothing() {
        let messages = vec![msg(Role::User, "hello")];
        let impact = estimate(
            &messages,
            CacheMutation::DeltaApply { snapshot_len: 99 },
            5_000,
            "",
        );

        assert_eq!(impact.break_at_seq, 1);
        assert_eq!(impact.invalidated_tokens, 0);
        assert_eq!(impact.invalidated_pct, 0.0);
        assert_eq!(
            impact.prefix_kept_tokens,
            5_000 + AgentContext::estimate_tokens_static(&messages[0])
        );
    }

    /// An empty context must not divide by zero.
    #[test]
    fn an_empty_context_yields_zero_pct() {
        let impact = estimate(&[], CacheMutation::FullApply, 0, "");
        assert_eq!(impact.total_tokens(), 0);
        assert_eq!(impact.invalidated_pct, 0.0);
    }

    /// The two halves are measured against the SAME vector, so they always add
    /// up to the whole request — the checksum the log line's reader relies on.
    #[test]
    fn kept_plus_invalidated_is_the_whole_request() {
        let messages = vec![
            msg(Role::User, "a".repeat(400).as_str()),
            msg(Role::Assistant, "b".repeat(400).as_str()),
            msg(Role::User, "c".repeat(400).as_str()),
        ];
        let impact = estimate(
            &messages,
            CacheMutation::ToolOffload { first_offloaded: 2 },
            1_000,
            "",
        );
        let whole = 1_000
            + messages
                .iter()
                .map(AgentContext::estimate_tokens_static)
                .sum::<usize>();

        assert_eq!(impact.total_tokens(), whole);
    }

    /// The hint carries the estimate marker and both halves.
    #[test]
    fn the_hint_names_the_re_read_share() {
        let impact = CacheImpact {
            break_at_seq: 4,
            prefix_kept_tokens: 26_000,
            invalidated_tokens: 42_000,
            invalidated_pct: 61.76,
        };
        assert_eq!(format_hint(&impact), "cache: ~42k of 68k re-read (62%)");
    }
}
