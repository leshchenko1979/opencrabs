//! Tests for the `await_external` tool (#344) — the durable WRITE path.
//!
//! The acceptance criteria for this task are behavioural and go through the
//! READER, not the writer: `set` must make `awaiting_for_channel` return the
//! row, and `clear` must make it not. Asserting only that the tool returned
//! success would prove nothing — the whole point of #344 is that a lane can
//! believe it is parked while no reader can ever see it, so every assertion
//! here is made against what the boot classifier and the sweep actually read.
//!
//! The second half pins the honesty guard: `set_await` is a bare `UPDATE`
//! keyed on `session_id`, so a session with no binding row matches nothing.
//! That case must fail LOUDLY, because a silent success there recreates the
//! exact defect #344 exists to remove.

use crate::brain::tools::await_external::AwaitExternalTool;
use crate::brain::tools::r#trait::{Tool, ToolCapability, ToolExecutionContext};
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

/// Create a session row (and optionally its telegram binding).
///
/// Bindings INNER JOIN `sessions`, so both rows are required before an await
/// record can be read back by `awaiting_for_channel`.
async fn make_session(
    db: &Database,
    session: Uuid,
    binding: Option<(&str, Option<i32>)>,
) -> String {
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
    if let Some((chat, thread)) = binding {
        SessionBindingRepository::new(db.pool().clone())
            .upsert(sid.clone(), "telegram", chat, thread, BindingOrigin::Text)
            .await
            .expect("Failed to upsert binding");
    }
    sid
}

fn tool_for(db: &Database) -> AwaitExternalTool {
    AwaitExternalTool::new(SessionBindingRepository::new(db.pool().clone()))
}

/// What the boot classifier and the sweep would read: the session ids on this
/// channel that carry an await record.
async fn awaiting(db: &Database) -> Vec<String> {
    SessionBindingRepository::new(db.pool().clone())
        .awaiting_for_channel("telegram")
        .await
        .expect("awaiting_for_channel failed")
        .into_iter()
        .map(|b| b.session_id)
        .collect()
}

/// Drive one tool call, returning the result.
async fn call(
    db: &Database,
    session: Uuid,
    input: serde_json::Value,
) -> crate::brain::tools::r#trait::ToolResult {
    let ctx = ToolExecutionContext::new(session);
    tool_for(db)
        .execute(input, &ctx)
        .await
        .expect("tool execution must not error at the Result level")
}

// ---------------------------------------------------------------- surface

#[tokio::test]
async fn name_is_await_external() {
    let db = test_db().await;
    assert_eq!(tool_for(&db).name(), "await_external");
}

#[tokio::test]
async fn description_says_it_is_for_parking_on_an_external_completion() {
    let db = test_db().await;
    let d = tool_for(&db).description().to_lowercase();
    assert!(d.contains("external"), "must name the external wait");
    assert!(d.contains("restart"), "must say it survives a restart");
    assert!(d.contains("clear"), "must tell the lane to clear it");
}

#[tokio::test]
async fn schema_exposes_set_and_clear_and_requires_action() {
    let db = test_db().await;
    let schema = tool_for(&db).input_schema();
    let actions: Vec<&str> = schema["properties"]["action"]["enum"]
        .as_array()
        .expect("action enum present")
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    assert!(actions.contains(&"set"), "set missing from schema");
    assert!(actions.contains(&"clear"), "clear missing from schema");
    let required = schema["required"].as_array().expect("required present");
    assert!(required.iter().any(|v| v.as_str() == Some("action")));
    // The closed kind set is what keeps the boot and sweep logs readable.
    let kinds: Vec<&str> = schema["properties"]["kind"]["enum"]
        .as_array()
        .expect("kind enum present")
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    for k in ["ci_run", "peer_lane", "owner_gate", "external"] {
        assert!(kinds.contains(&k), "kind '{k}' missing from schema");
    }
}

