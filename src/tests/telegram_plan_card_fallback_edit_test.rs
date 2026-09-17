//! #290 regression tests: plan-card edit failures must not cascade into
//! duplicate cards.
//!
//! Before this change every edit error that was not "message is not modified"
//! and not a 429 was treated as `Gone`: the tracked message id was dropped and
//! a brand-new card was posted. A rich-media formatting rejection
//! (`RICH_MESSAGE_PHOTO_INVALID`) or a transient 5xx therefore duplicated the
//! card in the chat on every refresh.
//!
//! The contract now has four outcomes — Saved / Suppressed / Gone / Preserved —
//! and only a genuinely gone message drops the tracked id.

use crate::channels::telegram::plan_card::{
    EditOutcome, handle_edit_failure, is_message_gone_error,
};
use crate::channels::telegram::state::TelegramState;
use teloxide::types::{ChatId, MessageId};
use uuid::Uuid;

#[test]
fn test_is_message_gone_error_classification() {
    // Positive: the tracked message really is unusable, so recreating is right.
    for err in [
        "Bad Request: message to edit not found",
        "Bad Request: message can't be edited",
        "Bad Request: MESSAGE_ID_INVALID",
        "Bad Request: chat not found",
        "Bad Request: TOPIC_CLOSED",
        "Bad Request: message thread not found",
        "Bad Request: thread not found",
    ] {
        assert!(
            is_message_gone_error(err),
            "expected gone-classification for: {err}"
        );
    }

    // Negative: recoverable failures must NOT drop the tracked card id.
    for err in [
        "Bad Request: message is not modified",
        "Bad Request: RICH_MESSAGE_PHOTO_INVALID",
        "Bad Request: can't parse entities: Unexpected end tag",
        "Too Many Requests: retry after 10",
        "Bad Gateway",
        "request timed out",
    ] {
        assert!(
            !is_message_gone_error(err),
            "expected NOT gone-classification for: {err}"
        );
    }
}

/// Seed a live card for a fresh session and return (state, session_id, mid).
async fn seeded_state() -> (TelegramState, Uuid, MessageId) {
    let state = TelegramState::new();
    let session_id = Uuid::new_v4();
    let mid = MessageId(2900);
    state
        .set_plan_card(session_id, ChatId(290), None, mid, "sig-1".to_string())
        .await;
    (state, session_id, mid)
}

#[tokio::test]
async fn test_handle_edit_failure_preserves_card_on_format_error() {
    let (state, session_id, mid) = seeded_state().await;

    let outcome = handle_edit_failure(
        "Bad Request: RICH_MESSAGE_PHOTO_INVALID",
        &state,
        session_id,
        ChatId(290),
        None,
        "sig-2",
        mid,
    )
    .await;

    assert!(
        matches!(outcome, EditOutcome::Preserved),
        "formatting rejection must be Preserved, got a different outcome"
    );
    let tracked = state.plan_card(session_id).await;
    assert_eq!(
        tracked,
        Some((mid, "sig-1".to_string())),
        "the tracked card id and signature must survive a non-fatal edit failure"
    );
}

#[tokio::test]
async fn test_handle_edit_failure_drops_card_on_gone_error() {
    let (state, session_id, mid) = seeded_state().await;

    let outcome = handle_edit_failure(
        "Bad Request: message to edit not found",
        &state,
        session_id,
        ChatId(290),
        None,
        "sig-2",
        mid,
    )
    .await;

    assert!(
        matches!(outcome, EditOutcome::Gone),
        "a deleted message must be Gone so the caller recreates it"
    );
    assert!(
        state.plan_card(session_id).await.is_none(),
        "a gone message must be untracked"
    );
}

#[tokio::test]
async fn test_handle_edit_failure_saves_on_unmodified() {
    let (state, session_id, mid) = seeded_state().await;

    let outcome = handle_edit_failure(
        "Bad Request: message is not modified",
        &state,
        session_id,
        ChatId(290),
        None,
        "sig-2",
        mid,
    )
    .await;

    assert!(
        matches!(outcome, EditOutcome::Saved),
        "an unmodified message is a silent success"
    );
    assert_eq!(
        state.plan_card(session_id).await,
        Some((mid, "sig-2".to_string())),
        "the signature must be advanced so identical refreshes sig-skip"
    );
}

#[tokio::test]
async fn test_handle_edit_failure_suppresses_on_throttle() {
    let (state, session_id, mid) = seeded_state().await;

    let outcome = handle_edit_failure(
        "Too Many Requests: retry after 20",
        &state,
        session_id,
        ChatId(290),
        None,
        "sig-2",
        mid,
    )
    .await;

    assert!(
        matches!(outcome, EditOutcome::Suppressed),
        "a 429 must suppress card writes rather than recreate the card"
    );
    assert!(
        state.plan_card_suppressed(session_id).await,
        "the session must be marked suppressed after a 429"
    );
    assert_eq!(
        state.plan_card(session_id).await.map(|(m, _)| m),
        Some(mid),
        "a throttled edit must keep tracking the existing message"
    );
}

