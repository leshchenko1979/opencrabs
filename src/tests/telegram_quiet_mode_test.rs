//! Quiet mode for groups (#679).
//!
//! Two effects, both scoped to an opting group: mid-turn intermediates stop
//! opening their own bubbles, and the previous turn's answer is folded in place
//! when the next turn starts. This file pins the second half — the fold
//! wrappers, the fold guard, the summary line, and the per-group config key.

use std::collections::HashMap;

use crate::channels::telegram::quiet::{fold_html, fold_markdown, should_fold, summary_line};
use crate::config::types::{TelegramConfig, TelegramGroupConfig};

// ── the fold wrappers ──────────────────────────────────────────────────────

#[test]
fn html_fold_uses_the_blockquote_expandable_primitive() {
    // The SAME primitive the flow block has shipped on since #451, so the
    // feature adds no new rendering surface — a folded answer collapses on
    // exactly the clients the processing log already collapses on.
    let folded = fold_html("<b>Answer</b>\n\nbody");
    assert!(folded.starts_with("<blockquote expandable>"), "{folded}");
    assert!(folded.ends_with("</blockquote>"), "{folded}");
    assert!(folded.contains("<b>Answer</b>"), "body preserved: {folded}");
}

#[test]
fn markdown_fold_uses_details_and_folds_closed() {
    let folded = fold_markdown("# Heading\n\nthe body\n\nmore");
    assert!(folded.starts_with("<details>"), "opener: {folded}");
    assert!(
        !folded.contains("<details open>"),
        "a folded answer must start COLLAPSED: {folded}"
    );
    assert!(folded.ends_with("</details>"), "closer: {folded}");
    assert!(
        folded.contains("<summary>Heading</summary>"),
        "summary carries the first line: {folded}"
    );
}

#[test]
fn markdown_fold_keeps_a_blank_line_before_the_closer() {
    // #552: a body ending on a `>` quote run would let a lazy continuation
    // swallow the closer, leaving the element unmatched — Telegram then rejects
    // the whole message with RICH_MESSAGE_CONTENT_REQUIRED. The blank line
    // terminates the quote run.
    let folded = fold_markdown("> a quoted close\n> more quote");
    assert!(
        folded.contains("\n\n</details>"),
        "closer must not directly follow body text: {folded}"
    );
}

// ── the fold guard ─────────────────────────────────────────────────────────

#[test]
fn one_line_answers_are_never_folded() {
    // Owner scope for #679: fold only messages OVER one line.
    assert!(!should_fold("Done."));
    assert!(!should_fold("Done.\n"));
    // Trailing blank lines do not make an answer multi-line to a READER.
    assert!(!should_fold("Done.\n\n"));
}

#[test]
fn multi_line_answers_are_folded() {
    assert!(should_fold("First line\nsecond line"));
    assert!(should_fold("Result.\n\nA second paragraph."));
}

// ── the summary line ───────────────────────────────────────────────────────

#[test]
fn summary_line_shores_block_markers_off_the_first_line() {
    assert_eq!(summary_line("## Reported result"), "Reported result");
    assert_eq!(summary_line("* bulleted start"), "bulleted start");
    assert_eq!(summary_line("> quoted start"), "quoted start");
}

#[test]
fn summary_line_skips_leading_blank_lines() {
    assert_eq!(
        summary_line("\n\n## Real first line\nbody"),
        "Real first line"
    );
}

#[test]
fn summary_line_is_capped_to_one_visible_line() {
    let long = "x".repeat(200);
    let s = summary_line(&long);
    assert_eq!(s.chars().count(), 61, "60 chars + the ellipsis: {s}");
    assert!(s.ends_with('…'), "{s}");
}

#[test]
fn summary_line_falls_back_when_there_is_no_text() {
    assert_eq!(summary_line("   \n\n"), "Answer");
    assert_eq!(summary_line("###"), "Answer");
}

// ── the per-group config key ───────────────────────────────────────────────

fn config_with_quiet_group(ids: &[&str]) -> TelegramConfig {
    let mut groups = HashMap::new();
    for id in ids {
        groups.insert(
            (*id).to_string(),
            TelegramGroupConfig {
                quiet: true,
                ..Default::default()
            },
        );
    }
    TelegramConfig {
        groups,
        ..Default::default()
    }
}

#[test]
fn quiet_is_off_by_default() {
    let c = config_with_quiet_group(&[]);
    assert!(!c.is_quiet_for("-100123"), "unknown chat is never quiet");
}

#[test]
fn quiet_applies_only_to_the_group_that_set_it() {
    let c = config_with_quiet_group(&["-100123"]);
    assert!(c.is_quiet_for("-100123"));
    assert!(!c.is_quiet_for("-100999"), "other groups untouched");
}

#[test]
fn quiet_parses_from_toml() {
    // The surface a human actually writes: proves the key deserializes under
    // the real config path rather than only through a struct literal.
    let c: crate::config::Config = toml::from_str(
        "[channels.telegram]\nenabled = true\n\n\
         [channels.telegram.groups.\"-100123\"]\nquiet = true\n",
    )
    .expect("group quiet key parses");
    assert!(c.channels.telegram.is_quiet_for("-100123"));
    assert!(!c.channels.telegram.is_quiet_for("-100999"));
}
