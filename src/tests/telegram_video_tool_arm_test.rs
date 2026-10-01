//! The outbound video path on the tool arm (#465).
//!
//! `send_markdown_outbox` is the fourth delivery site #465 names — the path a
//! tool result takes to the chat. It already extracted and attached the image
//! family (`resolve_remote_images(extract_local_images(…))`); the video family
//! had no arm at all, so a `<<VID:path>>` a tool echoed reached the reader as
//! literal marker text.
//!
//! The site needs a live bot, so it is pinned the way this repo pins the other
//! sites it cannot call — the COMPOSITION behaviourally, and the SITE'S OWN
//! SOURCE against `send.rs` itself.
//!
//! Two things the source pins exist to hold:
//!
//! - the video walk runs BEFORE the image walk. `record_candidate` (the image
//!   family's scan) CONSUMES a local reference whose bytes fail image
//!   validation: it records the failure and returns `true`, so the reference
//!   leaves the text. A clip is exactly that (`UnsupportedFormat`), so an
//!   image-first order eats `![clip](render.mp4)` before the video family can
//!   claim it — the clip is never delivered and the reader is told "Image not
//!   attached" instead. The video walk is the selective one: it declines a
//!   picture by content and leaves markers, picture references and remote
//!   targets verbatim, so running it first cannot starve the image family.
//! - a clip leaves through `video_in_thread`, the shared writer, rather than a
//!   hand-built request (#1079), and its kind is chosen from the BYTES.

use crate::channels::telegram::delivery::send_local_videos;
use crate::utils::{extract_local_images, extract_local_videos};
use std::path::Path;

const SEND_SRC: &str = include_str!("../channels/telegram/send.rs");

/// The chat a fixture turn is addressed to.
const CHAT: i64 = 133_526_395;

/// A response shaped like a successful send. Only the ROUTING is under test, so
/// the body mirrors the proven shape rather than inventing a media object (same
/// reasoning as `telegram_video_delivery_test.rs`).
const SEND_OK_MESSAGE: &str = r#"{"ok":true,"result":{"message_id":601,"date":1757166400,"chat":{"id":133526395,"type":"private"},"text":"ok"}}"#;

/// An ISO base-media header whose major brand is MPEG-4 — the bytes an
/// `ffmpeg`/Remotion MP4 actually starts with.
const MP4_BYTES: &[u8] = b"\x00\x00\x00\x18ftypisom\x00\x00\x02\x00isomiso2avc1mp41";

/// Minimal byte string that passes the magic-byte sniff for PNG, so the
/// positive control below is a file the IMAGE family genuinely owns.
const PNG_BYTES: &[u8] = b"\x89PNG\r\n\x1a\n\x00\x01\x02\x03\x04\x05\x06\x07";

fn fixture(dir: &Path, name: &str, bytes: &[u8]) -> std::path::PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, bytes).expect("write fixture");
    path
}

// ---------------------------------------------------------------------------
// The composition — what the tool arm's two scans do to a tool result
// ---------------------------------------------------------------------------

#[test]
fn a_local_video_marker_is_claimed_and_leaves_the_text() {
    let dir = tempfile::tempdir().expect("tempdir");
    let clip = fixture(dir.path(), "render.mp4", MP4_BYTES);
    let text = format!("uploading <<VID:{}>> now", clip.display());

    let video_scan = extract_local_videos(&text, None);
    let image_scan = extract_local_images(&video_scan.text, None);

    assert_eq!(video_scan.attachments.len(), 1, "the clip is claimed");
    assert_eq!(video_scan.attachments[0].path, clip);
    assert!(
        !image_scan.text.contains("<<VID:"),
        "no marker text survives to the reader: {:?}",
        image_scan.text
    );
    assert_eq!(image_scan.text, "uploading  now");
    assert!(
        image_scan.failures.is_empty(),
        "the image family owns nothing on this turn"
    );
}

