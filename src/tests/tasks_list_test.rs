//! #1160 tasks_list + detached status files + prompt rot-guard.

use crate::brain::agent::service::detached_status::{self, DetachedFinish, test_override};
use crate::brain::tools::Tool;
use crate::brain::tools::tasks_list::{
    DetachedRow, SubagentRow, TasksListTool, render_tasks, subagent_status_file,
};
use tempfile::TempDir;
use uuid::Uuid;

#[test]
fn schema_has_no_params_and_tool_is_read_only() {
    let tool = TasksListTool::new();
    assert_eq!(tool.name(), "tasks_list");
    assert_eq!(
        tool.input_schema()["properties"].as_object().unwrap().len(),
        0
    );
    assert!(tool.hints().read_only);
}

#[test]
fn render_empty_roster_says_so_explicitly() {
    let out = render_tasks(&[], &[]);
    assert_eq!(out, "No background tasks.");
}

#[test]
fn render_lists_both_systems_with_states_and_pointers() {
    // Fixture path is deliberately NEUTRAL: it must not be the retired
    // pre-#26 layout (see `work_status::legacy_dir()`), which this test used
    // to enshrine as correct (#165). The real advertised path is pinned by
    // `advertised_status_file_is_the_file_the_writer_creates` below.
    let subs = vec![SubagentRow {
        id: "agt-1".into(),
        label: "research".into(),
        state: "running".into(),
        status_file: Some("/tmp/subagents/agt-1.json".into()),
        output_out: Some("/tmp/runs/child-run-1.out".into()),
    }];
    let det = vec![DetachedRow {
        id: "run-1".into(),
        label: "cargo test".into(),
        state: "running".into(),
        elapsed_secs: 42,
        pid: Some(4242),
        output_out: Some("/tmp/runs/run-1.out".into()),
        output_err: Some("/tmp/runs/run-1.err".into()),
        status_file: Some("/tmp/detached/run-1.json".into()),
    }];
    let out = render_tasks(&subs, &det);
    assert!(out.contains("Sub-agents (1)"), "was: {out}");
    assert!(out.contains("- agt-1 [research] running"), "was: {out}");
    assert!(out.contains("status file: /tmp/subagents/agt-1.json"));
    // A sub-agent row must carry a path to what the CHILD is producing, not
    // only to its own status record: a child's detached runs are scoped to the
    // child's session, so they never appear in the caller's detached list
    // below — without this line the model sees a child at work and has no way
    // to look at the work (#692 D6).
    assert!(out.contains("output: /tmp/runs/child-run-1.out"), "was: {out}");
    assert!(out.contains("Detached commands (1)"), "was: {out}");
    // The row names the run's STATE, not just that it exists (#752). Before
    // this, a detached row rendered `- <id> [<label>] <elapsed>s` and nothing
    // else, so a run whose own status record said `interrupted` or had gone
    // missing read exactly like a healthy one. The state is read from the run's
    // status file — the run's own declaration — never assumed from the handle
    // being in memory.
    assert!(
        out.contains("- run-1 [cargo test] running — 42s"),
        "was: {out}"
    );
    // A row must carry the run's ADDRESS, not just its label: the id is the
    // handle `task_output`/`task_wait` take, the pgid is what stops it, and the
    // paths are where the live streams are. A label-only row left the model
    // able to see that something ran and unable to look at or stop it (#692).
    //
    // The pgid is asserted explicitly because it became the ONLY stop handle
    // when the cancel tool was removed (owner directive 2026-09-29): if a row
    // stops printing it, the skill-side `kill -- -<pgid>` has nothing to read
    // and the run becomes unstoppable again — the exact defect this row exists
    // to close.
    assert!(out.contains("pgid: 4242"), "was: {out}");
    assert!(out.contains("stdout: /tmp/runs/run-1.out"), "was: {out}");
    assert!(out.contains("stderr: /tmp/runs/run-1.err"), "was: {out}");
    assert!(out.contains("status file: /tmp/detached/run-1.json"), "was: {out}");
}

