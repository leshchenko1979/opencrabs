//! #1021: live tool roll renders expanded; settled and quiet mode stay collapsed.

use crate::channels::telegram::flow::{
    FlowHeader, FlowLine, FlowOutcome, render_flow, render_flow_details_chrome_pref,
    render_flow_html_chrome_pref,
};
use crate::tests::telegram_flow_telemetry_test::base_test_state;

fn tline(label: &str, context: &str) -> FlowLine {
    FlowLine::Tool {
        label: label.to_string(),
        context: context.to_string(),
        raw_context: String::new(),
    }
}

fn processing_log_blockquote(html: &str) -> Option<&str> {
    html.split('\n')
        .find(|line| line.contains("<blockquote expandable"))
        .map(str::trim)
}

#[test]
fn live_edit_renders_tool_roll_expanded_classic() {
    let out = render_flow_html_chrome_pref(
        &[tline("⚙️ bash", "git status")],
        &FlowHeader::Live(None),
        None,
        &Default::default(),
        usize::MAX,
        3,
        false,
        None,
        false,
    );
    let roll = processing_log_blockquote(&out).expect("processing log");
    assert!(
        roll.starts_with("<blockquote expandable open>"),
        "mid-turn classic roll must be expanded: {roll}"
    );
}

#[test]
fn live_edit_renders_tool_roll_expanded_rich() {
    let out = render_flow_details_chrome_pref(
        &[tline("⚙️ bash", "git status")],
        &FlowHeader::Live(None),
        None,
        &Default::default(),
        usize::MAX,
        3,
        false,
        None,
        false,
    );
    assert!(
        out.contains("<details open><summary>"),
        "mid-turn rich roll must be open: {out}"
    );
}

#[test]
fn settled_edit_renders_tool_roll_collapsed() {
    let out = render_flow_html_chrome_pref(
        &[tline("✅ bash", "git status")],
        &FlowHeader::Settled {
            icon: "✅",
            verb: "Finished",
            duration: "5s",
        },
        None,
        &Default::default(),
        usize::MAX,
        5,
        false,
        None,
        false,
    );
    let roll = processing_log_blockquote(&out).expect("processing log");
    assert!(
        roll.starts_with("<blockquote expandable>"),
        "settled roll must stay collapsed: {roll}"
    );
    assert!(
        !roll.starts_with("<blockquote expandable open>"),
        "settled roll must not ship open: {roll}"
    );
}

#[test]
fn quiet_mode_keeps_roll_collapsed_while_live() {
    for classic in [true, false] {
        if classic {
            let out = render_flow_html_chrome_pref(
                &[tline("⚙️ bash", "git status")],
                &FlowHeader::Live(None),
                None,
                &Default::default(),
                usize::MAX,
                3,
                false,
                None,
                true,
            );
            let roll = processing_log_blockquote(&out).expect("processing log");
            assert!(
                roll.starts_with("<blockquote expandable>") && !roll.contains(" expandable open>"),
                "quiet classic must not expand: {roll}"
            );
        } else {
            let out = render_flow_details_chrome_pref(
                &[tline("⚙️ bash", "git status")],
                &FlowHeader::Live(None),
                None,
                &Default::default(),
                usize::MAX,
                3,
                false,
                None,
                true,
            );
            assert!(
                out.contains("<details><summary>") && !out.contains("<details open>"),
                "quiet rich must not expand: {out}"
            );
        }
    }
}

#[test]
fn render_flow_final_edit_collapses_after_outcome() {
    let mut s = base_test_state();
    s.quiet_mode = false;
    s.flow_rich = false;
    let live = render_flow(&s);
    let live_roll = processing_log_blockquote(&live).expect("live roll");
    assert!(
        live_roll.starts_with("<blockquote expandable open>"),
        "{live_roll}"
    );

    s.flow_outcome = Some(FlowOutcome::Finished);
    let settled = render_flow(&s);
    let settled_roll = processing_log_blockquote(&settled).expect("settled roll");
    assert!(
        settled_roll.starts_with("<blockquote expandable>"),
        "turn-end edit must collapse: {settled_roll}"
    );
    assert!(!settled_roll.starts_with("<blockquote expandable open>"));
}

#[test]
fn overflow_falls_back_to_collapsed_roll() {
    let pad = "x".repeat(60);
    let lines: Vec<FlowLine> = (0..120)
        .map(|i| tline("✅ bash", &format!("{pad}{i}")))
        .collect();
    let out = render_flow_html_chrome_pref(
        &lines,
        &FlowHeader::Live(None),
        None,
        &Default::default(),
        usize::MAX,
        0,
        false,
        None,
        false,
    );
    assert!(
        out.chars().count() > 4000,
        "fixture must exceed the HTML freeze threshold to exercise fallback"
    );
    let roll = processing_log_blockquote(&out).expect("processing log");
    assert!(
        roll.starts_with("<blockquote expandable>") && !roll.contains(" expandable open>"),
        "overflow must fall back to collapsed roll: {roll}"
    );
}
