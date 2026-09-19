//! Receipt legs of the goal evidence pack (#364).
//!
//! Before #364 the pack carried only background tasks, open plan tasks and the
//! turn budget, while `JUDGE_SYSTEM` promised "tool results, background task
//! states, plan task states, files touched". The judge was reasoning correctly
//! from a pack that could not carry what it was told to expect, so every action
//! criterion came back `NO_EVIDENCE`.
//!
//! These tests pin the three things that keep that from recurring:
//!
//! 1. the receipts and files legs render, and the section list is the single
//!    contract the prompt quotes (`GoalEvidence::SECTIONS`);
//! 2. the pack stays bounded — a fifty-command turn must not inflate the judge
//!    call, and the omitted count is stated rather than swallowed;
//! 3. the `Write `/`Edit ` prefixes `files_touched_from` depends on are the ones
//!    `format_tool_summary` actually produces, so a rename there fails here
//!    instead of silently emptying the FILES TOUCHED section.

use crate::brain::agent::service::AgentService;
use crate::brain::goal::evidence::{
    GoalEvidence, MAX_FILES_TOUCHED, MAX_RECEIPT_BYTES, MAX_TOOL_RECEIPTS, files_touched_from,
    merge_files_touched, receipt_line,
};
use crate::brain::goal::judge::JUDGE_SYSTEM;
use serde_json::json;

// ---------------------------------------------------------------------------
// The receipt line
// ---------------------------------------------------------------------------

/// Success is the tool's own verdict and must be visible in the pack: a FAILED
/// receipt is what lets the judge mark a criterion UNMET rather than merely
/// unevidenced.
#[test]
fn receipt_line_carries_the_tool_verdict() {
    assert_eq!(receipt_line("bash: git status", true), "bash: git status — ok");
    assert_eq!(
        receipt_line("bash: cargo test", false),
        "bash: cargo test — FAILED"
    );
}

/// A failed receipt must not be confusable with a successful one — the judge
/// reads these lines, not a colour.
#[test]
fn failed_receipt_is_distinguishable_from_ok() {
    let ok = receipt_line("Edit ~/a.md", true);
    let failed = receipt_line("Edit ~/a.md", false);
    assert_ne!(ok, failed);
    assert!(failed.contains("FAILED"));
    assert!(!ok.contains("FAILED"));
}

// ---------------------------------------------------------------------------
// Files touched
// ---------------------------------------------------------------------------

/// Only writes and edits count as touching a file; a read is not a change and
/// must not appear — the judge uses this leg to see what the turn PRODUCED.
#[test]
fn files_touched_picks_writes_and_edits_only() {
    let descriptions = vec![
        "Read ~/a.md".to_string(),
        "Write ~/b.md".to_string(),
        "Edit ~/c.rs".to_string(),
        "bash: ls".to_string(),
        "Glob **/*.rs".to_string(),
        "ls ~/tmp".to_string(),
    ];
    assert_eq!(
        files_touched_from(&descriptions),
        vec!["~/b.md".to_string(), "~/c.rs".to_string()]
    );
}

/// A file edited five times is one touched file, and call order is preserved.
#[test]
fn files_touched_dedupes_preserving_call_order() {
    let descriptions = vec![
        "Write ~/b.md".to_string(),
        "Edit ~/a.rs".to_string(),
        "Edit ~/b.md".to_string(),
        "Edit ~/a.rs".to_string(),
    ];
    assert_eq!(
        files_touched_from(&descriptions),
        vec!["~/b.md".to_string(), "~/a.rs".to_string()]
    );
}

/// The loop collects per iteration, so the accumulator must merge across them
/// without repeating a file edited in two iterations.
#[test]
fn merge_files_touched_accumulates_across_iterations() {
    let mut acc: Vec<String> = Vec::new();
    merge_files_touched(&mut acc, &["Write ~/a.md".to_string()]);
    merge_files_touched(&mut acc, &["Edit ~/a.md".to_string(), "Edit ~/b.rs".to_string()]);
    merge_files_touched(&mut acc, &["Edit ~/b.rs".to_string()]);
    assert_eq!(acc, vec!["~/a.md".to_string(), "~/b.rs".to_string()]);
}

/// A read-only turn touches nothing, and the section says so rather than
/// rendering an empty gap the judge would read as unknown.
#[test]
fn read_only_turn_renders_an_empty_files_section() {
    let evidence = GoalEvidence {
        tool_receipts: vec![receipt_line("Read ~/a.md", true)],
        ..GoalEvidence::default()
    };
    assert!(evidence.files_touched.is_empty());
    let rendered = evidence.render();
    assert!(rendered.contains("FILES TOUCHED:\n- (none)"));
    assert!(rendered.contains("TOOL RECEIPTS:\n- Read ~/a.md — ok"));
}

// ---------------------------------------------------------------------------
// The producer contract
// ---------------------------------------------------------------------------

/// `files_touched_from` strips the `Write `/`Edit ` prefixes that
/// `format_tool_summary` is the single producer of. If that function changes
/// shape, this test fails here instead of the FILES TOUCHED section silently
/// going empty in production.
#[test]
fn tool_summary_produces_the_prefixes_files_touched_strips() {
    let written = AgentService::format_tool_summary("write_file", &json!({"path": "/tmp/a.md"}));
    let edited = AgentService::format_tool_summary("edit_file", &json!({"path": "/tmp/b.rs"}));
    let read = AgentService::format_tool_summary("read_file", &json!({"path": "/tmp/c.md"}));

    assert_eq!(written, "Write /tmp/a.md");
    assert_eq!(edited, "Edit /tmp/b.rs");

    // And the collector reads exactly those strings.
    let collected = files_touched_from(&[written.clone(), edited.clone(), read.clone()]);
    assert_eq!(collected, vec!["/tmp/a.md".to_string(), "/tmp/b.rs".to_string()]);
}

