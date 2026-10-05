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
//! the image walk running afterwards — twice, on the video family's rich and
//! stripped forms (#732). It is not interchangeable with the reverse. The image
//! walk CONSUMES a markdown reference whose bytes fail image validation — it
//! records the failure and
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

/// The walks `handle_intermediate` runs, in the order it runs them (#732):
/// video on the turn's text, then the image family TWICE — on the video
/// family's rich form for the rich plane, and on its stripped form for the
/// HTML plane and the dedup record.
///
/// Returns `(rich_images, stripped_images, videos)`. The two image rewrites
/// differ only in their BASE: `rich_images.rich` keeps both families'
/// `tg://` references — the media array answers them — while
/// `stripped_images.stripped` carries neither, because the HTML plane has no
/// array to resolve one against.
fn walks(
    text: &str,
    cwd: &Path,
) -> (
    crate::utils::image::LocalImageRewrite,
    crate::utils::image::LocalImageRewrite,
    crate::utils::image::LocalVideoRewrite,
) {
    let vw = rewrite_local_videos(text, Some(cwd), VID_ID_PREFIX, &[]);
    let rw_rich = rewrite_local_images(&vw.rich, Some(cwd), "img", &[]);
    let rw_stripped = rewrite_local_images(&vw.stripped, Some(cwd), "img", &[]);
    (rw_rich, rw_stripped, vw)
}

// ---------------------------------------------------------------------------
// the marker — the bug this task closes
// ---------------------------------------------------------------------------

