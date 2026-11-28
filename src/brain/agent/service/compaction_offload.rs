//! Tier-0 offloading: replace stale tool results before a consolidation summariser
//! (#1009). The mid-loop, 65%-gated pass (#1003) was removed in #1088 — it measured
//! 3.13x cache-negative, because it rewrote blocks deep inside the cached prefix.
//! Cache-aware sizing (#1010): one contiguous chronological prefix per call, with
//! hysteresis down to [`PRESSURE_WARN_FLOOR`] (#1015).

use crate::brain::agent::context::AgentContext;
use crate::brain::agent::service::nudge::PRESSURE_WARN_FLOOR;
use crate::brain::agent::service::tool_loop::TOOL_OUTPUT_DIR;
use crate::brain::provider::{ContentBlock, Role};
use uuid::Uuid;

/// Keep tool results from the last N tool rounds (0 = most recent round).
pub(crate) const OFFLOAD_KEEP_RECENT_TOOL_ROUNDS: usize = 3;

/// Minimum inline body size before offloading is considered.
pub(crate) const OFFLOAD_MIN_RESULT_BYTES: usize = 2_000;

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct ToolOffloadReport {
    pub blocks_offloaded: u32,
    pub tokens_freed: usize,
    /// Earliest message index whose body was stubbed (#1025). This is the
    /// prompt-cache break point for the next request: everything before it
    /// serializes byte-identically, everything from it on is new.
    pub first_offloaded: Option<usize>,
}

struct OffloadCandidate {
    msg_idx: usize,
    block_idx: usize,
}

/// The SAME fill the compaction gates read (#1030) — delegated rather than
/// re-derived, so an offload decision can never sit on a third numerator.
fn usage_pct(ctx: &AgentContext, static_overhead: usize) -> f64 {
    ctx.budget_usage_pct(static_overhead)
}

/// After offloading, fill must sit **below** 55% so the window is not merely
/// dented and a following consolidation has real headroom (#1015).
pub(crate) fn below_offload_headroom(ctx: &AgentContext, static_overhead: usize) -> bool {
    usage_pct(ctx, static_overhead) < PRESSURE_WARN_FLOOR
}

fn is_offload_stub(content: &str) -> bool {
    content.contains("Full output saved to:")
        || content.contains("[Tool output offloaded:")
        || content.contains("[Tool output shed:")
}

/// Map each user tool-result message to its age in tool rounds from the live edge.
fn tool_round_from_end(messages: &[crate::brain::provider::Message]) -> Vec<(usize, usize)> {
    let mut round_from_end = 0usize;
    let mut ages = Vec::new();
    for (msg_idx, msg) in messages.iter().enumerate().rev() {
        if msg.role != Role::User {
            continue;
        }
        let has_tool_result = msg
            .content
            .iter()
            .any(|b| matches!(b, ContentBlock::ToolResult { .. }));
        if !has_tool_result {
            continue;
        }
        ages.push((msg_idx, round_from_end));
        round_from_end += 1;
    }
    ages.sort_by_key(|(idx, _)| *idx);
    ages
}

/// Eligible blocks in **chronological** order (oldest eligible first).
fn collect_offload_candidates_chronological(
    messages: &[crate::brain::provider::Message],
) -> Vec<OffloadCandidate> {
    let ages: std::collections::HashMap<usize, usize> =
        tool_round_from_end(messages).into_iter().collect();
    let mut candidates = Vec::new();

    for (msg_idx, msg) in messages.iter().enumerate() {
        let Some(&round_from_end) = ages.get(&msg_idx) else {
            continue;
        };
        if round_from_end < OFFLOAD_KEEP_RECENT_TOOL_ROUNDS {
            continue;
        }

        for (block_idx, block) in msg.content.iter().enumerate() {
            let ContentBlock::ToolResult {
                content, is_error, ..
            } = block
            else {
                continue;
            };
            if matches!(is_error, Some(true)) || content.len() < OFFLOAD_MIN_RESULT_BYTES {
                continue;
            }
            if is_offload_stub(content) {
                continue;
            }
            candidates.push(OffloadCandidate { msg_idx, block_idx });
        }
    }
    candidates
}

async fn spill_and_stub_tool_result(
    session_id: Uuid,
    tool_use_id: &str,
    content: &mut String,
) -> bool {
    let total_bytes = content.len();
    let total_lines = content.lines().count();
    let sanitized_id: String = tool_use_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let file_path = format!(
        "{}/session_{}_{}.log",
        TOOL_OUTPUT_DIR, session_id, sanitized_id
    );

    if let Err(e) = tokio::fs::create_dir_all(TOOL_OUTPUT_DIR).await {
        tracing::warn!("Tier-0 offload: failed to create {TOOL_OUTPUT_DIR}: {e}");
    }
    if let Err(e) = tokio::fs::write(&file_path, content.as_bytes()).await {
        tracing::warn!("Tier-0 offload: failed to write spill {file_path}: {e}");
        return false;
    }

    *content = format!(
        "[Tool output offloaded: {total_bytes} bytes ({total_lines} lines). \
         File: {file_path}. Use read_file on this path to inspect.]"
    );
    true
}

async fn offload_eligible_prefix(
    session_id: Uuid,
    context: &mut AgentContext,
    static_overhead: usize,
) -> ToolOffloadReport {
    let mut report = ToolOffloadReport::default();
    if context.tier0_offload_this_cycle {
        return report;
    }

    let candidates = collect_offload_candidates_chronological(&context.messages);
    if candidates.is_empty() {
        return report;
    }

    for cand in candidates {
        let at_headroom = below_offload_headroom(context, static_overhead);
        // Best-effort: keep offloading when fill started below 55% (#1009); only
        // stop on headroom once this call has made progress.
        if at_headroom && report.blocks_offloaded > 0 {
            break;
        }

        let msg = &mut context.messages[cand.msg_idx];
        let old_msg_tokens = AgentContext::estimate_tokens_static_for(msg, &context.model);

        let ContentBlock::ToolResult {
            tool_use_id,
            content,
            is_error,
            ..
        } = &mut msg.content[cand.block_idx]
        else {
            continue;
        };
        if matches!(is_error, Some(true))
            || content.len() < OFFLOAD_MIN_RESULT_BYTES
            || is_offload_stub(content)
        {
            continue;
        }

        if !spill_and_stub_tool_result(session_id, tool_use_id, content).await {
            continue;
        }
        report.blocks_offloaded += 1;
        // Candidates arrive oldest-first, so the first success is the break.
        report.first_offloaded.get_or_insert(cand.msg_idx);

        let new_msg_tokens = AgentContext::estimate_tokens_static_for(msg, &context.model);
        if new_msg_tokens < old_msg_tokens {
            let freed = old_msg_tokens - new_msg_tokens;
            report.tokens_freed += freed;
            context.token_count = context.token_count.saturating_sub(freed);
        }
    }

    if report.blocks_offloaded > 0 {
        context.tier0_offload_this_cycle = true;
    }

    report
}

/// The consolidation-path pass (#1009 / idle consolidation): no 65% gate, no
/// all-or-nothing precheck; still stops when fill drops below 55% or candidates
/// are exhausted.
pub(crate) async fn offload_stale_tool_outputs_best_effort(
    session_id: Uuid,
    context: &mut AgentContext,
    static_overhead: usize,
) -> ToolOffloadReport {
    offload_eligible_prefix(session_id, context, static_overhead).await
}
