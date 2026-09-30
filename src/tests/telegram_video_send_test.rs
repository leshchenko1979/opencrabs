//! The outbound-video send floor (#465).
//!
//! `<<VID:path>>` was a parser with nothing behind it: the four callers that
//! extract the paths discard them, and Telegram never extracted them at all.
//! This file covers the FLOOR half of the fix — the `*_in_thread` family
//! member and the kind decision that decides which of the two send methods a
//! given file needs.
//!
//! Covered here:
//!
//! - `video_in_thread` request shape: the thread id reaches the wire when it
//!   is present, and the `None` arm adds nothing (asserted at both the wire
//!   and the source, because a matcher cannot express "this field is absent").
//! - the caption contract, inherited from `photo_in_thread` (#487/#645): a
//!   markdown title renders and ships with `parse_mode`, never as its source.
//! - the kind boundary at exactly [`TELEGRAM_VIDEO_MAX_BYTES`].
//! - the format arm (D3): a non-MPEG4 container UNDER the ceiling still takes
//!   the document arm, because the Bot API supports "MPEG4 videos (other
//!   formats may be sent as Document)". The container is sniffed from the
//!   bytes rather than the extension, so a `.mp4` name over AVI payload cannot
//!   claim the video arm.
//!
//! NOT covered here (deliberately): the call sites that consume the kind.
//! Their routing pin (`TelegramVideoKind::Document` → `document_in_thread`)
//! rides with the sites themselves, in `delivery.rs`/`send.rs`, so the test
//! asserts a call site that exists rather than one anticipated here.

use crate::channels::telegram::send::{
    TELEGRAM_VIDEO_MAX_BYTES, TelegramVideoKind, VIDEO_FORMAT_HEAD_BYTES, VideoFormat,
    sniff_video_format, telegram_video_media_kind, video_in_thread,
};
use std::path::Path;

const CHAT: i64 = 133_526_395;

/// An ISO base-media header whose major brand is MPEG-4 — the bytes an
/// `ffmpeg`/Remotion MP4 actually starts with.
const MP4_BYTES: &[u8] = b"\x00\x00\x00\x18ftypisom\x00\x00\x02\x00isomiso2avc1mp41";

/// An AVI container: `RIFF....AVI `, no `ftyp` box anywhere.
const AVI_BYTES: &[u8] = b"RIFF\x24\x00\x00\x00AVI LIST\x00\x00\x00\x00hdrlavih";

/// A QuickTime file: a real `ftyp` box, but a brand Telegram's clients do not
/// play — the case the brand list (not the box marker) exists to catch.
const MOV_BYTES: &[u8] = b"\x00\x00\x00\x14ftypqt  \x00\x00\x02\x00qt  ";

/// A response shaped like a successful `sendVideo`.
///
/// Only the REQUEST shape is under test here, so the result body mirrors the
/// proven text-message shape (`plain_outbox_image_test.rs`) rather than
/// inventing a media object: teloxide decodes the result into a `Message`, and
/// a hand-written media body would add a second failure surface that has
/// nothing to do with the pin.
const SEND_OK_MESSAGE: &str = r#"{"ok":true,"result":{"message_id":601,"date":1757166400,"chat":{"id":133526395,"type":"private"},"text":"ok"}}"#;

fn write_fixture(dir: &Path, name: &str, bytes: &[u8]) -> std::path::PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, bytes).expect("write fixture");
    path
}

fn video_fixture() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = write_fixture(dir.path(), "clip.mp4", MP4_BYTES);
    (dir, path)
}

/// A bot pointed at the mockito server.
///
/// MOCK PATHS ARE PASCALCASE FOR EVERY TELOXIDE REQUEST: teloxide builds the
/// method segment from the payload struct name, so `bot.send_video(..)` hits
/// `/botTESTTOKEN/SendVideo`. A lowercase mock never matches — mockito serves
/// its own unmatched 501, whose empty body teloxide reports as `InvalidJson`
/// and the send reads as a network failure (same trap, same explanation:
/// `plain_outbox_image_test.rs`, `governor_gates_test.rs`).
fn test_bot(server: &mockito::ServerGuard) -> teloxide::Bot {
    teloxide::Bot::with_client(
        "TESTTOKEN",
        reqwest_teloxide::Client::builder().build().unwrap(),
    )
    .set_api_url(server.url().parse().unwrap())
}

// ---------------------------------------------------------------------------
// telegram_video_media_kind — the 50 MB ceiling, and the container beside it
// ---------------------------------------------------------------------------

#[test]
fn video_ceiling_is_fifty_megabytes() {
    assert_eq!(TELEGRAM_VIDEO_MAX_BYTES, 50 * 1024 * 1024);
}

