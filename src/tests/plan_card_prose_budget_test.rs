//! Plan-card PROSE budget: fair-share allocation with honest truncation (#983).
//!
//! The classic card (`sendMessage`, 4096-char cap) spent one 2400-char prose
//! budget **first-come in document order** and cut each section with a
//! head-only `truncate_chars` that appends no marker. Measured on the live box
//! for the *Fix CI Ship Process* plan — 17,310 chars across five sections —
//! Context (3171) consumed the whole budget and the `break` then dropped the
//! four following sections entirely, so the implementation plan (11,369 chars)
//! never reached the message at all.
//!
//! These tests pin the replacement: every section gets a share, any section
//! that loses text says so inline, and the total stays inside the budget.
//!
//! Distinct from `plan_card_budget_test.rs`, which covers the TUI card HEIGHT
//! budget (#1750) — different budget, different surface.

use crate::channels::telegram::flow_chrome::ProseSection;
use crate::channels::telegram::plan_card::{render_plan_card_html, render_plan_card_rich_html};

/// The live fixture's section lengths, in document order: Context 3171 ·
/// Tool-log review 1224 · Design diagram 705 · Implementation steps 11369 ·
/// Verification and boundaries 841 — 17,310 chars total.
const LIVE_FIXTURE: [usize; 5] = [3171, 1224, 705, 11369, 841];

const HEADINGS: [&str; 5] = [
    "Context",
    "Tool-log review",
    "Design diagram",
    "Implementation steps",
    "Verification and boundaries",
];

/// A body of exactly `len` chars, deterministic and distinct per section so a
/// mixed-up section is visible rather than merely a wrong length.
fn body_of(heading: &str, len: usize) -> String {
    let seed = format!("{heading}: ");
    let mut s = String::with_capacity(len);
    while s.chars().count() < len {
        s.push_str(&seed);
    }
    s.chars().take(len).collect()
}

fn live_fixture() -> Vec<ProseSection> {
    LIVE_FIXTURE
        .iter()
        .zip(HEADINGS)
        .map(|(&len, heading)| ProseSection {
            heading: Some(heading.to_string()),
            body: body_of(heading, len),
        })
        .collect()
}

fn many_sections(n: usize) -> Vec<ProseSection> {
    (0..n)
        .map(|i| ProseSection {
            heading: Some(format!("Section {i}")),
            body: body_of(&format!("Section {i}"), 300),
        })
        .collect()
}

/// Visible text of a rendered fragment: every `<…>` span removed, so tag
/// names cannot be mistaken for prose.
fn strip_tags(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out
}

/// One per prose section actually delivered.
fn prose_section_count(html: &str) -> usize {
    html.matches("<blockquote expandable>").count()
}

/// #983 — the defect itself. Before the fix this render carried ONE section
/// (Context, cut mid-word) and no trace of the other four; the implementation
/// plan was simply absent. Every heading must now appear.
#[tokio::test]
async fn every_section_renders_when_budget_is_overspent() {
    let sections = live_fixture();
    let html = render_plan_card_html(Some("Fix CI Ship Process"), None, Some(&sections), None)
        .await
        .expect("a plan with prose renders a card");

    for heading in HEADINGS {
        assert!(
            html.contains(heading),
            "section '{heading}' is missing from the card: the prose budget \
             starved it, which is the #983 defect"
        );
    }
    assert_eq!(
        prose_section_count(&html),
        HEADINGS.len(),
        "every one of the five sections must reach the card"
    );
}

