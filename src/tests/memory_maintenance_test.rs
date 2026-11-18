//! Tests for memory garbage collection, orphan pruning, and maintenance sweeps (#241).

use crate::brain::agent::service::session_routes;
use crate::config::profile::with_home_override_async;
use crate::db::Database;
use crate::db::MaintenanceKnobs;
use crate::db::models::{Message, Session};
use crate::db::repository::{MessageRepository, SessionRepository};
use crate::memory::db::{MemoryGcReport, Store};
use crate::services::context::ServiceContext;
use crate::services::maintenance::{
    MAINTENANCE_CONTENDED_RETRY, MAX_CONSECUTIVE_FAILURE_RETRIES, MEMORY_RECLAIM_TICK,
    MaintenanceService, MaintenanceTiming, RECLAIM_WARMUP_OFFSET, SWEEP_WARMUP, SweepOutcome,
    enter_maintenance, leave, next_sweep_delay, note_subagent_catchup_done, reclaim_is_due,
    take_subagent_catchup, try_enter,
};
use crate::services::session::SessionService;
use tempfile::tempdir;

/// #531: the single-flight flag is process-global, so a sibling test mid-sweep
/// can hold the slot at the instant this one starts. Serialize the suite and
/// release the slot on entry, so a test that failed while holding it cannot
/// poison the rest.
async fn guard() -> tokio::sync::MutexGuard<'static, ()> {
    static GUARD: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    let g = GUARD.lock().await;
    leave();
    g
}

#[tokio::test]
async fn test_prune_expired_messages_via_session_service() {
    let db = Database::connect_in_memory().await.unwrap();
    db.run_migrations().await.unwrap();
    let session_repo = SessionRepository::new(db.pool().clone());
    let message_repo = MessageRepository::new(db.pool().clone());

    let session = Session::new(Some("t".to_string()), Some("m".to_string()), None);
    session_repo.create(&session).await.unwrap();

    let mut old_msg = Message::new(session.id, "user".into(), "old".into(), 1);
    old_msg.created_at = chrono::Utc::now() - chrono::Duration::days(120);
    message_repo.create(&old_msg).await.unwrap();

    let mut fresh_msg = Message::new(session.id, "user".into(), "fresh".into(), 2);
    fresh_msg.created_at = chrono::Utc::now() - chrono::Duration::days(5);
    message_repo.create(&fresh_msg).await.unwrap();

    let ctx = ServiceContext::new(db.pool().clone());
    let svc = SessionService::new(ctx);

    // Retention disabled (0 days) -> no-op
    let pruned_disabled = svc.prune_expired_messages(0).await.unwrap();
    assert_eq!(pruned_disabled, 0);
    assert_eq!(
        message_repo
            .list_by_session(session.id)
            .await
            .unwrap()
            .len(),
        2
    );

    // Retention 90 days -> prunes old_msg
    let pruned = svc.prune_expired_messages(90).await.unwrap();
    assert_eq!(pruned, 1);
    let rem = message_repo.list_by_session(session.id).await.unwrap();
    assert_eq!(rem.len(), 1);
    assert_eq!(rem[0].content, "fresh");
}

