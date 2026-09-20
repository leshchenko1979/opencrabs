//! Markdown image extraction on the plain outbox ladder (#368).
//!
//! `send_markdown_outbox` is the single wire path for every PROACTIVE
//! Telegram writer — the `telegram_send` tool, a cron `deliver_to`, and the
//! crash-recovery resume. Its rich arm hands the body to Telegram's own
//! `sendRichMessage`, which resolves a remote markdown image SERVER-side
//! (PROBE 5, msg 68618) — but the HTML fallback leg carried no extraction at
//! all, so a plain-ladder body delivered `![alt](https://…/x.png)` as dead
//! text (PROBE 3, msg 68332, `path=html_chunk`, zero media).
//!
//! The contract under test:
//! 1. on the fallback leg the reference LEAVES the body and the image ships
//!    as native media through the same helpers the turn path uses;
//! 2. a reference inside a code span is not delivery input and stays put;
//! 3. a rich-shaped body is NOT extracted here — the rich plane already
//!    inlined the image, and extracting as well would send it twice (#360);
//! 4. a reference that cannot be delivered reaches the user as the honest
//!    `⚠️ Image not attached` notice rather than vanishing;
//! 5. a body that was ONLY an image reference sends the image and no empty
//!    chunk (an empty text message is a 400 from Telegram);
//! 6. both reference forms reach the same outcome — a LOCAL path and a
//!    REMOTE url, the latter fetched over real HTTP through
//!    `resolve_remote_images`.

use teloxide::types::ChatId;

const CHAT: i64 = 133_526_395;

/// Minimal byte blob that passes the PNG magic-byte gate.
const PNG_BYTES: &[u8] = b"\x89PNG\r\n\x1a\n\x00\x01\x02\x03\x04\x05\x06\x07";

const SEND_MESSAGE_OK: &str = r#"{"ok":true,"result":{"message_id":501,"date":1757166400,"chat":{"id":133526395,"type":"private"},"text":"ok"}}"#;
const SEND_PHOTO_OK: &str = r#"{"ok":true,"result":{"message_id":502,"date":1757166400,"chat":{"id":133526395,"type":"private"},"photo":[{"file_id":"AgAC","file_unique_id":"AQAD","width":1,"height":1}]}}"#;
const SEND_RICH_OK: &str = r#"{"ok":true,"result":{"message_id":503,"date":1757166400,"chat":{"id":133526395,"type":"private"},"text":"ok"}}"#;

/// Pin the process-wide config mirror: the rich-arm choice reads
/// `Config::current()`, and parallel tests in this binary leave the mirror in
/// arbitrary states. The guard serializes the swap against the other tests
/// that mutate the same mirror (same pattern as `stale_topic_eviction_test`).
fn pin_rich(on: bool) {
    let mut pinned: crate::config::Config =
        toml::from_str(include_str!("../../config.toml.example"))
            .expect("embedded config.toml.example must parse");
    pinned.channels.telegram.rich_messages = on;
    crate::config::Config::set_current(pinned);
}

fn test_bot(server: &mockito::ServerGuard) -> teloxide::Bot {
    teloxide::Bot::with_client(
        "TESTTOKEN",
        reqwest_teloxide::Client::builder().build().unwrap(),
    )
    .set_api_url(server.url().parse().unwrap())
}

/// A real PNG on disk, so the reference resolves through the LOCAL path of the
/// extraction layer and the test never depends on the network.
fn png_fixture() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let png = dir.path().join("chart.png");
    std::fs::write(&png, PNG_BYTES).expect("write fixture");
    (dir, png)
}

