//! `cron_jobs.deliver_mode` splits two decisions `deliver_to` used to fuse (#969).
//!
//! Before this column, `deliver_to` was the single input to BOTH:
//!
//! 1. **send permission** — `cron_job_scope_async(pool, deliver_to)` yields
//!    `Permitted` only for a non-empty target list, else `Nowhere(Undeclared)`,
//!    and `telegram_send` is refused under `Nowhere`;
//! 2. **harness auto-delivery** — the scheduler posts the turn's final text
//!    (and an agent error) to every `deliver_to` target.
//!
//! So a declared job both could send AND was posted unconditionally (spam on
//! quiet days), and an undeclared job had no permission at all.
//!
//! `deliver_mode = "silent"` turns OFF (2) and leaves (1) untouched. The tests
//! below pin each half separately: the scope test would still pass if `silent`
//! quietly meant `Nowhere`, and the delivery test would still pass if `silent`
//! quietly meant "no permission" — only both together say the split is real.

use crate::db::CronJobRepository;
use crate::db::Database;
use crate::db::models::CronJob;

/// A job as it would be stored: `deliver_to` declared, mode as given.
fn job_with(name: &str, deliver_to: Option<&str>, deliver_mode: &str) -> CronJob {
    let mut job = CronJob::new(
        name.to_string(),
        "0 9 * * *".to_string(),
        "UTC".to_string(),
        "do the thing".to_string(),
        None,
        None,
        "off".to_string(),
        true,
        deliver_to.map(str::to_string),
        None,
    );
    job.deliver_mode = deliver_mode.to_string();
    job
}

async fn fresh_db() -> (Database, CronJobRepository) {
    let db = Database::connect_in_memory().await.unwrap();
    db.run_migrations().await.unwrap();
    let repo = CronJobRepository::new(db.pool().clone());
    (db, repo)
}

// ---------------------------------------------------------------------------
// 1. Scope independence — `silent` is NOT `Nowhere`.
// ---------------------------------------------------------------------------

/// The property the whole change rests on: a `silent` job keeps the send scope
/// its `deliver_to` earns. If a later refactor made `silent` collapse the scope
/// to `Nowhere`, the job would lose the very capability that makes silence
/// useful (deciding FOR ITSELF what to post) — so pin it here.
#[tokio::test]
async fn silent_job_keeps_its_declared_send_scope() {
    use crate::cron::send_scope::{
        NoTargetReason, PermittedTarget, SendScope, cron_job_scope_async,
    };

    let (db, repo) = fresh_db().await;
    let job = job_with(
        "silent but permitted",
        Some("telegram:-1003985199158"),
        "silent",
    );
    repo.insert(&job).await.unwrap();

    let stored = repo
        .find_by_name("silent but permitted")
        .await
        .unwrap()
        .expect("job must round-trip");
    assert_eq!(stored.deliver_mode, "silent");
    assert!(
        !stored.delivers_result(),
        "a silent job must not auto-deliver"
    );

    let scope = cron_job_scope_async(db.pool(), stored.deliver_to.as_deref()).await;
    assert_eq!(
        scope,
        SendScope::Permitted(vec![PermittedTarget {
            channel: "telegram",
            target_id: "-1003985199158".to_string(),
        }]),
        "deliver_mode = silent must NOT narrow the send scope (#969)"
    );

    // The contrast the scope test exists to distinguish: an UNDECLARED job is
    // Nowhere, a silent-but-declared one is Permitted. Same code path, so the
    // two answers come from `deliver_to` alone.
    let undeclared = job_with("no target", None, "silent");
    assert_eq!(
        cron_job_scope_async(db.pool(), undeclared.deliver_to.as_deref()).await,
        SendScope::Nowhere(NoTargetReason::Undeclared),
    );
}

// ---------------------------------------------------------------------------
// 2. Delivery gating — silent posts ZERO, always posts exactly ONE.
// ---------------------------------------------------------------------------

