//! #107: cron result-delivery failures must never masquerade as success.
//!
//! The run row carries the outcome distinctly: `complete_delivery_failed`
//! stamps `status='delivery_failed'` with the reason, leaves an execution
//! `error` row untouched (no-overwrite guard), and never flips an already-
//! completed row's `content`. The delivery path's fail-fast validator
//! (`validate_delivery_target`) rejects targets that would silently drop
//! every future run's result before a job ever goes live.

use crate::brain::tools::cron_manage::validate_delivery_target;
use crate::config::profile::with_home_override_async;
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

#[tokio::test]
async fn delivery_target_validation_checks_channel_credential() {
    // Hermetic (#341): a throwaway home carrying NO channel credential in
    // either file. The telegram arm must refuse exactly the way runtime
    // would (feature-gated compile line mirrors the scheduler's own cfg
    // gate). This used to run against the live profile, which made the
    // outcome depend on whatever credential happened to be on the box —
    // the assertion is about AGREEMENT between validator and resolver, so
    // the home must be one the test controls.
    let dir = tempfile::tempdir().expect("tempdir");
    let home = dir.path().join(".opencrabs");
    std::fs::create_dir_all(&home).expect("create home");
    std::fs::write(home.join("config.toml"), "").expect("write config");
    std::fs::write(home.join("keys.toml"), "").expect("write keys");

    with_home_override_async(home, async {
        #[cfg(feature = "telegram")]
        {
            // Guard: the home must be LOADABLE. `read_channel_secret` maps a
            // failed load to None via `.ok()?`, so without this the test
            // would pass for the wrong reason — a broken config load would
            // look identical to an absent credential.
            assert!(
                crate::config::Config::load().is_ok(),
                "temp home must load cleanly, else the None below proves nothing"
            );
            let result = validate_delivery_target("telegram:12345");
            // The same lookup the delivery path uses, so the two can never
            // disagree about whether this target is deliverable.
            let secret = crate::cron::scheduler::read_channel_secret("telegram", "token");
            assert!(
                secret.is_none(),
                "an empty home must resolve no credential, got {secret:?}"
            );
            assert!(
                result.is_err(),
                "validator accepted an unresolvable credential — silent drop (#107)"
            );
        }
    })
    .await;
}

/// A placeholder must never be handed to a delivery client as a credential.
///
/// Premise correction: the issue text calls this a "stored: marker", but no
/// such string exists in the codebase (`grep -rn '"stored:' src/` → 0 hits).
/// The real sentinel is [`crate::config::stored_key::EXISTING_KEY_SENTINEL`]
/// (`__EXISTING_KEY__`), the placeholder a secret input shows when a key is
/// already on disk. That value is never a credential, so it must resolve to
/// `None` — and a sentinel left glued to a real key (the #1075 shape) must
/// resolve to the SANITISED key, never the marker-prefixed string.
#[cfg(feature = "telegram")]
#[tokio::test]
async fn channel_credential_rejects_placeholders() {
    use crate::config::stored_key::EXISTING_KEY_SENTINEL;

    let dir = tempfile::tempdir().expect("tempdir");
    let home = dir.path().join(".opencrabs");
    std::fs::create_dir_all(&home).expect("create home");

    // Empty token in config.toml, nothing in keys.toml.
    std::fs::write(
        home.join("config.toml"),
        "[channels.telegram]\ntoken = \"\"\n",
    )
    .expect("write config");
    std::fs::write(home.join("keys.toml"), "").expect("write keys");
    with_home_override_async(home.clone(), async {
        assert!(
            crate::config::Config::load().is_ok(),
            "temp home must load cleanly, else the None below proves nothing"
        );
        assert_eq!(
            crate::cron::scheduler::read_channel_secret("telegram", "token"),
            None,
            "an empty token is not a credential"
        );
        assert!(
            validate_delivery_target("telegram:12345").is_err(),
            "validator must refuse a target whose credential is empty"
        );
    })
    .await;

    // Bare sentinel in config.toml — "a key is set" is a DISPLAY state, not a
    // credential.
    std::fs::write(
        home.join("config.toml"),
        format!("[channels.telegram]\ntoken = \"{EXISTING_KEY_SENTINEL}\"\n"),
    )
    .expect("write config");
    with_home_override_async(home.clone(), async {
        assert_eq!(
            crate::cron::scheduler::read_channel_secret("telegram", "token"),
            None,
            "the existing-key marker is never a credential"
        );
    })
    .await;

    // Sentinel glued to a real key (#1075 shape): the SANITISED key is
    // returned, never the marker-prefixed string — handing the marker to a
    // delivery client would be the exact bug stored_key exists to close.
    std::fs::write(
        home.join("config.toml"),
        format!("[channels.telegram]\ntoken = \"{EXISTING_KEY_SENTINEL}123456789:AAreal\"\n"),
    )
    .expect("write config");
    with_home_override_async(home, async {
        assert_eq!(
            crate::cron::scheduler::read_channel_secret("telegram", "token").as_deref(),
            Some("123456789:AAreal"),
            "a sentinel-prefixed real key must resolve SANITISED (#1075)"
        );
    })
    .await;
}

