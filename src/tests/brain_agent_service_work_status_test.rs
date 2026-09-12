//! Unit tests for the unified detached-work status module (#26 P1).

use crate::brain::agent::service::work_status::*;
use std::fs;
use std::time::Duration;

/// Point the test override at a NESTED dir so [`legacy_dir`] (the
/// `subagents` sibling) resolves inside the per-test sandbox instead of the
/// real /tmp.
fn isolate(tag: &str) {
    let dir = std::env::temp_dir().join(format!(
        "opencrabs-work-status-test-{}-{}",
        tag,
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    test_override::set(dir.join("detached"));
}

#[test]
fn status_dir_returns_correct_path() {
    test_override::clear();
    let home = crate::config::opencrabs_home();
    let expected = home.join("tmp").join("detached");
    assert_eq!(status_dir(), expected);
    // The pre-#26 sub-agent dir is the sibling migration reads from.
    assert_eq!(legacy_dir(), home.join("tmp").join("subagents"));
}

#[test]
fn status_path_ends_with_json() {
    isolate("path");
    let p = status_path("abc123");
    assert_eq!(p.file_name().unwrap().to_str().unwrap(), "abc123.json");
}

#[test]
fn new_agent_is_pending() {
    isolate("new_pending");
    let s = WorkStatus::new_agent("test-1", "test", "sess-1", "do things", None).unwrap();
    assert_eq!(s.state, WorkState::Pending);
    assert_eq!(s.kind, WorkKind::Agent);
    assert_eq!(s.id, "test-1");
    assert_eq!(s.label, "test");
    assert_eq!(s.task, "do things");
}

#[test]
fn agent_transitions_to_running() {
    isolate("running");
    let mut s = WorkStatus::new_agent("test-2", "test", "sess-1", "do things", None).unwrap();
    s.mark_running().unwrap();
    assert_eq!(s.state, WorkState::Running);
}

#[test]
fn agent_parks_as_awaiting_input_and_flips_back() {
    // #1183: a parked agent used to keep reading `state: "Running"` with no
    // finish, misleading every consumer into waiting on work that was
    // already finished. The parked state is distinct, not terminal, and
    // round-trips through the file so external readers see it too.
    isolate("awaiting_input");
    let mut s = WorkStatus::new_agent("test-7", "test", "sess-1", "do things", None).unwrap();
    s.mark_running().unwrap();
    s.mark_awaiting_input().unwrap();
    assert_eq!(s.state, WorkState::AwaitingInput);
    assert!(!s.state.is_terminal(), "parked is not a terminal state");
    assert!(s.finish.is_none(), "parked stamps no finish");

    let reread = WorkStatus::read("test-7").expect("should read back");
    assert_eq!(reread.state, WorkState::AwaitingInput);

    // Follow-up input flips the file back to working.
    s.mark_running().unwrap();
    assert_eq!(s.state, WorkState::Running);
    let reread = WorkStatus::read("test-7").expect("should read back");
    assert_eq!(reread.state, WorkState::Running);
}

#[test]
fn agent_progress_snapshot() {
    isolate("progress");
    let mut s = WorkStatus::new_agent("test-3", "test", "sess-1", "do things", None).unwrap();
    s.mark_running().unwrap();
    s.update_progress(1, Some("bash".into()), Some("cargo check ok".into()))
        .unwrap();
    assert!(s.progress.is_some());
    let p = s.progress.unwrap();
    assert_eq!(p.iteration, 1);
    assert_eq!(p.last_tool, Some("bash".to_string()));
    assert_eq!(p.last_event, Some("cargo check ok".to_string()));
}

#[test]
fn agent_completed_sets_finish() {
    isolate("completed");
    let mut s = WorkStatus::new_agent("test-4", "test", "sess-1", "do things", None).unwrap();
    s.mark_completed("done".into()).unwrap();
    assert_eq!(s.state, WorkState::Completed);
    let finish = s.finish.as_ref().expect("completed stamps a finish");
    assert!(!finish.completed_at.is_empty());
    assert_eq!(finish.output_full.as_deref(), Some("done"));
}

#[test]
fn agent_failed_sets_error() {
    isolate("failed");
    let mut s = WorkStatus::new_agent("test-5", "test", "sess-1", "do things", None).unwrap();
    s.mark_failed("something broke".into()).unwrap();
    assert_eq!(s.state, WorkState::Failed);
    let finish = s.finish.as_ref().expect("failed stamps a finish");
    assert_eq!(finish.error.as_deref(), Some("something broke"));
    assert!(!finish.completed_at.is_empty());
}

#[test]
fn agent_read_roundtrip() {
    isolate("roundtrip");
    let mut s = WorkStatus::new_agent("test-6", "test", "sess-1", "do things", None).unwrap();
    s.mark_running().unwrap();
    s.update_progress(2, Some("write_file".into()), None)
        .unwrap();

    let read = WorkStatus::read("test-6").expect("should read back");
    assert_eq!(read.id, "test-6");
    assert_eq!(read.state, WorkState::Running);
    assert_eq!(read.progress.unwrap().iteration, 2);
}

#[test]
fn command_spawns_running_and_carries_its_kind() {
    // #1160 mid-run visibility: the file exists from spawn, in the shared
    // dir, reading as live work of the Command kind.
    isolate("command_spawn");
    let s = WorkStatus::new_command("task-1", "sess-1", "cargo test", "cargo test --lib").unwrap();
    assert_eq!(s.state, WorkState::Running);
    assert_eq!(s.kind, WorkKind::Command);
    assert!(s.finish.is_none(), "mid-run must be unfinished");

    let raw = fs::read_to_string(status_path("task-1")).unwrap();
    assert!(raw.contains("\"kind\": \"command\""), "was: {raw}");
    assert!(raw.contains("\"cargo test\""), "was: {raw}");
}

#[test]
fn command_finish_success_stamps_exit_info() {
    isolate("command_finish_ok");
    WorkStatus::new_command("task-2", "sess-1", "cargo test", "cargo test --lib").unwrap();
    WorkStatus::finish_command(
        "task-2",
        "sess-1",
        "cargo test",
        "cargo test --lib",
        CommandExit {
            success: true,
            code: 0,
            elapsed_secs: 99.5,
            output_bytes: 2048,
        },
    )
    .unwrap();

    let s = WorkStatus::read("task-2").expect("should read back");
    assert_eq!(s.state, WorkState::Completed);
    let finish = s.finish.as_ref().expect("finished stamps a finish");
    assert_eq!(finish.success, Some(true));
    assert_eq!(finish.code, Some(0));
    assert_eq!(finish.elapsed_secs, Some(99.5));
    assert_eq!(finish.output_bytes, Some(2048));
}

#[test]
fn command_finish_failure_reads_failed() {
    isolate("command_finish_err");
    WorkStatus::new_command("task-3", "sess-1", "build", "make").unwrap();
    WorkStatus::finish_command(
        "task-3",
        "sess-1",
        "build",
        "make",
        CommandExit {
            success: false,
            code: 101,
            elapsed_secs: 3.0,
            output_bytes: 12,
        },
    )
    .unwrap();

    let s = WorkStatus::read("task-3").expect("should read back");
    assert_eq!(s.state, WorkState::Failed);
    assert_eq!(s.finish.as_ref().and_then(|f| f.success), Some(false));
}

#[test]
fn command_finish_without_spawn_write_falls_back_to_fresh_record() {
    // The pre-#26 contract: a finish whose spawn write never landed still
    // produces a readable terminal record.
    isolate("command_finish_orphan");
    WorkStatus::finish_command(
        "task-4",
        "sess-1",
        "ghost",
        "true",
        CommandExit {
            success: true,
            code: 0,
            elapsed_secs: 0.1,
            output_bytes: 0,
        },
    )
    .unwrap();
    let s = WorkStatus::read("task-4").expect("fallback record written");
    assert_eq!(s.kind, WorkKind::Command);
    assert_eq!(s.state, WorkState::Completed);
    assert_eq!(s.label, "ghost");
}

#[test]
fn interrupted_error_text_is_kind_aware() {
    // Agents keep the exact pre-#26 sentence; commands get the same
    // sentence about a command.
    isolate("interrupted_text");
    let mut agent = WorkStatus::new_agent("agt-i", "a", "sess-1", "p", None).unwrap();
    agent.mark_interrupted().unwrap();
    let mut command = WorkStatus::new_command("cmd-i", "sess-1", "c", "sleep 9").unwrap();
    command.mark_interrupted().unwrap();

    assert_eq!(
        agent.finish.as_ref().and_then(|f| f.error.as_deref()),
        Some("OpenCrabs restarted while this agent was running, so it was killed before finishing")
    );
    assert_eq!(
        command.finish.as_ref().and_then(|f| f.error.as_deref()),
        Some(
            "OpenCrabs restarted while this command was running, so it was killed before finishing"
        )
    );
    assert_eq!(agent.state, WorkState::Interrupted);
    assert_eq!(command.state, WorkState::Interrupted);
}

#[test]
fn cleanup_removes_old_files() {
    isolate("cleanup");
    let mut s = WorkStatus::new_agent("old-1", "old", "sess", "task", None).unwrap();
    s.mark_completed("done".into()).unwrap();

    let old_ts = chrono::Utc::now()
        .checked_sub_signed(chrono::Duration::days(8))
        .unwrap()
        .to_rfc3339();
    let mut raw = fs::read_to_string(status_path("old-1")).unwrap();
    let mut parsed: serde_json::Value = serde_json::from_str(&raw).unwrap();
    parsed["finish"]["completed_at"] = serde_json::json!(old_ts);
    raw = serde_json::to_string_pretty(&parsed).unwrap();
    fs::write(status_path("old-1"), raw).unwrap();

    let cleanup_result = cleanup_stale(Duration::from_secs(7 * 86400)).unwrap();
    assert!(cleanup_result.1 >= 1, "should have removed at least 1 file");
    assert!(WorkStatus::read("old-1").is_none());
}

/// Pre-#26 sub-agent files (old schema, `tmp/subagents/`) migrate into the
/// unified dir with the field mapping applied, so an agent in flight across
/// the binary swap is still reconciled (#1038 survives the upgrade).
#[test]
fn legacy_agent_files_migrate_into_the_unified_dir() {
    isolate("legacy_migrate");
    fs::create_dir_all(legacy_dir()).unwrap();

    // In flight under the old schema: no completed_at.
    let running = serde_json::json!({
        "id": "legacy-run",
        "label": "old runner",
        "parent_session_id": "sess-legacy",
        "state": "Running",
        "prompt": "do old things",
        "started_at": "2026-08-28T10:00:00+00:00"
    });
    fs::write(
        legacy_dir().join("legacy-run.json"),
        serde_json::to_string_pretty(&running).unwrap(),
    )
    .unwrap();

    // Terminal under the old schema.
    let done = serde_json::json!({
        "id": "legacy-done",
        "label": "old done",
        "parent_session_id": "sess-legacy",
        "state": "Completed",
        "prompt": "finish old things",
        "started_at": "2026-08-28T09:00:00+00:00",
        "completed_at": "2026-08-28T09:30:00+00:00",
        "output_summary": "all done"
    });
    fs::write(
        legacy_dir().join("legacy-done.json"),
        serde_json::to_string_pretty(&done).unwrap(),
    )
    .unwrap();

    let migrated = migrate_legacy_dir(&legacy_dir());
    assert_eq!(migrated, 2);
    assert!(!legacy_dir().exists(), "empty legacy dir is dropped");

    let run = WorkStatus::read("legacy-run").expect("migrated into the unified dir");
    assert_eq!(run.kind, WorkKind::Agent);
    assert_eq!(run.session_id, "sess-legacy");
    assert_eq!(run.task, "do old things");
    assert_eq!(run.spawned_at, "2026-08-28T10:00:00+00:00");
    assert_eq!(run.state, WorkState::Running);
    assert!(run.finish.is_none());

    let done = WorkStatus::read("legacy-done").expect("migrated into the unified dir");
    assert_eq!(done.state, WorkState::Completed);
    let finish = done
        .finish
        .as_ref()
        .expect("terminal legacy keeps its finish");
    assert_eq!(finish.completed_at, "2026-08-28T09:30:00+00:00");
    assert!(finish.output_full.is_none());
}

/// A missing legacy dir is the common case, not an error.
#[test]
fn legacy_migration_with_no_legacy_dir_is_a_noop() {
    isolate("legacy_none");
    assert_eq!(migrate_legacy_dir(&legacy_dir()), 0);
}

/// #187 D2 — a round-end write must not reset the tool count.
///
/// The sub-agent keeps ONE status handle for the child's whole life, taken at
/// spawn. The progress callback writes `tool_count` through its own fresh read;
/// the round-end `update_progress` then re-serialises the spawn-time copy, and
/// because it derives `prev_tool_count` from that copy it wrote 0 over the
/// callback's count — every round. That alone explains the at-rest census (0 of
/// 273 status files with a truthy `tool_count`), which is why the census could
/// never settle whether the callback fires at all.
///
/// Both halves are pinned: the hazard (writing from the stale copy clobbers)
/// and the fix (reload first, and the count survives).
#[test]
fn round_end_write_keeps_a_mid_round_tool_count() {
    isolate("round_end_count");
    let id = "d2-round-end";
    let mut spawn_handle =
        WorkStatus::new_agent(id, "review", "sess-1", "review the plan", None).unwrap();
    assert!(
        spawn_handle.progress.is_none(),
        "a freshly spawned agent carries no progress snapshot — this is the copy spawn.rs holds"
    );

    // The progress callback: fresh read, explicit count.
    let mut from_callback = WorkStatus::read(id).expect("status file");
    from_callback
        .update_progress_tool_count(1, 5, Some("bash".to_string()), None)
        .unwrap();
    assert_eq!(
        WorkStatus::read(id).unwrap().progress.unwrap().tool_count,
        5
    );

    // THE HAZARD, pinned deliberately: writing from the spawn-time handle
    // clobbers the callback's count back to 0. This is the pre-fix behaviour.
    let mut stale = spawn_handle.clone();
    stale
        .update_progress(1, None, Some("tool call(s) completed".to_string()))
        .unwrap();
    assert_eq!(
        WorkStatus::read(id).unwrap().progress.unwrap().tool_count,
        0,
        "writing from the spawn-time copy IS the clobber this fix removes"
    );

    // The callback writes again (a later tool in the same round).
    let mut from_callback = WorkStatus::read(id).expect("status file");
    from_callback
        .update_progress_tool_count(1, 6, Some("read_file".to_string()), None)
        .unwrap();

    // THE FIX: reload first, then the round-end write.
    assert!(spawn_handle.reload(), "the status file is there to re-read");
    spawn_handle
        .update_progress(1, None, Some("tool call(s) completed".to_string()))
        .unwrap();

    let after = WorkStatus::read(id).unwrap().progress.unwrap();
    assert_eq!(
        after.tool_count, 6,
        "the round-end write must carry the callback's count, not the spawn-time 0"
    );
    assert_eq!(after.iteration, 1);
    assert_eq!(
        after.last_tool, None,
        "the round-end write still clears last_tool — only the count is preserved"
    );
    assert_eq!(after.last_event.as_deref(), Some("tool call(s) completed"));
}

/// `reload` reports a vanished file rather than blanking the handle.
///
/// A caller is mid-write on a status the sweep may have aged out; turning a
/// missing file into an empty handle would write a WRONG status instead of
/// leaving the caller's copy alone.
#[test]
fn reload_reports_a_missing_file_and_leaves_the_handle_alone() {
    isolate("reload_gone");
    let mut orphan = WorkStatus::new_agent("d2-gone", "review", "sess-1", "x", None).unwrap();
    fs::remove_file(status_path("d2-gone")).unwrap();

    assert!(!orphan.reload(), "a missing file reports false");
    assert_eq!(orphan.id, "d2-gone", "and the handle is left untouched");
    assert_eq!(orphan.task, "x");
}
