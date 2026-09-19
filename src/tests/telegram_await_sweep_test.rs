//! The runtime await sweep wakes a lane whose declared wait outlived the thing
//! it waited for — exactly once per declaration (#344).
//!
//! The boot classifier only runs on a RESTART, so a lane parked on a CI run
//! that dies inside a healthy daemon waits forever. These tests drive a REAL
//! sweep pass against a temp DB, with a recorder standing in for the live bot:
//! the selection, the staleness boundary, the single-flight guard and the
//! consumption of the await record are all exercised through `run_once` rather
//! than re-implemented here, because a test that re-implements the selection
//! proves nothing about the sweep.
//!
//! What is NOT exercised is `resume::spawn_resumes`, which the pass hands its
//! batch to: it waits for a Telegram bot and drives a real turn. Stated rather
//! than hidden, per the same reasoning as `memory_backfill_sweep_test`.

use crate::channels::telegram::await_sweep::{
    interval_for, leave, run_once, stale_bindings, try_enter,
};
use crate::channels::telegram::resume::ResumeTargets;
use crate::config::TelegramConfig;
use crate::db::{BindingOrigin, Database, Session, SessionBinding, SessionBindingRepository, SessionRepository};
use uuid::Uuid;

/// The single-flight flag is process-global, so two passes running in parallel
/// would see each other's slot and one would silently skip — a flake, not a
/// finding. Serialize the suite, and release the slot on entry so a test that
/// failed while holding it cannot poison the rest.
async fn guard() -> tokio::sync::MutexGuard<'static, ()> {
    static GUARD: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    let g = GUARD.lock().await;
    leave();
    g
}

async fn test_db() -> Database {
    let db = Database::connect_in_memory().await.unwrap();
    db.run_migrations().await.unwrap();
    db
}

