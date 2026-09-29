//! The rewrite half of #465: `extract_local_videos` and `rewrite_local_videos`.
//!
//! The send floor (`telegram_video_send_test.rs`) delivers a video once a call
//! site hands it one. This file covers how a reply STARTS carrying one — the
//! validated rewrite that turns a reference into a `tg://video?id=vidN` pointer
//! and the media entry that pointer names.
//!
//! Both forms come out of ONE walk, for the reason the orphan shield makes
//! load-bearing: `neutralize_orphan_photo_refs` defuses a `tg://video?id=`
//! whose id is absent from the request's `media` array, so a reference built by
//! a different pass than its entry degrades the video to dead markdown
//! silently. The pin for that contract is
//! `the_rewritten_reference_id_is_the_entry_id_the_media_array_must_carry`.
//!
//! Two rules are INHERITED from the image scanner rather than re-decided here,
//! so one question keeps one home:
//!
//! - the MARKER form has no code-span guard — a marker is machine syntax, never
//!   prose (pinned for images by `img_marker_inside_a_code_span_is_still_
//!   extracted`);
//! - the MARKDOWN form does — a fenced or backticked reference stays
//!   byte-identical, because it may be documentation rather than a live
//!   reference.
//!
//! One rule genuinely differs from the image plane: there is NO remote arm, so
//! a remote target is left in the text as written.

use crate::utils::image::{
    IMG_ID_PREFIX, LocalImageFailureReason, VID_ID_PREFIX, extract_local_videos,
    rewrite_local_videos,
};
use std::path::{Path, PathBuf};

/// An ISO base-media header whose major brand is MPEG-4 — the bytes an
/// `ffmpeg`/Remotion MP4 actually starts with.
const MP4_BYTES: &[u8] = b"\x00\x00\x00\x18ftypisom\x00\x00\x02\x00isomiso2avc1mp41";

/// Minimal byte string that passes the magic-byte sniff for PNG. Used to pin
/// that a reference the IMAGE family owns is not claimed here.
const PNG_BYTES: &[u8] = b"\x89PNG\r\n\x1a\n\x00\x01\x02\x03\x04\x05\x06\x07";

/// The prefix the delivery site passes. Named here rather than inlined so a
/// change to the namespace shows up as this constant's value changing, not as
/// an invisible edit to a string literal in twenty tests.
const PREFIX: &str = VID_ID_PREFIX;

fn write_fixture(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, bytes).expect("write fixture");
    path
}

fn video_fixture() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = write_fixture(dir.path(), "clip.mp4", MP4_BYTES);
    (dir, path)
}

fn ids(rw: &crate::utils::image::LocalVideoRewrite) -> Vec<String> {
    rw.entries.iter().map(|e| e.id.clone()).collect()
}

// ---------------------------------------------------------------------------
// the happy path: in-place embedding, and the stripped twin
// ---------------------------------------------------------------------------

#[test]
fn a_resolvable_marker_is_embedded_in_place_and_stripped_from_the_twin() {
    let (_dir, clip) = video_fixture();
    let rw = rewrite_local_videos(
        &format!("before <<VID:{}>> after", clip.display()),
        None,
        PREFIX,
        &[],
    );

    assert_eq!(rw.entries.len(), 1, "one reference must yield one entry");
    assert_eq!(
        rw.rich,
        format!("before ![video](tg://video?id={PREFIX}0) after"),
        "the reference is replaced AT ITS OWN OFFSET, not appended"
    );
    assert!(
        !rw.stripped.contains("tg://") && !rw.stripped.contains("VID:"),
        "the strip twin carries neither the reference nor a dead tg:// pointer: {}",
        rw.stripped
    );
    // The reference is DELETED, not replaced by a space: the surrounding
    // spaces are the author's and both survive. Asserted the way the image
    // plane asserts it (`img_marker_with_an_absolute_path_is_attached`:
    // "see  now").
    assert_eq!(rw.stripped, "before  after");
    assert_eq!(rw.entries[0].video.path, clip);
}

#[test]
fn a_resolvable_markdown_reference_is_embedded_in_place_and_stripped_from_the_twin() {
    let (_dir, clip) = video_fixture();
    let rw = rewrite_local_videos(
        &format!("see ![the clip]({}) here", clip.display()),
        None,
        PREFIX,
        &[],
    );

    assert_eq!(rw.entries.len(), 1);
    assert_eq!(
        rw.rich,
        format!("see ![the clip](tg://video?id={PREFIX}0) here")
    );
    assert_eq!(rw.stripped, "see  here", "alt text and target both go");
}

#[test]
fn two_references_get_ids_in_order_of_appearance() {
    let dir = tempfile::tempdir().expect("tempdir");
    let a = write_fixture(dir.path(), "a.mp4", MP4_BYTES);
    let b = write_fixture(dir.path(), "b.mp4", MP4_BYTES);
    let rw = rewrite_local_videos(
        &format!("![one]({}) and ![two]({})", a.display(), b.display()),
        None,
        PREFIX,
        &[],
    );

    assert_eq!(ids(&rw), vec![format!("{PREFIX}0"), format!("{PREFIX}1")]);
    assert_eq!(rw.entries[0].video.path, a);
    assert_eq!(rw.entries[1].video.path, b);
}

