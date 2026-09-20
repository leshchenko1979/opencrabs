//! Provenance contract for `FlowLine::System` (#444, child of #291).
//!
//! Compaction banners and turn-lifecycle chrome reach the flow log as
//! `FlowEntry::System`. Before #444 `flow_lines` flattened them into
//! `FlowLine::Text`, so the collapsed header preview and the
//! `latest_intermediary_thought` slot promoted compaction chrome as if the
//! MODEL had said it — while genuine thoughts never appeared there. That is
//! the owner-reported symptom: "the thoughts don't get into the tool roll
//! header, but the compaction result does".
//!
//! Cases a–e are the negative controls: on the pre-fix tree each one returns
//! the chrome where the fix returns real content or `None`. Case f is the
//! parity guard — it fails only if the fix over-reaches and drops chrome from
//! the expanded body log, or counts it as a tool call.
//!
//! `FlowLine` carries no derives (production never compares or prints it), so
//! the cases read the variants explicitly instead of using `assert_eq!` on a
//! `FlowLine` value.

use crate::channels::telegram::flow::{
    FlowEntry, FlowLine, StreamingState, flow_lines, latest_activity_preview,
    latest_intermediary_thought, render_flow_details, render_flow_html, render_flow_rich,
};
use crate::tests::telegram_flow_telemetry_test::base_test_state;

/// A realistic `⏳` compaction banner, as the daemon emits it.
const CHROME: &str = "⏳ Compacting context — 66% full…";
/// A second chrome line (turn lifecycle), so the skip is not a one-off string.
const CHROME_RESUME: &str = "🔄 Resuming after compaction";
/// Genuine model narration — what the header SHOULD promote.
const THOUGHT: &str = "Considering edge cases in the parser";

/// Payload of a `FlowLine::System`, `None` for every other variant.
fn system_text(line: &FlowLine) -> Option<&str> {
    match line {
        FlowLine::System(t) => Some(t),
        _ => None,
    }
}

/// Payload of a `FlowLine::Text`, `None` for every other variant.
fn narration_text(line: &FlowLine) -> Option<&str> {
    match line {
        FlowLine::Text(t) => Some(t),
        _ => None,
    }
}

/// The shared telemetry fixture, narrowed to one narration entry followed by
/// one chrome entry and no tool calls.
fn provenance_state() -> StreamingState {
    let mut s = base_test_state();
    s.tool_msgs.clear();
    s.flow_entries = vec![
        FlowEntry::Text(THOUGHT.to_string()),
        FlowEntry::System(CHROME.to_string()),
    ];
    s
}

/// A tool line, as `flow_lines` would project it.
fn tool_line(label: &str, context: &str) -> FlowLine {
    FlowLine::Tool {
        label: label.to_string(),
        context: context.to_string(),
        raw_context: String::new(),
    }
}

/// Case a: the projection PRESERVES provenance — `FlowEntry::Text` becomes
/// `FlowLine::Text` and `FlowEntry::System` becomes `FlowLine::System`, never
/// the other way round. This is the root-cause leg: cases b–f construct
/// `FlowLine` values directly, so only this case catches a regression inside
/// `flow_lines` itself.
#[test]
fn case_a_projection_preserves_provenance() {
    let lines = flow_lines(&provenance_state());
    assert_eq!(lines.len(), 2, "one flow line per flow entry");

    // Model narration stays narration.
    assert_eq!(
        narration_text(&lines[0]),
        Some(THOUGHT),
        "FlowEntry::Text projects to FlowLine::Text"
    );
    assert_eq!(
        system_text(&lines[0]),
        None,
        "model narration is never mislabelled as system chrome"
    );

    // System chrome keeps its provenance — the #444 defect was exactly this
    // projection collapsing into FlowLine::Text.
    assert_eq!(
        system_text(&lines[1]),
        Some(CHROME),
        "FlowEntry::System projects to FlowLine::System (#444)"
    );
    assert_eq!(
        narration_text(&lines[1]),
        None,
        "system chrome is NOT flattened into FlowLine::Text — the #444 defect"
    );
}

/// Case b: the header skips trailing chrome and falls back to the real
/// thought. Pre-fix, `latest_activity_preview` returned the banner instead.
#[test]
fn case_b_header_skips_trailing_chrome() {
    let lines = [
        FlowLine::Text(THOUGHT.to_string()),
        FlowLine::System(CHROME.to_string()),
    ];

    let preview = latest_activity_preview(&lines);
    assert_eq!(
        preview.as_deref(),
        Some(THOUGHT),
        "the header preview must skip trailing chrome and find the earlier thought"
    );
    assert_ne!(
        preview.as_deref(),
        Some(CHROME),
        "the compaction banner must never become the header preview"
    );
}

