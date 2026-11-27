//! Structured `compaction_event` tracing records (#observability P0).

use std::sync::{Arc, Mutex};

use crate::brain::agent::context::CompactionScope;
use crate::brain::agent::service::compaction::BudgetPhase;
use crate::brain::agent::service::compaction_event::{
    CompactionEventFields, CompactionEventStage, CompactionOutcomeKind, CompactionPath,
    CompactionRunContext, CompactionTrigger, emit_compaction_event,
};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::SubscriberExt;
use uuid::Uuid;

#[derive(Clone, Default)]
struct EventCapture {
    events: Arc<Mutex<Vec<(String, String)>>>,
}

impl EventCapture {
    fn drain(&self) -> Vec<(String, String)> {
        self.events.lock().unwrap().drain(..).collect()
    }
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
        let name = event.metadata().target().to_string();
        let mut fields = String::new();
        let mut visitor = FieldVisitor { out: &mut fields };
        event.record(&mut visitor);
        self.events.lock().unwrap().push((name, fields));
    }
}

struct FieldVisitor<'a> {
    out: &'a mut String,
}

impl<'a> tracing::field::Visit for FieldVisitor<'a> {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        use std::fmt::Write;
        let _ = write!(self.out, "{}={:?} ", field.name(), value);
    }

    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        use std::fmt::Write;
        let _ = write!(self.out, "{}=\"{}\" ", field.name(), value);
    }
}

#[test]
fn background_spawn_event_carries_required_fields() {
    let capture = EventCapture::default();
    let subscriber = tracing_subscriber::registry().with(capture.clone());
    let session_id = Uuid::new_v4();
    let _guard = tracing::subscriber::set_default(subscriber);

    emit_compaction_event(
        CompactionEventStage::BackgroundSpawn,
        session_id,
        &CompactionEventFields {
            trigger: Some(CompactionTrigger::Auto65),
            path: Some(CompactionPath::Background),
            scope: Some(CompactionScope::DeltaSinceMarker),
            usage_pct_before: Some(67.5),
            messages_before: Some(42),
            model: Some("test-model".into()),
            ..Default::default()
        },
    );

    let events = capture.drain();
    assert_eq!(events.len(), 1, "expected one compaction_event");
    let (name, fields) = &events[0];
    assert_eq!(name, "compaction_event");
    assert!(
        fields.contains("session_id=") && fields.contains(&session_id.to_string()),
        "expected session_id in event fields, got: {fields}"
    );
    assert!(fields.contains("stage=\"background_spawn\""));
    assert!(fields.contains("trigger=\"auto_65\""));
    assert!(fields.contains("path=\"background\""));
}

#[test]
fn apply_event_carries_outcome_and_trigger() {
    let capture = EventCapture::default();
    let subscriber = tracing_subscriber::registry().with(capture.clone());
    let session_id = Uuid::new_v4();
    let _guard = tracing::subscriber::set_default(subscriber);

    emit_compaction_event(
        CompactionEventStage::Apply,
        session_id,
        &CompactionEventFields {
            trigger: Some(CompactionTrigger::Manual),
            path: Some(CompactionPath::Sync),
            outcome: Some(CompactionOutcomeKind::Summarised),
            usage_pct_before: Some(72.0),
            usage_pct_after: Some(34.0),
            duration_ms: Some(8_500),
            summary_tokens: Some(2_100),
            truncated: Some(false),
            ..Default::default()
        },
    );

    let fields = &capture.drain()[0].1;
    assert!(fields.contains("stage=\"apply\""));
    assert!(fields.contains("trigger=\"manual\""));
    assert!(fields.contains("outcome=\"summarised\""));
    assert!(fields.contains("duration_ms=Some(8500)"));
}

#[test]
fn suppress_event_pins_loop_guard() {
    let capture = EventCapture::default();
    let subscriber = tracing_subscriber::registry().with(capture.clone());
    let session_id = Uuid::new_v4();
    let _guard = tracing::subscriber::set_default(subscriber);

    emit_compaction_event(
        CompactionEventStage::Suppress,
        session_id,
        &CompactionEventFields {
            trigger: Some(CompactionTrigger::LoopGuard),
            path: Some(CompactionPath::Sync),
            phase: Some(BudgetPhase::MidLoop),
            outcome: Some(CompactionOutcomeKind::LoopGuard),
            usage_pct_before: Some(71.0),
            ..Default::default()
        },
    );

    let fields = &capture.drain()[0].1;
    assert!(fields.contains("stage=\"suppress\""));
    assert!(fields.contains("trigger=\"loop_guard\""));
    assert!(fields.contains("phase=\"mid_loop\""));
}

