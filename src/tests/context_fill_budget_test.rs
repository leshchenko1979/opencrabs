//! Budget fill: static overhead without anchor, skip overhead with anchor (#context-fill).

use crate::brain::agent::context::AgentContext;
use crate::brain::agent::service::AgentService;
use crate::brain::provider::Message;
use crate::brain::tools::registry::ToolRegistry;
use crate::db::Database;
use crate::services::ServiceContext;
use crate::tests::agent_service_mocks::{MockProvider, MockTool};
use std::sync::Arc;
use uuid::Uuid;

#[test]
fn budget_token_count_adds_overhead_without_anchor() {
    let session_id = Uuid::new_v4();
    let mut ctx = AgentContext::new(session_id, 200_000);
    ctx.add_message(Message::user("hello".to_string()));
    assert_eq!(ctx.budget_token_count(12_000), ctx.token_count + 12_000);
}

#[test]
fn budget_token_count_ignores_overhead_with_anchor() {
    let session_id = Uuid::new_v4();
    let mut ctx = AgentContext::new(session_id, 200_000);
    ctx.add_message(Message::user("hello".to_string()));
    let local = ctx.token_count;
    ctx.record_provider_reported_tokens(local + 50_000);
    assert_eq!(ctx.budget_token_count(12_000), ctx.effective_token_count());
}

#[tokio::test]
async fn base_context_tokens_for_session_counts_sent_tool_schemas() {
    let provider = Arc::new(MockProvider);
    let db = Database::connect_in_memory().await.unwrap();
    db.run_migrations().await.unwrap();
    let context = ServiceContext::new(db.pool().clone());

    let bare = AgentService::new_for_test(provider.clone(), context.clone())
        .await
        .with_system_brain("Brain.".to_string());

    let registry = ToolRegistry::new();
    registry.register(Arc::new(MockTool));
    let session_id = Uuid::new_v4();
    registry.activate_tools(session_id, ["test_tool".to_string()]);
    let with_tools = AgentService::new_for_test(provider, context)
        .await
        .with_system_brain("Brain.".to_string())
        .with_tool_registry(Arc::new(registry));
    let base_bare = bare.base_context_tokens_for_session(session_id);
    let base_tools = with_tools.base_context_tokens_for_session(session_id);
    assert!(
        base_tools > base_bare,
        "session tool schemas ({base_tools}) must exceed brain-only ({base_bare})"
    );
}
