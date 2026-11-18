//! The queue keeps context and display apart (#765).
//!
//! A synthetic entry carries the full result for the model and a compact tag
//! for the transcript. Collapsing the two when draining published a background
//! task's whole `[System: ...]` block, command and output included, as a user
//! turn in chat.

use crate::brain::agent::QueuedUserMessage;
use crate::channels::telegram::handler::build_midturn_queued_message;

#[test]
fn an_empty_queue_injects_nothing() {
    assert!(QueuedUserMessage::join(&[]).is_none());
}

#[test]
fn a_system_entry_keeps_its_compact_display() {
    // The exact shape that leaked: the model must still read the full result.
    let queued = vec![QueuedUserMessage::system(
        "[System: the background task you started has finished. Command: cargo test …]".to_string(),
        "\u{2705} cargo test \u{1f552} 42s".to_string(),
    )];
    let joined = QueuedUserMessage::join(&queued).expect("one entry joins");
    assert_eq!(
        joined.display_text, "\u{2705} cargo test \u{1f552} 42s",
        "the transcript must never receive the scaffolding"
    );
    assert!(
        joined.context_text.contains("Command: cargo test"),
        "the model still needs the full result"
    );
}

#[test]
fn a_typed_message_reads_the_same_on_both_sides() {
    let queued = vec![QueuedUserMessage::plain("do the thing".to_string())];
    let joined = QueuedUserMessage::join(&queued).expect("one entry joins");
    assert_eq!(joined.context_text, "do the thing");
    assert_eq!(joined.display_text, "do the thing");
}

#[test]
fn mixed_entries_join_each_half_independently() {
    // A typed follow-up landing next to a background completion must not drag
    // the completion's context into the visible transcript.
    let queued = vec![
        QueuedUserMessage::plain("also check the logs".to_string()),
        QueuedUserMessage::system(
            "[System: task finished. Output: …50 lines…]".to_string(),
            "\u{2705} cargo clippy \u{1f552} 1m 3s".to_string(),
        ),
    ];
    let joined = QueuedUserMessage::join(&queued).expect("joins");
    assert_eq!(
        joined.display_text,
        "also check the logs\n\u{2705} cargo clippy \u{1f552} 1m 3s"
    );
    assert_eq!(
        joined.context_text,
        "also check the logs\n[System: task finished. Output: …50 lines…]"
    );
}

#[test]
fn ordering_is_preserved_on_both_halves() {
    let queued = vec![
        QueuedUserMessage::plain("first".to_string()),
        QueuedUserMessage::plain("second".to_string()),
        QueuedUserMessage::plain("third".to_string()),
    ];
    let joined = QueuedUserMessage::join(&queued).expect("joins");
    assert_eq!(joined.display_text, "first\nsecond\nthird");
    assert_eq!(joined.context_text, "first\nsecond\nthird");
}

/// #882: a drained burst can carry messages from DIFFERENT senders, including
/// forwarded material. Each item's label must survive the join attached to its
/// own line, and the wrapper must not claim the block has one speaker.
#[test]
fn two_senders_keep_their_own_labels_and_forward_tags() {
    let alice = "\u{0410}\u{043b}\u{0438}\u{0441}\u{0430} (@alice): deploy is broken";
    let bob =
        "\u{0411}\u{043e}\u{0431} (@bob): [Forwarded from \"Carol Smith\"]:\nstaging is down";

    let queued = vec![
        build_midturn_queued_message(None, alice, alice),
        build_midturn_queued_message(None, bob, bob),
    ];
    let joined = QueuedUserMessage::join(&queued).expect("both entries join");

    // Both labels survive, in arrival order, on the display half too.
    assert_eq!(
        joined.display_text,
        format!("{alice}\n{bob}"),
        "each sender's label must ride into the transcript on its own line"
    );

    // The model sees both senders.
    assert!(joined.context_text.contains("(@alice)"));
    assert!(joined.context_text.contains("(@bob)"));

    // The forward tag belongs to the forwarded person's line, never the
    // forwarder's: Bob forwarded Carol, so only Bob's line carries the tag.
    let bob_line = joined
        .context_text
        .lines()
        .find(|l| l.contains("(@bob)"))
        .expect("bob's line is present");
    assert!(
        bob_line.contains("[Forwarded from \"Carol Smith\"]"),
        "the forward tag must sit on the forwarding sender's line: {bob_line}"
    );
    let alice_line = joined
        .context_text
        .lines()
        .find(|l| l.contains("(@alice)"))
        .expect("alice's line is present");
    assert!(
        !alice_line.contains("Forwarded"),
        "a sender's own words must not inherit someone else's forward tag: {alice_line}"
    );

    // And the wrapper is sender-neutral: it may not assert one speaker.
    assert!(
        !joined.context_text.contains("The user sent this follow-up"),
        "the wrapper must not claim a single speaker for a multi-sender burst"
    );
    assert!(
        joined.context_text.contains("sender prefix"),
        "the wrapper must tell the agent that each line carries its own sender"
    );
    assert!(
        joined.context_text.contains("[Forwarded from"),
        "the wrapper must explain what a forward tag means"
    );
}
