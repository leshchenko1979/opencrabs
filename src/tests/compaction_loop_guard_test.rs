//! #438 A3 — the loop guard: a run of compactions with no completed tool call
//! between them is a loop, and the harness brakes it instead of compacting
//! again.
//!
//! Pure — no agent, no mocks, no locks. The decision is a function of the
//! streak S1 already maintains, so all of it is testable without a service.
//! What is NOT covered here is the wiring inside `enforce_context_budget` (the
//! guard sitting after the 65% gate and before the first summariser attempt)
//! and the `LoopGuard` marker reaching the database; those are behavioural and
//! belong to a live smoke on the swapped binary.
//!
//! The boundary is the point. #226 was a session that compacted four times in
//! eight minutes and then went comatose for ~7 h, so braking one compaction
//! late is expensive — and so is braking one early: a session whose FIRST
//! compaction worked must never be braked, which is why the below-threshold
//! half is asserted value by value rather than inferred from the `>=`.

use crate::brain::agent::service::compaction::{
    compaction_loop_guard, CompactionOutcome, CompactionState, COMPACTION_LOOP_GUARD_STREAK,
};

#[test]
fn guard_does_not_fire_before_the_threshold() {
    // Every streak below the threshold is a session still getting a normal
    // compaction, the first one included.
    for streak in 0..COMPACTION_LOOP_GUARD_STREAK {
        assert!(
            !compaction_loop_guard(streak),
            "streak {streak} is below the threshold and must not be braked"
        );
    }
}

#[test]
fn guard_fires_at_the_threshold() {
    assert!(
        compaction_loop_guard(COMPACTION_LOOP_GUARD_STREAK),
        "the guard must break a run at exactly {COMPACTION_LOOP_GUARD_STREAK} compactions"
    );
}

#[test]
fn guard_stays_fired_above_the_threshold() {
    // `>=`, not `==`: a streak that keeps climbing is the #226 signature — the
    // floor is not coming down — and must keep the brake on.
    for streak in [COMPACTION_LOOP_GUARD_STREAK + 1, 10, 1_000] {
        assert!(
            compaction_loop_guard(streak),
            "streak {streak} is past the threshold and must stay braked"
        );
    }
}

#[test]
fn a_fresh_session_is_never_braked() {
    // A session with no `CompactionState` entry reads as the default, so this
    // is the value the guard actually sees on a brand-new session.
    let fresh = CompactionState::default();
    assert_eq!(fresh.compaction_streak, 0);
    assert!(!compaction_loop_guard(fresh.compaction_streak));
}

#[test]
fn threshold_is_two() {
    // Pinned, not tunable. Raising it tolerates a second compaction that
    // demonstrably bought no room; lowering it to 1 would brake the ordinary
    // first compaction on a full window. A silent retune should fail here
    // rather than pass quietly.
    assert_eq!(COMPACTION_LOOP_GUARD_STREAK, 2);
}

#[test]
fn loop_guard_marker_tells_the_truth() {
    // The marker is the only thing the model ever sees about the break, so it
    // must not borrow `Truncated`'s wording: no summariser ran, and nothing
    // was dropped by a summary that never happened.
    let marker = CompactionOutcome::LoopGuard.marker("");
    let truncated = CompactionOutcome::Truncated.marker("");
    assert!(marker.contains("SUSPENDED"));
    assert!(!marker.contains("every summariser attempt failed"));
    assert_ne!(marker, truncated);
    assert!(marker.starts_with("[CONTEXT COMPACTION SUSPENDED — The conversation"));
    // The trigger clause the other variants interpolate still works.
    assert!(CompactionOutcome::LoopGuard.marker(" (mid-loop)").contains("(mid-loop)"));
}
