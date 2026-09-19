//! #1114: `/doctor --fix` engine.
//!
//! Stuck cron rows clear, fresh ones survive; stale pre-init markers go,
//! fresh ones stay; permissions tighten only where loose.

use crate::cli::doctor_fix::{
    clear_stale_preinit_markers, clear_stuck_cron_runs, clear_stuck_cron_runs_with_owner,
    ClearPolicy, STUCK_CRON_MAX_AGE_SECS,
};
use crate::config::profile::InstanceOwner;
use crate::db::Database;
use rusqlite::params;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};
use tempfile::TempDir;
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
    let job_id = Uuid::new_v4().to_string();
    let ts = started_at.to_rfc3339();
    let parent_job_id = job_id.clone();
    pool.get()
        .await
        .unwrap()
        .interact(move |conn| {
            // Parent row first — cron_job_runs.job_id has an FK to cron_jobs(id).
            conn.execute(
                "INSERT INTO cron_jobs (id, name, cron_expr, timezone, prompt, thinking, \
                 auto_approve, enabled, created_at, updated_at) \
                 VALUES (?1, 'doctor-fix-test', '0 0 * * *', 'UTC', 'x', 'off', 1, 1, ?2, ?2)",
                params![parent_job_id, ts],
            )?;
            conn.execute(
                "INSERT INTO cron_job_runs (id, job_id, job_name, status, content, error, \
                 input_tokens, output_tokens, cost, provider, model, started_at, completed_at, created_at)
                 VALUES (?1, ?2, 'job', 'running', '', '', 0, 0, 0, '', '', ?3, NULL, ?3)",
                params![id, job_id, ts],
            )
        })
        .await
        .unwrap()
        .unwrap();
}

/// Status of one seeded run row.
async fn run_status(pool: &crate::db::Pool, id: &str) -> String {
    let id = id.to_string();
    pool.get()
        .await
        .unwrap()
        .interact(move |conn| {
            conn.query_row("SELECT status FROM cron_job_runs WHERE id = ?1", params![id], |r| {
                r.get::<_, String>(0)
            })
        })
        .await
        .unwrap()
        .unwrap()
}

/// Error text of one seeded run row (the audit trail of WHY it was closed).
async fn run_error(pool: &crate::db::Pool, id: &str) -> String {
    let id = id.to_string();
    pool.get()
        .await
        .unwrap()
        .interact(move |conn| {
            conn.query_row(
                "SELECT error FROM cron_job_runs WHERE id = ?1",
                params![id],
                |r| r.get::<_, String>(0),
            )
        })
        .await
        .unwrap()
        .unwrap()
}

#[tokio::test]
async fn test_clear_stuck_cron_runs_marks_interrupted() {
    let pool = test_db().await;
    let old = Uuid::new_v4().to_string();
    let fresh = Uuid::new_v4().to_string();
    seed_run(&pool, &old, chrono::Utc::now() - chrono::Duration::hours(3)).await;
    seed_run(&pool, &fresh, chrono::Utc::now()).await;

    // The PUBLIC entry point, driven with the startup policy: at startup every
    // running row is an orphan, so this verdict does not depend on which lock
    // the host happens to hold. The core's owner is injected in the tests
    // below, which is where the policy branches are pinned.
    let n = clear_stuck_cron_runs(&pool, ClearPolicy::OrphanedAtStartup, STUCK_CRON_MAX_AGE_SECS)
        .await
        .unwrap();
    assert_eq!(n, 2, "startup clears every running row, however fresh");

    assert_eq!(run_status(&pool, &old).await, "interrupted");
    assert_eq!(run_status(&pool, &fresh).await, "interrupted");
    let err = run_error(&pool, &old).await;
    assert!(
        err.contains("interrupted: cleared by doctor --fix"),
        "error message must reflect doctor fix interruption: {err}"
    );
    assert!(
        err.contains("orphaned"),
        "the reason must say WHY the row is dead (no live owner): {err}"
    );
}

fn make_marker(dir: &PathBuf, age: Option<Duration>) -> PathBuf {
    std::fs::create_dir_all(dir).unwrap();
    let p = dir.join(format!(".opencrabs_plan_{}.preinit", Uuid::new_v4()));
    std::fs::write(&p, "").unwrap();
    if let Some(age) = age {
        let aged = SystemTime::now() - age;
        let f = std::fs::File::open(&p).unwrap();
        f.set_modified(aged).unwrap();
    }
    p
}

#[test]
fn stale_preinit_markers_removed_fresh_kept() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("session");
    let stale = make_marker(&root, Some(Duration::from_secs(8 * 24 * 3600)));
    let fresh = make_marker(&root, None);
    // Unrelated files must never match the marker predicate.
    let noise = root.join(".opencrabs_plan_something.json");
    std::fs::write(&noise, "").unwrap();

    let removed = clear_stale_preinit_markers(&[root], Duration::from_secs(7 * 24 * 3600));
    assert_eq!(removed.len(), 1);
    assert_eq!(removed[0].detail, stale.display().to_string());
    assert!(!stale.exists(), "stale marker must be gone");
    assert!(fresh.exists(), "fresh marker must survive");
    assert!(noise.exists(), "non-marker sibling must survive");
}

