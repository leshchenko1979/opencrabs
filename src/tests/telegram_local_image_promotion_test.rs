//! The promotion-side half of #502: a mid-turn intermediate that names a local
//! image must reach the bubble that referenced it, and the rewrite that makes
//! that possible must produce BOTH text forms from ONE walk.
//!
//! Covered here (the media-plane half lives in
//! `telegram_local_image_media_test.rs`):
//!
//! - `should_promote_intermediate` — the fourth trigger, and the three existing
//!   arms it delegates to.
//! - `rewrite_local_images` — in-place `tg://photo?id=imgN` embedding, the
//!   stripped twin, the already-delivered skip, the failure arm, and the
//!   code-span/fence guard.
//! - the parity pin: the stripped form is byte-identical to what
//!   `strip_image_references` produces for the same input and base, so the fold
//!   path and the final-response dedup see exactly what they see today.

use crate::channels::telegram::intermediates::{
    is_deliverable_rich_report, should_promote_intermediate,
};
use crate::utils::image::{
    LocalImageFailureReason, rewrite_local_images, strip_image_references,
};
use std::path::{Path, PathBuf};

/// Minimal byte string that passes the magic-byte sniff for PNG.
const PNG_BYTES: &[u8] = b"\x89PNG\r\n\x1a\n\x00\x01\x02\x03\x04\x05\x06\x07";

fn write_fixture(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, bytes).expect("write fixture");
    path
}

fn ids(rw: &crate::utils::image::LocalImageRewrite) -> Vec<String> {
    rw.entries.iter().map(|e| e.id.clone()).collect()
}

// ---------------------------------------------------------------------------
// should_promote_intermediate — the fourth trigger
// ---------------------------------------------------------------------------

#[test]
fn a_fresh_image_promotes_thin_prose_that_would_otherwise_fold() {
    // The filed shape (#502): one sentence and a picture. No table, no fence,
    // no status report — nothing the three existing arms recognise.
    let thin = "Here is the chart I promised.";
    assert!(
        !is_deliverable_rich_report(thin),
        "sanity: the text really is unstructured, so the image is the only trigger"
    );
    assert!(
        should_promote_intermediate(thin, 1),
        "a fresh image must promote thin prose — folding it loses the picture"
    );
}

#[test]
fn no_fresh_image_leaves_thin_prose_folding() {
    assert!(
        !should_promote_intermediate("Let me check the logs.", 0),
        "with no image and no rich structure the intermediate keeps folding"
    );
}

#[test]
fn an_already_delivered_image_is_not_a_reason_to_promote_again() {
    // `fresh_images` is a count of images NOT yet in the chat, so a second
    // intermediate naming the same file passes 0 and folds — no second bubble
    // whose only content would be a picture the reader already has.
    assert!(!should_promote_intermediate("Same chart again.", 0));
}

#[test]
fn the_three_existing_arms_still_promote() {
    let table = format!(
        "| Metric | Value |\n|---|---|\n|{}",
        "| filler | filler |\n".repeat(12)
    );
    assert!(
        should_promote_intermediate(&table, 0),
        "a substantial table still promotes on its own (#582)"
    );

    let mermaid = "```mermaid\nflowchart TD\n  A --> B\n```";
    assert!(
        should_promote_intermediate(mermaid, 0),
        "a mermaid fence still promotes on its own"
    );

    // Rule A of `is_deliverable_status_report` wants a keyword heading AND
    // `line_count >= 2` AND `>= 50` chars — the first draft of this fixture was
    // 43 chars and legitimately failed the predicate. Same shape the existing
    // status-report corpus uses.
    let status = "### Status Update: Issue #502\nCarrier build dispatched.\nWaiting on the CI gate.";
    assert!(
        should_promote_intermediate(status, 0),
        "a status report still promotes on its own"
    );
}

// ---------------------------------------------------------------------------
// rewrite_local_images — one walk, two forms
// ---------------------------------------------------------------------------

