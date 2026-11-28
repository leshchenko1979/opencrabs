//! Startup reconciliation of orphaned sub-agent status files (#1038, #192).
//!
//! A sub-agent dies with the process; its status file does not. Every file
//! still `Pending`, `Running`, or `AwaitingInput` at startup belongs to an
//! agent that no longer exists, and must stop reading as live.

use crate::brain::agent::service::work_status::{
    self, CommandPaths, StaleWindows, WorkState, WorkStatus,
};
use crate::brain::tools::subagent::reconcile::reconcile;
use std::fs;
use uuid::Uuid;

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

/// The production split, pinned (#849): a finished detached command ages out on
/// 2 days, a sub-agent's record on 7. Pinned rather than read from config so a
/// host whose `[agent.retention]` differs cannot turn a correct pass red.
fn test_windows() -> StaleWindows {
    StaleWindows::from_ages(
        Some(std::time::Duration::from_secs(2 * 86_400)),
        Some(std::time::Duration::from_secs(7 * 86_400)),
    )
}

#[test]
fn a_running_agent_becomes_interrupted() {
    isolate("running");
    let mut s =
        WorkStatus::new_agent("agent-1", "build docs", "sess-a", "do things", None, None).unwrap();
    s.mark_running().unwrap();

    let orphans = reconcile();

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
    WorkStatus::new_agent("agent-2", "lint", "sess-b", "do things", None, None).unwrap();

    let orphans = reconcile();

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
    let mut s =
        WorkStatus::new_agent("agent-4", "audit", "sess-d", "do things", None, None).unwrap();
    s.mark_running().unwrap();
    s.mark_awaiting_input().unwrap();

    let orphans = reconcile();

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
    let mut s =
        WorkStatus::new_agent("agent-3", "test", "sess-c", "do things", None, None).unwrap();
    s.mark_running().unwrap();

    let orphans = reconcile();

    let finish = orphans[0].finish.as_ref().expect("finish stamped");
    assert!(finish.error.as_deref().unwrap_or("").contains("restarted"));
    assert!(!finish.completed_at.is_empty());
}

#[test]
fn terminal_agents_are_left_alone() {
    isolate("terminal");
    let mut done = WorkStatus::new_agent("agent-done", "a", "sess-d", "p", None, None).unwrap();
    done.mark_completed("output".to_string()).unwrap();
    let mut failed = WorkStatus::new_agent("agent-failed", "b", "sess-d", "p", None, None).unwrap();
    failed.mark_failed("boom".to_string()).unwrap();

    let orphans = reconcile();

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
        None,
    )
    .unwrap();
    s.mark_running().unwrap();

    let orphans = reconcile();

    assert_eq!(orphans[0].parent_session_id.as_deref(), Some("sess-parent"));
}

#[test]
fn reconciling_is_idempotent() {
    // A second startup must not re-report an agent already accounted for.
    isolate("idempotent");
    let mut s = WorkStatus::new_agent("agent-5", "x", "sess-e", "p", None, None).unwrap();
    s.mark_running().unwrap();

    assert_eq!(reconcile().len(), 1);
    assert!(reconcile().is_empty());
}

#[test]
fn a_missing_status_dir_is_not_an_error() {
    isolate("missing");
    assert!(reconcile().is_empty());
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
    let mut s = WorkStatus::new_agent("agent-6", "y", "sess-f", "p", None, None).unwrap();
    s.mark_running().unwrap();

    let orphans = reconcile();

    assert_eq!(orphans.len(), 1, "the healthy file is still reconciled");
    assert_eq!(orphans[0].id, "agent-6");
}