#[tokio::test]
async fn the_fallback_leg_ships_a_local_image_and_strips_the_reference() {
    use crate::channels::telegram::send::send_markdown_outbox;

    let _guard = crate::channels::telegram::governor::test_support::registry_guard().await;
    pin_rich(true);
    let (_dir, png) = png_fixture();

    let mut server = mockito::Server::new_async().await;
    let text_mock = server
        .mock("POST", "/botTESTTOKEN/sendMessage")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(SEND_MESSAGE_OK)
        .expect(1)
        .create_async()
        .await;
    let photo_mock = server
        .mock("POST", "/botTESTTOKEN/sendPhoto")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(SEND_PHOTO_OK)
        .expect(1)
        .create_async()
        .await;

    let bot = test_bot(&server);
    // Plain prose: no table, heading, list or fence, so the rich arm declines
    // and the body rides the HTML fallback leg under test.
    let body = format!("see ![chart]({}) for details", png.display());
    let outbox = send_markdown_outbox(&bot, ChatId(CHAT), None, &body, "tool", "send", None)
        .await
        .expect("the fallback leg must deliver the message");

    assert_eq!(outbox.sent.len(), 2, "one text chunk plus one image message");
    assert!(
        !outbox.sent[0].1.contains("![chart]"),
        "the reference must not survive as dead text: {}",
        outbox.sent[0].1
    );
    assert!(outbox.sent[0].1.contains("see"), "the prose survives");
    assert!(
        outbox.sent[1].1.starts_with("[image] "),
        "the image message is recorded for reply recovery: {}",
        outbox.sent[1].1
    );

    text_mock.assert_async().await;
    photo_mock.assert_async().await;
}

#[tokio::test]
async fn a_reference_inside_a_code_span_is_not_delivery_input() {
    use crate::channels::telegram::send::send_markdown_outbox;

    let _guard = crate::channels::telegram::governor::test_support::registry_guard().await;
    pin_rich(true);
    let (_dir, png) = png_fixture();

    let mut server = mockito::Server::new_async().await;
    let text_mock = server
        .mock("POST", "/botTESTTOKEN/sendMessage")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(SEND_MESSAGE_OK)
        .expect(1)
        .create_async()
        .await;
    let no_photo = server
        .mock("POST", "/botTESTTOKEN/sendPhoto")
        .expect(0)
        .create_async()
        .await;

    let bot = test_bot(&server);
    let body = format!("the syntax is `![chart]({})`", png.display());
    let outbox = send_markdown_outbox(&bot, ChatId(CHAT), None, &body, "tool", "send", None)
        .await
        .expect("a code span must not break delivery");

    assert_eq!(outbox.sent.len(), 1, "text only — nothing to attach");
    assert!(
        outbox.sent[0].1.contains("![chart]"),
        "a code span is documentation, not delivery input: {}",
        outbox.sent[0].1
    );

    text_mock.assert_async().await;
    no_photo.assert_async().await;
}

#[tokio::test]
async fn a_rich_shaped_body_is_not_extracted_on_the_rich_plane() {
    use crate::channels::telegram::send::send_markdown_outbox;

    let _guard = crate::channels::telegram::governor::test_support::registry_guard().await;
    pin_rich(true);
    let (_dir, png) = png_fixture();

    let mut server = mockito::Server::new_async().await;
    let rich_mock = server
        .mock("POST", "/botTESTTOKEN/sendRichMessage")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(SEND_RICH_OK)
        .expect(1)
        .create_async()
        .await;
    // The double-send guard (#360): the rich plane resolves the reference
    // server-side, so attaching it here as well would deliver it twice.
    let no_photo = server
        .mock("POST", "/botTESTTOKEN/sendPhoto")
        .expect(0)
        .create_async()
        .await;
    let no_text = server
        .mock("POST", "/botTESTTOKEN/sendMessage")
        .expect(0)
        .create_async()
        .await;

    let bot = test_bot(&server);
    let body = format!("| a | b |\n|---|---|\n| 1 | ![chart]({}) |", png.display());
    let outbox = send_markdown_outbox(&bot, ChatId(CHAT), None, &body, "tool", "send", None)
        .await
        .expect("the rich plane must deliver the table");

    assert_eq!(outbox.sent.len(), 1);
    assert!(
        outbox.sent[0].1.contains("![chart]"),
        "the rich body is handed over verbatim: {}",
        outbox.sent[0].1
    );

    rich_mock.assert_async().await;
    no_photo.assert_async().await;
    no_text.assert_async().await;
}