#[test]
fn a_resolvable_reference_is_embedded_in_place_and_stripped_from_the_twin() {
    let dir = tempfile::tempdir().expect("tempdir");
    let png = write_fixture(dir.path(), "chart.png", PNG_BYTES);

    let text = format!("Here is the chart:\n\n![revenue]({})\n\nDone.", png.display());
    let rw = rewrite_local_images(&text, Some(dir.path()), "img", &[]);

    assert_eq!(ids(&rw), vec!["img0".to_string()]);
    assert!(
        rw.rich.contains("![revenue](tg://photo?id=img0)"),
        "the reference is rewritten IN PLACE, keeping the author's alt text: {}",
        rw.rich
    );
    assert!(
        !rw.rich.contains(&png.display().to_string()),
        "no filesystem path survives into the rich form"
    );
    assert!(
        !rw.stripped.contains("tg://photo"),
        "the stripped twin drops the reference entirely: {}",
        rw.stripped
    );
    assert!(rw.stripped.contains("Here is the chart:"));
    assert!(rw.stripped.contains("Done."));
    assert!(rw.failures.is_empty());
    assert_eq!(rw.entries[0].image.path, png);
}

#[test]
fn two_references_get_ids_in_order_of_appearance() {
    let dir = tempfile::tempdir().expect("tempdir");
    let a = write_fixture(dir.path(), "a.png", PNG_BYTES);
    let b = write_fixture(dir.path(), "b.png", PNG_BYTES);

    let text = format!("![first]({})\n\n![second]({})", a.display(), b.display());
    let rw = rewrite_local_images(&text, Some(dir.path()), "img", &[]);

    assert_eq!(ids(&rw), vec!["img0".to_string(), "img1".to_string()]);
    let first = rw.rich.find("img0").expect("img0 present");
    let second = rw.rich.find("img1").expect("img1 present");
    assert!(first < second, "ids follow order of appearance");
}

#[test]
fn a_markdown_title_never_leaks_into_the_rich_reference() {
    // The rich media plane has no caption field — `InputRichMessageMedia` is
    // `{id, media}` — so the title must not survive as markdown the rich
    // parser would render as literal text.
    let dir = tempfile::tempdir().expect("tempdir");
    let png = write_fixture(dir.path(), "chart.png", PNG_BYTES);

    let text = format!("![alt]({} \"Quarterly revenue\")", png.display());
    let rw = rewrite_local_images(&text, Some(dir.path()), "img", &[]);

    assert_eq!(rw.rich, "![alt](tg://photo?id=img0)");
    assert!(
        !rw.rich.contains("Quarterly revenue"),
        "the title is dropped, not carried as dead text"
    );
}

#[test]
fn a_relative_reference_resolves_against_the_session_cwd() {
    let dir = tempfile::tempdir().expect("tempdir");
    let png = write_fixture(dir.path(), "chart.png", PNG_BYTES);

    let rw = rewrite_local_images("![c](chart.png)", Some(dir.path()), "img", &[]);

    assert_eq!(ids(&rw), vec!["img0".to_string()]);
    assert_eq!(rw.entries[0].image.path, png);
}

#[test]
fn an_already_delivered_image_is_consumed_without_a_second_entry_or_failure() {
    let dir = tempfile::tempdir().expect("tempdir");
    let png = write_fixture(dir.path(), "chart.png", PNG_BYTES);

    let text = format!("![again]({})", png.display());
    // `from_ref` rather than `&[png.clone()]`: clippy's
    // `cloned_ref_to_slice_refs` is denied under `-D warnings`, and the clone
    // was pointless anyway — the slice borrows the same path the assertion
    // below compares against.
    let rw = rewrite_local_images(&text, Some(dir.path()), "img", std::slice::from_ref(&png));

    assert!(rw.entries.is_empty(), "no second entry for a delivered picture");
    assert!(
        rw.failures.is_empty(),
        "a delivered picture is not a lost one — it must not be reported"
    );
    assert!(!rw.rich.contains("tg://photo"));
    assert!(!rw.stripped.contains("chart.png"));
}

