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
    let s = WorkStatus::new_agent("test-1", "test", "sess-1", "do things", None, None).unwrap();
    assert_eq!(s.state, WorkState::Pending);
    assert_eq!(s.kind, WorkKind::Agent);
    assert_eq!(s.id, "test-1");
    assert_eq!(s.label, "test");
    assert_eq!(s.task, "do things");
}

#[test]
fn agent_transitions_to_running() {
    isolate("running");
    let mut s = WorkStatus::new_agent("test-2", "test", "sess-1", "do things", None, None).unwrap();
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
    let mut s = WorkStatus::new_agent("test-7", "test", "sess-1", "do things", None, None).unwrap();
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
    let mut s = WorkStatus::new_agent("test-3", "test", "sess-1", "do things", None, None).unwrap();
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
    let mut s = WorkStatus::new_agent("test-4", "test", "sess-1", "do things", None, None).unwrap();
    s.mark_completed("done".into()).unwrap();
    assert_eq!(s.state, WorkState::Completed);
    let finish = s.finish.as_ref().expect("completed stamps a finish");
    assert!(!finish.completed_at.is_empty());
    assert_eq!(finish.output_full.as_deref(), Some("done"));
}

#[test]
fn agent_failed_sets_error() {
    isolate("failed");
    let mut s = WorkStatus::new_agent("test-5", "test", "sess-1", "do things", None, None).unwrap();
    s.mark_failed("something broke".into()).unwrap();
    assert_eq!(s.state, WorkState::Failed);
    let finish = s.finish.as_ref().expect("failed stamps a finish");
    assert_eq!(finish.error.as_deref(), Some("something broke"));
    assert!(!finish.completed_at.is_empty());
}

#[test]
fn agent_read_roundtrip() {
    isolate("roundtrip");
    let mut s = WorkStatus::new_agent("test-6", "test", "sess-1", "do things", None, None).unwrap();
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
    let s = WorkStatus::new_command(
        "task-1",
        "sess-1",
        "cargo test",
        "cargo test --lib",
        CommandPaths::none(),
    )
    .unwrap();
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
    WorkStatus::new_command(
        "task-2",
        "sess-1",
        "cargo test",
        "cargo test --lib",
        CommandPaths::none(),
    )
    .unwrap();
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
            cause: RunCause::Ordinary,
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
    WorkStatus::new_command("task-3", "sess-1", "build", "make", CommandPaths::none()).unwrap();
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
            cause: RunCause::Ordinary,
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
            cause: RunCause::Ordinary,
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
    let mut agent = WorkStatus::new_agent("agt-i", "a", "sess-1", "p", None, None).unwrap();
    agent.mark_interrupted().unwrap();
    let mut command =
        WorkStatus::new_command("cmd-i", "sess-1", "c", "sleep 9", CommandPaths::none()).unwrap();
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
    let mut s = WorkStatus::new_agent("old-1", "old", "sess", "task", None, None).unwrap();
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

    let cleanup_result =
        cleanup_stale(StaleWindows::uniform(Duration::from_secs(7 * 86400))).unwrap();
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
        WorkStatus::new_agent(id, "review", "sess-1", "review the plan", None, None).unwrap();
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
    let mut orphan = WorkStatus::new_agent("d2-gone", "review", "sess-1", "x", None, None).unwrap();
    fs::remove_file(status_path("d2-gone")).unwrap();

    assert!(!orphan.reload(), "a missing file reports false");
    assert_eq!(orphan.id, "d2-gone", "and the handle is left untouched");
    assert_eq!(orphan.task, "x");
}

// ── #847: the run-capture sweep tests mtime BEFORE liveness ──────────────────
//
// The reordering is behaviour-preserving by construction — a file that is not
// stale is not removed whether its run is live or not — so these tests pin the
// OUTCOMES that must survive it. The speed-up itself is not observable from a
// unit test (it is the absence of work); it is measured on the live box.

/// A cutoff that puts `OLD` behind it and `YOUNG` in front of it.
fn sweep_windows() -> StaleWindows {
    StaleWindows::uniform(std::time::Duration::from_secs(7 * 24 * 60 * 60))
}

const OLD: std::time::Duration = std::time::Duration::from_secs(30 * 24 * 60 * 60);
const YOUNG: std::time::Duration = std::time::Duration::from_secs(60);