#[test]
fn summariser_run_context_constants_are_stable() {
    let manual = CompactionRunContext::manual();
    assert_eq!(manual.trigger.as_str(), "manual");
    assert_eq!(manual.path.as_str(), "sync");
    let auto = CompactionRunContext::auto_sync(BudgetPhase::TurnStart);
    assert_eq!(auto.trigger.as_str(), "auto_65");
    assert_eq!(auto.phase.unwrap().as_str(), "turn_start");
}

/// #1037: the record must be reconcilable without arithmetic.
///
/// `usage_pct_before` is a BUDGET percentage (`budget_token_count(ov) /
/// max_tokens`, #1030) while the token field beside it used to be one of three
/// different quantities depending on the emission site — so a reader
/// recomputing the percentage from the token field got the wrong answer at
/// three of five stages, and nothing in the record said which accessor ran.
///
/// This pins the two halves of the fix:
///
/// 1. on an un-anchored context the three accessors are genuinely distinct
///    (`effective_token_count() == token_count`, `budget_token_count(ov) ==
///    token_count + ov`), which is the premise of the issue — if they ever
///    collapse, the split is pointless and this test should fail rather than
///    silently pass;
/// 2. a record carrying the budget numerator, the raw estimate, the static
///    overhead and the anchor flag emits all four under names that cannot be
///    confused for one another.
#[test]
fn budget_and_raw_token_fields_are_separable_in_the_record() {
    use crate::brain::agent::context::AgentContext;

    // The measured shape from the issue: a 200K window holding 193,324 tokens
    // with 33,165 tokens of static request overhead, un-anchored.
    let mut context = AgentContext::new(Uuid::new_v4(), 200_000);
    context.token_count = 193_324;
    let static_overhead = 33_165usize;

    assert_eq!(
        context.effective_token_count(),
        context.token_count,
        "un-anchored: the provider-effective count IS the raw estimate"
    );
    let budget = context.budget_token_count(static_overhead);
    assert_eq!(
        budget,
        context.token_count + static_overhead,
        "un-anchored: the budget numerator adds the static overhead"
    );
    assert_ne!(
        budget, context.token_count,
        "the whole point: budget and raw must NOT be the same number here"
    );
    // The percentage the gate logs is the BUDGET one — the same numerator the
    // `budget_tokens_before` field now carries, and NOT `token_count / max`.
    let pct = context.budget_usage_pct(static_overhead);
    assert!(
        (pct - 113.2445).abs() < 0.001,
        "expected the issue's measured 113.2445%, got {pct}"
    );

    let capture = EventCapture::default();
    let subscriber = tracing_subscriber::registry().with(capture.clone());
    let session_id = Uuid::new_v4();
    let _guard = tracing::subscriber::set_default(subscriber);

    emit_compaction_event(
        CompactionEventStage::BackgroundSpawn,
        session_id,
        &CompactionEventFields {
            trigger: Some(CompactionTrigger::Auto65),
            path: Some(CompactionPath::Background),
            usage_pct_before: Some(pct),
            budget_tokens_before: Some(budget),
            raw_tokens_before: Some(context.token_count),
            provider_anchor: Some(context.provider_anchor.is_some()),
            static_overhead: Some(static_overhead),
            ..Default::default()
        },
    );

    let fields = &capture.drain()[0].1;
    assert!(
        fields.contains("budget_tokens_before=Some(226489)"),
        "the budget numerator must be the field beside the percentage, got: {fields}"
    );
    assert!(
        fields.contains("raw_tokens_before=Some(193324)"),
        "the raw estimate must ride its own field, got: {fields}"
    );
    assert!(
        fields.contains("static_overhead=Some(33165)"),
        "the static overhead must be readable without arithmetic, got: {fields}"
    );
    assert!(
        fields.contains("provider_anchor=Some(false)"),
        "the anchor flag is the discriminator between the two regimes, got: {fields}"
    );
    // The old ambiguous name must be gone from the schema entirely.
    assert!(
        !fields.contains("effective_tokens_before"),
        "the three-meaning field must not survive, got: {fields}"
    );
}