// ---------------------------------------------------------------------------
// Bounds
// ---------------------------------------------------------------------------

/// The pack goes into every judge call, so a long turn must not inflate it —
/// and the pack must not pretend it is complete: the dropped count is stated.
#[test]
fn receipts_are_capped_and_the_overflow_is_declared() {
    let total = MAX_TOOL_RECEIPTS + 160;
    let evidence = GoalEvidence {
        tool_receipts: (0..total).map(|i| format!("bash: step{i}")).collect(),
        ..GoalEvidence::default()
    };
    let rendered = evidence.render();

    let section = rendered
        .split("TOOL RECEIPTS:\n")
        .nth(1)
        .and_then(|rest| rest.split("FILES TOUCHED:").next())
        .expect("receipts section present");
    let lines: Vec<&str> = section.lines().filter(|l| l.starts_with("- ")).collect();

    // One overflow line plus the cap itself — never the whole 200.
    assert_eq!(lines.len(), MAX_TOOL_RECEIPTS + 1);
    assert!(lines[0].contains("160 earlier receipt(s) omitted"));
    // The MOST RECENT are kept: the first rendered receipt is the one just
    // after the omitted prefix, and the last is the newest.
    assert_eq!(lines[1], format!("- bash: step{}", total - MAX_TOOL_RECEIPTS));
    assert_eq!(lines[lines.len() - 1], format!("- bash: step{}", total - 1));
    // The omitted prefix is genuinely absent, not merely summarised.
    assert!(!rendered.contains("- bash: step0\n"));
    assert!(!rendered.contains(&format!("- bash: step{}\n", total - MAX_TOOL_RECEIPTS - 1)));
}

/// A short turn renders no overflow line at all — the cap is not a banner.
#[test]
fn receipts_under_the_cap_are_all_rendered() {
    let evidence = GoalEvidence {
        tool_receipts: (0..MAX_TOOL_RECEIPTS).map(|i| format!("bash: step{i}")).collect(),
        ..GoalEvidence::default()
    };
    let rendered = evidence.render();
    assert!(!rendered.contains("omitted"));
    assert!(rendered.contains("- bash: step0"));
    assert!(rendered.contains(&format!("- bash: step{}", MAX_TOOL_RECEIPTS - 1)));
}

/// One long receipt must not dominate the prompt, and the cut has to land on a
/// char boundary — a command label carrying Cyrillic or emoji is normal here.
#[test]
fn long_receipt_lines_are_truncated_on_a_char_boundary() {
    let ascii = "a".repeat(MAX_RECEIPT_BYTES + 100);
    let evidence = GoalEvidence {
        tool_receipts: vec![ascii.clone()],
        ..GoalEvidence::default()
    };
    let rendered = evidence.render();
    assert!(rendered.contains(&format!("- {}{}", &ascii[..MAX_RECEIPT_BYTES], "…")));
    assert!(!rendered.contains(&ascii));

    // Multi-byte: the cut must back off to a boundary rather than panic or
    // split a char. 2 bytes per char, so the prefix is at most the byte cap.
    let multibyte = "é".repeat(MAX_RECEIPT_BYTES);
    let evidence = GoalEvidence {
        tool_receipts: vec![multibyte.clone()],
        ..GoalEvidence::default()
    };
    let rendered = evidence.render();
    let receipt_line = rendered
        .split("TOOL RECEIPTS:\n")
        .nth(1)
        .and_then(|rest| rest.split("FILES TOUCHED:").next())
        .and_then(|section| section.lines().find(|l| l.starts_with("- ")))
        .expect("a receipt line");
    let prefix = receipt_line.trim_start_matches("- ").trim_end_matches('…');
    assert!(receipt_line.ends_with('…'), "long multibyte line not capped");
    assert!(
        multibyte.starts_with(prefix),
        "the cut must be a prefix of the original, not a split char"
    );
    assert!(prefix.len() <= MAX_RECEIPT_BYTES);
}

/// The files leg carries the same byte cap as the receipts leg.
#[test]
fn files_touched_are_capped_too() {
    let total = MAX_FILES_TOUCHED + 5;
    let evidence = GoalEvidence {
        files_touched: (0..total).map(|i| format!("~/f{i}.md")).collect(),
        ..GoalEvidence::default()
    };
    let rendered = evidence.render();
    assert!(rendered.contains("5 earlier file(s) omitted"));
}

// ---------------------------------------------------------------------------
// The prompt contract
// ---------------------------------------------------------------------------

/// The prompt must name exactly the sections the pack renders. This is the
/// test that would have caught #364: the old prompt advertised tool results and
/// files touched while the pack rendered neither.
#[test]
fn judge_prompt_names_every_rendered_section() {
    for section in GoalEvidence::SECTIONS {
        // The prompt names sections by their bare title, without the colon.
        let title = section.trim_end_matches(':');
        assert!(
            JUDGE_SYSTEM.contains(title),
            "JUDGE_SYSTEM does not name the rendered section {section}"
        );
    }
}

/// The old false promise is gone: the prompt no longer claims legs the pack
/// does not carry.
#[test]
fn judge_prompt_drops_the_stale_leg_list() {
    assert!(
        !JUDGE_SYSTEM.contains("tool results, background task states, plan task states"),
        "JUDGE_SYSTEM still advertises the pre-#364 leg list"
    );
}

/// The pack is turn-scoped, and the prompt must say so — otherwise the judge
/// reads an absent receipt as proof the work never happened.
#[test]
fn judge_prompt_states_the_pack_is_turn_scoped() {
    assert!(JUDGE_SYSTEM.contains("THIS TURN ONLY"));
}
