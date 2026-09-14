//! Regression (#35): the rich flow-card entry list dropped intra-entry
//! newlines.
//!
//! The two flow surfaces speak different HTML dialects. The classic
//! `sendMessage` path renders a literal `\n` as a line break, so its entry list
//! is joined with `"\n\n"` and each entry's own newlines survive. The rich
//! `sendRichMessage` dialect renders real HTML, where a `\n` inside a `<p>` is
//! ordinary whitespace and collapses — so a multi-paragraph entry arrived as
//! one run-on line.
//!
//! The loss point was the hand-rolled `<p>{entry}</p>` wrap in the rich
//! details renderer, which bypassed the AST renderer that already carried the
//! `\n` → `<br>` rule (#1142). That rule now lives in one place —
//! `rich::render_html::soft_breaks_to_br`, reached from the fragment sites via
//! `paragraph_html` — and these tests pin both halves: the rich dialect breaks,
//! the classic dialect does not, and a newline inside an inline code span is
//! data in both.

use crate::channels::telegram::flow::{
    FlowHeader, FlowLine, render_flow_details, render_flow_details_chrome, render_flow_html,
};
use crate::channels::telegram::flow_chrome::FlowSections;
use crate::channels::telegram::rich::{markdown_to_html, markdown_to_html_p, paragraph_html};

fn text(s: &str) -> FlowLine {
    FlowLine::Text(s.to_string())
}

// ---------------------------------------------------------------------------
// The defect: a multi-paragraph flow entry through the rich renderer.
// ---------------------------------------------------------------------------

#[test]
fn rich_flow_entry_soft_break_becomes_br() {
    let html = render_flow_details(&[text("first paragraph\nsecond paragraph")], None);

    assert!(
        html.contains("<p>first paragraph<br>second paragraph</p>"),
        "the rich sendRichMessage dialect collapses a bare newline inside <p>, \
         so a multi-paragraph flow entry must arrive with an explicit <br> or it \
         renders as one line. Got:\n{html}"
    );
}

#[test]
fn rich_flow_entry_keeps_every_paragraph_of_a_multi_paragraph_narration() {
    let html = render_flow_details(&[text("one\ntwo\nthree")], None);

    assert!(
        html.contains("<p>one<br>two<br>three</p>"),
        "every soft break in an entry must become a break, not just the first. \
         Got:\n{html}"
    );
}

// ---------------------------------------------------------------------------
// The regression guard: the classic dialect must not inherit the rich
// convention. A `<br>` there is visible literal text.
// ---------------------------------------------------------------------------

#[test]
fn classic_flow_entry_keeps_the_literal_newline() {
    let html = render_flow_html(&[text("first paragraph\nsecond paragraph")], None);

    assert!(
        html.contains("first paragraph\nsecond paragraph"),
        "classic ParseMode::Html renders a literal newline as the break; the \
         entry's own newlines must survive untouched. Got:\n{html}"
    );
    assert!(
        !html.contains("<br>"),
        "no <br> may leak into the classic dialect — it shows as literal text. \
         Got:\n{html}"
    );
}

// ---------------------------------------------------------------------------
// Span-awareness: a newline inside inline code is data, not a break. Narration
// routinely carries inline code, and `format_inline` finds the closing backtick
// across newlines, so multi-line code spans really occur.
// ---------------------------------------------------------------------------

#[test]
fn a_soft_break_inside_an_inline_code_span_stays_literal() {
    let html = render_flow_details(&[text("run `cargo build\n--release` now")], None);

    assert!(
        html.contains("<code>cargo build\n--release</code>"),
        "a newline inside an inline code span is data and must be copied \
         verbatim. Got:\n{html}"
    );
    assert!(
        !html.contains("<code>cargo build<br>--release</code>"),
        "a <br> must never be injected inside a code span. Got:\n{html}"
    );
}

#[test]
fn text_around_a_code_span_still_breaks() {
    let html = render_flow_details(&[text("before\n`code\nspan`\nafter")], None);

    assert!(
        html.contains("before<br>"),
        "the break before the code span must still apply. Got:\n{html}"
    );
    assert!(
        html.contains("<code>code\nspan</code>"),
        "the code span's newline stays data. Got:\n{html}"
    );
    assert!(
        html.contains("<br>after"),
        "the break after the code span must still apply. Got:\n{html}"
    );
}

// ---------------------------------------------------------------------------
// Idempotency: an entry that already carries a <br> is left alone, so a
// fragment is never double-converted.
// ---------------------------------------------------------------------------

#[test]
fn an_entry_already_carrying_br_is_unchanged() {
    let html = paragraph_html("line one<br>line two");

    assert_eq!(
        html, "<p>line one<br>line two</p>",
        "an entry that already carries <br> must pass through unchanged"
    );
}

#[test]
fn a_fragment_with_no_soft_break_is_a_plain_wrap() {
    assert_eq!(paragraph_html("just one line"), "<p>just one line</p>");
}

// ---------------------------------------------------------------------------
// End-to-end through the flow card: the entry list rides inside the
// processing-log <details>, and the break has to survive that assembly.
// ---------------------------------------------------------------------------

#[test]
fn the_flow_card_entry_list_end_to_end_contains_br() {
    let lines = vec![
        FlowLine::Tool {
            label: "⚙️ bash".to_string(),
            context: "ls".to_string(),
            raw_context: String::new(),
        },
        text("**Status:** two paragraphs follow\n\nsecond one"),
    ];
    let html = render_flow_details(&lines, None);

    assert!(
        html.contains("<details>"),
        "a multi-entry log collapses into the processing-log details. Got:\n{html}"
    );
    assert!(
        html.contains("<br>"),
        "the entry's soft break must survive the card assembly. Got:\n{html}"
    );
    assert!(
        html.contains("<b>Status:</b>"),
        "inline formatting inside the entry must still apply. Got:\n{html}"
    );
}

#[test]
fn rich_chrome_checklist_row_soft_break_becomes_br() {
    let secs = FlowSections {
        plan_title: Some("Plan".to_string()),
        checklist: Some(vec!["☐ first\nsecond".to_string()]),
        ..Default::default()
    };
    let html = render_flow_details_chrome(&[], &FlowHeader::Live(None), None, &secs, 0);

    assert!(
        html.contains("<p>☐ first<br>second</p>"),
        "a checklist row carrying a soft break must break in the rich dialect \
         too — every rich <p> site shares the one rule. Got:\n{html}"
    );
}

// ---------------------------------------------------------------------------
// The extraction must not have changed the AST arm's output. The #1142 pin
// stays byte-identical: the arm now delegates to the extracted rule instead of
// inlining the replace.
// ---------------------------------------------------------------------------

#[test]
fn the_ast_arm_still_emits_br_for_rich_paragraphs() {
    assert_eq!(
        markdown_to_html_p("first line\nsecond line"),
        "<p>first line<br>second line</p>",
        "the rich dialect's soft break must still be an explicit <br> after the \
         extraction"
    );
}

#[test]
fn the_ast_arm_still_keeps_literal_newlines_in_the_classic_dialect() {
    assert_eq!(
        markdown_to_html("first line\nsecond line"),
        "first line\nsecond line",
        "the classic dialect must not gain a <br> from the extraction"
    );
}