/// The harness's auto-post list is what all three scheduler arms iterate, so
/// this one function decides "posts nothing" vs "posts exactly one message per
/// declared target" for the agent-success arm, the agent-error arm and the
/// direct-trigger arm alike.
///
/// The three arms are NOT individually reachable from a test — each sits inside
/// `execute_job` / `execute_direct_trigger_job` behind a live `ServiceContext`
/// and a model turn. So the arms' shared decision point is tested directly, and
/// the structural test below pins that all three still go through it.
#[tokio::test]
async fn silent_posts_nothing_always_posts_once() {
    let silent = job_with("quiet", Some("telegram:-1003985199158"), "silent");
    assert!(
        crate::cron::scheduler::auto_delivery_targets(&silent).is_empty(),
        "a silent job must produce ZERO auto-delivery targets, even with a \
         non-empty deliver_to"
    );

    let always = job_with("loud", Some("telegram:-1003985199158"), "always");
    assert_eq!(
        crate::cron::scheduler::auto_delivery_targets(&always),
        vec!["telegram:-1003985199158"],
        "an always job must post exactly ONE message per declared target"
    );

    // Comma-separated targets keep working, one post each.
    let multi = job_with("two", Some("telegram:1, telegram:2"), "always");
    assert_eq!(
        crate::cron::scheduler::auto_delivery_targets(&multi),
        vec!["telegram:1", "telegram:2"]
    );

    // A silent job with several targets still posts nothing.
    let silent_multi = job_with("quiet two", Some("telegram:1, telegram:2"), "silent");
    assert!(crate::cron::scheduler::auto_delivery_targets(&silent_multi).is_empty());

    // And a targetless job is empty in both modes — the pre-#969 behaviour.
    assert!(
        crate::cron::scheduler::auto_delivery_targets(&job_with("none", None, "always")).is_empty()
    );
}

/// Every scheduler delivery arm must go through `auto_delivery_targets`.
///
/// The three arms are unreachable from a unit test (see above), so the one thing
/// a test CAN check is that none of them grew back its own ungated
/// `for target in deliver_to.split(…)` loop — that is exactly how the error arm
/// would leak `Cron job '<name>' failed: <e>` into a silent job's chat.
#[test]
fn all_three_scheduler_arms_share_the_gate() {
    let src = include_str!("../cron/scheduler.rs");

    // Three arms (agent success, agent error, direct trigger) + the definition.
    assert_eq!(
        src.matches("for target in auto_delivery_targets(job)")
            .count(),
        3,
        "the agent-success, agent-error and direct-trigger arms must all iterate \
         auto_delivery_targets(job); a fourth site or a missing one means an arm \
         bypasses the #969 gate"
    );

    // Every actual call to `deliver_result(` is inside one of those loops.
    let calls = src.matches("let _ = deliver_result(").count();
    assert_eq!(
        calls, 3,
        "expected exactly three harness delivery call sites (one per arm), found {calls}"
    );
}

// ---------------------------------------------------------------------------
// 3. CLI validation.
// ---------------------------------------------------------------------------

/// `--deliver-mode` is validated, not coerced: a typo must not silently choose
/// `always`, because that would re-enable exactly the posting the operator was
/// trying to turn off.
///
/// `main` returns `anyhow::Result`, so the `Err` asserted here IS the non-zero
/// exit (`anyhow::bail!` → `Err` → process exit 1). The clap layer accepts any
/// string for the flag and hands it to this validator, which is why both halves
/// are asserted: parsing succeeds, validation rejects.
#[test]
fn cli_rejects_unknown_deliver_mode_and_defaults_to_always() {
    use crate::cli::cron::parse_deliver_mode;
    use crate::cli::{Cli, Commands, CronCommands};
    use clap::Parser;

    // Absent => `always`.
    assert_eq!(parse_deliver_mode(None).unwrap(), "always");
    // The two accepted values pass through.
    assert_eq!(parse_deliver_mode(Some("always")).unwrap(), "always");
    assert_eq!(parse_deliver_mode(Some("silent")).unwrap(), "silent");
    // Anything else is a hard error.
    let err = parse_deliver_mode(Some("bogus")).unwrap_err().to_string();
    assert!(
        err.contains("bogus") && err.contains("always") && err.contains("silent"),
        "the rejection must name the bad value and both valid ones, got: {err}"
    );
    assert!(
        parse_deliver_mode(Some("Silent")).is_err(),
        "case-sensitive"
    );
    assert!(parse_deliver_mode(Some("")).is_err(), "blank is not a mode");

    // The flag reaches the Add variant as a plain string, and clap does not
    // reject it before validation runs (so the error above is the one the user
    // sees, not clap's).
    let cli = Cli::try_parse_from([
        "opencrabs",
        "cron",
        "add",
        "--name",
        "Test",
        "--cron",
        "0 9 * * *",
        "--prompt",
        "Hello",
        "--deliver-mode",
        "bogus",
    ])
    .expect("clap accepts any --deliver-mode value; the validator is the gate");
    match cli.command {
        Some(Commands::Cron {
            operation: CronCommands::Add { deliver_mode, .. },
        }) => assert_eq!(deliver_mode.as_deref(), Some("bogus")),
        _ => panic!("expected cron add"),
    }

    // Absent from the command line => `None`, which `parse_deliver_mode` maps
    // to `always`.
    let cli = Cli::try_parse_from([
        "opencrabs",
        "cron",
        "add",
        "--name",
        "Test",
        "--cron",
        "0 9 * * *",
        "--prompt",
        "Hello",
    ])
    .unwrap();
    match cli.command {
        Some(Commands::Cron {
            operation: CronCommands::Add { deliver_mode, .. },
        }) => {
            assert_eq!(deliver_mode, None);
            assert_eq!(
                parse_deliver_mode(deliver_mode.as_deref()).unwrap(),
                "always"
            );
        }
        _ => panic!("expected cron add"),
    }
}