#[test]
fn commands_are_not_reconciled_by_agent_pass() {
    isolate("commands");
    WorkStatus::new_command(
        "cmd-1",
        "test cmd",
        "sess-c",
        "cargo test",
        CommandPaths::none(),
    )
    .unwrap();
    assert!(reconcile().is_empty());
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
    fs::write(legacy_dir.join("legacy-agent-1.json"), old_json.to_string()).unwrap();

    let migrated = work_status::migrate_legacy_dir(&legacy_dir);
    assert_eq!(migrated, 1);

    let orphans = reconcile();
    assert_eq!(orphans.len(), 1);
    assert_eq!(orphans[0].id, "legacy-agent-1");
    assert_eq!(orphans[0].state, WorkState::Interrupted);
}

/// Backdate a file's mtime so the mtime arm of the staleness decision can be
/// exercised without sleeping.
fn backdate(path: &std::path::Path, days: u64) {
    let f = fs::File::options()
        .write(true)
        .open(path)
        .expect("fixture file must exist");
    f.set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(days * 86_400))
        .expect("backdate mtime");
}

/// An RFC3339 stamp `days` in the past.
fn stamp_days_ago(days: i64) -> String {
    chrono::Utc::now()
        .checked_sub_signed(chrono::Duration::days(days))
        .expect("clock within range")
        .to_rfc3339()
}