#[test]
fn test_memory_gc_orphans() {
    let dir = tempdir().expect("tempdir");
    let db_path = dir.path().join("memory.db");
    let store = Store::open(&db_path).expect("open store");

    store.ensure_vector_table(4).expect("ensure vector table");

    // Insert content and documents
    store
        .insert_content("hash_active", "Active doc content", "2026-09-15T00:00:00Z")
        .expect("insert active content");
    store
        .insert_content("hash_orphan", "Orphan doc content", "2026-09-15T00:00:00Z")
        .expect("insert orphan content");

    store
        .insert_document(
            "brain",
            "ACTIVE.md",
            "Active Doc",
            "hash_active",
            "2026-09-15T00:00:00Z",
            "2026-09-15T00:00:00Z",
        )
        .expect("insert active doc");

    // Insert embedding for active and orphan
    store
        .insert_embedding(
            "hash_active",
            0,
            0,
            &[1.0, 2.0, 3.0, 4.0],
            "test-model",
            "2026-09-15T00:00:00Z",
            Some("hash_active_chunk"),
        )
        .expect("insert active embedding");
    store
        .insert_embedding(
            "hash_orphan",
            0,
            0,
            &[5.0, 6.0, 7.0, 8.0],
            "test-model",
            "2026-09-15T00:00:00Z",
            Some("hash_orphan_chunk"),
        )
        .expect("insert orphan embedding");

    // Pre-GC state check: content_vectors has 2 rows
    let stats_before = store.vector_stats().expect("stats before");
    assert_eq!(stats_before.vector_rows, 2);

    // Run GC
    let report: MemoryGcReport = store.gc_orphans().expect("gc_orphans");

    assert_eq!(report.orphaned_vectors_pruned, 1);
    assert_eq!(report.orphaned_embeddings_pruned, 1);
    assert_eq!(report.unreferenced_content_pruned, 1);

    // Post-GC stats: only active vector row remains
    let stats_after = store.vector_stats().expect("stats after");
    assert_eq!(stats_after.vector_rows, 1);

    // Running GC again is idempotent
    let report2 = store.gc_orphans().expect("gc_orphans again");
    assert_eq!(report2.orphaned_vectors_pruned, 0);
    assert_eq!(report2.orphaned_embeddings_pruned, 0);
    assert_eq!(report2.unreferenced_content_pruned, 0);

    // Test vacuum_memory: in-memory or empty freelist skips vacuum gracefully and returns Ok(false)
    let vacuumed = store.vacuum_memory().expect("vacuum_memory");
    assert!(!vacuumed);
}

#[tokio::test]
async fn test_maintenance_single_flight() {
    // #531: take the file guard first. It releases the slot on entry, so the
    // first assertion below is true by construction rather than by scheduler
    // luck, and a sibling test cannot hold the slot across this test's start.
    let _g = guard().await;
    assert!(try_enter());
    // Second entry fails while holding
    assert!(!try_enter());
    leave();
    // Re-entry succeeds after leave
    assert!(try_enter());
    leave();

    // #522: the sweep takes the RAII permit, not the bare try_enter/leave pair,
    // so an unwind inside the sweep body cannot wedge maintenance for the
    // process lifetime — which would silently disable the 24 h sweep AND the
    // reclaim ticker together. Dropping the permit must release the flag.
    //
    // These assertions live in THIS test rather than a second one on purpose:
    // they share one process-global flag, and (#531) the file guard now
    // serializes this suite anyway, so a second test would buy nothing.
    let permit = enter_maintenance().expect("first permit");
    assert!(
        enter_maintenance().is_none(),
        "a held permit must exclude a second"
    );
    drop(permit);
    let again = enter_maintenance().expect("dropping the permit must release the flag");
    // Explicitly released, so the flag is free for whatever runs next.
    drop(again);
}

/// Row count for one query, read on a SECOND connection to the memory store.
///
/// `Store` exposes no raw statement access, so this mirrors the house pattern
/// in `memory::store::clear_skipped_placeholders`: its own connection, with a
/// busy timeout, against the same WAL file.
fn memory_row_count(db_path: &std::path::Path, sql: &str) -> i64 {
    let conn = rusqlite::Connection::open(db_path).expect("open count connection");
    conn.busy_timeout(std::time::Duration::from_secs(30))
        .expect("busy_timeout");
    conn.query_row(sql, [], |r| r.get(0)).expect("count query")
}

