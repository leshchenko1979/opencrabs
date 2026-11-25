//! Tier-0 shedding: replace stale in-loop tool results before the LLM summariser (#1003).
//! Cache-aware sizing (#1010): one contiguous chronological prefix per trigger, with
//! hysteresis down to [`PRESSURE_WARN_FLOOR`] (#1015).

use crate::brain::agent::context::AgentContext;
use crate::brain::agent::service::nudge::PRESSURE_WARN_FLOOR;
use crate::brain::agent::service::tool_loop::{TOOL_OUTPUT_DIR, build_head_tail_preview};
use crate::brain::provider::{ContentBlock, Role};
use uuid::Uuid;

/// Keep tool results from the last N tool rounds (0 = most recent round).
pub(crate) const SHED_KEEP_RECENT_TOOL_ROUNDS: usize = 3;

/// Minimum inline body size before shedding is considered.
pub(crate) const SHED_MIN_RESULT_BYTES: usize = 2_000;

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct ToolShedReport {
    pub blocks_shed: u32,
    pub tokens_freed: usize,
}

struct ShedCandidate {
    msg_idx: usize,
    block_idx: usize,
    body_tokens: usize,
}

fn window_fill(ctx: &AgentContext, static_overhead: usize) -> usize {
    ctx.budget_token_count(static_overhead)
}

fn usage_pct(ctx: &AgentContext, static_overhead: usize) -> f64 {
    if ctx.max_tokens == 0 {
        return 100.0;
    }
    (window_fill(ctx, static_overhead) as f64 / ctx.max_tokens as f64) * 100.0
}

/// Soft compaction trigger: strictly above the 65% ceiling (#909 / Tier-1).
pub(crate) fn over_compaction_soft_trigger(ctx: &AgentContext, static_overhead: usize) -> bool {
    usage_pct(ctx, static_overhead) > super::nudge::PRESSURE_WARN_CEILING
}

/// After shedding, fill must sit **below** 55% so the next iteration does not
/// immediately re-trigger and [#1015] can skip the summariser.
pub(crate) fn below_shed_headroom(ctx: &AgentContext, static_overhead: usize) -> bool {
    usage_pct(ctx, static_overhead) < PRESSURE_WARN_FLOOR
}

fn is_shed_stub(content: &str) -> bool {
    content.contains("Full output saved to:") || content.contains("[Tool output shed:")
}

/// Rough stub size after spill; used only for the feasibility pre-check.
fn shed_stub_token_estimate() -> usize {
    AgentContext::estimate_tokens(
        "[Tool output shed: 0 bytes (0 lines). Spill file: /tmp/x. Use read_file on this path to inspect.]",
    )
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
fn collect_shed_candidates_chronological(
    messages: &[crate::brain::provider::Message],
) -> Vec<ShedCandidate> {
    let ages: std::collections::HashMap<usize, usize> =
        tool_round_from_end(messages).into_iter().collect();
    let mut candidates = Vec::new();

    for (msg_idx, msg) in messages.iter().enumerate() {
        let Some(&round_from_end) = ages.get(&msg_idx) else {
            continue;
        };
        if round_from_end < SHED_KEEP_RECENT_TOOL_ROUNDS {
            continue;
        }

        for (block_idx, block) in msg.content.iter().enumerate() {
            let ContentBlock::ToolResult {
                content, is_error, ..
            } = block
            else {
                continue;
            };
            if matches!(is_error, Some(true)) || content.len() < SHED_MIN_RESULT_BYTES {
                continue;
            }
            if is_shed_stub(content) {
                continue;
            }
            candidates.push(ShedCandidate {
                msg_idx,
                block_idx,
                body_tokens: AgentContext::estimate_tokens(content),
            });
        }
    }
    candidates
}

fn estimated_tokens_freed(body_tokens: usize) -> usize {
    body_tokens.saturating_sub(shed_stub_token_estimate())
}

/// If every eligible block were stubbed, would fill sit below [`PRESSURE_WARN_FLOOR`]?
fn shedding_all_reaches_headroom(
    ctx: &AgentContext,
    static_overhead: usize,
    candidates: &[ShedCandidate],
) -> bool {
    let save: usize = candidates
        .iter()
        .map(|c| estimated_tokens_freed(c.body_tokens))
        .sum();
    let projected = window_fill(ctx, static_overhead).saturating_sub(save);
    if ctx.max_tokens == 0 {
        return false;
    }
    (projected as f64 / ctx.max_tokens as f64) * 100.0 < PRESSURE_WARN_FLOOR
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
        tracing::warn!("Tier-0 shed: failed to create {TOOL_OUTPUT_DIR}: {e}");
    }
    if let Err(e) = tokio::fs::write(&file_path, content.as_bytes()).await {
        tracing::warn!("Tier-0 shed: failed to write spill {file_path}: {e}");
        return false;
    }

    *content = format!(
        "[Tool output shed: {total_bytes} bytes ({total_lines} lines). \
         Spill file: {file_path}. Use read_file on this path to inspect.]"
    );
    true
}

/// Replace stale tool results when the window is above 65%.
///
/// Sheds a **contiguous chronological prefix** of eligible blocks in one pass until
/// fill is strictly below 55%. If stubbing every eligible block would still not reach
/// that headroom, the window is left untouched (#1010 cache policy — one invalidation
/// band, or defer to the summariser).
pub(crate) async fn shed_stale_tool_outputs(
    session_id: Uuid,
    context: &mut AgentContext,
    static_overhead: usize,
) -> ToolShedReport {
    let mut report = ToolShedReport::default();
    if !over_compaction_soft_trigger(context, static_overhead) {
        return report;
    }

    let candidates = collect_shed_candidates_chronological(&context.messages);
    if candidates.is_empty() {
        return report;
    }

    if !shedding_all_reaches_headroom(context, static_overhead, &candidates) {
        return report;
    }

    for cand in candidates {
        if below_shed_headroom(context, static_overhead) {
            break;
        }

        let msg = &mut context.messages[cand.msg_idx];
        let old_msg_tokens = AgentContext::estimate_tokens_static(msg);

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
            || content.len() < SHED_MIN_RESULT_BYTES
            || is_shed_stub(content)
        {
            continue;
        }

        if !spill_and_stub_tool_result(session_id, tool_use_id, content).await {
            continue;
        }
        report.blocks_shed += 1;

        let new_msg_tokens = AgentContext::estimate_tokens_static(msg);
        if new_msg_tokens < old_msg_tokens {
            let freed = old_msg_tokens - new_msg_tokens;
            report.tokens_freed += freed;
            context.token_count = context.token_count.saturating_sub(freed);
        }
    }

    report
}

/// Head/tail preview helper (shared with spill ingestion tests).
#[allow(dead_code)]
pub(crate) fn spill_preview_for_bytes(content: &str, max_preview_bytes: usize) -> String {
    build_head_tail_preview(content, max_preview_bytes)
}
