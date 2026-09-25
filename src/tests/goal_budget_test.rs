//! Tests for goal turn budget config, creation, and exhaustion (#190).

use crate::brain::goal::GoalManager;
use crate::brain::goal::evidence::GoalEvidence;
use crate::brain::goal::types::{DEFAULT_MAX_TURNS, GoalDecision};
use crate::brain::provider::Provider;
use crate::brain::tools::goal_manage::GoalManageTool;
use crate::brain::tools::r#trait::Tool;
use crate::config::AgentConfig;
use crate::db::models::Session;
use crate::db::{BindingOrigin, Database, SessionBindingRepository, SessionRepository};
use crate::services::ServiceContext;
use crate::tests::agent_service_mocks::MockProvider;
use std::sync::Arc;
use uuid::Uuid;

async fn test_db() -> Database {
    let db = Database::connect_in_memory()
        .await
        .expect("Failed to create database");
    db.run_migrations().await.expect("Failed to run migrations");
    db
}

/// Create a session row and a telegram binding for it, returning the session id
/// as a string. Bindings INNER JOIN `sessions`, so the session row must exist
/// before an await record can stick (#344).
async fn bind_session(db: &Database, session: Uuid) -> String {
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
        .upsert(sid.clone(), "telegram", "-100344", Some(344), BindingOrigin::Text)
        .await
        .expect("Failed to upsert binding");
    sid
}

#[test]
fn config_goal_max_turns_parses_cleanly() {
    let toml_str = r#"
        approval_policy = "ask"
        goal_max_turns = 35
    "#;
    let cfg: AgentConfig = toml::from_str(toml_str).expect("should parse AgentConfig");
    assert_eq!(cfg.goal_max_turns, Some(35));

    let default_cfg = AgentConfig::default();
    assert_eq!(default_cfg.goal_max_turns, None);

    let serialized = toml::to_string(&default_cfg).expect("should serialize");
    assert!(!serialized.contains("goal_max_turns"));
}

#[test]
fn goal_manage_schema_exposes_max_turns() {
    let schema = GoalManageTool.input_schema();
    let max_turns_prop = &schema["properties"]["max_turns"];
    assert!(
        max_turns_prop.is_object(),
        "max_turns property missing in schema"
    );
    assert_eq!(max_turns_prop["type"].as_str(), Some("integer"));
}

#[tokio::test]
async fn goal_creation_defaults_to_20_when_unspecified() {
    let db = Database::connect_in_memory().await.unwrap();
    db.run_migrations().await.unwrap();
    let ctx = ServiceContext::new(db.pool().clone());
    let goal_mgr = GoalManager::new(ctx);
    let sid = Uuid::new_v4();

    let goal = goal_mgr
        .set_goal(
            sid,
            "test goal without max_turns".to_string(),
            None,
            None,
            None,
        )
        .await
        .expect("set_goal should succeed");

    assert_eq!(goal.max_turns, DEFAULT_MAX_TURNS as i32);
    assert_eq!(goal.state, "active");
    assert_eq!(goal.turns_used, 0);

    let loaded = goal_mgr
        .get_goal(sid)
        .await
        .unwrap()
        .expect("goal must exist");
    assert_eq!(loaded.max_turns, DEFAULT_MAX_TURNS as i32);
}

#[tokio::test]
async fn goal_creation_respects_custom_max_turns() {
    let db = Database::connect_in_memory().await.unwrap();
    db.run_migrations().await.unwrap();
    let ctx = ServiceContext::new(db.pool().clone());
    let goal_mgr = GoalManager::new(ctx);
    let sid = Uuid::new_v4();

    let goal = goal_mgr
        .set_goal(
            sid,
            "test goal with custom max_turns".to_string(),
            None,
            None,
            Some(50),
        )
        .await
        .expect("set_goal should succeed");

    assert_eq!(goal.max_turns, 50);

    let loaded = goal_mgr
        .get_goal(sid)
        .await
        .unwrap()
        .expect("goal must exist");
    assert_eq!(loaded.max_turns, 50);
}

