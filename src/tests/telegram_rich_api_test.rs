//! Regression tests for the rich API client parameterised `api_url` (#1088).
//!
//! Verifies that the4 public functions in `crate::channels::telegram::rich::api`
//! route through a caller-supplied base URL instead of hardcoding
//! `api.telegram.org`. Uses `mockito` to intercept the HTTP call and confirm
//! the constructed endpoint is hit.

use crate::channels::telegram::rich::api;

#[tokio::test]
async fn send_rich_markdown_id_uses_custom_api_url() {
    let mut server = mockito::Server::new_async().await;
    let mock = server
        .mock("POST", "/botTESTTOKEN/sendRichMessage")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"ok":true,"result":{"message_id":42}}"#)
        .create_async()
        .await;

    let result = api::send_rich_markdown_id(
        &server.url(),
        "TESTTOKEN",
        12345,
        None,
        "hello **world**",
        None,
        "test",
        "-",
    )
    .await;

    assert!(result.is_ok(), "send should succeed: {:?}", result.err());
    assert_eq!(result.unwrap(), 42);
    mock.assert_async().await;
}

#[tokio::test]
async fn send_rich_html_id_uses_custom_api_url() {
    let mut server = mockito::Server::new_async().await;
    let mock = server
        .mock("POST", "/botTESTTOKEN/sendRichMessage")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"ok":true,"result":{"message_id":99}}"#)
        .create_async()
        .await;

    let result = api::send_rich_html_id(
        &server.url(),
        "TESTTOKEN",
        67890,
        None,
        "<b>bold</b>",
        None,
        "test",
        "-",
    )
    .await;

    assert!(result.is_ok(), "send should succeed: {:?}", result.err());
    assert_eq!(result.unwrap(), 99);
    mock.assert_async().await;
}

#[tokio::test]
async fn edit_rich_html_uses_custom_api_url() {
    let mut server = mockito::Server::new_async().await;
    let mock = server
        .mock("POST", "/botTESTTOKEN/editMessageText")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"ok":true,"result":true}"#)
        .create_async()
        .await;

    let result = api::edit_rich_html(
        &server.url(),
        "TESTTOKEN",
        12345,
        1,
        "<b>edited</b>",
        None,
        "test",
        "-",
        crate::channels::telegram::governor::EditClass::Final,
    )
    .await;

    assert!(result.is_ok(), "edit should succeed: {:?}", result.err());
    mock.assert_async().await;
}

#[tokio::test]
async fn edit_rich_markdown_media_url_entry_uses_custom_api_url() {
    let mut server = mockito::Server::new_async().await;
    let mock = server
        .mock("POST", "/botTESTTOKEN/editMessageText")
        .match_body(mockito::Matcher::PartialJson(serde_json::json!({
            "chat_id": 12345,
            "message_id": 7,
            "rich_message": {
                "markdown": "![diagram](tg://photo?id=diag0)",
                "media": [
                    {"id": "diag0", "media": {"type": "photo", "media": "https://mermaid.ink/img/abc"}}
                ]
            }
        })))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"ok":true,"result":true}"#)
        .create_async()
        .await;

    let kb = serde_json::json!({"inline_keyboard": [[{"text": "b", "callback_data": "cb0"}]]});
    let result = api::edit_rich_markdown_media(
        &server.url(),
        "TESTTOKEN",
        12345,
        7,
        "![diagram](tg://photo?id=diag0)",
        &[crate::channels::telegram::rich::mermaid::MediaEntry {
            kind: crate::channels::telegram::rich::mermaid::MediaKind::Photo,
            id: "diag0".to_string(),
            url: Some("https://mermaid.ink/img/abc".to_string()),
            bytes: None,
        }],
        Some(&kb),
        "test",
        "-",
    )
    .await;

    assert!(
        result.is_ok(),
        "media edit should succeed: {:?}",
        result.err()
    );
    mock.assert_async().await;
}

#[tokio::test]
async fn send_rich_markdown_media_target_id_uses_custom_api_url() {
    let mut server = mockito::Server::new_async().await;
    let mock = server
        .mock("POST", "/botTESTTOKEN/sendRichMessage")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"ok":true,"result":{"message_id":77}}"#)
        .create_async()
        .await;

    let result = api::send_rich_markdown_media_target_id(
        &server.url(),
        "TESTTOKEN",
        11111,
        None,
        None,
        "![img](tg://photo?id=1)",
        &[crate::channels::telegram::rich::mermaid::MediaEntry {
            kind: crate::channels::telegram::rich::mermaid::MediaKind::Photo,
            id: "1".to_string(),
            url: Some("https://example.com/img.png".to_string()),
            bytes: None,
        }],
        None,
        "test",
        "-",
    )
    .await;

    assert!(
        result.is_ok(),
        "media send should succeed: {:?}",
        result.err()
    );
    assert_eq!(result.unwrap(), 77);
    mock.assert_async().await;
}

// ── Base-URL normalisation (#1117) ───────────────────────────────────
//
// The tests above pass `mockito::Server::url()`, which has no trailing
// slash. Production passes `Bot::api_url().as_str()`, and the URL spec
// normalises an empty path to `/`, so that value DOES end in one. String
// concatenation then produced `https://api.telegram.org//bot<token>/method`,
// Telegram rejected it, and every rich send fell back to plain HTML — tool
// blocks stopped rendering rich and completions arrived as separate
// messages. These pin the shape production actually uses.

