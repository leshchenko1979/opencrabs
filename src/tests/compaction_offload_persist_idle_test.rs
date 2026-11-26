//! Tier-0 offload DB persist + idle eager-apply prefix (#bug-A, #bug-B).

use crate::brain::agent::context::AgentContext;
use crate::brain::agent::service::AgentService;
use crate::brain::agent::service::compaction::{
    compaction_prefix_fingerprint, idle_pending_with_prefix_for_tests,
};
use crate::brain::agent::service::compaction_offload::{
    OFFLOAD_MIN_RESULT_BYTES, offload_stale_tool_outputs_best_effort,
};
use crate::brain::agent::service::compaction_prefix::compaction_prefix_fingerprint_db_rows;
use crate::brain::agent::service::context::CompactionDocument;
use crate::brain::agent::service::context_rows::clean_rows_for_llm;
use crate::brain::provider::{ContentBlock, Message, Role};
use crate::db::Database;
use crate::services::{MessageService, ServiceContext, SessionService};
use crate::tests::agent_service_mocks::MockProvider;
use std::sync::Arc;
use uuid::Uuid;

fn tool_body() -> String {
    "line\n".repeat(OFFLOAD_MIN_RESULT_BYTES / 4 + 50)
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

async fn seeded_session_with_tool_ledger() -> (AgentService, Uuid, String, MessageService) {
    let db = Database::connect_in_memory().await.unwrap();
    db.run_migrations().await.unwrap();
    let ctx = ServiceContext::new(db.pool().clone());
    let session = SessionService::new(ctx.clone())
        .create_session(Some("offload-persist".to_string()))
        .await
        .unwrap();
    let session_id = session.id;
    let message_service = MessageService::new(ctx.clone());
    let body = tool_body();
    let ledger = format!(
        "step <!-- tools-v2: {} -->",
        serde_json::to_string(&[serde_json::json!({"d":"run","s":true,"o": body})]).unwrap()
    );
    message_service
        .create_message(session_id, "assistant".to_string(), ledger)
        .await
        .unwrap();
    let svc = AgentService::new_for_test(Arc::new(MockProvider), ctx).await;
    (svc, session_id, body, message_service)
}

#[tokio::test]
async fn offload_persists_stub_into_assistant_tools_v2_ledger() {
    let (_svc, session_id, body, message_service) = seeded_session_with_tool_ledger().await;
    let mut context = AgentContext::new(session_id, 500_000);
    for i in 0..4 {
        context.add_message(Message::assistant(format!("step{i}")));
        context.add_message(user_tool_result(&format!("tool_{i}"), &body));
    }

    let report =
        offload_stale_tool_outputs_best_effort(session_id, &mut context, 0, Some(&message_service))
            .await;
    assert!(report.blocks_offloaded >= 1);

    let rows = message_service
        .list_messages_for_session(session_id)
        .await
        .unwrap();
    let assistant = rows.iter().find(|r| r.role == "assistant").unwrap();
    assert!(
        assistant.content.contains("[Tool output offloaded:"),
        "DB ledger must hold the stub after offload"
    );
    assert!(!assistant.content.contains(&body));
}

#[tokio::test]
async fn folded_offload_at_apply_keeps_canonical_prefix() {
    let (svc, session_id, body, message_service) = seeded_session_with_tool_ledger().await;
    let model = svc
        .provider_for_session(session_id)
        .default_model()
        .to_string();

    let (len_before, hash_before) = svc
        .canonical_compaction_prefix_snapshot(session_id, &model)
        .await
        .expect("snapshot before offload");

    let mut context = AgentContext::new(session_id, 500_000);
    for i in 0..6 {
        context.add_message(Message::user(format!("q{i}")));
        AgentService::apply_scoped_compaction_summary(
            &mut context,
            crate::brain::agent::context::CompactionScope::DeltaSinceMarker,
            &format!("MARKER_{i}"),
            None,
        );
    }
    context.add_message(Message::assistant("step"));
    context.add_message(user_tool_result("stale", &body));

    let _ =
        offload_stale_tool_outputs_best_effort(session_id, &mut context, 0, Some(&message_service))
            .await;

    let (len_after, hash_after) = svc
        .canonical_compaction_prefix_snapshot(session_id, &model)
        .await
        .expect("snapshot after offload");

    assert_eq!(
        (len_before, hash_before),
        (len_after, hash_after),
        "offload must not change the canonical idle prefix fingerprint"
    );

    let in_memory_wrong = compaction_prefix_fingerprint(&context, context.messages.len());
    assert_ne!(
        in_memory_wrong, hash_after,
        "in-memory-only fingerprint should differ after offload — idle apply must not use it"
    );

    let pending = idle_pending_with_prefix_for_tests(
        &context,
        CompactionDocument {
            text: "doc".into(),
            truncated: false,
        },
        0,
        len_after,
        hash_after,
    );
    assert!(
        svc.idle_compaction_prefix_still_valid(session_id, &model, &pending)
            .await
    );
}

#[test]
fn db_fingerprint_normalizes_tools_v2_body_and_stub() {
    let body = tool_body();
    let stub = "[Tool output offloaded: 1 bytes (1 lines). File: /tmp/x. Use read_file on this path to inspect.]";
    let full = format!(
        "a <!-- tools-v2: {} -->",
        serde_json::to_string(&[serde_json::json!({"d":"x","s":true,"o": body})]).unwrap()
    );
    let stubbed = format!(
        "a <!-- tools-v2: {} -->",
        serde_json::to_string(&[serde_json::json!({"d":"x","s":true,"o": stub})]).unwrap()
    );
    use crate::db::models::Message as DbMessage;
    use chrono::Utc;
    let mk = |content: &str| DbMessage {
        id: Uuid::new_v4(),
        session_id: Uuid::new_v4(),
        role: "assistant".to_string(),
        content: content.to_string(),
        sequence: 1,
        created_at: Utc::now(),
        token_count: None,
        cost: None,
        input_tokens: None,
        cache_creation_tokens: None,
        cache_read_tokens: None,
        thinking: None,
        duration_secs: None,
    };
    let mut rows = vec![mk(&full)];
    clean_rows_for_llm(&mut rows, false);
    let h1 = compaction_prefix_fingerprint_db_rows(&rows, 1);
    rows[0].content = stubbed;
    clean_rows_for_llm(&mut rows, false);
    let h2 = compaction_prefix_fingerprint_db_rows(&rows, 1);
    assert_eq!(h1, h2);
}
