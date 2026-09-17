//! Tests verifying canonical rich markdown normalization (#280).
//!
//! Confirms that named entities (&rarr;, &bull;, etc.) are decoded to Unicode glyphs,
//! code fences are balanced, tables are reflowed with proper separators and blank lines,
//! bare leading hashes are shielded, and button layout fits across all rich builders:
//! `build_body_target`, `build_body_markdown_media_target`, `build_body_markdown_media_edit`,
//! and `normalize_rich_markdown`.

use crate::channels::telegram::rich::api::{
    build_body_markdown_media_edit, build_body_markdown_media_target, build_body_target,
};
use crate::channels::telegram::rich::mermaid::MediaEntry;
use crate::channels::telegram::rich::normalize_rich_markdown;
use teloxide::types::{MessageId, ThreadId};

#[test]
fn test_normalize_rich_markdown_named_entities() {
    let input = "Latency: 120ms &rarr; 45ms &bull; Status: OK &mdash; Verified";
    let output = normalize_rich_markdown(input);
    assert_eq!(output, "Latency: 120ms → 45ms • Status: OK — Verified");
}

#[test]
fn test_normalize_rich_markdown_table_reflow_and_entities() {
    let input = "Summary: | Metric | Value | |---|---| | Latency | 50ms &rarr; 20ms |";
    let output = normalize_rich_markdown(input);

    // Entity decoded
    assert!(output.contains("50ms → 20ms"));
    // Table has blank line preceding it and inferred separator
    assert!(output.contains("Summary:\n\n| Metric | Value |"));
    assert!(output.contains("|---|---|"));
}

#[test]
fn test_normalize_rich_markdown_fence_balance_and_shield_hashes() {
    let input = "```text\n# Bare heading inside code fence\n```\n#174 Bare issue outside fence";
    let output = normalize_rich_markdown(input);

    assert!(output.contains("```text\n# Bare heading inside code fence\n```"));
    assert!(output.contains("\\#174 Bare issue outside fence"));
}

#[test]
fn test_normalize_rich_markdown_button_fit() {
    // Multi-button row where one button label exceeds SINGLE_BUTTON_MAX_UNITS (30)
    // collapses into numbered text + pick button.
    let input = "<tg-button-row>\
<tg-button data=\"b1\">One extremely long button label exceeding thirty units</tg-button>\
<tg-button data=\"b2\">Two 123456</tg-button>\
</tg-button-row>";
    let output = normalize_rich_markdown(input);
    assert!(output.contains("<li>One extremely long button label exceeding thirty units</li>"));
    assert!(output.contains("<li>Two 123456</li>"));
}

#[test]
fn test_build_body_target_normalizes_markdown() {
    let raw_md = "Status: &bull; #280 &rarr; Complete\n\n| A | B |\n| 1 | 2 |";
    let body = build_body_target(12345, Some(ThreadId(MessageId(99))), Some(42), raw_md);

    assert_eq!(body["chat_id"], 12345);
    assert_eq!(body["message_thread_id"], 99);
    assert_eq!(body["reply_parameters"]["message_id"], 42);

    let md = body["rich_message"]["markdown"].as_str().unwrap();
    assert!(md.contains("• #280 → Complete"));
    assert!(md.contains("|---|---|"));
}

#[test]
fn test_build_body_markdown_media_target_normalizes_markdown() {
    let raw_md = "Result &rarr; Success\n\n| X | Y |\n| a | b |";
    let media = vec![MediaEntry {
        id: "diag1".to_string(),
        url: Some("https://example.com/diag.png".to_string()),
        bytes: None,
    }];
    let body = build_body_markdown_media_target(54321, None, None, raw_md, &media, None);

    assert_eq!(body["chat_id"], 54321);
    let md = body["rich_message"]["markdown"].as_str().unwrap();
    assert!(md.contains("Result → Success"));
    assert!(md.contains("|---|---|"));
    assert_eq!(body["rich_message"]["media"][0]["id"], "diag1");
}

#[test]
fn test_build_body_markdown_media_edit_normalizes_markdown() {
    let raw_md = "Edited &bull; #280 &rarr; Done";
    let media = vec![];
    let body = build_body_markdown_media_edit(98765, 555, raw_md, &media);

    assert_eq!(body["chat_id"], 98765);
    assert_eq!(body["message_id"], 555);
    let md = body["rich_message"]["markdown"].as_str().unwrap();
    assert_eq!(md, "Edited • #280 → Done");
}

#[test]
fn test_shield_unresolvable_markdown_images() {
    // Unresolvable local/relative paths -> shielded
    let input = "Here is an image: ![Visual Diagram](tmp/plot.png) and ![Ref](/root/arch.svg)";
    let output = normalize_rich_markdown(input);
    assert_eq!(
        output,
        "Here is an image: \\![Visual Diagram](tmp/plot.png) and \\![Ref](/root/arch.svg)"
    );

    // Valid http, https, tg://photo?id=, attach:// -> preserved
    let valid_input = "Remote: ![Web](https://example.com/pic.png) and ![HTTP](http://test.org/a.jpg)\n\
                       Telegram: ![Diag](tg://photo?id=diag0) and ![Attach](attach://photo1)";
    let valid_output = normalize_rich_markdown(valid_input);
    assert_eq!(valid_output, valid_input);

    // Code blocks & inline code -> untouched
    let code_input =
        "```markdown\n![Visual Diagram](tmp/plot.png)\n```\nInline: `![Alt](path.png)`";
    let code_output = normalize_rich_markdown(code_input);
    assert_eq!(code_output, code_input);

    // Already escaped -> not double-escaped
    let escaped_input = "Already: \\![Manual](local/file.png)";
    let escaped_output = normalize_rich_markdown(escaped_input);
    assert_eq!(escaped_output, escaped_input);
}
