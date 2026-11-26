//! Prompt cache invalidation estimates for compaction actions (#1025).
//!
//! Providers cache the serialized prompt prefix. When compaction mutates the
//! live window, everything from the earliest changed message index onward is
//! treated as uncached on the next request. Estimates use the existing local
//! tokenizer — cheap, no provider calls.

use crate::brain::agent::context::{AgentContext, CompactionScope};
use crate::brain::provider::Message;

/// Estimated cache impact of one prompt mutation.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub(crate) struct PromptCacheBreakEstimate {
    pub break_at_seq: usize,
    pub prefix_kept_tokens: usize,
    pub invalidated_tokens: usize,
    pub invalidated_pct: f64,
    pub total_prompt_tokens: usize,
}

impl PromptCacheBreakEstimate {
    #[allow(dead_code)]
    pub(crate) fn with_tokens_freed(self, tokens_freed: usize) -> CompactionCacheFields {
        CompactionCacheFields {
            break_at_seq: self.break_at_seq,
            prefix_kept_tokens: self.prefix_kept_tokens,
            invalidated_tokens: self.invalidated_tokens,
            invalidated_pct: self.invalidated_pct,
            tokens_freed: Some(tokens_freed),
        }
    }

    #[allow(dead_code)]
    pub(crate) fn into_fields(self) -> CompactionCacheFields {
        CompactionCacheFields {
            break_at_seq: self.break_at_seq,
            prefix_kept_tokens: self.prefix_kept_tokens,
            invalidated_tokens: self.invalidated_tokens,
            invalidated_pct: self.invalidated_pct,
            tokens_freed: None,
        }
    }
}

/// Subset wired into structured logs and channel notices.
#[derive(Debug, Clone, Copy, Default)]
#[allow(dead_code)]
pub(crate) struct CompactionCacheFields {
    pub break_at_seq: usize,
    pub prefix_kept_tokens: usize,
    pub invalidated_tokens: usize,
    pub invalidated_pct: f64,
    pub tokens_freed: Option<usize>,
}

/// Earliest message index whose bytes change for this compaction apply.
pub(crate) fn cache_break_at_for_compaction(
    context: &AgentContext,
    scope: CompactionScope,
    snapshot_len: usize,
    consolidation_prefix: Option<usize>,
) -> usize {
    match scope {
        CompactionScope::FullWindow => 0,
        CompactionScope::DeltaSinceMarker => snapshot_len.min(context.messages.len()),
        CompactionScope::SegmentConsolidation => {
            let markers = context.marker_indices();
            if markers.is_empty() {
                return 0;
            }
            let merge = consolidation_prefix
                .unwrap_or_else(|| AgentContext::consolidation_prefix_count(markers.len()));
            let idx = merge.saturating_sub(1);
            markers.get(idx).copied().unwrap_or(0)
        }
    }
}

/// Head-cut and synchronous full-window paths replace from the leading edge.
pub(crate) const fn cache_break_at_head_cut() -> usize {
    0
}

pub(crate) fn estimate_prompt_cache_break(
    messages: &[Message],
    break_at_seq: usize,
    static_prefix_tokens: usize,
) -> PromptCacheBreakEstimate {
    let break_at = break_at_seq.min(messages.len());
    let prefix_kept_messages: usize = messages
        .iter()
        .take(break_at)
        .map(AgentContext::estimate_tokens_static)
        .sum();
    let invalidated: usize = messages
        .iter()
        .skip(break_at)
        .map(AgentContext::estimate_tokens_static)
        .sum();
    let prefix_kept_tokens = static_prefix_tokens.saturating_add(prefix_kept_messages);
    let total_prompt_tokens = prefix_kept_tokens.saturating_add(invalidated);
    let invalidated_pct = if total_prompt_tokens > 0 {
        (invalidated as f64 / total_prompt_tokens as f64) * 100.0
    } else {
        0.0
    };
    PromptCacheBreakEstimate {
        break_at_seq: break_at,
        prefix_kept_tokens,
        invalidated_tokens: invalidated,
        invalidated_pct,
        total_prompt_tokens,
    }
}