#[test]
fn a_missing_path_becomes_one_not_found_failure_and_leaves_both_forms() {
    let dir = tempfile::tempdir().expect("tempdir");
    let missing = dir.path().join("nope.png");

    let text = format!("before\n\n![gone]({})\n\nafter", missing.display());
    let rw = rewrite_local_images(&text, Some(dir.path()), "img", &[]);

    assert!(rw.entries.is_empty());
    assert_eq!(rw.failures.len(), 1);
    assert_eq!(rw.failures[0].reason, LocalImageFailureReason::NotFound);
    assert!(
        !rw.stripped.contains("nope.png") && !rw.rich.contains("nope.png"),
        "a rejected reference leaves both forms — dead markdown never reaches the user"
    );
    assert!(rw.stripped.contains("before") && rw.stripped.contains("after"));
}

#[test]
fn references_inside_a_fence_or_a_code_span_are_byte_identical_in_both_forms() {
    let dir = tempfile::tempdir().expect("tempdir");
    let png = write_fixture(dir.path(), "chart.png", PNG_BYTES);
    let reference = format!("![inside]({})", png.display());

    let fenced = format!("```mermaid\n{reference}\n```");
    let rw = rewrite_local_images(&fenced, Some(dir.path()), "img", &[]);
    assert!(rw.entries.is_empty(), "a fence body is not a live reference");
    assert_eq!(rw.rich, fenced.trim());
    assert_eq!(rw.stripped, fenced.trim());

    let spanned = format!("`{reference}` and {reference}");
    let rw = rewrite_local_images(&spanned, Some(dir.path()), "img", &[]);
    assert_eq!(
        ids(&rw),
        vec!["img0".to_string()],
        "only the live reference outside the span becomes an entry"
    );
    assert!(
        rw.rich.contains(&format!("`{reference}`")),
        "the code span is left byte-identical: {}",
        rw.rich
    );
}

#[test]
fn the_stripped_twin_matches_strip_image_references_exactly() {
    // The parity pin. The fold path and the final-response dedup consume the
    // stripped form, so it must be byte-identical to what the pre-existing
    // scanner produced for the same input and base.
    let dir = tempfile::tempdir().expect("tempdir");
    let png = write_fixture(dir.path(), "chart.png", PNG_BYTES);

    // An array, not a `vec!`: the list is only iterated, and `useless_vec` is
    // denied under `-D warnings` like every other clippy lint in this gate.
    let cases = [
        "prose only, no reference at all".to_string(),
        format!("![c]({})", png.display()),
        format!("a\n\n![c]({})\n\nb", png.display()),
        format!("![missing]({})", dir.path().join("nope.png").display()),
        "![remote](https://example.invalid/x.png)".to_string(),
        "![rel](chart.png)".to_string(),
        format!("```\n![fenced]({})\n```", png.display()),
        format!("`![spanned]({})`", png.display()),
    ];

    for text in cases {
        let rw = rewrite_local_images(&text, Some(dir.path()), "img", &[]);
        let expected = strip_image_references(&text, Some(dir.path())).text;
        assert_eq!(
            rw.stripped, expected,
            "stripped twin diverged from strip_image_references for: {text}"
        );
    }
}

#[test]
fn the_stripped_twin_keeps_a_remote_link_like_the_strip_only_scanner() {
    // #286: a strip-only call site has no fetch step, so deleting a link would
    // lose it. The rewrite must inherit that behaviour, not "fix" it.
    let dir = tempfile::tempdir().expect("tempdir");
    let text = "see ![chart](https://example.invalid/chart.png) for details";
    let rw = rewrite_local_images(text, Some(dir.path()), "img", &[]);

    assert!(rw.entries.is_empty());
    assert_eq!(rw.stripped, text);
    assert_eq!(rw.rich, text);
}
