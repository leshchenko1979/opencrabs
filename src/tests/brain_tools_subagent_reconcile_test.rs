//! Startup reconciliation of orphaned sub-agent status files (#1038, #192).
//!
//! A sub-agent dies with the process; its status file does not. Every file
//! still `Pending`, `Running`, or `AwaitingInput` at startup belongs to an
//! agent that no longer exists, and must stop reading as live.

use crate::brain::agent::service::work_status::{self, WorkState, WorkStatus};
use crate::brain::tools::subagent::reconcile::reconcile_orphaned_agents;
use std::fs;

fn isolate(tag: &str) {
    let base = std::env::temp_dir().join(format!(
        "opencrabs-subagent-reconcile-{}-{}",
        tag,
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&base);
    let dir = base.join("detached");
    work_status::test_override::set(dir);
}

#[test]
fn a_running_agent_becomes_interrupted() {
    isolate("running");
    let mut s =
        WorkStatus::new_agent("agent-1", "build docs", "sess-a", "do things", None).unwrap();
    s.mark_running().unwrap();

    let orphans = reconcile_orphaned_agents();

    assert_eq!(orphans.len(), 1);
    assert_eq!(orphans[0].id, "agent-1");
    assert_eq!(orphans[0].state, WorkState::Interrupted);

    // Persisted, not just returned: the next reader must see it too.
    let reread = WorkStatus::read("agent-1").expect("status file still present");
    assert_eq!(reread.state, WorkState::Interrupted);
}

#[test]
fn a_pending_agent_becomes_interrupted() {
    // Killed between the status file being written and the task starting.
    isolate("pending");
    WorkStatus::new_agent("agent-2", "lint", "sess-b", "do things", None).unwrap();

    let orphans = reconcile_orphaned_agents();

    assert_eq!(orphans.len(), 1);
    assert_eq!(orphans[0].state, WorkState::Interrupted);
}

#[test]
fn a_parked_agent_becomes_interrupted() {
    // #1183: a file parked `AwaitingInput` is non-terminal, so a restart must
    // interrupt it like any other live agent — before the parked state
    // existed such a file read `Running` and was swept by the same rule, and
    // the new variant must not slip through it.
    isolate("parked");
    let mut s = WorkStatus::new_agent("agent-4", "audit", "sess-d", "do things", None).unwrap();
    s.mark_running().unwrap();
    s.mark_awaiting_input().unwrap();

    let orphans = reconcile_orphaned_agents();

    assert_eq!(orphans.len(), 1);
    assert_eq!(orphans[0].state, WorkState::Interrupted);
    let reread = WorkStatus::read("agent-4").expect("status file still present");
    assert_eq!(reread.state, WorkState::Interrupted);
}

#[test]
fn interrupted_carries_a_reason_and_a_completion_stamp() {
    // The reason distinguishes a restart from a genuine failure, and the
    // stamp lets the file age out on the same schedule as any other
    // terminal state.
    isolate("reason");
    let mut s = WorkStatus::new_agent("agent-3", "test", "sess-c", "do things", None).unwrap();
    s.mark_running().unwrap();

    let orphans = reconcile_orphaned_agents();

    let finish = orphans[0].finish.as_ref().expect("finish stamped");
    assert!(
        finish
            .error
            .as_deref()
            .unwrap_or("")
            .contains("restarted")
    );
    assert!(!finish.completed_at.is_empty());
}

#[test]
fn terminal_agents_are_left_alone() {
    isolate("terminal");
    let mut done = WorkStatus::new_agent("agent-done", "a", "sess-d", "p", None).unwrap();
    done.mark_completed("output".to_string()).unwrap();
    let mut failed = WorkStatus::new_agent("agent-failed", "b", "sess-d", "p", None).unwrap();
    failed.mark_failed("boom".to_string()).unwrap();

    let orphans = reconcile_orphaned_agents();

    assert!(orphans.is_empty(), "nothing mid-flight to reconcile");
    assert_eq!(
        WorkStatus::read("agent-done").unwrap().state,
        WorkState::Completed
    );
    assert_eq!(
        WorkStatus::read("agent-failed").unwrap().state,
        WorkState::Failed
    );
}

#[test]
fn the_parent_session_survives_so_the_report_can_be_routed() {
    // The whole point of returning the statuses: the caller needs to know
    // which session to tell.
    isolate("parent");
    let mut s = WorkStatus::new_agent(
        "agent-4",
        "deploy",
        "sess-child",
        "p",
        Some("sess-parent"),
    )
    .unwrap();
    s.mark_running().unwrap();

    let orphans = reconcile_orphaned_agents();

    assert_eq!(orphans[0].parent_session_id.as_deref(), Some("sess-parent"));
}

#[test]
fn reconciling_is_idempotent() {
    // A second startup must not re-report an agent already accounted for.
    isolate("idempotent");
    let mut s = WorkStatus::new_agent("agent-5", "x", "sess-e", "p", None).unwrap();
    s.mark_running().unwrap();

    assert_eq!(reconcile_orphaned_agents().len(), 1);
    assert!(reconcile_orphaned_agents().is_empty());
}

#[test]
fn a_missing_status_dir_is_not_an_error() {
    isolate("missing");
    assert!(reconcile_orphaned_agents().is_empty());
}

#[test]
fn unparseable_files_do_not_abort_the_pass() {
    isolate("corrupt");
    work_status::ensure_dir().unwrap();
    fs::write(
        work_status::status_dir().join("garbage.json"),
        "not json at all",
    )
    .unwrap();
    let mut s = WorkStatus::new_agent("agent-6", "y", "sess-f", "p", None).unwrap();
    s.mark_running().unwrap();

    let orphans = reconcile_orphaned_agents();

    assert_eq!(orphans.len(), 1, "the healthy file is still reconciled");
    assert_eq!(orphans[0].id, "agent-6");
}

#[test]
fn commands_are_not_reconciled_by_agent_pass() {
    isolate("commands");
    WorkStatus::new_command("cmd-1", "test cmd", "sess-c", "cargo test").unwrap();
    assert!(reconcile_orphaned_agents().is_empty());
}

#[test]
fn legacy_subagent_files_are_migrated_and_reconciled() {
    isolate("migration");
    let legacy_dir = work_status::legacy_dir();
    fs::create_dir_all(&legacy_dir).unwrap();
    let old_json = serde_json::json!({
        "id": "legacy-agent-1",
        "label": "legacy subagent",
        "parent_session_id": "parent-1",
        "prompt": "do old things",
        "started_at": "2026-09-01T00:00:00Z",
        "state": "Running"
    });
    fs::write(
        legacy_dir.join("legacy-agent-1.json"),
        old_json.to_string(),
    )
    .unwrap();

    let migrated = work_status::migrate_legacy_dir(&legacy_dir);
    assert_eq!(migrated, 1);

    let orphans = reconcile_orphaned_agents();
    assert_eq!(orphans.len(), 1);
    assert_eq!(orphans[0].id, "legacy-agent-1");
    assert_eq!(orphans[0].state, WorkState::Interrupted);
}
