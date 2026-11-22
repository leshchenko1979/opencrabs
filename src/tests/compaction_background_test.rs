//! Auto-compaction runs in the background, and the three things that makes
//! dangerous are covered here.
//!
//! 1. **The gap.** A summariser that blocks the turn cannot miss anything:
//!    nothing arrives while it runs. A backgrounded one leaves a gap, and
//!    everything in that gap is the most recent work the agent did. The swap
//!    has to keep it.
//! 2. **The ceiling.** When the context outgrows its headroom the turn waits
//!    for the summary instead of discarding it. Cancelling to reclaim room is
//!    what left a truncated context with no marker and looped two sessions on
//!    reload (2026-05-05), so the predicate that decides to wait is asserted
//!    directly rather than inferred.
//! 3. **The notice.** The gate is re-entered on every tool iteration, so an
//!    announcement placed above the dispatch repeats for a compaction that is
//!    already running (#1686). The predicate deciding to announce, and the
//!    order that puts it below the guard, are both asserted here.

use crate::brain::agent::context::{AgentContext, CompactionScope};
use crate::brain::agent::service::AgentService;
use crate::brain::agent::service::compaction::{
    BudgetPhase, PendingState, gate_announces_compaction, gate_warn_message,
    must_wait_for_compaction,
};
use crate::brain::provider::{ContentBlock, Message, Role};

fn ctx(messages: Vec<Message>) -> AgentContext {
    let mut c = AgentContext::new(uuid::Uuid::nil(), 200_000);
    for m in messages {
        c.add_message(m);
    }
    c
}

fn tool_use(id: &str) -> Message {
    Message {
        role: Role::Assistant,
        content: vec![ContentBlock::ToolUse {
            id: id.into(),
            name: "bash".into(),
            input: serde_json::json!({ "command": "ls" }),
        }],
    }
}

fn tool_result(id: &str, body: &str) -> Message {
    Message {
        role: Role::User,
        content: vec![ContentBlock::ToolResult {
            tool_use_id: id.into(),
            content: body.into(),
            is_error: None,
        }],
    }
}