fn capture_path(name: &str) -> std::path::PathBuf {
    runs_dir().join(name)
}

/// Write a capture file and back-date its mtime by `age`.
///
/// `set_modified` rather than a wall-clock sleep: the sweep compares against a
/// cutoff the caller passes, so the file's own timestamp is the input under
/// test and must be exact.
fn write_capture(name: &str, age: std::time::Duration) {
    // `isolate` points the override at a path it does not create, and
    // `sweep_stale_runs` early-returns on a missing dir.
    fs::create_dir_all(runs_dir()).unwrap();
    let p = capture_path(name);
    fs::write(&p, b"stream").unwrap();
    let f = fs::OpenOptions::new().write(true).open(&p).unwrap();
    f.set_modified(std::time::SystemTime::now() - age).unwrap();
}

/// A stale capture whose run has NO status record is swept: a missing record
/// counts as not-live, or a stream whose record aged out would be stranded
/// forever. Both streams of the run go in one pass.
#[test]
fn sweep_removes_a_stale_capture_of_a_dead_run() {
    isolate("sweep_dead");
    write_capture("dead1.out", OLD);
    write_capture("dead1.err", OLD);

    let (_, removed) = sweep_stale_runs(sweep_windows()).unwrap();

    assert_eq!(removed, 2, "both streams of the dead run are swept");
    assert!(!capture_path("dead1.out").exists());
    assert!(!capture_path("dead1.err").exists());
}

/// The whole reason this cannot be a plain mtime sweep: a STALE capture whose
/// run is still live must survive. Reordering the tests must not lose this.
#[test]
fn sweep_keeps_a_stale_capture_of_a_live_run() {
    isolate("sweep_live");
    // `finish: None` — the record is the authority on liveness.
    WorkStatus::new_agent("live1", "review", "sess-1", "x", None, None).unwrap();
    write_capture("live1.out", OLD);
    write_capture("live1.err", OLD);

    let (_, removed) = sweep_stale_runs(sweep_windows()).unwrap();

    assert_eq!(removed, 0, "a live run's stream is never unlinked");
    assert!(capture_path("live1.out").exists());
    assert!(capture_path("live1.err").exists());
}

/// A young capture with no status record survives. Under the old order this
/// still held, but only after reading a record that did not exist; the
/// regression this pins is that the mtime test now decides it on its own.
#[test]
fn sweep_leaves_a_young_capture_alone() {
    isolate("sweep_young");
    write_capture("young1.out", YOUNG);

    let (_, removed) = sweep_stale_runs(sweep_windows()).unwrap();

    assert_eq!(removed, 0);
    assert!(capture_path("young1.out").exists());
}

// ── #849: a capture follows the KIND of its record ──────────────────────────
//
// One window for both kinds kept 58,455 captures and 30,112 status files on a
// 7-day rule, of which half were already past two days (measured on the host
// that filed #849). A finished command's record is read once, by the boot pass,
// and never again; a sub-agent's backs revived routing and must outlive the
// session that may come back for it. The two windows are now separate and the
// record's own `kind` picks between them — the caller cannot pass the wrong one
// for a file it has already parsed.

/// The shipped pair: 2 days for a finished command, 7 for a sub-agent.
fn split_windows() -> StaleWindows {
    StaleWindows::from_ages(
        Some(Duration::from_secs(2 * 24 * 60 * 60)),
        Some(Duration::from_secs(7 * 24 * 60 * 60)),
    )
}

/// Five days: past the command window, inside the agent one. The age the split
/// tests turn on.
const FIVE_DAYS: Duration = Duration::from_secs(5 * 24 * 60 * 60);

/// Rewrite a record's `finish.completed_at` so it reads as finished `age` ago.
///
/// The sweep judges a FINISHED record on that stamp, not on the file's mtime, so
/// the stamp is the input under test and has to be exact. The rewrite also
/// freshens the file's mtime, which keeps the json out of the capture walk when
/// the test override puts both in one directory.
fn backdate_finish(id: &str, age: Duration) {
    let path = status_path(id);
    let raw = fs::read_to_string(&path).unwrap();
    let mut parsed: serde_json::Value = serde_json::from_str(&raw).unwrap();
    let ts = chrono::Utc::now()
        .checked_sub_signed(chrono::Duration::seconds(age.as_secs() as i64))
        .unwrap()
        .to_rfc3339();
    parsed["finish"]["completed_at"] = serde_json::json!(ts);
    fs::write(&path, serde_json::to_string(&parsed).unwrap()).unwrap();
}

