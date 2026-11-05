//! #811: a detached run is stated by ONE icon on its own row.
//!
//! #808 shipped this twice over and the owner rejected both halves on sight:
//! the row read `💻 <label> ⏏️` (class icon plus a trailing marker) and a
//! sentence was queued under it (`⏏️ handed over: … · deadline 10800s`).
//! "We don't need 'handed over' or 'starting detached' — just put the ⏏ icon
//! instead of the ⌨️ icon. Also just ⏏ instead of ⌨️⏏."
//!
//! So the contract is three lines of it:
//!
//! - a row whose run is out of the turn renders `⏏️` IN PLACE of its class
//!   icon — never beside it;
//! - the roll gains NO line for the detach — the row count is the count of
//!   calls, not calls plus announcements;
//! - the icon reverts when the liveness prune clears the flag, which is the
//!   same rule that used to clear the marker (a finished run is not live).
//!
//! The negative control is the second test: every ordinary call in every turn
//! carries `handed_over: None`, and it must keep its class icon. A swap that
//! leaked onto ordinary rows would turn the whole roll into ⏏️.

use crate::channels::telegram::flow::{FlowLine, flow_lines, prune_handed_over};
use crate::tests::telegram_flow_telemetry_test::base_test_state;

/// One detached `bash` call: the class icon would be `💻`.
fn detached_state() -> crate::channels::telegram::flow::StreamingState {
    let mut s = base_test_state();
    s.tool_msgs[0].name = "bash".to_string();
    s.tool_msgs[0].context = "sleep 4000".to_string();
    s.tool_msgs[0].raw_context = "sleep 4000".to_string();
    s.tool_msgs[0].completed = Some(true);
    s.tool_msgs[0].handed_over = Some("run-811".to_string());
    s.flow_entries = vec![crate::channels::telegram::flow::FlowEntry::Tool(0)];
    s.display_queue.clear();
    s
}

fn label_of(lines: &[FlowLine]) -> String {
    match lines.first() {
        Some(FlowLine::Tool { label, .. }) => label.clone(),
        Some(FlowLine::System(t)) => panic!("the detach must not add a line, got: {t}"),
        Some(FlowLine::Text(t)) => panic!("the detach must not add narration, got: {t}"),
        None => panic!("the flow lost its only row"),
    }
}

/// The whole ask in one assertion: the icon REPLACES the class icon.
#[test]
fn a_detached_row_renders_the_detached_icon_instead_of_the_class_icon() {
    let s = detached_state();
    let label = label_of(&flow_lines(&s));

    assert_eq!(label, "\u{23cf}\u{fe0f}", "expected ⏏️ alone as the label");
    assert!(
        !label.contains('\u{1f4bb}'),
        "the 💻 class icon must be REPLACED, not decorated: {label}"
    );
}

/// No announcement line. #808 queued one per detach, so a turn with three
/// detached calls rendered six lines; the row count must stay the call count.
#[test]
fn a_detach_adds_no_line_to_the_roll() {
    let s = detached_state();
    let lines = flow_lines(&s);

    assert_eq!(
        lines.len(),
        1,
        "one call, one line — got {} line(s)",
        lines.len()
    );
}

/// The old marker cleared by liveness, and the icon keeps that rule: a run
/// that is no longer live gets its class icon back, with no second mechanism.
#[test]
fn the_class_icon_returns_once_the_run_is_no_longer_live() {
    let mut s = detached_state();

    // Live: still detached.
    assert_eq!(prune_handed_over(&mut s, &["run-811".to_string()]), 0);
    assert_eq!(label_of(&flow_lines(&s)), "\u{23cf}\u{fe0f}");

    // Reaped: the manager reports nothing live.
    assert_eq!(prune_handed_over(&mut s, &[]), 1);
    assert_eq!(label_of(&flow_lines(&s)), "\u{1f4bb}");
}

/// NEGATIVE CONTROL — the case every ordinary call takes.
#[test]
fn an_ordinary_row_keeps_its_class_icon() {
    let mut s = base_test_state();
    s.flow_entries = vec![crate::channels::telegram::flow::FlowEntry::Tool(0)];

    // `read_file` with a context renders the bare class icon.
    assert_eq!(label_of(&flow_lines(&s)), "\u{1f4c4}");
}
