//! Tests for caption and reply_parameters support in `send_photo` / `send_document`
//! actions of the telegram_send tool (#257).
//!
//! Since the actual Telegram Bot API calls require a live connection, these tests
//! verify the schema contract and the graceful error path when the bot is not
//! connected. The parameter-extraction patterns are also exercised directly.

use crate::brain::tools::telegram_send::TelegramSendTool;
use crate::brain::tools::r#trait::Tool;
use crate::brain::tools::r#trait::ToolExecutionContext;
use crate::channels::telegram::TelegramState;
use serde_json::json;
use std::sync::Arc;
use uuid::Uuid;

/// Helper: build a `TelegramSendTool` with a disconnected state.
fn make_tool() -> TelegramSendTool {
    let state = Arc::new(TelegramState::new());
    TelegramSendTool::new(state)
}

// ── Schema contract tests ──────────────────────────────────────────────

#[test]
fn schema_has_caption_property() {
    let tool = make_tool();
    let schema = tool.input_schema();
    let caption = schema.pointer("/properties/caption");
    assert!(
        caption.is_some(),
        "input_schema must include a 'caption' property"
    );
    let desc = caption
        .unwrap()
        .get("description")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    assert!(
        desc.contains("send_photo") && desc.contains("send_document"),
        "caption description should mention send_photo and send_document, got: {desc}"
    );
}

#[test]
fn schema_message_id_mentions_media_actions() {
    let tool = make_tool();
    let schema = tool.input_schema();
    let desc = schema
        .pointer("/properties/message_id/description")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    assert!(
        desc.contains("send_photo") && desc.contains("send_document"),
        "message_id description should mention send_photo/send_document, got: {desc}"
    );
}

// ── Graceful error path (no bot connected) ─────────────────────────────

#[tokio::test]
async fn send_photo_without_bot_returns_error() {
    let tool = make_tool();
    let ctx = ToolExecutionContext::new(Uuid::new_v4());
    let result = tool
        .execute(
            json!({"action": "send_photo", "photo_url": "https://example.com/cat.png"}),
            &ctx,
        )
        .await
        .unwrap();
    assert!(!result.success, "should fail when bot not connected");
    let err = result.error.unwrap_or_default();
    assert!(
        err.contains("not connected"),
        "error should mention not connected, got: {err}"
    );
}

#[tokio::test]
async fn send_document_without_bot_returns_error() {
    let tool = make_tool();
    let ctx = ToolExecutionContext::new(Uuid::new_v4());
    let result = tool
        .execute(
            json!({"action": "send_document", "document_url": "https://example.com/doc.pdf"}),
            &ctx,
        )
        .await
        .unwrap();
    assert!(!result.success, "should fail when bot not connected");
    let err = result.error.unwrap_or_default();
    assert!(
        err.contains("not connected"),
        "error should mention not connected, got: {err}"
    );
}

#[tokio::test]
async fn send_photo_with_caption_without_bot_returns_error() {
    let tool = make_tool();
    let ctx = ToolExecutionContext::new(Uuid::new_v4());
    let result = tool
        .execute(
            json!({
                "action": "send_photo",
                "photo_url": "https://example.com/cat.png",
                "caption": "A cute cat"
            }),
            &ctx,
        )
        .await
        .unwrap();
    assert!(!result.success, "should fail when bot not connected");
}

#[tokio::test]
async fn send_document_with_caption_and_reply_without_bot_returns_error() {
    let tool = make_tool();
    let ctx = ToolExecutionContext::new(Uuid::new_v4());
    let result = tool
        .execute(
            json!({
                "action": "send_document",
                "document_url": "/tmp/report.pdf",
                "caption": "Monthly report",
                "message_id": 42
            }),
            &ctx,
        )
        .await
        .unwrap();
    assert!(!result.success, "should fail when bot not connected");
}

// ── Parameter extraction pattern tests ─────────────────────────────────

