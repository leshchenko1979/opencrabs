//! Per-row disposition of interrupted boot rows (#481).
//!
//! The boot loop used to wipe `pending_requests` with a blanket `clear_all()`
//! *before* it had resumed anything, so a row whose hand-off never produced a
//! report was already gone: the work disappeared with nothing left to resume
//! and nothing said about it. `dispose_pending_row` is the replacement — it
//! clears ONE row, from the arm that accounted for it.
//!
//! These cases pin that it cannot quietly behave like the wipe it replaced. A
//! regression to `clear_all` semantics would sail through a test that merely
//! counted deletions, and fails here, because every assertion is about the
//! SURVIVING set rather than the number of calls.
//!
//! `NotDispatched` is the one arm the production boot loop reaches only
//! defensively (a `session_id` that is not a UUID cannot be reached by the
//! insert path), so this file is its primary coverage.
//!
//! No ambient state is read: an in-memory database, every input supplied here,
//! no profile home and nothing taken from the environment.

use uuid::Uuid;

use crate::brain::agent::service::restart_recovery::{RowOutcome, dispose_pending_row};
use crate::db::Database;
use crate::db::repository::PendingRequestRepository;
use crate::db::repository::pending_request::PendingRequest;

/// Seed one in-flight row and return the id used for it.
async fn seed(repo: &PendingRequestRepository, session_id: Uuid, body: &str) -> Uuid {
    let id = Uuid::new_v4();
    repo.insert(id, session_id, body, "tui", None, None, "user")
        .await
        .expect("seed a pending row");
    id
}

/// The ids `get_interrupted` reports, sorted so comparisons are stable.
async fn surviving(repo: &PendingRequestRepository) -> Vec<String> {
    let mut ids: Vec<String> = repo
        .get_interrupted()
        .await
        .expect("read the table")
        .into_iter()
        .map(|r| r.id)
        .collect();
    ids.sort();
    ids
}

/// Fetch one row by id, so a case can hand the real struct to the helper
/// rather than a value it assembled itself.
async fn row(repo: &PendingRequestRepository, id: Uuid) -> PendingRequest {
    repo.get_interrupted()
        .await
        .expect("read the table")
        .into_iter()
        .find(|r| r.id == id.to_string())
        .expect("row should be present")
}

fn sorted(ids: Vec<Uuid>) -> Vec<String> {
    let mut v: Vec<String> = ids.into_iter().map(|u| u.to_string()).collect();
    v.sort();
    v
}

#[tokio::test]
async fn handed_off_and_superseded_delete_only_their_own_rows() {
    let db = Database::connect_in_memory().await.unwrap();
    db.run_migrations().await.unwrap();
    let repo = PendingRequestRepository::new(db.pool().clone());

    let session_a = Uuid::new_v4();
    let session_b = Uuid::new_v4();
    let a1 = seed(&repo, session_a, "first interrupted turn").await;
    let a2 = seed(&repo, session_a, "duplicate row for the same session").await;
    let b1 = seed(&repo, session_b, "an unrelated session").await;

    assert_eq!(surviving(&repo).await, sorted(vec![a1, a2, b1]));

    // (1) HandedOff — the row whose hand-off returned goes, and nothing else.
    let row_a1 = row(&repo, a1).await;
    assert!(
        dispose_pending_row(&repo, &row_a1, RowOutcome::HandedOff).await,
        "a cleared row reports true"
    );
    assert_eq!(
        surviving(&repo).await,
        sorted(vec![a2, b1]),
        "disposing one row must leave every other row in the table"
    );

    // (2) Superseded — the duplicate goes. This is the case that would pass a
    // deletion-counting test while behaving like `clear_all`; asserting the
    // survivors is what makes the difference visible.
    let row_a2 = row(&repo, a2).await;
    assert!(dispose_pending_row(&repo, &row_a2, RowOutcome::Superseded).await);
    assert_eq!(
        surviving(&repo).await,
        sorted(vec![b1]),
        "an unrelated session's row must survive both dispositions (#481)"
    );
}

#[tokio::test]
async fn not_dispatched_reports_and_leaves_the_row() {
    let db = Database::connect_in_memory().await.unwrap();
    db.run_migrations().await.unwrap();
    let repo = PendingRequestRepository::new(db.pool().clone());

    let b1 = seed(&repo, Uuid::new_v4(), "row no boot can resume").await;
    let row_b1 = row(&repo, b1).await;

    // A row whose session_id is not a UUID has no session to deliver to, so it
    // is reported and LEFT: it is the only evidence the session was ever
    // interrupted, and erasing it would hide the one fact worth keeping.
    assert!(
        !dispose_pending_row(&repo, &row_b1, RowOutcome::NotDispatched).await,
        "NotDispatched reports false — it did not clear the row"
    );
    assert_eq!(
        surviving(&repo).await,
        sorted(vec![b1]),
        "the never-resumable row must still be present (#481)"
    );
}

/// The spellings carried into the disposition log line. Pinned because the
/// line is the observability half of the design: a future smoke verdict greps
/// for these, so renaming a variant silently breaks that reading.
#[test]
fn outcome_spellings_are_stable() {
    assert_eq!(RowOutcome::HandedOff.as_str(), "handed_off");
    assert_eq!(RowOutcome::Superseded.as_str(), "superseded");
    assert_eq!(RowOutcome::NotDispatched.as_str(), "not_dispatched");
}