/// The path `tasks_list` hands the model must be a file a writer really
/// created.
///
/// Deliberately crosses the writer boundary instead of comparing
/// `subagent_status_file()` to the resolver it delegates to: that comparison
/// is a tautology, green on any build, including one whose resolver points at
/// a directory nothing creates. Here a real `WorkStatus::new_agent` persists
/// the record and the advertised STRING is what opens it, so the test fails
/// on any drift between the two sides, retired `legacy_dir()` included.
///
/// An empty read from a wrong path is indistinguishable from "this sub-agent
/// never existed", which is why this is pinned rather than assumed.
#[test]
fn advertised_status_file_is_the_file_the_writer_creates() {
    use crate::brain::agent::service::work_status;

    let dir = TempDir::new().unwrap();
    work_status::test_override::set(dir.path().join("detached"));

    let id = Uuid::new_v4();
    let session_id = Uuid::new_v4();
    work_status::WorkStatus::new_agent(
        &id.to_string(),
        "research",
        &session_id.to_string(),
        "probe the advertised path",
        None,
    )
    .unwrap();

    let advertised = subagent_status_file(&id.to_string());
    let raw = std::fs::read_to_string(&advertised)
        .unwrap_or_else(|e| panic!("advertised path {advertised} is not readable: {e}"));
    assert!(raw.contains("\"kind\": \"agent\""), "was: {raw}");
    assert!(raw.contains(&session_id.to_string()), "was: {raw}");

    // The retired pre-#26 sibling is a directory nothing creates; advertising
    // through it is the failure mode this pins against.
    let retired = work_status::legacy_dir().display().to_string();
    assert!(
        !advertised.starts_with(&retired),
        "advertised path {advertised} must not live under the retired {retired}"
    );

    work_status::test_override::clear();
}

/// Gap 2: a detached command's status file exists mid-run with spawn data,
/// and gains exit info on completion.
#[test]
fn detached_status_file_written_then_finished() {
    let dir = TempDir::new().unwrap();
    test_override::set(dir.path().to_path_buf());

    let task_id = Uuid::new_v4();
    let session_id = Uuid::new_v4();
    detached_status::write_started(task_id, session_id, "cargo test", "cargo test --lib");
    let raw = std::fs::read_to_string(dir.path().join(format!("{task_id}.json"))).unwrap();
    assert!(raw.contains("\"cargo test\""), "was: {raw}");
    assert!(raw.contains(&session_id.to_string()), "was: {raw}");
    assert!(!raw.contains("\"finished\""), "mid-run must be unfinished");

    detached_status::write_finished(
        task_id,
        session_id,
        "cargo test",
        "cargo test --lib",
        DetachedFinish {
            success: true,
            code: 0,
            elapsed_secs: 99.5,
            output_bytes: 2048,
        },
    );
    let raw = std::fs::read_to_string(dir.path().join(format!("{task_id}.json"))).unwrap();
    assert!(raw.contains("\"success\": true"), "was: {raw}");
    assert!(raw.contains("\"output_bytes\": 2048"), "was: {raw}");
}

/// Gap 3 rot-guard: the LONG TASKS paragraph must keep covering sub-agents,
/// not only bash — it regressed to bash-only once already (#762).
#[test]
fn prompt_builder_keeps_subagent_background_contract() {
    let dir = TempDir::new().unwrap();
    let prompt = crate::brain::prompt_builder::BrainLoader::new(dir.path().to_path_buf())
        .build_system_brain(None);
    assert!(
        prompt.contains("spawned agents run in the background"),
        "subagent background contract missing from system prompt"
    );
}

