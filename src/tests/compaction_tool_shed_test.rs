//! Tier-0 tool-output shedding (#1003, cache-aware #1010, headroom #1015).

use crate::brain::agent::context::AgentContext;
use crate::brain::agent::service::compaction::tier0_shed_resolves_compaction;
use crate::brain::agent::service::compaction_shed::{
    SHED_KEEP_RECENT_TOOL_ROUNDS, SHED_MIN_RESULT_BYTES, below_shed_headroom,
    over_compaction_soft_trigger, shed_stale_tool_outputs,
};
use crate::brain::agent::service::nudge::{PRESSURE_WARN_CEILING, PRESSURE_WARN_FLOOR};
use crate::brain::provider::{ContentBlock, Message, Role};
use uuid::Uuid;

fn tool_result_body() -> String {
    "line\n".repeat(SHED_MIN_RESULT_BYTES / 4 + 200)
}

fn sized_body(bytes: usize) -> String {
    "x".repeat(bytes)
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

fn usage_pct(ctx: &AgentContext) -> f64 {
    ctx.usage_percentage()
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
    let before = ctx.token_count;
    let report = shed_stale_tool_outputs(session_id, &mut ctx, 0).await;
    assert_eq!(report.blocks_shed, 0);
    assert_eq!(ctx.token_count, before);
}

/// #1010: if even stubbing every eligible block cannot reach 55%, leave the window as-is.
#[tokio::test]
async fn shed_noop_when_headroom_infeasible() {
    let session_id = Uuid::new_v4();
    let max_tokens = 4_000;
    let mut ctx = AgentContext::new(session_id, max_tokens);
    let body = tool_result_body();

    for i in 0..SHED_KEEP_RECENT_TOOL_ROUNDS {
        add_tool_round(&mut ctx, &format!("keep_{i}"), &body);
    }
    add_tool_round(&mut ctx, "stale_only", &body);

    assert!(over_compaction_soft_trigger(&ctx, 0));

    let snapshot: Vec<_> = ctx
        .messages
        .iter()
        .flat_map(|m| {
            m.content.iter().filter_map(|b| match b {
                ContentBlock::ToolResult { content, .. } => Some(content.clone()),
                _ => None,
            })
        })
        .collect();
    let before_tokens = ctx.token_count;

    let report = shed_stale_tool_outputs(session_id, &mut ctx, 0).await;
    assert_eq!(
        report.blocks_shed, 0,
        "infeasible shed must not touch blocks"
    );
    assert_eq!(ctx.token_count, before_tokens);

    let after: Vec<_> = ctx
        .messages
        .iter()
        .flat_map(|m| {
            m.content.iter().filter_map(|b| match b {
                ContentBlock::ToolResult { content, .. } => Some(content.clone()),
                _ => None,
            })
        })
        .collect();
    assert_eq!(after, snapshot);
}

/// #1010: one contiguous pass lands strictly below 55%; recent rounds stay intact.
#[tokio::test]
async fn shed_one_pass_reaches_headroom() {
    let session_id = Uuid::new_v4();
    let max_tokens = 10_000;
    let mut ctx = AgentContext::new(session_id, max_tokens);
    let huge = sized_body(SHED_MIN_RESULT_BYTES + 50_000);
    let small = sized_body(SHED_MIN_RESULT_BYTES + 300);

    for i in 0..4 {
        add_tool_round(&mut ctx, &format!("old_{i}"), &huge);
    }
    for i in 0..SHED_KEEP_RECENT_TOOL_ROUNDS {
        add_tool_round(&mut ctx, &format!("recent_{i}"), &small);
    }

    assert!(over_compaction_soft_trigger(&ctx, 0));

    let report = shed_stale_tool_outputs(session_id, &mut ctx, 0).await;
    assert!(report.blocks_shed >= 1);
    assert!(below_shed_headroom(&ctx, 0));
    assert!(usage_pct(&ctx) < PRESSURE_WARN_FLOOR);

    let last_body = match &ctx.messages.last().unwrap().content[0] {
        ContentBlock::ToolResult { content, .. } => content.clone(),
        _ => panic!("expected tool result"),
    };
    assert!(!last_body.contains("Tool output shed"));
}

/// #1010: eligible blocks are shed in chronological order as a prefix — no gaps.
#[tokio::test]
async fn shed_prefix_is_contiguous_in_chronological_order() {
    let session_id = Uuid::new_v4();
    let max_tokens = 12_000;
    let mut ctx = AgentContext::new(session_id, max_tokens);
    let huge = sized_body(SHED_MIN_RESULT_BYTES + 40_000);
    let small = sized_body(SHED_MIN_RESULT_BYTES + 300);

    add_tool_round(&mut ctx, "first", &huge);
    add_tool_round(&mut ctx, "second", &huge);
    add_tool_round(&mut ctx, "third", &huge);
    for i in 0..SHED_KEEP_RECENT_TOOL_ROUNDS {
        add_tool_round(&mut ctx, &format!("recent_{i}"), &small);
    }

    let _ = shed_stale_tool_outputs(session_id, &mut ctx, 0).await;

    let stubbed = |id: &str| {
        ctx.messages.iter().any(|m| {
            m.content.iter().any(|b| match b {
                ContentBlock::ToolResult {
                    tool_use_id,
                    content,
                    ..
                } if tool_use_id == id => content.contains("Tool output shed"),
                _ => false,
            })
        })
    };

    assert!(stubbed("first"), "oldest eligible must shed first");
    if stubbed("third") {
        assert!(
            stubbed("second"),
            "prefix must not skip a chronologically older block"
        );
    }
}

/// #1010: fill in the 55–65% band must not re-enter tier-0 shed (hysteresis).
#[tokio::test]
async fn shed_does_not_run_between_headroom_and_soft_trigger() {
    let session_id = Uuid::new_v4();
    let max_tokens = 10_000;
    let mut ctx = AgentContext::new(session_id, max_tokens);
    let body = tool_result_body();

    for i in 0..(SHED_KEEP_RECENT_TOOL_ROUNDS + 3) {
        add_tool_round(&mut ctx, &format!("call_{i}"), &body);
    }

    // Simulate a prior shed that left fill in the hysteresis band (55–65%).
    ctx.token_count = ((max_tokens as f64) * 60.0 / 100.0) as usize;

    let pct = usage_pct(&ctx);
    assert!(
        pct > PRESSURE_WARN_FLOOR && pct <= PRESSURE_WARN_CEILING,
        "fixture should sit in the hysteresis band, got {pct}%"
    );

    let before = ctx.token_count;
    let report = shed_stale_tool_outputs(session_id, &mut ctx, 0).await;
    assert_eq!(report.blocks_shed, 0);
    assert_eq!(ctx.token_count, before);
    assert!(!over_compaction_soft_trigger(&ctx, 0));
}

/// #1015: after a successful cache-aware shed, summariser skip gate opens.
#[tokio::test]
async fn successful_shed_allows_tier0_resolve_without_summariser() {
    let session_id = Uuid::new_v4();
    let max_tokens = 10_000;
    let mut ctx = AgentContext::new(session_id, max_tokens);
    let huge = sized_body(SHED_MIN_RESULT_BYTES + 50_000);
    let small = sized_body(SHED_MIN_RESULT_BYTES + 300);

    for i in 0..4 {
        add_tool_round(&mut ctx, &format!("old_{i}"), &huge);
    }
    for i in 0..SHED_KEEP_RECENT_TOOL_ROUNDS {
        add_tool_round(&mut ctx, &format!("recent_{i}"), &small);
    }

    assert!(over_compaction_soft_trigger(&ctx, 0));
    let crossed = true;
    let shed = shed_stale_tool_outputs(session_id, &mut ctx, 0).await;
    assert!(shed.blocks_shed > 0);

    let pct = usage_pct(&ctx);
    assert!(tier0_shed_resolves_compaction(crossed, pct));
    assert!(pct < PRESSURE_WARN_FLOOR);
}