/// #1081: the two decisions this pass used to make in one walk are now made by
/// two owners, and this pins the split.
///
/// Boot reconciles — it marks records whose process is gone. Retention sweeps —
/// it deletes records that have aged out, on the periodic maintenance sweep,
/// because boot is not a retention event and a daemon that stays up must still
/// sweep. Every fixture below is still aged exactly as it was when one pass made
/// both decisions, so the assertions are unchanged; what changed is that they
/// now hold across two calls instead of one.
#[test]
fn reconcile_interrupts_and_retention_sweeps_a_mixed_directory() {
    // #858: this used to be two walks of one directory. The reconcile pass must
    // still read and parse each file once, and the file it interrupts must still
    // be on disk when it returns — deleting it would destroy the record of an
    // agent whose session has not been told yet.
    isolate("one_pass");

    // A live orphan, ALSO old on disk: interrupted, never swept in that pass.
    let mut orphan =
        WorkStatus::new_agent("orphan-1", "mid-flight", "sess-o", "p", None, None).unwrap();
    orphan.mark_running().unwrap();
    backdate(&work_status::status_path("orphan-1"), 8);

    // Finished 8 days ago with a FRESH mtime: the finish arm sweeps it.
    let mut done =
        WorkStatus::new_agent("done-old", "finished", "sess-d", "p", None, None).unwrap();
    done.mark_completed("ok".into()).unwrap();
    let path = work_status::status_path("done-old");
    let mut raw: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    raw["finish"]["completed_at"] = serde_json::json!(stamp_days_ago(8));
    fs::write(&path, serde_json::to_string(&raw).unwrap()).unwrap();

    // A FINISHED COMMAND 4 days old (#849): inside the 7-day window this pass
    // used before the split, past the 2-day command window now. Same mtime
    // treatment as `done-old`, so only the KIND decides its fate.
    let mut cmd_done = WorkStatus::new_command(
        "cmd-done-4d",
        "sess-c",
        "finished command",
        "cargo test",
        CommandPaths::none(),
    )
    .unwrap();
    cmd_done.mark_completed("ok".into()).unwrap();
    let cmd_path = work_status::status_path("cmd-done-4d");
    let mut cmd_raw: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&cmd_path).unwrap()).unwrap();
    cmd_raw["finish"]["completed_at"] = serde_json::json!(stamp_days_ago(4));
    fs::write(&cmd_path, serde_json::to_string(&cmd_raw).unwrap()).unwrap();

    // The SAME age as `cmd-done-4d`, but an AGENT. The pair is what pins the
    // split: age alone cannot decide these two, only the record's KIND can.
    let mut agent_done =
        WorkStatus::new_agent("agent-done-4d", "finished", "sess-a", "p", None, None).unwrap();
    agent_done.mark_completed("ok".into()).unwrap();
    let agent_path = work_status::status_path("agent-done-4d");
    let mut agent_raw: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&agent_path).unwrap()).unwrap();
    agent_raw["finish"]["completed_at"] = serde_json::json!(stamp_days_ago(4));
    fs::write(&agent_path, serde_json::to_string(&agent_raw).unwrap()).unwrap();

    // An unfinished COMMAND with an old mtime: not an orphan (only agents are
    // interrupted), swept by the mtime arm.
    WorkStatus::new_command(
        "cmd-old",
        "sess-c",
        "old command",
        "cargo test",
        CommandPaths::none(),
    )
    .unwrap();
    backdate(&work_status::status_path("cmd-old"), 8);

    // A corrupt file with an old mtime: swept, never silently skipped.
    let corrupt = work_status::status_dir().join("corrupt-old.json");
    fs::write(&corrupt, "not json at all").unwrap();
    backdate(&corrupt, 8);

    // A fresh unfinished command: left alone.
    WorkStatus::new_command("cmd-fresh", "sess-c", "fresh", "ls", CommandPaths::none()).unwrap();

    // The boot pass: interrupt only, sweep nothing.
    let orphans = reconcile();

    assert_eq!(orphans.len(), 1, "only the live orphan is interrupted");
    assert_eq!(orphans[0].id, "orphan-1");
    assert_eq!(orphans[0].state, WorkState::Interrupted);

    assert!(
        work_status::status_path("orphan-1").exists(),
        "the file interrupted in this pass must survive it: deleting it here would \
         destroy the record of an agent whose session still has to be told"
    );
    assert!(
        work_status::status_path("done-old").exists(),
        "#1081: boot no longer deletes. Retention belongs to the periodic sweep, \
         so an aged-out record must still be on disk when the boot pass returns"
    );
    assert!(
        work_status::status_path("cmd-old").exists(),
        "#1081: the boot pass must not remove a file for being old"
    );
    assert!(corrupt.exists(), "#1081: nor for being unreadable");

    // The retention sweep: remove what has aged out, on the window its KIND gets.
    let (scanned, removed) = work_status::cleanup_stale(test_windows()).unwrap();
    assert!(
        scanned >= 7,
        "the sweep saw every fixture file, got {scanned}"
    );
    assert_eq!(
        removed, 4,
        "done-old (8d, agent window), cmd-done-4d (4d, command window), cmd-old \
         (old mtime) and corrupt-old are removed — agent-done-4d is not, and \
         neither are the interrupted orphan or the fresh command"
    );

    assert!(
        !work_status::status_path("done-old").exists(),
        "finished-and-old is swept"
    );
    assert!(
        !work_status::status_path("cmd-done-4d").exists(),
        "#849: a finished COMMAND past its 2-day window is swept, though 4 days \
         is inside the 7-day window the boot pass used for every kind"
    );
    assert!(
        work_status::status_path("agent-done-4d").exists(),
        "#849: the same age as a COMMAND, but an agent keeps the 7-day window — \
         if this is swept the sweep is deciding on age, not on kind"
    );
    assert!(
        !work_status::status_path("cmd-old").exists(),
        "unfinished-and-old is swept by its mtime"
    );
    assert!(!corrupt.exists(), "corrupt-and-old is swept, not skipped");
    assert!(
        work_status::status_path("cmd-fresh").exists(),
        "a fresh file is left alone"
    );
    assert!(
        work_status::status_path("orphan-1").exists(),
        "the just-interrupted agent's fresh finish stamp keeps it for another pass"
    );
}

#[test]
fn the_pass_walks_the_status_dir_once() {
    // The one property behaviour alone cannot show. A second `read_dir` in this
    // module is a second walk of 30k files, whatever the answers look like — so
    // the guard is structural on purpose.
    let src = include_str!("../brain/tools/subagent/reconcile.rs");
    assert_eq!(
        src.matches("read_dir").count(),
        1,
        "reconcile.rs must walk the status directory exactly once (#858)"
    );
    assert_eq!(
        src.matches("read_to_string").count(),
        1,
        "one read per file: a second read_to_string is a second walk"
    );
    assert_eq!(
        src.matches("from_str").count(),
        1,
        "one parse per file: a second from_str is a second walk"
    );
}