/// The #341 regression, exactly: the token lives in `config.toml` ONLY.
///
/// Before the fix `read_channel_secret` parsed keys.toml directly and
/// returned `None` here, so every cron delivery died silently
/// (`delivery_failed`, job still reading healthy) while the live channel —
/// which reads the merged config — answered in chats normally. 27 such rows
/// over nine days on profile `default`.
#[cfg(feature = "telegram")]
#[tokio::test]
async fn channel_credential_resolves_from_config_toml_only() {
    let dir = tempfile::tempdir().expect("tempdir");
    let home = dir.path().join(".opencrabs");
    std::fs::create_dir_all(&home).expect("create home");
    // config.toml carries the token; keys.toml exists but is EMPTY — the
    // layout that produced the outage. keys.toml is written (rather than
    // omitted) because that is what the affected box actually looked like.
    std::fs::write(
        home.join("config.toml"),
        "[channels.telegram]\ntoken = \"123456789:AAconfigonly\"\n",
    )
    .expect("write config");
    std::fs::write(home.join("keys.toml"), "").expect("write keys");

    with_home_override_async(home, async {
        let resolved = crate::cron::scheduler::read_channel_secret("telegram", "token");
        assert_eq!(
            resolved.as_deref(),
            Some("123456789:AAconfigonly"),
            "a token present in config.toml ONLY must resolve — this is the #341 regression"
        );
        // The validator consumes the same lookup, so it must accept the
        // target now instead of refusing a working credential.
        assert!(
            validate_delivery_target("telegram:12345").is_ok(),
            "validator must accept the target when the credential resolves"
        );
    })
    .await;
}

/// #435: the SESSION arm was the one arm that lost its failure.
///
/// Every other arm of `deliver_result` calls `record_delivery_failure` before
/// it gives up (#107), so a job whose result went nowhere leaves a
/// `delivery_failed` row. The session arm logged `Invalid session deliver_to
/// target … no session matches` and returned, leaving the run row reading
/// `success` — the job looked delivered while its output was dropped. Caught
/// live: a probe job burned 140 657 tokens, logged `completed`, and the
/// delivery error sat in the daemon log with no corresponding row.
///
/// Fails against the pre-fix tree, where the row stays `running`.
#[tokio::test]
async fn unresolvable_session_target_records_delivery_failed() {
    let pool = test_db().await;
    let run_id = format!("{}-435", Uuid::new_v4());
    seed_run(&pool, &run_id, chrono::Utc::now()).await;

    // The failure path bails before any session machinery is touched, so this
    // needs no live agent: a `session:` target whose id matches no row.
    let out = crate::cron::scheduler::deliver_result(
        "session:zzzzzzzz",
        "435-test",
        "body",
        None,
        Some(pool.clone()),
        Some(run_id.clone()),
    )
    .await;
    assert!(out.is_none(), "the session arm never hands back a task handle");

    let run_id_owned = run_id.clone();
    let (status, error) = pool
        .get()
        .await
        .unwrap()
        .interact(move |conn| {
            conn.query_row(
                "SELECT status, error FROM cron_job_runs WHERE id = ?1",
                params![run_id_owned],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?)),
            )
        })
        .await
        .unwrap()
        .unwrap();

    assert_eq!(
        status, "delivery_failed",
        "a session target that resolves to nothing must not leave the run reading success"
    );
    let error = error.expect("a delivery_failed row carries its reason");
    assert!(
        error.contains("no session matches"),
        "the reason names the cause: {error}"
    );
}
