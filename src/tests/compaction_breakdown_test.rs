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
    CompactionBreakdown, TRUNCATED_SUMMARY_CARD_LABEL, TRUNCATED_SUMMARY_MARKER_REF,
    compacted_flow_line, compaction_breakdown,
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
        truncated: false,
        scope: "delta",
        input_tokens: 41_300,
        documents: &[],
        document_refusals: &[],
        cache_notice: None,
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
    assert!(lines[2].is_empty(), "blank line before the table: {out}");
    assert_eq!(lines[3], "| metric | value |", "table header: {out}");
    assert_eq!(lines[4], "|---|---|", "table separator: {out}");

    // Rows are found by their LABEL, not by index: the table grows a row at a
    // time, and an index assertion turns a new metric into a false failure.
    let row = |label: &str| -> String {
        out.lines()
            .find(|l| l.starts_with(&format!("| {label} |")))
            .unwrap_or_else(|| panic!("a {label} row: {out}"))
            .to_string()
    };
    assert!(row("tokens").contains("131K → 106K"), "tokens row: {out}");
    assert!(row("tokens").contains("-24K, -18.7%"), "delta row: {out}");
    assert!(row("fill").contains("65% → 53% of 200K"), "fill row: {out}");
    // Truncated, not rounded: 21.5 s is reported as 21 s.
    assert!(row("took").contains("21s"), "took row: {out}");
}

#[test]
fn tools_row_survives_a_document_with_no_manifest() {
    // The schema cost is measured at emit, so the rows stand even when the
    // summariser omitted its fence — the number is known either way.
    let out = card(no_manifest());
    let tools = out
        .lines()
        .find(|l| l.starts_with("| tools |"))
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
fn the_tools_row_is_a_total_only() {
    // Owner order 2026-11-15: the ranked list of six schema names plus a `+18`
    // tail answered nothing. What the reader needs is what the tool set COSTS —
    // the per-tool measurements stay on the event, the card keeps the total.
    let out = card(no_manifest());
    assert!(
        !out.contains("`bash`"),
        "no per-tool rows on the card: {out}"
    );
    let tools = out
        .lines()
        .find(|l| l.starts_with("| tools |"))
        .expect("a tools row");
    // Count · total KB · total share, summed from the three measurements.
    assert!(tools.contains("3 · 28.0KB · 3.7%"), "{tools}");
    // And it is ONE row, with nothing beneath it: the tools group emits a total
    // and no continuation row (the `|  | …` name row belongs to skills and
    // documents). It is also the card's LAST line, which is the emission-order
    // guarantee — a detail row added under it fails here, not on the wire.
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(
        lines.last().copied(),
        Some(tools),
        "the tools total closes the card: {out}"
    );
}

#[test]
fn a_resolvable_skill_carries_kb_share_and_loc() {
    let out = card(mixed_manifest());
    let kept_head = out
        .lines()
        .find(|l| l.starts_with("| kept |"))
        .expect("a kept row");
    assert!(kept_head.contains("1 entry"), "{kept_head}");
    // The group's own totals, in the same units as its rows.
    assert!(
        kept_head.contains("KB"),
        "kept head carries KB: {kept_head}"
    );
    assert!(kept_head.contains('%'), "kept head carries %: {kept_head}");

    let kept_detail = out
        .lines()
        .skip_while(|l| !l.starts_with("| kept |"))
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
        .find(|l| l.starts_with("| dropped |"))
        .expect("a dropped row");
    assert!(
        dropped_head.contains("none resolve to a skill"),
        "tool names must be reported as unresolved: {dropped_head}"
    );
    let dropped_detail = out
        .lines()
        .skip_while(|l| !l.starts_with("| dropped |"))
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
        truncated: false,
        scope: "delta",
        input_tokens: 41_300,
        documents: &[],
        document_refusals: &[],
        cache_notice: None,
    });
    let tools = out
        .lines()
        .find(|l| l.starts_with("| tools |"))
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
        truncated: false,
        scope: "delta",
        input_tokens: 41_300,
        documents: &[],
        document_refusals: &[],
        cache_notice: None,
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
        truncated: false,
        scope: "delta",
        input_tokens: 41_300,
        documents: &[],
        document_refusals: &[],
        cache_notice: None,
    });
    assert!(out.contains("1m 42s"), "{out}");
}