#[test]
fn is_stale_keeps_the_three_way_decision() {
    // The predicate both sweepers share. Each arm is exercised with a fixture
    // that ONLY that arm can answer correctly.
    isolate(&format!("is_stale-{}", Uuid::new_v4().simple()));
    work_status::ensure_dir().unwrap();

    // (1) finished 8 days ago, fresh mtime → stale by the finish, not the mtime.
    let mut done = WorkStatus::new_agent("s-done", "d", "sess", "p", None, None).unwrap();
    done.mark_completed("ok".into()).unwrap();
    done.finish.as_mut().expect("finish stamped").completed_at = stamp_days_ago(8);
    let finished_windows = test_windows();
    assert!(
        work_status::is_stale(
            Some(&done),
            &work_status::status_path("s-done"),
            finished_windows
        ),
        "a finished-and-old file is stale whatever its mtime says"
    );

    // (2) unfinished, fresh mtime → not stale. Anchor the agent cutoff to the
    // file's on-disk mtime so coarse timestamp resolution cannot make a brand-new
    // file look older than a cutoff captured before the write.
    let pending = WorkStatus::new_agent("s-pending", "p", "sess", "p", None, None).unwrap();
    let pending_path = work_status::status_path("s-pending");
    let pending_mtime = pending_path
        .metadata()
        .expect("pending status file exists")
        .modified()
        .expect("pending status file is datable");
    let agent_cutoff = pending_mtime
        .checked_sub(std::time::Duration::from_secs(1))
        .unwrap_or(pending_mtime);
    let pending_windows = StaleWindows {
        command: agent_cutoff,
        agent: agent_cutoff,
    };
    assert!(
        !work_status::is_stale(Some(&pending), &pending_path, pending_windows),
        "an unfinished file with a fresh mtime is not stale"
    );

    // (3) unparseable (`None`), judged on mtime — and an unstatable path is
    //     treated as stale, which is the arm's documented fallback.
    assert!(
        work_status::is_stale(
            None,
            &work_status::status_path("never-written"),
            pending_windows
        ),
        "an undatable file falls back to the mtime arm, which cannot stat it"
    );
}

#[test]
fn an_agent_spawned_during_the_pass_is_not_interrupted() {
    // #858 made the pass run detached, which opened a window the two-walk shape
    // never had: a live agent can spawn WHILE the sweep walks the directory.
    // Interrupting its own fresh status file would corrupt the record of a run
    // that is still going, so the pass bounds itself by its start instant.
    // `spawned_during_pass` is the only guard standing between a detached sweep
    // and a live agent's file, so it gets a fixture of its own.
    isolate("spawned_during_pass");

    let mut live =
        WorkStatus::new_agent("live-1", "started mid-pass", "sess-l", "p", None, None).unwrap();
    live.mark_running().unwrap();

    // The file's own spawn stamp is what the guard reads, so age the field, not
    // the mtime: a pass that started before this instant must treat the agent
    // as live.
    let path = work_status::status_path("live-1");
    let mut raw: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    raw["spawned_at"] = serde_json::json!(
        chrono::Utc::now()
            .checked_add_signed(chrono::Duration::hours(1))
            .expect("clock within range")
            .to_rfc3339()
    );
    fs::write(&path, serde_json::to_string(&raw).unwrap()).unwrap();

    let orphans = reconcile();

    assert!(
        orphans.iter().all(|o| o.id != "live-1"),
        "an agent spawned after the pass began is still running: interrupting it \
         would rewrite a live run's status file out from under it"
    );
    let after = WorkStatus::read("live-1").expect("the live agent's file must still exist");
    assert_eq!(
        after.state,
        WorkState::Running,
        "the live agent must still read as running after the pass"
    );
}

