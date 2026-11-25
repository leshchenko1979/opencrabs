//! Structured `compaction_event` tracing for context-window compaction.
//!
//! Emits a stable, fielded record (`target = "compaction_event"`) so log
//! subscribers can grep or parse JSON without scraping prose lines.

use super::compaction::{BudgetPhase, CompactionOutcome};
use crate::brain::agent::context::CompactionScope;
use uuid::Uuid;

/// Lifecycle stage of one observability record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CompactionEventStage {
    GateFire,
    BackgroundSpawn,
    SummariserAttemptStart,
    SummariserAttemptEnd,
    Apply,
    Suppress,
    TruncateOnly,
}

impl CompactionEventStage {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::GateFire => "gate_fire",
            Self::BackgroundSpawn => "background_spawn",
            Self::SummariserAttemptStart => "summariser_attempt_start",
            Self::SummariserAttemptEnd => "summariser_attempt_end",
            Self::Apply => "apply",
            Self::Suppress => "suppress",
            Self::TruncateOnly => "truncate_only",
        }
    }
}

/// Why compaction fired or was suppressed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CompactionTrigger {
    Auto65,
    Manual,
    Emergency,
    Tier2Truncate,
    LoopGuard,
    NonReducing,
    BackgroundApply,
}

impl CompactionTrigger {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Auto65 => "auto_65",
            Self::Manual => "manual",
            Self::Emergency => "emergency",
            Self::Tier2Truncate => "tier2_truncate",
            Self::LoopGuard => "loop_guard",
            Self::NonReducing => "non_reducing",
            Self::BackgroundApply => "background_apply",
        }
    }
}

/// Sync compaction vs background summariser task.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CompactionPath {
    Sync,
    Background,
}

impl CompactionPath {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Sync => "sync",
            Self::Background => "background",
        }
    }
}

/// Caller context threaded into summariser attempts.
#[derive(Debug, Clone, Copy)]
pub(crate) struct CompactionRunContext {
    pub trigger: CompactionTrigger,
    pub path: CompactionPath,
    pub phase: Option<BudgetPhase>,
}

impl CompactionRunContext {
    pub(crate) const fn auto_sync(phase: BudgetPhase) -> Self {
        Self {
            trigger: CompactionTrigger::Auto65,
            path: CompactionPath::Sync,
            phase: Some(phase),
        }
    }

    pub(crate) const fn manual() -> Self {
        Self {
            trigger: CompactionTrigger::Manual,
            path: CompactionPath::Sync,
            phase: None,
        }
    }

    pub(crate) const fn emergency() -> Self {
        Self {
            trigger: CompactionTrigger::Emergency,
            path: CompactionPath::Sync,
            phase: None,
        }
    }

    pub(crate) const fn background_auto() -> Self {
        Self {
            trigger: CompactionTrigger::Auto65,
            path: CompactionPath::Background,
            phase: None,
        }
    }
}

/// Metrics captured from an [`AgentContext`](crate::brain::agent::context::AgentContext) snapshot.
#[derive(Debug, Clone, Copy)]
pub(crate) struct CompactionContextSnapshot {
    pub usage_pct: f64,
    pub effective_tokens: usize,
    pub messages: usize,
    #[allow(dead_code)]
    pub local_tokens: usize,
}

impl CompactionContextSnapshot {
    pub(crate) fn from_context(context: &crate::brain::agent::context::AgentContext) -> Self {
        Self {
            usage_pct: context.usage_percentage(),
            effective_tokens: context.effective_token_count(),
            messages: context.messages.len(),
            local_tokens: context.token_count,
        }
    }
}

/// Optional fields for one structured record. Unset options are omitted from the line.
#[derive(Debug, Clone, Default)]
pub(crate) struct CompactionEventFields {
    pub trigger: Option<CompactionTrigger>,
    pub path: Option<CompactionPath>,
    pub phase: Option<BudgetPhase>,
    pub scope: Option<CompactionScope>,
    pub outcome: Option<CompactionOutcomeKind>,
    pub usage_pct_before: Option<f64>,
    pub usage_pct_after: Option<f64>,
    pub effective_tokens_before: Option<usize>,
    pub effective_tokens_after: Option<usize>,
    pub messages_before: Option<usize>,
    pub messages_after: Option<usize>,
    pub model: Option<String>,
    pub provider: Option<String>,
    pub attempt: Option<u32>,
    pub duration_ms: Option<u64>,
    pub summary_tokens: Option<usize>,
    pub truncated: Option<bool>,
}

