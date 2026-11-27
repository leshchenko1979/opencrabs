//! Persist compaction boundaries so reload windows match the live tail (#1008).

use crate::brain::agent::context::AgentContext;
use crate::brain::agent::error::AgentError;
use crate::brain::provider::{ContentBlock, Message, Role};
use crate::services::MessageService;
use uuid::Uuid;

pub(crate) fn message_role_for_persistence(msg: &Message) -> &'static str {
    match msg.role {
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::System => "system",
    }
}

pub(crate) fn message_content_for_persistence(msg: &Message) -> String {
    msg.content
        .iter()
        .map(|block| match block {
            ContentBlock::Text { text } => text.clone(),
            ContentBlock::ToolResult { content, .. } => content.clone(),
            ContentBlock::ToolUse { name, input, .. } => {
                format!("[tool_use: {name}] {}", input)
            }
            ContentBlock::Thinking { thinking, .. } => {
                format!("[thinking omitted: {} chars]", thinking.len())
            }
            ContentBlock::Image { .. } => "[image]".to_string(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Retire the obligation apparatus on SUPERSEDED marker rows (#1022).
///
/// The apply retires those markers in MEMORY, but the rows on disk keep their
/// `Obligation status: OPEN` lines, and `load_compaction_window` would hand them
/// straight back on the next turn — an in-memory-only strip is undone by the
/// first reload. So the same strip is written through here, in the same step
/// that applied the new summary.
///
/// Only rows STRICTLY ABOVE the newest SUMMARISING marker are touched: that one
/// is the summary this apply just wrote, and the SYSTEM continuation appended
/// after it refers to exactly that document. A SUSPENDED row is neither: the
/// loop guard summarised nothing, so it supersedes nothing and carries no
/// obligation of its own to protect.
///
/// The sweep starts at the reload boundary rather than at row 0. A marker above
/// the newest restart is never loaded (`boundary_start_index` is the predicate
/// `load_compaction_window` anchors on, #987), so rewriting it would be a write
/// with no reader.
///
/// `CompactionScope` is deliberately NOT consulted: a delta leaves earlier
/// markers in place, and a consolidation already deleted them, so the set of
/// superseded rows is whatever the window contains minus the newest — computed
/// from the rows themselves rather than assumed from the scope.
///
/// Returns how many rows were rewritten.
pub(crate) async fn retire_superseded_marker_rows(
    session_id: Uuid,
    message_service: &MessageService,
) -> Result<usize, AgentError> {
    use crate::brain::agent::compaction_boundary::{
        MarkerKind, boundary_start_index, classify_user_marker,
    };

    let rows = message_service
        .list_messages_for_session(session_id)
        .await
        .map_err(AgentError::db)?;
    // Only the rows a reload actually loads. A marker above the newest RESTART
    // is never in the window (`boundary_start_index` is the predicate
    // `load_compaction_window` anchors on, #987), so rewriting it would be a
    // write with no reader.
    let window_start = boundary_start_index(&rows).unwrap_or(0);
    let summarised: Vec<usize> = rows
        .iter()
        .enumerate()
        .skip(window_start)
        .filter(|(_, r)| {
            matches!(
                classify_user_marker(&r.content),
                Some(MarkerKind::Restart | MarkerKind::DeltaSegment)
            )
        })
        .map(|(i, _)| i)
        .collect();
    // The newest summarising marker is the row this apply just created — it
    // keeps its obligation. Everything above it is superseded.
    let Some((_newest, superseded)) = summarised.split_last() else {
        return Ok(0);
    };
    let mut rewritten = 0usize;
    for &i in superseded {
        let Some(stripped) = super::context::retire_superseded_obligation(&rows[i].content) else {
            continue;
        };
        message_service
            .set_content(rows[i].id, &stripped)
            .await
            .map_err(AgentError::db)?;
        rewritten += 1;
    }
    if rewritten > 0 {
        tracing::info!(
            "Compaction (#1022): retired the obligation in {rewritten} superseded marker row(s) \
             — only the newest may be OPEN"
        );
    }
    Ok(rewritten)
}

/// After a no-summary cut, re-persist the in-memory tail AFTER the marker row so
/// `messages_from_last_compaction` reloads the kept verbatim tail instead of
/// hiding it behind the boundary (#226, #1008).
pub(crate) async fn repersist_context_tail_after_marker(
    session_id: Uuid,
    message_service: &MessageService,
    context: &AgentContext,
) -> Result<usize, AgentError> {
    let mut written = 0usize;
    for msg in &context.messages {
        if AgentContext::is_compaction_marker_msg(msg) {
            continue;
        }
        let role = message_role_for_persistence(msg);
        let content = message_content_for_persistence(msg);
        message_service
            .create_message(session_id, role.to_string(), content)
            .await
            .map_err(AgentError::db)?;
        written += 1;
    }
    Ok(written)
}
