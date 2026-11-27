//! Prompt-cache invalidation metrics for compaction actions (#1025).
//!
//! Three surfaces are pinned here:
//!
//! 1. the break-index arithmetic per action (unit level, `cache_impact`);
//! 2. the notice subline on all three Telegram receipts — `🧹 compact:`,
//!    `📎 offloaded:` and `🧹 idle merge:` — because a metric nobody sees is a
//!    metric nobody acts on;
//! 3. the structured `compaction_event` fields, so the log and the notice
//!    cannot disagree about what one action cost.

use std::sync::{Arc, Mutex};

use tracing_subscriber::Layer;
use tracing_subscriber::layer::SubscriberExt;
use uuid::Uuid;

use crate::brain::agent::context::{AgentContext, CompactionScope};
use crate::brain::agent::service::cache_impact::{
    CacheEstimate, CacheImpact, CacheMutation, estimate, format_hint, mutation_after_apply,
};
use crate::brain::agent::service::compaction_event::{
    CompactionEventFields, CompactionEventStage, emit_compaction_event,
};
use crate::brain::provider::{ContentBlock, Message, Role};
use crate::channels::telegram::flow::{
    CompactionBreakdown, ToolOffloadNotice, compaction_breakdown, idle_compaction_notice,
    tool_offload_notice,
};

fn text_msg(role: Role, text: &str) -> Message {
    Message {
        role,
        content: vec![ContentBlock::Text {
            text: text.to_string(),
        }],
    }
}

// ---------------------------------------------------------------------------
// Break-index arithmetic
// ---------------------------------------------------------------------------

/// The issue's table, one case per action. A delta apply breaks at the frozen
/// prefix, a full apply at the first message, an offload at the oldest stubbed
/// result, a consolidation at the first merged segment.
#[test]
fn each_action_breaks_the_cache_at_its_own_boundary() {
    let cases = [
        (CacheMutation::ToolOffload { first_offloaded: 9 }, 9),
        (CacheMutation::DeltaApply { snapshot_len: 14 }, 14),
        (CacheMutation::FullApply, 0),
        (CacheMutation::Consolidate { first_merged: 2 }, 2),
    ];
    for (mutation, expected) in cases {
        assert_eq!(
            mutation.break_at_seq(),
            expected,
            "{} must break at {expected}",
            mutation.label()
        );
    }
}

/// `mutation_after_apply` reads the break off the context the apply LEFT, so a
/// scope's policy word and its actual break index cannot drift apart.
#[test]
fn the_applied_scope_names_the_same_break_the_vector_shows() {
    let session_id = Uuid::new_v4();
    let mut ctx = AgentContext::new(session_id, 200_000);
    ctx.add_message(text_msg(Role::User, "before the marker"));
    ctx.add_message(text_msg(
        Role::User,
        "[CONTEXT COMPACTION — a frozen segment]",
    ));
    ctx.add_message(text_msg(Role::Assistant, "after the marker"));

    // A full-window swap rewrites from the very first message.
    assert_eq!(
        mutation_after_apply(CompactionScope::FullWindow, &ctx).break_at_seq(),
        0
    );
    // A delta appends AFTER the frozen segment, so the segment survives.
    assert_eq!(
        mutation_after_apply(CompactionScope::DeltaSinceMarker, &ctx).break_at_seq(),
        1,
        "a delta apply leaves the frozen segment cached"
    );
    // A consolidation rewrites the segments themselves, from the first one.
    assert_eq!(
        mutation_after_apply(CompactionScope::SegmentConsolidation, &ctx).break_at_seq(),
        1
    );
}

/// The two halves always sum to the whole request — the checksum a reader of
/// the log line relies on when checking `kept + invalidated` against the
/// provider's own `prompt_tokens`.
#[test]
fn kept_and_invalidated_partition_the_request() {
    let messages = vec![
        text_msg(Role::User, &"a".repeat(800)),
        text_msg(Role::Assistant, &"b".repeat(800)),
    ];
    let static_overhead = 12_000;
    let impact = estimate(
        &messages,
        CacheMutation::ToolOffload { first_offloaded: 1 },
        static_overhead,
        "",
    );

    let message_tokens: usize = messages
        .iter()
        .map(AgentContext::estimate_tokens_static)
        .sum();
    assert_eq!(impact.total_tokens(), static_overhead + message_tokens);
    assert_eq!(
        impact.invalidated_tokens + impact.prefix_kept_tokens,
        impact.total_tokens()
    );
}

