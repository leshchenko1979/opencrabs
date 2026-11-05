//! Cue 2 of #808: a handed-over call's row says its work is still running.
//!
//! Cue 1 announces the handover once, in the roll. This is the other half: the
//! tool row itself must stop reading as finished while its command keeps
//! going. Before the fix the row was indistinguishable from a completed call —
//! same class icon, no marker — because the completion event the roll consumes
//! arrives at the DEADLINE (when the tool hands back its handle), not at the
//! command's end. So `completed` was already `Some(true)` with work still live.
//!
//! What these tests pin:
//!
//! - the marker rides the LABEL, so it survives both the live renderer and the
//!   settled one;
//! - a row with no handover renders byte-identically to the pre-fix tree — the
//!   negative control that matters most, since every ordinary tool call takes
//!   that path;
//! - clearing is by LIVENESS: a run that ends while the daemon is down folds no
//!   notification at all, so a push-driven clear would leave the marker on
//!   forever. `prune_handed_over` asks the manager which ids are live instead,
//!   and that one rule settles both the pushed and the silent case.

use crate::channels::telegram::flow::{
    DisplayItem, FlowEntry, FlowLine, ToolMsg, flow_lines, prune_handed_over,
};
use crate::tests::telegram_flow_telemetry_test::base_test_state;

const MARKER: &str = "\u{23cf}\u{fe0f}";

/// A state holding exactly one bash row, optionally mid-handover.
fn state_with_row(handed_over: Option<&str>) -> crate::channels::telegram::flow::StreamingState {
    let mut s = base_test_state();
    s.tool_msgs.clear();
    s.display_queue.clear();
    s.flow_entries.clear();
    s.tool_msgs.push(ToolMsg {
        msg_id: None,
        name: "bash".to_string(),
        context: " (`sleep 4000`)".to_string(),
        raw_context: "sleep 4000".to_string(),
        completed: Some(true),
        handed_over: handed_over.map(|id| id.to_string()),
        dirty: true,
    });
    s.display_queue.push(DisplayItem::NewTool(0));
    s.flow_entries.push(FlowEntry::Tool(0));
    s
}

fn label_of(s: &crate::channels::telegram::flow::StreamingState) -> String {
    match flow_lines(s).first() {
        Some(FlowLine::Tool { label, .. }) => label.clone(),
        _ => panic!("expected a Tool line"),
    }
}

/// The whole point: the row carries the marker while the run is live, and it
/// rides the label so both renderers see it.
#[test]
fn a_live_handover_marks_the_row() {
    let s = state_with_row(Some("run-808-a"));
    let label = label_of(&s);
    assert!(
        label.ends_with(MARKER),
        "row for a live handover must end with the marker, got {label:?}"
    );
}

/// Negative control. Every ordinary tool call in every turn lands here, so a
/// marker that leaked would decorate the whole roll.
#[test]
fn an_ordinary_row_carries_no_marker() {
    let s = state_with_row(None);
    let label = label_of(&s);
    assert!(
        !label.contains(MARKER),
        "an ordinary completed call must carry no marker, got {label:?}"
    );
}

/// Liveness clearing. A finished run that pushed nothing (daemon down, cue 11)
/// must not keep its marker forever — the manager is the authority, not a push.
#[test]
fn a_run_that_is_no_longer_live_loses_its_marker() {
    let mut s = state_with_row(Some("run-808-a"));
    assert!(label_of(&s).ends_with(MARKER));

    let cleared = prune_handed_over(&mut s, &[]);
    assert_eq!(cleared, 1, "one row should have been cleared");

    let label = label_of(&s);
    assert!(
        !label.contains(MARKER),
        "marker must be gone once the run is not live, got {label:?}"
    );
}

/// The other side of liveness: while the id IS live the marker stays, and the
/// prune reports no change so a tick can skip a re-render.
#[test]
fn a_live_run_keeps_its_marker() {
    let mut s = state_with_row(Some("run-808-a"));
    let cleared = prune_handed_over(&mut s, &["run-808-a".to_string()]);
    assert_eq!(cleared, 0, "a live run must not be cleared");
    assert!(label_of(&s).ends_with(MARKER));
}

/// Pruning is id-scoped: another run finishing must not clear this row.
#[test]
fn pruning_clears_only_the_matching_run() {
    let mut s = state_with_row(Some("run-808-a"));
    let cleared = prune_handed_over(&mut s, &["run-808-b".to_string()]);
    assert_eq!(cleared, 1, "an unrelated live id must not protect the row");
    assert!(!label_of(&s).contains(MARKER));
}
