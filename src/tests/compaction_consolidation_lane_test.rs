//! Acceptance tests for #1011–#1014 and #1016 (parent #1000 consolidation lane).

use crate::brain::agent::context::{AgentContext, CompactionScope};
use crate::brain::agent::service::AgentService;
use crate::brain::agent::service::compaction::CompactionState;
use crate::brain::agent::service::request_budget::{
    COMPACTION_SUMMARY_MAX_TOKENS, DELTA_DOCUMENT_INPUT_FRACTION, compaction_document_budget_tokens,
};
use crate::brain::provider::Message;

fn ctx(max_tokens: usize) -> AgentContext {
    AgentContext::new(uuid::Uuid::nil(), max_tokens)
}

fn text_of(msg: &Message) -> String {
    msg.content
        .iter()
        .filter_map(|b| match b {
            crate::brain::provider::ContentBlock::Text { text } => Some(text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join(" ")
}

// ── #1013 ─────────────────────────────────────────────────────────────────

#[test]
fn expected_summary_tokens_uses_median_not_last_outlier() {
    let mut trend = CompactionState::default();
    for &t in &[1800, 1900, 1850, 1950, 1800] {
        trend.recent_summary_tokens[trend.recent_summary_len as usize] = t;
        trend.recent_summary_len += 1;
    }
    trend.last_summary_tokens = 3000;
    let expected = trend.expected_summary_tokens();
    assert!(
        (1750..=1950).contains(&expected),
        "median should ignore the 3000 outlier in last_summary_tokens; got {expected}"
    );
}

#[test]
fn expected_summary_tokens_falls_back_to_default_with_no_history() {
    let trend = CompactionState::default();
    assert_eq!(
        trend.expected_summary_tokens(),
        AgentContext::DEFAULT_SUMMARY_TOKENS
    );
}

// ── #1011 ─────────────────────────────────────────────────────────────────

#[test]
fn segment_count_cap_forces_consolidation_below_floor() {
    let mut c = ctx(200_000);
    for i in 0..6 {
        c.add_message(Message::user(format!("q{i}")));
        AgentService::apply_scoped_compaction_summary(
            &mut c,
            CompactionScope::DeltaSinceMarker,
            &format!("SEG {i}"),
            None,
        );
    }
    c.add_message(Message::user("alpha ".repeat(5_000)));
    let expected = AgentContext::DEFAULT_SUMMARY_TOKENS;
    assert!(
        c.delta_token_count() > expected * 2,
        "probe: delta should pass the payoff test on its own"
    );
    assert_eq!(
        c.compaction_scope_with(expected, false),
        CompactionScope::SegmentConsolidation,
        "six frozen segments must consolidate even when floor+document is under 50%"
    );
}

#[test]
fn healthy_two_segments_still_deltas_when_payoff_passes() {
    let mut c = ctx(200_000);
    c.add_message(Message::user("q0"));
    AgentService::apply_scoped_compaction_summary(
        &mut c,
        CompactionScope::FullWindow,
        "FIRST",
        None,
    );
    c.add_message(Message::user("alpha ".repeat(20_000)));
    assert_eq!(
        c.compaction_scope_with(AgentContext::DEFAULT_SUMMARY_TOKENS, false),
        CompactionScope::DeltaSinceMarker
    );
}

// ── #1012 ─────────────────────────────────────────────────────────────────

#[test]
fn incremental_consolidation_replaces_only_oldest_three_segments() {
    let mut c = ctx(400_000);
    for i in 0..5 {
        c.add_message(Message::user(format!("q{i}")));
        AgentService::apply_scoped_compaction_summary(
            &mut c,
            CompactionScope::DeltaSinceMarker,
            &format!("MARKER_{i}"),
            None,
        );
    }
    c.add_message(Message::user("LIVE TAIL"));

    let (_, input, _) =
        c.compaction_input_for_scope(CompactionScope::SegmentConsolidation, Some(3));
    let joined = text_of(&input[0]);
    assert!(joined.contains("MARKER_0"));
    assert!(joined.contains("MARKER_2"));
    assert!(!joined.contains("MARKER_3"));

    AgentService::apply_scoped_compaction_summary(
        &mut c,
        CompactionScope::SegmentConsolidation,
        "PREFIX MERGE",
        Some(3),
    );

    let texts: Vec<String> = c.messages.iter().map(text_of).collect();
    assert_eq!(
        texts.len(),
        4,
        "one new marker + 2 newer markers + live tail"
    );
    assert!(texts[0].contains("PREFIX MERGE"));
    assert!(texts[1].contains("MARKER_3"));
    assert!(texts[2].contains("MARKER_4"));
    assert!(texts[3].contains("LIVE TAIL"));
}

// ── #1014 ─────────────────────────────────────────────────────────────────

#[test]
fn delta_document_budget_is_quarter_of_input_capped_at_max() {
    let budget = compaction_document_budget_tokens(CompactionScope::DeltaSinceMarker, 8_000);
    assert_eq!(
        budget,
        (8_000_f64 * DELTA_DOCUMENT_INPUT_FRACTION).round() as u32,
        "8k input → 2k document budget"
    );
    let huge = compaction_document_budget_tokens(CompactionScope::DeltaSinceMarker, 50_000);
    assert_eq!(huge, COMPACTION_SUMMARY_MAX_TOKENS);
}

#[test]
fn consolidation_keeps_full_document_cap() {
    let budget = compaction_document_budget_tokens(CompactionScope::SegmentConsolidation, 8_000);
    assert_eq!(budget, COMPACTION_SUMMARY_MAX_TOKENS);
}
