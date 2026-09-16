//! Regression tests for the Telegram command-menu payload budget (#1613).
//!
//! Telegram silently rejects `setMyCommands` bodies over ~7.8KB with a
//! misleading `BOT_COMMANDS_TOO_MUCH` (empirically bisected: 7750B OK,
//! 7876B rejected). The real 64-command catalog serialized to 8505B and
//! every publish failed from 2026-09-08, freezing the server-side menu
//! without `/clear`. `trim_catalog_to_budget` keeps the catalog under a
//! 7000-byte serialized budget by shortening descriptions in tiers and only
//! then dropping tail commands.

use crate::channels::telegram::agent::{
    budget_command_catalog,
    clean_menu_description,
    collect_command_catalog,
    trim_catalog_to_budget,
    truncate_description,
};
use teloxide::types::BotCommand;

fn synthetic(n: usize, desc_len: usize) -> Vec<BotCommand> {
    (0..n)
        .map(|i| {
            BotCommand::new(
                format!("zprobe{:02}", i),
                format!("d{}", "x".repeat(desc_len)),
            )
        })
        .collect()
}

fn serialized_len(cmds: &[BotCommand]) -> usize {
    serde_json::to_string(cmds).unwrap().len()
}

#[test]
fn budget_shrinks_descriptions_and_keeps_everything_when_possible() {
    // 64 x 256-char descriptions is the #1613 shape: doesn't fit at any
    // single long tier, but fits once descriptions are shortened.
    let cmds = synthetic(64, 256);
    let out = trim_catalog_to_budget(cmds);
    assert_eq!(out.len(), 64, "all commands should survive via tiering");
    assert!(
        serialized_len(&out) <= 7000,
        "catalog must fit the 7000-byte budget, got {}",
        serialized_len(&out)
    );
    assert!(
        out.iter().all(|c| c.description.chars().count() <= 96),
        "descriptions must be tier-capped"
    );
}

#[test]
fn budget_drops_tail_only_after_all_tiers_exhausted() {
    // 150 commands cannot fit even at 32-char descriptions: tail commands
    // must be dropped, but the head (built-ins are pushed first) survives.
    let cmds = synthetic(150, 256);
    let out = trim_catalog_to_budget(cmds);
    assert!(serialized_len(&out) <= 7000);
    assert!(out.len() < 150, "oversized catalog must shed tail commands");
    assert_eq!(out[0].command, "zprobe00", "head of catalog must survive");
}

#[test]
fn budget_never_returns_empty() {
    let cmds = synthetic(1, 256);
    let out = trim_catalog_to_budget(cmds);
    assert_eq!(out.len(), 1);
}

#[test]
fn budget_preserves_catalog_order() {
    let cmds = synthetic(80, 200);
    let out = trim_catalog_to_budget(cmds);
    let names: Vec<&str> = out.iter().map(|c| c.command.as_str()).collect();
    let mut sorted = names.clone();
    sorted.sort_unstable();
    assert_eq!(names, sorted, "trimming must not reorder the catalog");
}

#[test]
fn budget_shrinks_descriptions_before_dropping() {
    // 64 x 96-char descriptions fit at the 48 tier without dropping anyone;
    // verify the tier pass alone suffices (no command loss).
    let cmds = synthetic(64, 96);
    let out = trim_catalog_to_budget(cmds);
    assert_eq!(out.len(), 64, "tiering alone must absorb this size");
}

/// The #1613 regression: the REAL catalog (built-ins + commands.toml +
/// skills from the live brain) must serialize under budget so every publish
/// succeeds and `/clear` stays on the server-side menu.
#[test]
fn real_catalog_fits_telegram_payload_budget() {
    let cat = collect_command_catalog();
    assert!(!cat.is_empty());
    assert!(
        serialized_len(&cat) <= 7000,
        "live catalog is {} bytes, budget is 7000 — publishes will fail with \
         BOT_COMMANDS_TOO_MUCH again",
        serialized_len(&cat)
    );
    assert!(
        cat.iter().any(|c| c.command == "clear"),
        "/clear must stay in the published menu"
    );
}

// ---------------------------------------------------------------------------
// Fork (#267): menu-payload helpers — first-sentence description cleaning, an
// adaptive character budget, and the emergency retry path used when Telegram
// still answers BOT_COMMANDS_TOO_MUCH after the byte-budget trim above.
// ---------------------------------------------------------------------------
#[test]
fn clean_menu_description_extracts_first_sentence_and_trims() {
    let raw = "> This is the first sentence. This is the second sentence.";
    assert_eq!(clean_menu_description(raw), "This is the first sentence");

    let raw_bullet = "- Short bullet description.";
    assert_eq!(
        clean_menu_description(raw_bullet),
        "Short bullet description"
    );

    let raw_multiline = "First line here.\nSecond line here.";
    assert_eq!(clean_menu_description(raw_multiline), "First line here");

    let raw_no_dot = "Single clause with no punctuation";
    assert_eq!(
        clean_menu_description(raw_no_dot),
        "Single clause with no punctuation"
    );

    let empty = "   ";
    assert_eq!(clean_menu_description(empty), "Custom command");
}

#[test]
fn budget_command_catalog_deduplicates_and_caps() {
    let commands = vec![
        BotCommand::new("help", "First help description"),
        BotCommand::new("help", "Duplicate help description"),
        BotCommand::new("doctor", "Health check description"),
    ];

    let budgeted = budget_command_catalog(commands, 4800);
    assert_eq!(budgeted.len(), 2);
    assert_eq!(budgeted[0].command, "help");
    assert_eq!(budgeted[1].command, "doctor");
}

#[test]
fn budget_command_catalog_scales_down_when_overflowing_budget() {
    // Create 50 commands with long descriptions
    let mut commands = Vec::new();
    for i in 0..50 {
        commands.push(BotCommand::new(
            format!("cmd_{i}"),
            "A very detailed and verbose description that would normally take up a lot of characters in Telegram's menu payload."
        ));
    }

    // Set budget to 1200 characters total
    let budgeted = budget_command_catalog(commands, 1200);
    let total_chars: usize = budgeted
        .iter()
        .map(|c| c.command.chars().count() + c.description.chars().count())
        .sum();

    assert!(
        total_chars <= 1250,
        "Total chars should be within budget window, got {total_chars}"
    );

    for cmd in &budgeted {
        assert!(!cmd.description.is_empty());
        assert!(cmd.description.chars().count() <= 30);
    }
}

#[test]
fn truncate_description_appends_ellipsis() {
    let text = "abcdefghij";
    assert_eq!(truncate_description(text, 10), "abcdefghij");
    assert_eq!(truncate_description(text, 5), "abcd…");
    assert_eq!(truncate_description(text, 1), "…");
}
