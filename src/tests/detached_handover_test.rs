//! #808: the detach signal the channel layer states a run from.
//!
//! Both `bash` detach arms stamp `run_id` (#787), but a run id alone says a run
//! EXISTS — not that this call left the turn. The signal therefore keys on the
//! explicit `detach_cause` key, and these tests pin the gate: a result carrying
//! only `run_id` must state nothing.
//!
//! #812: the PAYLOAD is the run id alone. #808 also carried the cause and the
//! run's lifetime ceiling, because the channel layer announced a detach in
//! words and the two arms read differently ("started detached" vs "handed
//! over"); #811 deleted those words, so both arms now render the same `⏏️` row
//! and nothing consumes either field. The two arms are still VALID inputs — the
//! distinction is simply not surfaced.

use crate::brain::tools::r#trait::{DetachedHandover, ToolResult};

fn result_with(pairs: &[(&str, &str)]) -> ToolResult {
    let mut r = ToolResult::success("ok".to_string());
    for (k, v) in pairs {
        r = r.with_metadata(k.to_string(), v.to_string());
    }
    r
}

#[test]
fn run_id_alone_is_not_a_detach_signal() {
    // The classifier-detach arm of `bash` writes `run_id` on every spawn. If
    // that alone counted, every spawn would state a detach it never made.
    let r = result_with(&[("run_id", "abc-123")]);
    assert_eq!(r.detached_handover(), None);
}

#[test]
fn a_handover_carries_its_run_id() {
    let r = result_with(&[("run_id", "abc-123"), ("detach_cause", "handover")]);
    assert_eq!(
        r.detached_handover(),
        Some(DetachedHandover {
            run_id: "abc-123".to_string(),
        })
    );
}

#[test]
fn both_detach_arms_yield_the_same_payload() {
    // #812 pins what #811 established: the row renders identically for both
    // causes, so the payload must too. If a future change reintroduces a
    // user-visible difference between spawn and handover, this is the test
    // that has to be rewritten deliberately rather than by accident.
    let handover = result_with(&[("run_id", "abc-123"), ("detach_cause", "handover")]);
    let spawn = result_with(&[("run_id", "abc-123"), ("detach_cause", "spawn")]);
    assert_eq!(handover.detached_handover(), spawn.detached_handover());
}

#[test]
fn the_lifetime_ceiling_is_not_part_of_the_payload() {
    // The ceiling's only reader was the `· deadline 10800s` clause of the
    // announcement sentence #811 deleted. A stale stamp must change nothing.
    let plain = result_with(&[("run_id", "abc-123"), ("detach_cause", "handover")]);
    let stamped = result_with(&[
        ("run_id", "abc-123"),
        ("detach_cause", "handover"),
        ("detach_deadline_secs", "3600"),
    ]);
    assert_eq!(plain.detached_handover(), stamped.detached_handover());
}

#[test]
fn unknown_cause_states_nothing() {
    // A typo or a future arm must fail closed: an unrecognised cause is not a
    // licence to guess, because a wrong cue is worse than a missing one.
    let r = result_with(&[("run_id", "abc-123"), ("detach_cause", "whatever")]);
    assert_eq!(r.detached_handover(), None);
}
