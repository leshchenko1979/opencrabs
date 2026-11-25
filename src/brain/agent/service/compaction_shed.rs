//! Tier-0 shedding: replace stale in-loop tool results before the LLM summariser (#1003).
//! Sized to the token target (#1010): shed only until fill is back under the 65% gate.

use crate::brain::agent::context::AgentContext;
use crate::brain::agent::service::tool_loop::{TOOL_OUTPUT_DIR, build_head_tail_preview};
use crate::brain::provider::{ContentBlock, Role};
use uuid::Uuid;

/// Keep tool results from the last N tool rounds (0 = most recent round).
pub(crate) const SHED_KEEP_RECENT_TOOL_ROUNDS: usize = 3;

/// Minimum inline body size before shedding is considered.
pub(crate) const SHED_MIN_RESULT_BYTES: usize = 2_000;

/// Window fill at or below this fraction of `max_tokens` is the shed stop target
/// (same boundary as the Tier-1 soft trigger in `compaction.rs`).
pub(crate) const SHED_USAGE_TARGET_NUMERATOR: usize = 65;
pub(crate) const SHED_USAGE_TARGET_DENOMINATOR: usize = 100;

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct ToolShedReport {
    pub blocks_shed: u32,
    pub tokens_freed: usize,
}

struct ShedCandidate {
    msg_idx: usize,
    block_idx: usize,
    /// Higher = older (further from the live edge of the tool loop).
    round_from_end: usize,
    body_tokens: usize,
    body_bytes: usize,
}

pub(crate) fn shed_target_tokens(max_tokens: usize) -> usize {
    max_tokens * SHED_USAGE_TARGET_NUMERATOR / SHED_USAGE_TARGET_DENOMINATOR
}

fn window_fill(ctx: &AgentContext, static_overhead: usize) -> usize {
    ctx.budget_token_count(static_overhead)
}

fn over_shed_target(ctx: &AgentContext, static_overhead: usize) -> bool {
    window_fill(ctx, static_overhead) > shed_target_tokens(ctx.max_tokens)
}

fn is_shed_stub(content: &str) -> bool {
    content.contains("Full output saved to:") || content.contains("[Tool output shed:")
}

fn collect_shed_candidates(messages: &[crate::brain::provider::Message]) -> Vec<ShedCandidate> {
    let mut round_from_end = 0usize;
    let mut candidates = Vec::new();

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

        let shed_this_round = round_from_end >= SHED_KEEP_RECENT_TOOL_ROUNDS;
        round_from_end += 1;

        if !shed_this_round {
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
                round_from_end: round_from_end.saturating_sub(1),
                body_tokens: AgentContext::estimate_tokens(content),
                body_bytes: content.len(),
            });
        }
    }

    candidates.sort_by(|a, b| {
        b.round_from_end
            .cmp(&a.round_from_end)
            .then(b.body_tokens.cmp(&a.body_tokens))
            .then(b.body_bytes.cmp(&a.body_bytes))
    });
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

/// Replace large, non-error tool results from tool rounds older than
/// [`SHED_KEEP_RECENT_TOOL_ROUNDS`] with a short stub pointing at a spill file.
///
/// Eligible results are ranked oldest and largest first; shedding stops once
/// [`window_fill`] is at or below [`shed_target_tokens`] (#1010). When the
/// keep-set still leaves fill above the target, every ranked eligible block is
/// shed and the summariser path remains responsible for the remainder.
pub(crate) async fn shed_stale_tool_outputs(
    session_id: Uuid,
    context: &mut AgentContext,
    static_overhead: usize,
) -> ToolShedReport {
    let mut report = ToolShedReport::default();
    if !over_shed_target(context, static_overhead) {
        return report;
    }

    let candidates = collect_shed_candidates(&context.messages);

    for cand in candidates {
        if !over_shed_target(context, static_overhead) {
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