/// Over the freelist cap the bulk memory GC is SKIPPED; under it the GC runs
/// and prunes (#522).
///
/// The `0` / `i64::MAX` cap injection is the file's own idiom
/// (`min_freelist_pages: 0` / `i64::MAX` in `sqlite_maintenance_test.rs`), and
/// it is what makes this fixture freelist-independent: `freelist_count() >= 0`
/// always holds, `>= i64::MAX` never does.
///
/// `run_maintenance_inner` is called directly — not `run_maintenance_with` — so
/// the test never contends for the process-global single-flight flag with a
/// test running in parallel.
#[tokio::test]
async fn test_memory_gc_skipped_over_freelist_cap() {
    let home = tempdir().expect("tempdir");

    with_home_override_async(home.path().to_path_buf(), async {
        // A `content` row with no matching `documents` row is an orphan for
        // `gc_orphans` step 3, which runs unconditionally.
        const ORPHAN: &str = "SELECT COUNT(*) FROM content WHERE hash = 'orphan-522'";
        let memory_db = crate::config::opencrabs_home()
            .join("memory")
            .join("memory.db");

        let db = Database::connect_in_memory().await.unwrap();
        db.run_migrations().await.unwrap();
        let svc = MaintenanceService::new(ServiceContext::new(db.pool().clone()));

        {
            // The store the sweep itself resolves, so the test drives the same
            // handle `run_maintenance_inner` takes. Guard scoped: holding it
            // across the sweep would deadlock the non-reentrant Mutex.
            let store_mutex = crate::memory::get_store().expect("get_store");
            let store = store_mutex.lock().expect("store lock");
            assert!(
                store.freelist_count() >= 0,
                "fixture precondition: cap 0 is always at or below the freelist"
            );
            store
                .insert_content("orphan-522", "orphan body", "2026-09-25T00:00:00Z")
                .expect("insert orphan content");
        }

        assert_eq!(
            memory_row_count(&memory_db, ORPHAN),
            1,
            "fixture must place the orphan before the sweep"
        );

        // Arm 1 — cap 0: the freelist is always at or above it, so the bulk GC
        // must stand down while the reclaim still runs.
        let over_cap = svc
            .run_maintenance_inner(MaintenanceKnobs {
                memory_gc_freelist_cap: 0,
                ..Default::default()
            })
            .await
            .expect("sweep over cap");
        assert!(
            over_cap.memory_gc_skipped,
            "freelist at or above the cap must skip the bulk GC"
        );
        assert_eq!(
            memory_row_count(&memory_db, ORPHAN),
            1,
            "a skipped GC must leave orphans in place"
        );

        // Arm 2 — cap i64::MAX: unreachable, so the GC runs and prunes.
        let under_cap = svc
            .run_maintenance_inner(MaintenanceKnobs {
                memory_gc_freelist_cap: i64::MAX,
                ..Default::default()
            })
            .await
            .expect("sweep under cap");
        assert!(
            !under_cap.memory_gc_skipped,
            "freelist below the cap must run the bulk GC"
        );
        assert_eq!(
            memory_row_count(&memory_db, ORPHAN),
            0,
            "a running GC must prune the orphan"
        );
    })
    .await;
}

/// The reclaim gate's contract, pinned without a clock (#522).
#[test]
fn test_memory_reclaim_due_predicate() {
    // Due once the quiet window elapses while nothing is in flight.
    assert!(!reclaim_is_due(
        false,
        std::time::Duration::from_secs(59),
        std::time::Duration::ZERO
    ));
    assert!(reclaim_is_due(
        false,
        std::time::Duration::from_secs(60),
        std::time::Duration::ZERO
    ));

    // A turn in flight defers — right up to the starvation cap, which fires
    // regardless so a permanently busy box cannot starve the reclaim.
    assert!(!reclaim_is_due(
        true,
        std::time::Duration::ZERO,
        std::time::Duration::from_secs(1799)
    ));
    assert!(reclaim_is_due(
        true,
        std::time::Duration::ZERO,
        std::time::Duration::from_secs(1800)
    ));

    // Long idle, but a live turn still defers.
    assert!(!reclaim_is_due(
        true,
        std::time::Duration::from_secs(600),
        std::time::Duration::ZERO
    ));
}

