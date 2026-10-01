//! The rich media entry's own `type` (#465).
//!
//! The write side was the last photo-only component in the chain: both
//! builders hardcoded `"type": "photo"` for EVERY entry, and the multipart
//! part identity sniffed images alone (falling back to `image/bmp` for bytes
//! it could not read). That is precisely the bug the anim-lab MIX probe hit as
//! `HTTP 400 RICH_MESSAGE_VIDEO_INVALID` — its video reference had been
//! generated with the photo helper — so the repo was carrying the mistake, not
//! merely missing the feature.
//!
//! The read side was already ready for this and is NOT re-tested here: the
//! validator accepts `photo | video | audio` (`rich/table.rs`), the orphan
//! shield already lists `tg://video?id=`, and `attach://<id>` resolution is
//! generic. What this file pins is that the two builders now emit the entry's
//! own kind, and that the multipart part agrees with it.

use crate::channels::telegram::rich::api::{
    build_body_markdown_media_edit, build_body_markdown_media_target, media_part_identity,
};
use crate::channels::telegram::rich::mermaid::{
    MediaEntry, MediaKind, neutralize_orphan_photo_refs,
};

/// An MPEG-4 header (`ftyp` + `isom` brand) — the bytes a real clip carries.
const MP4_BYTES: &[u8] = b"\x00\x00\x00\x18ftypisom\x00\x00\x02\x00isomiso2avc1mp41";
const PNG_BYTES: &[u8] = b"\x89PNG\r\n\x1a\n\x00\x01\x02\x03\x04\x05\x06\x07";

fn entry(id: &str, kind: MediaKind, bytes: &[u8]) -> MediaEntry {
    MediaEntry {
        id: id.to_string(),
        url: None,
        bytes: Some(bytes.to_vec()),
        kind,
    }
}

fn media_array(body: &serde_json::Value) -> &Vec<serde_json::Value> {
    body["rich_message"]["media"]
        .as_array()
        .expect("the media array is the sole authority for a `tg://` ref")
}

// ---------------------------------------------------------------------------
// the entry's declared type — both builders
// ---------------------------------------------------------------------------

#[test]
fn a_video_entry_declares_the_video_type_on_the_send_path() {
    let media = vec![entry("vid0", MediaKind::Video, MP4_BYTES)];
    let body = build_body_markdown_media_target(
        -100,
        None,
        None,
        "see ![clip](tg://video?id=vid0)",
        &media,
        None,
    );
    let arr = media_array(&body);
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["id"], "vid0");
    assert_eq!(
        arr[0]["media"]["type"], "video",
        "a video entry must not be declared as a photo — that IS the \
         RICH_MESSAGE_VIDEO_INVALID bug, carried in the repo"
    );
    assert_eq!(arr[0]["media"]["media"], "attach://vid0");
}

#[test]
fn a_video_entry_declares_the_video_type_on_the_edit_path() {
    // The edit builder is the second of the two, and #98 gave it its own copy
    // of the media-array code — so a fix applied to one is not a fix.
    let media = vec![entry("vid0", MediaKind::Video, MP4_BYTES)];
    let body = build_body_markdown_media_edit(
        -100,
        42,
        "see ![clip](tg://video?id=vid0)",
        &media,
    );
    let arr = media_array(&body);
    assert_eq!(arr[0]["media"]["type"], "video");
    assert_eq!(arr[0]["media"]["media"], "attach://vid0");
}

#[test]
fn a_photo_entry_still_declares_the_photo_type() {
    // The control: the change must not turn every entry into a video.
    let media = vec![entry("img0", MediaKind::Photo, PNG_BYTES)];
    let body =
        build_body_markdown_media_target(-100, None, None, "![chart](tg://photo?id=img0)", &media, None);
    assert_eq!(media_array(&body)[0]["media"]["type"], "photo");
    assert_eq!(media_array(&body)[0]["media"]["media"], "attach://img0");
}