#[tokio::test]
async fn goal_turn_exhaustion_pauses_and_updates_db_state() {
    let db = Database::connect_in_memory().await.unwrap();
    db.run_migrations().await.unwrap();
    let ctx = ServiceContext::new(db.pool().clone());
    let goal_mgr = GoalManager::new(ctx.clone());
    let sid = Uuid::new_v4();

    // Set goal with a low turn budget of 2
    let goal = goal_mgr
        .set_goal(sid, "exhaust budget test".to_string(), None, None, Some(2))
        .await
        .expect("set_goal should succeed");
    assert_eq!(goal.max_turns, 2);

    // Increment turns to reach the budget (2/2)
    goal_mgr.increment_turns(sid, false).await.unwrap();
    goal_mgr.increment_turns(sid, false).await.unwrap();

    let before_eval = goal_mgr.get_goal(sid).await.unwrap().unwrap();
    assert_eq!(before_eval.turns_used, 2);
    assert_eq!(before_eval.state, "active");

    let provider: Arc<dyn Provider> = Arc::new(MockProvider);
    let decision = goal_mgr
        .evaluate_after_turn(
            provider.as_ref(),
            "mock-model",
            sid,
            &GoalEvidence::default(),
            "done with work",
        )
        .await;

    match decision {
        GoalDecision::Paused { reason } => {
            assert!(
                reason.contains("exhausted"),
                "reason should mention exhausted, got: {reason}"
            );
            assert!(
                reason.contains("2/2"),
                "reason should mention turn count 2/2, got: {reason}"
            );
            assert!(
                reason.contains("paused"),
                "reason should mention paused, got: {reason}"
            );
        }
        other => panic!("expected GoalDecision::Paused, got: {:?}", other),
    }

    // Crucial check: DB state must be reconciled to "paused", NOT "failed"
    let after_eval = goal_mgr.get_goal(sid).await.unwrap().unwrap();
    assert_eq!(
        after_eval.state, "paused",
        "turn budget exhaustion state must be 'paused', not 'failed'"
    );
}

// ── #567: goal deferral on a running background task ────────────────────────
//
// The measured defect: a goal waiting on one detached task was re-prompted 20
// times, each re-prompt billed a turn, and the goal paused at 20/20 while the
// task was still running — so the work it waited for was never judged. Every
// leg below states its FALSIFYING INPUT: the value that makes it fail.

/// Falsifying input: `running_tasks` non-empty with no open plan task. Before
/// the fix this returned `Continue` and billed the turn; if `turns_used` reads
/// anything but 0 the deferral is spending the budget it exists to preserve.
#[tokio::test]
async fn deferral_does_not_bill_the_turn_budget() {
    let db = test_db().await;
    let ctx = ServiceContext::new(db.pool().clone());
    let goal_mgr = GoalManager::new(ctx);
    let sid = Uuid::new_v4();

    goal_mgr
        .set_goal(
            sid,
            "follow the skill".to_string(),
            None,
            None,
            Some(20),
        )
        .await
        .expect("set_goal should succeed");

    let provider: Arc<dyn Provider> = Arc::new(MockProvider);
    let evidence = GoalEvidence {
        running_tasks: vec!["oc-ship-chain (running 722s)".to_string()],
        ..Default::default()
    };

    let decision = goal_mgr
        .evaluate_after_turn(
            provider.as_ref(),
            "mock-model",
            sid,
            &evidence,
            "the ship chain is still running",
        )
        .await;

    match decision {
        GoalDecision::Deferred {
            ref reason,
            ref wake_ref,
        } => {
            assert!(
                reason.contains("still running"),
                "reason should name the running task, got: {reason}"
            );
            assert_eq!(
                wake_ref, "oc-ship-chain (running 722s)",
                "wake_ref must carry the task label so the await record names what it waits on"
            );
        }
        other => panic!("expected GoalDecision::Deferred, got: {other:?}"),
    }

    let after = goal_mgr.get_goal(sid).await.unwrap().unwrap();
    assert_eq!(
        after.turns_used, 0,
        "a deferred turn performs no goal work and must not consume the budget \
         (billing it is the #567 defect)"
    );
    assert_eq!(
        after.state, "active",
        "a deferral keeps the goal active — it must never pause or complete"
    );
}

/// Falsifying input: `unresolved_tasks` non-empty. An open plan task is work the
/// agent CAN advance, so the #299 re-prompt must survive unchanged — and it
/// still bills, because that turn does real work.
#[tokio::test]
async fn open_plan_task_still_continues_and_bills() {
    let db = test_db().await;
    let ctx = ServiceContext::new(db.pool().clone());
    let goal_mgr = GoalManager::new(ctx);
    let sid = Uuid::new_v4();

    goal_mgr
        .set_goal(sid, "finish the plan".to_string(), None, None, Some(20))
        .await
        .expect("set_goal should succeed");

    let provider: Arc<dyn Provider> = Arc::new(MockProvider);
    let evidence = GoalEvidence {
        unresolved_tasks: vec!["Task 3: implement the deferral".to_string()],
        ..Default::default()
    };

    let decision = goal_mgr
        .evaluate_after_turn(provider.as_ref(), "mock-model", sid, &evidence, "working on it")
        .await;

    match decision {
        GoalDecision::Continue {
            ref continuation_prompt,
            ..
        } => assert!(
            continuation_prompt.contains("still open"),
            "the continuation must name the open plan task, got: {continuation_prompt}"
        ),
        other => panic!("expected GoalDecision::Continue, got: {other:?}"),
    }

    let after = goal_mgr.get_goal(sid).await.unwrap().unwrap();
    assert_eq!(
        after.turns_used, 1,
        "a re-prompt on open plan work is a real turn and must still be billed"
    );
}

