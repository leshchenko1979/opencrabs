//! #107: cron result-delivery failures must never masquerade as success.
//!
//! The run row carries the outcome distinctly: `complete_delivery_failed`
//! stamps `status='delivery_failed'` with the reason, leaves an execution
//! `error` row untouched (no-overwrite guard), and never flips an already-
//! completed row's `content`. The delivery path's fail-fast validator
//! (`validate_delivery_target`) rejects targets that would silently drop
//! every future run's result before a job ever goes live.

use crate::brain::tools::cron_manage::validate_delivery_target;
use crate::db::Database;
use rusqlite::params;
use uuid::Uuid;

async fn test_db() -> crate::db::Pool {
    let db = Database::connect_in_memory().await.unwrap();
    db.run_migrations().await.unwrap();
    db.pool().clone()
}

/// Seed a cron run row in the exact shape the insert path writes:
/// status='running' with an RFC3339 (+00:00) started_at.
async fn seed_run(pool: &crate::db::Pool, id: &str, started_at: chrono::DateTime<chrono::Utc>) {
    let id = id.to_string();
    let ts = started_at.to_rfc3339();
    let parent_job_id = Uuid::new_v4().to_string();
    pool.get()
        .await
        .unwrap()
        .interact(move |conn| {
            // Parent row first — cron_job_runs.job_id has an FK to cron_jobs(id).
            conn.execute(
                "INSERT INTO cron_jobs (id, name, cron_expr, timezone, prompt, thinking, \
                 auto_approve, enabled, created_at, updated_at) \
                 VALUES (?1, '107-test', '0 0 * * *', 'UTC', 'x', 'off', 1, 1, ?2, ?2)",
                params![parent_job_id, ts],
            )?;
            conn.execute(
                "INSERT INTO cron_job_runs (id, job_id, job_name, status, content, error, \
                 input_tokens, output_tokens, cost, provider, model, started_at, completed_at, created_at)
                 VALUES (?1, ?2, '107-test', 'running', NULL, NULL, 0, 0, 0.0, NULL, NULL, ?3, NULL, ?3)",
                params![id, parent_job_id, ts],
            )
        })
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn delivery_failure_stamps_delivery_failed_not_success() {
    let pool = test_db().await;
    let repo = crate::db::CronJobRunRepository::new(pool.clone());
    let run_id = format!("{}-107", Uuid::new_v4());
    seed_run(&pool, &run_id, chrono::Utc::now()).await;

    // Simulate the scheduler flow: execution succeeded (content recorded),
    // then delivery failed.
    repo.complete_success(&run_id, "report body", 10, 20, 0.001)
        .await
        .unwrap();
    repo.complete_delivery_failed(&run_id, "No Telegram bot token in keys.toml (#107)")
        .await
        .unwrap();

    let (status, content, error): (String, Option<String>, Option<String>) = pool
        .get()
        .await
        .unwrap()
        .interact(move |conn| {
            conn.query_row(
                "SELECT status, content, error FROM cron_job_runs WHERE id = ?1",
                params![run_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
        })
        .await
        .unwrap()
        .unwrap();

    assert_eq!(
        status, "delivery_failed",
        "silent success is the #107 defect"
    );
    assert_eq!(
        content.as_deref(),
        Some("report body"),
        "execution content must survive the delivery-failure stamp"
    );
    assert!(
        error
            .as_deref()
            .unwrap_or_default()
            .contains("No Telegram bot token"),
        "reason must be recorded on the row"
    );
}

#[tokio::test]
async fn delivery_failure_never_overwrites_execution_error() {
    let pool = test_db().await;
    let repo = crate::db::CronJobRunRepository::new(pool.clone());
    let run_id = format!("{}-107b", Uuid::new_v4());
    seed_run(&pool, &run_id, chrono::Utc::now()).await;

    repo.complete_error(&run_id, "agent exploded")
        .await
        .unwrap();
    repo.complete_delivery_failed(&run_id, "late delivery failure")
        .await
        .unwrap();

    let (status, error): (String, Option<String>) = pool
        .get()
        .await
        .unwrap()
        .interact(move |conn| {
            conn.query_row(
                "SELECT status, error FROM cron_job_runs WHERE id = ?1",
                params![run_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
        })
        .await
        .unwrap()
        .unwrap();

    assert_eq!(status, "error", "execution error must keep its status");
    assert_eq!(
        error.as_deref(),
        Some("agent exploded"),
        "execution error reason must survive"
    );
}

#[test]
fn session_delivery_target_passes_validation() {
    assert!(validate_delivery_target("session:12345678-1234-1234-1234-123456789abc").is_ok());
    assert!(validate_delivery_target("oc://session/12345678-1234-1234-1234-123456789abc").is_ok());
    assert!(validate_delivery_target("session:").is_err());
    assert!(validate_delivery_target("oc://session/").is_err());
}

#[test]
fn delivery_target_validation_rejects_silent_drop_shapes() {
    // Unknown channel → every run would drop.
    assert!(validate_delivery_target("carrier:pigeon").is_err());
    // Missing separator → unparseable at delivery time.
    assert!(validate_delivery_target("telegram-only").is_err());
    // Empty id.
    assert!(validate_delivery_target("telegram:").is_err());
    assert!(validate_delivery_target("session:").is_err());
    assert!(validate_delivery_target("oc://session/").is_err());
    // Webhooks carry no host-side credential — always valid.
    assert!(validate_delivery_target("https://example.com/hook").is_ok());
    // Session targets are valid destinations (active turn injection).
    assert!(validate_delivery_target("session:12345678-1234-1234-1234-123456789abc").is_ok());
    assert!(validate_delivery_target("oc://session/12345678-1234-1234-1234-123456789abc").is_ok());
}

#[tokio::test]
async fn test_cron_set_goal_requires_session() {
    use crate::brain::tools::cron_manage::CronManageTool;
    use crate::brain::tools::r#trait::{Tool, ToolExecutionContext};
    use serde_json::json;

    let pool = test_db().await;
    let repo = crate::db::repository::CronJobRepository::new(pool.clone());
    let tool = CronManageTool::new(repo);
    let ctx = ToolExecutionContext::new(uuid::Uuid::new_v4());

    // 1. Create with set_goal = true but no deliver_to -> REJECTED
    let input_no_deliver = json!({
        "action": "create",
        "name": "goal-job-1",
        "cron": "0 0 * * *",
        "tz": "UTC",
        "prompt": "do something",
        "set_goal": true
    });
    let res = tool.execute(input_no_deliver, &ctx).await.unwrap();
    assert!(!res.success, "set_goal without deliver_to must be rejected");
    assert!(
        res.error
            .unwrap_or_default()
            .contains("set_goal requires oc://session"),
        "error message must explain set_goal requires session target"
    );

    // 2. Create with set_goal = true and channel delivery -> REJECTED
    let input_channel_deliver = json!({
        "action": "create",
        "name": "goal-job-2",
        "cron": "0 0 * * *",
        "tz": "UTC",
        "prompt": "do something",
        "deliver_to": "https://example.com/webhook",
        "set_goal": true
    });
    let res2 = tool.execute(input_channel_deliver, &ctx).await.unwrap();
    assert!(
        !res2.success,
        "set_goal with channel delivery must be rejected"
    );
    assert!(
        res2.error
            .unwrap_or_default()
            .contains("set_goal requires oc://session"),
        "error message must explain channel delivery is passive"
    );

    // 3. Create with set_goal = true and session delivery -> SUCCESS
    let input_session_deliver = json!({
        "action": "create",
        "name": "goal-job-3",
        "cron": "0 0 * * *",
        "tz": "UTC",
        "prompt": "do something",
        "deliver_to": "oc://session/12345678-1234-1234-1234-123456789abc",
        "set_goal": true
    });
    let res3 = tool.execute(input_session_deliver, &ctx).await.unwrap();
    assert!(
        res3.success,
        "set_goal with session delivery must succeed: {:?}",
        res3.error
    );
}

#[test]
fn delivery_target_validation_checks_channel_credential() {
    // In a test env keys.toml has no channel credentials — the telegram arm
    // must refuse exactly the way runtime would (feature-gated compile line
    // mirrors the scheduler's own cfg gate).
    #[cfg(feature = "telegram")]
    {
        let result = validate_delivery_target("telegram:12345");
        // Either the credential exists (Ok) or it doesn't (Err with reason) —
        // but it must agree with read_channel_secret, the same lookup the
        // delivery path uses.
        let secret = crate::cron::scheduler::read_channel_secret("telegram", "token");
        match (secret.is_some(), result.is_ok()) {
            (true, true) | (false, false) => {} // agreement
            (true, false) => panic!("validator refused a resolvable credential"),
            (false, true) => {
                panic!("validator accepted an unresolvable credential — silent drop (#107)")
            }
        }
    }
}
