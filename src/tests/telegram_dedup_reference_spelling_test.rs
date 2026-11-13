//! #826 AC-1/2: one bubble per reply — a reply already delivered on the rich
//! plane must not be re-shipped by the final leg in a degraded form.
//!
//! Measured 2026-11-12 on this box, a reply carrying six documents went out as
//! rich message `96054`, the final leg's dedup compared its own text against the
//! one recorded bubble, found no match, and sent `96056` 0.9 s later with every
//! document detached and both pictures gone.
//!
//! The texts were the same reply. Only the SPELLING of each file reference
//! differed, because each plane records its own form:
//!
//! | plane | the reference reads |
//! |---|---|
//! | the model's answer (the final leg) | `[report.pdf](/tmp/x.pdf)` |
//! | the rich body that goes out | `[report.pdf](tg://document?id=doc0)` |
//! | the `📎` marker recorded in `sent_bubbles` | `📎 report.pdf` |
//!
//! `normalize_for_dedup` collapses whitespace and nothing else, so it can never
//! match those. `normalize_reference_spelling` collapses the reference to its
//! label, which is the question the suppression arm is actually asking: does
//! this bubble already SAY this reply?

use crate::channels::telegram::intermediates::{
    normalize_for_dedup, normalize_reference_spelling,
};

/// The three spellings of one reply must compare equal — the measured failure.
#[test]
fn the_three_spellings_of_one_reply_compare_equal() {
    let model_form = "Attached.\n\n[report.pdf](/tmp/oc-probes/report.pdf) and \
                      [notes.md](/tmp/oc-probes/notes.md)";
    let rich_form = "Attached.\n\n[report.pdf](tg://document?id=doc0) and \
                     [notes.md](tg://document?id=doc1)";
    let marker_form = "Attached.\n\n📎 report.pdf and 📎 notes.md";

    let a = normalize_reference_spelling(model_form);
    let b = normalize_reference_spelling(rich_form);
    let c = normalize_reference_spelling(marker_form);
    assert_eq!(a, b, "the model's form and the rich form are one reply");
    assert_eq!(b, c, "the rich form and the marker form are one reply");

    // …and the OLD predicate could not see it, which is the defect: without
    // this contrast the test would pass against a predicate that never needed
    // to change.
    assert_ne!(
        normalize_for_dedup(model_form),
        normalize_for_dedup(marker_form),
        "a whitespace-only comparison must NOT match these — that is why the \
         duplicate shipped"
    );
}

/// A real hyperlink is NOT a media reference: two replies that link different
/// pages are different replies, and erasing that difference would swallow a
/// genuinely new answer.
#[test]
fn a_remote_hyperlink_keeps_its_target() {
    let a = "See [#825](https://github.com/leshchenko1979/opencrabs/issues/825) for the fix.";
    let b = "See [#826](https://github.com/leshchenko1979/opencrabs/issues/826) for the fix.";
    assert_ne!(
        normalize_reference_spelling(a),
        normalize_reference_spelling(b),
        "a different hyperlink is a different reply"
    );
    // The link itself survives intact — this predicate erases file and media
    // references only, never prose the author wrote.
    assert!(
        normalize_reference_spelling(a).contains("https://github.com/leshchenko1979/opencrabs/issues/825"),
        "a remote target must be left byte-identical"
    );
}

/// An image reference collapses to its alt text on every plane, including the
/// `![…]` embed form the rich plane emits for pictures and videos.
#[test]
fn an_image_reference_collapses_to_its_alt_text() {
    let cases = [
        "![chart](/tmp/chart.png)",
        "![chart](tg://photo?id=img0)",
        "![chart](file:///tmp/chart.png)",
    ];
    for case in cases {
        assert_eq!(
            normalize_reference_spelling(case),
            "chart",
            "an image reference must reduce to its label: {case}"
        );
    }
}

/// The marker's label is kept, and the emoji plus the space it introduced are
/// not — otherwise the two sides would still differ by one character.
#[test]
fn the_marker_emoji_does_not_survive_normalization() {
    assert_eq!(normalize_reference_spelling("📎 report.pdf"), "report.pdf");
    assert_eq!(normalize_reference_spelling("📎   spaced.pdf"), "spaced.pdf");
}

/// Text that is not a reference is untouched, and a half-written link is left
/// alone rather than guessed at.
#[test]
fn non_reference_text_is_untouched() {
    let prose = "Plain prose with a [bracket] and a (paren) and no link at all.";
    assert_eq!(normalize_reference_spelling(prose), prose);
    // `[label]` with no target is not a link; `[label](` never closes.
    assert_eq!(normalize_reference_spelling("[dangling]"), "[dangling]");
    assert_eq!(
        normalize_reference_spelling("[half](/tmp/x.pdf"),
        "[half](/tmp/x.pdf"
    );
}
