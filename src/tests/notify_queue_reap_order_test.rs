//! The age reap must run AFTER the redelivery pass, never before (#1617).
//!
//! `reap_stale_unclaimed` deletes by `created_at` alone: its SQL carries no
//! claim or route predicate, so "unclaimed" is true only because the caller
//! has already offered every row to a live route and kept what re-parked.
//! Run before the offer, the same query eats live pushes after any long
//! downtime — a sleeping laptop, a machine off over a holiday — which is the
//! one outcome this module forbids ("a DUPLICATE after restart, never a lost
//! message", `notify_queue.rs` module doc).
//!
//! Two layers pin it. The data layer proves `all()` is age-blind, so nothing
//! below the caller protects an old-but-live row. The source sentinel proves
//! the call order inside `redeliver_persisted`, which cannot be exercised
//! directly: it resolves its pool through the process-wide `global_pool()`
//! `OnceLock` that only a real `Database::connect` sets.
//!
//! #931 adds the cadence half: the pass used to run once per boot, so the
//! ceiling was unenforced for the whole uptime of a long-lived daemon. The
//! periodic tick must therefore run the WHOLE pass and never the bare reap —
//! the sentinel below is what catches that, because a test that only proves
//! "a 73h row disappears" passes on the data-loss version too.

use crate::brain::agent::service::notify_queue;
use crate::brain::agent::service::restart_recovery::deliver_or_park;
use crate::brain::agent::service::session_routes;
use crate::db::Database;
use crate::db::repository::NotifyQueueRepository;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use uuid::Uuid;

const SERVICE_SRC: &str = include_str!("../brain/agent/service/notify_queue.rs");

async fn setup() -> (NotifyQueueRepository, Database) {
    let db = Database::connect_in_memory()
        .await
        .expect("in-memory database");
    db.run_migrations().await.expect("migrations");
    (NotifyQueueRepository::new(db.pool().clone()), db)
}

#[tokio::test]
async fn redelivery_read_is_age_blind_so_only_call_order_protects_an_old_live_row() {
    let (repo, db) = setup().await;
    let now = 1_700_000_000i64;
    let session = Uuid::new_v4();
    let id = Uuid::new_v4();
    let sess_str = session.to_string();
    let id_str = id.to_string();

    // A push parked 100h ago for a session whose route comes back at boot:
    // the machine was simply off for four days.
    raw(&db, move |conn| {
        conn.execute(
            "INSERT INTO notify_queue \
             (id, session_id, context_text, display_text, origin, bg_meta, created_at) \
             VALUES (?1, ?2, 'four days parked', 'four days parked', 'session_notify', NULL, ?3)",
            rusqlite::params![id_str, sess_str, now - 100 * 3600],
        )
    })
    .await;

    // The redelivery pass reads through `all()`, which applies no age filter:
    // the row is still offered no matter how old it is. That is the whole
    // reason the reap has to wait until after the offer.
    let offered = repo.all().await.expect("all");
    assert_eq!(
        offered.len(),
        1,
        "all() must surface the 100h row for redelivery, it is age-blind by design"
    );
    assert_eq!(offered[0].id, id);

    // Only once that offer has happened does reaping it become correct.
    let reaped = repo
        .reap_stale_unclaimed(now - 72 * 3600)
        .await
        .expect("reap");
    assert_eq!(reaped.len(), 1, "after the offer, the same row is reapable");
    assert_eq!(reaped[0].id, id);
}

#[test]
fn reap_call_sits_after_the_deliver_or_park_loop() {
    // The offer is a parameter of `redeliver_pass_with` (so the pass can be
    // driven from a test at all); production is pinned to `deliver_or_park`
    // by the assertion below, and the offer site keeps the same shape.
    assert!(
        SERVICE_SRC
            .contains("redeliver_pass_with(&repo, super::restart_recovery::deliver_or_park)"),
        "production must offer rows through deliver_or_park, the one function that \
         consults the session route registry"
    );
    let deliver = SERVICE_SRC
        .find("offer(row.session_id, msg)")
        .expect("redeliver_pass_with must still offer rows through the injected offer");
    let reap = SERVICE_SRC
        .find("reap_stale_after_redelivery(repo).await")
        .expect("redeliver_pass_with must still call the age reap");
    assert!(
        reap > deliver,
        "the age reap must be called AFTER the redelivery loop: moved before it, \
         a 72h+ row for a live session is deleted instead of delivered"
    );
}

#[test]
fn dead_session_reap_stays_before_the_loop() {
    // The other reaper is safe to run first and belongs first: a row whose
    // session is gone can never be claimed, so offering it only re-parks a
    // push nobody can receive.
    let dead = SERVICE_SRC
        .find("repo.clear_dead_sessions().await")
        .expect("redeliver_persisted must still reap rows for dead sessions");
    let read = SERVICE_SRC
        .find("repo.all().await")
        .expect("redeliver_persisted must still read the rows it offers");
    assert!(
        dead < read,
        "the dead-session reap belongs before the read: its rows are unclaimable"
    );
}