/// Outcome without carrying marker text into logs.
#[derive(Debug, Clone, Copy)]
pub(crate) enum CompactionOutcomeKind {
    Summarised,
    TruncatedSummary,
    Truncated,
    LoopGuard,
    NonReducingBrake,
}

impl From<&CompactionOutcome> for CompactionOutcomeKind {
    fn from(value: &CompactionOutcome) -> Self {
        match value {
            CompactionOutcome::Summarised(_) => Self::Summarised,
            CompactionOutcome::TruncatedSummary(_) => Self::TruncatedSummary,
            CompactionOutcome::Truncated => Self::Truncated,
            CompactionOutcome::LoopGuard => Self::LoopGuard,
            CompactionOutcome::NonReducingBrake => Self::NonReducingBrake,
        }
    }
}

impl CompactionOutcomeKind {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Summarised => "summarised",
            Self::TruncatedSummary => "truncated_summary",
            Self::Truncated => "truncated",
            Self::LoopGuard => "loop_guard",
            Self::NonReducingBrake => "non_reducing_brake",
        }
    }
}

pub(crate) fn emit_compaction_event(
    stage: CompactionEventStage,
    session_id: Uuid,
    fields: &CompactionEventFields,
) {
    let trigger = fields.trigger.map(CompactionTrigger::as_str).unwrap_or("-");
    let path = fields.path.map(CompactionPath::as_str).unwrap_or("-");
    let phase = fields.phase.map(BudgetPhase::as_str).unwrap_or("-");
    let scope = fields.scope.map(CompactionScope::label).unwrap_or("-");
    let outcome = fields
        .outcome
        .map(CompactionOutcomeKind::as_str)
        .unwrap_or("-");

    let model = fields.model.as_deref().unwrap_or("-");
    let provider = fields.provider.as_deref().unwrap_or("-");
    tracing::info!(
        target: "compaction_event",
        session_id = %session_id,
        stage = stage.as_str(),
        trigger,
        path,
        phase,
        scope,
        outcome,
        usage_pct_before = ?fields.usage_pct_before,
        usage_pct_after = ?fields.usage_pct_after,
        effective_tokens_before = ?fields.effective_tokens_before,
        effective_tokens_after = ?fields.effective_tokens_after,
        messages_before = ?fields.messages_before,
        messages_after = ?fields.messages_after,
        model,
        provider,
        attempt = ?fields.attempt,
        duration_ms = ?fields.duration_ms,
        summary_tokens = ?fields.summary_tokens,
        truncated = ?fields.truncated,
        "compaction_event"
    );
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn emit_summariser_attempt_end(
    session_id: Uuid,
    run_ctx: CompactionRunContext,
    scope: CompactionScope,
    attempt: u32,
    model: &str,
    provider: &str,
    duration_ms: u64,
    summary_tokens: Option<usize>,
    truncated: Option<bool>,
) {
    emit_compaction_event(
        CompactionEventStage::SummariserAttemptEnd,
        session_id,
        &CompactionEventFields {
            trigger: Some(run_ctx.trigger),
            path: Some(run_ctx.path),
            phase: run_ctx.phase,
            scope: Some(scope),
            model: Some(model.to_string()),
            provider: Some(provider.to_string()),
            attempt: Some(attempt),
            duration_ms: Some(duration_ms),
            summary_tokens,
            truncated,
            ..Default::default()
        },
    );
}

#[allow(clippy::type_complexity)]
pub(crate) fn snapshot_pair(
    before: CompactionContextSnapshot,
    after: CompactionContextSnapshot,
) -> (
    Option<f64>,
    Option<f64>,
    Option<usize>,
    Option<usize>,
    Option<usize>,
    Option<usize>,
) {
    (
        Some(before.usage_pct),
        Some(after.usage_pct),
        Some(before.effective_tokens),
        Some(after.effective_tokens),
        Some(before.messages),
        Some(after.messages),
    )
}
