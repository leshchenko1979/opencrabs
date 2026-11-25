//! Drop in-loop thinking blocks for providers that do not preserve them (#1004).

use crate::brain::agent::context::AgentContext;
use crate::brain::provider::{ContentBlock, Message, Role};

/// Remove `Thinking` blocks from assistant messages whose sub-turn has ended.
///
/// Keeps thinking on the last assistant message when it still carries `ToolUse`
/// blocks (in-flight tool round). Adjusts `token_count` to match.
pub(crate) fn drop_thinking_from_completed_assistant_turns(context: &mut AgentContext) {
    let last_assistant_idx = context
        .messages
        .iter()
        .rposition(|m| m.role == Role::Assistant);

    for (i, msg) in context.messages.iter_mut().enumerate() {
        if msg.role != Role::Assistant {
            continue;
        }
        let in_flight = last_assistant_idx == Some(i)
            && msg
                .content
                .iter()
                .any(|b| matches!(b, ContentBlock::ToolUse { .. }));
        if in_flight {
            continue;
        }
        let old = AgentContext::estimate_tokens_static(msg);
        msg.content
            .retain(|b| !matches!(b, ContentBlock::Thinking { .. }));
        let new = AgentContext::estimate_tokens_static(msg);
        if new < old {
            context.token_count = context.token_count.saturating_sub(old - new);
        }
    }
}

/// Summariser input: drop thinking; omit messages that become empty.
pub(crate) fn strip_thinking_for_summariser(messages: Vec<Message>) -> Vec<Message> {
    messages
        .into_iter()
        .filter_map(|mut m| {
            m.content
                .retain(|b| !matches!(b, ContentBlock::Thinking { .. }));
            if m.content.is_empty() { None } else { Some(m) }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::brain::provider::Message;

    #[test]
    fn drops_prior_thinking_keeps_inflight_tool_assistant() {
        let mut ctx = AgentContext::new(uuid::Uuid::new_v4(), 200_000);
        ctx.add_message(Message {
            role: Role::Assistant,
            content: vec![ContentBlock::Thinking {
                thinking: "old".repeat(400),
                signature: None,
            }],
        });
        ctx.add_message(Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::Thinking {
                    thinking: "live".repeat(400),
                    signature: None,
                },
                ContentBlock::ToolUse {
                    id: "t1".into(),
                    name: "bash".into(),
                    input: serde_json::json!({}),
                },
            ],
        });
        drop_thinking_from_completed_assistant_turns(&mut ctx);
        assert!(
            !ctx.messages[0]
                .content
                .iter()
                .any(|b| matches!(b, ContentBlock::Thinking { .. }))
        );
        assert!(
            ctx.messages[1]
                .content
                .iter()
                .any(|b| matches!(b, ContentBlock::Thinking { .. }))
        );
    }
}