async fn raw<F>(db: &Database, f: F)
where
    F: FnOnce(&rusqlite::Connection) -> rusqlite::Result<usize> + Send + 'static,
{
    db.pool()
        .get()
        .await
        .expect("pool connection")
        .interact(move |conn| f(conn))
        .await
        .expect("interact")
        .expect("raw sql");
}

// ---------------------------------------------------------------------------
// #931 — the pass must also run on a cadence, not only at boot.
//
// A boot-only pass left the 72h ceiling unenforced for the whole uptime of a
// long-lived daemon (measured: rows 6.78h past the ceiling, daemon up 11.59h,
// NRestarts=0). The fix spawns the WHOLE pass hourly. The trap is that the
// obvious cheaper fix — calling `reap_stale_after_redelivery` on a timer — is
// data loss: a row is "unclaimed" only because the pass just offered it and
// `deliver_or_park` re-parked it. So the tests below prove, in order: the
// periodic tick runs the pass and never the bare reap (source sentinel), the
// first tick waits a full interval (source sentinel), the guard is
// single-flight and released on panic, and each row class behaves as the
// ordering contract says.
// ---------------------------------------------------------------------------

const RECOVERY_SRC: &str = include_str!("../brain/agent/service/restart_recovery.rs");

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

async fn insert_session(db: &Database, session: Uuid, now: i64) {
    let id = session.to_string();
    raw(db, move |conn| {
        conn.execute(
            "INSERT INTO sessions (id, created_at, updated_at) VALUES (?1, ?2, ?2)",
            rusqlite::params![id, now],
        )
    })
    .await;
}

async fn insert_row(db: &Database, id: Uuid, session: Uuid, text: &str, created_at: i64) {
    let (id, session, text) = (id.to_string(), session.to_string(), text.to_string());
    raw(db, move |conn| {
        conn.execute(
            "INSERT INTO notify_queue \
             (id, session_id, context_text, display_text, origin, bg_meta, created_at) \
             VALUES (?1, ?2, ?3, ?3, 'session_notify', NULL, ?4)",
            rusqlite::params![id, session, text, created_at],
        )
    })
    .await;
}

#[test]
fn the_cadence_is_hourly_and_the_ceiling_is_still_72h() {
    // The ceiling is a product decision (#182) and #931 changes only whether
    // anything RUNS the pass — not how old a row may get.
    assert_eq!(
        notify_queue::MAX_ROW_AGE_SECS,
        72 * 3600,
        "the 72h ceiling must not move: #931 is about enforcement, not policy"
    );
    assert_eq!(
        notify_queue::NOTIFY_REAP_INTERVAL_SECS,
        3600,
        "the periodic pass runs hourly, the unit row ages are expressed in"
    );
}

#[test]
fn the_periodic_tick_runs_the_whole_pass_and_never_the_bare_reap() {
    // The tick itself.
    let tick_start = SERVICE_SRC
        .find("pub(crate) async fn run_periodic_pass")
        .expect("the periodic entry point must exist in notify_queue.rs");
    let tick_end = SERVICE_SRC[tick_start..]
        .find("\n}")
        .expect("run_periodic_pass must be a whole function")
        + tick_start;
    let tick = &SERVICE_SRC[tick_start..tick_end];
    assert!(
        tick.contains("redeliver_persisted().await"),
        "the periodic tick must run the WHOLE redelivery pass: the reap is only \
         correct as its tail"
    );
    assert!(
        !tick.contains("reap_stale_after_redelivery"),
        "the periodic tick must NEVER call the bare reap: every row it deletes must \
         have just been offered and re-parked, or a long downtime deletes live pushes"
    );

    // ...and nothing in restart_recovery may reach for the reap directly.
    assert!(
        !RECOVERY_SRC.contains("reap_stale_after_redelivery"),
        "the spawned loop must go through run_periodic_pass, never the bare reap"
    );
    assert!(
        RECOVERY_SRC.contains("run_periodic_pass().await"),
        "recover() must spawn the periodic pass"
    );
}

#[test]
fn the_first_periodic_tick_waits_a_full_interval() {
    let spawn = RECOVERY_SRC
        .find("let _periodic_notify_reap = tokio::spawn")
        .expect("recover() must spawn the periodic loop");
    let body = &RECOVERY_SRC[spawn..];
    let sleep = body
        .find("tokio::time::sleep")
        .expect("the loop must sleep between ticks");
    let pass = body
        .find("run_periodic_pass().await")
        .expect("the loop must run the pass");
    assert!(
        sleep < pass,
        "the first tick must wait one full interval: recover() ran a pass \
         microseconds earlier, so an immediate tick only repeats it"
    );
    assert!(
        body[..pass].contains("NOTIFY_REAP_INTERVAL_SECS"),
        "the interval must come from the named constant, not a bare literal"
    );
}