/// The continuation document's numbered sections, as the summariser writes
/// them (#824 follow-up): the per-part rows must come from the DELIVERED
/// string, so this fixture is the delivered form.
fn numbered_document() -> &'static str {
    "## 0. IMMEDIATE TASK\n\
     finish the breakdown card\n\
     \n\
     ## 1. Chronological Analysis\n\
     a longer body that runs on for a while so its size differs from the first\n"
}

/// A card for a CONSOLIDATION pass, so the two scope labels are proven
/// distinct rather than both reading `delta`.
fn consolidate_card(summary: &str) -> String {
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
        // The rich fixture is a consolidation, so the two legs' scope labels
        // are proven distinct rather than both reading `delta`.
        truncated: false,
        scope: "consolidate",
        input_tokens: 130_600,
        documents: &[],
        document_refusals: &[],
        cache_notice: None,
    })
}

#[test]
fn the_classic_leg_states_the_summary_total_only() {
    // The summary is the largest thing a compaction leaves in the window, and
    // the card used to describe the whole window without naming it. The total
    // and the part COUNT stay; the per-part rows are gone (owner order
    // 2026-11-15) — `2 parts` was the only fact the list carried.
    let out = card(numbered_document());
    let head = out
        .lines()
        .find(|l| l.starts_with("| summary |"))
        .expect("a summary row");
    assert!(head.contains("2 parts"), "part count: {head}");
    assert!(head.contains("KB"), "summary size: {head}");
    assert!(head.contains('%'), "summary share: {head}");
    assert!(
        !out.contains("§0 IMMEDIATE TASK"),
        "no per-part rows: {out}"
    );
}

#[test]
fn the_card_is_one_table() {
    // The #853 ban on tables is RETIRED and this test is its replacement; the
    // argument for the reversal lives on `compaction_breakdown` itself. Both of
    // the ban's premises are gone: the `: : :` cells were FABRICATED
    // placeholders, removed by #856, which made an unresolved entry say `—`
    // instead of inventing a number; and the one real rich-plane defect left —
    // an empty first cell rendering as a bare `<b></b>:` — is fixed in
    // `render_key_value` and pinned by `an_empty_first_cell_renders_the_value_alone`.
    // So the card is ONE table, and this is the guard against it becoming two
    // or none.
    let out = card(mixed_manifest());
    let lines: Vec<&str> = out.lines().collect();

    // Exactly one header, exactly one separator, separator immediately below
    // the header: a delimiter row anywhere else parses as nothing at all.
    let headers = lines.iter().filter(|l| l.starts_with("| metric |")).count();
    assert_eq!(headers, 1, "exactly one header row: {out}");
    let seps = lines.iter().filter(|l| l.starts_with("|---")).count();
    assert_eq!(seps, 1, "exactly one separator row: {out}");
    let h = lines
        .iter()
        .position(|l| *l == "| metric | value |")
        .expect("a header row");
    assert_eq!(lines[h + 1], "|---|---|", "separator follows header: {out}");

    // The table begins its own block: without the blank line above the header
    // it degrades to pipe text on the markdown plane.
    assert_eq!(lines[h - 1], "", "blank line before the header: {out}");

    // Everything below the separator is a row — no prose, no second table.
    for line in &lines[h + 2..] {
        assert!(line.starts_with('|'), "not a table row: {line}");
    }
    // Two cells per row, so exactly three pipes: two outer and one between.
    // More means a row lost its line break and collapsed into its neighbour.
    for line in lines.iter().filter(|l| l.starts_with('|')) {
        assert_eq!(line.matches('|').count(), 3, "one row, two cells: {line}");
    }

    // The card's own title stays OUTSIDE the table: the headline must be line 1
    // (`is_compaction_card` matches it, and a folded card is invisible), and the
    // section span names the block for the reader.
    assert!(lines[0].contains("compact:"), "headline first: {out}");
    assert!(
        lines[1].contains("post-compaction context"),
        "section span second: {out}"
    );

    // The CONTENT survives the re-flow: the kept total, the name it counts, and
    // the tool total that names no tool.
    let kept = lines
        .iter()
        .find(|l| l.starts_with("| kept |"))
        .expect("a kept row");
    assert!(kept.contains("1 entry"), "{kept}");
    assert!(kept.contains("KB"), "size: {kept}");
    assert!(kept.contains('%'), "share: {kept}");
    assert!(out.contains("`cost-estimate`"), "the name it counts: {out}");

    let tools = lines
        .iter()
        .find(|l| l.starts_with("| tools |"))
        .expect("a tools total row");
    // 28_672 B = 28.0 KB, 7_400 tok of 200 K = 3.7 %.
    assert!(tools.contains("3 · 28.0KB · 3.7%"), "{tools}");
    assert!(!out.contains("`read_file`"), "no tool names: {out}");
}