/// A finished detached command, stamped `age` ago, with both streams captured
/// at the same age.
fn finished_command(id: &str, age: Duration) {
    WorkStatus::new_command(
        id,
        "sess-cmd",
        "cmd",
        "echo hi",
        CommandPaths {
            output_out: capture_path(&format!("{id}.out")).display().to_string(),
            output_err: capture_path(&format!("{id}.err")).display().to_string(),
            pid: None,
        },
    )
    .unwrap();
    WorkStatus::finish_command(
        id,
        "sess-cmd",
        "cmd",
        "echo hi",
        CommandExit {
            success: true,
            code: 0,
            cause: RunCause::Ordinary,
            elapsed_secs: 0.1,
            output_bytes: 6,
        },
    )
    .unwrap();
    backdate_finish(id, age);
    write_capture(&format!("{id}.out"), age);
    write_capture(&format!("{id}.err"), age);
}

/// A finished SUB-AGENT, stamped `age` ago.
fn finished_agent(id: &str, age: Duration) {
    let mut status =
        WorkStatus::new_agent(id, "review", "sess-agent", "do things", None, None).unwrap();
    status.mark_running().unwrap();
    status.mark_completed("done".to_string()).unwrap();
    backdate_finish(id, age);
}

/// Five days is past a command's window and inside an agent's. The SAME age must
/// therefore give two different answers, or the split is not being applied.
#[test]
fn sweep_uses_the_short_window_for_a_finished_command() {
    isolate("split_command");
    finished_command("cmd1", FIVE_DAYS);

    let (_, removed) = sweep_stale_runs(split_windows()).unwrap();

    assert_eq!(
        removed, 2,
        "both streams of a 5-day-old finished command are past its 2-day window"
    );
    assert!(!capture_path("cmd1.out").exists());
    assert!(!capture_path("cmd1.err").exists());
}

/// The control for the test above: identical age, identical fixtures, only the
/// record's kind differs — and the captures survive.
#[test]
fn sweep_keeps_a_finished_agents_capture_on_the_long_window() {
    isolate("split_agent");
    finished_agent("agent1", FIVE_DAYS);
    write_capture("agent1.out", FIVE_DAYS);

    let (_, removed) = sweep_stale_runs(split_windows()).unwrap();

    assert_eq!(
        removed, 0,
        "a 5-day-old sub-agent is inside its 7-day window and its stream is kept"
    );
    assert!(capture_path("agent1.out").exists());
}

/// A command that has not finished is never swept, whatever its age: unlinking a
/// stream under a running child truncates output the agent is still reading.
#[test]
fn sweep_never_touches_a_running_command_however_old() {
    isolate("split_running");
    WorkStatus::new_command(
        "cmd2",
        "sess-cmd",
        "cmd",
        "sleep 999",
        CommandPaths {
            output_out: capture_path("cmd2.out").display().to_string(),
            output_err: capture_path("cmd2.err").display().to_string(),
            pid: None,
        },
    )
    .unwrap();
    write_capture("cmd2.out", OLD);

    let (_, removed) = sweep_stale_runs(split_windows()).unwrap();

    assert_eq!(removed, 0, "a live run's stream is never unlinked");
    assert!(capture_path("cmd2.out").exists());
}

/// The STATUS record follows the same split, not only its captures.
#[test]
fn cleanup_stale_ages_a_finished_command_record_on_its_short_window() {
    isolate("split_status");
    finished_command("cmd3", FIVE_DAYS);
    finished_agent("agent3", FIVE_DAYS);

    let (_, removed) = cleanup_stale(split_windows()).unwrap();

    assert_eq!(removed, 1, "only the finished command's record ages out");
    assert!(!status_path("cmd3").exists(), "the command record is gone");
    assert!(
        status_path("agent3").exists(),
        "the agent record is inside its long window and stays"
    );
}

// ── #848: the runs leg has ONE owner, and it is not this module ─────────────
//
// `cleanup_stale` and `logging::cleanup_old_temp_files` both swept captured
// streams, and both run on every boot, so a start paid the `tmp/runs` scan
// twice — the largest population on the box. The leg now lives only in the temp
// purge. This test FAILS before that change (both captures removed here) and
// passes after it, which is what makes it a regression test rather than a
// restatement.

