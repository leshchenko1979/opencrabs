//! #620 — the rich-fallback burial must delete only the intermediates it
//! actually re-sends.
//!
//! The burial arm used to delete EVERY id in the intermediate list on the
//! premise that the final rich message "REPLACES the intermediates it deletes".
//! That premise is true of exactly one intermediate: the one whose text the
//! final answer repeats. A narration bubble the rich message does not carry is
//! the sole copy of that prose, and deleting it loses the text with no trace —
//! the reader sees a gap and nothing reports it.
//!
//! `superseded_ids` replaces that premise. It is pure and free for the same
//! reason `promoted_bubble_is_burial_evidence` (#617) is: both directions —
//! selected and spared — are testable without a live bot.
//!
//! Cases:
//! (a) the #620 core — A, B, C where the re-sent text equals C: only C's ids.
//! (b) one text, N ids — a bubble chunked into several messages is superseded
//!     WHOLE, never partially.
//! (c) whitespace tolerance — the comparison is normalized, so a bubble that
//!     differs from the final text only in whitespace is still superseded.
//! (d) #617 composition — a media bubble carries `ids: vec![]` and can never be
//!     selected, so the media guard stays load-bearing.
//! (e) negative control — no bubble matches, so nothing is deleted.
//! (f) #788 — the seam the arm actually got wrong: the STORED body carries a
//!     `📎 label` marker, the WIRE body carries `[📎 label](url)`. The two are
//!     not interchangeable, and passing the wire form deleted nothing.

use crate::channels::telegram::flow::SentBubble;
use crate::channels::telegram::intermediates::{
    fallback_would_duplicate, intermediate_file_links, normalize_for_dedup,
    promoted_bubble_is_burial_evidence, superseded_ids,
};
use std::path::PathBuf;
use teloxide::types::MessageId;

fn bubble(text: &str, ids: &[i32]) -> SentBubble {
    SentBubble {
        text: text.to_string(),
        ids: ids.iter().copied().map(MessageId).collect(),
        // Most cases here are about the TEXT/ID pairing; a bubble that carried
        // no document records none (#1939). The file-link cases build their own.
        delivered_files: Vec::new(),
    }
}

/// A bubble that delivered `files`, as `(path, bubble id)` pairs — the shape
/// [`SentBubble::delivered_files`] holds (#1939). On the rich plane these are
/// the documents that rode this bubble's media array; on the HTML plane they
/// are the separate document bubbles this message produced.
fn bubble_with_files(text: &str, ids: &[i32], files: &[(&str, i32)]) -> SentBubble {
    SentBubble {
        text: text.to_string(),
        ids: ids.iter().copied().map(MessageId).collect(),
        delivered_files: files
            .iter()
            .map(|(path, id)| (PathBuf::from(path), *id))
            .collect(),
    }
}

fn ids_of(ids: &[i32]) -> Vec<MessageId> {
    ids.iter().copied().map(MessageId).collect()
}

/// (a) The #620 core. Three narration bubbles were delivered; the final answer
/// repeats only the third. Only that one's id may enter the delete set — the
/// other two are the sole copies of their prose.
#[test]
fn only_the_bubble_the_rich_message_repeats_is_superseded() {
    let bubbles = vec![
        bubble("Checking the deploy log now.", &[11]),
        bubble("The build is still running.", &[12]),
        bubble("Deploy finished: all 11 steps green.", &[13]),
    ];

    let got = superseded_ids(&bubbles, "Deploy finished: all 11 steps green.");

    assert_eq!(got, ids_of(&[13]));
    // The spared pair is exactly the silent-loss direction #620 reports: these
    // two carry text the rich message does not, so deleting them loses it.
    assert!(!got.contains(&MessageId(11)));
    assert!(!got.contains(&MessageId(12)));
}

/// (b) One text, N ids. A bubble long enough to be chunked occupies several
/// messages; the delete set must take the WHOLE bubble or none of it. A prefix
/// would leave orphaned fragments of a superseded narration on screen.
#[test]
fn a_chunked_bubble_is_superseded_whole() {
    let long = "x".repeat(9000);
    let bubbles = vec![bubble(&long, &[21, 22, 23])];

    let got = superseded_ids(&bubbles, &long);

    assert_eq!(got, ids_of(&[21, 22, 23]));
}

