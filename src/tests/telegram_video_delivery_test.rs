//! The outbound video DELIVERY layer (#465).
//!
//! `send.rs` carries the floor's request shape and kind decision
//! (`telegram_video_send_test.rs`); this file covers what the delivery layer
//! does with the result of it:
//!
//! - the failure-entry contract: a video the channel could not deliver comes
//!   back as an entry the caller can name, never as a silent drop. The two
//!   classes are distinct — a path that cannot be READ is a reference problem
//!   (`Unreadable`), while a file the API refuses is a delivery problem
//!   (`DeliveryFailed`), and the notice wording differs by class.
//! - the no-duplicate property (D6): the rich plane owning a clip must
//!   suppress exactly that family's floor and nothing else. `#360` was this
//!   bug one media type over (an attachment shipped twice, once detached), so
//!   the property is pinned at the decision rather than inferred from a
//!   rendered reply.
//!
//! Deliberately NOT covered here: the request shapes (`telegram_video_send_test`),
//! the marker/markdown rewrite (`telegram_video_rewrite_test`), and the rich
//! entry type (`telegram_rich_media_kind_test`).

use crate::channels::telegram::delivery::send_local_videos;
use crate::channels::telegram::rich::mermaid::{MediaEntry, MediaKind, rich_media_ownership};
use crate::utils::image::{LocalImageFailureReason, LocalVideo};
use std::path::{Path, PathBuf};

const CHAT: i64 = 133_526_395;

/// An ISO base-media header whose major brand is MPEG-4 — the bytes an
/// `ffmpeg`/Remotion MP4 actually starts with, so the file takes the video arm
/// when it is under the ceiling.
const MP4_BYTES: &[u8] = b"\x00\x00\x00\x18ftypisom\x00\x00\x02\x00isomiso2avc1mp41";

/// A response shaped like a successful text send. Only the ROUTING is under
/// test, so the body mirrors the proven shape rather than inventing a media
/// object (same reasoning as `telegram_video_send_test.rs`).
const SEND_MESSAGE_OK: &str = r#"{"ok":true,"result":{"message_id":701,"date":1757166400,"chat":{"id":133526395,"type":"private"},"text":"ok"}}"#;

/// What Telegram answers for an upload past the ceiling.
const TOO_LARGE: &str = r#"{"ok":false,"error_code":413,"description":"Request Entity Too Large"}"#;

/// A bot pointed at the mockito server.
///
/// MOCK PATHS ARE PASCALCASE FOR EVERY TELOXIDE REQUEST: teloxide builds the
/// method segment from the payload struct name, so `bot.send_document(..)` hits
/// `/botTESTTOKEN/SendDocument`, never the lowercase form the Bot API docs use.
/// A lowercase mock never matches — mockito serves its own unmatched 501, whose
/// empty body teloxide reports as `InvalidJson` and the send reads as a network
/// failure (same trap, same explanation: `plain_outbox_image_test.rs`).
fn test_bot(server: &mockito::ServerGuard) -> teloxide::Bot {
    teloxide::Bot::with_client(
        "TESTTOKEN",
        reqwest_teloxide::Client::builder().build().unwrap(),
    )
    .set_api_url(server.url().parse().unwrap())
}

fn clip_at(path: PathBuf, caption: Option<&str>) -> LocalVideo {
    LocalVideo {
        path,
        caption: caption.map(str::to_string),
    }
}

fn write_fixture(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, bytes).expect("write fixture");
    path
}

// ---------------------------------------------------------------------------
// the failure-entry contract
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_unreadable_video_returns_a_failure_entry() {
    // The reference resolves to a path that is not there. The caller must be
    // able to NAME it: a clip the model announced that vanishes with no notice
    // is the #502 failure mode (silent in both directions).
    let dir = tempfile::tempdir().expect("tempdir");
    let missing = dir.path().join("gone.mp4");
    let server = mockito::Server::new_async().await;
    let bot = test_bot(&server);

    let (delivered, failures) = send_local_videos(
        uuid::Uuid::new_v4(),
        &bot,
        teloxide::types::ChatId(CHAT),
        None,
        &[clip_at(missing.clone(), None)],
    )
    .await;

    assert!(delivered.is_empty(), "nothing was delivered");
    assert_eq!(failures.len(), 1, "the unreadable clip must be reported");
    assert_eq!(failures[0].raw, missing.display().to_string());
    assert_eq!(
        failures[0].reason,
        LocalImageFailureReason::Unreadable,
        "a path that cannot be read is a REFERENCE problem, not a delivery one"
    );
}