// ---------------------------------------------------------------------------
// Behavioral integration through `refresh_plan_card` itself.
//
// The tests above pin the classifier and the state transitions. They cannot
// show that a rejected rich edit actually reaches the in-place HTML fallback,
// nor that the fresh-post path stays shut. That is the whole contract of #290,
// so it is driven here against a mocked Bot API: a real plan is seeded, a real
// card is tracked, and the wire traffic decides the verdict.
// ---------------------------------------------------------------------------

use std::sync::Arc;

use crate::brain::agent::service::AgentService;
use crate::brain::provider::Provider;
use crate::channels::telegram::flow_chrome::PlanKb;
use crate::channels::telegram::plan_card::refresh_plan_card;
use crate::db::Database;
use crate::services::ServiceContext;
use crate::tests::agent_service_mocks::MockProvider;
use crate::tui::plan::{PlanDocument, PlanStatus, PlanTask, TaskType};
use crate::utils::plan_files::save_plan;

async fn agent_for_test() -> Arc<AgentService> {
    let db = Database::connect_in_memory().await.unwrap();
    db.run_migrations().await.unwrap();
    let context = ServiceContext::new(db.pool().clone());
    let provider: Arc<dyn Provider> = Arc::new(MockProvider);
    Arc::new(AgentService::new_for_test(provider, context).await)
}

/// Pin the process-wide config mirror to the native-rich arm for the duration
/// of one test, and hand back the previous mirror so the caller can restore it.
///
/// `refresh_plan_card` picks rich-vs-classic from `Config::current()`, and the
/// mirror is a process singleton that other suites swap under
/// `governor::test_support::registry_guard` — reading it unpinned makes this
/// test depend on whichever suite ran last. The example file also ships
/// `rich_messages` commented out, so the pinned parse must force it on.
fn pin_rich_config() -> Arc<crate::config::Config> {
    let prev = crate::config::Config::current();
    let mut pinned: crate::config::Config =
        toml::from_str(include_str!("../../config.toml.example"))
            .expect("embedded config.toml.example must parse");
    pinned.channels.telegram.rich_messages = true;
    crate::config::Config::set_current(pinned);
    prev
}

fn restore_config(prev: Arc<crate::config::Config>) {
    crate::config::Config::set_current((*prev).clone());
}

/// A throwaway profile home: `load_plan_sections` reads the plan off disk, so
/// the seeded fixture must land in an isolated home rather than the live one.
async fn in_temp_home<F, T>(f: F) -> T
where
    F: std::future::Future<Output = T>,
{
    let profile = format!("plan-card-290-{}", Uuid::new_v4());
    let out = crate::config::profile::with_profile_home_async(Some(&profile), f).await;
    let home = crate::config::profile::home_for_profile(Some(&profile));
    let _ = std::fs::remove_dir_all(&home);
    out
}

/// An Active checklist with one started task — enough for both the rich and the
/// classic renderers to produce a card.
async fn seed_active_plan(sid: Uuid) {
    let mut plan = PlanDocument::new(sid, "Fallback card".to_string());
    let mut task = PlanTask::new(
        1,
        "Keep the card".to_string(),
        "in place".to_string(),
        TaskType::Edit,
    );
    task.start();
    plan.add_task(task);
    plan.status = PlanStatus::Active;
    save_plan(&plan).await.unwrap();
}

/// A minimal but valid Bot API `Message` envelope: teloxide decodes
/// `editMessageText`'s result into `Message`, and a bare `{"message_id":N}`
/// fails to deserialize.
fn message_envelope(mid: i32, chat: i64) -> String {
    format!(
        r#"{{"ok":true,"result":{{"message_id":{mid},"date":1756166400,"chat":{{"id":{chat},"type":"private"}},"text":"card"}}}}"#
    )
}

const CHAT: i64 = 290290;
const TRACKED_MID: i32 = 4242;