#[test]
fn a_remote_video_marker_stays_verbatim_on_the_tool_arm() {
    // The family's declared non-goal: no remote fetch arm. Deleting a link
    // nobody replaces is the #286 loss, so the reference is left as written —
    // and a future arm must NOT inherit the 50 MB `sendVideo` ceiling.
    let text = "watch <<VID:https://example.com/clip.mp4>> please";

    let video_scan = extract_local_videos(text, None);

    assert!(
        video_scan.attachments.is_empty(),
        "no remote video fetch exists on this plane"
    );
    assert!(
        video_scan.failures.is_empty(),
        "and a remote target is not a failure either"
    );
    assert_eq!(video_scan.text, text, "the reference is left as written");
}

#[test]
fn a_markdown_clip_reference_is_lost_if_the_image_scan_walks_first() {
    // The hazard the tool arm's order exists to avoid, DEMONSTRATED rather
    // than asserted: the image walk consumes a reference whose bytes fail
    // image validation, so an image-first order leaves the video family
    // nothing to claim and the clip is never delivered.
    let dir = tempfile::tempdir().expect("tempdir");
    let clip = fixture(dir.path(), "render.mp4", MP4_BYTES);
    let text = format!("rendered ![clip]({})", clip.display());

    // Image-first — the wrong order.
    let image_first = extract_local_images(&text, None);
    assert!(
        image_first.attachments.is_empty(),
        "a clip is not a picture, so the image family cannot deliver it"
    );
    assert_eq!(
        image_first.failures.len(),
        1,
        "…but it still JUDGES it, and that verdict is the false notice"
    );
    let video_after = extract_local_videos(&image_first.text, None);
    assert!(
        video_after.attachments.is_empty(),
        "the image walk consumed the reference, so the clip is LOST — this is \
         exactly the outcome the video-first order avoids"
    );

    // Video-first — the order the site uses.
    let video_first = extract_local_videos(&text, None);
    let image_after = extract_local_images(&video_first.text, None);
    assert_eq!(video_first.attachments.len(), 1, "the clip is claimed");
    assert!(
        image_after.attachments.is_empty() && image_after.failures.is_empty(),
        "and the image family is neither starved of what it owns nor handed a \
         reference it would refuse"
    );
}

#[test]
fn a_picture_reference_is_left_to_the_image_family() {
    // The positive control for the ordering claim: the video walk is
    // SELECTIVE, so running it first cannot cost the image family a picture.
    let dir = tempfile::tempdir().expect("tempdir");
    let picture = fixture(dir.path(), "shot.png", PNG_BYTES);
    let text = format!("shot ![pic]({})", picture.display());

    let video_scan = extract_local_videos(&text, None);
    let image_scan = extract_local_images(&video_scan.text, None);

    assert!(
        video_scan.attachments.is_empty(),
        "a picture is not video by content, whatever the name says"
    );
    assert_eq!(image_scan.attachments.len(), 1, "the image family claims it");
    assert_eq!(image_scan.attachments[0].path, picture);
}

// ---------------------------------------------------------------------------
// The site's own source — the order and the writer a behavioural test cannot see
// ---------------------------------------------------------------------------

/// Where the tool arm feeds the video family the raw markdown.
const VIDEO_WALK_ON_THE_MARKDOWN: &str = "extract_local_videos(markdown, None)";

/// Where the image family is fed the video family's stripped text.
const IMAGE_WALK_ON_THE_VIDEO_TEXT: &str = "extract_local_images(\n        &video_scan.text,";

