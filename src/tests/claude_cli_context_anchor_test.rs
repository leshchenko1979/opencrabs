//! Claude CLI's `context_input()` never reaches the ctx meter or the budget (#1677).
//!
//! The field reports Claude Code's own internal cache, not the prompt
//! OpenCrabs sent. `tool_loop` already refused it on the calibration branch
//! ("represents Claude's internal cache, not our sent context") while feeding
//! the same value into `call_context_tokens` sixty lines above, which is what
//! the footer displays and what `record_provider_reported_tokens` anchors the
//! compaction budget on. One day of logs carried both failure directions: a
//! 236,325 report against a 122,082 local estimate pushed the budget to 118%
//! of a 200k window, and a report of `2` against a 28,688 estimate collapsed
//! it so compaction could never fire. The only guard was
//! `is_implausible_token_report`, a 2x heuristic that lets both through.
//!
//! The gate lives inside `run_tool_loop`, which no unit test can call, so the
//! source is the assertion surface.

const TOOL_LOOP: &str = include_str!("../brain/agent/service/tool_loop.rs");

const DECL: &str =
    r#"let is_claude_cli = self.provider_for_session(session_id).name() == "claude-cli";"#;

#[test]
fn the_provider_check_is_declared_once_and_before_the_context_figure() {
    assert_eq!(
        TOOL_LOOP.matches(DECL).count(),
        1,
        "one declaration only — a second one below the anchor is how the gate got missed"
    );

    let decl = TOOL_LOOP.find(DECL).expect("is_claude_cli declaration");
    let call_ctx = TOOL_LOOP
        .find("let call_context_tokens =")
        .expect("call_context_tokens binding");
    assert!(
        decl < call_ctx,
        "the provider check must be hoisted above the figure it gates"
    );
}

#[test]
fn the_displayed_context_figure_skips_claude_cli() {
    assert!(
        TOOL_LOOP.contains("let call_context_tokens = if reported_usage && !is_claude_cli {"),
        "claude-cli must fall through to the local tiktoken estimate, not context_input()"
    );
}

#[test]
fn the_budget_anchor_skips_claude_cli() {
    let claude_start = TOOL_LOOP
        .find("} else if is_claude_cli {")
        .expect("claude-cli calibration branch");
    let claude_end = claude_start
        + TOOL_LOOP[claude_start..]
            .find("\n            } else {")
            .expect("the API branch must follow the claude-cli one");
    assert!(
        !TOOL_LOOP[claude_start..claude_end].contains("record_provider_reported_tokens"),
        "the claude-cli branch must never anchor the budget: its context_input() is \
         Claude's internal cache, not the prompt we sent"
    );
    assert_eq!(
        TOOL_LOOP
            .matches("context.record_provider_reported_tokens(")
            .count(),
        2,
        "exactly two adopting call sites (CLI calibration + API Adopt arm) — a third \
         would mean a branch anchors a report it rejected"
    );
}

#[test]
fn the_budget_anchor_is_recorded_after_the_calibration_overwrite() {
    // The double-count this file exists for (#1677). `record_provider_reported_tokens`
    // snapshots `self.token_count` as `estimated_at_anchor`, and
    // `effective_token_count()` is `reported + token_count - estimated_at_anchor`.
    // Anchoring ABOVE the calibration block captured the pre-calibration estimate
    // (115,961) while the branches below replaced `token_count` with the provider's
    // absolute (139,183), so the 23,222-token gap was added twice: 162,405, or
    // 81.2% of a 200k window, against a real 69.6%. Anchoring after the overwrite
    // makes the gap zero at the instant it is taken.
    let calibration = TOOL_LOOP
        .find("// Calibrate context token count from the provider's reported usage.")
        .expect("calibration block");
    for (assign, anchor, who) in [
        (
            "context.token_count = cli_context;",
            "context.record_provider_reported_tokens(cli_context);",
            "CLI calibration",
        ),
        (
            "context.token_count = actual;",
            "context.record_provider_reported_tokens(actual);",
            "API Adopt arm",
        ),
    ] {
        let assign_at = TOOL_LOOP.find(assign).expect("token_count assignment");
        let anchor_at = TOOL_LOOP.find(anchor).expect("anchor call site");
        assert!(
            assign_at > calibration,
            "{who}: the assignment must live inside the calibration block"
        );
        assert!(
            assign_at < anchor_at,
            "{who}: the anchor must be recorded AFTER the overwrite, or the \
             calibration gap is counted twice"
        );
    }
}

#[test]
fn the_calibration_branch_still_refuses_the_same_field() {
    assert!(
        TOOL_LOOP.contains("represents Claude's internal cache, not our sent context"),
        "the calibration branch is the stated reason for the gate above; keep them together"
    );
}