/// Case c: with no narration at all, the header falls through the chrome to
/// the prior tool line. Pre-fix it stopped on the banner.
#[test]
fn case_c_header_falls_through_chrome_to_a_tool() {
    let lines = [
        tool_line("✅ read_file", "flow.rs"),
        FlowLine::System(CHROME.to_string()),
    ];

    let preview = latest_activity_preview(&lines);
    assert_eq!(
        preview.as_deref(),
        Some("✅ read_file flow.rs"),
        "the header preview must fall through chrome to the prior tool call"
    );
    assert_ne!(
        preview.as_deref(),
        Some(CHROME),
        "the compaction banner must never become the header preview"
    );
}

/// Case d: the intermediary-thought slot skips chrome. A lone banner yields
/// `None`, and a banner AFTER a thought is skipped so the thought is found.
#[test]
fn case_d_thought_slot_skips_chrome() {
    let lone_chrome = [FlowLine::System(CHROME.to_string())];
    assert_eq!(
        latest_intermediary_thought(&lone_chrome),
        None,
        "a lone system line is not a thought"
    );

    let thought_then_chrome = [
        FlowLine::Text(THOUGHT.to_string()),
        FlowLine::System(CHROME.to_string()),
    ];
    assert_eq!(
        latest_intermediary_thought(&thought_then_chrome).as_deref(),
        Some(THOUGHT),
        "the thought slot skips the trailing chrome and returns the thought"
    );
}

/// Case e: an all-chrome turn yields `None` from both selectors, so the
/// header falls back to the plain processing-log label rather than narrating
/// the harness's own bookkeeping.
#[test]
fn case_e_all_chrome_turn_yields_none() {
    let lines = [
        FlowLine::System(CHROME.to_string()),
        FlowLine::System(CHROME_RESUME.to_string()),
    ];

    assert_eq!(
        latest_activity_preview(&lines),
        None,
        "an all-chrome turn has no activity preview"
    );
    assert_eq!(
        latest_intermediary_thought(&lines),
        None,
        "an all-chrome turn has no intermediary thought"
    );
}

/// Case f: the fix is provenance-only, never suppression — the expanded body
/// log still shows the chrome, and chrome is not counted as a tool call.
///
/// Discriminator (fails on the pre-fix tree): the collapsed summary falls back
/// to the `⛏ Processing log` label instead of promoting the banner. Pre-fix
/// `summary_header` returned the banner here.
///
/// The `1 tool calls` counter-control is what keeps the zero-count assertion
/// from being vacuous: it proves this renderer really does emit that string
/// when a real tool is present.
#[test]
fn case_f_body_renders_chrome_and_counts_only_real_tools() {
    let chrome_only = [FlowLine::System(CHROME.to_string())];

    // Classic HTML path: body keeps the banner, summary does NOT promote it.
    let html = render_flow_html(&chrome_only, None);
    assert!(
        html.contains(CHROME),
        "the classic body log keeps the chrome (#444 parity): {html:?}"
    );
    assert!(
        html.contains("⛏ Processing log"),
        "the collapsed summary falls back to the log label, never the chrome: {html:?}"
    );

    // Rich-details path: same contract, same fallback.
    let details = render_flow_details(&chrome_only, None);
    assert!(
        details.contains(CHROME),
        "the details body keeps the chrome (#444 parity): {details:?}"
    );
    assert!(
        details.contains("⛏ Processing log"),
        "the details summary falls back to the log label, never the chrome: {details:?}"
    );

    // Rich markdown path: chrome is in the body and the header counts ZERO
    // tools — a banner is bookkeeping, not a call.
    let rich = render_flow_rich(&chrome_only, None, false);
    assert!(
        rich.contains(CHROME),
        "the rich body keeps the chrome (#444 parity): {rich:?}"
    );
    assert!(
        rich.contains("Processing log"),
        "a chrome-only turn keeps the zero-tool header: {rich:?}"
    );
    assert!(
        !rich.contains("tool calls"),
        "chrome is never counted as a tool call: {rich:?}"
    );

    // Counter-control: one real tool line DOES produce the count, so the
    // assertion above is a measurement and not a string that never appears.
    let with_tool = [
        tool_line("✅ read_file", "flow.rs"),
        FlowLine::System(CHROME.to_string()),
    ];
    let rich_tool = render_flow_rich(&with_tool, None, false);
    assert!(
        rich_tool.contains("1 tool calls"),
        "one real tool line is counted: {rich_tool:?}"
    );
    assert!(
        rich_tool.contains(CHROME),
        "the chrome stays in the body beside a real tool: {rich_tool:?}"
    );
}
