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