#[test]
fn caption_extraction_from_json() {
    // Mirrors the extraction pattern in send_photo/send_document:
    //   input.get("caption").and_then(|v| v.as_str())
    let input = json!({"caption": "Hello world"});
    let caption = input.get("caption").and_then(|v| v.as_str());
    assert_eq!(caption, Some("Hello world"));

    let input_no_caption = json!({});
    assert_eq!(
        input_no_caption.get("caption").and_then(|v| v.as_str()),
        None
    );

    let input_null = json!({"caption": null});
    assert_eq!(input_null.get("caption").and_then(|v| v.as_str()), None);

    let input_number = json!({"caption": 123});
    assert_eq!(
        input_number.get("caption").and_then(|v| v.as_str()),
        None,
        "non-string caption should be ignored"
    );
}

#[test]
fn message_id_extraction_from_json() {
    // Mirrors: input.get("message_id").and_then(|v| v.as_i64())
    let input = json!({"message_id": 42});
    let mid = input.get("message_id").and_then(|v| v.as_i64());
    assert_eq!(mid, Some(42));

    let input_missing = json!({});
    assert_eq!(
        input_missing.get("message_id").and_then(|v| v.as_i64()),
        None
    );

    let input_string = json!({"message_id": "42"});
    assert_eq!(
        input_string.get("message_id").and_then(|v| v.as_i64()),
        None,
        "string message_id should be ignored (must be integer)"
    );
}

#[test]
fn empty_caption_string_is_treated_as_present() {
    // An empty string is still Some("") from as_str(). The Telegram API
    // accepts 0-1024 chars, so an empty caption is valid (removes previous
    // caption on forwarded messages). The code does NOT skip empty captions.
    let input = json!({"caption": ""});
    let caption = input.get("caption").and_then(|v| v.as_str());
    assert_eq!(caption, Some(""));
}

// ── Caption RENDERING (#645) ───────────────────────────────────────────
//
// A caption is content like any other, but it was the one surface that declared
// no parse mode: every classic text send passes `ParseMode::Html`, while the
// caption arms passed nothing — so markdown written into a caption arrived
// literally. Measured live before the fix: msg 82136 carried `**bold**` and a
// pipe table verbatim as the caption of a `send_document` upload.
//
// `caption_html` (src/channels/telegram/send.rs) is the shared renderer both
// media legs now route through. Its WIRE-level half — that the request really
// declares the parse mode — is asserted in `plain_outbox_image_test.rs`, where
// a mock server sees the multipart body.

/// A caption renders markdown the way a message body does. The expectations
/// below are the ones already pinned for the classic text path in
/// `telegram_resume_test.rs` (`markdown_to_html_bold` and siblings).
#[test]
fn caption_renders_markdown_like_a_message_body() {
    assert!(crate::channels::telegram::send::caption_html("**Bold** title").contains("<b>Bold</b>"));
    assert!(crate::channels::telegram::send::caption_html("Use `foo()` here")
        .contains("<code>foo()</code>"));
    assert!(crate::channels::telegram::send::caption_html("Click [here](https://example.com)")
        .contains("<a href=\"https://example.com\">here</a>"));
}

/// Model-written text must reach Telegram escaped, never as markup.
#[test]
fn caption_escapes_html_entities() {
    let html = crate::channels::telegram::send::caption_html("a < b & c > d");
    assert!(html.contains("&lt;"), "{html}");
    assert!(html.contains("&amp;"), "{html}");
    assert!(html.contains("&gt;"), "{html}");
}

/// Telegram's classic HTML dialect has no paragraph element, so a `<p>` or
/// `<br>` in a caption is a parse error rather than a layout choice. This is
/// what fixes the renderer for the surface: the classic converter in
/// `markdown.rs` emits neither tag anywhere in its body (the `<p>`/`<br>`
/// variants belong to the rich dialect, which captions do not ride).
#[test]
fn caption_carries_no_paragraph_or_break_tag() {
    for input in ["first para\n\nsecond para", "a soft\nbreak", "# Heading"] {
        let html = crate::channels::telegram::send::caption_html(input);
        assert!(!html.contains("<p>"), "<p> in caption for {input:?}: {html}");
        assert!(
            !html.contains("<br"),
            "<br> in caption for {input:?}: {html}"
        );
    }
}
