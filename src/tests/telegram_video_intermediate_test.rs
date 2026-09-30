//! The intermediate-bubble half of #465: a promoted intermediate carrying a
//! video, the order the two families walk in, and the site that delivers the
//! clip.
//!
//! `handle_intermediate` and `deliver_intermediate_message` need a live bot and
//! `TelegramState`, so what is pinned here is the COMPOSITION they perform —
//! the same shape `telegram_local_image_media_test.rs` pins for the image half,
//! and for the same reason: the decision lives in the pure functions, and the
//! transport above them is covered by the delivery battery
//! (`telegram_video_delivery_test.rs`).
//!
//! The order under test is the VIDEO walk first, on the turn's own text, with
//! the image walk running on the video family's stripped form. It is not
//! interchangeable with the reverse. The image walk CONSUMES a markdown
//! reference whose bytes fail image validation — it records the failure and
//! returns `true`, so the reference leaves both of its buffers — and a clip is
//! exactly such a reference (`UnsupportedFormat`). Image-first therefore eats a
//! `![clip](x.mp4)` before the video family can claim it: the clip is never
//! delivered and the reader is told "Image not attached" instead. The video
//! walk is the selective one, declining anything that is not video-ish by
//! content and leaving every picture reference, `<<IMG:…>>` marker and `tg://`
//! ref verbatim, so running it first cannot starve the image family.

use crate::channels::telegram::delivery::intermediate_failures;
use crate::utils::image::{LocalImageFailureReason, rewrite_local_images, rewrite_local_videos};
use crate::utils::VID_ID_PREFIX;
use std::path::{Path, PathBuf};

/// An ISO base-media header whose major brand is MPEG-4 — the bytes an
/// `ffmpeg`/Remotion MP4 actually starts with.
const MP4_BYTES: &[u8] = b"\x00\x00\x00\x18ftypisom\x00\x00\x02\x00isomiso2avc1mp41";

/// Minimal byte string that passes the magic-byte sniff for PNG, so the
/// positive control below is a file the IMAGE family genuinely owns.
const PNG_BYTES: &[u8] = b"\x89PNG\r\n\x1a\n\x00\x01\x02\x03\x04\x05\x06\x07";

fn fixture(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, bytes).expect("write fixture");
    path
}

/// The two walks `handle_intermediate` runs, in the order it runs them: video
/// on the turn's text, image on the video family's stripped form.
fn walks(
    text: &str,
    cwd: &Path,
) -> (
    crate::utils::image::LocalImageRewrite,
    crate::utils::image::LocalVideoRewrite,
) {
    let vw = rewrite_local_videos(text, Some(cwd), VID_ID_PREFIX, &[]);
    let rw = rewrite_local_images(&vw.stripped, Some(cwd), "img", &[]);
    (rw, vw)
}

// ---------------------------------------------------------------------------
// the marker — the bug this task closes
// ---------------------------------------------------------------------------

#[test]
fn an_intermediate_carrying_a_marker_yields_a_video_and_no_marker_text() {
    let dir = tempfile::tempdir().expect("tempdir");
    let clip = fixture(dir.path(), "clip.mp4", MP4_BYTES);
    let (rw, vw) = walks(
        &format!("Uploading the render now. <<VID:{}>>", clip.display()),
        dir.path(),
    );

    assert_eq!(
        vw.entries.len(),
        1,
        "the intermediate must carry the clip as an attachment"
    );
    assert_eq!(vw.entries[0].video.path, clip);
    assert!(
        !vw.stripped.contains("VID:"),
        "and no marker substring may reach the delivered text: {:?}",
        vw.stripped
    );
    assert_eq!(vw.stripped, "Uploading the render now.");
    // The rich form embeds the reference AT ITS OWN OFFSET, so the clip lands
    // where the model wrote it rather than as a detached bubble.
    assert!(
        vw.rich.contains(&format!("![video](tg://video?id={VID_ID_PREFIX}0)")),
        "the rich form must embed the reference in place: {:?}",
        vw.rich
    );
    // The clip belongs to ONE family. The image walk runs second and must not
    // see it at all — the marker is gone from its input — so it can neither
    // claim the clip a second time nor answer it with a false "Image not
    // attached" notice.
    assert!(
        rw.entries.is_empty() && rw.failures.is_empty(),
        "the image walk must never judge a reference the video family claimed: \
         entries={:?} failures={:?}",
        rw.entries,
        rw.failures
    );
    assert!(
        !rw.rich.contains("VID:") && !rw.stripped.contains("VID:"),
        "and no video marker may survive into either image form"
    );
}

#[test]
fn the_two_families_keep_separate_id_namespaces_in_one_media_array() {
    let dir = tempfile::tempdir().expect("tempdir");
    let clip = fixture(dir.path(), "clip.mp4", MP4_BYTES);
    let png = fixture(dir.path(), "chart.png", PNG_BYTES);
    let (rw, vw) = walks(
        &format!("<<IMG:{}>> and <<VID:{}>>", png.display(), clip.display()),
        dir.path(),
    );

    assert_eq!(rw.entries.len(), 1, "the picture is the image family's");
    assert_eq!(vw.entries.len(), 1, "the clip is the video family's");
    assert_eq!(rw.entries[0].id, "img0");
    assert_eq!(vw.entries[0].id, format!("{VID_ID_PREFIX}0"));
    // Entries are matched to references BY ID inside one message's media array,
    // so a shared prefix would let the image entry answer the video reference.
    assert_ne!(rw.entries[0].id, vw.entries[0].id);
}