#[tokio::test]
async fn a_base_with_a_trailing_slash_does_not_double_the_separator() {
    let mut server = mockito::Server::new_async().await;
    // Exactly what `Bot::api_url().as_str()` yields: a trailing slash.
    let base_with_slash = format!("{}/", server.url());

    let hit = server
        .mock("POST", "/botTOKEN/sendRichMessage")
        .with_status(200)
        .with_body(r#"{"ok":true,"result":{"message_id":1}}"#)
        .create_async()
        .await;

    let _ = api::send_rich_html_id(
        &base_with_slash,
        "TOKEN",
        123,
        None,
        "<b>hi</b>",
        None,
        "test",
        "-",
    )
    .await;

    // Asserts the single-slash path. A double slash would miss this mock.
    hit.assert_async().await;
}

#[tokio::test]
async fn a_base_without_a_trailing_slash_still_works() {
    // The mockito shape, kept so trimming cannot regress the other direction.
    let mut server = mockito::Server::new_async().await;
    let hit = server
        .mock("POST", "/botTOKEN/sendRichMessage")
        .with_status(200)
        .with_body(r#"{"ok":true,"result":{"message_id":1}}"#)
        .create_async()
        .await;

    let _ = api::send_rich_html_id(
        &server.url(),
        "TOKEN",
        123,
        None,
        "<b>hi</b>",
        None,
        "test",
        "-",
    )
    .await;

    hit.assert_async().await;
}

/// #629 — the HTML fallback must render with the paragraph-wrapping variant.
///
/// The rich HTML dialect treats a bare newline as INSIGNIFICANT whitespace, so
/// a fallback rendered by the bare `render_html` joins every block with a bare
/// newline and the whole reply arrives as one wall of text. This drives the
/// real fallback leg end to end: the primary markdown+media send is failed by
/// the mock, and the assertion is on the body the fallback actually sent.
///
/// Two things about the setup are load-bearing:
///
/// * `local_media` is deliberately NON-EMPTY. `send_rich_with_media_target_id`
///   early-returns to the no-media path when nothing is embeddable, and that
///   path has no HTML fallback at all — a test written without media would
///   pass while exercising nothing.
/// * The primary mock returns **400**, not 429: `post_rich` retries only 429,
///   so a 400 reports the failure on the first attempt and the `expect(1)` on
///   each mock holds.
#[tokio::test]
async fn the_html_fallback_wraps_each_block_in_its_own_p_tag() {
    let mut server = mockito::Server::new_async().await;

    // Primary leg: identified by its `media` array, which the HTML body never
    // carries. 400 (not 429) so `post_rich` reports it without retrying.
    let primary = server
        .mock("POST", "/botTESTTOKEN/sendRichMessage")
        .match_body(mockito::Matcher::Regex(r#""media"\s*:\s*\["#.to_string()))
        .with_status(400)
        .with_body(r#"{"ok":false,"description":"Bad Request: can't parse rich message"}"#)
        .expect(1)
        .create_async()
        .await;

    // Fallback leg: its html body must carry one <p> per block.
    let fallback = server
        .mock("POST", "/botTESTTOKEN/sendRichMessage")
        .match_body(mockito::Matcher::Regex(r#"<p>First paragraph\.</p>"#.to_string()))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"ok":true,"result":{"message_id":7}}"#)
        .expect(1)
        .create_async()
        .await;

    let media = [crate::channels::telegram::rich::mermaid::MediaEntry {
        kind: crate::channels::telegram::rich::mermaid::MediaKind::Photo,
        id: "img0".to_string(),
        url: Some("https://example.test/img0.png".to_string()),
        bytes: None,
    }];

    let id = api::send_rich_with_media_target_id(
        &server.url(),
        "TESTTOKEN",
        12345,
        None,
        None,
        "First paragraph.\n\nSecond paragraph.",
        &media,
        "test",
        "-",
    )
    .await;

    assert_eq!(
        id.expect("the html fallback must deliver the message"),
        7,
        "the fallback leg must return its own message id"
    );
    // `expect(1)` on the fallback mock is the guard against the no-media early
    // return: if that return is taken the fallback is never called and this
    // assertion fails rather than the test passing vacuously.
    fallback.assert_async().await;
    primary.assert_async().await;
}

/// #676 - a 429 refusal line must name the message it was editing, and a
/// send (no `message_id` in the body) must still report a constant shape.
#[test]
fn refusal_line_carries_the_target_message_id() {
    let edit = serde_json::json!({
        "chat_id": -100123,
        "message_id": 84439,
        "rich_message": { "html": "x" },
    });
    assert_eq!(api::target_message_id(&edit), "84439");

    let send = serde_json::json!({
        "chat_id": -100123,
        "rich_message": { "html": "x" },
    });
    assert_eq!(
        api::target_message_id(&send),
        "-",
        "a sendRichMessage body carries no message_id; the field stays present \
         so the line shape is constant"
    );
}
