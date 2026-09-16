use teloxide::types::BotCommand;

use crate::channels::telegram::agent::{
    budget_command_catalog, clean_menu_description, truncate_description,
};

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
