//! Repository tests for the durable await record (#344).
//!
//! `awaiting_for_channel` is an OR-path beside the boot classifier's freshness
//! gate, NOT a widening of it: it carries no time filter, so a binding the gate
//! stopped considering hours ago is still readable. These tests pin that
//! orthogonality, because the failure mode it guards against is silent — a lane
//! parked on an external completion simply never gets classified, and nothing
//! reports it (the 2026-09-14 restart: six comatose lanes, `lanes inside the
//! window = 0`).
//!
//! Bindings INNER JOIN `sessions`, so a session row is required before an
//! upsert will stick.

use crate::channels::telegram::resume::WAKE_RECENT_SECS;
use crate::db::models::Session;
use crate::db::{BindingOrigin, Database, SessionBindingRepository, SessionRepository};
use uuid::Uuid;

async fn test_db() -> Database {
    let db = Database::connect_in_memory()
        .await
        .expect("Failed to create database");
    db.run_migrations().await.expect("Failed to run migrations");
    db
}

async fn bind(db: &Database, session: Uuid, chat: &str, thread: Option<i32>) -> String {
    SessionRepository::new(db.pool().clone())
        .create(&Session {
            id: session,
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
        .expect("Failed to create session row");
    let sid = session.to_string();
    SessionBindingRepository::new(db.pool().clone())
        .upsert(
            sid.clone(),
            "telegram",
            chat,
            thread,
            BindingOrigin::Text,
        )
        .await
        .expect("Failed to upsert binding");
    sid
}

/// Run raw SQL against the pool — used to backdate timestamps, which the
/// repository deliberately does not expose.
async fn raw_exec(db: &Database, sql: &str, session_id: &str, age_secs: i64) {
    let pool = db.pool().clone();
    let sid = session_id.to_string();
    let query = sql.to_string();
    let conn = pool.get().await.expect("Failed to get connection");
    conn.interact(move |conn| {
        conn.execute(
            &query,
            rusqlite::params![sid, age_secs],
        )
    })
    .await
    .expect("interact failed")
    .expect("execute failed");
}

/// THE acceptance criterion: a binding whose `updated_at` is older than
/// `WAKE_RECENT_SECS` is invisible to `recent_for_channel` but still returned
/// by `awaiting_for_channel`. If this ever flips to "0 rows", the fix has
/// become a widening of the freshness gate rather than an OR-path beside it.
#[tokio::test]
async fn awaiting_survives_the_freshness_gate_that_hides_the_binding() {
    let db = test_db().await;
    let sid = bind(&db, Uuid::new_v4(), "-100344", Some(344)).await;
    let repo = SessionBindingRepository::new(db.pool().clone());

    repo.set_await(&sid, "ci_run", Some("35423208985"))
        .await
        .expect("set_await failed");
    // Parked 12 h ago — an order of magnitude past the 3600 s gate.
    raw_exec(
        &db,
        "UPDATE session_bindings SET updated_at = strftime('%s','now') - ?2 WHERE session_id = ?1",
        &sid,
        12 * 3600,
    )
    .await;

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .expect("clock before epoch");

    // The gate hides it.
    let recent = repo
        .recent_for_channel("telegram", now - WAKE_RECENT_SECS)
        .await
        .expect("recent_for_channel failed");
    assert_eq!(
        recent.len(),
        0,
        "a binding updated 12 h ago must be OUTSIDE the freshness gate \
         (if this fails the test setup changed, not the fix)"
    );

    // The await path still finds it.
    let awaiting = repo
        .awaiting_for_channel("telegram")
        .await
        .expect("awaiting_for_channel failed");
    assert_eq!(
        awaiting.len(),
        1,
        "await_at IS NOT NULL must be readable regardless of updated_at"
    );
    assert_eq!(awaiting[0].session_id, sid);
    assert_eq!(awaiting[0].await_kind.as_deref(), Some("ci_run"));
    assert_eq!(awaiting[0].await_ref.as_deref(), Some("35423208985"));
    assert!(awaiting[0].await_at.is_some(), "await_at must be stamped");
    assert!(
        awaiting[0].is_awaiting(),
        "is_awaiting must agree with await_at IS NOT NULL"
    );
    assert_eq!(awaiting[0].thread_id, Some(344), "route must survive intact");
}

/// `set` makes the row visible to `awaiting_for_channel`, `clear` makes it not —
/// and a NULL `await_at` reads as "not awaiting" so pre-feature rows keep their
/// classification.
#[tokio::test]
async fn set_then_clear_await_round_trip() {
    let db = test_db().await;
    let sid = bind(&db, Uuid::new_v4(), "-100345", Some(345)).await;
    let repo = SessionBindingRepository::new(db.pool().clone());

    // Never awaited: absent, and `await_at` NULL.
    assert!(
        repo.awaiting_for_channel("telegram").await.unwrap().is_empty(),
        "a binding with no await record must not be returned"
    );

    repo.set_await(&sid, "peer_lane", Some("8b278a4f"))
        .await
        .expect("set_await failed");
    let after_set = repo.awaiting_for_channel("telegram").await.unwrap();
    assert_eq!(after_set.len(), 1, "set must make the row visible");

    repo.clear_await(&sid).await.expect("clear_await failed");
    assert!(
        repo.awaiting_for_channel("telegram").await.unwrap().is_empty(),
        "clear must make the row invisible again"
    );
    let all = repo.all_for_channel("telegram").await.unwrap();
    assert_eq!(all.len(), 1, "the binding itself must survive a clear");
    assert!(all[0].await_kind.is_none());
    assert!(all[0].await_ref.is_none());
    assert!(all[0].await_at.is_none());
}

/// Re-declaring a wait replaces the kind/ref rather than stacking — a lane that
/// moves from one CI run to the next must not leave the old handle behind.
#[tokio::test]
async fn set_await_is_idempotent_and_replaces_the_handle() {
    let db = test_db().await;
    let sid = bind(&db, Uuid::new_v4(), "-100346", Some(346)).await;
    let repo = SessionBindingRepository::new(db.pool().clone());

    repo.set_await(&sid, "ci_run", Some("run-1")).await.unwrap();
    repo.set_await(&sid, "ci_run", Some("run-2")).await.unwrap();

    let awaiting = repo.awaiting_for_channel("telegram").await.unwrap();
    assert_eq!(awaiting.len(), 1, "re-declaring must not duplicate the row");
    assert_eq!(awaiting[0].await_ref.as_deref(), Some("run-2"));
}

/// A wait with no handle is legal — the owner-gate case names no identifier.
#[tokio::test]
async fn await_ref_may_be_null() {
    let db = test_db().await;
    let sid = bind(&db, Uuid::new_v4(), "-100347", Some(347)).await;
    let repo = SessionBindingRepository::new(db.pool().clone());

    repo.set_await(&sid, "owner_gate", None).await.unwrap();

    let awaiting = repo.awaiting_for_channel("telegram").await.unwrap();
    assert_eq!(awaiting.len(), 1);
    assert_eq!(awaiting[0].await_kind.as_deref(), Some("owner_gate"));
    assert!(awaiting[0].await_ref.is_none(), "no handle is legal");
}

/// Re-binding a session that is parked on an external completion must NOT erase
/// its wait. `upsert`'s column lists are explicit and name no `await_*` column;
/// this test is what makes that property load-bearing rather than incidental.
#[tokio::test]
async fn upsert_never_clobbers_an_existing_await_record() {
    let db = test_db().await;
    let session = Uuid::new_v4();
    let sid = bind(&db, session, "-100348", Some(348)).await;
    let repo = SessionBindingRepository::new(db.pool().clone());

    repo.set_await(&sid, "ci_run", Some("run-9")).await.unwrap();

    // The session moves to another topic — a routine re-bind.
    repo.upsert(sid.clone(), "telegram", "-100349", Some(349), BindingOrigin::Callback)
        .await
        .expect("upsert failed");

    let awaiting = repo.awaiting_for_channel("telegram").await.unwrap();
    assert_eq!(
        awaiting.len(),
        1,
        "a re-bind must not silently drop the wait"
    );
    assert_eq!(awaiting[0].await_kind.as_deref(), Some("ci_run"));
    assert_eq!(awaiting[0].await_ref.as_deref(), Some("run-9"));
    assert_eq!(awaiting[0].thread_id, Some(349), "the new route must win");
}

/// `set_await` must NOT refresh `updated_at`: that column is the freshness
/// gate's input, and bumping it would keep an awaiting lane artificially fresh
/// and hide the very stall this column exists to catch.
#[tokio::test]
async fn set_await_does_not_refresh_updated_at() {
    let db = test_db().await;
    let sid = bind(&db, Uuid::new_v4(), "-100350", Some(350)).await;
    let repo = SessionBindingRepository::new(db.pool().clone());

    raw_exec(
        &db,
        "UPDATE session_bindings SET updated_at = strftime('%s','now') - ?2 WHERE session_id = ?1",
        &sid,
        6 * 3600,
    )
    .await;
    let before = repo.all_for_channel("telegram").await.unwrap()[0].await_at;
    assert!(before.is_none(), "precondition: not awaiting yet");

    repo.set_await(&sid, "ci_run", Some("run-3")).await.unwrap();

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .expect("clock before epoch");
    let recent = repo
        .recent_for_channel("telegram", now - WAKE_RECENT_SECS)
        .await
        .unwrap();
    assert_eq!(
        recent.len(),
        0,
        "declaring a wait must not make a stale binding fresh again"
    );
    assert_eq!(
        repo.awaiting_for_channel("telegram").await.unwrap().len(),
        1,
        "but the wait must still be readable"
    );
}

/// Oldest wait first, so a sweep processes the longest-parked lane before the
/// one that just closed its turn.
#[tokio::test]
async fn awaiting_is_ordered_oldest_wait_first() {
    let db = test_db().await;
    let newer = bind(&db, Uuid::new_v4(), "-100351", Some(351)).await;
    let older = bind(&db, Uuid::new_v4(), "-100352", Some(352)).await;
    let repo = SessionBindingRepository::new(db.pool().clone());

    repo.set_await(&newer, "ci_run", Some("run-new")).await.unwrap();
    repo.set_await(&older, "ci_run", Some("run-old")).await.unwrap();
    // Backdate `await_at` itself — strftime has 1 s resolution, so two sets in
    // the same second cannot be ordered reliably.
    raw_exec(
        &db,
        "UPDATE session_bindings SET await_at = await_at - ?2 WHERE session_id = ?1",
        &older,
        7200,
    )
    .await;

    let awaiting = repo.awaiting_for_channel("telegram").await.unwrap();
    assert_eq!(awaiting.len(), 2);
    assert_eq!(
        awaiting[0].session_id, older,
        "ORDER BY await_at ASC must put the longest wait first"
    );
    assert_eq!(awaiting[1].session_id, newer);
}

/// The await path is channel-scoped: a wait on one surface must not wake a
/// binding on another.
#[tokio::test]
async fn awaiting_is_scoped_to_the_channel() {
    let db = test_db().await;
    let sid = bind(&db, Uuid::new_v4(), "-100353", Some(353)).await;
    let repo = SessionBindingRepository::new(db.pool().clone());

    repo.set_await(&sid, "ci_run", Some("run-4")).await.unwrap();

    assert_eq!(repo.awaiting_for_channel("telegram").await.unwrap().len(), 1);
    assert!(
        repo.awaiting_for_channel("discord").await.unwrap().is_empty(),
        "another channel must see nothing"
    );
}

/// `clear_await` on a session that never awaited is a safe no-op, not an error —
/// the sweep and the tool both call it defensively.
#[tokio::test]
async fn clear_await_on_a_non_awaiting_binding_is_a_noop() {
    let db = test_db().await;
    let sid = bind(&db, Uuid::new_v4(), "-100354", Some(354)).await;
    let repo = SessionBindingRepository::new(db.pool().clone());

    repo.clear_await(&sid).await.expect("clear must not error");
    repo.clear_await("no-such-session")
        .await
        .expect("clear on a missing binding must not error");
    assert!(repo.awaiting_for_channel("telegram").await.unwrap().is_empty());
}
