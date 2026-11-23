//! Regression pins for #859 — telling a parent its sub-agent is stuck.
//!
//! Before this, a child that reported a blocker or parked at a round boundary
//! did so in total silence: the ledger entry existed, but nothing woke the
//! parent, and a parked child is indistinguishable from a working one from the
//! outside. The parent either waited forever or reported half-finished work.
//!
//! Three things are proved here:
//!
//! 1. The message a parent receives says PARKED, never FINISHED. A parent that
//!    mistakes a blocker for a result reports unfinished work to the user,
//!    which is worse than not being told at all.
//! 2. The wake is throttled per agent, and the throttle is per agent — a
//!    second agent's first blocker must still land, or the throttle has
//!    become a mute button.
//! 3. Both call sites still notify, and both still go through the throttle.
//!    Source-shape pins in house style: they fail loudly when a later edit
//!    drops the guard instead of silently restoring the silence.

use crate::brain::tools::subagent::SubAgentManager;
use crate::brain::tools::subagent::spawn::attention_message;
use std::path::Path;

fn repo_path(rel: &str) -> String {
    let p = Path::new(env!("CARGO_MANIFEST_DIR")).join(rel);
    p.to_string_lossy().into_owned()
}

#[test]
fn a_blocker_reads_as_a_park_and_never_as_a_result() {
    // A child parked at a round boundary: still Running/AwaitingInput, so
    // `send_input` is the verb that reaches it.
    let msg = attention_message(
        "pdf-worker",
        "abc12345",
        "blocker",
        "pdftoppm needs 2 GB",
        true,
    );

    assert!(
        msg.context_text.contains("needs your input"),
        "a blocker must announce itself as input needed, got: {}",
        msg.context_text
    );
    assert!(
        msg.context_text.contains("parked, not finished"),
        "the parent must be told the child is parked, not done: {}",
        msg.context_text
    );
    assert!(
        msg.context_text.contains("Do not report"),
        "the message must forbid reporting the work as done: {}",
        msg.context_text
    );
    assert!(
        !msg.context_text.contains("has finished"),
        "a blocker must never borrow the completion framing: {}",
        msg.context_text
    );
    // The two ways out, both named: answering it and reading it.
    assert!(
        msg.context_text.contains("send_input abc12345"),
        "the answer path must carry the real id: {}",
        msg.context_text
    );
    assert!(
        msg.context_text.contains("agent_progress abc12345"),
        "the read path must carry the real id: {}",
        msg.context_text
    );
    // The child's own words travel with it — the parent decides from those,
    // not from a summary this code invented.
    assert!(
        msg.context_text.contains("pdftoppm needs 2 GB"),
        "the child's text must survive into the message: {}",
        msg.context_text
    );
}

/// The other half of #984, and the bug the old wording carried: a child that
/// reports a blocker and ENDS ITS TURN has completed, and `send_input` refuses
/// a completed agent outright. Naming it there sent the parent at the one verb
/// that cannot work, so the two arms must not share a sentence.
#[test]
fn a_child_that_ended_its_turn_is_continued_with_resume_not_send_input() {
    let msg = attention_message("pdf-worker", "abc12345", "question", "which bucket?", false);

    assert!(
        msg.context_text.contains("resume_agent abc12345"),
        "a finished turn must be continued with resume_agent, got: {}",
        msg.context_text
    );
    assert!(
        !msg.context_text.contains("send_input abc12345"),
        "send_input refuses a COMPLETED agent, so naming it here is a dead \
         end for the parent: {}",
        msg.context_text
    );
    assert!(
        !msg.context_text.contains("parked, not finished"),
        "it is not parked - it finished its turn: {}",
        msg.context_text
    );
    // Still not a result: the work is unfinished either way.
    assert!(
        msg.context_text.contains("Do not report"),
        "unfinished work must never read as done: {}",
        msg.context_text
    );
    assert!(
        msg.context_text.contains("agent_progress abc12345"),
        "the read path still carries the real id: {}",
        msg.context_text
    );
}

