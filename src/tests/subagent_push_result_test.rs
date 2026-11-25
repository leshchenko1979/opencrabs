//! A finished sub-agent reports to the session that spawned it (#1036).
//!
//! The push exists for the fire-and-forget case the tool description
//! advertises: the parent turn ends, the child finishes later, and without a
//! push its output would sit in the manager map with nobody to read it. A
//! parent already blocked on `wait_agent` receives the output as that tool's
//! result, so pushing as well would deliver it twice.

use crate::brain::tools::subagent::manager::{SubAgent, SubAgentManager};
use crate::brain::tools::subagent::spawn::completion_message;
use uuid::Uuid;

fn manager_with_agent(id: &str) -> SubAgentManager {
    let manager = SubAgentManager::new();
    manager.insert(SubAgent {
        input_tx: None,
        ..SubAgent::new(
            id.to_string(),
            "worker".to_string(),
            Uuid::new_v4(),
            Uuid::new_v4(),
        )
    });
    manager
}

#[test]
fn an_unwatched_agent_pushes_its_result() {
    let manager = manager_with_agent("a1");
    assert!(
        manager.mark_completed("a1", "done".to_string()),
        "nobody is waiting, so nothing else would ever carry the output"
    );
}

#[test]
fn an_agent_someone_waits_on_does_not_push() {
    let manager = manager_with_agent("a2");
    assert!(manager.enter_wait("a2"));

    assert!(
        !manager.mark_completed("a2", "done".to_string()),
        "wait_agent returns this output as its own result"
    );
}

#[test]
fn the_push_resumes_once_the_waiter_leaves() {
    // A wait that times out releases its registration, so a later completion
    // is unwatched again and must push.
    let manager = manager_with_agent("a3");
    manager.enter_wait("a3");
    manager.leave_wait("a3");

    assert!(manager.mark_completed("a3", "done".to_string()));
}

#[test]
fn concurrent_waiters_each_hold_the_push_back() {
    let manager = manager_with_agent("a4");
    manager.enter_wait("a4");
    manager.enter_wait("a4");
    manager.leave_wait("a4");

    assert!(
        !manager.mark_completed("a4", "done".to_string()),
        "one waiter remains"
    );
}

#[test]
fn a_failure_nobody_waits_on_still_pushes() {
    // A failure is a result too: the parent has to know not to wait on work
    // that will never arrive.
    let manager = manager_with_agent("a5");
    assert!(manager.mark_failed("a5", "boom".to_string()));
}

#[test]
fn a_failure_someone_waits_on_does_not_push() {
    let manager = manager_with_agent("a6");
    manager.enter_wait("a6");

    assert!(!manager.mark_failed("a6", "boom".to_string()));
}

#[test]
fn waiting_on_an_unknown_agent_reports_no_registration() {
    // The guard must not leave a decrement outstanding for an agent that was
    // never there, which would corrupt another agent's count.
    let manager = SubAgentManager::new();
    assert!(!manager.enter_wait("ghost"));
}

#[test]
fn leaving_a_wait_never_underflows() {
    let manager = manager_with_agent("a7");
    manager.leave_wait("a7");
    manager.leave_wait("a7");

    assert!(
        manager.mark_completed("a7", "done".to_string()),
        "count floors at zero rather than wrapping"
    );
}

#[test]
fn a_completion_message_carries_the_output_and_forbids_a_respawn() {
    let msg = completion_message("build docs", "abc123", Ok("42 pages written"));

    assert!(msg.context_text.contains("42 pages written"));
    assert!(msg.context_text.contains("completed"));
    assert!(
        msg.context_text.contains("Do not re-spawn"),
        "the message IS the result; re-running would duplicate the work"
    );
    assert!(msg.display_text.contains("build docs"));
}

#[test]
fn a_failure_message_says_so_rather_than_implying_success() {
    let msg = completion_message("deploy", "def456", Err("connection refused"));

    assert!(msg.context_text.contains("connection refused"));
    assert!(msg.context_text.contains("failed"));
    assert!(
        msg.context_text
            .contains("Do not assume the work was completed"),
        "an absent result must not read as success"
    );
    assert!(msg.display_text.contains("deploy"));
}

#[test]
fn a_long_output_keeps_its_tail() {
    // The conclusion matters more than the opening, matching how the detached
    // command path truncates.
    let long: String = std::iter::repeat_n('x', 5000)
        .chain("THE-CONCLUSION".chars())
        .collect();
    let msg = completion_message("big", "ghi789", Ok(&long));

    assert!(msg.context_text.contains("THE-CONCLUSION"));
    assert!(msg.context_text.contains("truncated"));
    assert!(msg.context_text.len() < long.len());
}

