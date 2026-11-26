//! Cache invalidation metrics (#1025).
//!
//! Tier-0 offload gating (owner): cache impact on Apply events includes `tokens_freed`
//! when offload is folded into a compaction apply; standalone mid-loop >65% offloads
//! continue to emit `CompactionEventStage::ToolOffload` with cache fields.

use crate::brain::agent::context::{AgentContext, CompactionScope};
use crate::brain::agent::service::compaction_cache::{
    cache_break_at_for_compaction, cache_break_at_head_cut, estimate_from_context,
    format_cache_notice_line,
};
use crate::brain::agent::service::compaction_event::{
    CompactionEventFields, CompactionEventStage, emit_compaction_cache_outcome,
    emit_compaction_event,
};
use crate::brain::provider::Message;
use crate::channels::telegram::flow::{
    CompactionBreakdown, ToolOffloadNotice, compaction_breakdown, tool_offload_notice,
    with_optional_cache_line,
};
use uuid::Uuid;

#[test]
fn offload_notice_appends_cache_subline() {
    let cache = "cache: ~42k of 68k re-read (62%)".to_string();
    let body = tool_offload_notice(&ToolOffloadNotice {
        blocks_offloaded: 1,
        tokens_freed: 2_000,
        before_pct: 70.0,
        after_pct: 55.0,
        before_tokens: 70_000,
        after_tokens: 55_000,
        ctx_max: 100_000,
        cache_notice: Some(cache.clone()),
    });
    assert!(body.contains(&cache));
    assert!(body.starts_with("📎 offloaded:"));
}

#[test]
fn compact_card_headline_carries_cache_line() {
    let cache = "cache: ~10k of 20k re-read (50%)";
    let headline = with_optional_cache_line("🧹 compact: 66% → 40% 🧠".to_string(), Some(cache));
    assert_eq!(
        headline,
        "🧹 compact: 66% → 40% 🧠\ncache: ~10k of 20k re-read (50%)"
    );
}

#[test]
fn compaction_breakdown_includes_cache_on_headline() {
    let card = compaction_breakdown(&CompactionBreakdown {
        before_tokens: 10_000,
        after_tokens: 8_000,
        before_pct: 66.0,
        after_pct: 40.0,
        scope: "delta",
        input_tokens: 2_000,
        ctx_max: 20_000,
        elapsed: std::time::Duration::from_secs(5),
        tool_sizes: &[],
        summary: "",
        truncated: false,
        documents: &[],
        document_refusals: &[],
        cache_notice: Some("cache: ~5k of 10k re-read (50%)"),
    });
    assert!(card.contains("cache: ~5k of 10k re-read (50%)"));
}

#[test]
fn delta_scope_break_uses_snapshot_index() {
    let mut ctx = AgentContext::new(Uuid::new_v4(), 50_000);
    ctx.add_message(Message::user("frozen".repeat(100)));
    ctx.add_message(Message::user("delta".repeat(100)));
    let break_at = cache_break_at_for_compaction(&ctx, CompactionScope::DeltaSinceMarker, 1, None);
    assert_eq!(break_at, 1);
    let est = estimate_from_context(&ctx, 500, break_at);
    assert!(est.prefix_kept_tokens > 500);
    assert!(est.invalidated_tokens > 0);
    assert!(format_cache_notice_line(&est).starts_with("cache: ~"));
}

#[test]
fn head_cut_break_starts_at_zero() {
    assert_eq!(cache_break_at_head_cut(), 0);
}

#[test]
fn cache_outcome_event_fields() {
    let session = Uuid::new_v4();
    emit_compaction_cache_outcome(session, 40_000, 38_000, 68_000);
    emit_compaction_event(
        CompactionEventStage::ToolOffload,
        session,
        &CompactionEventFields {
            cache_break_at_seq: Some(3),
            cache_prefix_kept_tokens: Some(26_000),
            cache_invalidated_tokens: Some(42_000),
            cache_invalidated_pct: Some(61.7),
            tokens_freed: Some(12_000),
            ..Default::default()
        },
    );
}