#[test]
fn an_mpeg4_video_under_the_ceiling_takes_the_video_arm() {
    assert_eq!(
        telegram_video_media_kind(1024, VideoFormat::Mpeg4),
        TelegramVideoKind::Video
    );
}

#[test]
fn exactly_the_ceiling_is_still_a_video() {
    // `sendVideo` accepts the ceiling itself; only MORE than it is refused —
    // the same inclusive bound `telegram_media_kind` uses for photos.
    assert_eq!(
        telegram_video_media_kind(TELEGRAM_VIDEO_MAX_BYTES, VideoFormat::Mpeg4),
        TelegramVideoKind::Video
    );
}

#[test]
fn one_byte_over_the_ceiling_is_a_document() {
    assert_eq!(
        telegram_video_media_kind(TELEGRAM_VIDEO_MAX_BYTES + 1, VideoFormat::Mpeg4),
        TelegramVideoKind::Document
    );
}

#[test]
fn non_mpeg4_under_the_ceiling_routes_to_the_document_arm() {
    // D3's whole point: the kind is a function of `(length, container)`, not
    // of the length. An AVI well under 50 MB still cannot ride `sendVideo`,
    // because clients play MPEG4 and anything else is only accepted as a
    // document. The pairing is the one `delivery.rs` already applies to
    // `TelegramMediaKind::Document => document_in_thread(..)`.
    assert_eq!(
        telegram_video_media_kind(1024, VideoFormat::Other),
        TelegramVideoKind::Document
    );
    assert_eq!(
        telegram_video_media_kind(3 * 1024 * 1024, VideoFormat::Other),
        TelegramVideoKind::Document
    );
}

// ---------------------------------------------------------------------------
// sniff_video_format — the container, from the bytes
// ---------------------------------------------------------------------------

#[test]
fn an_iso_mpeg4_brand_is_recognised() {
    assert_eq!(sniff_video_format(MP4_BYTES), VideoFormat::Mpeg4);
}

#[test]
fn a_brand_telegram_clients_do_not_play_is_other() {
    // QuickTime carries a real `ftyp` box, so a box-marker-only check would
    // take the video arm and settle as a bubble that never plays.
    assert_eq!(sniff_video_format(MOV_BYTES), VideoFormat::Other);
}

#[test]
fn a_container_with_no_ftyp_box_is_other() {
    assert_eq!(sniff_video_format(AVI_BYTES), VideoFormat::Other);
}

#[test]
fn an_ftyp_after_a_leading_free_box_is_still_found() {
    // Some writers emit a `free`/`wide` box before the type box, so the marker
    // is not guaranteed at offset 4 — the walk is what keeps those files out
    // of the document arm.
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"\x00\x00\x00\x10free\x00\x00\x00\x00\x00\x00\x00\x00");
    bytes.extend_from_slice(b"\x00\x00\x00\x18ftypmp42\x00\x00\x02\x00mp42isom");
    assert_eq!(sniff_video_format(&bytes), VideoFormat::Mpeg4);
}

#[test]
fn a_truncated_or_empty_head_errs_towards_document() {
    // The instrument is specified to err RESTRICTIVELY: an unreadable file
    // arrives as an un-previewable document rather than as a video that
    // Telegram's clients refuse to play.
    assert_eq!(sniff_video_format(b""), VideoFormat::Other);
    assert_eq!(sniff_video_format(b"\x00\x00\x00\x18ftyp"), VideoFormat::Other);
    assert_eq!(sniff_video_format(b"\x00\x00\x00\x18fty"), VideoFormat::Other);
}

/// Classify a file the way the delivery floor does: read the bytes, slice the
/// head, sniff the container — `sniff_video_format(&bytes[..VIDEO_FORMAT_HEAD_BYTES.min(len)])`,
/// exactly as `delivery.rs` does it.
///
/// The composition lives here rather than in `send.rs` because the floor
/// already holds the bytes it uploads, so a path-taking wrapper would have no
/// production caller — and an unused `pub fn` on the lib unit is a clippy
/// error. The instrument under test is still the production one.
fn format_of_path(path: &Path) -> VideoFormat {
    let bytes = std::fs::read(path).expect("read fixture");
    sniff_video_format(&bytes[..VIDEO_FORMAT_HEAD_BYTES.min(bytes.len())])
}

#[test]
fn the_container_is_read_from_the_bytes_not_the_name() {
    // The extension is a claim the bytes may not honour: this fixture is NAMED
    // .mp4 and carries AVI bytes, and it must NOT take the video arm.
    let dir = tempfile::tempdir().expect("tempdir");
    let honest = write_fixture(dir.path(), "clip.mp4", MP4_BYTES);
    let renamed = write_fixture(dir.path(), "liar.mp4", AVI_BYTES);

    assert_eq!(format_of_path(&honest), VideoFormat::Mpeg4);
    assert_eq!(format_of_path(&renamed), VideoFormat::Other);
}

