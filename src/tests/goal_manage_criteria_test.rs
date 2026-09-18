//! Tests for the `goal_manage` criteria surface (#299).
//!
//! The tool is the model's own entry point into the goal system, so these lock
//! the two halves #299 added to it: declared criteria are stored verbatim on
//! `set` (never re-derived), and `status` reports the declared criteria
//! alongside the judge's last per-criterion evaluation.

use crate::brain::goal::GoalManager;
use crate::brain::goal::criteria::parse_criteria;
use crate::brain::tools::goal_manage::GoalManageTool;
use crate::brain::tools::r#trait::{Tool, ToolExecutionContext};
use crate::db::Database;
use crate::services::ServiceContext;
use uuid::Uuid;

/// An in-memory DB with migrations applied, plus a context wired to it.
async fn harness(sid: Uuid) -> (Database, ToolExecutionContext) {
    let db = Database::connect_in_memory().await.unwrap();
    db.run_migrations().await.unwrap();
    let mut ctx = ToolExecutionContext::new(sid);
    ctx.service_context = Some(ServiceContext::new(db.pool().clone()));
    (db, ctx)
}

/// Read back the stored criteria for a session.
async fn stored_criteria(db: &Database, sid: Uuid) -> Option<String> {
    let mgr = GoalManager::new(ServiceContext::new(db.pool().clone()));
    mgr.get_goal(sid)
        .await
        .unwrap()
        .expect("goal must exist")
        .criteria
}

/// Write a judge evaluation row directly, standing in for a judge call.
async fn store_evaluation(
    db: &Database,
    sid: Uuid,
    verdict: &str,
    reason: &str,
    evaluations: &str,
) {
    // `connect_in_memory` caps the pool at ONE connection, so this guard must be
    // released before the caller's next pool user runs -- holding it across a
    // later tool call deadlocks that call on `pool.get()`. Routing every raw
    // write through this helper is what keeps that invariant structural.
    let pool = db.pool().clone();
    let conn = pool.get().await.unwrap();
    let sid_s = sid.to_string();
    let evals = evaluations.to_owned();
    let v = verdict.to_string();
    let r = reason.to_string();
    conn.interact(move |conn| {
        conn.execute(
            "UPDATE goal_state SET judge_verdict = ?1, judge_reason = ?2, \
             criterion_evaluations = ?3 WHERE session_id = ?4",
            rusqlite::params![v, r, evals, sid_s],
        )
    })
    .await
    .unwrap()
    .unwrap();
}

#[test]
fn schema_exposes_criteria_as_an_array_of_strings() {
    let schema = GoalManageTool.input_schema();
    let criteria = &schema["properties"]["criteria"];
    assert!(criteria.is_object(), "criteria property missing in schema");
    assert_eq!(criteria["type"].as_str(), Some("array"));
    assert_eq!(criteria["items"]["type"].as_str(), Some("string"));
    // Optional: 'set' without criteria stays legal (derivation on first eval).
    let required = schema["required"].as_array().expect("required present");
    assert!(!required.iter().any(|v| v.as_str() == Some("criteria")));
}

#[tokio::test]
async fn set_with_explicit_criteria_stores_exactly_those_strings() {
    let sid = Uuid::new_v4();
    let (db, ctx) = harness(sid).await;

    let result = GoalManageTool
        .execute(
            serde_json::json!({
                "action": "set",
                "goal": "ship the fix",
                "criteria": ["CI gate green on the head", "commit pushed"]
            }),
            &ctx,
        )
        .await
        .unwrap();
    assert!(result.success, "set failed: {:?}", result.error);

    let stored = stored_criteria(&db, sid).await;
    assert_eq!(
        parse_criteria(stored.as_deref().unwrap_or("")),
        vec![
            "CI gate green on the head".to_string(),
            "commit pushed".to_string()
        ],
    );

    // The success message tells the caller what it will be judged against.
    assert!(result.output.contains("CI gate green on the head"));
}

#[tokio::test]
async fn set_without_criteria_leaves_the_column_null() {
    // NULL — not an empty array — is what makes the first evaluation derive a
    // list. An empty array would mean "nothing to prove" and cap the verdict.
    let sid = Uuid::new_v4();
    let (db, ctx) = harness(sid).await;

    let result = GoalManageTool
        .execute(
            serde_json::json!({"action": "set", "goal": "no criteria given"}),
            &ctx,
        )
        .await
        .unwrap();
    assert!(result.success, "set failed: {:?}", result.error);

    assert!(stored_criteria(&db, sid).await.is_none());
}

