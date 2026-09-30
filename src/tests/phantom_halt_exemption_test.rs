//! Regression coverage for the #541 phantom-kill exemption.
//!
//! Incident: after `suggest_options` surfaced a question, the turn issued
//! one more text-only request so the model could sign off — and the phantom
//! guard killed that sign-off, stripping it from the screen
//! (`StripStreamedContent { bytes: usize::MAX }`). The user's options bubble
//! was left with no closing text.
//!
//! Mechanism: the halt is a TURN-STRUCTURE fact (`option_surface_halt_seen`
//! is set, then the tool loop `break`s), while `phantom_eligible` is decided
//! from the iteration's PROSE — and one arm of that disjunction,
//! `has_forward_intent_post_success`, matches a sign-off that narrates what
//! happens next ("pick A or B"). So a turn with thirty completed tool calls
//! still read as phantom-eligible.
//!
//! #31 wired the flag into a log line, but that read sits inside
//! `if !phantom_eligible` while the kill predicate requires
//! `phantom_eligible` — mutually exclusive, so the exemption could never fire
//! on the one iteration it was written to describe.
//!
//! These are source sentinels, not behavioural drives: like
//! `phantom_persist_budget_test` and `thinking_loop_fallback_test`, they pin
//! control flow of the 8k-line async fn on its source, because reproducing
//! the incident live needs a provider that narrates on demand. The
//! behavioural half is the Phase 6b smoke — driving a real
//! `suggest_options` halt on the running binary and observing the sign-off
//! survive to the user.
//!
//! Measured before the fix (5 days, 09-24 → 09-28): 218 halts, 115 kills,
//! 30 halts killed within 120 s (14 %), `intent_no_tools` alone in 79 of
//! them — i.e. most kills fired on prose shape with no fabricated claim.

const TOOL_LOOP_SRC: &str = include_str!("../brain/agent/service/tool_loop.rs");

/// The `let kill = …;` region, extracted exactly as
/// `phantom_structured_report_test::the_kill_still_requires_a_fired_branch_and_budget`
/// extracts it. Both tests must agree on the anchor or one of them is
/// reading a different site than it thinks.
fn kill_region() -> String {
    TOOL_LOOP_SRC
        .split("let kill =")
        .nth(1)
        .expect("the kill gate must exist")
        .split(';')
        .next()
        .unwrap()
        .to_string()
}

#[test]
fn kill_region_is_unique_in_the_loop() {
    // The sentinels anchor on `let kill =`. A second occurrence would make
    // every one of them read the wrong site while still passing.
    assert_eq!(
        TOOL_LOOP_SRC.matches("let kill =").count(),
        1,
        "exactly one kill predicate expected — a second would mis-anchor the sentinels"
    );
}

#[test]
fn kill_region_carries_the_halt_exemption() {
    // The #541 arm itself. It must be NEGATED: a positive
    // `option_surface_halt_seen` would invert the fix and kill only the
    // post-halt sign-off, which is the opposite of the intent.
    let region = kill_region();
    assert!(
        region.contains("!option_surface_halt_seen"),
        "the kill must be suppressed after a suggest_options halt (#541)"
    );
    assert!(
        !region.contains("&& option_surface_halt_seen"),
        "the halt arm must be negated — a positive conjunct inverts #541"
    );
}

#[test]
fn halt_flag_is_read_at_the_kill_not_only_at_the_log_line() {
    // The #541 defect was not a missing flag: it was a flag read only where
    // it could not act. The prior read lives inside `if !phantom_eligible`
    // and the kill requires `phantom_eligible`, so they are mutually
    // exclusive. Pin that a read now exists on the kill side, which is the
    // side that discards the iteration.
    let region = kill_region();
    assert!(
        region.contains("option_surface_halt_seen"),
        "the kill predicate must consult the halt flag — the log-line read alone \
         could never fire on the iteration it was written for (#541)"
    );
    let kill_pos = TOOL_LOOP_SRC
        .find("let kill =")
        .expect("kill predicate must exist");
    let log_read = TOOL_LOOP_SRC
        .find("if option_surface_halt_seen {")
        .expect("the #31 log-line read must still exist");
    assert_ne!(
        kill_pos, log_read,
        "the #31 log-line read and the #541 kill read must be distinct sites"
    );
}

#[test]
fn halt_flag_is_declared_before_the_iteration_loop_opens() {
    assert!(
        TOOL_LOOP_SRC.contains("let mut option_surface_halt_seen = false;"),
        "the flag must be declared and false-initialised at turn scope (#31)"
    );
    let decl = TOOL_LOOP_SRC
        .find("let mut option_surface_halt_seen = false;")
        .expect("declaration must exist");
    // Anchor on the FIRST statement inside the iteration loop, not on some
    // other prologue variable: a decl that merely precedes `iteration_text`
    // can still sit inside the loop body, which resets the flag every
    // iteration and makes the post-halt iteration never see the halt. That
    // is the exact failure this test exists to prevent, and the weaker proxy
    // was blind to it (caught by neuter-testing this file).
    let loop_entry = TOOL_LOOP_SRC
        .find("let iter_is_truncation_continue = current_iter_is_truncation_continue;")
        .expect("loop-entry marker must exist");
    assert!(
        decl < loop_entry,
        "the flag must be declared BEFORE the iteration loop opens, or it resets \
         every iteration and the post-halt sign-off never sees the halt (#541)"
    );
}

#[test]
fn both_halt_sites_set_the_flag() {
    let sets = TOOL_LOOP_SRC
        .matches("option_surface_halt_seen = true;")
        .count();
    assert!(
        sets >= 2,
        "both suggest_options halt sites must set the flag (#1178 M1); found {sets}"
    );
}

#[test]
fn phantom_eligible_itself_is_not_halt_gated() {
    // The fix gates the KILL, not eligibility. Gating eligibility would
    // disarm the loop's termination mechanism: `max_tool_iterations` is 0
    // (unlimited) by design and the phantom block is the safety net (#746).
    let anchor = "let phantom_eligible = !is_cli_provider";
    let pos = TOOL_LOOP_SRC
        .find(anchor)
        .expect("phantom_eligible definition must exist");
    let window = &TOOL_LOOP_SRC[pos..(pos + 2500).min(TOOL_LOOP_SRC.len())];
    assert!(
        !window.contains("option_surface_halt_seen"),
        "do NOT gate phantom_eligible on the halt flag — that disarms the loop's \
         termination bound (max_tool_iterations == 0, #746)"
    );
}

#[test]
fn exemption_does_not_touch_the_suppression_or_roll_paths() {
    // #1506's structured-report suppression and the roll/exhausted path are
    // separate arms of the same block. #541 must not disturb either: the
    // suppression still skips the budget, and the roll guard still exists.
    assert!(
        TOOL_LOOP_SRC.contains("if kill && structured_report"),
        "the #1506 suppression branch must still key off the kill predicate"
    );
    assert!(
        TOOL_LOOP_SRC.contains("if phantom_rolls < MAX_PHANTOM_ROLLS"),
        "the roll guard must survive (#1172 ceiling arithmetic depends on it)"
    );
}