#[test]
fn two_overlapping_ticks_cannot_both_run_a_pass() {
    let gate = AtomicBool::new(false);
    let first = notify_queue::try_enter_pass(&gate).expect("the first tick takes the permit");
    assert!(
        notify_queue::try_enter_pass(&gate).is_none(),
        "a second tick that overlaps a running pass must be REFUSED: two concurrent \
         passes deliver the same rows twice"
    );
    drop(first);
    assert!(
        notify_queue::try_enter_pass(&gate).is_some(),
        "the permit must be released when the pass ends, or the ticker dies forever"
    );
}

#[test]
fn a_panicking_pass_still_releases_the_single_flight_permit() {
    let gate = AtomicBool::new(false);
    let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _held = notify_queue::try_enter_pass(&gate).expect("permit");
        panic!("simulated panic inside a redelivery pass");
    }));
    assert!(caught.is_err(), "the panic must propagate");
    assert!(
        notify_queue::try_enter_pass(&gate).is_some(),
        "the RAII permit must be released on panic: a stranded flag turns one \
         transient overlap into a permanently dead ticker"
    );
}

#[tokio::test]
async fn a_72h_row_whose_session_is_gone_is_reaped_without_being_offered() {
    let (repo, db) = setup().await;
    let now = now_unix();
    // No `sessions` row: nothing can ever claim this push.
    insert_row(
        &db,
        Uuid::new_v4(),
        Uuid::new_v4(),
        "orphaned 100h push",
        now - 100 * 3600,
    )
    .await;

    let offers = AtomicUsize::new(0);
    let redelivered = notify_queue::redeliver_pass_with(&repo, |_, _| {
        offers.fetch_add(1, Ordering::SeqCst);
        false
    })
    .await;

    assert_eq!(
        offers.load(Ordering::SeqCst),
        0,
        "a dead-session row is reaped by clear_dead_sessions BEFORE the offer loop"
    );
    assert_eq!(redelivered, 0);
    assert!(
        repo.all().await.expect("all").is_empty(),
        "the periodic pass must clear a row whose session no longer exists"
    );
}

#[tokio::test]
async fn a_72h_row_with_a_live_route_is_delivered_and_cleared_not_reaped() {
    let (repo, db) = setup().await;
    let now = now_unix();
    let session = Uuid::new_v4();
    insert_session(&db, session, now).await;
    insert_row(
        &db,
        Uuid::new_v4(),
        session,
        "four days parked, route came back",
        now - 100 * 3600,
    )
    .await;

    // The REAL production offer, against a real registered route: this is the
    // "sleeping laptop" case the ordering contract exists to protect.
    let seen = Arc::new(Mutex::new(Vec::<String>::new()));
    let sink = seen.clone();
    session_routes::register_session_route(
        session,
        Arc::new(move |_sid, msg| {
            sink.lock().expect("receipt sink").push(msg.context_text);
        }),
    );

    let redelivered = notify_queue::redeliver_pass_with(&repo, deliver_or_park).await;

    assert_eq!(
        redelivered, 1,
        "the 100h row must still be OFFERED: a pass that reaped by age first would \
         return 0 here, which is the data-loss regression"
    );
    assert_eq!(
        seen.lock().expect("receipt sink").len(),
        1,
        "the live route must RECEIVE the 100h push instead of having it reaped"
    );
    assert!(
        repo.all().await.expect("all").is_empty(),
        "a delivered row is cleared by clear_batch, not left for the reaper"
    );
}

#[tokio::test]
async fn a_72h_row_for_an_unrouted_session_is_offered_and_only_then_reaped() {
    let (repo, db) = setup().await;
    let now = now_unix();
    let session = Uuid::new_v4();
    // The session EXISTS, so clear_dead_sessions leaves it alone; it simply has
    // no route, which is the headless/cron/archived-topic class.
    insert_session(&db, session, now).await;
    insert_row(
        &db,
        Uuid::new_v4(),
        session,
        "no route will ever claim this",
        now - 100 * 3600,
    )
    .await;

    let offers = AtomicUsize::new(0);
    let redelivered = notify_queue::redeliver_pass_with(&repo, |_, _| {
        offers.fetch_add(1, Ordering::SeqCst);
        false // deliver_or_park's "re-parked" answer
    })
    .await;

    assert_eq!(
        offers.load(Ordering::SeqCst),
        1,
        "the row must be OFFERED before it is reaped: an offer-less reap is exactly \
         the data loss this ordering forbids"
    );
    assert_eq!(redelivered, 1);
    assert!(
        repo.all().await.expect("all").is_empty(),
        "past the ceiling, a row that parked again is reaped by the tail of the pass"
    );
}