/// An estimate carries its own label so a `cache_outcome` record is readable
/// without correlating it to the action's record by timestamp.
#[test]
fn an_estimate_remembers_which_action_armed_it() {
    let impact = CacheImpact {
        break_at_seq: 3,
        prefix_kept_tokens: 20_000,
        invalidated_tokens: 5_000,
        invalidated_pct: 20.0,
    };
    let estimate =
        CacheEstimate::from_impact(CacheMutation::DeltaApply { snapshot_len: 3 }, impact);
    assert_eq!(estimate.mutation, "delta_apply");
    assert_eq!(estimate.estimated_invalidated_tokens, 5_000);
    assert_eq!(estimate.estimated_prompt_tokens, 25_000);
}

// ---------------------------------------------------------------------------
// Notice sublines
// ---------------------------------------------------------------------------

/// `📎 offloaded:` carries the cache line under its table, outside it — the
/// figure is an estimate of the NEXT request, not a measurement of this one.
#[test]
fn the_offload_notice_carries_the_cache_subline() {
    let notice = tool_offload_notice(&ToolOffloadNotice {
        blocks_offloaded: 3,
        tokens_freed: 41_000,
        before_pct: 71.0,
        after_pct: 56.0,
        before_tokens: 71_000,
        after_tokens: 56_000,
        ctx_max: 100_000,
        cache_hint: Some("cache: ~42k of 68k re-read (62%)".to_string()),
    });
    assert!(
        notice.ends_with("cache: ~42k of 68k re-read (62%)"),
        "the cache line closes the notice, got:\n{notice}"
    );
    // Outside the table: the last table row must come BEFORE it.
    let cache_at = notice.find("cache:").expect("cache line present");
    let last_row = notice.rfind("| fill |").expect("fill row present");
    assert!(last_row < cache_at);
}

/// The idle-merge receipt carries the same line, in the same position.
#[test]
fn the_idle_merge_notice_carries_the_cache_subline() {
    let notice = idle_compaction_notice(
        42.0,
        38.0,
        84_000,
        76_000,
        5,
        3,
        200_000,
        std::time::Duration::from_secs(4),
        Some("cache: ~76k of 200k re-read (38%)"),
    );
    assert!(notice.starts_with("🧹 idle merge:"));
    assert!(notice.ends_with("cache: ~76k of 200k re-read (38%)"));
}

/// The summariser card carries it too — that is the `🧹 compact:` receipt.
#[test]
fn the_compaction_card_carries_the_cache_subline() {
    let sizes: Vec<crate::brain::agent::service::ToolSize> = Vec::new();
    let card = compaction_breakdown(&CompactionBreakdown {
        before_tokens: 130_600,
        after_tokens: 106_186,
        before_pct: 65.0,
        after_pct: 53.0,
        ctx_max: 200_000,
        elapsed: std::time::Duration::from_millis(21_500),
        tool_sizes: &sizes,
        summary: "no manifest here",
        truncated: false,
        scope: "delta",
        input_tokens: 41_300,
        documents: &[],
        document_refusals: &[],
        cache_hint: Some("cache: ~41k of 130k re-read (32%)"),
    });
    assert!(card.starts_with("🧹 compact:"));
    assert!(
        card.ends_with("cache: ~41k of 130k re-read (32%)"),
        "got:\n{card}"
    );
}

/// A notice with no estimate prints no line — an action that did not mutate the
/// prompt must not borrow the authority of one that did.
#[test]
fn a_notice_without_an_estimate_prints_no_cache_line() {
    let notice = tool_offload_notice(&ToolOffloadNotice {
        blocks_offloaded: 1,
        tokens_freed: 2_000,
        before_pct: 70.0,
        after_pct: 66.0,
        before_tokens: 70_000,
        after_tokens: 68_000,
        ctx_max: 100_000,
        cache_hint: None,
    });
    assert!(!notice.contains("cache:"));
}

/// The rendered figure never claims to be a measurement.
#[test]
fn the_hint_is_marked_as_an_estimate() {
    let hint = format_hint(&CacheImpact {
        break_at_seq: 1,
        prefix_kept_tokens: 26_000,
        invalidated_tokens: 42_000,
        invalidated_pct: 61.76,
    });
    assert_eq!(hint, "cache: ~42k of 68k re-read (62%)");
    assert!(hint.contains('~'), "the tilde is the estimate marker");
}