#[test]
fn a_long_output_hint_names_the_persisted_file_not_the_ram_tool() {
    // #147: the hint must point at the durable status file (field
    // output_full), not at wait_agent — which reads daemon RAM and dies at
    // every restart/swap. The path must be real (status_path), not a bare id.
    let long: String = std::iter::repeat_n('x', 5000)
        .chain("THE-CONCLUSION".chars())
        .collect();
    let msg = completion_message("big", "hint-id-1", Ok(&long));

    assert!(
        !msg.context_text.contains("wait_agent"),
        "hint must no longer claim the RAM-resident tool holds the report"
    );
    assert!(msg.context_text.contains("output_full"));
    let expected_path = crate::brain::agent::service::work_status::status_path("hint-id-1");
    assert!(
        msg.context_text
            .contains(&expected_path.display().to_string()),
        "hint carries the real status-file path: {expected_path:?}"
    );
}

// ── #1024: the durable history record ────────────────────────────────────────

/// A finished sub-agent's report must be findable in the transcript AFTER a
/// compaction, not only for the one turn the model sees it.
///
/// The bug this pins: `completion_message` builds two strings and history kept
/// only the tag, so a 34k review persisted as `🤖 sub-agent finished: x` and the
/// parent — rebuilding its view from the DB after compacting — reported the
/// work as empty while the full report sat intact on disk.
#[test]
fn a_completion_persists_a_record_richer_than_its_tag() {
    let report = format!("HEADLINE-FINDING {}", "m".repeat(30_000));
    let msg = completion_message("review-buyer", "abc123", Ok(&report));

    let history = msg
        .history_text
        .as_deref()
        .expect("a completion carries a durable record");

    // The tag still names the child, so a reader knows what finished.
    assert!(history.contains("🤖 sub-agent finished: review-buyer"));
    assert!(history.contains("abc123"), "names the agent id");
    assert!(history.contains("status: completed"));

    // The pointer is real and its expiry is stated, not implied.
    let path = crate::brain::agent::service::work_status::status_path("abc123");
    assert!(history.contains(&path.display().to_string()));
    assert!(history.contains("output_full"));
    assert!(
        history.contains("retained") || history.contains("kept until removed"),
        "a pointer that expires in silence is the same lie one layer down"
    );

    // Head AND tail: `truncate_output` keeps only the last 4,000 chars, so the
    // opening — the framing and the headline — is exactly what used to be lost.
    assert!(
        history.contains("HEADLINE-FINDING"),
        "the report's opening must survive: {history:.200}"
    );
    assert!(history.contains("mmmm"), "the tail survives too");
    assert!(history.len() < 6_000, "the record stays bounded");
}

/// A short report is carried whole, with no truncation marker: a
/// `…(truncated)` note on an untruncated text is its own small lie.
#[test]
fn a_short_report_is_carried_whole() {
    let msg = completion_message("quick", "id-short", Ok("all good"));
    let history = msg.history_text.as_deref().expect("record");
    assert!(history.contains("all good"));
    assert!(!history.contains("truncated"));
}

/// A failure's reason is durable too — a later turn needs it to avoid
/// re-running the same doomed work.
#[test]
fn a_failure_persists_its_reason() {
    let msg = completion_message("doomed", "id-fail", Err("provider returned 503"));
    let history = msg.history_text.as_deref().expect("record");
    assert!(history.contains("🤖 sub-agent failed: doomed"));
    assert!(history.contains("status: failed"));
    assert!(history.contains("provider returned 503"));
}

/// The excerpt bound itself, including the two degenerate inputs.
#[test]
fn history_excerpt_bounds_without_panicking() {
    use crate::brain::tools::subagent::spawn::history_excerpt;

    assert_eq!(history_excerpt(""), "");
    assert_eq!(history_excerpt("tiny"), "tiny");

    let long = "a".repeat(30_000);
    let out = history_excerpt(&long);
    assert!(out.contains("truncated"));
    assert!(out.len() < 4_600, "bounded, got {}", out.len());

    // Char-safe: a byte slice at a codepoint boundary would panic here, and
    // the reports this carries are routinely Cyrillic.
    let cyrillic: String = "проверка ".repeat(3_000);
    let out = history_excerpt(&cyrillic);
    assert!(out.contains("проверка"));
    assert!(out.len() < 9_000);
}

/// A plain message must still persist exactly its own text — the #722
/// guarantee, unbroken by the new slot.
#[test]
fn a_plain_message_persists_its_own_text() {
    let msg = crate::brain::agent::QueuedUserMessage::plain("hello".to_string());
    assert!(msg.history_text.is_none());
    assert_eq!(msg.history_or_display(), "hello");
}

/// A folded batch keeps every member's record instead of dropping to tags.
#[test]
fn a_joined_batch_keeps_each_record() {
    let a = completion_message("one", "id-one", Ok("first report body"));
    let b = completion_message("two", "id-two", Ok("second report body"));

    let joined = crate::brain::agent::QueuedUserMessage::join(&[a, b]).expect("joined");
    let history = joined
        .history_text
        .as_deref()
        .expect("records survive folding");
    assert!(history.contains("first report body"));
    assert!(history.contains("second report body"));
}