/// (c) Whitespace tolerance. The streamed intermediate and the final answer can
/// differ only in whitespace. Both consumers — the dedup that suppresses the
/// final send, and this burial filter — must agree on what "the same text"
/// means, which is why the comparison lives in ONE function.
#[test]
fn whitespace_only_differences_still_supersede() {
    let bubbles = vec![bubble("Deploy  finished:\n  all green.\n", &[31])];

    let got = superseded_ids(&bubbles, "Deploy finished: all green.");

    assert_eq!(got, ids_of(&[31]));
    assert_eq!(
        normalize_for_dedup("Deploy  finished:\n  all green.\n"),
        normalize_for_dedup("Deploy finished: all green.")
    );
}

/// (d) #617 composition. A promoted bubble that carried media holds the only
/// copy of that picture, so its ids are kept out of the list entirely. It
/// therefore arrives here with `ids: vec![]` and contributes nothing — even
/// when its text matches the final answer exactly. The guard is not duplicated
/// in `superseded_ids`; it is relied on.
#[test]
fn a_media_bubble_can_never_be_superseded() {
    assert!(promoted_bubble_is_burial_evidence(0));
    assert!(!promoted_bubble_is_burial_evidence(1));

    let bubbles = vec![
        // promoted bubble: media carried, so the guard left its ids out
        bubble("Here is the screenshot.", &[]),
        bubble("Here is the screenshot.", &[41]),
    ];

    let got = superseded_ids(&bubbles, "Here is the screenshot.");

    assert_eq!(got, ids_of(&[41]));
}

/// (e) Negative control. When the final answer repeats no intermediate, nothing
/// is deleted. This direction was entirely absent before #620 — the arm deleted
/// the whole list regardless of whether the rich message carried the text.
#[test]
fn no_match_deletes_nothing() {
    let bubbles = vec![
        bubble("Narration one.", &[51]),
        bubble("Narration two.", &[52]),
    ];

    assert!(superseded_ids(&bubbles, "An entirely different final answer.").is_empty());
    assert!(superseded_ids(&[], "anything at all").is_empty());
}

/// The shared normalizer's contract, pinned because both the dedup and the
/// burial filter depend on it agreeing with itself across call sites.
#[test]
fn normalize_collapses_all_whitespace_runs() {
    assert_eq!(normalize_for_dedup("a\n\n b\tc "), "a b c");
    assert_eq!(normalize_for_dedup(""), "");
    assert_eq!(normalize_for_dedup("   "), "");
}

// ---------------------------------------------------------------------------
// #1939 — the fallback must not duplicate an undeletable intermediate
// ---------------------------------------------------------------------------

/// A promoted intermediate that carried MEDIA records EMPTY ids (#617), so
/// [`superseded_ids`] cannot select it: the fallback would delete nothing and
/// still re-send the body, leaving the reader with it twice and orphaning the
/// `📎` markers (the documents rode the intermediate, so the re-sent copy's
/// markers point at a bubble that holds nothing). `fallback_would_duplicate` is
/// what stops that, and it must fire ONLY for the undeletable case.
#[test]
fn a_media_intermediate_that_carries_the_final_body_suppresses_the_resend() {
    let final_body = "Here is the report, inline.";
    // Media-bearing: empty ids, and it carries the document the body names.
    let media = bubble_with_files(final_body, &[], &[("/tmp/q3.pdf", 90_465)]);

    assert!(
        fallback_would_duplicate(std::slice::from_ref(&media), final_body),
        "a media-bearing intermediate already carries this body, so the \
         fallback's re-send would duplicate it"
    );
    // The premise, pinned: it is undeletable, so the burial arm would delete
    // nothing and the duplicate would survive the cleanup.
    assert!(superseded_ids(std::slice::from_ref(&media), final_body).is_empty());

    // Negative control — the deletable case. An HTML bubble WITH ids is
    // superseded, so the fallback must still run and replace it: suppressing
    // here would leave the reader with the stale copy and no final.
    let html = bubble(final_body, &[7]);
    assert!(!fallback_would_duplicate(std::slice::from_ref(&html), final_body));
    assert_eq!(
        superseded_ids(std::slice::from_ref(&html), final_body),
        ids_of(&[7])
    );

    // And a media bubble whose text DIFFERS does not suppress an unrelated body.
    assert!(!fallback_would_duplicate(
        std::slice::from_ref(&media),
        "An entirely different answer."
    ));
}

