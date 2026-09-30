//! The outbound video strip on the reaction/intermediate path (#465).
//!
//! `handle_reaction` is the third strip site #465 names — the text a reaction
//! turn ships. It knew only `<<IMG:`: `strip_image_references` reads that form
//! and nothing else, so a `<<VID:path>>` the model echoed reached the user as
//! literal marker text, because nothing on this path read the video family.
//!
//! The site needs the live agent and telegram state, so it is not callable from
//! a unit test. It is pinned the way this repo pins the other sites it cannot
//! call — the COMPOSITION behaviourally, and the SITE'S OWN SOURCE ORDER
//! against `handler.rs` itself. The order is the part a behavioural test cannot
//! see, and it matters twice over:
//!
//! - the video strip must run FIRST. Both families read the same two reference
//!   forms, so a clip's reference left for the image strip would be answered
//!   with a second, false notice ("Image not attached") on the very turn the
//!   clip arrives.
//! - the empty-body early return must sit AFTER both strips, or a turn whose
//!   only content was a marker takes the blank branch and drops the body.

use crate::utils::{extract_local_videos, strip_image_references};
use std::path::Path;

const HANDLER_SRC: &str = include_str!("../channels/telegram/handler.rs");

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
// The composition — what the site's two strips do to a reaction turn
// ---------------------------------------------------------------------------

#[test]
fn a_reaction_turn_carrying_a_video_marker_ships_no_marker_text() {
    let dir = tempfile::tempdir().expect("tempdir");
    let clip = fixture(dir.path(), "clip.mp4", MP4_BYTES);
    let body = format!("before <<VID:{}>> after", clip.display());

    let video_scan = extract_local_videos(&body, None);
    let image_scan = strip_image_references(&video_scan.text, None);

    assert_eq!(
        video_scan.attachments.len(),
        1,
        "the video family must CLAIM the marker, not merely erase it"
    );
    assert_eq!(video_scan.attachments[0].path, clip);
    assert!(
        !image_scan.text.contains("<<VID:"),
        "no marker substring may reach the delivered text: {:?}",
        image_scan.text
    );
    assert_eq!(image_scan.text, "before  after");
}

#[test]
fn the_video_family_claims_a_local_clip_before_the_image_strip_can_mistake_it() {
    let dir = tempfile::tempdir().expect("tempdir");
    let clip = fixture(dir.path(), "clip.mp4", MP4_BYTES);
    let body = format!("see ![the clip]({}) here", clip.display());

    let video_scan = extract_local_videos(&body, Some(dir.path()));
    let image_scan = strip_image_references(&video_scan.text, Some(dir.path()));

    assert_eq!(
        video_scan.attachments.len(),
        1,
        "the clip is the video family's to claim"
    );
    assert!(
        image_scan.attachments.is_empty(),
        "the image strip must see nothing left to attach: {:?}",
        image_scan.attachments
    );
    assert!(
        image_scan.failures.is_empty(),
        "and nothing left to report as a broken image: {:?}",
        image_scan.failures
    );
}

#[test]
fn the_video_strip_leaves_an_image_reference_to_the_image_family() {
    // The positive control for the test above: if the video strip claimed
    // everything, "the image strip sees nothing" would hold for the wrong
    // reason and the order pin below would be pinning nothing.
    let dir = tempfile::tempdir().expect("tempdir");
    let png = fixture(dir.path(), "chart.png", PNG_BYTES);
    let body = format!("x <<IMG:{}>> y", png.display());

    let video_scan = extract_local_videos(&body, None);
    let image_scan = strip_image_references(&video_scan.text, None);

    assert!(
        video_scan.attachments.is_empty(),
        "a PNG is not a clip, however the reference is spelled"
    );
    assert_eq!(
        image_scan.attachments.len(),
        1,
        "the image family keeps its own reference"
    );
    assert_eq!(image_scan.text, "x  y");
}

#[test]
fn a_remote_video_target_stays_verbatim_on_this_path() {
    let body = "watch <<VID:https://example.com/clip.mp4>> now";

    let video_scan = extract_local_videos(body, None);

    assert!(
        video_scan.attachments.is_empty(),
        "this plane has no fetch step, so a remote target is never an attachment"
    );
    assert_eq!(video_scan.text, body, "and it is left exactly as written");
}

// ---------------------------------------------------------------------------
// The site's own source order — the half the composition cannot show
// ---------------------------------------------------------------------------

/// Where the reaction path feeds the video strip's output into the image strip.
///
/// The slice is anchored on the ARGUMENT, not on the bare function name: the
/// two names also occur elsewhere in `handler.rs`, and a pin that matches any
/// of them would pass while the site itself regressed.
const IMAGE_STRIP_OF_VIDEO_OUTPUT: &str = "strip_image_references(&video_scan.text";

#[test]
fn the_video_strip_runs_before_the_image_strip_on_this_path() {
    let video = HANDLER_SRC
        .find("extract_local_videos")
        .expect("the reaction path must strip the video family");
    let image = HANDLER_SRC
        .find(IMAGE_STRIP_OF_VIDEO_OUTPUT)
        .expect("the image strip must consume the video strip's output");

    assert!(
        video < image,
        "the video strip must run first and feed the image strip \
         (video at {video}, image at {image})"
    );
}

#[test]
fn the_empty_body_early_return_sits_after_both_strips() {
    let image = HANDLER_SRC
        .find(IMAGE_STRIP_OF_VIDEO_OUTPUT)
        .expect("the image strip must consume the video strip's output");
    let early_return = HANDLER_SRC
        .find("if text_only.trim().is_empty()")
        .expect("the empty-body early return must exist");

    assert!(
        image < early_return,
        "the blank-body branch must be evaluated AFTER the strips, or a turn \
         whose only content was a marker takes it (strip at {image}, return at \
         {early_return})"
    );
}
