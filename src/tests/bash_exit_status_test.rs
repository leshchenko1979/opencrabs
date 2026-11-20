//! Exit-status reporting for the bash tool (#910, #914).
//!
//! Two defects on one instrument.
//!
//! #910: the tool spawns the command as one verbatim argument to a shell and
//! reports the shell's status, and no shell reports a PIPELINE's status except
//! its LAST element's. So `false | tail -1` — a failing gate trimmed with
//! `tail` — came back as `exit 0 (success)`, and the background wake message
//! said so too. The fix does NOT turn on `pipefail`: `… | head -20` is the
//! normal way an agent trims output, and `pipefail` reports the producer's
//! SIGPIPE (141) as a failure. Instead the shell reports `${PIPESTATUS[@]}`,
//! SIGPIPEs are ignored, and the LEFTMOST remaining non-zero element is the
//! status.
//!
//! #914: on success the body carried no status at all, so `(no output)`,
//! `(no matches found)` and "the command never ran" were the same string and a
//! lane had to spend a round-trip telling them apart. Every body now ends with
//! `exit=<N>`.
//!
//! The end-to-end cases drive a real `execute()` because that is the surface
//! the model reads; the unit cases pin the policy underneath it.

use crate::brain::tools::Tool;
use crate::brain::tools::ToolExecutionContext;
use crate::brain::tools::ToolResult;
use crate::brain::tools::bash::*;
use crate::utils::shell::shell_pair;
use uuid::Uuid;

async fn run(command: &str) -> ToolResult {
    let context = ToolExecutionContext::new(Uuid::new_v4()).with_auto_approve(true);
    BashTool
        .execute(serde_json::json!({ "command": command }), &context)
        .await
        .expect("bash execute")
}