/// The activity clock advances when noted, and a note leaves a readable
/// instant (#522).
#[test]
fn test_activity_clock_advances() {
    // Deliberately race-tolerant: `LAST_ACTIVITY` is a process-global shared
    // with every other test in this binary, and a concurrent note can only move
    // it forward. Do NOT assert absence before the note — the ticker seeds the
    // clock at spawn, so an absolute-absence assertion would be flaky rather
    // than stronger.
    let before = session_routes::last_activity();
    session_routes::note_activity();
    let after = session_routes::last_activity();
    assert!(after.is_some(), "a note must leave a readable instant");
    assert!(after >= before, "the clock must never move backwards");
}

/// #836: `feedback_ledger` had no retention at all — 1.04M rows / 3.75 GiB on
/// a busy profile, and every `feedback_analyze` aggregate scanned the lot.
///
/// `created_at` is TEXT ISO8601 with a column default, so the fixture sets it
/// EXPLICITLY: a test that relied on the default could not tell an old row from
/// a fresh one, and would pass against a prune that deleted nothing.
#[tokio::test]
async fn test_prune_expired_feedback_ledger() {
    let _g = guard().await;
    let db = Database::connect_in_memory().await.unwrap();
    db.run_migrations().await.unwrap();

    let old_iso = (chrono::Utc::now() - chrono::Duration::days(120))
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string();
    let fresh_iso = (chrono::Utc::now() - chrono::Duration::days(5))
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string();

    // The in-memory pool is max_size(1), so the connection is released before
    // the prune (which needs its own) is called.
    let conn = db.pool().get().await.expect("pool connection");
    for (event_type, created_at) in [
        ("tool_failure".to_string(), old_iso.clone()),
        ("tool_success".to_string(), fresh_iso.clone()),
    ] {
        conn.interact(move |c| {
            c.execute(
                "INSERT INTO feedback_ledger (session_id, event_type, dimension, value, created_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                rusqlite::params!["sess-836", event_type, "bash", 1.0_f64, created_at],
            )
        })
        .await
        .expect("interact should succeed")
        .expect("insert should succeed");
    }
    drop(conn);

    let ctx = ServiceContext::new(db.pool().clone());
    let svc = MaintenanceService::new(ctx);

    // Retention disabled (0 days) -> no-op, both rows survive.
    let pruned_disabled = svc.prune_expired_feedback_ledger(0).await.unwrap();
    assert_eq!(pruned_disabled, 0, "0 days must disable pruning");

    let ledger_repo =
        crate::db::repository::feedback_ledger::FeedbackLedgerRepository::new(db.pool().clone());
    assert_eq!(
        ledger_repo.recent(10).await.unwrap().len(),
        2,
        "0 days must keep every row"
    );

    // Retention 90 days -> prunes exactly the 120-day-old row.
    let pruned = svc.prune_expired_feedback_ledger(90).await.unwrap();
    assert_eq!(pruned, 1, "the returned count must match the rows deleted");

    let remaining = ledger_repo.recent(10).await.unwrap();
    assert_eq!(remaining.len(), 1, "the fresh row must survive");
    assert_eq!(remaining[0].event_type, "tool_success");
}

