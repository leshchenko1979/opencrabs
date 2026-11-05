//! Cue 1 of #808: the roll announces that a call's work LEFT the turn.
//!
//! The pre-fix tree said nothing at all. A `bash` call crossing its deadline
//! rendered exactly like one that finished: same class icon, no marker, no
//! counter, and the line never changed afterwards — because the completion
//! event the roll consumes arrives at the deadline, when the tool returns its
//! handle, not when the command ends. The user's only signal that work was
//! still running was a typing indicator the code itself calls "too easy to
//! miss".
//!
//! What these tests pin is the WORDING CONTRACT, which is the part a later
//! refactor can silently break while every cue still "appears":
//!
//! - a handover names the deadline that expired, because that number is the
//!   one the user can act on (raise `run_max_lifetime_secs`);
//! - a spawn is NOT a handover — no deadline ever applied, so claiming one
//!   expired would be a lie about a run that is behaving as designed;
//! - a `0` ceiling drops the clause rather than rendering `deadline 0s`,
//!   which would describe a run that cannot exist;
//! - the label is `short_label`, the same function the bash tool and
//!   `tasks_list` use, so one run has ONE name across three surfaces.
//!
//! The negative control (`detached: None` queues nothing) is the case that
//! matters most: every ordinary tool call in every turn takes it, so a cue
//! that leaked there would put a `⏏️` line under every single bash call.

use serde_json::json;

use crate::brain::tools::r#trait::{DetachCause, DetachedHandover};
use crate::channels::telegram::flow::{
    DisplayItem, FlowEntry, FlowLine, flow_lines, push_handover_cue,
};
use crate::tests::telegram_flow_telemetry_test::base_test_state;

fn handover(cause: DetachCause, deadline_secs: Option<u64>) -> DetachedHandover {
    DetachedHandover {
        run_id: "run-808-test".to_string(),
        cause,
        deadline_secs,
    }
}

/// The shared `StreamingState` fixture (#232) — a new field on that struct is
/// added in ONE place, never per-test.
fn fresh_state() -> crate::channels::telegram::flow::StreamingState {
    let mut s = base_test_state();
    s.display_queue.clear();
    s.flow_entries.clear();
    s
}

/// The number the user can act on. A handover exists BECAUSE a ceiling was
/// reached, so naming it is the whole point of the line.
#[test]
fn a_handover_names_the_deadline_that_expired() {
    let mut s = fresh_state();
    let text = push_handover_cue(
        &mut s,
        "bash",
        &json!({"command": "sleep 4000"}),
        Some(&handover(DetachCause::Handover, Some(3600))),
    )
    .expect("a handover must queue a line");

    assert_eq!(text, "⏏️ handed over: sleep 4000 · deadline 3600s");
    assert_eq!(s.display_queue.len(), 1);
}

/// `0` is the operator saying "no ceiling", not a missing value. Rendering
/// `deadline 0s` would describe a run that is killed instantly — the opposite
/// of what the setting means.
#[test]
fn a_zero_ceiling_drops_the_deadline_clause_instead_of_printing_zero() {
    let mut s = fresh_state();
    let text = push_handover_cue(
        &mut s,
        "bash",
        &json!({"command": "sleep 4000"}),
        Some(&handover(DetachCause::Handover, None)),
    )
    .expect("a handover with no ceiling still hands over");

    assert_eq!(text, "⏏️ handed over: sleep 4000");
    assert!(
        !text.contains("deadline"),
        "a `0` ceiling must not render a deadline clause, got: {text}"
    );
}

/// A spawn was classified long BEFORE it ran, so no deadline ever applied.
/// Calling it a handover would claim an expiry that never happened — and the
/// fix the user would reach for (raise the ceiling) does nothing for a spawn.
#[test]
fn a_spawn_is_not_reported_as_a_handover() {
    let mut s = fresh_state();
    let text = push_handover_cue(
        &mut s,
        "bash",
        &json!({"command": "sleep 4000"}),
        Some(&handover(DetachCause::Spawn, Some(3600))),
    )
    .expect("a spawn must queue a line");

    assert_eq!(text, "⏏️ started detached: sleep 4000");
    assert!(
        !text.contains("handed over"),
        "a spawn never had a deadline to cross, got: {text}"
    );
}

/// One run, one name: the roll must label a run the way `tasks_list` and the
/// bash tool's own text do, or the user has to match two strings by eye to
/// connect the announcement to the later card.
#[test]
fn the_label_is_the_shared_short_label() {
    let mut s = fresh_state();
    let text = push_handover_cue(
        &mut s,
        "bash",
        &json!({"command": "cd /root/opencrabs && python3 tools/audit.py --all"}),
        Some(&handover(DetachCause::Handover, Some(3600))),
    )
    .expect("a handover must queue a line");

    assert!(
        text.contains("python3 tools/audit.py --all"),
        "the command itself must be the label, got: {text}"
    );
    assert!(
        !text.contains("cd /root/opencrabs"),
        "the `cd` prefix is navigation, not the work, got: {text}"
    );
}

/// NEGATIVE CONTROL. Every ordinary tool call in every turn arrives with
/// `detached: None`; if this arm queued anything the roll would carry a
/// `⏏️` line under every bash call and the real handovers would be noise.
#[test]
fn an_ordinary_completion_queues_nothing() {
    let mut s = fresh_state();
    let text = push_handover_cue(&mut s, "bash", &json!({"command": "ls"}), None);

    assert!(text.is_none(), "an ordinary call must not announce a detach");
    assert!(
        s.display_queue.is_empty(),
        "an ordinary call must queue nothing, got {} queued item(s)",
        s.display_queue.len()
    );
}

/// The queued text survives into the rendered flow verbatim: cue 1 rides
/// `DisplayItem::System`, which the delivery leg folds into
/// `FlowEntry::System` unchanged (model pipelines — artifact stripping, rich
/// promotion — are skipped for chrome, #444).
#[test]
fn the_queued_line_reaches_the_flow_as_chrome() {
    let mut s = fresh_state();
    let text = push_handover_cue(
        &mut s,
        "bash",
        &json!({"command": "sleep 4000"}),
        Some(&handover(DetachCause::Handover, Some(3600))),
    )
    .expect("a handover must queue a line");

    match s.display_queue.first() {
        Some(DisplayItem::System(queued)) => assert_eq!(queued, &text),
        Some(_) => panic!("cue 1 must ride DisplayItem::System, not a tool/text item"),
        None => panic!("cue 1 queued nothing"),
    }

    // The fold the delivery leg performs, reproduced so the assertion below is
    // about the SURFACE and not just the queue.
    s.flow_entries.push(FlowEntry::System(text.clone()));
    let lines = flow_lines(&s);
    let last = lines.last().expect("the flow must carry the cue");
    match last {
        FlowLine::System(rendered) => assert_eq!(rendered, &text),
        FlowLine::Tool { .. } => panic!("cue 1 must render as chrome, not a tool row"),
        FlowLine::Text(_) => panic!("cue 1 must render as chrome, not as model narration"),
    }
}
