//! #824: the post-compaction breakdown card.
//!
//! Pins the rendered shape and the rules the live prototype exposed: every
//! emitted name is wrapped in a backtick span (bare `.md` and `.rs` auto-link —
//! both are real ccTLDs), a `discard_skills` entry that resolves to no skill is
//! never counted as one, and every entry states its own size (KB), its share of
//! the window (%) and, for a document, its line count (LOC).
//!
//! Backticks rather than `<code>`: the card lands in the flow body, which runs
//! `escape_html` over the whole line BEFORE `format_inline` converts markdown
//! spans. An HTML tag emitted by the card would therefore render as literal
//! text; a markdown span is the form that survives that pipeline.
//!
//! Pure: no agent, no locks, no network. `load_all_skills()` reads the local
//! skill registry, which always contains the built-ins.

use std::time::Duration;

use crate::brain::agent::service::ToolSize;
use crate::channels::telegram::flow::{
    CompactionBreakdown, compacted_flow_line, compaction_breakdown,
};

/// The measured tool set the next request carries. Sizes chosen so every
/// rendered figure is exact: 16_384 B = 16.0 KB, 4_000 tok of 200 K = 2.0 %.
fn tool_sizes() -> Vec<ToolSize> {
    vec![
        ToolSize {
            name: "bash".to_string(),
            tokens: 4_000,
            bytes: 16_384,
        },
        ToolSize {
            name: "read_file".to_string(),
            tokens: 2_000,
            bytes: 8_192,
        },
        ToolSize {
            name: "oc_questions".to_string(),
            tokens: 1_400,
            bytes: 4_096,
        },
    ]
}

/// A card built from the live receipt the prototype used: session `2fbfb2f8`,
/// 130,600 → 106,186 tokens (65% → 53%), 21.5 s.
fn card(summary: &str) -> String {
    let sizes = tool_sizes();
    compaction_breakdown(&CompactionBreakdown {
        before_tokens: 130_600,
        after_tokens: 106_186,
        before_pct: 65.0,
        after_pct: 53.0,
        ctx_max: 200_000,
        elapsed: Duration::from_millis(21_500),
        tool_sizes: &sizes,
        summary,
    })
}

/// The document as it arrives without a manifest fence.
fn no_manifest() -> &'static str {
    "the conversation continued; no machine-readable block here."
}

/// A manifest naming one resolvable built-in skill and two tool names, which
/// is the shape the live `discard_skills` list actually took.
fn mixed_manifest() -> &'static str {
    "## Continuation\n\n\
     ```context-manifest\n\
     active_skills:\n  - cost-estimate\n\
     discard_skills:\n  - a2a_send\n  - send_input\n\
     required_tools:\n  - bash\n  - read_file\n\
     ```\n"
}

/// Drop every backtick span, leaving what would reach the link detector.
fn without_code_spans(s: &str) -> String {
    let mut out = String::new();
    let mut rest = s;
    while let Some(start) = rest.find('`') {
        out.push_str(&rest[..start]);
        match rest[start + 1..].find('`') {
            Some(end) => rest = &rest[start + 1 + end + 1..],
            None => return out,
        }
    }
    out.push_str(rest);
    out
}

#[test]
fn card_keeps_the_headline_and_states_the_measured_numbers() {
    let out = card(no_manifest());
    let lines: Vec<&str> = out.lines().collect();

    // The numbered line is still the headline — the card EXTENDS it, never
    // replaces it, so the #29 contract holds.
    assert_eq!(
        lines[0],
        compacted_flow_line(65.0, 53.0, Duration::from_millis(21_500)),
        "headline must be the unchanged numbered line"
    );
    // The section label the owner asked to have back: without it the rows read
    // as part of the tool log rather than as a post-compaction report.
    assert!(
        lines[1].contains("post-compaction context"),
        "section header: {}",
        lines[1]
    );
    assert!(lines[2].contains("131K → 106K"), "tokens row: {}", lines[2]);
    assert!(lines[2].contains("-24K, -18.7%"), "delta row: {}", lines[2]);
    assert!(
        lines[3].contains("65% → 53% of 200K"),
        "fill row: {}",
        lines[3]
    );
    // Truncated, not rounded: 21.5 s is reported as 21 s.
    assert!(lines[4].contains("21s"), "took row: {}", lines[4]);
}

#[test]
fn tools_row_survives_a_document_with_no_manifest() {
    // The schema cost is measured at emit, so the rows stand even when the
    // summariser omitted its fence — the number is known either way.
    let out = card(no_manifest());
    let tools = out
        .lines()
        .find(|l| l.contains("tools"))
        .expect("a tools row");
    // Count · total KB · total share. Total = the sum of the three rows:
    // 28_672 B = 28.0 KB, 7_400 tok of 200 K = 3.7 %.
    assert!(tools.contains("3 · 28.0KB · 3.7%"), "{tools}");
    assert!(!out.contains("kept"), "no manifest, no kept row: {out}");
    assert!(
        !out.contains("dropped"),
        "no manifest, no dropped row: {out}"
    );
}