#[tokio::test]
async fn set_drops_blank_and_non_string_criteria_entries() {
    let sid = Uuid::new_v4();
    let (db, ctx) = harness(sid).await;

    let result = GoalManageTool
        .execute(
            serde_json::json!({
                "action": "set",
                "goal": "trim the criteria",
                "criteria": ["  real one  ", "", "   ", 42, "second"]
            }),
            &ctx,
        )
        .await
        .unwrap();
    assert!(result.success, "set failed: {:?}", result.error);

    let stored = stored_criteria(&db, sid).await;
    assert_eq!(
        parse_criteria(stored.as_deref().unwrap_or("")),
        vec!["real one".to_string(), "second".to_string()],
    );
}

#[tokio::test]
async fn set_treats_an_all_blank_criteria_list_as_undeclared() {
    // Declaring only blanks is the same as declaring nothing: the column stays
    // NULL so derivation still runs, rather than pinning "nothing to prove".
    let sid = Uuid::new_v4();
    let (db, ctx) = harness(sid).await;

    let result = GoalManageTool
        .execute(
            serde_json::json!({
                "action": "set",
                "goal": "blank criteria only",
                "criteria": ["", "   "]
            }),
            &ctx,
        )
        .await
        .unwrap();
    assert!(result.success, "set failed: {:?}", result.error);

    assert!(stored_criteria(&db, sid).await.is_none());
}

#[tokio::test]
async fn status_reports_the_criteria_and_the_last_per_criterion_statuses() {
    let sid = Uuid::new_v4();
    let (db, ctx) = harness(sid).await;

    GoalManageTool
        .execute(
            serde_json::json!({
                "action": "set",
                "goal": "ship the fix",
                "criteria": ["CI gate green on the head", "commit pushed"]
            }),
            &ctx,
        )
        .await
        .unwrap();

    store_evaluation(
        &db,
        sid,
        "UNCERTAIN",
        "gate still running",
        &serde_json::json!([
            {
                "id": "c1",
                "criterion": "CI gate green on the head",
                "status": "MET",
                "evidence": "run 123 concluded success"
            },
            {
                "id": "c2",
                "criterion": "commit pushed",
                "status": "NO_EVIDENCE",
                "evidence": ""
            }
        ])
        .to_string(),
    )
    .await;

    let result = GoalManageTool
        .execute(serde_json::json!({"action": "status"}), &ctx)
        .await
        .unwrap();
    assert!(result.success, "status failed: {:?}", result.error);
    let out = &result.output;

    // The declared criteria, each with the judge's status for it.
    assert!(out.contains("CI gate green on the head"), "output: {out}");
    assert!(out.contains("c1 [MET]"), "output: {out}");
    assert!(out.contains("c2 [NO_EVIDENCE]"), "output: {out}");
    assert!(
        out.contains("run 123 concluded success"),
        "evidence must be shown: {out}"
    );
    // And the verdict it came from, so the reading is not orphaned.
    assert!(out.contains("Last verdict: UNCERTAIN"), "output: {out}");
    assert!(out.contains("gate still running"), "output: {out}");
}

#[tokio::test]
async fn status_marks_criteria_the_judge_never_evaluated() {
    // Declared but no judge call yet — the criteria must still be listed, and
    // must not be presented as though they had a status.
    let sid = Uuid::new_v4();
    let (_db, ctx) = harness(sid).await;

    GoalManageTool
        .execute(
            serde_json::json!({
                "action": "set",
                "goal": "ship the fix",
                "criteria": ["CI gate green on the head"]
            }),
            &ctx,
        )
        .await
        .unwrap();

    let result = GoalManageTool
        .execute(serde_json::json!({"action": "status"}), &ctx)
        .await
        .unwrap();
    assert!(result.success, "status failed: {:?}", result.error);
    let out = &result.output;

    assert!(out.contains("CI gate green on the head"), "output: {out}");
    assert!(out.contains("NOT EVALUATED"), "output: {out}");
    assert!(
        out.contains("Last verdict: (not yet evaluated)"),
        "output: {out}"
    );
}

#[tokio::test]
async fn status_survives_a_garbled_evaluation_column() {
    // Display-only column: unparseable JSON degrades to "not evaluated" rather
    // than failing the whole action.
    let sid = Uuid::new_v4();
    let (db, ctx) = harness(sid).await;

    GoalManageTool
        .execute(
            serde_json::json!({
                "action": "set",
                "goal": "ship the fix",
                "criteria": ["CI gate green on the head"]
            }),
            &ctx,
        )
        .await
        .unwrap();

    store_evaluation(
        &db,
        sid,
        "UNCERTAIN",
        "garbled evaluation column",
        "{not json at all",
    )
    .await;

    let result = GoalManageTool
        .execute(serde_json::json!({"action": "status"}), &ctx)
        .await
        .unwrap();
    assert!(result.success, "status failed: {:?}", result.error);
    assert!(result.output.contains("NOT EVALUATED"), "{}", result.output);
}
