use crate::brain::tools::subagent::status::*;
use std::fs;
use std::time::Duration;

fn isolate(tag: &str) {
    let dir = std::env::temp_dir().join(format!(
        "opencrabs-subagent-test-{}-{}",
        tag,
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    test_override::set(dir);
}

#[test]
fn status_dir_returns_correct_path() {
    let home = crate::config::opencrabs_home();
    let expected = home.join("tmp").join("subagents");
    assert_eq!(status_dir(), expected);
}

#[test]
fn status_path_ends_with_json() {
    let p = status_path("abc123");
    assert_eq!(p.file_name().unwrap().to_str().unwrap(), "abc123.json");
}

#[test]
fn new_status_is_pending() {
    isolate("new_pending");
    let s = AgentStatus::new("test-1", "test", "sess-1", "do things").unwrap();
    assert_eq!(s.state, AgentState::Pending);
    assert_eq!(s.id, "test-1");
    assert_eq!(s.label, "test");
}

#[test]
fn status_transitions_to_running() {
    isolate("running");
    let mut s = AgentStatus::new("test-2", "test", "sess-1", "do things").unwrap();
    s.mark_running().unwrap();
    assert_eq!(s.state, AgentState::Running);
}

#[test]
fn status_parks_as_awaiting_input_and_flips_back() {
    // #1183: a parked agent used to keep reading `state: "Running"` with
    // `completed_at: null`, misleading every consumer into waiting on work
    // that was already finished. The parked state is distinct, not terminal,
    // and round-trips through the file so external readers see it too.
    isolate("awaiting_input");
    let mut s = AgentStatus::new("test-7", "test", "sess-1", "do things").unwrap();
    s.mark_running().unwrap();
    s.mark_awaiting_input().unwrap();
    assert_eq!(s.state, AgentState::AwaitingInput);
    assert!(!s.state.is_terminal(), "parked is not a terminal state");
    assert!(s.completed_at.is_none(), "parked stamps no completion time");

    let reread = AgentStatus::read("test-7").expect("should read back");
    assert_eq!(reread.state, AgentState::AwaitingInput);

    // Follow-up input flips the file back to working.
    s.mark_running().unwrap();
    assert_eq!(s.state, AgentState::Running);
    let reread = AgentStatus::read("test-7").expect("should read back");
    assert_eq!(reread.state, AgentState::Running);
}

#[test]
fn status_progress_snapshot() {
    isolate("progress");
    let mut s = AgentStatus::new("test-3", "test", "sess-1", "do things").unwrap();
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
fn status_completed_sets_timestamp() {
    isolate("completed");
    let mut s = AgentStatus::new("test-4", "test", "sess-1", "do things").unwrap();
    s.mark_completed("done".into()).unwrap();
    assert_eq!(s.state, AgentState::Completed);
    assert!(s.completed_at.is_some());
    assert_eq!(s.output_full, Some("done".to_string()));
}

#[test]
fn status_failed_sets_error() {
    isolate("failed");
    let mut s = AgentStatus::new("test-5", "test", "sess-1", "do things").unwrap();
    s.mark_failed("something broke".into()).unwrap();
    assert_eq!(s.state, AgentState::Failed);
    assert_eq!(s.error, Some("something broke".to_string()));
    assert!(s.completed_at.is_some());
}

#[test]
fn status_read_roundtrip() {
    isolate("roundtrip");
    let mut s = AgentStatus::new("test-6", "test", "sess-1", "do things").unwrap();
    s.mark_running().unwrap();
    s.update_progress(2, Some("write_file".into()), None)
        .unwrap();

    let read = AgentStatus::read("test-6").expect("should read back");
    assert_eq!(read.id, "test-6");
    assert_eq!(read.state, AgentState::Running);
    assert_eq!(read.progress.unwrap().iteration, 2);
}

#[test]
fn cleanup_removes_old_files() {
    isolate("cleanup");
    let mut s = AgentStatus::new("old-1", "old", "sess", "task").unwrap();
    s.mark_completed("done".into()).unwrap();

    let old_ts = chrono::Utc::now()
        .checked_sub_signed(chrono::Duration::days(8))
        .unwrap()
        .to_rfc3339();
    let mut raw = fs::read_to_string(status_path("old-1")).unwrap();
    let mut parsed: serde_json::Value = serde_json::from_str(&raw).unwrap();
    parsed["completed_at"] = serde_json::json!(old_ts);
    raw = serde_json::to_string_pretty(&parsed).unwrap();
    fs::write(status_path("old-1"), raw).unwrap();

    let cleanup_result = cleanup_stale(Duration::from_secs(7 * 86400)).unwrap();
    assert!(cleanup_result.1 >= 1, "should have removed at least 1 file");
    assert!(AgentStatus::read("old-1").is_none());
}

// ── #147: output_full persistence, serde-skip, legacy compat ─────────

/// Roundtrip: a 5 KB report survives byte-exact in `output_full`, and the
/// persisted JSON carries no `output_summary` field at all.
#[test]
fn output_full_roundtrip_is_byte_exact_and_summary_is_gone() {
    isolate("output_full");
    let report: String = "# REVIEW\n\n"
        .to_string()
        + &"detail line with plenty of words to bulk it up.\n".repeat(128);
    assert!(report.chars().count() > 5000, "fixture must be ~5 KB");
    let mut s = AgentStatus::new("of-1", "review", "sess-of", "review the skill").unwrap();
    s.mark_completed(report.clone()).unwrap();
    assert_eq!(s.output_full.as_deref(), Some(report.as_str()));

    let raw = fs::read_to_string(status_path("of-1")).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(
        parsed["output_full"].as_str(),
        Some(report.as_str()),
        "persisted file holds the complete report byte-exact"
    );
    assert!(
        parsed.get("output_summary").is_none(),
        "removed field must not appear in persisted JSON"
    );
}

/// Serde-skip: a fresh Pending file contains neither `output_full` nor any
/// `null` placeholders for it — old tools reading the file see no change.
#[test]
fn output_full_absent_field_stays_absent() {
    isolate("output_full_skip");
    let _s = AgentStatus::new("of-2", "idle", "sess-of2", "nothing").unwrap();
    let raw = fs::read_to_string(status_path("of-2")).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&raw).unwrap();
    assert!(parsed.get("output_full").is_none());
    assert!(parsed.get("output_summary").is_none());
}

/// Legacy compat: a pre-#147 file carrying `output_summary` deserializes
/// cleanly (unknown field ignored) and no backfill happens.
#[test]
fn legacy_output_summary_file_deserializes_cleanly() {
    isolate("output_full_legacy");
    let legacy = serde_json::json!({
        "id": "of-3", "label": "old", "parent_session_id": "sess-of3",
        "state": "Completed", "prompt": "old task",
        "started_at": "2026-08-28T09:00:00+00:00",
        "completed_at": "2026-08-28T09:30:00+00:00",
        "output_summary": "all done"
    });
    fs::write(status_path("of-3"), serde_json::to_string_pretty(&legacy).unwrap()).unwrap();
    let s = AgentStatus::read("of-3").expect("legacy file parses");
    assert_eq!(s.state, AgentState::Completed);
    assert_eq!(s.output_full, None, "no backfill of the old stub");
}