#[tokio::test]
async fn an_undeliverable_reference_reaches_the_user_as_a_notice() {
    use crate::channels::telegram::send::send_markdown_outbox;

    let _guard = crate::channels::telegram::governor::test_support::registry_guard().await;
    pin_rich(true);
    let (dir, _png) = png_fixture();
    let missing = dir.path().join("gone.png");

    let mut server = mockito::Server::new_async().await;
    let text_mock = server
        .mock("POST", "/botTESTTOKEN/sendMessage")
        .match_body(mockito::Matcher::Regex("Image not attached".to_string()))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(SEND_MESSAGE_OK)
        .expect(1)
        .create_async()
        .await;
    let no_photo = server
        .mock("POST", "/botTESTTOKEN/sendPhoto")
        .expect(0)
        .create_async()
        .await;

    let bot = test_bot(&server);
    let body = format!("the chart: ![chart]({})", missing.display());
    let outbox = send_markdown_outbox(&bot, ChatId(CHAT), None, &body, "tool", "send", None)
        .await
        .expect("an undeliverable reference must not fail the send");

    assert_eq!(outbox.sent.len(), 1);
    assert!(
        outbox.sent[0].1.contains("Image not attached"),
        "a reference that could not be delivered must say so: {}",
        outbox.sent[0].1
    );
    assert!(
        !outbox.sent[0].1.contains("![chart]"),
        "dead markdown never reaches the user: {}",
        outbox.sent[0].1
    );

    text_mock.assert_async().await;
    no_photo.assert_async().await;
}

#[tokio::test]
async fn an_image_only_body_sends_the_image_and_no_empty_chunk() {
    use crate::channels::telegram::send::send_markdown_outbox;

    let _guard = crate::channels::telegram::governor::test_support::registry_guard().await;
    pin_rich(true);
    let (_dir, png) = png_fixture();

    let mut server = mockito::Server::new_async().await;
    let photo_mock = server
        .mock("POST", "/botTESTTOKEN/sendPhoto")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(SEND_PHOTO_OK)
        .expect(1)
        .create_async()
        .await;
    // Stripping the reference leaves an empty body, and Telegram rejects an
    // empty text message with a 400 — the ladder must be skipped entirely.
    let no_text = server
        .mock("POST", "/botTESTTOKEN/sendMessage")
        .expect(0)
        .create_async()
        .await;

    let bot = test_bot(&server);
    let body = format!("![chart]({})", png.display());
    let outbox = send_markdown_outbox(&bot, ChatId(CHAT), None, &body, "tool", "send", None)
        .await
        .expect("the image alone must still deliver");

    assert_eq!(outbox.sent.len(), 1, "the image is the whole message");
    assert!(outbox.sent[0].1.starts_with("[image] "));

    photo_mock.assert_async().await;
    no_text.assert_async().await;
}

/// AC2 of the #368 checklist names the REMOTE form explicitly: the reference
/// is `![alt](https://…/x.png)`, not a local path. The image host is the same
/// mock server that answers the Bot API, so the fetch is real HTTP through
/// `resolve_remote_images` and still hermetic — no network, no fixture on
/// disk for the input side.
#[tokio::test]
async fn the_fallback_leg_fetches_a_remote_reference_and_attaches_it() {
    use crate::channels::telegram::send::send_markdown_outbox;

    let _guard = crate::channels::telegram::governor::test_support::registry_guard().await;
    pin_rich(true);

    let mut server = mockito::Server::new_async().await;
    let image_mock = server
        .mock("GET", "/chart.png")
        .with_status(200)
        .with_header("content-type", "image/png")
        .with_body(PNG_BYTES)
        .expect(1)
        .create_async()
        .await;
    let text_mock = server
        .mock("POST", "/botTESTTOKEN/sendMessage")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(SEND_MESSAGE_OK)
        .expect(1)
        .create_async()
        .await;
    let photo_mock = server
        .mock("POST", "/botTESTTOKEN/sendPhoto")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(SEND_PHOTO_OK)
        .expect(1)
        .create_async()
        .await;

    let bot = test_bot(&server);
    let url = format!("{}/chart.png", server.url());
    let body = format!("see ![chart]({url}) for details");
    let outbox = send_markdown_outbox(&bot, ChatId(CHAT), None, &body, "tool", "send", None)
        .await
        .expect("the fallback leg must deliver the message");

    assert_eq!(outbox.sent.len(), 2, "one text chunk plus one fetched image");
    assert!(
        !outbox.sent[0].1.contains("![chart]"),
        "a remote reference must not survive as dead text: {}",
        outbox.sent[0].1
    );
    assert!(
        !outbox.sent[0].1.contains(&url),
        "the bare URL is not delivery input either: {}",
        outbox.sent[0].1
    );
    assert!(
        outbox.sent[1].1.starts_with("[image] "),
        "the fetched image is recorded for reply recovery: {}",
        outbox.sent[1].1
    );

    image_mock.assert_async().await;
    text_mock.assert_async().await;
    photo_mock.assert_async().await;
}