#[test]
fn the_tool_arm_walks_the_video_family_first() {
    assert_eq!(
        SEND_SRC.matches(VIDEO_WALK_ON_THE_MARKDOWN).count(),
        1,
        "the anchor must be unique, else the pin proves nothing"
    );
    assert_eq!(
        SEND_SRC.matches(IMAGE_WALK_ON_THE_VIDEO_TEXT).count(),
        1,
        "the anchor must be unique, else the pin proves nothing"
    );
    let video_at = SEND_SRC
        .find(VIDEO_WALK_ON_THE_MARKDOWN)
        .expect("the tool arm runs the video walk");
    let image_at = SEND_SRC
        .find(IMAGE_WALK_ON_THE_VIDEO_TEXT)
        .expect("the image walk runs on the video family's text");
    assert!(
        video_at < image_at,
        "video-first is load-bearing: image-first consumes a clip's markdown \
         reference and delivers nothing"
    );
}

#[test]
fn the_tool_arm_sends_clips_through_the_shared_writer() {
    // #1079: the thread routing, caption HTML and parse mode live in
    // `video_in_thread`. A hand-built `send_video` here is the second copy
    // that produced the General-topic regression.
    assert!(
        SEND_SRC.contains("TelegramVideoKind::Video => video_in_thread("),
        "the tool arm must send a clip through `video_in_thread`"
    );
    assert!(
        SEND_SRC.contains("telegram_video_media_kind(len as u64, format)"),
        "and pick its kind through the shared helper"
    );
    assert!(
        SEND_SRC.contains("sniff_video_format(&bytes[..VIDEO_FORMAT_HEAD_BYTES.min(len)])"),
        "reading the container from the BYTES, not the name"
    );
}

#[test]
fn the_tool_arm_names_the_video_family_in_its_notice() {
    assert!(
        SEND_SRC.contains("append_video_failure_notice("),
        "a missing clip must not be announced as a missing image (#465)"
    );
}

// ---------------------------------------------------------------------------
// (f) end-to-end: marker -> extraction -> floor -> wire
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_reply_carrying_a_marker_delivers_the_clip_and_leaves_no_marker_text() {
    // Leg (f) of the #465 battery. Each half is proven elsewhere — the rewrite
    // drops the marker (`telegram_video_rewrite_test`), the floor sends a clip
    // (`telegram_video_delivery_test`) — but no single test walks a REPLY
    // through both. This one does, in the tool arm's own order, so the property
    // a reader actually cares about is asserted as ONE fact: the marker never
    // reaches the delivered text AND the clip reaches the wire. Split across two
    // tests it would be satisfiable by a pair that never met.
    let dir = tempfile::tempdir().expect("tempdir");
    let clip = fixture(dir.path(), "render.mp4", MP4_BYTES);
    let reply = format!("Uploading the render now. <<VID:{}>>", clip.display());

    // The tool arm's order, exactly: video first, then the image family on the
    // video family's stripped text.
    let video_scan = extract_local_videos(&reply, None);
    let image_scan = extract_local_images(&video_scan.text, None);

    assert_eq!(video_scan.attachments.len(), 1, "the clip is claimed");
    assert!(
        !image_scan.text.contains("VID:"),
        "no marker substring may reach the delivered text: {:?}",
        image_scan.text
    );
    assert_eq!(
        image_scan.text, "Uploading the render now.",
        "the marker leaves the text with the surrounding prose intact"
    );

    // The clip must actually ARRIVE, not merely be extracted: drive the floor
    // against a mock server and assert the request lands.
    let mut server = mockito::Server::new_async().await;
    let video_mock = server
        .mock("POST", "/botTESTTOKEN/SendVideo")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(SEND_OK_MESSAGE)
        .expect(1)
        .create_async()
        .await;

    let bot = teloxide::Bot::with_client(
        "TESTTOKEN",
        reqwest_teloxide::Client::builder().build().unwrap(),
    )
    .set_api_url(server.url().parse().unwrap());

    let (delivered, failures) = send_local_videos(
        uuid::Uuid::new_v4(),
        &bot,
        teloxide::types::ChatId(CHAT),
        None,
        &video_scan.attachments,
    )
    .await;

    video_mock.assert_async().await;
    assert_eq!(delivered, vec![clip], "the clip arrived on the wire");
    assert!(failures.is_empty(), "and nothing failed: {:?}", failures);
}