/// A stale capture of a dead run survives `cleanup_stale`: the status sweep
/// owns `tmp/detached`, and the capture belongs to the temp purge.
#[test]
fn cleanup_stale_leaves_the_runs_leg_to_its_owner() {
    isolate("cleanup_no_runs");
    write_capture("dedup1.out", OLD);
    write_capture("dedup1.err", OLD);

    let (_, removed) =
        cleanup_stale(StaleWindows::uniform(Duration::from_secs(7 * 86400))).unwrap();

    assert_eq!(
        removed, 0,
        "cleanup_stale must not sweep captured streams (#848)"
    );
    assert!(
        capture_path("dedup1.out").exists(),
        "the .out capture is the temp purge's to remove, not this sweep's"
    );
    assert!(
        capture_path("dedup1.err").exists(),
        "the .err capture is the temp purge's to remove, not this sweep's"
    );
}

// ── #924: a deliberate stop is a recorded outcome, not a deletion ───────────
//
// `close_agent` promised to terminate AND clean up, and the second half was a
// lie: nothing was ever removed, so a parent that trusted it kept polling a
// record it thought was gone. The tool is now `stop_agent` and the record is
// the product — it is what `agent_progress` reads afterwards and what
// `resume_agent` rehydrates from. These two tests pin that contract on the
// status file itself, because the tool's tests can only see the in-memory map.

/// `mark_cancelled` persists a terminal record that keeps its identity.
///
/// The mutant this kills: a `mark_cancelled` that only sets the in-memory state
/// (or reuses `mark_failed`). Either would pass a map-level assertion in the
/// tool's tests while leaving the file `Running`, so a parent that restarts
/// rehydrates a child it already stopped — and `reconcile_with` then rewrites
/// the honest "cancelled" as "interrupted", blaming a restart for a decision
/// the parent made.
#[test]
fn agent_cancelled_records_a_terminal_state_that_is_not_a_failure() {
    // Kills the mutant that leaves Cancelled non-terminal, so reconcile_with marks it Interrupted at boot and the stop is lost.
    isolate("cancelled");
    let mut s =
        WorkStatus::new_agent("test-cxl", "test", "sess-1", "do things", None, None).unwrap();
    s.mark_cancelled().unwrap();

    // Re-read from disk: the assertion is about what SURVIVES the process, so
    // reading the handle we just mutated would prove nothing.
    let back =
        WorkStatus::read("test-cxl").expect("the record is still there — a stop is not a delete");
    assert_eq!(back.state, WorkState::Cancelled);
    assert!(
        back.state.is_terminal(),
        "Cancelled must be terminal, or reconcile_with marks it Interrupted at boot and the stop is lost"
    );
    let finish = back
        .finish
        .as_ref()
        .expect("a stop stamps a finish, otherwise the record never ages out");
    assert!(!finish.completed_at.is_empty());
    assert!(
        finish.output_full.is_none(),
        "a stop produces no output — storing one would let the parent read a result the child never finished"
    );
    assert!(
        finish.error.is_none(),
        "a deliberate stop is not an error; putting one here reports the parent's own decision as a crash"
    );
}

/// A cancelled agent keeps its original label, task and spawn instant.
///
/// The mutant this kills: `mark_cancelled` rebuilding the record (or a `stop`
/// path that re-mints the id). `resume_agent` rehydrates from these fields, so
/// a rebuilt record silently becomes a different agent — one whose task the
/// model has to re-read from a prompt that no longer matches what it was doing.
#[test]
fn a_cancelled_agent_keeps_its_identity() {
    // Kills the mutant that resets label, task or spawned_at on a stop, making stopped work indistinguishable from a fresh spawn.
    isolate("cancelled_identity");
    let mut s = WorkStatus::new_agent(
        "test-cxl-id",
        "the-label",
        "sess-1",
        "the original task",
        None,
        None,
    )
    .unwrap();
    let spawned_at = s.spawned_at.clone();
    s.mark_cancelled().unwrap();

    let back = WorkStatus::read("test-cxl-id").expect("record survives the stop");
    assert_eq!(back.label, "the-label");
    assert_eq!(back.task, "the original task");
    assert_eq!(
        back.spawned_at, spawned_at,
        "a stop must not look like a fresh spawn — `agent_progress` renders elapsed time from this"
    );
    assert_eq!(back.kind, WorkKind::Agent);
}
