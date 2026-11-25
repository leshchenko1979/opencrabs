//! Tier-0 shedding: replace stale in-loop tool results before the LLM summariser (#1003).

use crate::brain::agent::context::AgentContext;
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

/// Replace large, non-error tool results from tool rounds older than
/// [`SHED_KEEP_RECENT_TOOL_ROUNDS`] with a short stub pointing at a spill file.
pub(crate) async fn shed_stale_tool_outputs(
    session_id: Uuid,
    context: &mut AgentContext,
) -> ToolShedReport {
    let mut report = ToolShedReport::default();
    let mut round_from_end: usize = 0;

    for msg in context.messages.iter_mut().rev() {
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

        let old_msg_tokens = AgentContext::estimate_tokens_static(msg);

        for block in msg.content.iter_mut() {
            let ContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
                ..
            } = block
            else {
                continue;
            };
            if matches!(is_error, Some(true)) || content.len() < SHED_MIN_RESULT_BYTES {
                continue;
            }
            if content.contains("Full output saved to:") || content.contains("[Tool output shed:") {
                continue;
            }

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
                continue;
            }

            *content = format!(
                "[Tool output shed: {total_bytes} bytes ({total_lines} lines). \
                 Spill file: {file_path}. Use read_file on this path to inspect.]"
            );
            report.blocks_shed += 1;
        }

        let new_msg_tokens = AgentContext::estimate_tokens_static(msg);
        if new_msg_tokens < old_msg_tokens {
            report.tokens_freed += old_msg_tokens - new_msg_tokens;
            context.token_count = context
                .token_count
                .saturating_sub(old_msg_tokens - new_msg_tokens);
        }
    }

    report
}

/// Head/tail preview helper (shared with spill ingestion tests).
pub(crate) fn spill_preview_for_bytes(content: &str, max_preview_bytes: usize) -> String {
    build_head_tail_preview(content, max_preview_bytes)
}
