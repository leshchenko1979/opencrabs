//! The media-plane half of #502: what the rich request must look like for a
//! promoted intermediate to carry a local picture, and the two gates a
//! media-bearing message has to pass on the way there.
//!
//! Covered here (the promotion-decision half lives in
//! `telegram_local_image_promotion_test.rs`):
//!
//! - `rich_required` — the pure half of the second gate. The `Config`-reading
//!   `should_send_native_rich_for_media` is deliberately NOT unit-tested,
//!   following the recorded precedent for `should_render_mermaid`
//!   (`telegram_mermaid_test.rs`, module doc): its values depend on the live
//!   config, so the flag-dependent half belongs to the live smoke.
//! - the merged body shape: one request carrying BOTH a resolved fence
//!   (`diag0`) and a local image (`img0`), with the markdown keeping both
//!   references — the media array is the sole authority for a `tg://` ref.
//! - `media_part_identity` — the multipart part's name and MIME derived from
//!   the bytes, so a JPEG does not ship mislabelled as PNG.

use crate::channels::telegram::delivery_dedup::content_key;
use crate::channels::telegram::rich::api::{
    build_body_markdown_media_target, media_part_identity,
};
use crate::channels::telegram::rich::detect::rich_required;
use crate::channels::telegram::rich::mermaid::MediaEntry;

const PNG_BYTES: &[u8] = b"\x89PNG\r\n\x1a\n\x00\x01\x02\x03\x04\x05\x06\x07";
const JPEG_BYTES: &[u8] = b"\xff\xd8\xff\xe0\x00\x10JFIF\x00\x01";
const GIF_BYTES: &[u8] = b"GIF89a\x01\x00\x01\x00";
const WEBP_BYTES: &[u8] = b"RIFF\x24\x00\x00\x00WEBPVP8 ";
const BMP_BYTES: &[u8] = b"BM\x36\x00\x00\x00\x00\x00\x00\x00";

fn local_entry(id: &str, bytes: &[u8]) -> MediaEntry {
    MediaEntry {
        id: id.to_string(),
        url: None,
        bytes: Some(bytes.to_vec()),
    }
}

// ---------------------------------------------------------------------------
// rich_required — the pure half of the second gate
// ---------------------------------------------------------------------------

#[test]
fn media_forces_the_rich_plane_where_structure_alone_would_not() {
    assert!(
        rich_required(false, true),
        "resolved media is itself a reason to ride the rich plane — it is the \
         only plane with a media array"
    );
    assert!(rich_required(true, false), "structure still promotes on its own");
    assert!(rich_required(true, true));
    assert!(
        !rich_required(false, false),
        "thin prose with nothing to embed keeps the HTML path"
    );
}

// ---------------------------------------------------------------------------
// the merged body shape — fence and local image in ONE request
// ---------------------------------------------------------------------------

#[test]
fn a_fence_and_a_local_image_share_one_request_and_both_references_survive() {
    // The composition the media-aware core performs: the fence resolver's
    // entries first, the caller's local entries appended. Both ids must be in
    // the array, because a `tg://photo?id=` reference survives normalization
    // only when its id names an entry in THIS request (#334).
    let media = vec![
        MediaEntry {
            id: "diag0".into(),
            url: None,
            bytes: Some(PNG_BYTES.to_vec()),
        },
        local_entry("img0", PNG_BYTES),
    ];
    let markdown = "![diagram](tg://photo?id=diag0)\n\n![chart](tg://photo?id=img0)";

    let body = build_body_markdown_media_target(-100, None, None, markdown, &media, None);

    let arr = body["rich_message"]["media"]
        .as_array()
        .expect("media array");
    assert_eq!(arr.len(), 2, "one request carries both entries");
    assert_eq!(arr[0]["id"], "diag0");
    assert_eq!(arr[1]["id"], "img0");
    assert_eq!(arr[0]["media"]["media"], "attach://diag0");
    assert_eq!(
        arr[1]["media"]["media"], "attach://img0",
        "local bytes ride the same multipart path a diagram does"
    );

    let md = body["rich_message"]["markdown"]
        .as_str()
        .expect("rich markdown");
    assert!(
        md.contains("tg://photo?id=diag0") && md.contains("tg://photo?id=img0"),
        "both references survive normalization: {md}"
    );
}

