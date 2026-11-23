//! The #974 destructive-park gate.
//!
//! The gate's decision is driven by the JOURNAL, not the pending row's
//! origin — the mutant that killed #974's first reading was keying on
//! `PendingOrigin`, which parks everything the system half already covered
//! and nothing the user half actually needs. Each leg builds its journal
//! through the real path: `record_started` at dispatch, `reconcile_started_
//! to_unknown` at boot — the exact rows `destructive_park_notice` reads.
//! One database per leg: the reconciler is a whole-table pass, so legs
//! sharing a DB would reconcile each other's STARTED controls.

use crate::brain::agent::service::restart_recovery::destructive_park_notice;
use crate::db::Database;
use crate::db::repository::ToolExecutionRepository;

/// The gate test's harness: an in-memory DB with its journal repo, bound so
/// the pool outlives the call (the digest tests found the dropped-`Database`
/// hazard first).
async fn gate_db() -> (Database, ToolExecutionRepository) {
    let db = Database::connect_in_memory().await.unwrap();
    db.run_migrations().await.unwrap();
    let repo = ToolExecutionRepository::new(db.pool().clone());
    (db, repo)
}

/// Journal one call through the real seam and flip it the way a restart
/// does: `record_started` before dispatch, `reconcile_started_to_unknown`
/// at boot.
async fn journal_and_kill(
    repo: &ToolExecutionRepository,
    session: &str,
    tool_name: &str,
    arguments: &str,
) {
    repo.record_started("r", "m", session, "t", tool_name, arguments)
        .await
        .unwrap();
    repo.reconcile_started_to_unknown().await.unwrap();
}

#[tokio::test]
async fn a_destructive_unknown_parks_the_turn() {
    let session = uuid::Uuid::from_u128(0x974_0000_0000_0000_0001);
    let sid = session.to_string();
    let (_db, repo) = gate_db().await;
    journal_and_kill(&repo, &sid, "bash", r#"{"command":"git push origin main"}"#).await;

    let notice = destructive_park_notice(session, &repo).await;
    let notice = notice.expect("a destructive bash unknown must park the turn");
    assert!(
        notice.contains("bash"),
        "the notice must name the call: {notice}"
    );
    assert!(
        notice.contains(&session.simple().to_string()[..8]),
        "the notice must carry the session: {notice}"
    );
}

#[tokio::test]
async fn a_mutating_unknown_resumes_with_the_digest() {
    let session = uuid::Uuid::from_u128(0x974_0000_0000_0000_0002);
    let sid = session.to_string();
    let (_db, repo) = gate_db().await;
    journal_and_kill(&repo, &sid, "edit_file", r#"{}"#).await;

    let notice = destructive_park_notice(session, &repo).await;
    assert!(
        notice.is_none(),
        "a mutating-only turn must replay — the digest covers it: {notice:?}"
    );
}

#[tokio::test]
async fn an_oc_family_destructive_unknown_parks_too() {
    let session = uuid::Uuid::from_u128(0x974_0000_0000_0000_0003);
    let sid = session.to_string();
    let (_db, repo) = gate_db().await;
    // The oc-* family is classified from the tool NAME when the arguments
    // carry nothing usable: `oc-ship-chain` parks on the name alone.
    journal_and_kill(&repo, &sid, "oc-ship-chain", "null").await;

    let notice = destructive_park_notice(session, &repo).await;
    assert!(
        notice.is_some(),
        "oc-ship-chain must park on the name alone: {notice:?}"
    );
}

#[tokio::test]
async fn a_started_row_is_in_flight_not_interrupted() {
    let session = uuid::Uuid::from_u128(0x974_0000_0000_0000_0004);
    let sid = session.to_string();
    let (_db, repo) = gate_db().await;
    // NO reconciler: the row still reads `started`, meaning the previous
    // process died BEFORE the boot reconciler ran — the gate must not treat
    // an in-flight call as an interrupted one. That is the reconciler's
    // receipt to write, not the gate's to assume.
    repo.record_started(
        "r",
        "m",
        &sid,
        "t",
        "bash",
        r#"{"command":"git reset --hard"}"#,
    )
    .await
    .unwrap();

    let notice = destructive_park_notice(session, &repo).await;
    assert!(
        notice.is_none(),
        "a started row must not park the turn — it is not yet unknown: {notice:?}"
    );
}