/// #191 regression pin: `execute` reports only the CALLER's sub-agents.
///
/// The manager is process-global (one instance per channel factory), so
/// pre-fix this iterated every session's children. That is not merely noise:
/// the tool's framing tells the model these are *its* in-flight sub-agents
/// ("do not spawn duplicates"), so a foreign row makes a lane silently skip
/// work it believes is already running.
///
/// Pins the call site rather than the renderer — `render_tasks` takes rows the
/// caller built, so it cannot see a scope regression.
#[tokio::test]
async fn execute_lists_only_the_callers_subagents() {
    use crate::brain::tools::ToolExecutionContext;
    use crate::brain::tools::subagent::{SubAgent, SubAgentManager};
    use std::sync::Arc;

    fn child(id: &str, label: &str, parent: Uuid) -> SubAgent {
        SubAgent::new(id.to_string(), label.to_string(), Uuid::new_v4(), parent)
    }

    let me = Uuid::from_u128(0x191);
    let other = Uuid::from_u128(0x192);

    let mgr = Arc::new(SubAgentManager::new());
    mgr.insert(child("mine0001", "my-research", me));
    mgr.insert(child("theirs01", "their-research", other));

    let mut ctx = ToolExecutionContext::new(me);
    ctx.subagent_manager = Some(mgr);

    let out = TasksListTool::new()
        .execute(serde_json::json!({}), &ctx)
        .await
        .unwrap();
    assert!(out.success, "tasks_list failed: {:?}", out.error);
    assert!(
        out.output.contains("mine0001"),
        "own child missing: {}",
        out.output
    );
    assert!(
        !out.output.contains("theirs01"),
        "another session's child leaked into the roster: {}",
        out.output
    );
}

/// #692 D6: a sub-agent's row carries a path to the CHILD's own runs.
///
/// Pins the CALL SITE, not the renderer: `render_tasks` renders rows the caller
/// built, so it cannot see whether `execute` looked the child's runs up under
/// the child's session or the caller's. The regression this catches is silent —
/// `handles_for(caller)` returns an empty list rather than an error, so the row
/// would simply lose its output line and every fixture-rendering test would
/// stay green while the feature did nothing.
#[tokio::test]
async fn execute_points_a_subagent_row_at_the_childs_own_run() {
    use crate::brain::agent::service::background_tasks::{BackgroundTaskManager, RunRequest};
    use crate::brain::tools::ToolExecutionContext;
    use crate::brain::tools::subagent::{SubAgent, SubAgentManager};
    use std::sync::Arc;

    let me = Uuid::from_u128(0x692);
    let child_session = Uuid::new_v4();

    // The spawn path creates its capture files IMMEDIATELY (`ensure_runs_dir`
    // then `File::create`), and `runs_dir()` falls back to `<home>/tmp/runs`
    // when no override is set — so without this the test writes into the home
    // the daemon is using. Pinned to a TempDir for the same reason the
    // neighbouring tests are.
    let dir = TempDir::new().unwrap();
    crate::brain::agent::service::work_status::test_override::set(dir.path().to_path_buf());

    let mgr = Arc::new(SubAgentManager::new());
    mgr.insert(SubAgent::new(
        "child001",
        "child-research",
        child_session,
        me,
    ));

    // A run the CHILD spawned: its session is the child's, not the caller's.
    // That is the whole point — a run is scoped to the session that spawned it,
    // so this one is invisible to the caller's own detached list.
    let bm = Arc::new(BackgroundTaskManager::new());
    bm.clone().spawn_command(RunRequest::new(
        child_session,
        std::env::temp_dir(),
        "child run".to_string(),
        "sleep 5".to_string(),
    ));

    let mut ctx = ToolExecutionContext::new(me);
    ctx.subagent_manager = Some(mgr);
    ctx.background_manager = Some(bm.clone());

    let out = TasksListTool::new()
        .execute(serde_json::json!({}), &ctx)
        .await
        .unwrap();
    // The child's run is addressable through the child's row...
    let expected = bm.handles_for(child_session)[0]
        .output_out
        .display()
        .to_string();

    // Nothing below touches the filesystem — `expected` comes from the same
    // in-memory handle `execute` read — so the override is cleared BEFORE the
    // assertions: a failing assert must not leave a thread-local pointing at a
    // TempDir that is about to be deleted, for the next test on this thread.
    crate::brain::agent::service::work_status::test_override::clear();

    assert!(out.success, "tasks_list failed: {:?}", out.error);
    assert!(
        out.output.contains(&format!("output: {expected}")),
        "child's run path missing from its row: {}\nwant: {expected}",
        out.output
    );
    // ...and the caller's detached section stays empty, because the run belongs
    // to the child. Had the lookup keyed on the caller, the path would appear
    // under the wrong heading and this assertion would fail instead.
    assert!(
        !out.output.contains("Detached commands"),
        "a child's run leaked into the caller's detached list: {}",
        out.output
    );
}