/// Bind a session the way ingress does. The `sessions` row is required, not
/// incidental: `awaiting_for_channel` INNER-JOINs it, so a binding without one
/// is invisible to the sweep by construction.
async fn bind(db: &Database, sid: Uuid, chat: &str, thread: Option<i32>) {
    SessionRepository::new(db.pool().clone())
        .create(&Session {
            id: sid,
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
        .unwrap();
    SessionBindingRepository::new(db.pool().clone())
        .upsert(
            sid.to_string(),
            "telegram",
            chat,
            thread,
            BindingOrigin::Text,
        )
        .await
        .unwrap();
}

/// The lane declares what it is parked on — the A2 write path, without the tool.
async fn declare_await(db: &Database, sid: Uuid, kind: &str, reference: Option<&str>) {
    SessionBindingRepository::new(db.pool().clone())
        .set_await(&sid.to_string(), kind, reference)
        .await
        .unwrap();
}

/// Age the await record, so the staleness boundary can be crossed without a
/// timer. Mirrors `backdate_binding` in `boot_classifier_test`.
async fn backdate_await(db: &Database, sid: Uuid, age_secs: i64) {
    let pool = db.pool().clone();
    let s = sid.to_string();
    let affected = {
        let conn = pool.get().await.unwrap();
        conn.interact(move |conn| {
            conn.execute(
                "UPDATE session_bindings \
                 SET await_at = strftime('%s','now') - ?2 \
                 WHERE session_id = ?1",
                rusqlite::params![s, age_secs],
            )
        })
        .await
        .unwrap()
        .unwrap()
    };
    assert_eq!(affected, 1, "backdate must touch exactly one binding row");
}

async fn awaiting_rows(db: &Database) -> usize {
    SessionBindingRepository::new(db.pool().clone())
        .awaiting_for_channel("telegram")
        .await
        .unwrap()
        .len()
}

fn binding(await_at: Option<i64>) -> SessionBinding {
    SessionBinding {
        session_id: Uuid::new_v4().to_string(),
        channel: "telegram".to_string(),
        chat_id: "-100123".to_string(),
        thread_id: Some(249),
        last_origin: None,
        turn_open_at: None,
        await_kind: Some("ci_run".to_string()),
        await_ref: Some("377".to_string()),
        await_at,
    }
}

// ---------------------------------------------------------------- interval

#[test]
fn the_default_interval_is_five_minutes() {
    // Chosen against the failure it fixes: a lane parked on a dead run should
    // be recovered inside the same working session, not at the next restart.
    assert_eq!(
        interval_for(&TelegramConfig::default()).map(|d| d.as_secs()),
        Some(300)
    );
}

#[test]
fn the_default_patience_is_one_hour() {
    // Must sit well clear of a normal CI run or peer-lane round trip, or the
    // sweep pre-empts waits that were about to resolve on their own.
    assert_eq!(TelegramConfig::default().await_stale_secs, 3600);
}

#[test]
fn an_explicit_interval_is_honoured() {
    let cfg = TelegramConfig {
        await_sweep_interval_secs: 60,
        ..Default::default()
    };
    assert_eq!(interval_for(&cfg).map(|d| d.as_secs()), Some(60));
}

#[test]
fn zero_disables_the_sweep() {
    let cfg = TelegramConfig {
        await_sweep_interval_secs: 0,
        ..Default::default()
    };
    assert_eq!(interval_for(&cfg), None);
}

// -------------------------------------------------------- staleness boundary

#[test]
fn a_wait_exactly_at_the_boundary_is_not_stale() {
    // Strictly older-than, so the knob means what it says rather than firing
    // one second early.
    let rows = stale_bindings(vec![binding(Some(1_000))], 4_600, 3_600);
    assert!(rows.is_empty(), "3600s of patience is not yet 3601s");
}

#[test]
fn a_wait_past_the_boundary_is_stale() {
    let rows = stale_bindings(vec![binding(Some(1_000))], 4_601, 3_600);
    assert_eq!(rows.len(), 1, "one second past the boundary is stale");
}

#[test]
fn a_wait_stamped_in_the_future_is_not_stale() {
    // A clock stepped backwards must wake a lane LATE, never pre-empt a wait
    // that has barely begun.
    let rows = stale_bindings(vec![binding(Some(9_999))], 4_600, 3_600);
    assert!(rows.is_empty());
}

#[test]
fn a_binding_without_an_await_record_is_never_stale() {
    // The SQL predicate already excludes these; restating it here is what keeps
    // the filter correct on its own terms rather than only behind its caller.
    let rows = stale_bindings(vec![binding(None)], 99_999, 3_600);
    assert!(rows.is_empty());
}

// ------------------------------------------------------------ sweep passes

#[tokio::test]
async fn a_stale_await_is_woken_exactly_once() {
    let _g = guard().await;
    let db = test_db().await;
    let pool = db.pool().clone();

    let parked = Uuid::new_v4();
    bind(&db, parked, "-100123", Some(249)).await;
    declare_await(&db, parked, "ci_run", Some("377")).await;
    backdate_await(&db, parked, 7_200).await;

    // A companion lane with no await record, equally old. Nothing may wake it:
    // this is the "do not widen the freshness gate" half of the design.
    let idle = Uuid::new_v4();
    bind(&db, idle, "-100456", Some(250)).await;

    let mut batches: Vec<ResumeTargets> = Vec::new();
    let woken = run_once(&pool, 3_600, |t| batches.push(t)).await;

    assert_eq!(woken, 1, "one stale await must produce exactly one wake");
    assert_eq!(batches.len(), 1, "the wake must be handed over exactly once");
    assert_eq!(
        batches[0],
        vec![(parked, -100123, Some(249))],
        "the wake must route to the parked lane, and only to it"
    );
    assert_eq!(
        awaiting_rows(&db).await,
        0,
        "the record is consumed by the wake, so it cannot fire again next tick"
    );
}

#[tokio::test]
async fn a_second_pass_over_a_consumed_record_wakes_nobody() {
    let _g = guard().await;
    let db = test_db().await;
    let pool = db.pool().clone();

    let parked = Uuid::new_v4();
    bind(&db, parked, "-100123", Some(249)).await;
    declare_await(&db, parked, "ci_run", Some("377")).await;
    backdate_await(&db, parked, 7_200).await;

    let first = run_once(&pool, 3_600, |_| {}).await;
    assert_eq!(first, 1, "the first pass wakes the parked lane");

    let mut second: Vec<ResumeTargets> = Vec::new();
    let again = run_once(&pool, 3_600, |t| second.push(t)).await;
    assert_eq!(again, 0, "a consumed record must not wake the lane twice");
    assert!(second.is_empty(), "no batch may be handed over on the second pass");
}

#[tokio::test]
async fn a_fresh_await_is_left_alone_and_keeps_its_record() {
    let _g = guard().await;
    let db = test_db().await;
    let pool = db.pool().clone();

    let parked = Uuid::new_v4();
    bind(&db, parked, "-100123", Some(249)).await;
    declare_await(&db, parked, "ci_run", Some("377")).await;

    let mut batches: Vec<ResumeTargets> = Vec::new();
    let woken = run_once(&pool, 3_600, |t| batches.push(t)).await;

    assert_eq!(woken, 0, "a wait inside its patience is not stale");
    assert!(batches.is_empty());
    assert_eq!(
        awaiting_rows(&db).await,
        1,
        "an unexpired wait keeps its record for the pass that will need it"
    );
}

#[tokio::test]
async fn an_await_on_another_channel_is_not_this_sweep_s_business() {
    let _g = guard().await;
    let db = test_db().await;
    let pool = db.pool().clone();

    let sid = Uuid::new_v4();
    bind(&db, sid, "-100123", Some(249)).await;
    declare_await(&db, sid, "ci_run", Some("377")).await;
    backdate_await(&db, sid, 7_200).await;

    // Re-point the binding at a different channel. The sweep serves Telegram
    // only, and the record is a property of the binding, so this lane must not
    // be woken into a surface it no longer lives on.
    {
        let s = sid.to_string();
        let conn = pool.get().await.unwrap();
        conn.interact(move |conn| {
            conn.execute(
                "UPDATE session_bindings SET channel = 'discord' WHERE session_id = ?1",
                rusqlite::params![s],
            )
        })
        .await
        .unwrap()
        .unwrap();
    }

    let woken = run_once(&pool, 3_600, |_| {}).await;
    assert_eq!(woken, 0, "only the sweep's own channel is served");
}

// ------------------------------------------------------------- single flight

#[test]
fn a_second_pass_skips_while_the_first_holds_the_slot() {
    // Two passes over the same rows would select the same lane twice, and the
    // second selection would wake a lane the first one is already waking.
    assert!(try_enter(), "the slot must be free to begin with");
    assert!(!try_enter(), "a second entry must be refused, not queued");
    leave();
    assert!(try_enter(), "the slot must be reusable after release");
    leave();
}