#[test]
fn missing_roster_roots_are_silent_noops() {
    let removed = clear_stale_preinit_markers(
        &[PathBuf::from("/nonexistent/definitely-gone")],
        Duration::from_secs(60),
    );
    assert!(removed.is_empty());
}

#[cfg(unix)]
#[test]
fn loose_permissions_tightened_only() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = TempDir::new().unwrap();
    let home = tmp.path();
    std::fs::create_dir_all(home.join("brain")).unwrap();
    std::fs::create_dir_all(home.join("logs")).unwrap();

    let loose = home.join("brain/MEMORY.md");
    std::fs::write(&loose, "x").unwrap();
    std::fs::set_permissions(&loose, std::fs::Permissions::from_mode(0o644)).unwrap();
    let tight = home.join("logs/opencrabs.today");
    std::fs::write(&tight, "x").unwrap();
    std::fs::set_permissions(&tight, std::fs::Permissions::from_mode(0o600)).unwrap();

    let fixed = crate::cli::doctor_fix::fix_brain_log_permissions(home);
    assert_eq!(fixed.len(), 1);
    assert_eq!(fixed[0].detail, loose.display().to_string());
    let mode = std::fs::metadata(&loose).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600);
}

// ---------------------------------------------------------------------------
// #332 · Step 4 — the instance-owner (orphanhood) probe
//
// `instance_owner` is what lets the stuck-run clear policy stop guessing from
// AGE: a process that has just taken the instance lock cannot be executing a
// `cron_job_runs` row written before it started, so a pre-existing running row
// is an orphan however young it is (#332, D4).
//
// These drive the dir-injectable core against a TempDir — NOT
// `with_home_override`: `base_opencrabs_dir()` resolves `dirs::home_dir()` and
// never consults the override, so the override cannot point the PUBLIC probe at
// a temp home. The core is exactly what that probe delegates to, one read and
// one classification, and no live lock file is written either way.
// ---------------------------------------------------------------------------

fn write_lock(dir: &std::path::Path, profile: &str, contents: &str) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(dir.join(format!("{profile}.lock")), contents).unwrap();
}

#[test]
fn instance_owner_is_self_for_our_own_pid() {
    use crate::config::profile::{InstanceOwner, instance_owner_in, instance_running_in};

    let tmp = TempDir::new().unwrap();
    let dir = tmp.path().join("locks/instance");
    write_lock(&dir, "ops", &std::process::id().to_string());

    assert_eq!(instance_owner_in(&dir, "ops"), InstanceOwner::Self_);
    // The bool probe is the same read, so the two cannot drift apart.
    assert!(instance_running_in(&dir, "ops"));
}

#[cfg(unix)]
#[test]
fn instance_owner_is_other_for_a_live_foreign_pid() {
    use crate::config::profile::{InstanceOwner, instance_owner_in, instance_running_in};

    let tmp = TempDir::new().unwrap();
    let dir = tmp.path().join("locks/instance");
    // PID 1 is the init process: always alive, never this test process.
    assert_ne!(std::process::id(), 1);
    write_lock(&dir, "ops", "1");

    assert_eq!(instance_owner_in(&dir, "ops"), InstanceOwner::Other(1));
    assert!(instance_running_in(&dir, "ops"));
}

#[test]
fn instance_owner_is_none_for_a_missing_lock_file() {
    use crate::config::profile::{InstanceOwner, instance_owner_in, instance_running_in};

    let tmp = TempDir::new().unwrap();
    let dir = tmp.path().join("locks/instance");
    std::fs::create_dir_all(&dir).unwrap();

    assert_eq!(instance_owner_in(&dir, "ops"), InstanceOwner::None);
    assert!(!instance_running_in(&dir, "ops"));
}

#[cfg(unix)]
#[test]
fn instance_owner_is_none_for_a_dead_pid() {
    use crate::config::profile::{InstanceOwner, instance_owner_in};

    // A reaped child's PID is a real PID that is genuinely no longer alive —
    // unlike a made-up number like u32::MAX, which never was one.
    let mut child = std::process::Command::new("true").spawn().unwrap();
    let dead = child.id();
    child.wait().unwrap();

    let tmp = TempDir::new().unwrap();
    let dir = tmp.path().join("locks/instance");
    write_lock(&dir, "ops", &dead.to_string());

    assert_eq!(instance_owner_in(&dir, "ops"), InstanceOwner::None);
}

#[test]
fn instance_owner_is_none_for_an_unparsable_or_impossible_stamp() {
    use crate::config::profile::{InstanceOwner, instance_owner_in};

    let tmp = TempDir::new().unwrap();
    let dir = tmp.path().join("locks/instance");

    for stamp in ["", "   ", "not-a-pid", "0"] {
        write_lock(&dir, "ops", stamp);
        assert_eq!(
            instance_owner_in(&dir, "ops"),
            InstanceOwner::None,
            "stamp {stamp:?} names no live owner"
        );
    }
}

