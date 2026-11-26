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

/// #1030: the two 65% gates (spawn an idle consolidation, apply one) must read
/// ONE numerator. `usage_percentage()` is `effective_token_count() / max_tokens`
/// and drops `static_overhead`, so while un-anchored the spawn gate used to
/// measure a smaller fill than the apply gate for the very same context. This
/// pins the two numbers apart so a future edit cannot quietly re-conflate them.
#[test]
fn budget_usage_pct_counts_overhead_that_usage_percentage_drops() {
    let session_id = Uuid::new_v4();
    let mut ctx = AgentContext::new(session_id, 100_000);
    ctx.add_message(Message::user("hello".to_string()));
    ctx.token_count = 60_000;

    // The un-anchored window: the apply side counts the static request
    // overhead, `usage_percentage()` does not.
    assert!(ctx.provider_anchor.is_none());
    assert!((ctx.usage_percentage() - 60.0).abs() < 1e-9);
    assert!((ctx.budget_usage_pct(8_000) - 68.0).abs() < 1e-9);

    // Once the provider has reported, the overhead is already inside its count
    // and the two bases converge — which is why the divergence was a
    // first-turn-only window.
    let local = ctx.token_count;
    ctx.record_provider_reported_tokens(local + 50_000);
    assert!((ctx.budget_usage_pct(8_000) - ctx.usage_percentage()).abs() < 1e-9);
}

/// A zero window reports 100% from the budget helper, matching the divisor
/// guards at the gate sites instead of producing NaN.
#[test]
fn budget_usage_pct_reports_full_on_a_zero_window() {
    let session_id = Uuid::new_v4();
    let ctx = AgentContext::new(session_id, 0);
    assert_eq!(ctx.budget_usage_pct(0), 100.0);
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

    let session_id = Uuid::new_v4();
    let registry = Arc::new(ToolRegistry::new());
    registry.register(Arc::new(MockTool));
    let mut config = crate::config::Config::default();
    config.agent.approval_policy = "ask".to_string();
    config.agent.lazy_tools = false;
    let with_tools = AgentService::new(provider, context, &config)
        .await
        .with_system_brain("Brain.".to_string())
        .with_tool_registry(registry);
    let base_bare = bare.base_context_tokens_for_session(session_id);
    let base_tools = with_tools.base_context_tokens_for_session(session_id);
    assert!(
        base_tools > base_bare,
        "session tool schemas ({base_tools}) must exceed brain-only ({base_bare})"
    );
}
