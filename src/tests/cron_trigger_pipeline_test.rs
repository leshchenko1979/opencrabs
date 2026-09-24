//! Tests for trigger-gated cron pipeline with mechanical pre-flight and goal dispatch (issue #233).

use crate::cron::pipeline::{PipelineExecutor, TriggerOutcome, interpolate_template};
use crate::cron::trigger::{TriggerCondition, TriggerResult, TriggerRunner};
use crate::db::models::CronJob;

#[test]
fn test_trigger_condition_parse() {
    assert_eq!(TriggerCondition::parse(None), TriggerCondition::NonEmpty);
    assert_eq!(
        TriggerCondition::parse(Some("non_empty")),
        TriggerCondition::NonEmpty
    );
    assert_eq!(
        TriggerCondition::parse(Some("exit_non_zero")),
        TriggerCondition::ExitNonZero
    );
    assert_eq!(
        TriggerCondition::parse(Some("exit_zero")),
        TriggerCondition::ExitZero
    );
    assert_eq!(
        TriggerCondition::parse(Some("exitzero")),
        TriggerCondition::ExitZero
    );
    assert_eq!(
        TriggerCondition::parse(Some("regex:disk [0-9]+%")),
        TriggerCondition::Regex("disk [0-9]+%".into())
    );
    assert_eq!(
        TriggerCondition::parse(Some("re:ERROR.*")),
        TriggerCondition::Regex("ERROR.*".into())
    );
    assert_eq!(
        TriggerCondition::parse(Some("always")),
        TriggerCondition::Always
    );
    assert_eq!(
        TriggerCondition::parse(Some("unknown_custom")),
        TriggerCondition::NonEmpty
    );
}

#[test]
fn test_trigger_condition_exit_zero() {
    let cond = TriggerCondition::ExitZero;

    let res_success = TriggerResult {
        stdout: "ok".into(),
        stderr: String::new(),
        exit_code: 0,
    };
    assert!(cond.should_fire(&res_success));

    let res_failure = TriggerResult {
        stdout: String::new(),
        stderr: "err".into(),
        exit_code: 1,
    };
    assert!(!cond.should_fire(&res_failure));
}

#[test]
fn test_trigger_condition_regex() {
    let cond = TriggerCondition::Regex("alert:\\s*([0-9]+)".into());

    let res_match = TriggerResult {
        stdout: "alert: 42 anomalies detected".into(),
        stderr: String::new(),
        exit_code: 0,
    };
    assert!(cond.should_fire(&res_match));

    let res_no_match = TriggerResult {
        stdout: "all systems normal".into(),
        stderr: String::new(),
        exit_code: 0,
    };
    assert!(!cond.should_fire(&res_no_match));
}

#[test]
fn test_trigger_condition_non_empty() {
    let cond = TriggerCondition::NonEmpty;

    let res_empty = TriggerResult {
        stdout: String::new(),
        stderr: String::new(),
        exit_code: 0,
    };
    assert!(!cond.should_fire(&res_empty));

    let res_stdout = TriggerResult {
        stdout: "found 1 item".into(),
        stderr: String::new(),
        exit_code: 0,
    };
    assert!(cond.should_fire(&res_stdout));

    let res_stderr = TriggerResult {
        stdout: String::new(),
        stderr: "warning: disk usage high".into(),
        exit_code: 0,
    };
    assert!(cond.should_fire(&res_stderr));

    let res_whitespace_only = TriggerResult {
        stdout: "   \n\t  ".into(),
        stderr: String::new(),
        exit_code: 0,
    };
    assert!(!cond.should_fire(&res_whitespace_only));
}

#[test]
fn test_trigger_condition_exit_non_zero() {
    let cond = TriggerCondition::ExitNonZero;

    let res_zero = TriggerResult {
        stdout: "some output".into(),
        stderr: String::new(),
        exit_code: 0,
    };
    assert!(!cond.should_fire(&res_zero));

    let res_err = TriggerResult {
        stdout: String::new(),
        stderr: "failure".into(),
        exit_code: 1,
    };
    assert!(cond.should_fire(&res_err));

    let res_exit_2 = TriggerResult {
        stdout: String::new(),
        stderr: String::new(),
        exit_code: 2,
    };
    assert!(cond.should_fire(&res_exit_2));
}

#[test]
fn test_trigger_condition_always() {
    let cond = TriggerCondition::Always;

    let res_empty = TriggerResult {
        stdout: String::new(),
        stderr: String::new(),
        exit_code: 0,
    };
    assert!(cond.should_fire(&res_empty));

    let res_err = TriggerResult {
        stdout: "err".into(),
        stderr: String::new(),
        exit_code: 127,
    };
    assert!(cond.should_fire(&res_err));
}

