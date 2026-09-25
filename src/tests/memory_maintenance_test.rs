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
    MaintenanceService, enter_maintenance, leave, reclaim_is_due, try_enter,
};
use crate::services::session::SessionService;
use tempfile::tempdir;

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

#[test]
fn test_maintenance_single_flight() {
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
    // the flag is process-global, so two tests taking it would race under the
    // parallel harness.
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
