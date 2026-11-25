//! Tier-0 tool-output shedding (#1003, #1010).

use crate::brain::agent::context::AgentContext;
use crate::brain::agent::service::compaction_shed::{
    SHED_KEEP_RECENT_TOOL_ROUNDS, SHED_MIN_RESULT_BYTES, shed_stale_tool_outputs,
    shed_target_tokens,
};
use crate::brain::provider::{ContentBlock, Message, Role};
use uuid::Uuid;

fn tool_result_body() -> String {
    "line\n".repeat(SHED_MIN_RESULT_BYTES / 4 + 200)
}

fn sized_body(lines: usize) -> String {
    "x".repeat(lines)
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

fn add_tool_round(ctx: &mut AgentContext, id: &str, body: &str) {
    ctx.add_message(Message::assistant("step"));
    ctx.add_message(user_tool_result(id, body));
}

#[tokio::test]
async fn shed_old_rounds_leaves_recent_intact() {
    let session_id = Uuid::new_v4();
    let max_tokens = 4_000;
    let mut ctx = AgentContext::new(session_id, max_tokens);
    let body = tool_result_body();
    for i in 0..(SHED_KEEP_RECENT_TOOL_ROUNDS + 2) {
        add_tool_round(&mut ctx, &format!("call_{i}"), &body);
    }
    assert!(
        ctx.token_count > shed_target_tokens(max_tokens),
        "fixture must start above the shed target"
    );
    let before = ctx.token_count;
    let report = shed_stale_tool_outputs(session_id, &mut ctx, 0).await;
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
    let report = shed_stale_tool_outputs(session_id, &mut ctx, 0).await;
    assert_eq!(report.blocks_shed, 0);
}

/// #1010: a narrow overshoot freed by one large old result must not also shed
/// smaller eligible results in more recent (but still stale) rounds.
#[tokio::test]
async fn shed_stops_once_fill_reaches_target_without_extra_blocks() {
    let session_id = Uuid::new_v4();
    let max_tokens = 6_000;
    let mut ctx = AgentContext::new(session_id, max_tokens);

    let large = sized_body(SHED_MIN_RESULT_BYTES + 28_000);
    let small = sized_body(SHED_MIN_RESULT_BYTES + 500);

    add_tool_round(&mut ctx, "older_large", &large);
    add_tool_round(&mut ctx, "older_small", &small);
    for i in 0..SHED_KEEP_RECENT_TOOL_ROUNDS {
        add_tool_round(&mut ctx, &format!("recent_{i}"), &small);
    }

    assert!(
        ctx.token_count > shed_target_tokens(max_tokens),
        "fixture must start above the shed target ({} > {})",
        ctx.token_count,
        shed_target_tokens(max_tokens),
    );

    let report = shed_stale_tool_outputs(session_id, &mut ctx, 0).await;
    assert_eq!(
        report.blocks_shed, 1,
        "only the single block needed to reach the target should be shed"
    );
    assert!(
        ctx.token_count <= shed_target_tokens(max_tokens),
        "fill should be at or under target after one shed"
    );

    let small_preserved = ctx
        .messages
        .iter()
        .flat_map(|m| m.content.iter())
        .find_map(|b| match b {
            ContentBlock::ToolResult {
                tool_use_id,
                content,
                ..
            } if tool_use_id == "older_small" => Some(content.clone()),
            _ => None,
        })
        .expect("older_small tool result");
    assert!(
        !small_preserved.contains("Tool output shed"),
        "smaller eligible result must survive when the target is already met"
    );
}

/// #1010: when keep-set and protected bodies leave fill above target, shed every
/// eligible block and leave the remainder to summarisation.
#[tokio::test]
async fn shed_exhausts_eligible_blocks_when_still_over_target() {
    let session_id = Uuid::new_v4();
    let max_tokens = 4_000;
    let mut ctx = AgentContext::new(session_id, max_tokens);

    let body = tool_result_body();
    for i in 0..SHED_KEEP_RECENT_TOOL_ROUNDS {
        add_tool_round(&mut ctx, &format!("keep_{i}"), &body);
    }
    add_tool_round(&mut ctx, "stale_a", &body);
    add_tool_round(&mut ctx, "stale_b", &body);

    assert!(
        ctx.token_count > shed_target_tokens(max_tokens),
        "fixture must start above the shed target"
    );

    let report = shed_stale_tool_outputs(session_id, &mut ctx, 0).await;
    assert_eq!(report.blocks_shed, 2, "both stale rounds should be shed");

    let still_over = ctx.token_count > shed_target_tokens(max_tokens);
    assert!(
        still_over,
        "recent-round keep-set should still leave fill above target so summariser can run"
    );
}
