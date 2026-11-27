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

/// #1063: the static overhead half of the gate's numerator is counted at the
/// session's own model, not with one global cl100k tokenizer.
///
/// Behavioural, through the real function: the brain's contribution is
/// isolated by differencing a service WITH the brain against one with an empty
/// brain under the SAME session model. Both carry the same tool registry, so
/// the schema term cancels and what is left is the brain counted by whichever
/// encoding `base_context_tokens_for_session` reached — a cl100k-only
/// implementation cannot produce the o200k number for the Claude session.
#[tokio::test]
async fn base_context_tokens_for_session_counts_at_the_sessions_model() {
    use crate::brain::tokenizer::{count_tokens, count_tokens_for_model};

    // Repeated so the two encodings' gap is far larger than the `+ 4` style
    // constants elsewhere in the arithmetic, and so an accidental coincidence
    // cannot pass for the fix.
    let brain = "compactionTargetPct backpressureCeilingPct preTruncatePct ".repeat(50);
    const CLAUDE: &str = "claude-opus-4-7";

    let db = Database::connect_in_memory().await.unwrap();
    db.run_migrations().await.unwrap();
    let service_context = ServiceContext::new(db.pool().clone());

    let registry = Arc::new(ToolRegistry::new());
    registry.register(Arc::new(MockTool));

    let with_brain = AgentService::new_for_test(Arc::new(MockProvider), service_context.clone())
        .await
        .with_system_brain(brain.clone())
        .with_tool_registry(registry.clone());
    let empty_brain = AgentService::new_for_test(Arc::new(MockProvider), service_context)
        .await
        .with_tool_registry(registry);

    let claude_session = Uuid::new_v4();
    let gpt_session = Uuid::new_v4();
    for service in [&with_brain, &empty_brain] {
        service.set_session_model(claude_session, CLAUDE.to_string());
        service.set_session_model(gpt_session, "gpt-4o".to_string());
    }

    let claude_brain = with_brain.base_context_tokens_for_session(claude_session)
        - empty_brain.base_context_tokens_for_session(claude_session);
    let gpt_brain = with_brain.base_context_tokens_for_session(gpt_session)
        - empty_brain.base_context_tokens_for_session(gpt_session);

    assert_eq!(
        gpt_brain as usize,
        count_tokens(&brain),
        "a GPT session must still count its brain with cl100k_base"
    );
    assert_eq!(
        claude_brain as usize,
        count_tokens_for_model(&brain, CLAUDE),
        "a Claude session must count its brain with o200k_base (#1063)"
    );
    assert!(
        claude_brain < gpt_brain,
        "the mechanism, as a number: o200k={claude_brain}, cl100k={gpt_brain}"
    );
}