// ---------------------------------------------------------------------------
// the titled reference: the markdown title is the caption channel
// ---------------------------------------------------------------------------

#[test]
fn a_markdown_title_survives_as_the_rich_caption_channel() {
    let (_dir, clip) = video_fixture();
    let rw = rewrite_local_videos(
        &format!("![alt text]({} \"the title\")", clip.display()),
        None,
        PREFIX,
        &[],
    );

    assert_eq!(rw.entries.len(), 1);
    assert_eq!(
        rw.entries[0].video.caption.as_deref(),
        Some("the title"),
        "the title rides the entry, which is what the fallback leg captions from"
    );
    assert_eq!(rw.entries[0].video.path, clip);
    assert_eq!(
        rw.rich,
        format!("![alt text](tg://video?id={PREFIX}0 \"the title\")"),
        "the title is ALSO emitted onto the reference, space-separated as \
         markdown requires — the rich media plane has no caption field, so the \
         title is a local video's only caption channel there (#487)"
    );
    assert!(rw.failures.is_empty());
}

#[test]
fn a_marker_never_carries_a_caption() {
    let (_dir, clip) = video_fixture();
    let rw = rewrite_local_videos(&format!("<<VID:{}>>", clip.display()), None, PREFIX, &[]);

    assert_eq!(rw.entries.len(), 1);
    assert_eq!(
        rw.entries[0].video.caption, None,
        "the marker form has no title syntax, so it can never produce one"
    );
}

// ---------------------------------------------------------------------------
// the code-span guard, and the asymmetry inherited with it
// ---------------------------------------------------------------------------

#[test]
fn references_inside_a_fence_or_a_code_span_are_byte_identical_in_both_forms() {
    let (_dir, clip) = video_fixture();
    let text = format!(
        "```\n![fenced]({p})\n```\nand `![spanned]({p})` and <<VID:{p}>>",
        p = clip.display()
    );
    let rw = rewrite_local_videos(&text, None, PREFIX, &[]);

    assert!(
        rw.rich.contains(&format!("```\n![fenced]({})", clip.display())),
        "a fenced markdown reference stays verbatim in the rich form: {}",
        rw.rich
    );
    assert!(
        rw.rich.contains(&format!("`![spanned]({})`", clip.display())),
        "a backticked markdown reference stays verbatim in the rich form: {}",
        rw.rich
    );
    assert!(
        rw.stripped.contains(&format!("![fenced]({})", clip.display())),
        "and in the strip twin: {}",
        rw.stripped
    );
    assert_eq!(
        rw.entries.len(),
        1,
        "only the MARKER resolves — the two markdown forms are documentation here"
    );
    assert_eq!(ids(&rw), vec![format!("{PREFIX}0")]);
}

#[test]
fn a_marker_inside_a_code_span_is_still_extracted() {
    // The inherited asymmetry: a marker is machine syntax, never prose, so the
    // code-span guard does not apply to it. Mirrors the image plane's pin
    // (`img_marker_inside_a_code_span_is_still_extracted`).
    let (_dir, clip) = video_fixture();
    let rw = rewrite_local_videos(&format!("`<<VID:{}>>`", clip.display()), None, PREFIX, &[]);

    assert_eq!(
        rw.entries.len(),
        1,
        "the marker is consumed even between backticks"
    );
    assert!(!rw.stripped.contains("VID:"));
}

// ---------------------------------------------------------------------------
// the ref/entry contract, and the namespace split against the image family
// ---------------------------------------------------------------------------

#[test]
fn the_rewritten_reference_id_is_the_entry_id_the_media_array_must_carry() {
    let dir = tempfile::tempdir().expect("tempdir");
    let a = write_fixture(dir.path(), "a.mp4", MP4_BYTES);
    let b = write_fixture(dir.path(), "b.mp4", MP4_BYTES);
    let rw = rewrite_local_videos(
        &format!("![one]({}) ![two]({})", a.display(), b.display()),
        None,
        PREFIX,
        &[],
    );

    for entry in &rw.entries {
        assert!(
            rw.rich.contains(&format!("tg://video?id={}", entry.id)),
            "every entry id must appear as a VIDEO reference in the rich form — \
             an id present only in `media` is an orphan the shield would defuse"
        );
    }
    assert_eq!(ids(&rw), vec![format!("{PREFIX}0"), format!("{PREFIX}1")]);
    assert!(
        !rw.rich.contains("tg://photo"),
        "a video reference must never be emitted in the PHOTO namespace: the \
         entry it names carries `type: video`, and the two are matched BY ID"
    );
}

