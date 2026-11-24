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
        let name = event.metadata().name().to_string();
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
    assert!(fields.contains(&format!("session_id=\"{session_id}\"")));
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