#[test]
fn instance_owner_answers_only_for_the_named_profile() {
    use crate::config::profile::{InstanceOwner, instance_owner_in};

    let tmp = TempDir::new().unwrap();
    let dir = tmp.path().join("locks/instance");
    write_lock(&dir, "ops", &std::process::id().to_string());

    // A lock for another profile says nothing about this one: the probe must
    // never adopt a sibling's owner.
    assert_eq!(instance_owner_in(&dir, "family"), InstanceOwner::None);
}

// ---------------------------------------------------------------------------
// #332 · Step 5 — the clear policy: orphanhood decides, age is only a backstop
//
// `clear_stuck_cron_runs_with_owner` is the owner-injectable core of
// `clear_stuck_cron_runs`; these drive it directly, so no test writes or reads
// the live `~/.opencrabs/locks/instance/<profile>.lock`.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn startup_policy_clears_a_fresh_row_under_a_live_owner() {
    let pool = test_db().await;
    let fresh = Uuid::new_v4().to_string();
    seed_run(&pool, &fresh, chrono::Utc::now()).await;

    // Startup consults neither the owner nor the age: a process that has just
    // taken the instance lock cannot own a pre-existing row (#332, D4).
    let n = clear_stuck_cron_runs_with_owner(
        &pool,
        ClearPolicy::OrphanedAtStartup,
        InstanceOwner::Other(1),
        STUCK_CRON_MAX_AGE_SECS,
    )
    .await
    .unwrap();

    assert_eq!(n, 1, "a fresh row is an orphan at startup");
    assert_eq!(run_status(&pool, &fresh).await, "interrupted");
}

#[tokio::test]
async fn conservative_policy_leaves_a_fresh_row_alone_under_a_live_owner() {
    let pool = test_db().await;
    let fresh = Uuid::new_v4().to_string();
    // 3h is inside the 4h backstop but PAST the old 1h one, so this row is
    // exactly the live work the age-only sweep used to kill.
    let long_run = Uuid::new_v4().to_string();
    seed_run(&pool, &fresh, chrono::Utc::now()).await;
    seed_run(
        &pool,
        &long_run,
        chrono::Utc::now() - chrono::Duration::hours(3),
    )
    .await;

    let n = clear_stuck_cron_runs_with_owner(
        &pool,
        ClearPolicy::Conservative,
        InstanceOwner::Other(1),
        STUCK_CRON_MAX_AGE_SECS,
    )
    .await
    .unwrap();

    assert_eq!(STUCK_CRON_MAX_AGE_SECS, 14400, "backstop is 4h");
    assert_eq!(n, 0, "a live owner means neither row is provably dead");
    assert_eq!(run_status(&pool, &fresh).await, "running");
    assert_eq!(run_status(&pool, &long_run).await, "running");
}

#[tokio::test]
async fn conservative_policy_still_clears_a_row_past_the_backstop() {
    let pool = test_db().await;
    let residue = Uuid::new_v4().to_string();
    let fresh = Uuid::new_v4().to_string();
    seed_run(
        &pool,
        &residue,
        chrono::Utc::now() - chrono::Duration::hours(5),
    )
    .await;
    seed_run(&pool, &fresh, chrono::Utc::now()).await;

    let n = clear_stuck_cron_runs_with_owner(
        &pool,
        ClearPolicy::Conservative,
        InstanceOwner::Other(1),
        STUCK_CRON_MAX_AGE_SECS,
    )
    .await
    .unwrap();

    assert_eq!(n, 1, "past the backstop, even a live box reclaims the row");
    assert_eq!(run_status(&pool, &residue).await, "interrupted");
    assert_eq!(run_status(&pool, &fresh).await, "running");
    assert!(
        run_error(&pool, &residue).await.contains("max age"),
        "the aged leg must say it was AGE, not orphanhood"
    );
}

#[tokio::test]
async fn conservative_policy_clears_fresh_rows_when_no_instance_owns_the_profile() {
    let pool = test_db().await;
    let fresh = Uuid::new_v4().to_string();
    seed_run(&pool, &fresh, chrono::Utc::now()).await;

    let n = clear_stuck_cron_runs_with_owner(
        &pool,
        ClearPolicy::Conservative,
        InstanceOwner::None,
        STUCK_CRON_MAX_AGE_SECS,
    )
    .await
    .unwrap();

    assert_eq!(n, 1, "nobody owns the profile, so nothing can be running");
    assert_eq!(run_status(&pool, &fresh).await, "interrupted");
}

#[tokio::test]
async fn conservative_policy_counts_our_own_lock_as_a_live_owner() {
    let pool = test_db().await;
    let fresh = Uuid::new_v4().to_string();
    seed_run(&pool, &fresh, chrono::Utc::now()).await;

    let n = clear_stuck_cron_runs_with_owner(
        &pool,
        ClearPolicy::Conservative,
        InstanceOwner::Self_,
        STUCK_CRON_MAX_AGE_SECS,
    )
    .await
    .unwrap();

    assert_eq!(n, 0, "our own lock is a live owner: the row may be ours");
    assert_eq!(run_status(&pool, &fresh).await, "running");
}
