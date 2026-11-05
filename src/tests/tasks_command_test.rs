//! `/tasks` — the only surface a user has for a detached run (#808 cue 12).
//!
//! The command reads TWO homes, because a run moves when it ends: a live run
//! exists only in the in-memory manager (no terminal record yet) and a
//! finished one only on disk. These tests pin the renderer's half — the row
//! format, the session filter, the cause line, and the empty case — against a
//! temp status dir so nothing depends on this box's 25k-file `tmp/detached`.
//!
//! The scan itself is deliberately bounded (`want * 8`, min 64) rather than a
//! `list_all()` + parse-everything walk; these tests do not pin the constant,
//! they pin the behaviour that matters: a run belonging to ANOTHER session is
//! never listed, and a record written before #786 still says something true.

use crate::brain::agent::service::work_status::{
    self, CommandExit, CommandPaths, RunCause, WorkFinish, WorkKind, WorkState, WorkStatus,
};
use crate::channels::commands::render_tasks_rows;

/// A scratch status dir bound to this test's thread. `status_dir()` reads the
/// override, so `new_command`/`finish_command` write here and nowhere else.
struct Scratch(std::path::PathBuf);

impl Scratch {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "oc-tasks-{tag}-{}",
            uuid::Uuid::new_v4().as_simple()
        ));
        std::fs::create_dir_all(&dir).expect("scratch dir");
        work_status::test_override::set(dir.clone());
        Self(dir)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        work_status::test_override::clear();
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn finished(
    id: &str,
    session: &str,
    label: &str,
    success: bool,
    code: i32,
    cause: RunCause,
    elapsed: f32,
) {
    WorkStatus::new_command(id, session, label, label, CommandPaths::none()).expect("spawn write");
    WorkStatus::finish_command(
        id,
        session,
        label,
        label,
        CommandExit {
            success,
            code,
            cause,
            elapsed_secs: elapsed,
            output_bytes: 12,
        },
    )
    .expect("finish write");
}

/// A record with no `finish.cause` — the shape every status file written by a
/// pre-#786 binary has. Written by hand because `finish_command` always stamps
/// a cause; the point is to prove the reader does not invent one.
fn legacy_record(id: &str, session: &str, label: &str, code: i32) {
    let status = WorkStatus {
        id: id.to_string(),
        kind: WorkKind::Command,
        session_id: session.to_string(),
        parent_session_id: None,
        label: label.to_string(),
        task: label.to_string(),
        spawned_at: "2026-01-01T00:00:00Z".to_string(),
        state: WorkState::Failed,
        progress: None,
        finish: Some(WorkFinish {
            completed_at: "2026-01-01T00:00:05Z".to_string(),
            success: Some(false),
            code: Some(code),
            cause: None,
            elapsed_secs: Some(5.0),
            output_bytes: Some(3),
            error: None,
            output_full: None,
        }),
        output_out: None,
        output_err: None,
        pid: None,
    };
    let path = work_status::status_path(id);
    std::fs::write(path, serde_json::to_string_pretty(&status).unwrap()).expect("legacy write");
}

#[test]
fn empty_status_dir_says_so_in_one_line() {
    let _scratch = Scratch::new("empty");
    assert_eq!(
        render_tasks_rows("", "session-a", &[]),
        "no detached runs in this session"
    );
}

#[test]
fn a_finished_row_names_why_it_ended_not_just_the_code() {
    let _scratch = Scratch::new("cause");
    // 137 twice, two different meanings — the whole reason #786 exists.
    finished(
        "run-ceiling",
        "session-a",
        "sleep 4000",
        false,
        137,
        RunCause::Ceiling { limit_secs: 3600 },
        3600.0,
    );
    finished(
        "run-signal",
        "session-a",
        "pytest -q",
        false,
        137,
        RunCause::Signalled { signal: 9 },
        2.0,
    );

    let out = render_tasks_rows("", "session-a", &[]);
    assert!(out.contains("ceiling 3600s"), "got: {out}");
    assert!(out.contains("signal 9 (killed)"), "got: {out}");
    // Two rows, and the header counts them.
    assert!(out.starts_with("## Detached runs (2)"), "got: {out}");
}

#[test]
fn another_sessions_runs_are_never_listed() {
    let _scratch = Scratch::new("scope");
    finished("run-mine", "session-a", "cargo build", true, 0, RunCause::Ordinary, 4.0);
    finished("run-theirs", "session-b", "rm -rf /tmp/x", true, 0, RunCause::Ordinary, 4.0);

    let out = render_tasks_rows("", "session-a", &[]);
    assert!(out.contains("cargo build"), "got: {out}");
    assert!(
        !out.contains("rm -rf"),
        "a foreign session's command text must not leak: {out}"
    );
    assert!(out.starts_with("## Detached runs (1)"), "got: {out}");
}

#[test]
fn a_live_run_is_listed_once_even_though_it_is_also_on_disk() {
    let _scratch = Scratch::new("live");
    finished("run-live", "session-a", "sleep 30", true, 0, RunCause::Ordinary, 1.0);
    let live = vec![(
        "run-live".to_string(),
        "▶️ `sleep 30` · running 12s".to_string(),
    )];

    let out = render_tasks_rows("", "session-a", &live);
    // The live row is the one shown, and the disk arm skips the same id.
    assert!(out.contains("running 12s"), "got: {out}");
    assert!(out.starts_with("## Detached runs (1)"), "got: {out}");
}

#[test]
fn a_pre_786_record_falls_back_to_the_code_instead_of_claiming_ordinary() {
    let _scratch = Scratch::new("legacy");
    legacy_record("run-legacy", "session-a", "make test", 7);

    let out = render_tasks_rows("", "session-a", &[]);
    assert!(out.contains("exit 7"), "got: {out}");
    assert!(
        !out.contains("ceiling") && !out.contains("signal"),
        "a missing cause must not be invented: {out}"
    );
}

#[test]
fn the_row_cap_clamps_and_never_panics_on_junk_args() {
    let _scratch = Scratch::new("args");
    // Non-numeric, zero and huge all resolve to a usable bound rather than an
    // error: this is a command a user types, not an API.
    for args in ["", "abc", "0", "99999"] {
        let out = render_tasks_rows(args, "session-a", &[]);
        assert_eq!(out, "no detached runs in this session", "args={args:?}");
    }
}