/// THE #299 GUARD. Falsifying input: a running task AND an open plan task
/// together. If this ever returns `Deferred`, a goal could stall behind a plan
/// task it is supposed to be working on — the deferral would become a way to
/// skip work rather than a way to wait for it.
#[tokio::test]
async fn deferral_is_unreachable_while_a_plan_task_is_open() {
    let db = test_db().await;
    let ctx = ServiceContext::new(db.pool().clone());
    let goal_mgr = GoalManager::new(ctx);
    let sid = Uuid::new_v4();

    goal_mgr
        .set_goal(sid, "finish the plan".to_string(), None, None, Some(20))
        .await
        .expect("set_goal should succeed");

    let provider: Arc<dyn Provider> = Arc::new(MockProvider);
    let evidence = GoalEvidence {
        running_tasks: vec!["cargo test --all-features (pid 4242)".to_string()],
        unresolved_tasks: vec!["Task 1: open".to_string()],
        ..Default::default()
    };

    let decision = goal_mgr
        .evaluate_after_turn(provider.as_ref(), "mock-model", sid, &evidence, "working on it")
        .await;

    match decision {
        GoalDecision::Continue { .. } => {}
        GoalDecision::Deferred { .. } => panic!(
            "a deferral must never shadow an open plan task — that is the #299 \
             guarantee this change is required to preserve"
        ),
        other => panic!("expected GoalDecision::Continue, got: {other:?}"),
    }
}

/// Falsifying input: a bound session that defers, then is evaluated again with
/// no running task. The await record must exist after the deferral (or nothing
/// can wake the lane — #480) and must be gone after the judged turn (or a stale
/// wait outlives the deferral and the sweep wakes a session that is not waiting).
#[tokio::test]
async fn deferral_records_the_await_and_a_judged_turn_clears_it() {
    let db = test_db().await;
    let sid = Uuid::new_v4();
    let sid_s = bind_session(&db, sid).await;
    let goal_mgr = GoalManager::new(ServiceContext::new(db.pool().clone()));
    let repo = SessionBindingRepository::new(db.pool().clone());

    goal_mgr
        .set_goal(sid, "follow the skill".to_string(), None, None, Some(20))
        .await
        .expect("set_goal should succeed");

    let provider: Arc<dyn Provider> = Arc::new(MockProvider);
    let waiting = GoalEvidence {
        running_tasks: vec!["oc-ship-chain (running 60s)".to_string()],
        ..Default::default()
    };
    let _ = goal_mgr
        .evaluate_after_turn(provider.as_ref(), "mock-model", sid, &waiting, "still running")
        .await;

    let binding = repo
        .by_session(&sid_s)
        .await
        .unwrap()
        .expect("binding must exist");
    assert_eq!(
        binding.await_kind.as_deref(),
        Some("background_task"),
        "the deferral must leave a waker behind, or the goal is stranded (#480)"
    );
    assert_eq!(
        binding.await_ref.as_deref(),
        Some("oc-ship-chain (running 60s)"),
        "the await record must name the task it waits on"
    );
    assert!(
        binding.await_at.is_some(),
        "await_at is what both readers key on"
    );

    // The task finished — the next evaluation is a real judgement, so the wait
    // must not outlive it.
    let done_waiting = GoalEvidence::default();
    let _ = goal_mgr
        .evaluate_after_turn(
            provider.as_ref(),
            "mock-model",
            sid,
            &done_waiting,
            "the ship chain finished",
        )
        .await;

    let binding = repo
        .by_session(&sid_s)
        .await
        .unwrap()
        .expect("binding must exist");
    assert!(
        binding.await_kind.is_none(),
        "a judged turn means the wait is over and the record must be cleared"
    );
    assert!(binding.await_at.is_none());
}

/// THE #344 GUARD. Falsifying input: a session carrying BOTH a goal and a wait
/// declared through `await_external`. An unscoped clear from the goal path would
/// wipe the `ci_run` wait and leave that lane permanently un-wakeable — the
/// exact failure #344 exists to remove.
#[tokio::test]
async fn the_goal_path_never_clears_a_foreign_await_kind() {
    let db = test_db().await;
    let sid = Uuid::new_v4();
    let sid_s = bind_session(&db, sid).await;
    let goal_mgr = GoalManager::new(ServiceContext::new(db.pool().clone()));
    let repo = SessionBindingRepository::new(db.pool().clone());

    goal_mgr
        .set_goal(sid, "follow the skill".to_string(), None, None, Some(20))
        .await
        .expect("set_goal should succeed");

    repo.set_await(&sid_s, "ci_run", Some("35423208985"))
        .await
        .expect("set_await failed");

    let provider: Arc<dyn Provider> = Arc::new(MockProvider);
    let _ = goal_mgr
        .evaluate_after_turn(
            provider.as_ref(),
            "mock-model",
            sid,
            &GoalEvidence::default(),
            "the build finished",
        )
        .await;

    let binding = repo
        .by_session(&sid_s)
        .await
        .unwrap()
        .expect("binding must exist");
    assert_eq!(
        binding.await_kind.as_deref(),
        Some("ci_run"),
        "the goal path must never un-park a wait it did not declare"
    );
    assert_eq!(binding.await_ref.as_deref(), Some("35423208985"));
}