#[tokio::test]
async fn an_over_ceiling_video_returns_a_failure_entry() {
    // Past the 50 MB `sendVideo` ceiling the file is no longer playable as an
    // inline video, so it takes the DOCUMENT arm (D3) and the channel's own
    // refusal is what gets reported — as a DELIVERY failure, distinct from the
    // unreadable class above. The file is sparse (`set_len`), so the ceiling is
    // crossed without writing 50 MB to disk.
    let dir = tempfile::tempdir().expect("tempdir");
    let big = dir.path().join("huge.mp4");
    // One handle: `File::create` is already write-capable, so the sparse length
    // and the header go in without a reopen.
    use std::io::{Seek, SeekFrom, Write};
    let mut file = std::fs::File::create(&big).expect("create fixture");
    file.set_len(crate::channels::telegram::send::TELEGRAM_VIDEO_MAX_BYTES + 1)
        .expect("sparse fixture past the ceiling");
    // The container sniff reads the leading bytes, which a sparse file has as
    // zeros — not MPEG-4 — so this file would take the document arm on the
    // format rule alone. Writing the real header on top keeps the ceiling as
    // the ONLY reason it does, which is the property under test.
    file.seek(SeekFrom::Start(0)).expect("seek");
    file.write_all(MP4_BYTES).expect("write header");
    drop(file);

    let mut server = mockito::Server::new_async().await;
    // Only the PATH is matched: the trailing `video/mp4` content type is
    // teloxide's to choose for a `memory(..)` upload and pinning it here would
    // assert a serializer detail this test does not own. Which ARM the file
    // takes is already pinned as a pure decision
    // (`one_byte_over_the_ceiling_is_a_document`); what this test adds is that
    // the arm's REFUSAL comes back as a failure entry.
    let document_mock = server
        .mock("POST", "/botTESTTOKEN/SendDocument")
        .with_status(413)
        .with_header("content-type", "application/json")
        .with_body(TOO_LARGE)
        .expect(1)
        .create_async()
        .await;

    let bot = test_bot(&server);
    let (delivered, failures) = send_local_videos(
        uuid::Uuid::new_v4(),
        &bot,
        teloxide::types::ChatId(CHAT),
        None,
        &[clip_at(big.clone(), None)],
    )
    .await;

    document_mock.assert_async().await;
    assert!(
        delivered.is_empty(),
        "the channel refused it, so nothing was delivered"
    );
    assert_eq!(failures.len(), 1, "the refusal must be reported");
    assert_eq!(
        failures[0].reason,
        LocalImageFailureReason::DeliveryFailed,
        "a file the channel refused is a DELIVERY problem — the reference itself was fine"
    );
}

#[tokio::test]
async fn a_deliverable_video_reports_no_failure_and_the_delivered_path() {
    // The positive control for the two tests above: the instrument must be able
    // to return an empty failure list, or "it reported a failure" proves
    // nothing about which inputs fail.
    let dir = tempfile::tempdir().expect("tempdir");
    let clip = write_fixture(dir.path(), "clip.mp4", MP4_BYTES);
    let mut server = mockito::Server::new_async().await;
    let video_mock = server
        .mock("POST", "/botTESTTOKEN/SendVideo")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(SEND_MESSAGE_OK)
        .expect(1)
        .create_async()
        .await;

    let bot = test_bot(&server);
    let (delivered, failures) = send_local_videos(
        uuid::Uuid::new_v4(),
        &bot,
        teloxide::types::ChatId(CHAT),
        None,
        &[clip_at(clip.clone(), None)],
    )
    .await;

    video_mock.assert_async().await;
    assert_eq!(delivered, vec![clip], "the delivered path comes back");
    assert!(failures.is_empty(), "a delivered clip reports no failure");
}

// ---------------------------------------------------------------------------
// D6 — the rich plane owning a clip suppresses exactly that family's floor
// ---------------------------------------------------------------------------

fn entry(id: &str, kind: MediaKind, bytes: &[u8]) -> MediaEntry {
    MediaEntry {
        id: id.to_string(),
        url: None,
        bytes: Some(bytes.to_vec()),
        kind,
    }
}

#[test]
fn no_duplicate_attachment_when_the_rich_plane_owns_the_video() {
    // #360's bug, one media type over: the rich plane inlines the clip AND the
    // floor sends it again — the same file delivered twice, once detached.
    // `videos = true` is what turns the video floor OFF.
    let own = rich_media_ownership(true, &[entry("vid0", MediaKind::Video, MP4_BYTES)]);
    assert!(own.videos, "the inline video is owned by the rich plane");
    assert!(
        !own.images,
        "a video-only body must NOT claim to own images — that would suppress \
         the image floor for a reply that has none"
    );
    assert!(own.any(), "the rich send itself is gated on the union");
}

#[test]
fn no_duplicate_attachment_when_the_rich_plane_owns_the_image() {
    // The mirror of the case above: the image floor goes off and the video
    // floor stays on, so an image-only reply cannot lose a clip it carries.
    let own = rich_media_ownership(true, &[entry("img0", MediaKind::Photo, MP4_BYTES)]);
    assert!(own.images, "the inline picture is owned by the rich plane");
    assert!(
        !own.videos,
        "an image-only body must not claim the video floor"
    );
}

#[test]
fn both_families_in_one_reply_are_owned_independently() {
    let own = rich_media_ownership(
        true,
        &[
            entry("img0", MediaKind::Photo, MP4_BYTES),
            entry("vid1", MediaKind::Video, MP4_BYTES),
        ],
    );
    assert!(own.images && own.videos, "each family is seen on its own");
    assert!(own.any());
}

#[test]
fn a_plane_that_is_not_sending_owns_nothing() {
    // When the rich send is not happening at all, BOTH floors must run — an
    // ownership flag that stayed set would drop the media silently.
    let own = rich_media_ownership(false, &[entry("vid0", MediaKind::Video, MP4_BYTES)]);
    assert!(!own.images, "nothing is owned when the rich plane is not used");
    assert!(!own.videos, "a floor must never be suppressed for a send that did not happen");
    assert!(!own.any());
}
