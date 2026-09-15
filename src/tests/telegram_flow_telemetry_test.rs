//! Integration tests for flow card telemetry metrics bar (Issue #232).
//! Verifies zero-suppressed telemetry bar rendering, glyph ordering,
//! and correct DOM placement above quotes and details blocks.

use teloxide::types::MessageId;

use crate::channels::telegram::flow::{
    FlowEntry, FlowOutcome, StreamingState, SubagentCounts, ToolMsg, render_flow,
    render_flow_details_state,
};

fn base_test_state() -> StreamingState {
    StreamingState {
        is_dm: false,
        is_cli: false,
        pending_suggestions: None,
        pending_trailer: None,
        msg_id: None,
        thinking: String::new(),
        tool_msgs: vec![
            ToolMsg {
                msg_id: None,
                name: "read_file".to_string(),
                context: "config.toml".to_string(),
                raw_context: String::new(),
                completed: Some(true),
                dirty: false,
            },
            ToolMsg {
                msg_id: None,
                name: "edit_file".to_string(),
                context: "src/main.rs".to_string(),
                raw_context: String::new(),
                completed: Some(true),
                dirty: false,
            },
        ],
        display_queue: Vec::new(),
        open_group_msg_id: Some(MessageId(42)),
        rich_transport_failures: 0,
        flow_entries: vec![FlowEntry::Tool(0), FlowEntry::Tool(1)],
        flow_status: None,
        flow_rich: false,
        response: String::new(),
        dirty: false,
        recreate: false,
        header_preview: None,
        compacting: false,
        sections: Default::default(),
        retained_goal: None,
        tool_round_count: 0,
        tools_started_at: None,
        turn_started_at: std::time::Instant::now() - std::time::Duration::from_secs(42),
        flow_outcome: None,
        bg_indicator: None,
        bg_count: None,
        subagent_counts: SubagentCounts {
            working: 0,
            awaiting: 0,
        },
        queued_count: 0,
        sent_intermediates: Vec::new(),
        intermediate_msg_ids: Vec::new(),
        voice_msg_ids: Vec::new(),
        applied_plan_kb: Default::default(),
        processing: false,
        final_bubble: None,
    }
}

#[test]
fn flow_telemetry_html_dom_placement() {
    let mut s = base_test_state();
    s.queued_count = 3;
    s.bg_count = Some(1);
    s.subagent_counts = SubagentCounts {
        working: 2,
        awaiting: 0,
    };

    let html = render_flow(&s);

    // Check telemetry line presence
    assert!(
        html.contains("2 ⛏ • 0:42 ⏱ • 1 ⏏️ • 2 🤖 • 3 ✉️"),
        "HTML must contain fully populated telemetry bar: {html}"
    );

    // Verify placement: telemetry line must appear before <blockquote expandable>
    let telem_pos = html.find("2 ⛏ • 0:42 ⏱").expect("telemetry bar present");
    let quote_pos = html
        .find("<blockquote expandable>")
        .expect("blockquote present");
    assert!(
        telem_pos < quote_pos,
        "Telemetry line must appear above <blockquote expandable>: telem_pos={telem_pos}, quote_pos={quote_pos}"
    );
}

#[test]
fn flow_telemetry_details_dom_placement() {
    let mut s = base_test_state();
    s.queued_count = 1;
    s.bg_count = Some(2);

    let details_html = render_flow_details_state(&s);

    // Check paragraph-wrapped telemetry line in rich details
    assert!(
        details_html.contains("<p>2 ⛏ • 0:42 ⏱ • 2 ⏏️ • 1 ✉️</p>"),
        "Details HTML must contain paragraph-wrapped telemetry bar: {details_html}"
    );

    // Verify placement: paragraph telemetry must appear before <details>
    let telem_pos = details_html
        .find("<p>2 ⛏ • 0:42 ⏱")
        .expect("telemetry bar present");
    let details_pos = details_html.find("<details>").expect("details tag present");
    assert!(
        telem_pos < details_pos,
        "Telemetry line must appear above <details>: telem_pos={telem_pos}, details_pos={details_pos}"
    );
}

#[test]
fn flow_telemetry_zero_suppression_in_renders() {
    let mut s = base_test_state();
    // Default zero state for optional fields
    s.queued_count = 0;
    s.bg_count = None;
    s.subagent_counts = SubagentCounts {
        working: 0,
        awaiting: 0,
    };

    let html = render_flow(&s);
    assert!(
        html.contains("2 ⛏ • 0:42 ⏱"),
        "Base metrics always shown: {html}"
    );
    assert!(
        !html.contains('⏏'),
        "Detached icon suppressed when 0: {html}"
    );
    assert!(
        !html.contains('🤖'),
        "Subagent icon suppressed when 0: {html}"
    );
    assert!(!html.contains('✉'), "Queued icon suppressed when 0: {html}");

    let details_html = render_flow_details_state(&s);
    assert!(
        details_html.contains("<p>2 ⛏ • 0:42 ⏱</p>"),
        "Base metrics paragraph in rich details: {details_html}"
    );
    assert!(
        !details_html.contains('⏏'),
        "Detached suppressed in details: {details_html}"
    );
    assert!(
        !details_html.contains('🤖'),
        "Subagent suppressed in details: {details_html}"
    );
    assert!(
        !details_html.contains('✉'),
        "Queued suppressed in details: {details_html}"
    );
}

#[test]
fn flow_telemetry_absent_when_no_tool_log() {
    // When there are no flow lines (has_log is false), telemetry bar is not rendered
    let mut s = base_test_state();
    s.tool_msgs.clear();
    s.flow_entries.clear();
    let html = render_flow(&s);
    assert!(
        !html.contains("⚙️"),
        "No telemetry bar when tool log is empty: {html}"
    );

    let details_html = render_flow_details_state(&s);
    assert!(
        !details_html.contains("⚙️"),
        "No telemetry bar in details when tool log is empty: {details_html}"
    );
}

#[test]
fn flow_telemetry_settled_outcome_renders() {
    let mut s = base_test_state();
    s.flow_outcome = Some(FlowOutcome::Finished);
    s.queued_count = 4;

    let html = render_flow(&s);
    assert!(
        html.contains("2 ⛏ • 0:42 ⏱ • 4 ✉️"),
        "Settled outcome includes telemetry: {html}"
    );

    let details_html = render_flow_details_state(&s);
    assert!(
        details_html.contains("<p>2 ⛏ • 0:42 ⏱ • 4 ✉️</p>"),
        "Settled outcome in details includes telemetry: {details_html}"
    );
}
