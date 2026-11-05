//! #808: the detach signal the channel layer announces a run from.
//!
//! Both `bash` detach arms stamp `run_id` (#787), but a run id alone says a run
//! EXISTS — not that this call left the turn. The cue therefore keys on the
//! explicit `detach_cause`, and these tests pin that distinction: a result
//! carrying only `run_id` must announce nothing.

use crate::brain::tools::r#trait::{DetachCause, DetachedHandover, ToolResult};

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
    // that alone counted, every spawn would announce a handover it never made.
    let r = result_with(&[("run_id", "abc-123")]);
    assert_eq!(r.detached_handover(), None);
}

#[test]
fn handover_carries_its_cause_and_ceiling() {
    let r = result_with(&[
        ("run_id", "abc-123"),
        ("detach_cause", "handover"),
        ("detach_deadline_secs", "3600"),
    ]);
    assert_eq!(
        r.detached_handover(),
        Some(DetachedHandover {
            run_id: "abc-123".to_string(),
            cause: DetachCause::Handover,
            deadline_secs: Some(3600),
        })
    );
}

#[test]
fn spawn_is_distinguishable_from_handover() {
    // The two arms read differently to the user ("started detached" vs
    // "handed over"), so the cause must survive the round trip.
    let r = result_with(&[
        ("run_id", "abc-123"),
        ("detach_cause", "spawn"),
        ("detach_deadline_secs", "3600"),
    ]);
    assert_eq!(
        r.detached_handover().map(|h| h.cause),
        Some(DetachCause::Spawn)
    );
}

#[test]
fn zero_ceiling_means_no_deadline_not_zero() {
    // `0` is the operator's "no ceiling" choice, not a missing value: the cue
    // drops its `· deadline Ns` clause rather than printing `deadline 0s`.
    let r = result_with(&[
        ("run_id", "abc-123"),
        ("detach_cause", "handover"),
        ("detach_deadline_secs", "0"),
    ]);
    assert_eq!(
        r.detached_handover(),
        Some(DetachedHandover {
            run_id: "abc-123".to_string(),
            cause: DetachCause::Handover,
            deadline_secs: None,
        })
    );
}

#[test]
fn unknown_cause_announces_nothing() {
    // A typo or a future arm must fail closed: an unrecognised cause is not a
    // licence to guess, because a wrong cue is worse than a missing one.
    let r = result_with(&[("run_id", "abc-123"), ("detach_cause", "whatever")]);
    assert_eq!(r.detached_handover(), None);
}