#[tokio::test]
async fn parking_never_requires_approval() {
    // A tool a lane must call every time it parks cannot cost an approval
    // round-trip, and it is non-destructive and idempotent.
    let db = test_db().await;
    let tool = tool_for(&db);
    assert!(!tool.requires_approval_for_input(
        &serde_json::json!({"action": "set", "kind": "ci_run", "ref": "377"})
    ));
    assert!(!tool.requires_approval_for_input(&serde_json::json!({"action": "clear"})));
    assert!(!tool.requires_approval());
}

#[tokio::test]
async fn declares_system_modification_capability() {
    let db = test_db().await;
    assert!(tool_for(&db)
        .capabilities()
        .contains(&ToolCapability::SystemModification));
}

// ------------------------------------------- acceptance: set then read back

#[tokio::test]
async fn set_makes_awaiting_for_channel_return_the_row() {
    let db = test_db().await;
    let session = Uuid::new_v4();
    let sid = make_session(&db, session, Some(("-100123", Some(249)))).await;

    // Nothing parked yet.
    assert!(
        awaiting(&db).await.is_empty(),
        "a fresh binding must not be awaiting"
    );

    let res = call(
        &db,
        session,
        serde_json::json!({"action": "set", "kind": "ci_run", "ref": "377"}),
    )
    .await;
    assert!(res.success, "set must succeed: {:?}", res.error);

    // The reader — not the tool's own return value — is the criterion.
    let rows = awaiting(&db).await;
    assert_eq!(rows, vec![sid], "set must make the row readable as awaiting");

    // And it carries what was declared, so the boot log can name the wait.
    let b = SessionBindingRepository::new(db.pool().clone())
        .awaiting_for_channel("telegram")
        .await
        .unwrap()
        .remove(0);
    assert!(b.is_awaiting());
    assert_eq!(b.await_kind.as_deref(), Some("ci_run"));
    assert_eq!(b.await_ref.as_deref(), Some("377"));
}

#[tokio::test]
async fn clear_makes_awaiting_for_channel_stop_returning_the_row() {
    let db = test_db().await;
    let session = Uuid::new_v4();
    make_session(&db, session, Some(("-100123", Some(249)))).await;

    call(
        &db,
        session,
        serde_json::json!({"action": "set", "kind": "peer_lane", "ref": "HQ"}),
    )
    .await;
    assert_eq!(awaiting(&db).await.len(), 1, "precondition: one parked lane");

    let res = call(&db, session, serde_json::json!({"action": "clear"})).await;
    assert!(res.success, "clear must succeed: {:?}", res.error);

    assert!(
        awaiting(&db).await.is_empty(),
        "clear must make the row stop reading as awaiting"
    );
}

#[tokio::test]
async fn ref_is_optional_but_kind_is_not() {
    let db = test_db().await;
    let session = Uuid::new_v4();
    make_session(&db, session, Some(("-100123", Some(249)))).await;

    let res = call(
        &db,
        session,
        serde_json::json!({"action": "set", "kind": "owner_gate"}),
    )
    .await;
    assert!(res.success, "kind alone must be enough: {:?}", res.error);
    assert_eq!(awaiting(&db).await.len(), 1);
    let b = SessionBindingRepository::new(db.pool().clone())
        .awaiting_for_channel("telegram")
        .await
        .unwrap()
        .remove(0);
    assert_eq!(b.await_kind.as_deref(), Some("owner_gate"));
    assert!(b.await_ref.is_none(), "an omitted ref must store as NULL");

    let missing = call(&db, session, serde_json::json!({"action": "set"})).await;
    assert!(!missing.success, "set without kind must be refused");
}

// ------------------------------------------------------- honesty guards

#[tokio::test]
async fn set_on_a_session_with_no_binding_is_refused_not_silently_successful() {
    // The precise lie #344 exists to remove: a lane told it is parked while
    // nothing can ever wake it, because there is no binding row to resume.
    let db = test_db().await;
    let session = Uuid::new_v4();
    make_session(&db, session, None).await;

    let res = call(
        &db,
        session,
        serde_json::json!({"action": "set", "kind": "ci_run", "ref": "377"}),
    )
    .await;

    assert!(!res.success, "an unrecordable await must not report success");
    let err = res.error.unwrap_or_default();
    assert!(
        err.contains("NOT recorded"),
        "the error must say plainly that nothing was recorded, got: {err}"
    );
    assert!(
        awaiting(&db).await.is_empty(),
        "nothing may become readable as awaiting"
    );
}