#[test]
fn test_interpolate_template() {
    let result = TriggerResult {
        stdout: "line 1\nline 2".into(),
        stderr: "warn".into(),
        exit_code: 0,
    };

    let tmpl = "Output:\n{output}\nStdout: {stdout}\nStderr: {stderr}\nCode: {exit_code}";
    let interpolated = interpolate_template(tmpl, &result);

    assert!(interpolated.contains("Output:\nline 1\nline 2\nwarn"));
    assert!(interpolated.contains("Stdout: line 1\nline 2"));
    assert!(interpolated.contains("Stderr: warn"));
    assert!(interpolated.contains("Code: 0"));
}

#[tokio::test]
async fn test_trigger_runner_echo() {
    let runner = TriggerRunner::default();
    let res = runner.run("echo 'trigger test'").await.expect("run echo");
    assert_eq!(res.exit_code, 0);
    assert_eq!(res.stdout.trim(), "trigger test");
    assert!(res.stderr.is_empty());
}

#[tokio::test]
async fn test_trigger_runner_exit_code() {
    let runner = TriggerRunner::default();
    let res = runner.run("sh -c 'exit 42'").await.expect("run exit 42");
    assert_eq!(res.exit_code, 42);
}

#[tokio::test]
async fn test_pipeline_evaluate_no_trigger() {
    let job = CronJob::new(
        "no-trigger-job".into(),
        "0 0 * * *".into(),
        "UTC".into(),
        "do something".into(),
        None,
        None,
        "off".into(),
        true,
        None,
        None,
    );

    match PipelineExecutor::evaluate_trigger(&job).await {
        TriggerOutcome::NoTrigger => {}
        other => panic!("expected NoTrigger, got {other:?}"),
    }
}

#[tokio::test]
async fn test_pipeline_evaluate_fired() {
    let job = CronJob::new_with_trigger(
        "fired-job".into(),
        "0 0 * * *".into(),
        "UTC".into(),
        "do something".into(),
        None,
        None,
        "off".into(),
        true,
        None,
        None,
        Some("echo 'work required'".into()),
        Some("non_empty".into()),
        false,
        None,
    );

    match PipelineExecutor::evaluate_trigger(&job).await {
        TriggerOutcome::Fired(res) => {
            assert_eq!(res.stdout.trim(), "work required");
        }
        other => panic!("expected Fired, got {other:?}"),
    }
}

#[tokio::test]
async fn test_pipeline_evaluate_skipped() {
    let job = CronJob::new_with_trigger(
        "skipped-job".into(),
        "0 0 * * *".into(),
        "UTC".into(),
        "do something".into(),
        None,
        None,
        "off".into(),
        true,
        None,
        None,
        Some("true".into()),
        Some("non_empty".into()),
        false,
        None,
    );

    match PipelineExecutor::evaluate_trigger(&job).await {
        TriggerOutcome::Skipped(res) => {
            assert!(res.stdout.is_empty());
        }
        other => panic!("expected Skipped, got {other:?}"),
    }
}