/// Run a command string through the real shell, exactly as the tool does,
/// returning the child's status and stdout.
#[cfg(not(windows))]
fn run_wrapped(command: &str) -> (Option<i32>, String) {
    let (program, flag) = shell_pair();
    let out = std::process::Command::new(program)
        .arg(flag)
        .arg(shell_command_text(command))
        .output()
        .unwrap_or_else(|e| panic!("spawn {program} {flag}: {e}"));
    (
        out.status.code(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
    )
}

// ─── #914: every body states its status ──────────────────────────────────

#[tokio::test]
async fn a_successful_call_with_no_output_states_exit_zero() {
    let result = run("true").await;
    assert!(result.success, "body was: {:?}", result.output);
    assert!(
        result.output.contains("(no output) · exit=0"),
        "a successful empty body must say so, got: {:?}",
        result.output
    );
    assert_eq!(
        result.metadata.get("exit_code"),
        Some(&"0".to_string()),
        "metadata must still carry the status"
    );
}

#[tokio::test]
async fn a_successful_call_with_output_ends_with_the_status() {
    let result = run("echo hello").await;
    assert!(result.success, "body was: {:?}", result.output);
    assert!(result.output.contains("hello"), "{:?}", result.output);
    assert!(
        result.output.trim_end().ends_with("exit=0"),
        "the body must end with the status line, got: {:?}",
        result.output
    );
}

#[tokio::test]
async fn a_search_that_matched_nothing_states_exit_one() {
    // A real file, so the assertion does not depend on the runner's cwd.
    let path = std::env::temp_dir().join(format!("oc-grep-{}", Uuid::new_v4()));
    std::fs::write(&path, "alpha\nbeta\n").expect("write fixture");
    let result = run(&format!("grep zzz {}", path.display())).await;
    std::fs::remove_file(&path).ok();

    assert!(
        result.success,
        "a no-match search is a successful empty search (#663), body: {:?}",
        result.output
    );
    assert!(
        result.output.contains("(no matches found) · exit=1"),
        "the body must say the search ran and returned 1, got: {:?}",
        result.output
    );
}

#[tokio::test]
async fn a_failure_still_reports_the_code_in_the_error_field() {
    let result = run("false").await;
    assert!(!result.success);
    assert_eq!(result.metadata.get("exit_code"), Some(&"1".to_string()));
    assert!(
        result.output.contains("exit=1"),
        "a failing body carries the status too, got: {:?}",
        result.output
    );
    assert!(
        result
            .error
            .as_deref()
            .is_some_and(|e| e.contains("Command exited with code 1")),
        "the error field keeps its old shape, got: {:?}",
        result.error
    );
}

// ─── #910: a pipeline reports the failing element, not the last one ──────

#[tokio::test]
#[cfg(not(windows))]
async fn a_failed_pipeline_element_is_reported_as_a_failure() {
    if !crate::utils::shell::shell_reports_pipestatus() {
        return; // sh fallback: nothing can report the list, see shell_command_text
    }
    let result = run("false | tail -1").await;
    assert!(
        !result.success,
        "a failed first element must not be reported as success, body: {:?}",
        result.output
    );
    assert_eq!(result.metadata.get("exit_code"), Some(&"1".to_string()));
    assert!(
        result.output.contains("exit=1 pipestatus=1 0"),
        "the body must quote the per-element statuses, got: {:?}",
        result.output
    );
}

#[tokio::test]
#[cfg(not(windows))]
async fn a_pipeline_trimmed_with_head_is_not_a_failure() {
    if !crate::utils::shell::shell_reports_pipestatus() {
        return;
    }
    // The control that keeps `pipefail` out of the design: the producer dies of
    // SIGPIPE because the consumer stopped reading, which is not a failure.
    let result = run("yes | head -1").await;
    assert!(
        result.success,
        "a SIGPIPE'd producer is not a failure, body: {:?}",
        result.output
    );
    assert_eq!(result.metadata.get("exit_code"), Some(&"0".to_string()));
    assert!(
        result.output.contains("pipestatus=141 0"),
        "the SIGPIPE is still visible, it just is not a failure: {:?}",
        result.output
    );
}

// ─── The policy, without a shell ─────────────────────────────────────────

#[test]
fn the_leftmost_failed_element_is_the_reported_status() {
    assert_eq!(effective_exit_status(&[1, 0], 0), 1);
    assert_eq!(effective_exit_status(&[2, 0], 0), 2);
    assert_eq!(effective_exit_status(&[0, 3], 3), 3);
    assert_eq!(effective_exit_status(&[0, 0], 0), 0);
    assert_eq!(
        effective_exit_status(&[], 7),
        7,
        "no list means the shell's own status stands"
    );
}

#[test]
fn a_sigpipe_producer_is_not_a_failure_but_a_real_failure_beside_it_is() {
    assert_eq!(effective_exit_status(&[141, 0], 0), 0);
    assert_eq!(effective_exit_status(&[141, 141, 0], 0), 0);
    assert_eq!(effective_exit_status(&[2, 141, 0], 2), 2);
    assert_eq!(effective_exit_status(&[141, 4], 4), 4);
}

#[test]
fn the_side_channel_parses_and_never_defaults_to_zero() {
    assert_eq!(parse_pipestatus("1 0 "), vec![1, 0]);
    assert_eq!(parse_pipestatus(""), Vec::<i32>::new());
    assert_eq!(
        parse_pipestatus("junk 3"),
        vec![3],
        "an unparseable token is dropped, never read as 0"
    );
}

#[test]
fn the_trailer_states_the_status_even_at_zero() {
    assert_eq!(status_trailer(0, &[]), "exit=0");
    assert_eq!(status_trailer(1, &[1]), "exit=1");
    assert_eq!(status_trailer(1, &[1, 0]), "exit=1 pipestatus=1 0");
}

#[test]
fn the_command_text_keeps_the_command_verbatim() {
    let text = shell_command_text("echo 'a b' | wc -l");
    assert!(
        text.starts_with("echo 'a b' | wc -l\n"),
        "the command must be passed through untouched (H-05), got: {text:?}"
    );
    if crate::utils::shell::shell_reports_pipestatus() {
        assert!(
            text.contains("PIPESTATUS"),
            "a bash host must get the status suffix, got: {text:?}"
        );
    }
}

// ─── The wrapper itself, through a real shell ────────────────────────────

#[cfg(not(windows))]
#[test]
fn the_wrapper_reports_a_failed_pipeline_and_tolerates_sigpipe() {
    if !crate::utils::shell::shell_reports_pipestatus() {
        return;
    }
    assert_eq!(
        run_wrapped("false | tail -1").0,
        Some(1),
        "a failed first element must reach the shell's exit status"
    );
    assert_eq!(
        run_wrapped("yes | head -1").0,
        Some(0),
        "a SIGPIPE'd producer must not"
    );
    assert_eq!(
        run_wrapped("exit 3").0,
        Some(3),
        "an explicit exit survives"
    );
}

#[cfg(not(windows))]
#[test]
fn the_wrapper_writes_the_per_element_statuses() {
    if !crate::utils::shell::shell_reports_pipestatus() {
        return;
    }
    let path = std::env::temp_dir().join(format!("oc-ps-{}", Uuid::new_v4()));
    let (program, flag) = shell_pair();
    let out = std::process::Command::new(program)
        .arg(flag)
        .arg(shell_command_text("false | tail -1"))
        .env(PIPESTATUS_ENV, &path)
        .output()
        .expect("spawn shell");
    let raw = std::fs::read_to_string(&path).expect("side channel written");
    std::fs::remove_file(&path).ok();

    assert_eq!(out.status.code(), Some(1));
    assert_eq!(parse_pipestatus(&raw), vec![1, 0]);
}

#[cfg(not(windows))]
#[test]
fn the_wrapper_survives_a_heredoc_and_a_trailing_comment() {
    if !crate::utils::shell::shell_reports_pipestatus() {
        return;
    }
    // The suffix is APPENDED, so it must not be swallowed by a heredoc body or
    // a `#` comment — the two shapes that would eat a naive `; echo $?`.
    let (code, stdout) = run_wrapped("cat <<'EOF'\nhi\nEOF");
    assert_eq!(code, Some(0));
    assert_eq!(stdout, "hi\n");

    let (code, stdout) = run_wrapped("echo hi # trailing comment");
    assert_eq!(code, Some(0));
    assert_eq!(stdout, "hi\n");
}
