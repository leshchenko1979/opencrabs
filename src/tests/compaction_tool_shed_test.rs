//! Tier-0 tool-output shedding (#1003).

use crate::brain::agent::context::AgentContext;
use crate::brain::agent::service::compaction_shed::{
    SHED_KEEP_RECENT_TOOL_ROUNDS, SHED_MIN_RESULT_BYTES, shed_stale_tool_outputs,
};
use crate::brain::provider::{ContentBlock, Message, Role};
use uuid::Uuid;

fn tool_result_body() -> String {
    "line\n".repeat(SHED_MIN_RESULT_BYTES / 4 + 200)
}

fn user_tool_result(id: &str, body: &str) -> Message {
    Message {
        role: Role::User,
        content: vec![ContentBlock::ToolResult {
            tool_use_id: id.to_string(),
            content: body.to_string(),
            is_error: None,
        }],
    }
}

#[tokio::test]
async fn shed_old_rounds_leaves_recent_intact() {
    let session_id = Uuid::new_v4();
    let mut ctx = AgentContext::new(session_id, 500_000);
    let body = tool_result_body();
    for i in 0..(SHED_KEEP_RECENT_TOOL_ROUNDS + 2) {
        ctx.add_message(Message::assistant(format!("step {i}")));
        ctx.add_message(user_tool_result(&format!("call_{i}"), &body));
    }
    let before = ctx.token_count;
    let report = shed_stale_tool_outputs(session_id, &mut ctx).await;
    assert!(report.blocks_shed >= 1);
    assert!(report.tokens_freed > 0);
    assert!(ctx.token_count < before);
    let last_body = match &ctx.messages.last().unwrap().content[0] {
        ContentBlock::ToolResult { content, .. } => content.clone(),
        _ => panic!("expected tool result"),
    };
    assert!(
        !last_body.contains("Tool output shed"),
        "most recent round must stay verbatim"
    );
}

#[tokio::test]
async fn shed_skips_errors() {
    let session_id = Uuid::new_v4();
    let mut ctx = AgentContext::new(session_id, 500_000);
    let body = tool_result_body();
    for i in 0..5 {
        ctx.add_message(Message::assistant("a"));
        ctx.add_message(Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: format!("e_{i}"),
                content: body.clone(),
                is_error: Some(true),
            }],
        });
    }
    let report = shed_stale_tool_outputs(session_id, &mut ctx).await;
    assert_eq!(report.blocks_shed, 0);
}
