//! `SessionBindingRepository::by_session` — the single-session binding read that
//! the cron send-scope leg resolves a `session:` deliver_to through (#332).
//!
//! Before this accessor existed, `parse_permitted_targets` matched only four
//! hardcoded channel prefixes, so a `session:<uuid>` target fell through the
//! `filter_map`, the permitted set came out empty, and every send in the cron
//! turn was refused — with a reason claiming the job had declared no
//! `deliver_to` at all. These tests pin the read that replaces that guess with
//! the binding row itself.
//!
//! Every test is scoped with `with_home_override_async` onto a throwaway tempdir
//! and runs against an in-memory database, so nothing here can read, repair or
//! rewrite the live profile home (CODE.md item 10).

use crate::config::profile::with_home_override_async;
use crate::db::models::Session;
use crate::db::{BindingOrigin, Database, SessionBindingRepository, SessionRepository};
use uuid::Uuid;

/// In-memory DB with migrations applied. Holds no file handle, so it cannot
/// reach a live home even if the override were absent.
async fn test_db() -> Database {
    let db = Database::connect_in_memory().await.expect("in-memory db");
    db.run_migrations().await.expect("migrations");
    db
}

/// Create the `sessions` row a binding JOINs against. `session_bindings`
/// declares no foreign key (migration 20260826000001), so this row — not a
/// cascade — is what makes a binding visible.
async fn create_session(db: &Database, id: Uuid, archived: bool) {
    SessionRepository::new(db.pool().clone())
        .create(&Session {
            id,
            title: None,
            model: None,
            provider_name: None,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            archived_at: if archived {
                Some(chrono::Utc::now())
            } else {
                None
            },
            token_count: 0,
            total_cost: 0.0,
            working_directory: None,
            auto_title_attempted: false,
            project_id: None,
        })
        .await
        .expect("create session row");
}

async fn bind(
    db: &Database,
    id: Uuid,
    chat: &str,
    thread: Option<i32>,
    origin: BindingOrigin,
) {
    SessionBindingRepository::new(db.pool().clone())
        .upsert(id.to_string(), "telegram", chat, thread, origin)
        .await
        .expect("upsert binding");
}

/// The happy path #332 needs: a bound session yields its channel coordinates.
/// A `None` here is precisely what collapsed the cron send scope to `Nowhere`.
#[tokio::test]
async fn session_binding_by_session_returns_the_binding_row() {
    let dir = tempfile::tempdir().expect("tempdir");
    with_home_override_async(
        dir.path().to_path_buf(),
        async {
            let db = test_db().await;
            let sid = Uuid::new_v4();
            create_session(&db, sid, false).await;
            bind(&db, sid, "-1003936827469", Some(49607), BindingOrigin::Text).await;

            let repo = SessionBindingRepository::new(db.pool().clone());
            let bound = repo
                .by_session(&sid.to_string())
                .await
                .expect("by_session read")
                .expect("a bound session must return its binding row");

            assert_eq!(bound.session_id, sid.to_string());
            assert_eq!(bound.channel, "telegram");
            assert_eq!(bound.chat_id, "-1003936827469");
            assert_eq!(bound.thread_id, Some(49607));
            // A text-origin upsert clears the tap marker (#200).
            assert_eq!(bound.last_origin.as_deref(), Some("text"));
            assert_eq!(bound.turn_open_at, None);

            // Re-binding to a different topic must be visible through the same
            // read: the scope leg has to see where the session lives NOW, not
            // where it was first created.
            bind(&db, sid, "-100999", Some(3), BindingOrigin::Callback).await;
            let moved = repo
                .by_session(&sid.to_string())
                .await
                .expect("by_session read")
                .expect("binding still present after re-bind");
            assert_eq!(moved.chat_id, "-100999");
            assert_eq!(moved.thread_id, Some(3));
            assert_eq!(moved.last_origin.as_deref(), Some("callback"));
            assert!(
                moved.turn_open_at.is_some(),
                "a callback-origin upsert stamps turn_open_at (#200)"
            );
        },
    )
    .await;
}

