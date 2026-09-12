//! A suppressed plan-card refresh must report that it rendered nothing (#187 D3).
//!
//! `refresh_plan_card` opens with the flood-control gate (#814): when Telegram
//! has told the session to wait, the whole refresh is skipped before any read or
//! API work. Callers use the return value to decide whether the card now shows
//! what they asked for, so the gate has to be observable. A caller that records
//! content as rendered on a skipped refresh will not retry it, and the card
//! keeps the older footer.
//!
//! The progress tracker in `agent.rs` is exactly such a caller: it carries a
//! `last_rendered` note and skips a refresh whose note is unchanged. Marking the
//! note rendered before the render is what made a suppressed refresh permanent.
//!
//! Both directions are pinned here, because `false` is only correct at the gate:
//! a refresh that ran must still report `true` even when there is no plan to
//! draw — the no-plan path removes nothing and posts nothing, so these tests
//! touch no network and no real state. Fixtures are synthetic.

use std::sync::Arc;
use std::time::Duration;

use teloxide::types::ChatId;
use uuid::Uuid;

use crate::brain::agent::service::AgentService;
use crate::brain::provider::Provider;
use crate::channels::telegram::flow_chrome::PlanKb;
use crate::channels::telegram::plan_card::refresh_plan_card;
use crate::channels::telegram::state::TelegramState;
use crate::db::Database;
use crate::services::ServiceContext;
use crate::tests::agent_service_mocks::MockProvider;

async fn agent_for_test() -> Arc<AgentService> {
    let db = Database::connect_in_memory().await.unwrap();
    db.run_migrations().await.unwrap();
    let context = ServiceContext::new(db.pool().clone());
    let provider: Arc<dyn Provider> = Arc::new(MockProvider);
    Arc::new(AgentService::new_for_test(provider, context).await)
}

/// The gate is checked before anything else, so a suppressed refresh makes no
/// Bot API call at all — asserted with `expect(0)`, not inferred.
#[tokio::test]
async fn a_suppressed_refresh_reports_that_it_rendered_nothing() {
    let mut server = mockito::Server::new_async().await;
    let no_calls = server
        .mock("POST", mockito::Matcher::Any)
        .with_status(200)
        .with_body("{}")
        .expect(0)
        .create_async()
        .await;

    let state = Arc::new(TelegramState::new());
    let agent = agent_for_test().await;
    let bot = teloxide::Bot::new("test-token").set_api_url(server.url().parse().unwrap());
    let session = Uuid::new_v4();

    state
        .suppress_plan_card(session, Duration::from_secs(60))
        .await;
    assert!(
        state.plan_card_suppressed(session).await,
        "precondition: the session is inside its backoff window"
    );

    let rendered = refresh_plan_card(
        &bot,
        ChatId(12345),
        None,
        &state,
        &agent,
        session,
        PlanKb::ApproveDiscard,
    )
    .await;

    assert!(
        !rendered,
        "a refresh skipped by the flood-control gate must not claim it rendered — \
         the caller would record the note as on-screen and never retry it"
    );
    no_calls.assert_async().await;
}

/// Without suppression the refresh runs, and it says so — `false` means the gate
/// fired, never "the card looked the same". A no-plan session is the cheapest
/// honest way to exercise the whole body: the renderers return nothing, the
/// just-archived flag is absent for a random session, and the removal path is a
/// no-op because no card is tracked.
#[tokio::test]
async fn an_unsuppressed_refresh_reports_that_it_ran() {
    let state = Arc::new(TelegramState::new());
    let agent = agent_for_test().await;
    let bot = teloxide::Bot::new("test-token");
    let session = Uuid::new_v4();

    assert!(
        !state.plan_card_suppressed(session).await,
        "precondition: a fresh session is not suppressed"
    );

    let rendered = refresh_plan_card(
        &bot,
        ChatId(12345),
        None,
        &state,
        &agent,
        session,
        PlanKb::ApproveDiscard,
    )
    .await;

    assert!(
        rendered,
        "a refresh that ran must report true — only the suppression gate reports false"
    );
}