/// #290, the regression itself: a rich edit refused for a NON-fatal reason must
/// be retried in place as classic HTML, and must NOT post a second card.
#[tokio::test]
async fn a_rejected_rich_edit_falls_back_in_place_without_a_duplicate() {
    in_temp_home(async {
        // The config mirror and the governor singletons are process-wide: take
        // the shared guard so no other suite swaps them mid-test.
        let _guard = crate::channels::telegram::governor::test_support::registry_guard().await;
        crate::channels::telegram::governor::test_support::reset(5_000);
        let prev_config = pin_rich_config();

        let sid = Uuid::new_v4();
        seed_active_plan(sid).await;

        let mut server = mockito::Server::new_async().await;

        // 1. The rich edit is refused, but the message is still there: only the
        //    rich-media formatting was rejected. Not a gone-class error.
        let rich_edit = server
            .mock("POST", "/botTESTTOKEN/editMessageText")
            .match_body(mockito::Matcher::Regex("rich_message".to_string()))
            .with_status(400)
            .with_header("content-type", "application/json")
            .with_body(
                r#"{"ok":false,"error_code":400,"description":"Bad Request: RICH_MESSAGE_PHOTO_INVALID"}"#,
            )
            .expect(1)
            .create_async()
            .await;

        // 2. The fallback must be an in-place classic HTML edit of the SAME
        //    message. `parse_mode` is what distinguishes this body from the
        //    rich one — both travel to the same endpoint.
        let html_edit = server
            .mock("POST", "/botTESTTOKEN/editMessageText")
            .match_body(mockito::Matcher::Regex("parse_mode".to_string()))
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(message_envelope(TRACKED_MID, CHAT))
            .expect(1)
            .create_async()
            .await;

        // 3. The regression: no duplicate card. A fresh post is exactly what
        //    the old code did on every non-429 edit failure.
        let no_rich_post = server
            .mock("POST", "/botTESTTOKEN/sendRichMessage")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(message_envelope(9999, CHAT))
            .expect(0)
            .create_async()
            .await;
        let no_html_post = server
            .mock("POST", "/botTESTTOKEN/sendMessage")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(message_envelope(9998, CHAT))
            .expect(0)
            .create_async()
            .await;

        let state = Arc::new(TelegramState::new());
        let agent = agent_for_test().await;
        let bot = teloxide::Bot::new("TESTTOKEN").set_api_url(server.url().parse().unwrap());

        // A live card is tracked, with a stale signature so the refresh does not
        // sig-skip before reaching the wire.
        state
            .set_plan_card(
                sid,
                ChatId(CHAT),
                None,
                MessageId(TRACKED_MID),
                "stale-sig".to_string(),
            )
            .await;

        let rendered = refresh_plan_card(
            &bot,
            ChatId(CHAT),
            None,
            &state,
            &agent,
            sid,
            PlanKb::ApproveDiscard,
        )
        .await;

        assert!(
            rendered,
            "the refresh handled the card, so it must report true"
        );
        rich_edit.assert_async().await;
        html_edit.assert_async().await;
        no_rich_post.assert_async().await;
        no_html_post.assert_async().await;

        assert_eq!(
            state.plan_card(sid).await.map(|(m, _)| m),
            Some(MessageId(TRACKED_MID)),
            "the original message must stay tracked after the in-place fallback"
        );

        restore_config(prev_config);
    })
    .await;
}

/// The other half of the contract: when the message really is gone, the card
/// MUST be recreated — and the in-place fallback must not be attempted.
#[tokio::test]
async fn a_gone_message_is_recreated_and_not_edited_in_place() {
    in_temp_home(async {
        // The config mirror and the governor singletons are process-wide: take
        // the shared guard so no other suite swaps them mid-test.
        let _guard = crate::channels::telegram::governor::test_support::registry_guard().await;
        crate::channels::telegram::governor::test_support::reset(5_000);
        let prev_config = pin_rich_config();

        let sid = Uuid::new_v4();
        seed_active_plan(sid).await;

        let mut server = mockito::Server::new_async().await;

        let rich_edit = server
            .mock("POST", "/botTESTTOKEN/editMessageText")
            .match_body(mockito::Matcher::Regex("rich_message".to_string()))
            .with_status(400)
            .with_header("content-type", "application/json")
            .with_body(
                r#"{"ok":false,"error_code":400,"description":"Bad Request: message to edit not found"}"#,
            )
            .expect(1)
            .create_async()
            .await;

        let fresh_card = server
            .mock("POST", "/botTESTTOKEN/sendRichMessage")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"{"ok":true,"result":{"message_id":7777}}"#)
            .expect(1)
            .create_async()
            .await;

        // A genuinely gone message must NOT be retried in place.
        let no_in_place = server
            .mock("POST", "/botTESTTOKEN/editMessageText")
            .match_body(mockito::Matcher::Regex("parse_mode".to_string()))
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(message_envelope(TRACKED_MID, CHAT))
            .expect(0)
            .create_async()
            .await;

        let state = Arc::new(TelegramState::new());
        let agent = agent_for_test().await;
        let bot = teloxide::Bot::new("TESTTOKEN").set_api_url(server.url().parse().unwrap());

        state
            .set_plan_card(
                sid,
                ChatId(CHAT),
                None,
                MessageId(TRACKED_MID),
                "stale-sig".to_string(),
            )
            .await;

        let rendered = refresh_plan_card(
            &bot,
            ChatId(CHAT),
            None,
            &state,
            &agent,
            sid,
            PlanKb::ApproveDiscard,
        )
        .await;

        assert!(rendered, "a recreated card is a handled refresh");
        rich_edit.assert_async().await;
        fresh_card.assert_async().await;
        no_in_place.assert_async().await;

        assert_eq!(
            state.plan_card(sid).await.map(|(m, _)| m),
            Some(MessageId(7777)),
            "the recreated card's id must replace the gone one"
        );

        restore_config(prev_config);
    })
    .await;
}