pub(crate) fn estimate_from_context(
    context: &AgentContext,
    static_prefix_tokens: usize,
    break_at_seq: usize,
) -> PromptCacheBreakEstimate {
    estimate_prompt_cache_break(&context.messages, break_at_seq, static_prefix_tokens)
}

/// Compact Telegram / log suffix, e.g. `cache: ~42k of 68k re-read (62%)`.
pub(crate) fn format_cache_notice_line(est: &PromptCacheBreakEstimate) -> String {
    let tok = |n: usize| crate::utils::format_token_count(n.min(u32::MAX as usize) as u32);
    format!(
        "cache: ~{} of {} re-read ({:.0}%)",
        tok(est.invalidated_tokens),
        tok(est.total_prompt_tokens),
        est.invalidated_pct,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::brain::provider::{ContentBlock, Message, Role};

    fn u_msg(n: usize, repeat: usize) -> Message {
        Message::user(format!("m{n} {}", "x".repeat(repeat)))
    }

    #[test]
    fn full_window_break_invalidates_everything_after_static_prefix() {
        let msgs = vec![u_msg(0, 400), u_msg(1, 400)];
        let est = estimate_prompt_cache_break(&msgs, 0, 1_000);
        assert_eq!(est.break_at_seq, 0);
        assert_eq!(est.prefix_kept_tokens, 1_000);
        assert!(est.invalidated_tokens > 0);
        assert!(est.invalidated_pct > 50.0);
        assert_eq!(
            est.total_prompt_tokens,
            est.prefix_kept_tokens + est.invalidated_tokens
        );
    }

    #[test]
    fn delta_break_keeps_prefix_messages() {
        let msgs = vec![u_msg(0, 200), u_msg(1, 200), u_msg(2, 200)];
        let kept0 = AgentContext::estimate_tokens_static(&msgs[0]);
        let est = estimate_prompt_cache_break(&msgs, 1, 500);
        assert_eq!(est.break_at_seq, 1);
        assert!(est.prefix_kept_tokens >= 500 + kept0);
        assert!(est.invalidated_tokens > 0);
        assert!(est.invalidated_pct < 100.0);
    }

    #[test]
    fn notice_line_uses_rounded_percent() {
        let est = PromptCacheBreakEstimate {
            break_at_seq: 3,
            prefix_kept_tokens: 26_000,
            invalidated_tokens: 42_000,
            invalidated_pct: 61.7,
            total_prompt_tokens: 68_000,
        };
        let line = format_cache_notice_line(&est);
        assert!(line.starts_with("cache: ~"));
        assert!(line.contains("re-read (62%)"));
    }

    #[test]
    fn consolidation_break_at_first_merged_marker() {
        use crate::brain::agent::context::{COMPACTION_MARKER_PREFIX, SEGMENT_SENTINEL};
        let mut ctx = AgentContext::new(uuid::Uuid::new_v4(), 100_000);
        ctx.add_message(Message::user("tail".repeat(50)));
        let marker = |label: &str| {
            Message::user(format!(
                "{COMPACTION_MARKER_PREFIX} — {SEGMENT_SENTINEL} {label}"
            ))
        };
        ctx.add_message(marker("a"));
        ctx.add_message(Message::user("between"));
        ctx.add_message(marker("b"));
        let idx = cache_break_at_for_compaction(
            &ctx,
            CompactionScope::SegmentConsolidation,
            ctx.messages.len(),
            Some(2),
        );
        assert_eq!(idx, ctx.marker_indices()[0]);
    }

    #[test]
    fn offload_break_index_is_earliest_message() {
        let msgs = vec![
            Message {
                role: Role::User,
                content: vec![ContentBlock::Text {
                    text: "early".repeat(100),
                }],
            },
            Message {
                role: Role::User,
                content: vec![ContentBlock::Text {
                    text: "late".repeat(100),
                }],
            },
        ];
        let est = estimate_prompt_cache_break(&msgs, 1, 0);
        assert!(est.invalidated_tokens < est.total_prompt_tokens);
    }
}
