//! Tier-0 tool-output offloading on the consolidation path (#1009,
//! cache-aware #1010, headroom #1015).
//!
//! The 65%-gated mid-loop pass (#1003) was removed in #1088: it rewrote tool
//! blocks deep in the cached prefix and measured 3.13x cache-negative. What
//! remains is the best-effort pass run before a consolidation summariser, so
//! these tests drive [`offload_stale_tool_outputs_best_effort`].

use crate::brain::agent::context::AgentContext;
use crate::brain::agent::service::compaction_offload::{
    OFFLOAD_KEEP_RECENT_TOOL_ROUNDS, OFFLOAD_MIN_RESULT_BYTES, below_offload_headroom,
    offload_stale_tool_outputs_best_effort,
};
use crate::brain::agent::service::nudge::{PRESSURE_WARN_CEILING, PRESSURE_WARN_FLOOR};
use crate::brain::provider::{ContentBlock, Message, Role};
use uuid::Uuid;

fn tool_result_body() -> String {
    "line\n".repeat(OFFLOAD_MIN_RESULT_BYTES / 4 + 200)
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
async fn offload_skips_errors() {
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
    let report = offload_stale_tool_outputs_best_effort(session_id, &mut ctx, 0).await;
    assert_eq!(report.blocks_offloaded, 0);
    assert_eq!(ctx.token_count, before);
}

/// #1010: one contiguous pass lands strictly below 55%; recent rounds stay intact.
#[tokio::test]
async fn offload_one_pass_reaches_headroom() {
    let session_id = Uuid::new_v4();
    let max_tokens = 10_000;
    let mut ctx = AgentContext::new(session_id, max_tokens);
    let huge = sized_body(OFFLOAD_MIN_RESULT_BYTES + 50_000);
    let small = sized_body(OFFLOAD_MIN_RESULT_BYTES + 300);

    for i in 0..4 {
        add_tool_round(&mut ctx, &format!("old_{i}"), &huge);
    }
    for i in 0..OFFLOAD_KEEP_RECENT_TOOL_ROUNDS {
        add_tool_round(&mut ctx, &format!("recent_{i}"), &small);
    }

    assert!(
        usage_pct(&ctx) > PRESSURE_WARN_CEILING,
        "fixture must start above the soft trigger"
    );

    let report = offload_stale_tool_outputs_best_effort(session_id, &mut ctx, 0).await;
    assert!(report.blocks_offloaded >= 1);
    assert!(below_offload_headroom(&ctx, 0));
    assert!(usage_pct(&ctx) < PRESSURE_WARN_FLOOR);

    let last_body = match &ctx.messages.last().unwrap().content[0] {
        ContentBlock::ToolResult { content, .. } => content.clone(),
        _ => panic!("expected tool result"),
    };
    assert!(
        !last_body.contains("Tool output offloaded"),
        "recent tool rounds must stay verbatim"
    );
}

/// #1010: eligible blocks are offloaded in chronological order as a prefix — no gaps.
#[tokio::test]
async fn offload_prefix_is_contiguous_in_chronological_order() {
    let session_id = Uuid::new_v4();
    let max_tokens = 12_000;
    let mut ctx = AgentContext::new(session_id, max_tokens);
    let huge = sized_body(OFFLOAD_MIN_RESULT_BYTES + 40_000);
    let small = sized_body(OFFLOAD_MIN_RESULT_BYTES + 300);

    add_tool_round(&mut ctx, "first", &huge);
    add_tool_round(&mut ctx, "second", &huge);
    add_tool_round(&mut ctx, "third", &huge);
    for i in 0..OFFLOAD_KEEP_RECENT_TOOL_ROUNDS {
        add_tool_round(&mut ctx, &format!("recent_{i}"), &small);
    }

    let _ = offload_stale_tool_outputs_best_effort(session_id, &mut ctx, 0).await;

    let stubbed = |id: &str| {
        ctx.messages.iter().any(|m| {
            m.content.iter().any(|b| match b {
                ContentBlock::ToolResult {
                    tool_use_id,
                    content,
                    ..
                } if tool_use_id == id => content.contains("Tool output offloaded"),
                _ => false,
            })
        })
    };

    assert!(stubbed("first"), "oldest eligible must offload first");
    if stubbed("third") {
        assert!(
            stubbed("second"),
            "prefix must not skip a chronologically older block"
        );
    }
}

/// #1009: the pass stops on headroom once it has made progress, so a window
/// that already sits below 55% is not stripped of every stale body.
///
/// This is the branch the removed 65% gate left behind: the gate used to be
/// what kept a low-fill window untouched, and without it the headroom check
/// alone has to bound the pass. One block, then stop.
#[tokio::test]
async fn offload_stops_after_one_block_when_already_below_headroom() {
    let session_id = Uuid::new_v4();
    let max_tokens = 100_000;
    let mut ctx = AgentContext::new(session_id, max_tokens);
    let body = tool_result_body();

    for i in 0..(OFFLOAD_KEEP_RECENT_TOOL_ROUNDS + 3) {
        add_tool_round(&mut ctx, &format!("call_{i}"), &body);
    }

    assert!(
        below_offload_headroom(&ctx, 0),
        "fixture must start below the 55% floor, got {:.1}%",
        usage_pct(&ctx)
    );

    let report = offload_stale_tool_outputs_best_effort(session_id, &mut ctx, 0).await;
    assert_eq!(
        report.blocks_offloaded, 1,
        "a pass that starts below the floor must stop after its first block"
    );
}

/// Existing sessions may still carry the legacy `[Tool output shed:` stub marker.
#[tokio::test]
async fn legacy_shed_stub_skipped_for_idempotence() {
    let session_id = Uuid::new_v4();
    let max_tokens = 10_000;
    let mut ctx = AgentContext::new(session_id, max_tokens);
    let huge = sized_body(OFFLOAD_MIN_RESULT_BYTES + 50_000);
    let legacy_stub = format!(
        "[Tool output shed: {} bytes (1 lines). File: /tmp/legacy.log. Use read_file on this path to inspect.]",
        huge.len()
    );

    for i in 0..4 {
        add_tool_round(&mut ctx, &format!("old_{i}"), &huge);
    }
    add_tool_round(&mut ctx, "legacy", &legacy_stub);
    let small = sized_body(OFFLOAD_MIN_RESULT_BYTES + 300);
    for i in 0..OFFLOAD_KEEP_RECENT_TOOL_ROUNDS {
        add_tool_round(&mut ctx, &format!("recent_{i}"), &small);
    }

    assert!(
        usage_pct(&ctx) > PRESSURE_WARN_CEILING,
        "fixture must start above the soft trigger"
    );

    let report = offload_stale_tool_outputs_best_effort(session_id, &mut ctx, 0).await;
    assert_eq!(
        report.blocks_offloaded, 4,
        "legacy stub body must count as already offloaded"
    );
}