#[test]
fn an_orphan_local_reference_is_neutralised_like_any_other() {
    // A local ref whose entry did not make it into the array (a failed byte
    // read) must be neutralised rather than shipped as dead markdown — the same
    // rule that governs a broken fence reference.
    let media = vec![local_entry("img0", PNG_BYTES)];
    let markdown = "kept ![a](tg://photo?id=img0) dropped ![b](tg://photo?id=img1)";

    let body = build_body_markdown_media_target(-100, None, None, markdown, &media, None);
    let md = body["rich_message"]["markdown"]
        .as_str()
        .expect("rich markdown");

    assert!(md.contains("tg://photo?id=img0"));
    assert!(
        !md.contains("tg://photo?id=img1"),
        "an orphan reference must not reach the user as literal text: {md}"
    );
}

// ---------------------------------------------------------------------------
// media_part_identity — the multipart part follows the bytes
// ---------------------------------------------------------------------------

#[test]
fn the_multipart_part_identity_follows_the_bytes() {
    assert_eq!(
        media_part_identity("img0", PNG_BYTES),
        ("img0.png".to_string(), "image/png")
    );
    assert_eq!(
        media_part_identity("img1", JPEG_BYTES),
        ("img1.jpg".to_string(), "image/jpeg"),
        "a JPEG must not ship as <id>.png with image/png"
    );
    assert_eq!(
        media_part_identity("img2", GIF_BYTES),
        ("img2.gif".to_string(), "image/gif")
    );
    assert_eq!(
        media_part_identity("img3", WEBP_BYTES),
        ("img3.webp".to_string(), "image/webp")
    );
    assert_eq!(
        media_part_identity("img4", BMP_BYTES),
        ("img4.bmp".to_string(), "image/bmp")
    );
}

#[test]
fn a_mermaid_part_keeps_its_own_id_and_png_identity() {
    // The existing diagram path is unchanged by #502: the sniffer reads PNG
    // bytes and returns exactly the name and mime the hardcoded version did.
    assert_eq!(
        media_part_identity("diag0", PNG_BYTES),
        ("diag0.png".to_string(), "image/png")
    );
}

// ---------------------------------------------------------------------------
// content_key — the dedup guard must not call two different pictures a duplicate
// ---------------------------------------------------------------------------

#[test]
fn two_sends_of_the_same_text_with_different_pictures_are_not_duplicates() {
    // A promoted intermediate rewrites its reference to `img0` whatever the
    // picture is, so two DIFFERENT charts in two intermediates carry identical
    // markdown. Keyed on the text alone the second would be suppressed as a
    // duplicate and its picture silently never sent.
    let text = "![chart](tg://photo?id=img0)";
    let small = vec![local_entry("img0", PNG_BYTES)];
    let large = vec![local_entry("img0", &[0u8; 4096])];

    assert_ne!(
        content_key(text, &small),
        content_key(text, &large),
        "same markdown, different picture bytes — not the same content"
    );
}

#[test]
fn a_media_free_send_keys_on_its_markdown_exactly_as_before() {
    // Every pre-#502 caller passes no media, and must keep the fingerprint it
    // had — the guard's recorded history and its tests depend on it.
    assert_eq!(
        content_key("the same answer", &[]),
        "the same answer",
        "with no media the key is the markdown verbatim"
    );
}

#[test]
fn identical_text_and_identical_media_still_key_the_same() {
    let text = "![chart](tg://photo?id=img0)";
    let media = vec![local_entry("img0", PNG_BYTES)];
    assert_eq!(
        content_key(text, &media),
        content_key(text, &media),
        "a genuine duplicate must still be recognised as one"
    );
}

#[test]
fn the_entry_id_participates_in_the_key() {
    // Same bytes, different id: distinct requests, because the id is what the
    // markdown reference resolves against.
    let text = "![chart](tg://photo?id=img0)";
    let a = vec![local_entry("img0", PNG_BYTES)];
    let b = vec![local_entry("img1", PNG_BYTES)];
    assert_ne!(content_key(text, &a), content_key(text, &b));
}