#[test]
fn the_summary_rides_the_one_table() {
    // The summary is a ROW in the card's single table, not a second table of
    // its own: one header, one separator, and `| summary |` as a row beneath
    // them. A second header would mean the card had grown a sub-table, which is
    // exactly the shape the rich plane cannot carry.
    let out = consolidate_card(numbered_document());
    assert_eq!(
        out.lines().filter(|l| l.starts_with("| metric |")).count(),
        1,
        "one header for the whole card: {out}"
    );
    assert_eq!(
        out.lines().filter(|l| l.starts_with("|---")).count(),
        1,
        "one separator for the whole card: {out}"
    );
    let head = out
        .lines()
        .find(|l| l.starts_with("| summary |"))
        .expect("a summary row");
    assert!(head.contains("2 parts"), "{head}");
    assert!(head.contains("KB"), "summary size: {head}");
    // The per-part rows are gone: the total is the whole claim.
    assert!(
        !out.contains("§0 IMMEDIATE TASK"),
        "no per-part rows: {out}"
    );
}

#[test]
fn every_row_occupies_its_own_line() {
    // The message-783 defect: on the rich MARKDOWN plane a single `\n` is a
    // SOFT break, so the whole card arrived as one run-on paragraph — five rows
    // and a table header flattened into a single line. The card is emitted one
    // row per line, and the flow plane it now rides (Telegram HTML) keeps `\n`
    // as a line break. This test pins the emission side of that contract: a row
    // that shares a line with its neighbour is the bug, whatever the plane.
    let out = card(mixed_manifest());
    let lines: Vec<&str> = out.lines().collect();
    assert!(
        lines.len() >= 6,
        "a row per line, got {}: {out}",
        lines.len()
    );

    for label in ["tokens", "fill", "took", "scope", "summary", "tools"] {
        // The label sits in the table's FIRST cell, so the row reads
        // `| tokens | … |`. Matching the cell rather than a bare prefix keeps
        // this honest: `|  | … |` continuation rows start with a pipe and no
        // label, and a row that lost its label would otherwise slip through.
        let cell = format!("| {label} |");
        let hits: Vec<&str> = lines
            .iter()
            .copied()
            .filter(|l| l.starts_with(&cell))
            .collect();
        assert_eq!(hits.len(), 1, "exactly one `{label}` row: {out}");
    }

    // No row may carry another row's label inside it, which is what a collapsed
    // card looks like once the newlines are gone.
    for line in &lines {
        let labels_on_this_line = ["| tokens |", "| fill |", "| took |", "| scope |"]
            .iter()
            .filter(|cell| line.contains(*cell))
            .count();
        assert!(
            labels_on_this_line <= 1,
            "collapsed rows on one line: {line}"
        );
    }
}

