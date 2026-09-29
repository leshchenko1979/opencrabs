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
use crate::channels::telegram::rich::mermaid::{MediaEntry, MediaKind};
use crate::channels::telegram::rich::normalize_rich_markdown;
use crate::channels::telegram::rich::{
    normalize_rich_markdown_with_media, table::shield_unresolvable_markdown_images,
};
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
        kind: MediaKind::Photo,
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

    // Remote URLs are valid by scheme alone -> preserved
    let valid_input =
        "Remote: ![Web](https://example.com/pic.png) and ![HTTP](http://test.org/a.jpg)";
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

/// #334 (H1): a `tg://` / `attach://` reference is judged against THIS request's
/// media array, never by scheme alone. `normalize_rich_markdown` is the media-free
/// entry, so every such reference there is an orphan by construction — which is
/// exactly the `RICH_MESSAGE_PHOTO_NO_MEDIA_FOUND` class this closes.
#[test]
fn test_shield_tg_and_attach_refs_are_media_aware() {
    // No media array at all -> neither ref can resolve. This runs the FULL shared
    // entry, so BOTH guards fire: the shield escapes the `!` (so the image cannot
    // render) and the orphan neutralizer drops the `//` (so Telegram cannot try to
    // resolve it). That is why the expected string carries `tg:photo?id=` and
    // `attach:photo1` rather than the raw schemes — asserting the pre-neutralizer
    // form here would pin a mid-pipeline state and fail on the real output.
    let orphan_input = "Diag: ![diagram](tg://photo?id=diag0) and ![a](attach://photo1)";
    let orphan_output = normalize_rich_markdown(orphan_input);
    assert_eq!(
        orphan_output, "Diag: \\![diagram](tg:photo?id=diag0) and \\![a](attach:photo1)",
        "with an empty media array every tg/attach ref is an orphan"
    );

    // Non-matching media array -> still an orphan.
    let other_media = vec![MediaEntry {
        kind: MediaKind::Photo,
        id: "diag9".to_string(),
        url: None,
        bytes: Some(vec![1, 2, 3]),
    }];
    assert_eq!(
        shield_unresolvable_markdown_images("![d](tg://photo?id=diag0)", &other_media),
        "\\![d](tg://photo?id=diag0)",
        "a ref naming an id absent from media must be escaped"
    );

    // Matching media array -> preserved, so a real diagram still renders.
    let matching_media = vec![MediaEntry {
        kind: MediaKind::Photo,
        id: "diag1".to_string(),
        url: None,
        bytes: Some(vec![1, 2, 3]),
    }];
    assert_eq!(
        shield_unresolvable_markdown_images("![d](tg://photo?id=diag1)", &matching_media),
        "![d](tg://photo?id=diag1)",
        "a ref whose id IS in media must keep resolving"
    );
    assert_eq!(
        shield_unresolvable_markdown_images("![d](attach://diag1)", &matching_media),
        "![d](attach://diag1)",
        "attach:// resolves through the same media array"
    );

    // Scheme alone never carries a tg/attach ref — the id decides.
    assert_eq!(
        shield_unresolvable_markdown_images("![d](tg://photo?id=diag1)", &[]),
        "\\![d](tg://photo?id=diag1)"
    );
}

/// #334 (step 9): the markdown image shield is fence- and code-span-safe. A
/// fenced or backticked reference is a LITERAL region — escaping the `!` there
/// would rewrite text the user asked to see verbatim — so those regions pass
/// through byte-identically while the same ref in prose is escaped.
#[test]
fn test_shield_is_fence_and_code_span_safe_for_media_refs() {
    let prose = "![d](tg://photo?id=absent)";
    assert_eq!(
        shield_unresolvable_markdown_images(prose, &[]),
        "\\![d](tg://photo?id=absent)",
        "an orphan ref in prose is escaped"
    );

    let fenced = "```text\n![d](tg://photo?id=absent)\n```";
    assert_eq!(
        shield_unresolvable_markdown_images(fenced, &[]),
        fenced,
        "a fenced ref is literal text and must survive byte-identically"
    );

    let span = "Inline: `![d](tg://photo?id=absent)`";
    assert_eq!(
        shield_unresolvable_markdown_images(span, &[]),
        span,
        "an inline code span is literal text and must survive byte-identically"
    );

    // A matching entry keeps the ref live even in prose.
    let matching = vec![MediaEntry {
        kind: MediaKind::Photo,
        id: "absent".to_string(),
        url: None,
        bytes: Some(vec![1, 2, 3]),
    }];
    assert_eq!(shield_unresolvable_markdown_images(prose, &matching), prose);
}

// ---------------------------------------------------------------------------
// #487 — a local image's caption rides the markdown title, and the title only
// reaches Telegram when the reference stands alone on its own line.
// ---------------------------------------------------------------------------

fn photo_entry(id: &str) -> MediaEntry {
    MediaEntry {
        kind: MediaKind::Photo,
        id: id.to_string(),
        url: None,
        bytes: Some(vec![0x89, b'P', b'N', b'G']),
    }
}

