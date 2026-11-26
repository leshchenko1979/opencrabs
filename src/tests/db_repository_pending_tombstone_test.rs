//! Tombstone durability tests (#73).
//!
//! A sub-agent death report that parks in memory must survive a restart via
//! its DB row, and the row must die the moment the report is delivered —
//! never before (lost report) and not permanently after (duplicate spam).

use crate::db::Database;
use crate::db::repository::PendingTombstoneRepository;
use uuid::Uuid;

async fn repo() -> PendingTombstoneRepository {
    let db = Database::connect_in_memory()
        .await
        .expect("in-memory database");
    db.run_migrations().await.expect("migrations");
    PendingTombstoneRepository::new(db.pool().clone())
}

#[tokio::test]
async fn tombstone_survives_as_row_until_cleared() {
    let repo = repo().await;
    let id = Uuid::new_v4();
    let session = Uuid::new_v4();

    repo.record(id, session, "context body", "display line", None)
        .await
        .expect("record");

    let rows = repo.all().await.expect("all");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].id, id);
    assert_eq!(rows[0].session_id, session);
    assert_eq!(rows[0].context_text, "context body");
    assert_eq!(rows[0].display_text, "display line");

    // Delivery clears the row: it must not re-deliver after a restart.
    repo.clear(id).await.expect("clear");
    assert!(repo.all().await.expect("all").is_empty());
}

#[tokio::test]
async fn clear_for_session_spares_other_sessions_rows() {
    let repo = repo().await;
    let (delivered, undelivered) = (Uuid::new_v4(), Uuid::new_v4());
    let (session_a, session_b) = (Uuid::new_v4(), Uuid::new_v4());

    repo.record(delivered, session_a, "a", "a", None)
        .await
        .expect("record a");
    repo.record(undelivered, session_b, "b", "b", None)
        .await
        .expect("record b");

    // Session A's route claimed its report — its durable copy goes away.
    repo.clear_for_session(session_a)
        .await
        .expect("clear_for_session");

    let rows = repo.all().await.expect("all");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].id, undelivered);
    assert_eq!(rows[0].session_id, session_b);
}

/// #1024: the durable record rides the tombstone too, not only the tag.
///
/// This store is NOT a redundant copy of `notify_queue`. `persist_tombstone`'s
/// callers reach `deliver_or_park` directly and never `parking_route` — the
/// only writer of a `notify_queue` row — so on the boot-reconciliation path
/// the tombstone is the ONLY durable copy of the report. Without this column a
/// restart rebuilt it from the 34-char display tag, which is the exact loss
/// #1024 exists to fix, one layer down.
#[tokio::test]
async fn the_history_record_rides_the_tombstone_row() {
    let repo = repo().await;
    let id = Uuid::new_v4();
    let session = Uuid::new_v4();

    repo.record(
        id,
        session,
        "context body",
        "🤖 sub-agent finished: review-buyer",
        Some("🤖 sub-agent finished: review-buyer\n\nTHE WHOLE REPORT"),
    )
    .await
    .expect("record");

    let rows = repo.all().await.expect("all");
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].history_text.as_deref(),
        Some("🤖 sub-agent finished: review-buyer\n\nTHE WHOLE REPORT"),
        "the record must survive the round trip, not degrade to the tag"
    );
}

/// A row written with no record keeps `NULL`, so the reader falls back to the
/// display tag exactly as it did before the column existed.
#[tokio::test]
async fn a_row_without_a_record_reads_back_as_none() {
    let repo = repo().await;
    let id = Uuid::new_v4();
    repo.record(id, Uuid::new_v4(), "ctx", "display", None)
        .await
        .expect("record");

    let rows = repo.all().await.expect("all");
    assert_eq!(rows.len(), 1);
    assert!(
        rows[0].history_text.is_none(),
        "None means 'persist the tag', never an empty string"
    );
}
