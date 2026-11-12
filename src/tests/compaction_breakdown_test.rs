//! #824: the post-compaction breakdown card.
//!
//! Pins the rendered shape and the two rules the live prototype exposed:
//! every emitted name is wrapped in a backtick span (bare `.md` and `.rs`
//! auto-link — both are real ccTLDs), and a `discard_skills` entry that
//! resolves to no skill is never counted as one.
//!
//! Backticks rather than `<code>`: the card lands in the flow body, which
//! runs `escape_html` over the whole line BEFORE `format_inline` converts
//! markdown spans. An HTML tag emitted by the card would therefore render as
//! literal text; a backtick span is the form that survives that pipeline.
//!
//! Pure: no agent, no locks, no network. `load_all_skills()` reads the local
//! skill registry, which always contains the built-ins.

use std::time::Duration;

use crate::channels::telegram::flow::{
    CompactionBreakdown, compacted_flow_line, compaction_breakdown,
};

/// A card built from the live receipt the prototype used: session `2fbfb2f8`,
/// 130,600 → 106,186 tokens (65% → 53%), 21.5 s.
fn card(summary: &str) -> String {
    compaction_breakdown(&CompactionBreakdown {
        before_tokens: 130_600,
        after_tokens: 106_186,
        before_pct: 65.0,
        after_pct: 53.0,
        ctx_max: 200_000,
        elapsed: Duration::from_millis(21_500),
        tool_tokens: 7_400,
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
    assert!(lines[1].contains("131K → 106K"), "tokens row: {}", lines[1]);
    assert!(lines[1].contains("-24K, -18.7%"), "delta row: {}", lines[1]);
    assert!(
        lines[2].contains("65% → 53% of 200K"),
        "fill row: {}",
        lines[2]
    );
    // Truncated, not rounded: 21.5 s is reported as 21 s.
    assert!(lines[3].contains("21s"), "took row: {}", lines[3]);
}

#[test]
fn tools_row_survives_a_document_with_no_manifest() {
    // The schema cost comes from the event, so the row stands even when the
    // summariser omitted its fence — the number is known either way.
    let out = card(no_manifest());
    let tools = out.lines().last().expect("a last line");
    assert!(tools.contains("tools"), "{tools}");
    assert!(tools.contains("7K"), "{tools}");
    assert!(!out.contains("kept"), "no manifest, no kept row: {out}");
    assert!(
        !out.contains("dropped"),
        "no manifest, no dropped row: {out}"
    );
}

#[test]
fn a_discarded_tool_name_is_not_reported_as_a_skill() {
    // Measured on session 2fbfb2f8: every `discard_skills` entry was a TOOL
    // name. They reach `unregister_active_skill`, which no-ops — inert, but
    // calling them skills would be a false statement on the card.
    let out = card(mixed_manifest());
    assert!(out.contains("dropped"), "{out}");
    assert!(
        out.contains("none resolve to a skill"),
        "tool names must be reported as unresolved: {out}"
    );
    assert!(
        out.contains("`a2a_send`"),
        "the names are still listed: {out}"
    );
}

#[test]
fn a_resolvable_skill_carries_a_measured_size() {
    let out = card(mixed_manifest());
    let kept_line = out
        .lines()
        .find(|l| l.contains("kept"))
        .expect("a kept row");
    assert!(kept_line.contains("1 entry"), "{kept_line}");
    // A size, not just the name: the number is what makes the row useful.
    assert!(
        kept_line.chars().any(|c| c.is_ascii_digit()),
        "the kept row must carry a measured token count: {kept_line}"
    );
    assert!(out.contains("`cost-estimate`"), "{out}");
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
    let out = compaction_breakdown(&CompactionBreakdown {
        before_tokens: 100_000,
        after_tokens: 120_000,
        before_pct: 50.0,
        after_pct: 60.0,
        ctx_max: 200_000,
        elapsed: Duration::from_secs(5),
        tool_tokens: 1_000,
        summary: no_manifest(),
    });
    assert!(out.contains("+20K, +20.0%"), "{out}");
}

#[test]
fn a_long_silent_window_reads_in_minutes() {
    let out = compaction_breakdown(&CompactionBreakdown {
        before_tokens: 130_600,
        after_tokens: 106_186,
        before_pct: 65.0,
        after_pct: 53.0,
        ctx_max: 200_000,
        elapsed: Duration::from_secs(102),
        tool_tokens: 7_400,
        summary: no_manifest(),
    });
    assert!(out.contains("1m 42s"), "{out}");
}
