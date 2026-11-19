//! #897: the rich plane's SACRIFICED media entries must reach the delivery
//! failure vectors, so the post-delivery ladder fires exactly as it does for a
//! channel refusal.
//!
//! The measured shape: `retry_rich_media_around_offender` dropped `doc0
//! (passive-recall-precision.md)` to keep the rest of the reply on the rich
//! plane, the send returned `Ok`, `file_failures` stayed empty, no nudge fired,
//! and the model claimed delivery of a document that was never attached.
//!
//! These pin the two pure halves of the fix: what a sacrificed entry becomes
//! (`sacrificed_failure`) and which family vector it joins
//! (`sacrificed_failures`). The transport half — that the retry ladder RETURNS
//! the dropped entries — is pinned against a scripted server in
//! `telegram_rich_media_rejection_test.rs`.
//!
//! The REPORTING half is `delivery::report_rich_plane_sacrifices`, and both
//! bubble legs call it. It is deliberately not pinned to a leg: the first
//! version of this fix had the final leg own the report, on the premise that an
//! intermediate is always superseded. Measured false on 2026-11-19T18:33:49 —
//! the intermediate landed as msg 101255 and `suppressing final response`
//! followed 12 ms later, so the sacrifice was real, delivered, and never
//! reported. Anything that re-introduces a single-leg owner re-opens #897.

use crate::channels::telegram::delivery::{sacrificed_failure, sacrificed_failures};
use crate::channels::telegram::rich::SacrificedEntry;
use crate::channels::telegram::rich::mermaid::MediaKind;
use crate::utils::image::LocalImageFailureReason;

fn entry(id: &str, name: Option<&str>, kind: MediaKind) -> SacrificedEntry {
    SacrificedEntry {
        id: id.to_string(),
        name: name.map(str::to_string),
        kind,
    }
}

/// AC-1: the record names the entry the way the retry log does, and the reason
/// is `DeliveryFailed` — the channel did not put it in front of the user.
#[test]
fn a_sacrificed_entry_becomes_a_delivery_failure_naming_the_file() {
    let with_name = sacrificed_failure(&entry(
        "doc0",
        Some("passive-recall-precision.md"),
        MediaKind::Document,
    ));
    assert_eq!(
        with_name.raw, "doc0 (passive-recall-precision.md)",
        "the record must name the entry and its file, as the ladder logs it"
    );
    assert_eq!(
        with_name.resolved, None,
        "a media id is not a filesystem path"
    );
    assert_eq!(
        with_name.reason,
        LocalImageFailureReason::DeliveryFailed,
        "the channel dropped it, so it is a delivery failure"
    );

    let bare = sacrificed_failure(&entry("img0", None, MediaKind::Photo));
    assert_eq!(
        bare.raw, "img0",
        "an entry with no part name must quote its bare id"
    );
}

/// AC-1: each kind extends the vector whose ladder speaks about that family.
#[test]
fn each_sacrificed_kind_joins_its_own_failure_vector() {
    let sacrificed = vec![
        entry("doc0", Some("a.pdf"), MediaKind::Document),
        entry("img0", None, MediaKind::Photo),
        entry("vid0", None, MediaKind::Video),
    ];

    let (images, files, videos) = sacrificed_failures(&sacrificed);

    assert_eq!(
        images.iter().map(|f| f.raw.as_str()).collect::<Vec<_>>(),
        vec!["img0"],
        "a photo belongs in the image vector"
    );
    assert_eq!(
        files.iter().map(|f| f.raw.as_str()).collect::<Vec<_>>(),
        vec!["doc0 (a.pdf)"],
        "a document belongs in the file vector — the family whose ladder names documents"
    );
    assert_eq!(
        videos.iter().map(|f| f.raw.as_str()).collect::<Vec<_>>(),
        vec!["vid0"],
        "a clip belongs in the video vector"
    );
}

/// AC-3: a send that sacrificed nothing populates nothing, so no false nudge
/// can fire on the clean path.
#[test]
fn no_sacrifice_populates_no_failure_vector() {
    let (images, files, videos) = sacrificed_failures(&[]);
    assert!(images.is_empty(), "{images:?}");
    assert!(files.is_empty(), "{files:?}");
    assert!(videos.is_empty(), "{videos:?}");
}