#[test]
fn the_display_tag_is_short_and_kind_specific() {
    let question = attention_message("w", "id1", "question", "which env?", false);
    let blocker = attention_message("w", "id1", "blocker", "no creds", true);
    let other = attention_message("w", "id1", "note", "just so you know", true);

    // The display half is what a channel shows; it must not leak the full
    // framed block (#765: collapsing the two published system scaffolding as
    // a user turn).
    for msg in [&question, &blocker, &other] {
        assert!(
            msg.display_text.chars().count() < 60,
            "display tag too long: {}",
            msg.display_text
        );
        assert!(
            !msg.display_text.contains("[System:"),
            "display tag must not carry the framed block: {}",
            msg.display_text
        );
    }
    assert_ne!(question.display_text, blocker.display_text);
    assert!(blocker.display_text.contains('w'), "label must appear");
}

#[test]
fn the_attention_wake_is_throttled_per_agent() {
    let manager = SubAgentManager::new();

    assert!(
        manager.attention_allowed("agent-a"),
        "the first blocker must notify"
    );
    assert!(
        !manager.attention_allowed("agent-a"),
        "a second blocker inside ATTENTION_FLOOR must not wake the parent again \
         - three blockers in a row are one problem, and every wake costs the \
         parent a message it reads past"
    );
    // The discriminating half: a different agent is untouched. Without this,
    // a throttle that simply returned false always would pass the two
    // assertions above and mute every child.
    assert!(
        manager.attention_allowed("agent-b"),
        "the throttle is per agent; a second child's first blocker must land"
    );
}

#[test]
fn report_progress_notifies_only_for_the_two_stuck_kinds() {
    let src = std::fs::read_to_string(repo_path("src/brain/tools/subagent/progress.rs"))
        .expect("cannot read progress.rs");

    assert!(
        src.contains("matches!(kind, LedgerKind::Blocker | LedgerKind::Question)"),
        "the notify guard must name exactly blocker and question - waking a \
         parent for note/decision is the noise that teaches it to \
         ignore the channel"
    );
    assert!(
        src.contains("attention_allowed(&id)"),
        "report_progress must go through the throttle"
    );
    assert!(
        src.contains("push_attention("),
        "report_progress must actually push; a ledger entry nobody is told \
         about is the pre-#859 behaviour"
    );
}

#[test]
fn the_round_boundary_park_notifies_through_the_throttle() {
    let src = std::fs::read_to_string(repo_path("src/brain/tools/subagent/spawn.rs"))
        .expect("cannot read spawn.rs");

    let park = src
        .find("mark_awaiting_input(&agent_id_clone)")
        .expect("the round-boundary park moved; this pin needs updating");
    let tail = &src[park..];
    let push = tail
        .find("push_attention(")
        .expect("a parked child is silent again - #859 regression");

    assert!(
        tail[..push].contains("attention_allowed(&agent_id_clone)"),
        "the park notify must be throttled: a child that re-parks every round \
         has one question, not one per round"
    );
    assert!(
        push < 2000,
        "the notify should sit at the park site, not somewhere later in the loop"
    );
}

#[test]
fn every_child_prompt_carries_the_answer_channel() {
    // Two prompt construction sites: the original spawn and every resume. A
    // resumed child is just as able to park as a fresh one, and a child that
    // has been told it has no answer channel guesses instead of asking.
    for rel in [
        "src/brain/tools/subagent/spawn.rs",
        "src/brain/tools/subagent/resume.rs",
    ] {
        let src = std::fs::read_to_string(repo_path(rel))
            .unwrap_or_else(|e| panic!("cannot read {rel}: {e}"));
        assert!(
            src.contains("SUBAGENT_ANSWER_CHANNEL"),
            "{rel}: child prompt no longer tells the child it can report a \
             blocker - the answer channel is the whole point of #859"
        );
    }

    let setup = std::fs::read_to_string(repo_path("src/cli/tool_setup.rs"))
        .expect("cannot read tool_setup.rs");
    assert!(
        setup.contains("pub const SUBAGENT_ANSWER_CHANNEL"),
        "the answer-channel block must exist in tool_setup.rs"
    );
    assert!(
        setup.contains("report_progress"),
        "the answer channel must name the tool that opens it"
    );
}