#[test]
fn the_scope_row_names_which_compaction_ran() {
    // Owner order 2026-11-15: "the notice does not tell anything about those
    // delta and document summaries, and so the percentage numbers don't adapt".
    // A `DeltaSinceMarker` pass keeps the frozen segments standing and replaces
    // only the tail, so a fill that RISES after one is the scope doing its job.
    // Without the label the percentages read as a bug.
    let out = card(numbered_document());
    let scope = out
        .lines()
        .find(|l| l.starts_with("| scope |"))
        .expect("a scope row");
    assert!(scope.contains("delta"), "the scope label: {scope}");
    // `format_token_count` renders whole K below 1M (41_300 -> `41K`), so the
    // assertion states the renderer's contract rather than a guess at it: an
    // earlier draft demanded `41.3K` and failed on the gate (run 43741914995).
    assert!(scope.contains("41K in"), "what it read: {scope}");
    assert!(scope.contains("doc "), "what it wrote back: {scope}");
    assert!(scope.contains("payoff"), "the trade: {scope}");

    // The other scope must read differently, or the label is decoration.
    let rich = consolidate_card(numbered_document());
    let rich_scope = rich
        .lines()
        .find(|l| l.starts_with("| scope |"))
        .expect("a scope row");
    assert!(
        rich_scope.contains("consolidate"),
        "consolidation is labelled: {rich_scope}"
    );
    assert!(!rich_scope.contains("delta"), "{rich_scope}");
}

#[test]
fn an_unmeasured_document_states_no_payoff_ratio() {
    // A ratio needs a denominator. With no document there is nothing to divide
    // by, and a `0.0:1` would read as a measured loss rather than as absent.
    let sizes = tool_sizes();
    let out = compaction_breakdown(&CompactionBreakdown {
        before_tokens: 1_000,
        after_tokens: 500,
        before_pct: 1.0,
        after_pct: 0.5,
        ctx_max: 200_000,
        elapsed: Duration::from_secs(1),
        tool_sizes: &sizes,
        summary: "",
        truncated: false,
        scope: "delta",
        input_tokens: 41_300,
        documents: &[],
        document_refusals: &[],
        cache_notice: None,
    });
    let scope = out
        .lines()
        .find(|l| l.starts_with("| scope |"))
        .expect("a scope row");
    assert!(scope.contains('—'), "unmeasurable payoff: {scope}");
    assert!(!scope.contains("0.0:1"), "{scope}");
}

// ─── #846: the documents row ────────────────────────────────────────────────
//
// The card named the summary's parts, the kept/dropped skills and every tool's
// schema cost, while the `.md` documents the window now carries — the whole
// point of #831 — appeared nowhere. These tests pin the row, and the two states
// that must not read alike: "the manifest named no documents" and "the manifest
// named documents and every one was refused".

use crate::brain::tools::retained_docs::{DocEntry, DocStatus};
use std::path::PathBuf;

/// One measured document, as `retained_docs::resolve` reports it.
fn doc(path: &str, tokens: usize, bytes: u64, lines: usize) -> DocEntry {
    DocEntry {
        path: PathBuf::from(path),
        tokens,
        lines: Some(lines),
        bytes: Some(bytes),
        status: DocStatus::Ok,
    }
}

fn card_with_docs(documents: &[DocEntry], refusals: &[String], summary: &str) -> String {
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
        truncated: false,
        scope: "delta",
        input_tokens: 41_300,
        documents,
        document_refusals: refusals,
        cache_notice: None,
    })
}

#[test]
fn a_retained_document_states_its_size_share_and_loc() {
    // 20_480 B = 20.0 KB, 4_000 tok of 200 K = 2.0 %. Exact figures, so a
    // rounding change is caught here rather than in the chat.
    let docs = vec![doc("/root/proj/AGENTS.md", 4_000, 20_480, 312)];
    let out = card_with_docs(&docs, &[], no_manifest());
    let head = out
        .lines()
        .find(|l| l.starts_with("| docs |"))
        .expect("a docs row");
    assert!(head.contains("1 entry"), "entry count: {head}");
    assert!(head.contains("20.0KB"), "group size: {head}");
    assert!(head.contains("2.0%"), "group share: {head}");
    let detail = out
        .lines()
        .skip_while(|l| !l.starts_with("| docs |"))
        .nth(1)
        .expect("a docs detail row");
    assert!(detail.contains("`/root/proj/AGENTS.md`"), "{detail}");
    assert!(detail.contains("20.0KB"), "{detail}");
    assert!(detail.contains("2.0%"), "{detail}");
    assert!(detail.contains("312 LOC"), "{detail}");
}