#[test]
fn the_video_namespace_is_distinct_from_the_image_one() {
    assert_ne!(
        VID_ID_PREFIX, IMG_ID_PREFIX,
        "one message can carry both planes; entries are matched to references \
         BY ID, so a shared prefix would let an image entry satisfy a video ref"
    );
    assert!(
        !VID_ID_PREFIX.starts_with(IMG_ID_PREFIX) && !IMG_ID_PREFIX.starts_with(VID_ID_PREFIX),
        "a PREFIX relationship is as dangerous as equality: `vid1` starts with \
         `vid`, but an id built by concatenating a prefix and an index must not \
         be readable as the other family's id"
    );
}

// ---------------------------------------------------------------------------
// the refusal arms — a reference that is not ours stays exactly as written
// ---------------------------------------------------------------------------

#[test]
fn an_image_target_is_left_to_the_image_family() {
    let dir = tempfile::tempdir().expect("tempdir");
    let png = write_fixture(dir.path(), "chart.png", PNG_BYTES);
    let rw = rewrite_local_videos(&format!("![chart]({})", png.display()), None, PREFIX, &[]);

    assert!(rw.entries.is_empty());
    assert_eq!(
        rw.rich,
        format!("![chart]({})", png.display()),
        "the image family owns this reference; claiming it here would deliver \
         one file twice, in the wrong plane"
    );
    assert_eq!(rw.rich, rw.stripped);
}

#[test]
fn a_remote_target_is_left_verbatim() {
    // The declared non-goal: this plane has no fetch step, so deleting a remote
    // link would be the #286 loss — a reference removed and nothing delivered.
    let rw = rewrite_local_videos(
        "look ![x](https://cdn.example.com/clip.mp4) here",
        None,
        PREFIX,
        &[],
    );

    assert!(rw.entries.is_empty());
    assert_eq!(rw.rich, "look ![x](https://cdn.example.com/clip.mp4) here");
    assert_eq!(rw.rich, rw.stripped);
}

#[test]
fn a_missing_path_becomes_one_not_found_failure_and_leaves_both_forms() {
    let rw = rewrite_local_videos("x <<VID:/nope/clip.mp4>> y", None, PREFIX, &[]);

    assert!(rw.entries.is_empty());
    assert_eq!(rw.failures.len(), 1, "one dead reference, one failure");
    assert_eq!(rw.failures[0].reason, LocalImageFailureReason::NotFound);
    assert_eq!(
        rw.rich, rw.stripped,
        "the two forms agree even on the refusal arm"
    );
    assert!(
        !rw.rich.contains("VID:"),
        "a marker is machine syntax: it leaves the text whether it resolves or \
         not, and is reported rather than shipped as a bare directive"
    );
}

#[test]
fn an_empty_marker_is_dropped_without_a_failure() {
    // Mirrors the image plane's `empty_img_marker_is_dropped_without_a_failure`.
    let rw = rewrite_local_videos("<<VID:>>", None, PREFIX, &[]);

    assert!(rw.entries.is_empty());
    assert!(
        rw.failures.is_empty(),
        "there is no path to report and nothing to deliver"
    );
    assert_eq!(rw.rich, "");
    assert_eq!(rw.stripped, "");
}

#[test]
fn an_already_delivered_video_is_consumed_without_a_second_entry() {
    let (_dir, clip) = video_fixture();
    let rw = rewrite_local_videos(
        &format!("![again]({})", clip.display()),
        None,
        PREFIX,
        &[clip.clone()],
    );

    assert!(
        rw.entries.is_empty(),
        "a delivered video is not a lost one; a second bubble for a video the \
         reader already has would be noise"
    );
    assert!(rw.failures.is_empty());
    assert_eq!(rw.rich, "", "the reference is still consumed");
}

// ---------------------------------------------------------------------------
// the parity pin: the strip twin agrees with the strip-only entry point
// ---------------------------------------------------------------------------

#[test]
fn the_stripped_twin_matches_extract_local_videos_exactly() {
    // The delivery path re-runs extraction on the final reply, exactly as
    // `strip_image_references`'s doc states. If the two walks disagreed about
    // which references are videos, one reply would be sanitised by a rule set
    // the other never saw.
    let dir = tempfile::tempdir().expect("tempdir");
    let clip = write_fixture(dir.path(), "clip.mp4", MP4_BYTES);
    let png = write_fixture(dir.path(), "chart.png", PNG_BYTES);
    let text = format!(
        "keep ![chart]({img}) and drop <<VID:{vid}>> plus ![clip]({vid}) \
         and `![code]({vid})` and ![remote](https://h/x.mp4)",
        img = png.display(),
        vid = clip.display()
    );

    let rw = rewrite_local_videos(&text, Some(dir.path()), PREFIX, &[]);
    let scan = extract_local_videos(&text, Some(dir.path()));

    assert_eq!(
        rw.stripped, scan.text,
        "the rewrite's strip twin and the scan-only walk must agree byte for byte"
    );
    assert_eq!(
        rw.entries.len(),
        scan.attachments.len(),
        "and they must agree about how many videos the reply carried"
    );
    assert_eq!(rw.failures.len(), scan.failures.len());
}
