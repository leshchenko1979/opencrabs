//! #572 — topic lifecycle teardown and its recorded facts.
//!
//! A closed forum topic used to keep its `session_bindings` row forever: the
//! closure arrived, was parsed, and was handled by nobody, so the dead topic
//! went on resolving to a live session. That table's whole write surface was 2
//! INSERTs and 4 UPDATEs with **no DELETE** — the row outlived its topic by
//! omission, not by design.
//!
//! The contract under test:
//! 1. a closure deletes the binding row of ITS topic and no other — a sibling
//!    topic in the same chat keeps its row;
//! 2. a closure retires the session that the deleted row named;
//! 3. a reopen deletes nothing and retires nobody;
//! 4. a deletion — which no inbound update can announce, because Telegram emits
//!    no such event and the pinned teloxide-core 0.13.0 carries no
//!    `forum_topic_deleted` at all — tears down exactly like a closure but is
//!    recorded under its OWN `message_type`, so the store can tell the two
//!    apart.
//!
//! `apply_topic_teardown` is the primitive-typed core; `handle_topic_teardown`
//! strips a teloxide `Message` down to it. Testing the core keeps this fixture
//! honest — a hand-built `Message`/`Chat`/`MessageKind` would stand in for a
//! real update while testing nothing about the teardown — and the
//! Message→core mapping is exercised by the live probe in the ship smoke.
//!
//! Every test runs against an in-memory DB, and the `ServiceContext` is built
//! on that SAME pool so the archive leg cannot land on a second database
//! (CODE.md item 10 — nothing here can reach a live profile home).

use crate::channels::telegram::handler::{TopicEvent, apply_topic_teardown};
use crate::db::models::Session;
use crate::db::{
    BindingOrigin, ChannelMessageRepository, Database, SessionBindingRepository, SessionRepository,
};
use crate::services::{ServiceContext, SessionService};
use uuid::Uuid;

const CHAT: i64 = -1_005_720_001;
const CHAT_STR: &str = "-1005720001";
const TOPIC: i32 = 501;
const SIBLING_TOPIC: i32 = 502;

async fn test_db() -> Database {
    let db = Database::connect_in_memory().await.expect("in-memory db");
    db.run_migrations().await.expect("migrations");
    db
}

async fn create_session(db: &Database) -> Uuid {
    let id = Uuid::new_v4();
    SessionRepository::new(db.pool().clone())
        .create(&Session {
            id,
            title: None,
            model: None,
            provider_name: None,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            archived_at: None,
            token_count: 0,
            total_cost: 0.0,
            working_directory: None,
            auto_title_attempted: false,
            project_id: None,
        })
        .await
        .expect("create session row");
    id
}

async fn bind(db: &Database, session: Uuid, thread: i32) {
    SessionBindingRepository::new(db.pool().clone())
        .upsert(
            session.to_string(),
            "telegram",
            CHAT_STR,
            Some(thread),
            BindingOrigin::Text,
        )
        .await
        .expect("bind session to topic");
}

/// The rows a given event was recorded under — one `message_type` per event is
/// the whole point, so the count is asserted per type rather than in aggregate.
async fn facts_of(messages: &ChannelMessageRepository, message_type: &str) -> usize {
    messages
        .recent(
            Some("telegram"),
            CHAT_STR,
            5,
            Some(&TOPIC.to_string()),
            Some(message_type),
        )
        .await
        .expect("read fact rows")
        .len()
}

fn svc_for(db: &Database) -> SessionService {
    SessionService::new(ServiceContext::new(db.pool().clone()))
}