#[test]
fn an_intermediate_carrying_a_marker_yields_a_video_and_no_marker_text() {
    let dir = tempfile::tempdir().expect("tempdir");
    let clip = fixture(dir.path(), "clip.mp4", MP4_BYTES);
    let (rw_rich, rw_stripped, vw) = walks(
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
    // attached" notice. Both image rewrites agree: neither family's reference
    // is in either one's input.
    assert!(
        rw_rich.entries.is_empty() && rw_rich.failures.is_empty(),
        "the rich-plane image walk must never judge a reference the video family \
         claimed: entries={:?} failures={:?}",
        rw_rich.entries,
        rw_rich.failures
    );
    assert!(
        rw_stripped.entries.is_empty() && rw_stripped.failures.is_empty(),
        "nor the stripped-plane one: entries={:?} failures={:?}",
        rw_stripped.entries,
        rw_stripped.failures
    );
    assert!(
        !rw_rich.rich.contains("VID:") && !rw_stripped.stripped.contains("VID:"),
        "and no video marker may survive into either image form"
    );
}

#[test]
fn the_two_families_keep_separate_id_namespaces_in_one_media_array() {
    let dir = tempfile::tempdir().expect("tempdir");
    let clip = fixture(dir.path(), "clip.mp4", MP4_BYTES);
    let png = fixture(dir.path(), "chart.png", PNG_BYTES);
    let (rw_rich, _rw_stripped, vw) = walks(
        &format!("<<IMG:{}>> and <<VID:{}>>", png.display(), clip.display()),
        dir.path(),
    );

    assert_eq!(rw_rich.entries.len(), 1, "the picture is the image family's");
    assert_eq!(vw.entries.len(), 1, "the clip is the video family's");
    assert_eq!(rw_rich.entries[0].id, "img0");
    assert_eq!(vw.entries[0].id, format!("{VID_ID_PREFIX}0"));
    // Entries are matched to references BY ID inside one message's media array,
    // so a shared prefix would let the image entry answer the video reference.
    assert_ne!(rw_rich.entries[0].id, vw.entries[0].id);
}

// ---------------------------------------------------------------------------
// the order — a clip is claimed, never judged as a broken picture
// ---------------------------------------------------------------------------

#[test]
fn the_video_walk_claims_a_clip_before_the_image_walk_can_judge_it() {
    let dir = tempfile::tempdir().expect("tempdir");
    let clip = fixture(dir.path(), "clip.mp4", MP4_BYTES);
    let (_rw_rich, rw_stripped, vw) =
        walks(&format!("see ![clip]({}) here", clip.display()), dir.path());

    // This is the markdown form BOTH families can read, and it is the one that
    // makes the order load-bearing: had the image walk gone first it would have
    // answered `UnsupportedFormat` — a true statement about the bytes and a
    // false notice for a reader who is about to receive that file as a video.
    assert_eq!(vw.entries.len(), 1, "the video family claims the reference");
    assert_eq!(vw.entries[0].video.path, clip);
    assert!(
        rw_stripped.failures.is_empty(),
        "the image walk never saw it, so it has no verdict to give: {:?}",
        rw_stripped.failures
    );
    assert!(rw_stripped.entries.is_empty(), "and nothing to attach");

    let failures = intermediate_failures(&rw_stripped, &vw);
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
    let (_rw_rich, rw_stripped, vw) =
        walks(&format!("x <<IMG:{}>> y", absent.display()), dir.path());

    let failures = intermediate_failures(&rw_stripped, &vw);
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
    let (_rw_rich, rw_stripped, vw) =
        walks(&format!("x <<VID:{}>> y", absent.display()), dir.path());

    assert!(vw.entries.is_empty(), "nothing to attach");
    let failures = intermediate_failures(&rw_stripped, &vw);
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
    // #732: the image walk runs TWICE — on the video family's rich form for the
    // rich plane (which is what the media array answers) and on its stripped
    // form for the HTML plane and the dedup record.
    let image_rich = DELIVERY_SRC
        .find("rewrite_local_images(&vw.rich, Some(cwd), \"img\", &delivered)")
        .expect("the intermediate path must run the image walk on the video family's rich form");
    let image_stripped = DELIVERY_SRC
        .find("rewrite_local_images(&vw.stripped, Some(cwd), \"img\", &delivered)")
        .expect("the intermediate path must run the image walk on the video family's stripped form");

    assert!(
        video < image_rich && video < image_stripped,
        "the image walk consumes a clip's reference, so the video walk must \
         claim it first (video at {video}, rich image at {image_rich}, \
         stripped image at {image_stripped})"
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

    // #732: the image family now arrives as TWO rewrites. Handing the delivery
    // the rich one alone would put a `tg://` reference into the HTML body, where
    // no media array exists to answer it; handing it the stripped one alone
    // would leave the clip's own reference unanswered in the rich body and the
    // clip would have to fall back to its own bubble — the defect itself.
    assert!(
        args.contains("rich_images: &rw_rich")
            && args.contains("stripped_images: &rw_stripped")
            && args.contains("videos: &vw"),
        "the delivery needs BOTH image rewrites AND the video walk — a single \
         image rewrite cannot serve both planes: {args}"
    );
    // #1918: the delivery also needs the session's base directory. The FILE
    // pass runs INSIDE `deliver_intermediate_message` (over the two reflowed
    // forms), and it resolves a relative link against that base — exactly as
    // the final leg does. Without it a relative file link in a promoted
    // intermediate could not resolve at all.
    assert!(
        args.contains("cwd,"),
        "the intermediate delivery must be handed the base directory: the file \
         pass inside it resolves relative links against it, the same way the \
         final leg does: {args}"
    );
}

/// #1918: the file pass runs LAST on the intermediate plane, over the text the
/// image family already emptied — the same one-owner-per-reference order the
/// final leg uses. Pinned as a site because `handle_intermediate` needs a live
/// bot; the pass itself is covered by the pure tests in
/// `telegram_local_image_delivery_test.rs`.
#[test]
fn the_intermediate_path_scans_files_over_the_image_walks_output() {
    let scan = DELIVERY_SRC
        .find("extract_local_files(&rw_stripped.stripped, Some(cwd))")
        .expect(
            "the intermediate path must run the file pass over the image family's \
             stripped output — the same order the final leg uses",
        );
    let image_stripped = DELIVERY_SRC
        .find("rewrite_local_images(&vw.stripped, Some(cwd), \"img\", &delivered)")
        .expect("the image walk must have run");
    assert!(
        image_stripped < scan,
        "the file pass consumes what the image family left, so it must run \
         AFTER the image walk (image at {image_stripped}, file at {scan})"
    );
    // #1918 INVERTS the rich half of this pin. The rich body is no longer a
    // second `extract_local_files` pass (that pass emits the MARKER form, which
    // the rich plane cannot inline from); it is the FILE walk's own `rich`,
    // built on the image family's rich form so the `tg://document?id=docN`
    // reference survives for the shared media array to answer. The marker form
    // stays exactly where it belongs: the stripped form the HTML fallback and
    // the dedup record use, on a plane that has no media array — a `tg://`
    // reference there would ship as dead visible markdown.
    assert!(
        INTERMEDIATES_SRC.contains("walks.rich_files.rich.as_str()")
            && INTERMEDIATES_SRC
                .contains("extract_local_files(stripped_expanded.as_str(), Some(base_dir))"),
        "the intermediate must carry a file form in BOTH planes — the rich body \
         answers its own tg://document reference against the media array, while \
         the HTML fallback and the dedup record keep the marker"
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
fn the_intermediate_media_array_carries_the_clip_beside_the_pictures() {
    // #732 INVERTS this pin. Before the fix the array was photo-only, and the
    // clip could only leave as its own bubble — which is the defect. The clip
    // now rides the SAME array as the pictures, each entry carrying its own
    // kind, and the rich body built on `rw_rich` answers its `tg://video`
    // reference in place.
    //
    // The pre-fix tree fails this assertion, so it doubles as the control that
    // the fix is actually present rather than merely untested.
    assert!(
        INTERMEDIATES_SRC.contains("MediaKind::Video"),
        "the clip must be lifted into the intermediate rich media array — \
         otherwise it can only ship as a detached bubble (#732)"
    );
}

#[test]
fn the_rich_body_keeps_the_clip_reference_the_stripped_body_drops() {
    // The whole point of the dual-base rewrite, stated as one behavioural
    // assertion: the SAME clip leaves TWO different bodies, and only one of them
    // may carry the reference.
    //
    //  * rich — `rw_rich`, built on `vw.rich` — MUST answer the media array's
    //    video entry, so the clip inlines in the rich body.
    //  * stripped — `rw_stripped`, built on `vw.stripped` — reaches the HTML
    //    plane, which has NO media array, and the dedup record. A `tg://video`
    //    reference there is dead visible markdown, so it must be absent.
    let dir = tempfile::tempdir().expect("tempdir");
    let clip = fixture(dir.path(), "clip.mp4", MP4_BYTES);
    let (rw_rich, rw_stripped, vw) = walks(
        &format!("render done <<VID:{}>>", clip.display()),
        dir.path(),
    );

    let reference = format!("![video](tg://video?id={VID_ID_PREFIX}0)");
    assert!(
        rw_rich.rich.contains(&reference),
        "the rich body must answer the media array's video entry, or the clip \
         can only fall back to its own bubble: {:?}",
        rw_rich.rich
    );
    assert!(
        !rw_stripped.stripped.contains("tg://video"),
        "the HTML/dedup body must carry NO dead reference — it has no media \
         array to answer one: {:?}",
        rw_stripped.stripped
    );
    assert_eq!(vw.entries.len(), 1, "and the clip is still the video family's");
}

// ---------------------------------------------------------------------------
// #1918 — the FILE family on the intermediate plane
// ---------------------------------------------------------------------------

/// The composition `handle_intermediate` performs for the file family (#1918):
/// build the two image rewrites, then run the FILE pass over the image family's
/// output — LAST, exactly as the final leg does. A promoted intermediate is the
/// bubble the reader SEES whenever the reply names a fresh image (the final leg
/// then skips that picture via `delivered_image_paths`), so a local-file link in
/// such a reply must leave a marker there rather than vanish.
#[test]
fn the_intermediate_file_pass_marks_a_link_in_both_planes() {
    const PDF_BYTES: &[u8] = b"%PDF-1.7\n1 0 obj\n<<>>\nendobj\n";
    let dir = tempfile::tempdir().expect("tempdir");
    let pic = fixture(dir.path(), "pic.png", PNG_BYTES);
    let doc = fixture(dir.path(), "q3-report.pdf", PDF_BYTES);
    let (rw_rich, rw_stripped, _vw) = walks(
        &format!(
            "![pic]({}) then [Q3 report]({})",
            pic.display(),
            doc.display()
        ),
        dir.path(),
    );

    // The picture is the image family's: it rides the rich media array...
    assert_eq!(rw_rich.entries.len(), 1, "the picture rides the media array");
    assert!(
        rw_rich.rich.contains("![pic](tg://photo?id=img0)"),
        "the rich body answers the media array: {:?}",
        rw_rich.rich
    );
    // ...and the file link is left to the file pass, in BOTH planes.
    for (plane, form) in [("rich", &rw_rich.rich), ("stripped", &rw_stripped.stripped)] {
        let scan = crate::utils::extract_local_files(form, Some(dir.path()));
        assert_eq!(
            scan.attachments.len(),
            1,
            "{plane}: the file link must still resolve to one document: {:?}",
            scan.text
        );
        assert!(
            scan.text.contains("📎 Q3 report"),
            "{plane}: the reference becomes a marker, not a hole: {:?}",
            scan.text
        );
        assert!(
            !scan.text.contains(doc.to_str().unwrap()),
            "{plane}: no raw path may survive in the body: {:?}",
            scan.text
        );
    }
    // The file pass never eats the picture's rich reference: the `!` guard in
    // `extract_local_files` is what stops the two families claiming one
    // reference (#1918). If it ever regressed, the picture would lose its
    // marker position in the very body the media array answers.
    let rich_files = crate::utils::extract_local_files(&rw_rich.rich, Some(dir.path()));
    assert!(
        rich_files.text.contains("![pic](tg://photo?id=img0)"),
        "the file pass must leave the image family's rich reference verbatim: {:?}",
        rich_files.text
    );
}

/// The RICH half of the #1918 intermediate composition, which the test above
/// deliberately does NOT cover: the file walk runs on the image family's RICH
/// form, and what it leaves there is a `tg://document?id=docN` REFERENCE — not
/// the marker. The marker is the HTML plane's form; putting it in the rich body
/// would leave the media array's document entry with nothing to attach to, and
/// the reader would get a detached bubble where the reply meant an inline one.
///
/// This is the Step-6 acceptance: the rich body carries the document reference,
/// and the ownership predicate folds that kind into `documents` so the file
/// floor below is suppressed — the one-owner rule the image and video families
/// already obey.
#[test]
fn the_intermediate_rich_body_carries_the_document_reference_and_owns_the_floor() {
    use crate::channels::telegram::rich::mermaid::{MediaEntry, MediaKind, rich_media_ownership};
    use crate::utils::image::{DOC_ID_PREFIX, rewrite_local_files};

    const PDF_BYTES: &[u8] = b"%PDF-1.7\n1 0 obj\n<<>>\nendobj\n";
    let dir = tempfile::tempdir().expect("tempdir");
    let pic = fixture(dir.path(), "pic.png", PNG_BYTES);
    let doc = fixture(dir.path(), "q3-report.pdf", PDF_BYTES);
    let (rw_rich, _rw_stripped, _vw) = walks(
        &format!(
            "![pic]({}) then [Q3 report]({})",
            pic.display(),
            doc.display()
        ),
        dir.path(),
    );

    // The file walk runs LAST, on the image family's rich form — the order the
    // intermediate performs.
    let fw = rewrite_local_files(&rw_rich.rich, Some(dir.path()), DOC_ID_PREFIX, &[]);
    assert_eq!(fw.entries.len(), 1, "one document resolved");
    assert!(
        fw.rich.contains("tg://document?id=doc0"),
        "the RICH body must carry the document REFERENCE the media array \
         answers — not the marker, which belongs to the HTML plane: {:?}",
        fw.rich
    );
    assert!(
        !fw.rich.contains(doc.to_str().unwrap()),
        "no raw path may survive in the rich body: {:?}",
        fw.rich
    );
    // The picture's own reference survives the file walk untouched: the `!`
    // guard is what keeps one reference from being claimed twice.
    assert!(
        fw.rich.contains("![pic](tg://photo?id=img0)"),
        "the file walk must leave the image family's rich reference verbatim: {:?}",
        fw.rich
    );

    // The ownership predicate folds the document kind in, so a body whose only
    // fresh media is a document suppresses the FILE floor alone.
    let document_entry = MediaEntry {
        kind: MediaKind::Document,
        id: "doc0".into(),
        url: None,
        bytes: Some(PDF_BYTES.to_vec()),
        name: Some("q3-report.pdf".into()),
    };
    let owned = rich_media_ownership(true, std::slice::from_ref(&document_entry));
    assert!(
        owned.documents,
        "a document entry must make the rich plane own the file family — \
         otherwise the floor ships the document a second time, detached"
    );
    assert!(
        !owned.images && !owned.videos,
        "the document kind must not be mistaken for a picture or a clip"
    );
    assert!(owned.any(), "the union gates the rich send itself");
    // The negative control: with the plane declined, NOTHING is owned, so the
    // floor is not suppressed for a send that will not happen.
    assert!(
        !rich_media_ownership(false, std::slice::from_ref(&document_entry)).any(),
        "a declined rich plane owns nothing — the floor must still ship"
    );
}
