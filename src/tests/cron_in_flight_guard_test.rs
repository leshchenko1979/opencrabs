//!
//! Verification tests for Issue #277:
//! In-flight guard prevents duplicate/concurrent executions of overrunning cron jobs.
//! When a job is due but a previous execution is still marked `running`, the scheduler
//! skips spawning a new execution and advances `next_run_at` to the subsequent boundary.

use crate::db::Database;
use crate::db::models::{CronJob, CronJobRun};
use crate::db::repository::{CronJobPatch, CronJobRepository, CronJobRunRepository};
use chrono::Utc;
use uuid::Uuid;

async fn test_db() -> (crate::db::Pool, CronJobRepository, CronJobRunRepository) {
    let db = Database::connect_in_memory().await.unwrap();
    db.run_migrations().await.unwrap();
    let pool = db.pool().clone();
    let job_repo = CronJobRepository::new(pool.clone());
    let run_repo = CronJobRunRepository::new(pool.clone());
    (pool, job_repo, run_repo)
}

fn make_test_job(name: &str, cron: &str) -> CronJob {
    CronJob::new(
        name.to_string(),
        cron.to_string(),
        "UTC".to_string(),
        "Test prompt".to_string(),
        None,
        None,
        "off".to_string(),
        true,
        None,
        None,
    )
}

#[tokio::test]
async fn test_has_running_job_detects_in_flight_status() {
    let (_pool, job_repo, run_repo) = test_db().await;
    let job = make_test_job("test-in-flight-guard", "*/5 * * * *");
    let job_id = job.id;
    job_repo.insert(&job).await.unwrap();

    // Initially no runs exist
    assert!(
        !run_repo.has_running_job(&job_id.to_string()).await.unwrap(),
        "New job must not have running status"
    );

    // Insert a running run
    let run_id = Uuid::new_v4();
    let run = CronJobRun {
        id: run_id,
        job_id,
        job_name: job.name.clone(),
        status: "running".to_string(),
        content: None,
        error: None,
        input_tokens: 0,
        output_tokens: 0,
        cost: 0.0,
        provider: None,
        model: None,
        started_at: Utc::now(),
        completed_at: None,
        created_at: Utc::now(),
    };
    run_repo.insert(&run).await.unwrap();

    // Now has_running_job must return true
    assert!(
        run_repo.has_running_job(&job_id.to_string()).await.unwrap(),
        "Active running job must return true for has_running_job"
    );

    // Complete the run with success
    run_repo
        .complete_success(&run_id.to_string(), "output", 10, 20, 0.001)
        .await
        .unwrap();

    // Now has_running_job must return false
    assert!(
        !run_repo.has_running_job(&job_id.to_string()).await.unwrap(),
        "Completed run must return false for has_running_job"
    );
}

#[tokio::test]
async fn test_cron_scheduler_skips_in_flight_job_and_advances_next_run() {
    let (_pool, job_repo, run_repo) = test_db().await;
    let mut job = make_test_job("test-overrunning-job", "* * * * *");
    let past_boundary = Utc::now() - chrono::Duration::minutes(10);
    job.next_run_at = Some(past_boundary);
    let job_id = job.id;
    job_repo.insert(&job).await.unwrap();

    // Mark an in-flight run
    let run = CronJobRun {
        id: Uuid::new_v4(),
        job_id,
        job_name: job.name.clone(),
        status: "running".to_string(),
        content: None,
        error: None,
        input_tokens: 0,
        output_tokens: 0,
        cost: 0.0,
        provider: None,
        model: None,
        started_at: past_boundary,
        completed_at: None,
        created_at: past_boundary,
    };
    run_repo.insert(&run).await.unwrap();

    // Verify in-flight guard sees it
    assert!(run_repo.has_running_job(&job_id.to_string()).await.unwrap());

    // Advance next_run_at as tick() would
    let now = Utc::now();
    let next_boundary = crate::cron::next_run_utc(&job.cron_expr, chrono_tz::UTC, now).unwrap();
    job_repo
        .update_fields(
            &job_id.to_string(),
            CronJobPatch {
                next_run_at: Some(Some(next_boundary)),
                ..Default::default()
            },
        )
        .await
        .unwrap();

    let updated_job = job_repo
        .find_by_id(&job_id.to_string())
        .await
        .unwrap()
        .unwrap();
    assert!(
        updated_job.next_run_at.unwrap() > now,
        "next_run_at must advance into the future when skipped due to in-flight execution"
    );
}
