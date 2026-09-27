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

use crate::channels::telegram::flow::SentBubble;
use crate::channels::telegram::intermediates::{
    normalize_for_dedup, promoted_bubble_is_burial_evidence, superseded_ids,
};
use teloxide::types::MessageId;

fn bubble(text: &str, ids: &[i32]) -> SentBubble {
    SentBubble {
        text: text.to_string(),
        ids: ids.iter().copied().map(MessageId).collect(),
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
