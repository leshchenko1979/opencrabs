//! The resume wrapper runs the end-of-turn flush (#201).
//!
//! Until #201 the only consumer of `drain_queued_items` was the tail of
//! `handle_message`. A user message (or detached result) arriving after the
//! tool loop's LAST between-rounds drain — during final-bubble generation,
//! or on a push-initiated turn at all — was queued with no consumer left:
//! the turn ended, the item sat in the queue, and the operator had to type
//! a nudge before the NEXT turn finally drained it. The fix extracts the
//! flush into `resume::flush_queued_after_turn` and calls it from the
//! resume wrapper after its guard drops.
//!
//! These tests drive the real wrapper over a mocked Bot API with a
//! single-round mock provider (no tool calls → no between-rounds drain),
//! so the only thing that can consume a queued item is the new flush.
//!
//! Fixtures are synthetic and carry no user identifiers.

use std::sync::Arc;

use teloxide::prelude::*;
use teloxide::types::ChatId;
use uuid::Uuid;

use crate::brain::agent::service::AgentService;
use crate::brain::agent::{PendingOrigin, QueuedUserMessage};
use crate::brain::provider::Provider;
use crate::channels::telegram::resume::resume_session;
use crate::db::Database;
use crate::services::ServiceContext;
use crate::tests::agent_service_mocks::MockProvider;

/// A minimal but valid Bot API `Message` envelope for sendMessage/editMessageText.
const MESSAGE_JSON: &str =
    r#"{"ok":true,"result":{"message_id":9001,"date":1,"chat":{"id":12345,"type":"private"}}}"#;
/// Envelope for methods whose result type is `True` (reactions, chat actions…).
const TRUE_JSON: &str = r#"{"ok":true,"result":true}"#;

async fn test_agent() -> Arc<AgentService> {
    // In-memory agent service (house pattern: telegram_stream_loop_resume_test).
    let db = Database::connect_in_memory().await.unwrap();
    db.run_migrations().await.unwrap();
    let context = ServiceContext::new(db.pool().clone());
    let provider: Arc<dyn Provider> = Arc::new(MockProvider);
    Arc::new(AgentService::new_for_test(provider, context).await)
}

async fn mocked_bot() -> (Bot, mockito::ServerGuard) {
    let mut server = mockito::Server::new_async().await;
    let bot = Bot::new("test-token").set_api_url(server.url().parse().unwrap());
    // Catch-alls (creation order = priority): message-shaped results for the
    // send/edit pair the streaming pipeline drives, generic True for the rest.
    server
        .mock(
            "POST",
            mockito::Matcher::Regex(r"(?i)bot.*/editmessagetext".into()),
        )
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(MESSAGE_JSON)
        .create_async()
        .await;
    server
        .mock(
            "POST",
            mockito::Matcher::Regex(r"(?i)bot.*/sendmessage".into()),
        )
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(MESSAGE_JSON)
        .create_async()
        .await;
    server
        .mock("POST", mockito::Matcher::Regex(r"bot.*".into()))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(TRUE_JSON)
        .create_async()
        .await;
    (bot, server)
}

#[tokio::test]
async fn resume_turn_flushes_an_item_queued_after_its_last_drain() {
    let state = Arc::new(crate::channels::telegram::TelegramState::new());
    let sid = Uuid::new_v4();
    let agent = test_agent().await;
    let (bot, _server) = mocked_bot().await;

    // The #201 shape: the item lands while the turn is in its final
    // generation round — after the loop's last between-rounds drain. With a
    // single-round provider nothing between-rounds ever fires, so the flush
    // at the wrapper's tail is the only possible consumer.
    state.enqueue_reaction(
        sid,
        QueuedUserMessage::plain("arrived too late".to_string()),
    );

    resume_session(
        bot,
        ChatId(12345),
        None,
        sid,
        "resume prompt".to_string(),
        agent,
        state.clone(),
        Some(PendingOrigin::System),
    )
    .await
    .expect("resume turn completes");

    assert!(
        state.drain_queued_items(sid).is_empty(),
        "#201: an item queued after the last drain must be flushed when the \
         resume turn ends, not sit until the operator types a nudge"
    );
    assert!(
        !state.is_turn_active(sid),
        "the turn slot must be free after the flush ran (the flush reaction \
         arm takes the guard itself, so it must have been released first)"
    );
}

#[tokio::test]
async fn empty_queue_flush_is_a_noop() {
    let state = Arc::new(crate::channels::telegram::TelegramState::new());
    let sid = Uuid::new_v4();
    let agent = test_agent().await;
    let (bot, _server) = mocked_bot().await;

    resume_session(
        bot,
        ChatId(12345),
        None,
        sid,
        "resume prompt".to_string(),
        agent,
        state.clone(),
        Some(PendingOrigin::System),
    )
    .await
    .expect("resume turn completes");

    assert!(
        !state.is_turn_active(sid),
        "an empty flush must leave the session idle"
    );
}

#[tokio::test]
async fn a_busy_skip_requeues_unstarted_prompt_as_detached_work() {
    // #227: when resume_session contends on active turn guard, it re-queues
    // the unstarted prompt back into telegram_state as detached work.
    let state = Arc::new(crate::channels::telegram::TelegramState::new());
    let sid = Uuid::new_v4();
    let agent = test_agent().await;
    let (bot, _server) = mocked_bot().await;

    let _running = state.try_begin_turn(sid).expect("first claims");
    state.enqueue_reaction(
        sid,
        QueuedUserMessage::plain("held for the live turn".to_string()),
    );

    resume_session(
        bot,
        ChatId(12345),
        None,
        sid,
        "resume prompt".to_string(),
        agent,
        state.clone(),
        Some(PendingOrigin::System),
    )
    .await
    .expect("busy skip returns Ok");

    let drained = state.drain_queued_items(sid);
    assert_eq!(
        drained.len(),
        2,
        "#227: busy skip must re-enqueue the unstarted prompt as detached work in addition to existing reaction"
    );
    assert_eq!(
        drained[0].origin,
        crate::channels::telegram::QueuedOrigin::Reaction
    );
    assert_eq!(
        drained[1].origin,
        crate::channels::telegram::QueuedOrigin::DetachedWork
    );
    assert_eq!(drained[1].msg.context_text, "resume prompt");
}

#[tokio::test]
async fn flush_combines_detached_and_reactions_into_single_turn() {
    // #227: when both detached results and reactions are queued,
    // flush_queued_after_turn merges them into a single spawned turn rather
    // than racing synchronous reaction execution against spawned detached resume.
    let state = Arc::new(crate::channels::telegram::TelegramState::new());
    let sid = Uuid::new_v4();
    let agent = test_agent().await;
    let (bot, _server) = mocked_bot().await;

    state.enqueue_detached_result(
        sid,
        QueuedUserMessage::plain("detached result payload".to_string()),
    );
    state.enqueue_reaction(
        sid,
        QueuedUserMessage::plain("stranded reaction payload".to_string()),
    );

    // Draining directly verifies that flush_queued_after_turn will consume both
    crate::channels::telegram::resume::flush_queued_after_turn(
        bot,
        ChatId(12345),
        None,
        sid,
        agent,
        state.clone(),
    )
    .await;

    // Both should be drained
    assert!(
        state.drain_queued_items(sid).is_empty(),
        "#227: flush_queued_after_turn must drain both detached and reaction items"
    );
}