/// #838: the 24 h sweep and the 5-min reclaim ticker are spawned in the same
/// bootstrap with the same 1800 s warmup, so their first wake is simultaneous
/// by construction and the ticker can take the shared permit first. Measured on
/// `ops`: the two calls were 32 µs apart, the sweep skipped, and the loop then
/// slept a full day — so every step inside it (message prune, ledger retention,
/// vacuums) was silently lost.
///
/// Two things must therefore hold: the skip must be VISIBLE to the caller, and
/// it must not cost the full interval.
#[tokio::test]
async fn test_contended_sweep_is_flagged_and_retries_soon() {
    let _g = guard().await;

    // The choice itself. A run that never happened waits the short retry; a run
    // that happened waits the configured interval.
    let timing = MaintenanceTiming::default();
    let day = timing.sweep_interval();
    assert_eq!(
        next_sweep_delay(SweepOutcome::Skipped, 0, &timing),
        MAINTENANCE_CONTENDED_RETRY,
        "a contended sweep must retry on the short interval, not the 24 h one"
    );
    assert_eq!(
        next_sweep_delay(SweepOutcome::Ran, 0, &timing),
        day,
        "a sweep that actually ran must keep the configured interval"
    );
    assert!(
        MAINTENANCE_CONTENDED_RETRY < day,
        "the retry must be shorter than the interval, or the fix is a no-op"
    );

    // The report must tell "starved" apart from "ran and found nothing".
    let db = Database::connect_in_memory().await.unwrap();
    db.run_migrations().await.unwrap();
    let svc = MaintenanceService::new(ServiceContext::new(db.pool().clone()));

    assert!(try_enter(), "the test must hold the single-flight permit");
    let starved = svc
        .run_maintenance_with(MaintenanceKnobs::default())
        .await
        .unwrap();
    assert!(
        starved.skipped,
        "a sweep that never ran must report skipped, not a silent default"
    );
    assert_eq!(starved.feedback_ledger_pruned, 0);
    assert_eq!(starved.messages_pruned, 0);

    // The skip path must not leak the permit: the next caller has to get it.
    leave();
    assert!(
        try_enter(),
        "a skipped sweep must release the single-flight permit"
    );
    leave();
}

/// #838: the contended retry MUST be shorter than the reclaim tick.
///
/// Both loops wake together after the same 1800 s warmup, so a retry equal to
/// the tick is phase-locked: the sweep re-collides with the ticker on every
/// attempt and the housekeeping it owns is lost for the whole incarnation.
/// Measured on `ops` with a 300 s retry against a 300 s tick — skipped at
/// 05:11:42.545545 and again at 05:16:42.547150, never once getting through.
/// Retrying below the tick guarantees attempts that land between ticks.
#[test]
fn test_contended_retry_is_shorter_than_the_reclaim_tick() {
    assert!(
        MAINTENANCE_CONTENDED_RETRY < MEMORY_RECLAIM_TICK,
        "#838: contended retry ({MAINTENANCE_CONTENDED_RETRY:?}) must be strictly \
         shorter than the reclaim tick ({MEMORY_RECLAIM_TICK:?}), or the sweep is \
         phase-locked to the ticker and re-collides forever"
    );
}

/// #838: a skip retries on the short interval; a real run waits the full one.
#[test]
fn test_next_sweep_delay_retries_short_only_after_a_skip() {
    let timing = MaintenanceTiming::default();
    let interval = timing.sweep_interval();
    assert_eq!(
        next_sweep_delay(SweepOutcome::Skipped, 0, &timing),
        MAINTENANCE_CONTENDED_RETRY
    );
    assert_eq!(next_sweep_delay(SweepOutcome::Ran, 0, &timing), interval);
}

/// #843: a FAILED sweep must not sleep the full interval either — but a
/// persistently broken one must not hot-loop against the DB it is failing on.
///
/// `spawn_periodic` used to fold the error arm into "not skipped", so a run that
/// errored out waited the full 86,400 s. Every retention knob then silently
/// stopped working for a day with one WARN line as the only trace.
#[test]
fn test_failed_sweep_retries_short_then_escalates() {
    let timing = MaintenanceTiming::default();
    let interval = timing.sweep_interval();

    // Fresh failures retry soon — the whole point of the fix.
    for already_retried in 0..MAX_CONSECUTIVE_FAILURE_RETRIES {
        assert_eq!(
            next_sweep_delay(SweepOutcome::Failed, already_retried, &timing),
            MAINTENANCE_CONTENDED_RETRY,
            "failure {already_retried} must retry on the short interval"
        );
    }

    // Past the bound it goes back to the configured interval, so a broken sweep
    // cannot spin against the database it is failing on.
    assert_eq!(
        next_sweep_delay(
            SweepOutcome::Failed,
            MAX_CONSECUTIVE_FAILURE_RETRIES,
            &timing
        ),
        interval,
        "a persistently failing sweep must escalate back to the full interval"
    );
    assert!(
        MAINTENANCE_CONTENDED_RETRY < interval,
        "the retry must be shorter than the interval, or the fix is a no-op"
    );
}

