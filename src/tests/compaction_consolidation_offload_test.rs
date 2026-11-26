//! Tier-0 tool-output offloading folded into compaction apply (#owner offload gating).

use crate::brain::agent::context::{AgentContext, CompactionScope};
use crate::brain::agent::service::AgentService;
use crate::brain::agent::service::compaction_offload::{
    OFFLOAD_KEEP_RECENT_TOOL_ROUNDS, OFFLOAD_MIN_RESULT_BYTES,
};
use crate::brain::agent::service::{ProgressCallback, ProgressEvent};
use crate::brain::provider::{ContentBlock, Message, Role};
use crate::db::Database;
use crate::services::ServiceContext;
use crate::tests::agent_service_mocks::MockProvider;
use std::sync::{Arc, Mutex};
use uuid::Uuid;

async fn make_service() -> AgentService {
    let db = Database::connect_in_memory().await.unwrap();
    db.run_migrations().await.unwrap();
    let context = ServiceContext::new(db.pool().clone());
    let provider: Arc<dyn crate::brain::provider::Provider> = Arc::new(MockProvider);
    AgentService::new_for_test(provider, context).await
}

fn large_tool_body() -> String {
    "x\n".repeat(OFFLOAD_MIN_RESULT_BYTES + 400)
}

fn tool_result(id: &str, body: &str) -> Message {
    Message {
        role: Role::User,
        content: vec![ContentBlock::ToolResult {
            tool_use_id: id.into(),
            content: body.into(),
            is_error: None,
        }],
    }
}

fn ctx(max_tokens: usize) -> AgentContext {
    AgentContext::new(Uuid::new_v4(), max_tokens)
}

/// Consolidation scope with stale tool output in the live tail (delta after the last marker).
fn consolidation_live_tail_with_stale_tools(
    max_tokens: usize,
    escalate: bool,
) -> (Uuid, AgentContext) {
    let session_id = Uuid::new_v4();
    let mut c = AgentContext::new(session_id, max_tokens);
    c.add_message(Message::user("q1"));
    c.compact_with_summary(format!("FIRST {}", "m".repeat(8_000)), 0);
    c.add_message(Message::user("q2"));
    AgentService::apply_scoped_compaction_summary(
        &mut c,
        CompactionScope::DeltaSinceMarker,
        &format!("SECOND {}", "n".repeat(8_000)),
        None,
    );
    let body = large_tool_body();
    for i in 0..(OFFLOAD_KEEP_RECENT_TOOL_ROUNDS + 3) {
        c.add_message(Message::assistant(format!("step {i}")));
        c.add_message(tool_result(&format!("live_{i}"), &body));
    }
    assert_eq!(
        c.compaction_scope_with(AgentContext::DEFAULT_SUMMARY_TOKENS, escalate),
        CompactionScope::SegmentConsolidation
    );
    (session_id, c)
}

#[tokio::test]
async fn folded_apply_offload_does_not_emit_standalone_tool_output_offloaded() {
    let svc = make_service().await;
    let (session_id, mut c) = consolidation_live_tail_with_stale_tools(500_000, true);
    let log = Arc::new(Mutex::new(Vec::<ProgressEvent>::new()));
    let sink = log.clone();
    let _cb: ProgressCallback = Arc::new(move |_sid, event| {
        sink.lock().unwrap().push(event);
    });
    let _ = svc
        .tier0_offload_folded_into_compaction_apply(session_id, &mut c)
        .await;

    assert!(
        !log.lock()
            .unwrap()
            .iter()
            .any(|e| matches!(e, ProgressEvent::ToolOutputOffloaded { .. })),
        "folded apply offload must not emit a standalone ToolOutputOffloaded card"
    );
}

#[tokio::test]
async fn folded_apply_offload_runs_below_65_percent_fill() {
    let svc = make_service().await;
    let (session_id, mut c) = consolidation_live_tail_with_stale_tools(500_000, true);
    assert!(
        c.usage_percentage() < 65.0,
        "fixture fill {}",
        c.usage_percentage()
    );

    let before = c.token_count;
    let report = svc
        .tier0_offload_folded_into_compaction_apply(session_id, &mut c)
        .await;

    assert!(report.blocks_offloaded >= 1);
    assert!(c.token_count < before, "stale live-tail tools must offload");
    let last = match &c.messages.last().unwrap().content[0] {
        ContentBlock::ToolResult { content, .. } => content.clone(),
        _ => panic!("expected tool result"),
    };
    assert!(
        !last.contains("[Tool output offloaded:"),
        "recent round stays verbatim"
    );
    let (_, _, input_tokens_after) =
        c.compaction_input_with(AgentContext::DEFAULT_SUMMARY_TOKENS, false);
    assert!(input_tokens_after < before);
}

#[tokio::test]
async fn folded_apply_offloads_for_delta_scope_tail() {
    let svc = make_service().await;
    let mut c = ctx(100_000);
    c.add_message(Message::user("q1"));
    c.compact_with_summary("marker".to_string(), 0);
    for i in 0..(OFFLOAD_KEEP_RECENT_TOOL_ROUNDS + 3) {
        c.add_message(Message::assistant("a"));
        c.add_message(tool_result(&format!("d_{i}"), &large_tool_body()));
    }
    assert_eq!(
        c.compaction_scope_with(AgentContext::DEFAULT_SUMMARY_TOKENS, false),
        CompactionScope::DeltaSinceMarker
    );
    let session_id = c.session_id;
    let before = c.token_count;
    let report = svc
        .tier0_offload_folded_into_compaction_apply(session_id, &mut c)
        .await;
    assert!(report.blocks_offloaded >= 1);
    assert!(c.token_count < before);
}

#[tokio::test]
async fn folded_apply_offload_keeps_error_results_in_live_tail() {
    let svc = make_service().await;
    let (session_id, mut c) = consolidation_live_tail_with_stale_tools(2_000, false);
    let body = large_tool_body();
    for i in 0..5 {
        c.add_message(Message::assistant("a"));
        c.add_message(Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: format!("err_{i}"),
                content: body.clone(),
                is_error: Some(true),
            }],
        });
    }
    let before = c.token_count;
    let _ = svc
        .tier0_offload_folded_into_compaction_apply(session_id, &mut c)
        .await;
    let err_body = c
        .messages
        .iter()
        .rev()
        .find_map(|m| {
            m.content.iter().find_map(|b| match b {
                ContentBlock::ToolResult {
                    content,
                    is_error: Some(true),
                    ..
                } => Some(content.clone()),
                _ => None,
            })
        })
        .expect("error tool result");
    assert_eq!(err_body, body);
    assert!(c.token_count < before);
}

#[tokio::test]
async fn restart_reload_offload_uses_folded_apply_without_channel_card() {
    let svc = make_service().await;
    let (session_id, mut c) = consolidation_live_tail_with_stale_tools(500_000, true);
    let before = c.token_count;
    let log = Arc::new(Mutex::new(Vec::<ProgressEvent>::new()));
    let sink = log.clone();
    let _cb: ProgressCallback = Arc::new(move |_sid, event| {
        sink.lock().unwrap().push(event);
    });
    svc.tier0_offload_on_restart_reload(session_id, &mut c).await;
    assert!(c.token_count < before);
    assert!(
        !log.lock()
            .unwrap()
            .iter()
            .any(|e| matches!(e, ProgressEvent::ToolOutputOffloaded { .. })),
        "restart reload offload logs only — no standalone channel card"
    );
}