/// #983 — the budget is a cap, not a suggestion: the prose the card carries
/// must still fit inside CARD_PROSE_BUDGET (2400), or it overruns the
/// 4096-char message cap the budget exists to protect.
///
/// Counted on the RAW bodies the renderer was handed, cross-checked against
/// what reached the HTML: markup is not part of the budget, but a body that
/// vanished from the HTML must not be counted as if it fitted.
#[tokio::test]
async fn prose_total_stays_within_budget() {
    let sections = live_fixture();
    let html = render_plan_card_html(Some("Fix CI Ship Process"), None, Some(&sections), None)
        .await
        .expect("a plan with prose renders a card");

    // The rendered prose, stripped of markup: each blockquote body minus its
    // bold heading line.
    let prose_chars: usize = html
        .split("<blockquote expandable>")
        .skip(1)
        .map(|chunk| {
            let body = chunk.split("</blockquote>").next().unwrap_or_default();
            let without_heading = body.split('\n').skip(1).collect::<String>();
            strip_tags(&without_heading).chars().count()
        })
        .sum();

    assert!(
        prose_chars <= 2400,
        "rendered prose is {prose_chars} chars, over the 2400 budget"
    );
    // A zero here would satisfy the bound vacuously.
    assert!(
        prose_chars > 1000,
        "the fixture must actually render prose ({prose_chars} chars), else \
         the bound proves nothing"
    );
}

/// A section that lost text must SAY so. The old head-only cut appended no
/// marker, so a truncated plan read as a complete one — the honesty half of
/// the fix (`truncate_chars_tail_preserving`, #490).
#[tokio::test]
async fn cut_sections_carry_the_omitted_marker() {
    let sections = live_fixture();
    let html = render_plan_card_html(Some("Fix CI Ship Process"), None, Some(&sections), None)
        .await
        .expect("a plan with prose renders a card");

    assert!(
        html.contains("truncated"),
        "a cut section must carry its marker; without it the card claims to \
         be the whole plan"
    );
}

/// A plan whose prose already fits renders byte-identically to before: no
/// marker, no cut, every section whole. The fix must not touch short plans.
#[tokio::test]
async fn short_plan_renders_every_section_whole() {
    let sections = vec![
        ProseSection {
            heading: Some("Context".to_string()),
            body: body_of("Context", 400),
        },
        ProseSection {
            heading: Some("Steps".to_string()),
            body: body_of("Steps", 500),
        },
    ];
    let html = render_plan_card_html(Some("Short plan"), None, Some(&sections), None)
        .await
        .expect("a plan with prose renders a card");

    assert!(
        !html.contains("truncated"),
        "a 900-char plan fits the 2400 budget and must not be cut"
    );
    assert_eq!(prose_section_count(&html), 2);
    for sec in &sections {
        assert!(
            html.contains(&sec.body),
            "an uncut body must appear verbatim in the card"
        );
    }
}

/// #983 step 2 — past `MAX_CARD_SECTIONS` (12) every fair share would fall
/// under the minimum readable body, and `truncate_chars_tail_preserving`
/// silently degrades to a marker-less cut below 80 chars. So the card bounds
/// the count and ANNOUNCES the overflow instead of emitting 20 stubs.
#[tokio::test]
async fn section_count_is_bounded_and_overflow_is_announced() {
    let sections = many_sections(20);
    let html = render_plan_card_html(Some("Twenty sections"), None, Some(&sections), None)
        .await
        .expect("a plan with prose renders a card");

    assert_eq!(
        prose_section_count(&html),
        12,
        "the classic card renders at most MAX_CARD_SECTIONS sections"
    );
    assert!(
        html.contains("8 more sections"),
        "the overflow must be announced with the count that was withheld \
         (20 - 12); got: {html}"
    );
    assert!(
        !html.contains("Section 19"),
        "a section past the bound must not render"
    );
}

/// The rich path is unbounded by design (32K cap, no budget): the same
/// 20-section document must render every section there. This is the control
/// proving the bound belongs to the classic arm, not to the document.
#[tokio::test]
async fn rich_path_renders_every_section_of_the_same_document() {
    let sections = many_sections(20);
    let html = render_plan_card_rich_html(Some("Twenty sections"), None, Some(&sections), None)
        .await
        .expect("a plan with prose renders a card");

    assert!(
        html.contains("Section 19"),
        "the rich path carries no budget and must render every section"
    );
    assert!(
        !html.contains("more sections"),
        "the rich path has no overflow to announce"
    );
}
