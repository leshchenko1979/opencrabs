//! Boot hygiene of orphaned detached-work status files (#1038, unified #26).
//!
//! Since #26 P2 this pass is MARKING-ONLY: the interruption reports come from
//! the recovery-table scan (`restart_recovery::report_interrupted`), which
//! covers both kinds. These tests therefore assert file states, not returned
//! reports — a report asserted here AND from the table would be the
//! double-message the unification exists to kill.

use crate::brain::agent::service::work_status::*;
use crate::brain::tools::subagent::reconcile::mark_orphans_and_sweep;
use std::fs;

/// Point the test override at a NESTED dir so [`legacy_dir`] (the
/// `subagents` sibling the reconcile pass migrates from) resolves inside
/// the per-test sandbox instead of the real /tmp.
fn isolate(tag: &str) {
    let dir = std::env::temp_dir().join(format!(
        "opencrabs-subagent-reconcile-{}-{}",
        tag,
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    test_override::set(dir.join("detached"));
}

#[test]
fn a_running_agent_becomes_interrupted() {
    isolate("running");
    let mut s = WorkStatus::new_agent("agent-1", "build docs", "sess-a", "do things").unwrap();
    s.mark_running().unwrap();

    mark_orphans_and_sweep();

    // Persisted, not just logged: the next reader must see it too.
    let reread = WorkStatus::read("agent-1").expect("status file still present");
    assert_eq!(reread.state, WorkState::Interrupted);
}

#[test]
fn a_pending_agent_becomes_interrupted() {
    // Killed between the status file being written and the task starting.
    isolate("pending");
    WorkStatus::new_agent("agent-2", "lint", "sess-b", "do things").unwrap();

    mark_orphans_and_sweep();

    assert_eq!(
        WorkStatus::read("agent-2").unwrap().state,
        WorkState::Interrupted
    );
}

#[test]
fn a_parked_agent_becomes_interrupted() {
    // #1183: a file parked `AwaitingInput` is non-terminal, so a restart must
    // interrupt it like any other live agent — before the parked state
    // existed such a file read `Running` and was swept by the same rule, and
    // the new variant must not slip through it.
    isolate("parked");
    let mut s = WorkStatus::new_agent("agent-4", "audit", "sess-d", "do things").unwrap();
    s.mark_running().unwrap();
    s.mark_awaiting_input().unwrap();

    mark_orphans_and_sweep();

    let reread = WorkStatus::read("agent-4").expect("status file still present");
    assert_eq!(reread.state, WorkState::Interrupted);
}

#[test]
fn interrupted_carries_a_reason_and_a_completion_stamp() {
    // The reason distinguishes a restart from a genuine failure, and the
    // stamp lets the file age out on the same schedule as any other
    // terminal state.
    isolate("reason");
    let mut s = WorkStatus::new_agent("agent-3", "test", "sess-c", "do things").unwrap();
    s.mark_running().unwrap();

    mark_orphans_and_sweep();

    let reread = WorkStatus::read("agent-3").expect("status file still present");
    let finish = reread.finish.as_ref().expect("interrupted stamps a finish");
    assert!(
        finish.error.as_deref().unwrap_or("").contains("restart"),
        "was: {:?}",
        finish.error
    );
    assert!(!finish.completed_at.is_empty());
}

#[test]
fn terminal_agents_are_left_alone() {
    isolate("terminal");
    let mut done = WorkStatus::new_agent("agent-done", "a", "sess-d", "p").unwrap();
    done.mark_completed("output".to_string()).unwrap();
    let mut failed = WorkStatus::new_agent("agent-failed", "b", "sess-d", "p").unwrap();
    failed.mark_failed("boom".to_string()).unwrap();

    mark_orphans_and_sweep();

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
fn the_status_file_keeps_its_session_stamp() {
    // The report is routed from the DB row now, but the file's session stamp
    // must survive the marking untouched — settle cards read it.
    isolate("parent");
    let mut s = WorkStatus::new_agent("agent-4", "deploy", "sess-parent", "p").unwrap();
    s.mark_running().unwrap();

    mark_orphans_and_sweep();

    assert_eq!(
        WorkStatus::read("agent-4").unwrap().session_id,
        "sess-parent"
    );
}

#[test]
fn marking_is_idempotent() {
    // A second startup must not re-stamp an agent already accounted for.
    isolate("idempotent");
    let mut s = WorkStatus::new_agent("agent-5", "x", "sess-e", "p").unwrap();
    s.mark_running().unwrap();

    mark_orphans_and_sweep();
    let first = WorkStatus::read("agent-5").unwrap();
    assert_eq!(first.state, WorkState::Interrupted);

    mark_orphans_and_sweep();
    let second = WorkStatus::read("agent-5").unwrap();
    assert_eq!(second.state, WorkState::Interrupted);
    assert_eq!(
        first.finish.as_ref().map(|f| f.completed_at.clone()),
        second.finish.as_ref().map(|f| f.completed_at.clone()),
        "the second pass must not restamp the finish"
    );
}

#[test]
fn a_missing_status_dir_is_not_an_error() {
    isolate("missing");
    mark_orphans_and_sweep();
}

#[test]
fn unparseable_files_do_not_abort_the_pass() {
    isolate("corrupt");
    ensure_dir().unwrap();
    fs::write(status_dir().join("garbage.json"), "not json at all").unwrap();
    let mut s = WorkStatus::new_agent("agent-6", "y", "sess-f", "p").unwrap();
    s.mark_running().unwrap();

    mark_orphans_and_sweep();

    assert_eq!(
        WorkStatus::read("agent-6").unwrap().state,
        WorkState::Interrupted,
        "the healthy file is still marked"
    );
}

// ── #26 P1/P2: two kinds share the dir, reports live in the table ──

#[test]
fn a_running_command_is_interrupted_on_disk_but_not_reported() {
    // Commands share the dir since #26, so the pass must stop them reading
    // as live too. Since P2 NEITHER kind is reported from files: the row
    // scan owns both reports, and reporting here as well would
    // double-message.
    isolate("command");
    WorkStatus::new_command("cmd-1", "sess-g", "nightly build", "cargo build").unwrap();

    mark_orphans_and_sweep();

    let reread = WorkStatus::read("cmd-1").expect("command file still present");
    assert_eq!(reread.state, WorkState::Interrupted, "marked on disk");
    assert!(
        reread
            .finish
            .as_ref()
            .and_then(|f| f.error.as_deref())
            .unwrap_or("")
            .contains("this command"),
        "kind-aware interruption text"
    );
}

#[test]
fn mixed_kinds_are_both_marked() {
    isolate("mixed");
    let mut agent = WorkStatus::new_agent("agent-m", "docs", "sess-m", "p").unwrap();
    agent.mark_running().unwrap();
    WorkStatus::new_command("cmd-m", "sess-m", "build", "make").unwrap();

    mark_orphans_and_sweep();

    assert_eq!(
        WorkStatus::read("agent-m").unwrap().state,
        WorkState::Interrupted
    );
    assert_eq!(
        WorkStatus::read("cmd-m").unwrap().state,
        WorkState::Interrupted
    );
}

#[test]
fn a_legacy_running_agent_is_migrated_then_marked() {
    // End-to-end upgrade path (#1038 survives the dir move): a pre-#26
    // agent file in the old dir is migrated into the unified dir first,
    // then interrupted like any other orphan.
    isolate("legacy_e2e");
    fs::create_dir_all(legacy_dir()).unwrap();
    let legacy = serde_json::json!({
        "id": "agent-old",
        "label": "old worker",
        "parent_session_id": "sess-old",
        "state": "Running",
        "prompt": "do old things",
        "started_at": "2026-08-28T10:00:00+00:00"
    });
    fs::write(
        legacy_dir().join("agent-old.json"),
        serde_json::to_string_pretty(&legacy).unwrap(),
    )
    .unwrap();

    mark_orphans_and_sweep();

    let reread = WorkStatus::read("agent-old").expect("migrated into the unified dir");
    assert_eq!(reread.state, WorkState::Interrupted);
}