// ---------------------------------------------------------------------------
// 4. Default preservation.
// ---------------------------------------------------------------------------

/// A row written by a build that predates the column must read `always`, not
/// fail and not read empty. `from_row` reads it through
/// `Option<String>` + `unwrap_or(None)` for exactly this case: the table below
/// has NO `deliver_mode` column at all.
#[tokio::test]
async fn legacy_row_without_the_column_reads_always() {
    let db = Database::connect_in_memory().await.unwrap();
    db.pool()
        .get()
        .await
        .unwrap()
        .interact(|conn| {
            conn.execute_batch(
                "CREATE TABLE cron_jobs (
                    id TEXT PRIMARY KEY NOT NULL,
                    name TEXT NOT NULL,
                    cron_expr TEXT NOT NULL,
                    timezone TEXT NOT NULL DEFAULT 'UTC',
                    prompt TEXT NOT NULL,
                    provider TEXT,
                    model TEXT,
                    thinking TEXT NOT NULL DEFAULT 'off',
                    auto_approve INTEGER NOT NULL DEFAULT 1,
                    deliver_to TEXT,
                    deliver_api_key TEXT,
                    enabled INTEGER NOT NULL DEFAULT 1,
                    last_run_at TEXT,
                    next_run_at TEXT,
                    created_at TEXT NOT NULL,
                    updated_at TEXT NOT NULL,
                    profile_name TEXT,
                    trigger_cmd TEXT,
                    trigger_on TEXT,
                    set_goal INTEGER NOT NULL DEFAULT 0,
                    goal_template TEXT,
                    run_once INTEGER NOT NULL DEFAULT 0
                );
                INSERT INTO cron_jobs (id, name, cron_expr, timezone, prompt, thinking,
                    auto_approve, deliver_to, enabled, created_at, updated_at)
                VALUES ('11111111-1111-1111-1111-111111111111', 'pre-969 row',
                    '0 9 * * *', 'UTC', 'legacy prompt', 'off', 1,
                    'telegram:-1003985199158', 1,
                    '2026-05-01T00:00:00Z', '2026-05-01T00:00:00Z');",
            )
        })
        .await
        .unwrap()
        .unwrap();

    let jobs = CronJobRepository::new(db.pool().clone())
        .list_all()
        .await
        .expect("a row without the deliver_mode column must still decode");
    assert_eq!(jobs.len(), 1);
    assert_eq!(
        jobs[0].deliver_mode, "always",
        "a pre-#969 row must read `always` — that is what keeps it posting as before"
    );
    assert!(jobs[0].delivers_result());
}

/// The migration itself defaults the column to `always`: a raw INSERT that
/// omits it — the shape every pre-#969 writer produced — lands as `always` on
/// the real migrated schema, and the constructor's default matches.
#[tokio::test]
async fn migrated_schema_defaults_new_rows_to_always() {
    let (db, repo) = fresh_db().await;

    // The constructor, touched exactly as a pre-#969 caller touches it.
    let job = CronJob::new(
        "defaulted".to_string(),
        "0 9 * * *".to_string(),
        "UTC".to_string(),
        "prompt".to_string(),
        None,
        None,
        "off".to_string(),
        true,
        Some("telegram:-1003985199158".to_string()),
        None,
    );
    assert_eq!(job.deliver_mode, "always", "ctor default");

    // And the column's own DEFAULT, with the column omitted entirely.
    db.pool()
        .get()
        .await
        .unwrap()
        .interact(|conn| {
            conn.execute(
                "INSERT INTO cron_jobs (id, name, cron_expr, timezone, prompt, thinking,
                    auto_approve, deliver_to, enabled, created_at, updated_at)
                 VALUES ('22222222-2222-2222-2222-222222222222', 'raw insert',
                    '0 9 * * *', 'UTC', 'legacy prompt', 'off', 1,
                    'telegram:-1003985199158', 1,
                    '2026-05-01T00:00:00Z', '2026-05-01T00:00:00Z')",
                [],
            )
        })
        .await
        .unwrap()
        .unwrap();

    let stored = repo.find_by_name("raw insert").await.unwrap().unwrap();
    assert_eq!(
        stored.deliver_mode, "always",
        "the column DEFAULT must be `always` so an omitting writer keeps posting"
    );
    assert!(stored.delivers_result());
}
