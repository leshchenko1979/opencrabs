//! A button tap in a forum's General topic must not wipe the session's thread
//! binding (#443).
//!
//! Ingress binds a General-topic session under `Some(GENERAL_TOPIC_ID)` (#1220)
//! so General stops colliding with a DM on the same chat. The callback path has
//! composed its session topic correctly since #1248 — but the BINDING WRITE
//! still took the raw `message_thread_id`, and a General message carries none.
//! A tap therefore wrote `NULL` over the row ingress had just established, and
//! the session stopped resolving for the very topic it serves.
//!
//! These tests pin the write: the topic handed to `record_tap_binding` is the
//! COMPOSED session-scoping topic, and it survives the round trip verbatim.

use teloxide::types::ChatId;
use uuid::Uuid;

use crate::channels::telegram::agent::record_tap_binding;
use crate::channels::telegram::session_resolve::{
    GENERAL_TOPIC_ID, session_topic_for_event, topic_session_id,
};
use crate::config::profile::with_home_override_async;
use crate::db::models::Session;
use crate::db::{BindingOrigin, Database, SessionBindingRepository, SessionRepository};

const CHAT: i64 = -1004407925328;
const REAL_TOPIC: i32 = 30045;

/// In-memory DB with migrations applied. Holds no file handle, so it cannot
/// reach a live home even if the override were absent.
async fn test_db() -> Database {
    let db = Database::connect_in_memory().await.expect("in-memory db");
    db.run_migrations().await.expect("migrations");
    db
}

/// `by_session` INNER JOINs `sessions`, so this row is what makes a binding
/// readable — `session_bindings` declares no foreign key.
async fn create_session(db: &Database, id: Uuid) {
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
}

fn repo(db: &Database) -> SessionBindingRepository {
    SessionBindingRepository::new(db.pool().clone())
}

/// The discriminating case: a General-topic tap composes
/// `Some(GENERAL_TOPIC_ID)` and the binding row must hold exactly that.
///
/// FALSIFYING INPUT, stated: `topic_session_id(false, None)` — the raw
/// `message_thread_id` of a General message — is `None`, which is precisely
/// what the pre-fix call site passed into the binding write. That input makes
/// the assertion below fail, and the last assertion names it.
#[tokio::test]
async fn general_topic_tap_persists_the_general_bucket() {
    let dir = tempfile::tempdir().expect("tempdir");
    with_home_override_async(
        dir.path().to_path_buf(),
        async {
            let db = test_db().await;
            let sid = Uuid::new_v4();
            create_session(&db, sid).await;

            // Ingress in a known forum binds General under Some(GENERAL_TOPIC_ID).
            let ingress = session_topic_for_event(false, None, true);
            assert_eq!(ingress, Some(GENERAL_TOPIC_ID));
            repo(&db)
                .upsert(
                    sid.to_string(),
                    "telegram",
                    &CHAT.to_string(),
                    ingress,
                    BindingOrigin::Text,
                )
                .await
                .expect("ingress bind");

            // A tap now writes the SAME composed topic, not the raw thread id.
            let tap = session_topic_for_event(false, None, true);
            record_tap_binding(&repo(&db), sid, ChatId(CHAT), tap).await;

            let bound = repo(&db)
                .by_session(&sid.to_string())
                .await
                .expect("read binding")
                .expect("binding row must exist");
            assert_eq!(
                bound.thread_id,
                Some(GENERAL_TOPIC_ID),
                "a General tap must not wipe the Some(1) binding ingress established"
            );
            assert_eq!(bound.last_origin.as_deref(), Some("callback"));

            // The witness, so the discriminating input is obtained rather than
            // merely described: this is what the pre-fix call site wrote.
            assert_eq!(
                topic_session_id(false, None),
                None,
                "pre-fix call site passed the raw thread id, which is None here"
            );
        },
    )
    .await;
}

/// A real forum topic round-trips unchanged — the fix must not Generalize it.
#[tokio::test]
async fn real_topic_tap_persists_that_topic() {
    let dir = tempfile::tempdir().expect("tempdir");
    with_home_override_async(
        dir.path().to_path_buf(),
        async {
            let db = test_db().await;
            let sid = Uuid::new_v4();
            create_session(&db, sid).await;

            let tap = session_topic_for_event(true, Some(REAL_TOPIC), true);
            assert_eq!(tap, Some(REAL_TOPIC));
            record_tap_binding(&repo(&db), sid, ChatId(CHAT), tap).await;

            let bound = repo(&db)
                .by_session(&sid.to_string())
                .await
                .expect("read binding")
                .expect("binding row must exist");
            assert_eq!(bound.thread_id, Some(REAL_TOPIC));
        },
    )
    .await;
}

/// Cold start: a chat not yet proven to be a forum (a DM, a plain group) keeps
/// the legacy no-topic behaviour, so the fix does not invent a General bucket
/// where none exists.
#[tokio::test]
async fn unknown_forum_tap_keeps_the_base_bucket() {
    let dir = tempfile::tempdir().expect("tempdir");
    with_home_override_async(
        dir.path().to_path_buf(),
        async {
            let db = test_db().await;
            let sid = Uuid::new_v4();
            create_session(&db, sid).await;

            let tap = session_topic_for_event(false, None, false);
            assert_eq!(tap, None);
            record_tap_binding(&repo(&db), sid, ChatId(CHAT), tap).await;

            let bound = repo(&db)
                .by_session(&sid.to_string())
                .await
                .expect("read binding")
                .expect("binding row must exist");
            assert_eq!(bound.thread_id, None);
        },
    )
    .await;
}

/// The guard that catches the defect's SHAPE.
///
/// The three tap sites can only be exercised by a real Telegram callback, which
/// a unit test cannot fabricate — so the behavioural tests above pin the WRITE,
/// and this one pins the CALL: every `record_tap_binding` invocation in the
/// production source must be fed by the composed topic.
///
/// The pre-fix text is what fails here: those call sites passed `thread_id,` /
/// `cb_thread,` — the raw `message_thread_id` — and nothing in the type system
/// distinguishes that from the composed `Option<i32>`.
#[test]
fn every_binding_write_is_fed_by_the_composed_topic() {
    const SRC: &str = include_str!("../channels/telegram/agent.rs");

    // The call sites precede the `fn` definition in file order, so every
    // occurrence after the first is considered and the definition is filtered
    // out by its parameter list rather than by position.
    let mut sites = 0;
    for part in SRC.split("record_tap_binding(").skip(1) {
        let head = part.lines().take(6).collect::<Vec<_>>().join("\n");
        if head.contains("repo: &SessionBindingRepository") {
            continue;
        }
        assert!(
            head.contains("callback_topic("),
            "a record_tap_binding call site bypasses the composed topic: {head}"
        );
        sites += 1;
    }
    assert_eq!(
        sites, 3,
        "expected the three tap sites (follow-up, plan approval, generic routing)"
    );
}