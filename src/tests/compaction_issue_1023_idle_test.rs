//! Idle post-turn consolidation apply timing (#1023 fix 1).

use std::sync::{Arc, Mutex};

use crate::brain::agent::context::CompactionScope;
use crate::brain::agent::service::AgentService;
use crate::brain::agent::service::compaction::{
    BudgetPhase, CompactionOutcome, PendingState, idle_pending_canonical_for_tests,
    running_idle_pending_canonical_for_tests,
};
use crate::brain::agent::service::compaction_persist::{
    message_content_for_persistence, message_role_for_persistence,
};
use crate::brain::agent::service::context::CompactionDocument;
use crate::brain::agent::service::{ProgressCallback, ProgressEvent};
use crate::brain::provider::Message;
use crate::channels::telegram::flow::{
    ToolOffloadNotice, idle_compacted_flow_line, tool_offload_notice,
};
use crate::channels::telegram::intermediates::{is_compaction_card, should_promote_intermediate};
use crate::db::{Database, models::Session, repository::SessionRepository};
use crate::services::{MessageService, ServiceContext};
use crate::tests::agent_service_mocks::MockProvider;
use chrono::Utc;
use uuid::Uuid;

async fn service() -> (AgentService, ServiceContext) {
    let db = Database::connect_in_memory().await.unwrap();
    db.run_migrations().await.unwrap();
    let ctx = ServiceContext::new(db.pool().clone());
    let svc = AgentService::new_for_test(Arc::new(MockProvider), ctx.clone()).await;
    (svc, ctx)
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

async fn ensure_session_row(ctx: &ServiceContext, session_id: Uuid) {
    let repo = SessionRepository::new(ctx.pool());
    let session = Session {
        id: session_id,
        title: Some("idle-test".into()),
        created_at: Utc::now(),
        updated_at: Utc::now(),
        archived_at: None,
        model: None,
        provider_name: None,
        token_count: 0,
        total_cost: 0.0,
        working_directory: None,
        auto_title_attempted: false,
        project_id: None,
        channel_chat_key: None,
    };
    let _ = repo.create(&session).await;
}

async fn seed_context_to_db(
    service_ctx: &ServiceContext,
    session_id: Uuid,
    ctx: &crate::brain::agent::context::AgentContext,
) {
    ensure_session_row(service_ctx, session_id).await;
    let message_service = MessageService::new(service_ctx.clone());
    for msg in &ctx.messages {
        message_service
            .create_message(
                session_id,
                message_role_for_persistence(msg).to_string(),
                message_content_for_persistence(msg),
            )
            .await
            .unwrap();
    }
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
    let (svc, service_ctx) = service().await;
    let (session_id, mut ctx) = consolidation_fixture();
    seed_context_to_db(&service_ctx, session_id, &ctx).await;
    let turn_gen = svc.session_turn_generation_for_tests(session_id);
    let pending = idle_pending_canonical_for_tests(&svc, &ctx, idle_document(), turn_gen).await;
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
    let (svc, service_ctx) = service().await;
    let (session_id, mut ctx) = consolidation_fixture();
    seed_context_to_db(&service_ctx, session_id, &ctx).await;
    let turn_gen = svc.session_turn_generation_for_tests(session_id);
    svc.insert_pending_compaction_for_tests(
        session_id,
        idle_pending_canonical_for_tests(&svc, &ctx, idle_document(), turn_gen).await,
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
async fn idle_prefix_mismatch_aborts_in_flight_cancel_token() {
    let (svc, service_ctx) = service().await;
    let (session_id, mut ctx) = consolidation_fixture();
    seed_context_to_db(&service_ctx, session_id, &ctx).await;
    let turn_gen = svc.session_turn_generation_for_tests(session_id);
    let (pending, cancel) = running_idle_pending_canonical_for_tests(&svc, &ctx, turn_gen).await;
    svc.insert_pending_compaction_for_tests(session_id, pending);

    let message_service = MessageService::new(service_ctx.clone());
    let first = message_service
        .list_messages_for_session(session_id)
        .await
        .unwrap()
        .into_iter()
        .next()
        .unwrap();
    message_service
        .set_content(first.id, "prefix changed in DB after spawn")
        .await
        .unwrap();

    let state = svc
        .resolve_pending_compaction_for_tests(
            session_id,
            &mut ctx,
            BudgetPhase::PostTurn,
            40.0,
            &None,
        )
        .await;
    assert!(matches!(state, PendingState::Empty));
    assert!(
        cancel.is_cancelled(),
        "stale idle work must be cancelled so the slot can accept a fresh spawn"
    );
    assert!(
        !svc.has_pending_compaction_for_tests(session_id),
        "discarded pending must not occupy the slot"
    );
}

#[tokio::test]
async fn idle_discarded_when_prefix_changes() {
    let (svc, service_ctx) = service().await;
    let (session_id, mut ctx) = consolidation_fixture();
    seed_context_to_db(&service_ctx, session_id, &ctx).await;
    let turn_gen = svc.session_turn_generation_for_tests(session_id);
    svc.insert_pending_compaction_for_tests(
        session_id,
        idle_pending_canonical_for_tests(&svc, &ctx, idle_document(), turn_gen).await,
    );

    let message_service = MessageService::new(service_ctx.clone());
    let first = message_service
        .list_messages_for_session(session_id)
        .await
        .unwrap()
        .into_iter()
        .next()
        .unwrap();
    message_service
        .set_content(first.id, "prefix changed in DB")
        .await
        .unwrap();

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
        cache_notice: None,
    });
    assert!(is_compaction_card(&offload));
    assert!(should_promote_intermediate(&offload, 0));

    let idle = format!(
        "{}\n   **idle consolidation**",
        idle_compacted_flow_line(70.0, 58.0, 5, 3, std::time::Duration::from_secs(12))
    );
    assert!(is_compaction_card(&idle));
    assert!(should_promote_intermediate(&idle, 0));
}