#[tokio::test]
async fn an_unknown_kind_is_refused_and_records_nothing() {
    let db = test_db().await;
    let session = Uuid::new_v4();
    make_session(&db, session, Some(("-100123", Some(249)))).await;

    let res = call(
        &db,
        session,
        serde_json::json!({"action": "set", "kind": "vibes", "ref": "x"}),
    )
    .await;

    assert!(!res.success, "an unknown kind must be refused");
    assert!(
        res.error.unwrap_or_default().contains("Unknown kind"),
        "the error must name the problem"
    );
    assert!(
        awaiting(&db).await.is_empty(),
        "a refused set must record nothing"
    );
}

#[tokio::test]
async fn an_unknown_action_is_refused() {
    let db = test_db().await;
    let session = Uuid::new_v4();
    make_session(&db, session, Some(("-100123", Some(249)))).await;

    let res = call(&db, session, serde_json::json!({"action": "park"})).await;
    assert!(!res.success);
    assert!(res.error.unwrap_or_default().contains("Unknown action"));
}

#[tokio::test]
async fn clear_on_a_session_that_never_awaited_is_a_safe_no_op() {
    // Clearing is the routine "the run finished" call, and a lane may make it
    // defensively; it must not error, and it must not invent a record.
    let db = test_db().await;
    let session = Uuid::new_v4();
    make_session(&db, session, Some(("-100123", Some(249)))).await;

    let res = call(&db, session, serde_json::json!({"action": "clear"})).await;
    assert!(res.success, "clear must never error: {:?}", res.error);
    assert!(awaiting(&db).await.is_empty());
}

#[tokio::test]
async fn re_declaring_switches_the_wait_in_place() {
    // A lane moving from one CI run to the next re-declares rather than
    // clearing first, so the record must be replaced, never duplicated.
    let db = test_db().await;
    let session = Uuid::new_v4();
    let sid = make_session(&db, session, Some(("-100123", Some(249)))).await;

    call(
        &db,
        session,
        serde_json::json!({"action": "set", "kind": "ci_run", "ref": "377"}),
    )
    .await;
    call(
        &db,
        session,
        serde_json::json!({"action": "set", "kind": "ci_run", "ref": "378"}),
    )
    .await;

    let rows = awaiting(&db).await;
    assert_eq!(rows, vec![sid], "one lane must appear exactly once");
    let b = SessionBindingRepository::new(db.pool().clone())
        .awaiting_for_channel("telegram")
        .await
        .unwrap()
        .remove(0);
    assert_eq!(b.await_ref.as_deref(), Some("378"), "the latest ref wins");
}

#[tokio::test]
async fn another_sessions_await_is_untouched() {
    // The write is keyed on the calling session only.
    let db = test_db().await;
    let mine = Uuid::new_v4();
    let theirs = Uuid::new_v4();
    let mine_sid = make_session(&db, mine, Some(("-100123", Some(249)))).await;
    let theirs_sid = make_session(&db, theirs, Some(("-100456", Some(456)))).await;

    call(
        &db,
        theirs,
        serde_json::json!({"action": "set", "kind": "peer_lane", "ref": "other"}),
    )
    .await;
    call(
        &db,
        mine,
        serde_json::json!({"action": "set", "kind": "ci_run", "ref": "377"}),
    )
    .await;
    call(&db, mine, serde_json::json!({"action": "clear"})).await;

    let rows = awaiting(&db).await;
    assert_eq!(
        rows,
        vec![theirs_sid],
        "clearing my own wait must leave theirs alone"
    );
    assert!(!rows.contains(&mine_sid));
}
