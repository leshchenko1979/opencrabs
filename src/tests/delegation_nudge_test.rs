//! The mid-turn delegation nudge (#920).
//!
//! Rule 26 tells a session to spawn a child once a turn passes ten tool calls,
//! but it is a PREDICTIVE rule: it asks the model to estimate work it has not
//! started. Measured over the 24h after the rule was last rewritten, 117 turns
//! of 10-14 tool calls produced ZERO spawns and 221 turns of 15-29 produced
//! four — compliance collapses exactly at the band the rule names. A session
//! cannot count its own tool calls, so the harness says it instead.
//!
//! These pin the decision and the wording, because the whole failure mode of
//! this mechanism is being either silent when it should fire or noisy when it
//! should not. Fixtures are synthetic and carry no user identifiers.

use crate::brain::agent::service::nudge::{
    DELEGATION_NUDGE_AGAIN_AT, DELEGATION_NUDGE_AT, delegation_nudge, delegation_nudge_tier,
};

/// The wiring site, pinned as source so a refactor that deletes the injection
/// cannot leave this suite green.
const TOOL_LOOP_SRC: &str = include_str!("../brain/agent/service/tool_loop.rs");

#[test]
fn the_threshold_is_the_one_rule_26_names() {
    // The harness and the written law must not drift apart: if rule 26's `>= 10`
    // changes, this constant has to move with it or the nudge fires at a count
    // no rule mentions.
    assert_eq!(DELEGATION_NUDGE_AT, 10);
    // A `const` block, because both sides are constants: the ordering invariant
    // is then a BUILD failure rather than a test failure, so it cannot be
    // missed by anyone who runs the suite selectively. The message carries no
    // interpolation because formatted panics are not const-evaluable.
    const {
        assert!(
            DELEGATION_NUDGE_AGAIN_AT > DELEGATION_NUDGE_AT,
            "the firm tier must sit above the threshold tier"
        )
    };
}

#[test]
fn a_turn_below_the_threshold_is_not_nudged() {
    assert_eq!(delegation_nudge_tier(0, false, 0), 0);
    assert_eq!(delegation_nudge_tier(9, false, 0), 0);
}

#[test]
fn the_threshold_nudge_fires_once_at_ten_calls() {
    assert_eq!(delegation_nudge_tier(10, false, 0), 1);
    // Already fired: the same turn must not be told again on iteration 11.
    assert_eq!(delegation_nudge_tier(11, false, 1), 1);
    assert_eq!(delegation_nudge_tier(24, false, 1), 1);
}

#[test]
fn the_firm_nudge_escalates_once() {
    assert_eq!(delegation_nudge_tier(25, false, 1), 2);
    // And never repeats after that, however long the turn runs.
    assert_eq!(delegation_nudge_tier(26, false, 2), 2);
    assert_eq!(delegation_nudge_tier(400, false, 2), 2);
}

#[test]
fn a_turn_that_already_spawned_is_never_nudged() {
    // The advice has been taken. Repeating it on top of running work is noise,
    // which is the one thing a mid-turn correction cannot afford to be.
    assert_eq!(delegation_nudge_tier(40, true, 0), 0);
    // A spawn AFTER the threshold nudge also silences the firm tier.
    assert_eq!(delegation_nudge_tier(40, true, 1), 1);
}

#[test]
fn the_threshold_nudge_names_the_count_and_keeps_inline_legitimate() {
    let nudge = delegation_nudge(10, 1);
    assert!(
        nudge.contains("10 tool calls"),
        "the nudge must name the count the model cannot see: {nudge}"
    );
    assert!(
        nudge.contains("spawn_agent"),
        "the nudge must name the tool, not gesture at delegation: {nudge}"
    );
    // Without this clause the nudge becomes a blanket order to delegate, which
    // is the opposite failure: children spawned for one-line edits.
    assert!(
        nudge.contains("Stay inline when the next step genuinely needs"),
        "the inline case must stay legitimate: {nudge}"
    );
    assert!(
        nudge.contains("do not spawn for a single command or one file"),
        "the nudge must carry rule 26's own negative bound: {nudge}"
    );
}

#[test]
fn the_firm_nudge_is_distinguishable_and_also_names_the_count() {
    let soft = delegation_nudge(10, 1);
    let firm = delegation_nudge(30, 2);
    assert_ne!(soft, firm, "the two tiers must not read identically");
    assert!(
        firm.contains("30 tool calls"),
        "the firm nudge must name the real count: {firm}"
    );
    assert!(
        firm.contains("spawn_agent"),
        "the firm nudge must still name the tool: {firm}"
    );
    assert!(
        firm.contains("background"),
        "the firm nudge must say the child runs in the background: {firm}"
    );
}

#[test]
fn both_tiers_are_framed_as_system_messages() {
    // Matches every other mid-turn correction in this module: an unmarked
    // instruction reads as the user speaking, which is a different authority.
    for tier in [1u8, 2u8] {
        let nudge = delegation_nudge(12, tier);
        assert!(
            nudge.starts_with("[System:"),
            "tier {tier} must be framed as a system message: {nudge}"
        );
        assert!(nudge.ends_with(']'), "tier {tier} must be closed: {nudge}");
    }
}

#[test]
fn the_nudge_is_wired_into_the_turn_loop() {
    // Every test above exercises a pure function, so none of them fails if the
    // injection is deleted from `tool_loop.rs` — the mechanism would be dead
    // behind a green suite, which is the shape of a gate that cannot see the
    // failure class it is aimed at. So pin the wiring itself, one assertion per
    // guard, so removing any single one of them fails this test.
    assert!(
        TOOL_LOOP_SRC.contains("let mut spawned_this_turn = false;"),
        "the spawn flag must be declared at turn scope, not per-iteration"
    );
    assert!(
        TOOL_LOOP_SRC.contains("let mut delegation_nudge_tier_fired: u8 = 0;"),
        "the tier budget must be declared at turn scope so each tier fires once"
    );
    assert!(
        TOOL_LOOP_SRC.contains("name == \"spawn_agent\""),
        "a spawn in the batch must set the flag BEFORE dispatch, because the \
         parallel path consumes `tool_uses`"
    );
    assert!(
        TOOL_LOOP_SRC.contains("super::nudge::delegation_nudge_tier("),
        "the loop must call the tier decision rather than re-deriving it inline"
    );
    assert!(
        TOOL_LOOP_SRC.contains("super::nudge::delegation_nudge("),
        "the loop must inject the nudge text"
    );
    assert!(
        TOOL_LOOP_SRC.contains("self.tool_registry.has_tool(\"spawn_agent\")"),
        "a registry without spawn_agent must not be told to call a tool it cannot see"
    );
    assert!(
        TOOL_LOOP_SRC.contains("m.find_by_session(session_id).is_some()"),
        "a session that is itself a sub-agent must not be nudged to spawn"
    );
}
