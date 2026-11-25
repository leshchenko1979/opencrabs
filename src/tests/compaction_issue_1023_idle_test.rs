//! Idle post-turn consolidation apply timing (#1023 fix 1).

use std::sync::{Arc, Mutex};

use crate::brain::agent::context::CompactionScope;
use crate::brain::agent::service::AgentService;
use crate::brain::agent::service::compaction::{
    BudgetPhase, CompactionOutcome, PendingState, finished_idle_pending_for_tests,
};
use crate::brain::agent::service::context::CompactionDocument;
use crate::brain::agent::service::{ProgressCallback, ProgressEvent};
use crate::brain::provider::Message;
use crate::channels::telegram::flow::{
    ToolOffloadNotice, idle_compacted_flow_line, tool_offload_notice,
};
use crate::channels::telegram::intermediates::{is_compaction_card, should_promote_intermediate};
use crate::db::Database;
use crate::services::ServiceContext;
use crate::tests::agent_service_mocks::MockProvider;
use uuid::Uuid;

async fn service() -> AgentService {
    let db = Database::connect_in_memory().await.unwrap();
    db.run_migrations().await.unwrap();
    AgentService::new_for_test(
        Arc::new(MockProvider),
        ServiceContext::new(db.pool().clone()),
    )
    .await
}

fn progress_log() -> (ProgressCallback, Arc<Mutex<Vec<ProgressEvent>>>) {
    let log = Arc::new(Mutex::new(Vec::new()));
    let sink = log.clone();
    let cb: ProgressCallback = Arc::new(move |_sid, event| {
        sink.lock().unwrap().push(event);
    });
    (cb, log)
}

fn consolidation_fixture() -> (Uuid, crate::brain::agent::context::AgentContext) {
    let session_id = Uuid::new_v4();
    let mut c = crate::brain::agent::context::AgentContext::new(session_id, 400_000);
    for i in 0..6 {
        c.add_message(Message::user(format!("q{i}")));
        AgentService::apply_scoped_compaction_summary(
            &mut c,
            CompactionScope::DeltaSinceMarker,
            &format!("MARKER_{i}"),
            None,
        );
    }
    c.add_message(Message::user("live tail"));
    assert!(c.frozen_segment_count() >= 5);
    (session_id, c)
}

fn idle_document() -> CompactionDocument {
    CompactionDocument {
        text: "## 1. Summary\n\nIdle merge doc.".to_string(),
        truncated: false,
    }
}

fn had_idle_receipt(events: &[ProgressEvent]) -> bool {
    events
        .iter()
        .any(|e| matches!(e, ProgressEvent::IdleCompactionSummary { .. }))
}

#[tokio::test]
async fn held_idle_applies_at_settle_not_midloop_under_threshold() {
    let svc = service().await;
    let (session_id, mut ctx) = consolidation_fixture();
    let turn_gen = svc.session_turn_generation_for_tests(session_id);
    let pending = finished_idle_pending_for_tests(&ctx, idle_document(), turn_gen);
    svc.insert_pending_compaction_for_tests(session_id, pending);

    let (cb, log) = progress_log();
    let mid = svc
        .resolve_pending_compaction_for_tests(
            session_id,
            &mut ctx,
            BudgetPhase::MidLoop,
            40.0,
            &Some(cb.clone()),
        )
        .await;
    assert!(
        matches!(mid, PendingState::StillRunning),
        "under 65% mid-loop must hold the idle result"
    );
    assert!(
        svc.has_pending_compaction_for_tests(session_id),
        "pending slot must stay occupied"
    );

    let settle = svc
        .apply_held_idle_compaction_at_settle(session_id, &mut ctx, &Some(cb))
        .await
        .expect("settle must apply");
    assert!(matches!(settle, CompactionOutcome::Summarised(_)));
    assert!(had_idle_receipt(&log.lock().unwrap()));
}

#[tokio::test]
async fn held_idle_applies_at_midloop_threshold_without_new_summariser() {
    let svc = service().await;
    let (session_id, mut ctx) = consolidation_fixture();
    let turn_gen = svc.session_turn_generation_for_tests(session_id);
    svc.insert_pending_compaction_for_tests(
        session_id,
        finished_idle_pending_for_tests(&ctx, idle_document(), turn_gen),
    );

    let (cb, log) = progress_log();
    let state = svc
        .resolve_pending_compaction_for_tests(
            session_id,
            &mut ctx,
            BudgetPhase::MidLoop,
            66.5,
            &Some(cb),
        )
        .await;
    assert!(
        matches!(state, PendingState::Applied(_)),
        "above 65% mid-loop must apply the held idle summary"
    );
    assert!(had_idle_receipt(&log.lock().unwrap()));
}

#[tokio::test]
async fn idle_discarded_when_prefix_changes() {
    let svc = service().await;
    let (session_id, mut ctx) = consolidation_fixture();
    let turn_gen = svc.session_turn_generation_for_tests(session_id);
    svc.insert_pending_compaction_for_tests(
        session_id,
        finished_idle_pending_for_tests(&ctx, idle_document(), turn_gen),
    );
    if let Some(first) = ctx.messages.first_mut() {
        *first = Message::user("prefix changed after spawn");
    }

    let state = svc
        .resolve_pending_compaction_for_tests(
            session_id,
            &mut ctx,
            BudgetPhase::PostTurn,
            40.0,
            &None,
        )
        .await;
    assert!(
        matches!(state, PendingState::Empty),
        "prefix mismatch must discard without apply"
    );
}

#[test]
fn offload_and_idle_merge_notices_promote_as_compaction_receipts() {
    let offload = tool_offload_notice(&ToolOffloadNotice {
        blocks_offloaded: 2,
        tokens_freed: 4_000,
        before_pct: 70.0,
        after_pct: 58.0,
        before_tokens: 70_000,
        after_tokens: 58_000,
        ctx_max: 100_000,
    });
    assert!(is_compaction_card(&offload));
    assert!(should_promote_intermediate(&offload, 0));

    let idle = format!(
        "{}\n   **idle consolidation**",
        idle_compacted_flow_line(42.0, 38.0, 5, 3, std::time::Duration::from_secs(3))
    );
    assert!(is_compaction_card(&idle));
    assert!(should_promote_intermediate(&idle, 0));
}