#[test]
fn every_tool_carries_its_own_kb_and_share() {
    // "size indication similar to skills": a bare name says nothing about what
    // the schema costs, so each row carries both units.
    let out = card(no_manifest());
    let detail = out
        .lines()
        .find(|l| l.contains("`bash`"))
        .expect("a tool detail row");
    assert!(detail.contains("`bash` 16.0KB · 2.0%"), "{detail}");
    // Ordered by cost: the row worth acting on leads.
    let bash = detail.find("`bash`").expect("bash");
    let read = detail.find("`read_file`").expect("read_file");
    assert!(bash < read, "largest schema first: {detail}");
}

#[test]
fn a_resolvable_skill_carries_kb_share_and_loc() {
    let out = card(mixed_manifest());
    let kept_head = out
        .lines()
        .find(|l| l.contains("kept"))
        .expect("a kept row");
    assert!(kept_head.contains("1 entry"), "{kept_head}");
    // The group's own totals, in the same units as its rows.
    assert!(kept_head.contains("KB"), "kept head carries KB: {kept_head}");
    assert!(kept_head.contains('%'), "kept head carries %: {kept_head}");

    let kept_detail = out
        .lines()
        .skip_while(|l| !l.contains("kept"))
        .nth(1)
        .expect("a kept detail row");
    assert!(kept_detail.contains("`cost-estimate`"), "{kept_detail}");
    assert!(kept_detail.contains("KB"), "size: {kept_detail}");
    assert!(kept_detail.contains('%'), "share: {kept_detail}");
    assert!(kept_detail.contains(" LOC"), "line count: {kept_detail}");
}

#[test]
fn a_discarded_tool_name_is_not_reported_as_a_skill() {
    // Measured on session 2fbfb2f8: every `discard_skills` entry was a TOOL
    // name. They reach `unregister_active_skill`, which no-ops — inert, but
    // calling them skills would be a false statement on the card.
    let out = card(mixed_manifest());
    let dropped_head = out
        .lines()
        .find(|l| l.contains("dropped"))
        .expect("a dropped row");
    assert!(
        dropped_head.contains("none resolve to a skill"),
        "tool names must be reported as unresolved: {dropped_head}"
    );
    let dropped_detail = out
        .lines()
        .skip_while(|l| !l.contains("dropped"))
        .nth(1)
        .expect("a dropped detail row");
    assert!(dropped_detail.contains("`a2a_send`"), "{dropped_detail}");
    // Unmeasured is stated as unmeasured: no invented KB, no invented LOC.
    let segment = dropped_detail
        .split(" · ")
        .find(|s| s.contains("a2a_send"))
        .expect("the a2a_send segment");
    assert!(!segment.contains("KB"), "no invented size: {segment}");
    assert!(!segment.contains("LOC"), "no invented LOC: {segment}");
}

#[test]
fn an_unknown_ceiling_states_no_share() {
    // A percentage needs a denominator. With none, the card says so rather
    // than printing a 0% that reads as "this entry costs nothing".
    let sizes = tool_sizes();
    let out = compaction_breakdown(&CompactionBreakdown {
        before_tokens: 1_000,
        after_tokens: 500,
        before_pct: 1.0,
        after_pct: 0.5,
        ctx_max: 0,
        elapsed: Duration::from_secs(1),
        tool_sizes: &sizes,
        summary: no_manifest(),
    });
    let tools = out
        .lines()
        .find(|l| l.contains("tools"))
        .expect("a tools row");
    assert!(tools.contains('—'), "unmeasurable share: {tools}");
}

#[test]
fn every_emitted_name_is_wrapped_in_code() {
    // Bare `.md` / `.rs` auto-link in Telegram (Moldova and Serbia are real
    // ccTLDs). Measured live: eight `url` entities on one card before the
    // wrapper. So no dot-file token may survive outside a code span.
    let summary = "```context-manifest\n\
                   active_skills:\n  - cost-estimate\n  - opencrabs-dev/editor.md\n\
                   discard_skills:\n  - opencrabs-dev/triage.md\n\
                   required_tools:\n  - bash\n  - src/brain/skills.rs\n\
                   ```\n";
    let out = card(summary);
    let bare = without_code_spans(&out);
    assert!(!bare.contains(".md"), "a bare .md leaked: {bare}");
    assert!(!bare.contains(".rs"), "a bare .rs leaked: {bare}");
}

#[test]
fn a_compaction_that_grew_the_context_reads_as_growth() {
    // Signed, never unsigned: a compaction can leave the window larger, and an
    // unsigned figure would present that as a saving.
    let sizes = tool_sizes();
    let out = compaction_breakdown(&CompactionBreakdown {
        before_tokens: 100_000,
        after_tokens: 120_000,
        before_pct: 50.0,
        after_pct: 60.0,
        ctx_max: 200_000,
        elapsed: Duration::from_secs(5),
        tool_sizes: &sizes,
        summary: no_manifest(),
    });
    assert!(out.contains("+20K, +20.0%"), "{out}");
}

#[test]
fn a_long_silent_window_reads_in_minutes() {
    let sizes = tool_sizes();
    let out = compaction_breakdown(&CompactionBreakdown {
        before_tokens: 130_600,
        after_tokens: 106_186,
        before_pct: 65.0,
        after_pct: 53.0,
        ctx_max: 200_000,
        elapsed: Duration::from_secs(102),
        tool_sizes: &sizes,
        summary: no_manifest(),
    });
    assert!(out.contains("1m 42s"), "{out}");
}