/// One closure: the closed topic loses its row, its session is retired, the
/// sibling topic in the same chat is untouched, and the fact is recorded.
#[tokio::test]
async fn closure_removes_only_its_own_binding_and_retires_the_session() {
    let db = test_db().await;
    let bindings = SessionBindingRepository::new(db.pool().clone());
    let messages = ChannelMessageRepository::new(db.pool().clone());
    let svc = svc_for(&db);

    let closed_session = create_session(&db).await;
    let sibling_session = create_session(&db).await;
    bind(&db, closed_session, TOPIC).await;
    bind(&db, sibling_session, SIBLING_TOPIC).await;

    apply_topic_teardown(
        TopicEvent::Closed,
        CHAT,
        TOPIC,
        Some(9001),
        Some(42),
        Some("admin"),
        &messages,
        &bindings,
        &svc,
    )
    .await;

    // 1. the closed topic's row is gone; the sibling topic's survives, because
    //    the delete is keyed on (channel, chat_id, thread_id) and not on chat.
    assert!(
        bindings
            .find_by_channel_chat_thread("telegram", CHAT_STR, Some(TOPIC))
            .await
            .unwrap()
            .is_none(),
        "the closed topic's binding row must be deleted"
    );
    let sibling = bindings
        .find_by_channel_chat_thread("telegram", CHAT_STR, Some(SIBLING_TOPIC))
        .await
        .unwrap()
        .expect("a sibling topic's binding must survive the closure of another topic");
    assert_eq!(sibling.session_id, sibling_session.to_string());

    // 2. the retired row's session is archived; the sibling's is not.
    let retired = SessionRepository::new(db.pool().clone())
        .find_by_id(closed_session)
        .await
        .unwrap()
        .expect("the session row survives archiving");
    assert!(
        retired.archived_at.is_some(),
        "the session bound to the closed topic must be archived"
    );
    let alive = SessionRepository::new(db.pool().clone())
        .find_by_id(sibling_session)
        .await
        .unwrap()
        .expect("sibling session exists");
    assert!(
        alive.archived_at.is_none(),
        "an unrelated session must not be retired by another topic's closure"
    );

    // 4. exactly one fact row, under this event's own type.
    assert_eq!(facts_of(&messages, "topic_closed").await, 1);
    assert_eq!(
        facts_of(&messages, "topic_reopened").await,
        0,
        "negative control — the reopen type must not be written by a closure"
    );
}

/// A reopen records the fact and tears down nothing — the assertion that keeps
/// the close path from firing on a topic that has just come back to life.
#[tokio::test]
async fn reopen_deletes_no_binding() {
    let db = test_db().await;
    let bindings = SessionBindingRepository::new(db.pool().clone());
    let messages = ChannelMessageRepository::new(db.pool().clone());
    let svc = svc_for(&db);

    let session = create_session(&db).await;
    bind(&db, session, TOPIC).await;

    apply_topic_teardown(
        TopicEvent::Reopened,
        CHAT,
        TOPIC,
        Some(9002),
        Some(42),
        Some("admin"),
        &messages,
        &bindings,
        &svc,
    )
    .await;

    let kept = bindings
        .find_by_channel_chat_thread("telegram", CHAT_STR, Some(TOPIC))
        .await
        .unwrap()
        .expect("a reopen must not delete the binding");
    assert_eq!(kept.session_id, session.to_string());

    let not_archived = SessionRepository::new(db.pool().clone())
        .find_by_id(session)
        .await
        .unwrap()
        .expect("session row exists");
    assert!(
        not_archived.archived_at.is_none(),
        "a reopen must not retire the session either"
    );

    assert_eq!(facts_of(&messages, "topic_reopened").await, 1);
    assert_eq!(
        facts_of(&messages, "topic_closed").await,
        0,
        "negative control — the closure type must not be written by a reopen"
    );
}

/// A deletion tears down like a closure but is recorded under its own type.
/// Telegram never announces a deletion, so this leg is the probe's path — and
/// the distinct type is what lets a sweep tell "closed" from "gone".
#[tokio::test]
async fn deletion_records_its_own_fact_and_tears_down() {
    let db = test_db().await;
    let bindings = SessionBindingRepository::new(db.pool().clone());
    let messages = ChannelMessageRepository::new(db.pool().clone());
    let svc = svc_for(&db);

    let session = create_session(&db).await;
    bind(&db, session, TOPIC).await;

    apply_topic_teardown(
        TopicEvent::Deleted,
        CHAT,
        TOPIC,
        Some(9003),
        None, // a deletion has no actor — nobody performed it
        None,
        &messages,
        &bindings,
        &svc,
    )
    .await;

    assert!(
        bindings
            .find_by_channel_chat_thread("telegram", CHAT_STR, Some(TOPIC))
            .await
            .unwrap()
            .is_none(),
        "a deleted topic must not keep its binding"
    );
    let retired = SessionRepository::new(db.pool().clone())
        .find_by_id(session)
        .await
        .unwrap()
        .expect("session row survives archiving");
    assert!(
        retired.archived_at.is_some(),
        "the session of a deleted topic must be archived"
    );

    assert_eq!(facts_of(&messages, "topic_deleted").await, 1);
    assert_eq!(
        facts_of(&messages, "topic_closed").await,
        0,
        "negative control — a deletion is not a closure and must not be recorded as one"
    );
}