#[test]
fn a_documents_path_is_never_bare() {
    // Same ccTLD trap as `.md` / `.rs`: a bare path segment can auto-link.
    let docs = vec![doc("/root/proj/AGENTS.md", 100, 512, 5)];
    let out = card_with_docs(&docs, &[], no_manifest());
    let bare = without_code_spans(&out);
    assert!(!bare.contains("AGENTS.md"), "a bare path leaked: {bare}");
}

#[test]
fn a_manifest_that_named_no_documents_adds_no_row() {
    // The pre-#846 shape, byte for byte: a session that read no documents must
    // not pay for a row saying so.
    let plain = card(no_manifest());
    let explicit = card_with_docs(&[], &[], no_manifest());
    assert_eq!(plain, explicit, "an empty document set changes nothing");
    assert!(!plain.contains("docs"), "{plain}");
    assert!(!plain.contains("refused"), "{plain}");
}

#[test]
fn every_refused_path_still_prints_a_row() {
    // The failure this exists for: the summariser names two documents, both
    // are refused (a `/tmp` log is not `.md`; an unread path was never
    // opened), and a card that prints nothing reads as "the model asked for
    // nothing" — the opposite of what happened.
    let refusals = vec![
        "not registered: /tmp/w831-marker.log (not .md)".to_string(),
        "not registered: /root/proj/GEMINI.md (not read in this process)".to_string(),
    ];
    let out = card_with_docs(&[], &refusals, no_manifest());
    let row = out
        .lines()
        .find(|l| l.starts_with("| refused |"))
        .expect("a refused row");
    assert!(row.contains("0 of 2 retained"), "count: {row}");
    assert!(row.contains("not .md"), "cause: {row}");
    assert!(row.contains("not read in this process"), "cause: {row}");
    // Distinct causes only: two paths refused for the same reason say it once.
    assert_eq!(row.matches("not .md").count(), 1, "{row}");
}

#[test]
fn a_partial_refusal_prints_both_the_rows_and_the_reason() {
    let docs = vec![doc("/root/proj/AGENTS.md", 4_000, 20_480, 312)];
    let refusals =
        vec!["not registered: /root/proj/CLAUDE.md (not read in this process)".to_string()];
    let out = card_with_docs(&docs, &refusals, no_manifest());
    assert!(
        out.lines().any(|l| l.starts_with("| docs |")),
        "a docs row beside the refusal: {out}"
    );
    let refused = out
        .lines()
        .find(|l| l.starts_with("| refused |"))
        .expect("a refused row");
    assert!(refused.contains("1 not retained"), "{refused}");
}

#[test]
fn documents_render_as_a_row_with_a_reconciling_total() {
    // The docs group carries the same shape as kept/dropped: one labelled row
    // stating the group's size and share, then the names it counts. It rides
    // the card's single table (`the_card_is_one_table`), so the total must
    // reconcile in the head rather than in a `**total**` row of its own.
    let docs = vec![
        doc("/root/proj/AGENTS.md", 4_000, 20_480, 312),
        doc("/root/proj/docs/ops.md", 1_000, 5_120, 90),
    ];
    let out = card_with_docs(&docs, &[], mixed_manifest());
    // One header for the whole card, and the docs group does NOT bring a second.
    assert_eq!(
        out.lines().filter(|l| l.starts_with("| metric |")).count(),
        1,
        "docs ride the one table: {out}"
    );
    let head = out
        .lines()
        .find(|l| l.starts_with("| docs |"))
        .expect("a docs row");
    // 25_600 B = 25.0 KB, 5_000 tok of 200 K = 2.5 % — the sum of both rows.
    assert!(head.contains("2 entries"), "{head}");
    assert!(head.contains("25.0KB"), "{head}");
    assert!(head.contains("2.5%"), "{head}");
    // The names stay: a total with no entries is a number with no subject.
    assert!(out.contains("`/root/proj/AGENTS.md`"), "{out}");
    assert!(out.contains("`/root/proj/docs/ops.md`"), "{out}");
}