#[test]
fn a_message_carrying_both_families_declares_each_entry_its_own_type() {
    // Ref and entry are matched BY ID, so the two families share one array
    // while keeping separate namespaces. This is the shape the MIX probe
    // validated live (`inner [photo, photo, video]`) — and the id prefixes are
    // what keep `imgN` from ever binding a video entry.
    let media = vec![
        entry("img0", MediaKind::Photo, PNG_BYTES),
        entry("img1", MediaKind::Photo, PNG_BYTES),
        entry("vid2", MediaKind::Video, MP4_BYTES),
    ];
    let body = build_body_markdown_media_target(
        -100,
        None,
        None,
        "![a](tg://photo?id=img0)\n\n![b](tg://photo?id=img1)\n\n[clip](tg://video?id=vid2)",
        &media,
        None,
    );
    let arr = media_array(&body);
    let types: Vec<&str> = arr
        .iter()
        .map(|e| e["media"]["type"].as_str().expect("a string type"))
        .collect();
    assert_eq!(types, vec!["photo", "photo", "video"]);
    let ids: Vec<&str> = arr.iter().map(|e| e["id"].as_str().unwrap()).collect();
    assert_eq!(ids, vec!["img0", "img1", "vid2"]);
}

// ---------------------------------------------------------------------------
// the multipart part agrees with the declared type
// ---------------------------------------------------------------------------

#[test]
fn a_video_part_is_named_and_mimed_as_a_video() {
    // The image sniffer answers `bmp`/`image/bmp` for bytes it cannot read, so
    // without this the one inline-video path would upload an MP4 as a BMP.
    assert_eq!(
        media_part_identity("vid0", MP4_BYTES, MediaKind::Video),
        ("vid0.mp4".to_string(), "video/mp4")
    );
}

#[test]
fn a_video_entry_with_url_bytes_absent_keeps_the_legacy_url_reference() {
    // The URL arm is untouched by #465 (no remote video fetch arm exists), but
    // the kind must still travel: a URL-sourced video entry declares `video`.
    let media = vec![MediaEntry {
        id: "vid0".to_string(),
        url: Some("https://example.invalid/clip.mp4".to_string()),
        bytes: None,
        kind: MediaKind::Video,
    }];
    let body = build_body_markdown_media_target(
        -100,
        None,
        None,
        "[clip](tg://video?id=vid0)",
        &media,
        None,
    );
    let arr = media_array(&body);
    assert_eq!(arr[0]["media"]["type"], "video");
    assert_eq!(arr[0]["media"]["media"], "https://example.invalid/clip.mp4");
}

// ---------------------------------------------------------------------------
// the read side: the orphan shield and a matched video ref (#465)
// ---------------------------------------------------------------------------

#[test]
fn the_orphan_shield_leaves_a_matched_video_ref_alone() {
    // The entry-type tests above never run the shield, so they prove the array
    // is well-formed and not that the body survives it. A `tg://video?id=`
    // whose id IS in the array must keep resolving: defusing a live reference
    // would deliver a dead markdown link in place of the clip the reader was
    // promised.
    let media = vec![entry("vid0", MediaKind::Video, MP4_BYTES)];

    assert_eq!(
        neutralize_orphan_photo_refs("see ![clip](tg://video?id=vid0) here", &media),
        "see ![clip](tg://video?id=vid0) here",
        "a video ref with a matching entry must keep resolving"
    );
    // The control: the same ref with no entry behind it is defused, which is
    // what protects the message from a rich rejection.
    assert_eq!(
        neutralize_orphan_photo_refs("see ![clip](tg://video?id=absent) here", &media),
        "see ![clip](tg:video?id=absent) here",
        "an orphan video ref must lose its scheme"
    );
}

#[test]
fn the_orphan_shield_matches_by_id_and_ignores_kind() {
    // The mechanism that makes the separate id namespaces load-bearing (#465).
    // The shield resolves a reference against an entry by ID alone, so a
    // `tg://video?id=` ref would be "resolved" by an entry of ANY kind sharing
    // that id — and a PHOTO entry cannot answer a video reference, so the whole
    // message would be rejected with RICH_MESSAGE_VIDEO_INVALID. Sharing one
    // `img` prefix between the families is exactly how that happens.
    let photos = vec![entry("img0", MediaKind::Photo, PNG_BYTES)];

    assert_eq!(
        neutralize_orphan_photo_refs("![clip](tg://video?id=img0)", &photos),
        "![clip](tg://video?id=img0)",
        "the shield matches on the id alone — it does not consult the kind, \
         which is WHY the two families must keep separate namespaces"
    );

    // With the video family's own prefix the same body is left untouched for
    // the right reason: `vid0` names a real video entry.
    let mixed = vec![
        entry("img0", MediaKind::Photo, PNG_BYTES),
        entry("vid0", MediaKind::Video, MP4_BYTES),
    ];
    let body = "![a](tg://photo?id=img0)\n\n![b](tg://video?id=vid0)";
    assert_eq!(
        neutralize_orphan_photo_refs(body, &mixed),
        body,
        "both families' refs resolve against their own entries"
    );
}
