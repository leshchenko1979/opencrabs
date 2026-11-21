//! The queue keeps context and display apart (#765).
//!
//! A synthetic entry carries the full result for the model and a compact tag
//! for the transcript. Collapsing the two when draining published a background
//! task's whole `[System: ...]` block, command and output included, as a user
//! turn in chat.

use crate::brain::agent::QueuedUserMessage;
use crate::channels::telegram::handler::{build_midturn_queued_message, join_midturn_batch};

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
    let bob = "\u{0411}\u{043e}\u{0431} (@bob): [Forwarded from \"Carol Smith\"]:\nstaging is down";

    let queued = vec![
        build_midturn_queued_message(None, alice, alice),
        build_midturn_queued_message(None, bob, bob),
    ];
    let joined = join_midturn_batch(queued).expect("both entries join");

    // #885: the batch carries ONE wrapper around the whole block, not one per
    // message. Two wrapped items joined per-item used to repeat the 443-char
    // framing twice around two short lines.
    assert_eq!(
        joined
            .context_text
            .matches("[Message(s) arrived while you were still working")
            .count(),
        1,
        "a joined burst must carry exactly one wrapper: {}",
        joined.context_text
    );

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

/// #885: a plain burst is wrapped ONCE. Before this, `join` concatenated each
/// item's already-wrapped `context_text`, so N messages meant N copies of the
/// 443-char instruction around N short lines — 9.3x framing-to-payload on the
/// live 4-message burst that surfaced it.
#[test]
fn a_plain_burst_carries_one_wrapper_around_all_its_lines() {
    let queued = vec![
        build_midturn_queued_message(None, "alice (@alice): one", "alice (@alice): one"),
        build_midturn_queued_message(None, "alice (@alice): two", "alice (@alice): two"),
        build_midturn_queued_message(None, "alice (@alice): three", "alice (@alice): three"),
    ];
    let joined = join_midturn_batch(queued).expect("burst joins");

    assert_eq!(
        joined
            .context_text
            .matches("[Message(s) arrived while you were still working")
            .count(),
        1,
        "one wrapper per burst, not per message: {}",
        joined.context_text
    );
    // Every payload line survives, in arrival order, under the single wrapper.
    let payload = joined
        .context_text
        .split_once("]:\n")
        .expect("the wrapper ends with its payload")
        .1;
    assert_eq!(
        payload,
        "alice (@alice): one\nalice (@alice): two\nalice (@alice): three"
    );
    // The transcript half stays bare.
    assert_eq!(
        joined.display_text,
        "alice (@alice): one\nalice (@alice): two\nalice (@alice): three"
    );
}

/// #885: a slash command is a NEW directive, so it keeps its OWN wrapper even
/// when it lands mid-burst — folding it under the follow-up framing neutralises
/// it (the `/drop_release` report).
#[test]
fn a_command_mid_burst_keeps_its_own_wrapper() {
    let queued = vec![
        build_midturn_queued_message(None, "alice (@alice): status?", "alice (@alice): status?"),
        build_midturn_queued_message(
            Some("/drop_release"),
            "run the release drop",
            "/drop_release",
        ),
    ];
    let joined = join_midturn_batch(queued).expect("mixed burst joins");

    assert!(
        joined
            .context_text
            .contains("[The /drop_release command was invoked"),
        "the command keeps its own directive framing: {}",
        joined.context_text
    );
    assert!(
        joined.context_text.contains("explicit NEW directive"),
        "the command framing must survive the join: {}",
        joined.context_text
    );
    // The plain half is still wrapped exactly once; the command half is not a
    // follow-up and must not inherit the follow-up wrapper.
    assert_eq!(
        joined
            .context_text
            .matches("[Message(s) arrived while you were still working")
            .count(),
        1,
        "only the plain item carries the follow-up wrapper: {}",
        joined.context_text
    );
}

/// A single queued message is untouched by the batch path.
#[test]
fn a_single_followup_still_gets_exactly_one_wrapper() {
    let queued = vec![build_midturn_queued_message(
        None,
        "bob (@bob): just one",
        "bob (@bob): just one",
    )];
    let joined = join_midturn_batch(queued).expect("single item joins");
    assert_eq!(
        joined
            .context_text
            .matches("[Message(s) arrived while you were still working")
            .count(),
        1
    );
    assert!(joined.context_text.ends_with("bob (@bob): just one"));
}