// ---------------------------------------------------------------------------
// the order — a clip is claimed, never judged as a broken picture
// ---------------------------------------------------------------------------

#[test]
fn the_video_walk_claims_a_clip_before_the_image_walk_can_judge_it() {
    let dir = tempfile::tempdir().expect("tempdir");
    let clip = fixture(dir.path(), "clip.mp4", MP4_BYTES);
    let (rw, vw) = walks(&format!("see ![clip]({}) here", clip.display()), dir.path());

    // This is the markdown form BOTH families can read, and it is the one that
    // makes the order load-bearing: had the image walk gone first it would have
    // answered `UnsupportedFormat` — a true statement about the bytes and a
    // false notice for a reader who is about to receive that file as a video.
    assert_eq!(vw.entries.len(), 1, "the video family claims the reference");
    assert_eq!(vw.entries[0].video.path, clip);
    assert!(
        rw.failures.is_empty(),
        "the image walk never saw it, so it has no verdict to give: {:?}",
        rw.failures
    );
    assert!(rw.entries.is_empty(), "and nothing to attach");

    let failures = intermediate_failures(&rw, &vw);
    assert!(
        failures.is_empty(),
        "a clip on its way to the user must not be reported as a broken image: {failures:?}"
    );
}

#[test]
fn a_genuinely_broken_image_is_still_reported() {
    // The positive control for the merge above: if `intermediate_failures`
    // dropped everything, "a clip is not reported" would hold for the wrong
    // reason and the reconciliation would be untested.
    let dir = tempfile::tempdir().expect("tempdir");
    let absent = dir.path().join("nowhere.png");
    let (rw, vw) = walks(&format!("x <<IMG:{}>> y", absent.display()), dir.path());

    let failures = intermediate_failures(&rw, &vw);
    assert_eq!(
        failures.len(),
        1,
        "a missing picture keeps its notice: {failures:?}"
    );
    assert_eq!(failures[0].reason, LocalImageFailureReason::NotFound);
}

#[test]
fn a_broken_video_reference_is_reported_by_its_own_family() {
    let dir = tempfile::tempdir().expect("tempdir");
    let absent = dir.path().join("nowhere.mp4");
    let (rw, vw) = walks(&format!("x <<VID:{}>> y", absent.display()), dir.path());

    assert!(vw.entries.is_empty(), "nothing to attach");
    let failures = intermediate_failures(&rw, &vw);
    assert_eq!(
        failures.len(),
        1,
        "the video family's own refusal survives the merge: {failures:?}"
    );
    assert_eq!(failures[0].reason, LocalImageFailureReason::NotFound);
    assert_eq!(failures[0].raw, absent.display().to_string());
}

// ---------------------------------------------------------------------------
// the site — the composition above is worthless if `handle_intermediate` stops
// performing it, and no behavioural test here can see that
// ---------------------------------------------------------------------------

const DELIVERY_SRC: &str = include_str!("../channels/telegram/delivery.rs");
const INTERMEDIATES_SRC: &str = include_str!("../channels/telegram/intermediates.rs");

/// Where `handle_intermediate` feeds the video walk the TURN'S OWN TEXT.
///
/// Anchored on the ARGUMENT rather than the bare function name: the name also
/// occurs in the final leg's pipeline, and a pin matching that one would pass
/// while the intermediate path regressed.
const VIDEO_WALK_ON_THE_TURN_TEXT: &str = "rewrite_local_videos(\n        &text,\n";

#[test]
fn the_intermediate_path_walks_the_video_family_first() {
    let video = DELIVERY_SRC
        .find(VIDEO_WALK_ON_THE_TURN_TEXT)
        .expect("the intermediate path must run the video walk on the turn's text");
    let image = DELIVERY_SRC
        .find("rewrite_local_images(&vw.stripped, Some(cwd), \"img\", &delivered)")
        .expect("the intermediate path must run the image walk on the video family's stripped form");

    assert!(
        video < image,
        "the image walk consumes a clip's reference, so the video walk must \
         claim it first (video at {video}, image at {image})"
    );
}

#[test]
fn the_intermediate_delivery_is_handed_both_walks() {
    let call = DELIVERY_SRC
        .find("super::intermediates::deliver_intermediate_message(")
        .expect("the intermediate delivery must be called");
    let call = &DELIVERY_SRC[call..];
    let end = call.find(")\n").expect("the call must close");
    let args = &call[..end];

    assert!(
        args.contains("&rw, &vw"),
        "the delivery needs BOTH families' rewrites — handing it only `rw` \
         would ship a `<<VID:…>>` marker as literal text: {args}"
    );
}

#[test]
fn the_intermediate_path_delivers_clips_through_the_shared_floor() {
    assert!(
        INTERMEDIATES_SRC.contains(
            "super::delivery::send_local_videos(session_id, bot, chat, thread_id, &videos)"
        ),
        "the clip must leave through the SAME floor helper the final leg uses, \
         so kind selection, captions, telemetry and failure reasons cannot \
         drift between the two planes (#502)"
    );
}

#[test]
fn the_intermediate_media_array_stays_photo_only() {
    // The design anchors this task at the SEND site, not in the media array. A
    // video entry here would need a `tg://video?id=` reference in the rich body
    // to answer, and the body on this path is built from the video family's
    // STRIPPED form — so the reference would be absent, and
    // `neutralize_orphan_photo_refs` would defuse it into a dead reference.
    assert!(
        !INTERMEDIATES_SRC.contains("MediaKind::Video"),
        "a video must not be lifted into the intermediate rich media array"
    );
}