// ---------------------------------------------------------------------------
// video_in_thread — the request shape (the `*_in_thread` family, #1079/#465)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_video_arm_carries_the_forum_topic() {
    // D4: threading is mandatory, not decorative. #1079's lesson is that an
    // arm which builds its own request lands in General — so the thread id has
    // to be ON THE WIRE, not merely accepted as a parameter.
    let (_dir, clip) = video_fixture();
    let mut server = mockito::Server::new_async().await;
    let video_mock = server
        .mock("POST", "/botTESTTOKEN/SendVideo")
        .match_body(mockito::Matcher::Regex(
            // The multipart field, then its value: `[\s\S]` spans the CRLFs a
            // plain `.` would not.
            r#"message_thread_id[\s\S]{0,64}249"#.to_string(),
        ))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(SEND_OK_MESSAGE)
        .expect(1)
        .create_async()
        .await;

    let bot = test_bot(&server);
    let sent = video_in_thread(
        &bot,
        teloxide::types::ChatId(CHAT),
        Some(teloxide::types::ThreadId(teloxide::types::MessageId(249))),
        teloxide::types::InputFile::file(clip),
        None,
    )
    .await;

    assert!(sent.is_ok(), "send should succeed: {:?}", sent.err());
    video_mock.assert_async().await;
}

#[tokio::test]
async fn the_video_arm_without_a_topic_still_sends() {
    // The `None` arm must produce a valid, sendable request. (That it adds no
    // `message_thread_id` FIELD cannot be asserted with a mockito matcher —
    // no matcher expresses absence — so `the_none_arm_adds_no_thread_field`
    // below pins it at the source, and the two together are the claim.)
    let (_dir, clip) = video_fixture();
    let mut server = mockito::Server::new_async().await;
    let video_mock = server
        .mock("POST", "/botTESTTOKEN/SendVideo")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(SEND_OK_MESSAGE)
        .expect(1)
        .create_async()
        .await;

    let bot = test_bot(&server);
    let sent = video_in_thread(
        &bot,
        teloxide::types::ChatId(CHAT),
        None,
        teloxide::types::InputFile::file(clip),
        None,
    )
    .await;

    assert!(sent.is_ok(), "send should succeed: {:?}", sent.err());
    video_mock.assert_async().await;
}

#[tokio::test]
async fn a_captioned_video_ships_the_rendered_caption_with_a_parse_mode() {
    // D5/#645: the caption is the markdown title the reference carried, and a
    // title arriving as its own source is the #487 defect.
    let (_dir, clip) = video_fixture();
    let mut server = mockito::Server::new_async().await;
    let video_mock = server
        .mock("POST", "/botTESTTOKEN/SendVideo")
        .match_body(mockito::Matcher::Regex(
            r"(parse_mode[\s\S]*<b>Caption</b>)|(<b>Caption</b>[\s\S]*parse_mode)".to_string(),
        ))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(SEND_OK_MESSAGE)
        .expect(1)
        .create_async()
        .await;

    let bot = test_bot(&server);
    let sent = video_in_thread(
        &bot,
        teloxide::types::ChatId(CHAT),
        None,
        teloxide::types::InputFile::file(clip),
        Some("**Caption**".to_string()),
    )
    .await;

    assert!(sent.is_ok(), "send should succeed: {:?}", sent.err());
    video_mock.assert_async().await;
}

#[test]
fn the_none_arm_adds_no_thread_field() {
    // The source half of the `None`-arm pin (see the test above for why the
    // wire cannot carry it). Reads the function body only, so the doc comment
    // above it — which names `message_thread_id` in prose — cannot satisfy
    // the count.
    let src = include_str!("../channels/telegram/send.rs");
    let start = src
        .find("pub fn video_in_thread")
        .expect("video_in_thread must exist");
    let rest = &src[start..];
    let end = rest.find("\n}\n").expect("the function body must close");
    let body = &rest[..end];

    assert!(
        body.contains("Some(t) => req.message_thread_id(t),"),
        "the Some arm must put the thread id on the request"
    );
    assert!(
        body.contains("None => req,"),
        "the None arm must return the request untouched"
    );
    assert_eq!(
        body.matches("message_thread_id").count(),
        1,
        "exactly one call may set the field, and it must be the Some arm — a \
         second one is how a non-forum chat gets an API error for a field that \
         should have been absent"
    );
}