/// Fail-closed: no binding row means `None`, so the caller keeps refusing
/// instead of inventing a channel. Covers both an unbound-but-existing session
/// and an id nobody has ever seen.
#[tokio::test]
async fn session_binding_by_session_returns_none_without_a_binding_row() {
    let dir = tempfile::tempdir().expect("tempdir");
    with_home_override_async(
        dir.path().to_path_buf(),
        async {
            let db = test_db().await;
            let repo = SessionBindingRepository::new(db.pool().clone());

            let unbound = Uuid::new_v4();
            create_session(&db, unbound, false).await;
            assert!(
                repo.by_session(&unbound.to_string())
                    .await
                    .expect("by_session read")
                    .is_none(),
                "a session with no binding row must resolve to None"
            );

            assert!(
                repo.by_session(&Uuid::new_v4().to_string())
                    .await
                    .expect("by_session read")
                    .is_none(),
                "an unknown session id must resolve to None, not an error"
            );
        },
    )
    .await;
}

/// The INNER JOIN is what hides a binding whose session was deleted (#1224) —
/// the binding row itself survives, because `session_bindings` declares no
/// foreign key. Recreating the session id proves that: the same binding comes
/// straight back, so the `None` came from the JOIN and not from a cascade.
#[tokio::test]
async fn session_binding_by_session_drops_a_binding_whose_session_is_deleted() {
    let dir = tempfile::tempdir().expect("tempdir");
    with_home_override_async(
        dir.path().to_path_buf(),
        async {
            let db = test_db().await;
            let sid = Uuid::new_v4();
            create_session(&db, sid, false).await;
            bind(&db, sid, "-100555", Some(7), BindingOrigin::Text).await;

            let repo = SessionBindingRepository::new(db.pool().clone());

            // Hard-delete the session row: `SessionRepository::delete` is a
            // SOFT delete — it stamps `archived_at` and deliberately preserves
            // the row ("preserved for usage") — so it cannot exercise the INNER
            // JOIN. The JOIN hides a binding only once the `sessions` row is
            // GONE, which is the #1224 state: a binding row outlives its
            // session, and a connect-time re-registration must not revive a
            // dead route off it.
            let sid_str = sid.to_string();
            db.pool()
                .get()
                .await
                .expect("pool")
                .interact(move |conn| {
                    conn.execute(
                        "DELETE FROM sessions WHERE id = ?1",
                        rusqlite::params![sid_str],
                    )
                })
                .await
                .expect("interact")
                .expect("hard-delete the session row");
            assert!(
                repo.by_session(&sid.to_string())
                    .await
                    .expect("by_session read")
                    .is_none(),
                "a session row that is GONE must not leave a live route behind"
            );

            // The binding row was never removed — only hidden by the JOIN.
            create_session(&db, sid, false).await;
            let revived = repo
                .by_session(&sid.to_string())
                .await
                .expect("by_session read")
                .expect("the binding row survived the session delete");
            assert_eq!(revived.chat_id, "-100555");
            assert_eq!(revived.thread_id, Some(7));
        },
    )
    .await;
}

/// Archiving is deliberately NOT filtered here: whether an archived session may
/// still be addressed is the caller's policy, owned by the session-listing tier
/// and its one `include_archived` rule. Pinning it stops a later lane from
/// quietly adding an archive filter to a single-row read and changing the scope
/// leg's behaviour from underneath the D5 alignment.
#[tokio::test]
async fn session_binding_by_session_returns_an_archived_sessions_binding() {
    let dir = tempfile::tempdir().expect("tempdir");
    with_home_override_async(
        dir.path().to_path_buf(),
        async {
            let db = test_db().await;
            let sid = Uuid::new_v4();
            create_session(&db, sid, false).await;
            bind(&db, sid, "-100777", Some(11), BindingOrigin::Text).await;

            let sessions = SessionRepository::new(db.pool().clone());
            sessions.archive(sid).await.expect("archive session");

            let repo = SessionBindingRepository::new(db.pool().clone());
            let bound = repo
                .by_session(&sid.to_string())
                .await
                .expect("by_session read")
                .expect("an archived session keeps its binding row readable");
            assert_eq!(bound.chat_id, "-100777");
            assert_eq!(bound.thread_id, Some(11));
        },
    )
    .await;
}