/// #1081 — a record UPDATED after the pass began is not the pass's to interrupt,
/// whatever its `spawned_at` says.
///
/// The measured clobber: boot recovery revives a child (or a parent resumes
/// one), the child keeps the `spawned_at` of its ORIGINAL spawn, and the pass —
/// reading only that stamp — classifies it as a stranded orphan and stamps
/// `Interrupted` over it. On the host that filed #1081 a child that had already
/// delivered a 12,000-character report still read `Interrupted`.
///
/// The spawn stamp cannot see this. The progress heartbeat can: a revived child
/// that is still working restamps `progress.updated_at` on every tool-loop
/// iteration. The fixture ages `spawned_at` AND the file's mtime into the past
/// so the heartbeat is the only fresh signal — a pass that reads either of the
/// other two alone fails here.
#[test]
fn an_agent_updated_during_the_pass_is_not_interrupted() {
    isolate("updated_during_pass");

    let mut live =
        WorkStatus::new_agent("live-2", "revived mid-pass", "sess-l", "p", None, None).unwrap();
    live.mark_running().unwrap();
    let path = work_status::status_path("live-2");

    let mut raw: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    raw["spawned_at"] = serde_json::json!(stamp_days_ago(1));
    raw["progress"] = serde_json::json!({
        "iteration": 2,
        "tool_count": 5,
        "last_tool": "bash",
        "last_event": null,
        "updated_at": chrono::Utc::now()
            .checked_add_signed(chrono::Duration::hours(1))
            .expect("clock within range")
            .to_rfc3339(),
    });
    fs::write(&path, serde_json::to_string(&raw).unwrap()).unwrap();
    // Writing the fixture moved the mtime; age it back so the mtime arm answers
    // "old" and cannot stand in for the progress arm under test.
    backdate(&path, 1);

    let orphans = reconcile();

    assert!(
        orphans.iter().all(|o| o.id != "live-2"),
        "a record whose progress is newer than the pass began belongs to a live \
         run: interrupting it destroys the report that run just wrote"
    );
    let after = WorkStatus::read("live-2").expect("the live agent's file must still exist");
    assert_eq!(
        after.state,
        WorkState::Running,
        "the revived agent must still read as running after the pass"
    );
}

/// #1081 — the same guard through the FILE clock, which is the only signal a
/// finished child leaves.
///
/// A child that finishes during the pass writes a terminal state and no progress
/// heartbeat; a record written before `progress` existed has none either. Every
/// state transition rewrites the file, though, so its mtime always moves. The
/// fixture ages `spawned_at` into the past and dates the mtime into the future,
/// making the mtime the only signal that can answer.
#[test]
fn an_agent_rewritten_during_the_pass_is_not_interrupted() {
    isolate("rewritten_during_pass");

    let mut live =
        WorkStatus::new_agent("live-3", "finished mid-pass", "sess-m", "p", None, None).unwrap();
    live.mark_running().unwrap();
    let path = work_status::status_path("live-3");

    let mut raw: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    raw["spawned_at"] = serde_json::json!(stamp_days_ago(1));
    fs::write(&path, serde_json::to_string(&raw).unwrap()).unwrap();

    // The child's own write, landing after any pass boundary instant.
    fs::File::options()
        .write(true)
        .open(&path)
        .expect("fixture file must exist")
        .set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(3600))
        .expect("date the record's mtime after the pass");

    let orphans = reconcile();

    assert!(
        orphans.iter().all(|o| o.id != "live-3"),
        "a record whose file moved after the pass began was written by a live \
         run, so the pass must leave it alone"
    );
    let after = WorkStatus::read("live-3").expect("the live agent's file must still exist");
    assert_eq!(
        after.state,
        WorkState::Running,
        "the rewritten record must keep the state its writer gave it"
    );
}