// ---------------------------------------------------------------------------
// Structured log fields
// ---------------------------------------------------------------------------

#[derive(Clone, Default)]
struct EventCapture {
    events: Arc<Mutex<Vec<String>>>,
}

impl<S: tracing::Subscriber> Layer<S> for EventCapture {
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        if event.metadata().target() != "compaction_event" {
            return;
        }
        let mut fields = String::new();
        event.record(&mut Visitor { out: &mut fields });
        self.events.lock().unwrap().push(fields);
    }
}

struct Visitor<'a> {
    out: &'a mut String,
}

impl tracing::field::Visit for Visitor<'_> {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        use std::fmt::Write;
        let _ = write!(self.out, "{}={:?} ", field.name(), value);
    }

    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        use std::fmt::Write;
        let _ = write!(self.out, "{}=\"{}\" ", field.name(), value);
    }
}

/// The `tool_offload` record carries the four estimate fields, so the log can
/// answer "what did that offload cost the cache" without the Telegram card.
#[test]
fn the_tool_offload_record_carries_the_estimate() {
    let capture = EventCapture::default();
    let subscriber = tracing_subscriber::registry().with(capture.clone());
    let _guard = tracing::subscriber::set_default(subscriber);

    emit_compaction_event(
        CompactionEventStage::ToolOffload,
        Uuid::new_v4(),
        &CompactionEventFields {
            cache_break_at_seq: Some(11),
            cache_prefix_kept_tokens: Some(26_000),
            cache_invalidated_tokens: Some(42_000),
            cache_invalidated_pct: Some(61.76),
            ..Default::default()
        },
    );

    let fields = &capture.events.lock().unwrap()[0];
    assert!(fields.contains("cache_break_at_seq=Some(11)"), "{fields}");
    assert!(
        fields.contains("cache_prefix_kept_tokens=Some(26000)"),
        "{fields}"
    );
    assert!(
        fields.contains("cache_invalidated_tokens=Some(42000)"),
        "{fields}"
    );
    assert!(
        fields.contains("cache_invalidated_pct=Some(61.76)"),
        "{fields}"
    );
}

/// The `cache_outcome` stage compares the estimate against the provider's own
/// numbers, and names the action that armed it.
#[test]
fn the_cache_outcome_record_pairs_estimate_and_actual() {
    let capture = EventCapture::default();
    let subscriber = tracing_subscriber::registry().with(capture.clone());
    let _guard = tracing::subscriber::set_default(subscriber);

    emit_compaction_event(
        CompactionEventStage::CacheOutcome,
        Uuid::new_v4(),
        &CompactionEventFields {
            cache_mutation: Some("delta_apply"),
            cache_invalidated_tokens: Some(42_000),
            cache_prefix_kept_tokens: Some(26_000),
            actual_cached_tokens: Some(31_000),
            actual_prompt_tokens: Some(68_000),
            ..Default::default()
        },
    );

    let fields = &capture.events.lock().unwrap()[0];
    assert!(fields.contains("stage=\"cache_outcome\""), "{fields}");
    assert!(
        fields.contains("cache_mutation=Some(\"delta_apply\")"),
        "{fields}"
    );
    assert!(
        fields.contains("actual_cached_tokens=Some(31000)"),
        "{fields}"
    );
    assert!(
        fields.contains("actual_prompt_tokens=Some(68000)"),
        "{fields}"
    );
}

/// A stage that did not mutate the prompt prints no cache fields at all — an
/// absent estimate and a zero estimate are different facts and must look
/// different in the log.
#[test]
fn a_non_mutating_stage_prints_no_cache_fields() {
    let capture = EventCapture::default();
    let subscriber = tracing_subscriber::registry().with(capture.clone());
    let _guard = tracing::subscriber::set_default(subscriber);

    emit_compaction_event(
        CompactionEventStage::GateFire,
        Uuid::new_v4(),
        &CompactionEventFields::default(),
    );

    let fields = &capture.events.lock().unwrap()[0];
    assert!(fields.contains("cache_break_at_seq=None"), "{fields}");
    assert!(fields.contains("cache_invalidated_tokens=None"), "{fields}");
}