#[test]
fn a_titled_reference_sharing_its_line_is_split_so_the_caption_survives() {
    // Telegram's rich parser drops the caption of an INLINE media reference
    // (measured 2026-09-27: inline → no caption, own line → caption, with or
    // without a blank line before it). The title is a local image's only caption
    // channel on this plane, so the reference has to move to keep the caption.
    let media = vec![photo_entry("img0")];
    let out = normalize_rich_markdown_with_media(
        "Here is the chart: ![alt](tg://photo?id=img0 \"Quarterly revenue\") enjoy.",
        &media,
    );

    let line = out
        .lines()
        .find(|l| l.contains("tg://photo?id=img0"))
        .expect("the reference survives the normalizer");
    assert_eq!(
        line.trim(),
        "![alt](tg://photo?id=img0 \"Quarterly revenue\")",
        "the reference stands alone on its line, title intact"
    );
    assert!(out.contains("Here is the chart:"));
    assert!(out.contains("enjoy."));
}

#[test]
fn an_untitled_reference_keeps_its_inline_placement() {
    // Nothing to gain, so nothing moves: splitting an untitled reference would
    // cost the inline placement #360 asks this plane for.
    let media = vec![photo_entry("img0")];
    let input = "Look here ![alt](tg://photo?id=img0) and then read on.";
    let out = normalize_rich_markdown_with_media(input, &media);

    assert!(
        out.contains("Look here ![alt](tg://photo?id=img0) and then read on."),
        "an untitled reference stays inline: {out:?}"
    );
}

#[test]
fn a_markdown_title_does_not_make_its_reference_look_like_an_orphan() {
    // The bug this pins: the target parser used to read the whole
    // `tg://photo?id=img0 "Quarterly revenue"` run as the target, so the id came
    // out as `img0 "Quarterly revenue"`, matched no entry, and the shield escaped
    // the `!` — which renders the image as literal text.
    let media = vec![photo_entry("img0")];
    let input = "![alt](tg://photo?id=img0 \"Quarterly revenue\")";

    assert_eq!(
        shield_unresolvable_markdown_images(input, &media),
        input,
        "a titled reference whose id matches is not an orphan"
    );
    let out = normalize_rich_markdown_with_media(input, &media);
    assert!(
        out.starts_with("![alt]("),
        "the `!` is not escaped, so this stays an image and not dead text: {out:?}"
    );
}

#[test]
fn two_titled_references_on_one_line_each_get_their_own() {
    let media = vec![photo_entry("img0"), photo_entry("img1")];
    let out = normalize_rich_markdown_with_media(
        "![a](tg://photo?id=img0 \"First\") and ![b](tg://photo?id=img1 \"Second\")",
        &media,
    );

    let refs: Vec<&str> = out
        .lines()
        .map(str::trim)
        .filter(|l| l.starts_with("!["))
        .collect();
    assert_eq!(
        refs,
        vec![
            "![a](tg://photo?id=img0 \"First\")",
            "![b](tg://photo?id=img1 \"Second\")",
        ],
        "each reference is alone on its line, in order"
    );
}

#[test]
fn a_titled_remote_image_is_split_too() {
    // The caption rule belongs to Telegram's parser, not to the local-image
    // path, so a fetchable URL gets the same treatment.
    let out = normalize_rich_markdown_with_media(
        "See ![chart](https://example.test/a.png \"Quarterly revenue\") here.",
        &[],
    );
    assert!(
        out.contains("![chart](https://example.test/a.png \"Quarterly revenue\")\n"),
        "the titled remote ref is alone on its line: {out:?}"
    );
}

#[test]
fn a_title_never_moves_a_reference_out_of_a_table_row_or_a_code_fence() {
    let media = vec![photo_entry("img0")];

    let table = "| col | other |\n|---|---|\n| a | ![x](tg://photo?id=img0 \"C\") |";
    let out = normalize_rich_markdown_with_media(table, &media);
    assert!(
        out.contains("| a | ![x](tg://photo?id=img0 \"C\") |")
            || out.contains("![x](tg://photo?id=img0 \"C\") |"),
        "splitting a table row would break the table: {out:?}"
    );

    let fenced = "```\nsee ![x](tg://photo?id=img0 \"C\") here\n```";
    assert_eq!(
        normalize_rich_markdown_with_media(fenced, &media),
        fenced,
        "a fenced block is literal text and survives byte-identically"
    );
}

#[test]
fn an_orphan_titled_reference_is_still_neutralised() {
    // The split only claims references that resolve against THIS request; an
    // orphan is the guards' business and must still be escaped, not uploaded.
    let out = normalize_rich_markdown_with_media(
        "see ![x](tg://photo?id=missing \"C\") here",
        &[photo_entry("img0")],
    );
    assert!(
        out.contains("\\![x]("),
        "an orphan keeps its escaped `!` rather than becoming a split image line: {out:?}"
    );
    assert!(
        !out.contains("\n![x]("),
        "an orphan must not be given a line of its own — the escape is what saves it: {out:?}"
    );
}