fn text_of(msg: &Message) -> String {
    msg.content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text { text } => Some(text.clone()),
            ContentBlock::ToolResult { content, .. } => Some(content.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join(" ")
}

// --- the gap ---

#[test]
fn work_done_during_the_summariser_call_survives_the_swap() {
    // Snapshot taken after two messages; the turn kept going and produced
    // three more while the summary was being written.
    let mut context = ctx(vec![
        Message::user("old question"),
        Message::assistant("old answer"),
        Message::user("new question"),
        tool_use("tu_1"),
        tool_result("tu_1", "the command output"),
    ]);

    AgentService::apply_compaction_summary_after(
        &mut context,
        CompactionScope::FullWindow,
        "SUMMARY BODY",
        2,
    );

    let rendered: Vec<String> = context.messages.iter().map(text_of).collect();
    assert!(
        rendered[0].contains("SUMMARY BODY"),
        "summary is not the anchor: {rendered:?}"
    );
    assert!(
        rendered.iter().any(|t| t.contains("new question")),
        "the turn's own question was deleted: {rendered:?}"
    );
    assert!(
        rendered.iter().any(|t| t.contains("the command output")),
        "a tool result the summary never saw was deleted: {rendered:?}"
    );
}

#[test]
fn the_summarised_prefix_does_not_survive_as_messages() {
    let mut context = ctx(vec![
        Message::user("old question"),
        Message::assistant("old answer"),
        Message::user("new question"),
    ]);

    AgentService::apply_compaction_summary_after(
        &mut context,
        CompactionScope::FullWindow,
        "SUMMARY BODY",
        2,
    );

    // #943: the pair the summariser saw is no longer gone — the window keeps a
    // bounded verbatim tail, so the exchange at the cut survives as messages.
    // What must NOT survive is the whole summarised prefix.
    let rendered: Vec<String> = context.messages.iter().map(text_of).collect();
    assert!(
        rendered[0].contains("SUMMARY BODY"),
        "summary is not the anchor"
    );
    assert!(
        rendered.iter().any(|t| t.contains("new question")),
        "the post-snapshot message was dropped: {rendered:?}"
    );
}

#[test]
fn the_summarised_prefix_survives_only_up_to_the_verbatim_tail() {
    // #943: the tail is bounded, not unlimited. Forty pre-snapshot messages of
    // roughly 250 tokens each cannot all fit a 2 000-token tail (200 000 / 50),
    // so the oldest go first and the exchange at the cut survives.
    let filler = "x".repeat(1_000);
    let mut messages: Vec<Message> = (0..40)
        .map(|i| Message::user(format!("{i:02} {filler}")))
        .collect();
    messages.push(Message::assistant("last pre-snapshot answer"));
    let mut context = ctx(messages);
    let total_before = context.messages.len();

    AgentService::apply_compaction_summary_after(
        &mut context,
        CompactionScope::FullWindow,
        "SUMMARY BODY",
        total_before,
    );

    let rendered: Vec<String> = context.messages.iter().map(text_of).collect();
    assert!(
        rendered[0].contains("SUMMARY BODY"),
        "summary is not the anchor: {} messages",
        rendered.len()
    );
    assert!(
        rendered
            .iter()
            .any(|t| t.contains("last pre-snapshot answer")),
        "the exchange at the cut was dropped: {} messages kept",
        rendered.len()
    );
    assert!(
        !rendered.iter().any(|t| t.starts_with("00 ")),
        "the oldest pre-snapshot message survived, so the tail is unbounded: {} kept",
        rendered.len()
    );
    assert!(
        rendered.len() < total_before,
        "the tail kept everything, so it is not bounded"
    );
}

#[test]
fn a_delta_larger_than_the_verbatim_tail_is_truncated_oldest_first() {
    // #943: the carried delta used to have no token accounting at all — its
    // size was whatever arrived while the summariser ran, so a busy lane could
    // re-inflate the window the summary had just cleared.
    let filler = "y".repeat(1_000);
    let mut messages: Vec<Message> = vec![Message::user("old question")];
    for i in 0..40 {
        messages.push(Message::user(format!("{i:02} {filler}")));
    }
    messages.push(Message::assistant("the last answer"));
    let mut context = ctx(messages);

    // Everything after the first message arrived during the summariser call.
    AgentService::apply_compaction_summary_after(
        &mut context,
        CompactionScope::FullWindow,
        "SUMMARY BODY",
        1,
    );

    let rendered: Vec<String> = context.messages.iter().map(text_of).collect();
    assert!(
        rendered.iter().any(|t| t.contains("the last answer")),
        "the newest delta message was dropped instead of the oldest: {} kept",
        rendered.len()
    );
    assert!(
        !rendered.iter().any(|t| t.starts_with("00 ")),
        "the delta was carried unbounded: {} messages",
        rendered.len()
    );
}

#[test]
fn a_delta_opening_on_tool_results_drops_the_orphans() {
    // The snapshot cut between an assistant tool_use and its results. The
    // summary lands as a user message, so those results now have no call to
    // belong to and the provider rejects the shape outright.
    let mut context = ctx(vec![
        Message::user("question"),
        tool_use("tu_1"),
        tool_result("tu_1", "orphaned output"),
        Message::assistant("carrying on"),
    ]);

    AgentService::apply_compaction_summary_after(
        &mut context,
        CompactionScope::FullWindow,
        "SUMMARY BODY",
        2,
    );

    assert!(
        !AgentContext::is_orphaned_tool_result_msg(&context.messages[1]),
        "an orphaned tool result was left directly after the summary"
    );
    let rendered: Vec<String> = context.messages.iter().map(text_of).collect();
    assert!(
        rendered.iter().any(|t| t.contains("carrying on")),
        "everything after the orphan was thrown away too: {rendered:?}"
    );
}

#[test]
fn an_all_orphan_delta_contributes_nothing() {
    let mut context = ctx(vec![
        Message::user("question"),
        tool_use("tu_1"),
        tool_result("tu_1", "one"),
        tool_result("tu_1", "two"),
    ]);

    AgentService::apply_compaction_summary_after(
        &mut context,
        CompactionScope::FullWindow,
        "SUMMARY BODY",
        2,
    );

    // #943: the pre-snapshot tail is kept, so the anchor is no longer the only
    // message. What must hold is that the all-orphan DELTA contributed
    // nothing, and that no orphan sits directly after the summary.
    let rendered: Vec<String> = context.messages.iter().map(text_of).collect();
    assert!(
        rendered[0].contains("SUMMARY BODY"),
        "summary is not the anchor"
    );
    assert!(
        !AgentContext::is_orphaned_tool_result_msg(&context.messages[1]),
        "an orphaned tool result was left directly after the summary: {rendered:?}"
    );
    assert!(
        !rendered.iter().any(|t| t == "one" || t == "two"),
        "an orphaned delta result survived the drain: {rendered:?}"
    );
}

#[test]
fn no_delta_behaves_like_a_blocking_compaction() {
    let mut context = ctx(vec![
        Message::user("question"),
        Message::assistant("answer"),
    ]);
    let len = context.messages.len();

    AgentService::apply_compaction_summary_after(
        &mut context,
        CompactionScope::FullWindow,
        "SUMMARY BODY",
        len,
    );

    // #943: with no delta the window is summary + the bounded pre-snapshot
    // tail, which is exactly what the blocking path produces. Nothing from a
    // delta may appear because there was none.
    let rendered: Vec<String> = context.messages.iter().map(text_of).collect();
    assert!(
        rendered[0].contains("SUMMARY BODY"),
        "summary is not the anchor"
    );
    assert_eq!(
        rendered.len(),
        3,
        "expected summary + the two pre-snapshot messages: {rendered:?}"
    );
    assert_eq!(
        rendered[2], "answer",
        "the newest pre-snapshot message was dropped"
    );
}

#[test]
fn a_snapshot_that_outlived_its_context_still_applies() {
    // The context is rebuilt from the database every turn, so an index from a
    // previous turn addresses a vector that no longer exists. There is no
    // delta to recover, but the summary must still land rather than panic.
    let mut context = ctx(vec![Message::user("only message")]);

    AgentService::apply_compaction_summary_after(
        &mut context,
        CompactionScope::FullWindow,
        "SUMMARY BODY",
        99,
    );

    // #943: the single message is inside the verbatim tail, so it survives
    // alongside the summary instead of being discarded with the delta.
    let rendered: Vec<String> = context.messages.iter().map(text_of).collect();
    assert!(
        rendered[0].contains("SUMMARY BODY"),
        "summary did not land: {rendered:?}"
    );
    assert_eq!(
        rendered.len(),
        2,
        "expected summary + the pre-snapshot message: {rendered:?}"
    );
    assert_eq!(rendered[1], "only message");
}

#[test]
fn the_budget_counts_the_delta_it_kept() {
    let mut context = ctx(vec![
        Message::user("old question"),
        Message::assistant("old answer"),
        Message::user("a considerably longer message that carries real weight in the budget"),
    ]);

    AgentService::apply_compaction_summary_after(
        &mut context,
        CompactionScope::FullWindow,
        "SUMMARY BODY",
        2,
    );

    let recomputed: usize = context
        .messages
        .iter()
        .map(|m| context.estimate_message_tokens(m))
        .sum::<usize>()
        + context
            .system_brain
            .as_deref()
            .map(AgentContext::estimate_tokens)
            .unwrap_or(0);
    assert_eq!(
        context.token_count, recomputed,
        "kept messages are in the context but not in its budget"
    );
}

// --- the ceiling ---

#[test]
fn a_turn_about_to_answer_always_waits() {
    // Answering from a context one swap away from replacement is the case the
    // owner called out: the reply would be composed against history that is
    // about to stop existing.
    for usage in [10.0, 66.0, 79.9, 95.0] {
        assert!(
            must_wait_for_compaction(BudgetPhase::TurnStart, usage),
            "turn start ran ahead of the summariser at {usage}%"
        );
    }
}

#[test]
fn the_tool_loop_runs_ahead_below_the_ceiling() {
    for usage in [10.0, 66.0, 79.9] {
        assert!(
            !must_wait_for_compaction(BudgetPhase::MidLoop, usage),
            "the loop blocked at {usage}%, which is the stall we removed"
        );
    }
}

#[test]
fn the_tool_loop_waits_at_the_ceiling() {
    for usage in [80.0, 91.0, 140.0] {
        assert!(
            must_wait_for_compaction(BudgetPhase::MidLoop, usage),
            "the loop kept growing the context at {usage}%"
        );
    }
}

// ── One notice per compaction cycle (#1686) ──

/// Three consecutive gate hits in one cycle: the first starts the summariser,
/// the next two find it already in flight. Before the reorder all three
/// announced, which is how session `a58b8714` came to pay 128 gate WARNs, 128
/// `tokio::spawn`s and 128 feedback-ledger rows. Across two days the gate hit
/// 241 times against 27 compactions that actually applied, about nine
/// announcements per summariser.
#[test]
fn one_cycle_announces_once_across_three_hits() {
    let cycle = [
        PendingState::Empty,
        PendingState::StillRunning,
        PendingState::StillRunning,
    ];
    let notices = cycle
        .iter()
        .filter(|state| gate_announces_compaction(state))
        .count();
    assert_eq!(notices, 1, "three gate hits announced {notices} times");
}

/// A summary that already landed is not a second compaction, and the visit that
/// applies it must not announce one.
#[test]
fn an_applied_summary_is_never_re_announced() {
    assert!(!gate_announces_compaction(&PendingState::Applied(
        "the summary".into()
    )));
}

/// A failed background attempt falls back to the blocking path. That is a second
/// summariser rather than a repeat of the first, so it owes its own notice.
#[test]
fn the_blocking_fallback_still_announces() {
    assert!(gate_announces_compaction(&PendingState::Failed));
}

/// The predicate is only the decision. The defect was an ordering: the WARN and
/// the ledger record sat above the dispatch, so a repeat hit announced before
/// it learned a summariser was already running. Pin the order too, or the
/// predicate stays green while the announcement moves back up.
#[test]
fn the_announcement_sits_below_the_guard() {
    const SRC: &str = include_str!("../brain/agent/service/compaction.rs");
    let guard = SRC
        .find("if !gate_announces_compaction(")
        .expect("the gate guard");
    // The WARN's format literal lives inside gate_warn_message, so pin the
    // emit site: the call must sit below the guard for the pin to hold.
    let announce = SRC[guard..]
        .find("tracing::warn!(\"{}\", gate_warn_message(")
        .expect("the gate WARN")
        + guard;
    assert!(
        guard < announce,
        "the guard at {guard} must return before the announcement at {announce}"
    );
}

/// The WARN's displayed value must stay consistent with the gate that fired
/// it. The integer format turned a 65.4% fill into "Context at 65% (>65%)",
/// a sentence contradicting its own claim (#1733). The message now shows one
/// decimal and states the threshold as a label, so the shown digits can never
/// sit below the trigger band and no inequality is left to break.
#[test]
fn the_gate_warn_never_displays_below_the_trigger_band() {
    for pct in [65.01, 65.04, 65.05, 65.4, 65.95, 89.99, 97.3] {
        let msg = gate_warn_message(pct);
        let shown: f64 = msg
            .split("Context at ")
            .nth(1)
            .and_then(|rest| rest.split('%').next())
            .and_then(|digits| digits.parse().ok())
            .expect("the message must carry a parseable fill percentage");
        assert!(
            shown >= 65.0,
            "{pct}% fired the gate but the message displays {shown}%: {msg}"
        );
        assert!(
            msg.contains("(threshold 65%)"),
            "the trigger must be a label, not an inequality against the shown value: {msg}"
        );
    }
}
