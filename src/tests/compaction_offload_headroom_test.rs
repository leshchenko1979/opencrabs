//! #1015 — skip the summariser only when tier-0 offload leaves comfortable headroom.

use crate::brain::agent::service::compaction::tier0_offload_resolves_compaction;
use crate::brain::agent::service::nudge::{PRESSURE_WARN_CEILING, PRESSURE_WARN_FLOOR};

#[test]
fn offload_to_fifty_percent_resolves_without_summariser() {
    assert!(tier0_offload_resolves_compaction(true, 50.0));
    assert!(tier0_offload_resolves_compaction(true, 49.9));
}

#[test]
fn offload_to_sixty_two_percent_still_needs_summariser() {
    assert!(!tier0_offload_resolves_compaction(true, 62.0));
    assert!(!tier0_offload_resolves_compaction(
        true,
        PRESSURE_WARN_FLOOR
    ));
    assert!(!tier0_offload_resolves_compaction(
        true,
        PRESSURE_WARN_CEILING
    ));
}

#[test]
fn still_above_sixty_five_after_offload_still_needs_summariser() {
    assert!(!tier0_offload_resolves_compaction(true, 66.0));
    assert!(!tier0_offload_resolves_compaction(true, 70.0));
}

#[test]
fn never_crossed_trigger_at_sixty_four_is_resolved() {
    assert!(tier0_offload_resolves_compaction(false, 64.0));
    assert!(tier0_offload_resolves_compaction(
        false,
        PRESSURE_WARN_FLOOR
    ));
}

/// #1015 wiring: the predicates above are pure, and pure predicates cannot
/// prove that production ever asks them. A call site that was deleted — or that
/// consults the predicate and then falls through to the summariser anyway — is
/// indistinguishable from the correct one as far as every test above can tell.
///
/// Pinned as ORDER in the squeezed source, the way the `#934`/`#1686` pins in
/// `compaction_summary_budget_test.rs` and `compaction_notice_test.rs` do it:
/// `include_str!` the source and assert on markers, never on a live run.
/// Squeezing is what keeps the anchors alive across rustfmt breaking a call
/// across lines — a literal needle then reports "not called" for a call site
/// that is plainly there.
#[test]
fn the_offload_predicate_gates_the_summariser() {
    const COMPACTION: &str = include_str!("../brain/agent/service/compaction.rs");

    fn squeeze(s: &str) -> String {
        s.chars().filter(|c| !c.is_whitespace()).collect()
    }
    let src = squeeze(COMPACTION);

    // Identifiers, not literals: a call site passing `true, 62.0` would satisfy
    // every assertion above while gating nothing about this visit.
    let gate = src
        .find("iftier0_offload_resolves_compaction(crossed_soft_trigger,usage_pct)")
        .expect("the sync path must consult the tier-0 offload predicate (#1015)");
    let announce = src
        .find("gate_warn_message(usage_pct)")
        .expect("the gate announcement must still exist (#1686)");
    let spawn = src
        .find("self.spawn_background_compaction(")
        .expect("the background summariser spawn must still exist");

    assert!(
        gate < announce,
        "the predicate must be decided BEFORE the visit announces a compaction — \
         below it, a resolved offload still tells the user a summariser is coming (#1015)"
    );
    assert!(
        gate < spawn,
        "the predicate must be decided BEFORE a summariser is spawned — below it, \
         the decision is made after the thing it exists to prevent (#1015)"
    );

    // And the resolved branch must LEAVE the visit. Anchor on the statement it
    // actually executes, then bound it above by the next decision point in the
    // visit (the #438 loop guard): a bare `contains("return")` would be satisfied
    // by a return in some later branch while this one fell through, and an
    // unbounded `find` would happily find that later one.
    let skip = "returntruncated.then_some(CompactionOutcome::Truncated);";
    let after_gate: &str = &src[gate..];
    let skip_at = after_gate.find(skip).map(|at| gate + at).expect(
        "the resolved branch of the offload gate must return `truncated.then_some(..)` \
         — a predicate that is asked and then ignored skips no summariser (#1015)",
    );
    let loop_guard = src
        .find("ifcompaction_loop_guard(")
        .expect("the loop guard must still sit below the offload gate (#438)");
    assert!(
        skip_at < loop_guard,
        "the resolved branch must exit the visit before it reaches the loop guard — \
         otherwise the gate resolved nothing and simply carried on (#438, #1015)"
    );
    assert!(
        skip_at < spawn,
        "the resolved branch must exit before any summariser is spawned (#1015)"
    );

    // Finally, the predicate has to judge the POST-offload fill: tier-0 offload
    // runs first and rewrites `usage_pct` from the shrunken window. Asked about
    // the pre-offload number it can never resolve, and the summariser runs on a
    // context the offload already fixed.
    //
    // Offsets come from `find` on `src` itself, so slicing between two of them
    // cannot land inside a multi-byte character — the em dashes in the comments
    // between the anchors are safe to cut across.
    let before_gate: &str = &src[..gate];
    let offload = before_gate
        .rfind("offload_stale_tool_outputs_gated(")
        .expect("the sync path must run the tier-0 offload before the predicate (#1003)");
    let offload_to_gate: &str = &src[offload..gate];
    assert!(
        offload_to_gate.contains("usage_pct="),
        "the fill the predicate reads must be recomputed after tier-0 offload (#1003)"
    );
}