// ---------------------------------------------------------------------------
// #994: the card's summary row states the OUTCOME, not a count.
//
// A cut document is not a short document. The old row printed a KB figure, a
// share and a parts count for whatever fragment arrived, which is how a
// truncated fragment came to wear the shape of a summary on this very card
// (measured 2026-11-24: an 848-byte note from a 21,904-character reasoning
// payload and zero text characters).
// ---------------------------------------------------------------------------

/// A card for a compaction whose summariser was cut at its output limit.
fn truncated_card(summary: &str) -> String {
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
        truncated: true,
        scope: "delta",
        input_tokens: 41_300,
        documents: &[],
        document_refusals: &[],
        cache_notice: None,
    })
}

#[test]
fn a_truncated_compaction_states_no_summary_and_names_the_marker() {
    // The owner-visible wording is a contract, not an implementation detail:
    // pin the literals AND the constants they come from, so renaming a constant
    // cannot silently change what the reader sees.
    assert_eq!(TRUNCATED_SUMMARY_CARD_LABEL, "no summary (truncated)");
    assert_eq!(TRUNCATED_SUMMARY_MARKER_REF, "TruncatedSummary");

    let out = truncated_card("");
    let row = out
        .lines()
        .find(|l| l.starts_with("| summary |"))
        .expect("a summary row, even with no summary");
    assert_eq!(
        row, "| summary | no summary (truncated) · marker TruncatedSummary |",
        "the truncated row is the outcome class, not a measurement"
    );
}

#[test]
fn a_truncated_card_never_counts_the_fragment_as_parts() {
    // A fragment handed to the card by an older path must not resurrect the old
    // shape: the label wins over the text, and no parts count is printed for a
    // document that was never finished.
    let out = truncated_card(numbered_document());
    let row = out
        .lines()
        .find(|l| l.starts_with("| summary |"))
        .expect("a summary row");
    assert!(row.contains("no summary (truncated)"), "{row}");
    assert!(
        !row.contains("part"),
        "a cut document has no parts to count: {row}"
    );
    assert!(
        !row.contains("KB"),
        "a cut document has no measured size to report: {row}"
    );
}

#[test]
fn a_healthy_card_row_is_byte_identical_to_the_pre_994_shape() {
    // The regression guard for the widening: `truncated: false` must render
    // exactly what the card rendered before #994 existed — the same cell order,
    // the same separators, the same trailing pipe.
    let out = card(numbered_document());
    let row = out
        .lines()
        .find(|l| l.starts_with("| summary |"))
        .expect("a summary row");
    assert_eq!(row.matches('|').count(), 3, "one row, two cells: {row}");
    assert!(
        row.ends_with(" 2 parts |"),
        "the part count closes the row: {row}"
    );
    assert!(row.contains("KB · "), "size then share: {row}");
    assert!(row.contains("% · "), "share then count: {row}");
    assert!(
        !row.contains("truncated") && !row.contains("marker"),
        "a whole document is not the truncated class: {row}"
    );
}

#[test]
fn a_truncated_card_still_reports_the_window_it_paid_for() {
    // Truncation is about the summary, not about the pass: the before/after
    // figures, the scope and the tool total are all still measured, and a reader
    // must not lose them because the document is missing.
    let out = truncated_card("");
    assert!(out.lines().any(|l| l.starts_with("| tokens |")), "{out}");
    assert!(out.lines().any(|l| l.starts_with("| scope |")), "{out}");
    assert!(out.lines().any(|l| l.starts_with("| tools |")), "{out}");
}