/// #843: the report must distinguish "ran and found nothing" from "failed".
///
/// Without this the loop reads both as a completed run and sleeps a day.
#[test]
fn test_report_outcome_distinguishes_skip_failure_and_success() {
    use crate::services::maintenance::MaintenanceReport;

    let ran = MaintenanceReport::default();
    assert_eq!(ran.outcome(), SweepOutcome::Ran);
    assert_eq!(ran.failed_legs, 0);

    let failed = MaintenanceReport {
        failed_legs: 1,
        ..Default::default()
    };
    assert_eq!(
        failed.outcome(),
        SweepOutcome::Failed,
        "a sweep with a failed leg must not read as a completed one"
    );

    // A skip outranks everything: nothing ran, so nothing could fail.
    let skipped = MaintenanceReport {
        skipped: true,
        ..Default::default()
    };
    assert_eq!(skipped.outcome(), SweepOutcome::Skipped);
}

/// #840: the two loops must NOT wake together.
///
/// `spawn_periodic` and `spawn_memory_reclaim` are both called from the same
/// bootstrap, so equal warmups make their first wake simultaneous by
/// construction — measured on `ops`, 32 µs apart, the ticker taking the shared
/// permit first and the sweep skipping. #838 taught the sweep to retry after
/// losing that race; this pins the race itself away, which is the actual fix.
///
/// The invariant is directional: the SWEEP wakes first, because losing costs it
/// every prune and both vacuums, while the ticker loses only one 300 s tick.
#[test]
fn test_sweep_and_reclaim_warmups_are_offset() {
    assert!(
        RECLAIM_WARMUP_OFFSET > std::time::Duration::ZERO,
        "#840: a zero offset restores the simultaneous first wake that made the \
         ticker win the permit every boot"
    );

    let sweep_wakes = SWEEP_WARMUP;
    let reclaim_wakes = SWEEP_WARMUP + RECLAIM_WARMUP_OFFSET;
    assert!(
        sweep_wakes < reclaim_wakes,
        "#840: the sweep must reach the permit before the ticker, or the loser is \
         the loop that owns every prune and both vacuums"
    );

    // The offset must be big enough to clear the sweep's arrival, not merely
    // non-zero: two wake-ups a microsecond apart still race.
    assert!(
        RECLAIM_WARMUP_OFFSET >= std::time::Duration::from_secs(30),
        "#840: offset ({RECLAIM_WARMUP_OFFSET:?}) must be a real gap, not a \
         rounding difference between two simultaneous spawns"
    );
}

/// #842: the startup catch-up and the sweep must not both prune the same table.
///
/// `subagent_session_ttl_days` had two schedulers. Under a long-lived daemon
/// both ran, enumerating the same row set twice a day; the fix keeps the startup
/// leg (a daemon that never survives 24 h would otherwise never prune) but makes
/// it a one-shot handover: the first sweep after a boot skips its copy and every
/// later sweep owns the prune again.
#[tokio::test]
async fn test_subagent_catchup_hands_over_exactly_once() {
    let _g = guard().await;

    // Start from a clean slate: a sibling test or an earlier boot may have left
    // the flag set.
    let _ = take_subagent_catchup();

    assert!(
        !take_subagent_catchup(),
        "#842: with no catch-up recorded, the sweep must own the prune"
    );

    note_subagent_catchup_done();
    assert!(
        take_subagent_catchup(),
        "#842: the first sweep after a startup catch-up must skip its duplicate prune"
    );
    assert!(
        !take_subagent_catchup(),
        "#842: the handover must be one-shot — a flag that never clears would \
         leave the sweep unable to prune sub-agent sessions at all"
    );
}