/// The fallback leg's own `file_links` is empty whenever the rich plane owned
/// the documents — nothing was sent there, so nothing could be linked. The
/// document's address is the INTERMEDIATE's bubble, and that is what the
/// predicate recovers: the `(path, id)` pair for each file a SUPERSEDED bubble
/// delivered, so the `📎` marker can point at the bubble that holds it.
#[test]
fn the_fallback_recovers_the_intermediates_delivered_files() {
    let body = "Report attached.";
    let media = bubble_with_files(body, &[], &[("/tmp/q3.pdf", 90_465)]);
    let narration = bubble_with_files("Earlier narration.", &[5], &[("/tmp/other.pdf", 90_400)]);

    let got = intermediate_file_links(&[narration, media], body);

    assert_eq!(
        got,
        vec![(PathBuf::from("/tmp/q3.pdf"), 90_465)],
        "only the bubble the fallback supersedes contributes its files — a \
         narration bubble's documents belong to a bubble that survives"
    );
    // Negative control: when no bubble matches, nothing is recovered, so every
    // marker stays plain rather than pointing at the wrong document.
    assert!(intermediate_file_links(&[], body).is_empty());
    assert!(intermediate_file_links(&[bubble("Something else.", &[6])], body).is_empty());
}

/// #788 — the burial arm compared the REWRITTEN body against the STORED one.
///
/// A bubble's `text` is the body as it stood when the bubble was sent, and for
/// a marker-bearing answer that body still carries the plain `📎 label` form.
/// The rich plane, by contrast, sends `link_file_markers(&body, …)`, which
/// expands every marker into `[📎 label](url)` so the reference becomes a
/// tappable link to the document bubble. The two strings differ by the link
/// targets, and `normalize_for_dedup` only collapses whitespace, so handing the
/// wire form to `superseded_ids` could never match a bubble: the delete set
/// came back empty, the intermediate survived, and the reader got the answer
/// twice while the log said `deleted 0 superseded HTML intermediates`.
///
/// The case the rest of this file never exercised: every other test passes the
/// SAME text form on both sides, which is exactly why the defect shipped.
#[test]
fn the_burial_compares_the_stored_marker_form_not_the_rewritten_wire_form() {
    // The body as the bubble recorded it: marker form, no link target.
    let stored = "Report ready.\n\n📎 q3-report.md";
    // The body as the rich plane put it on the wire: the marker expanded into a
    // link pointing at the detached document bubble.
    let wire = "Report ready.\n\n[📎 q3-report.md](https://t.me/c/3936827469/31847/93956)";
    let b = bubble(stored, &[93_956]);

    // The premise of the defect: the rewrite GROWS the buffer, so the two forms
    // cannot normalize to the same string.
    assert!(
        wire.len() > stored.len(),
        "the marker rewrite adds the link target, so the wire body is longer"
    );
    assert_ne!(normalize_for_dedup(stored), normalize_for_dedup(wire));

    // The fix: the arm passes the stored form, and the bubble is superseded.
    assert_eq!(
        superseded_ids(std::slice::from_ref(&b), stored),
        ids_of(&[93_956]),
        "the body the bubbles stored is what the burial must compare against"
    );

    // The #788 signature, pinned as a negative control: the wire form finds
    // nothing. If this ever starts matching, the two forms have collapsed and
    // the fix's premise needs re-reading — but while they differ, passing the
    // wire form is the duplicate send.
    assert!(
        superseded_ids(std::slice::from_ref(&b), wire).is_empty(),
        "the rewritten body must never select a bubble that stored the marker form"
    );

    // And the bubble is deletable, so the duplicate guard does NOT suppress the
    // send: the burial is the only thing standing between the reader and a
    // second copy of the answer.
    assert!(!fallback_would_duplicate(std::slice::from_ref(&b), stored));
}