#[tokio::test]
async fn test_cron_set_goal_requires_session() {
    use crate::brain::tools::cron_manage::CronManageTool;
    use crate::brain::tools::r#trait::{Tool, ToolExecutionContext};
    use serde_json::json;

    let db = crate::db::Database::connect_in_memory().await.unwrap();
    db.run_migrations().await.unwrap();
    let pool = db.pool().clone();
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

// ── trigger process lifecycle ────────────────────────────────────────────────

/// stderr is carried back alongside stdout and the exit code.
///
/// Came in with the inline block that used to live in `cron/trigger.rs`; the
/// rest of that block duplicated assertions already above, this one did not.
#[tokio::test]
async fn test_trigger_runner_captures_stderr() {
    let runner = TriggerRunner::default();
    let res = runner
        .run("echo 'err' >&2; exit 2")
        .await
        .expect("run stderr probe");
    assert_eq!(res.exit_code, 2);
    assert_eq!(res.stderr.trim(), "err");
    assert!(res.stdout.trim().is_empty());
    assert!(TriggerCondition::ExitNonZero.should_fire(&res));
}

/// A trigger that outruns its timeout is an error, and its shell is killed.
///
/// The second half is the part worth testing: on timeout the `wait_with_output`
/// future is dropped, and a tokio `Child` does not kill on drop by default, so
/// without `kill_on_drop(true)` the shell keeps running unsupervised — once per
/// schedule tick, for the life of the daemon. Asserted by looking for the
/// process, with a unique marker in the command line, not by reading the
/// builder.
///
/// The `; :` matters: with a single command `sh` execs it directly and the
/// marker vanishes from the surviving process, which made the first version of
/// this test pass with `kill_on_drop` removed. The trailing no-op keeps `sh`
/// itself alive and carrying the marker.
///
/// Scope is the direct child. `kill_on_drop` kills the shell, not its
/// descendants, so the inner `sleep` outlives it either way — kept short for
/// that reason. Reaping the whole tree would need a process group.
#[cfg(unix)]
#[tokio::test]
async fn test_timed_out_trigger_kills_its_shell() {
    use std::time::Duration;

    let marker = format!("opencrabs_trigger_orphan_probe_{}", std::process::id());
    let runner = TriggerRunner::new(Duration::from_millis(200));

    let err = runner
        .run(&format!("sleep 5; : # {marker}"))
        .await
        .expect_err("a 5s sleep must outrun a 200ms timeout");
    assert!(err.contains("timed out"), "unexpected error: {err}");

    // The kill is delivered as the dropped child is reaped; give it a moment.
    tokio::time::sleep(Duration::from_millis(500)).await;

    let survivors = std::process::Command::new("pgrep")
        .arg("-f")
        .arg(&marker)
        .output()
        .expect("pgrep");
    let found = String::from_utf8_lossy(&survivors.stdout);
    assert!(
        found.trim().is_empty(),
        "timed-out trigger left its shell running (pids: {})",
        found.trim()
    );
}

/// #457 — the gate-outcome arms, pinned where the decision actually lives.
///
/// **Why this is a source-shape test and not a behavioral one.** The decision
/// under test is the `match` inside `CronScheduler::tick`, and that path has no
/// seam: `tick` is private (as are `is_due`/`next_run_after`), the only public
/// entry points (`run`/`run_adoptive`) are infinite poll loops, and an
/// `TriggerOutcome::Error` cannot be produced cheaply — `TriggerRunner` reads a
/// bad command as exit 127 (`Fired`), so the ONLY route to `Error` is genuinely
/// outrunning the hard-coded 30 s ceiling. A behavioral test would therefore
/// cost a production visibility widening plus a >30 s sleep plus a `Config::load()`
/// dependency, to assert an ABSENCE. Extracting a pure "outcome ⇒ action" helper
/// would be worse: production would never consult it, so it would pin a parallel
/// function that can drift from the real match and would NOT fail if the early
/// `return` were re-added. This test instead reads the arm that would have to
/// change, and it FAILS on the pre-fix tree — which is the only property that
/// makes a probe worth having.
///
/// The behavioral proof of the fix is the live smoke leg (a job whose gate
/// provably overruns executes anyway), not this file.
#[test]
fn test_gate_outcome_arms_only_skipped_is_terminal() {
    // Path is relative to this file: src/tests/ -> src/cron/scheduler.rs
    let src = include_str!("../cron/scheduler.rs");

    // Arms are ordered Skipped, Error, Fired, NoTrigger, so each arm's text runs
    // from its own marker to the next marker. Slicing on the next marker is
    // robust here, whereas brace-matching is not: the arms contain `{}` and
    // `{err}` inside string literals, which a naive brace counter miscounts.
    fn arm<'a>(src: &'a str, from: &str, to: &str) -> &'a str {
        let start = src
            .find(from)
            .unwrap_or_else(|| panic!("arm `{from}` not found — did the match move?"));
        let rest = &src[start..];
        let end = rest
            .find(to)
            .unwrap_or_else(|| panic!("arm `{to}` not found after `{from}` — did the match move?"));
        &rest[..end]
    }

    const SKIPPED: &str = "TriggerOutcome::Skipped(ref trig_res) =>";
    const ERROR: &str = "TriggerOutcome::Error(err) =>";
    const FIRED: &str = "TriggerOutcome::Fired(ref trig_res) =>";
    const NO_TRIGGER: &str = "TriggerOutcome::NoTrigger =>";

    let skipped = arm(src, SKIPPED, ERROR);
    let error = arm(src, ERROR, FIRED);
    let no_trigger = arm(src, NO_TRIGGER, "resolve_or_create_cron_session");

    // Skipped => Skip. The one outcome that legitimately blocks execution: the
    // gate ran and said NO.
    assert!(
        skipped.contains("record_skipped_run"),
        "the Skipped arm must still record the skipped run"
    );
    assert!(
        skipped.contains("return Ok(())"),
        "the Skipped arm must remain terminal — a gate that ran and said NO blocks the job"
    );

    // Error => Proceed. This is the #457 fix. A gate that cannot complete is
    // UNKNOWN, not FALSE, so the arm must warn and fall THROUGH.
    assert!(
        error.contains("tracing::warn!"),
        "#457: a failed/timed-out gate must warn and fail open"
    );
    assert!(
        !error.contains("tracing::error!"),
        "#457: the gate error must not be logged as an error — it is not terminal"
    );
    assert!(
        !error.contains("return"),
        "#457 REGRESSION: the Error arm returns early again — that is the terminality \
         that destroys a scheduled wake (a date-keyed job loses it for a YEAR). \
         The arm must fall through to resolve_or_create_cron_session + execute_job."
    );
    assert!(
        !error.contains("complete_error"),
        "#457: the Error arm must not write a terminal error run row"
    );
    assert!(
        !error.contains("new_running"),
        "#457: the Error arm must not write its own run row — a second row for a job \
         that did execute would corrupt the census surface this issue was found through"
    );

    // NoTrigger => Proceed. Already an empty arm that falls through; pinned so a
    // later edit cannot quietly make it terminal.
    assert!(
        !no_trigger.contains("return"),
        "the NoTrigger arm must keep falling through to execution"
    );
    assert!(
        !no_trigger.contains("complete_error"),
        "the NoTrigger arm must not write an error run row"
    );
}

