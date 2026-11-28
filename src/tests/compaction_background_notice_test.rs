//! #1047 — a background spawn announces itself on the progress channel.
//!
//! # The defect these tests pin
//!
//! `enforce_context_budget` has two compaction paths. With backgrounding on,
//! the `PendingState::Empty if self.background_compaction` arm hands the
//! summariser to a spawned task and **returns** to let the turn carry on. The
//! `ProgressEvent::Compacting` emit that tells channels to render their
//! "compacting" flow line — and to pin the header state — sits *below that
//! return*, on the synchronous path only. So a background spawn produced a
//! `tracing::info!` line and a `BackgroundSpawn` event, both invisible to the
//! user, and no `ProgressEvent::Compacting` at all: the owner saw neither the
//! flow line nor the header while the summariser ran.
//!
//! The fix emits the event from `spawn_background_compaction` itself, after the
//! `BackgroundSpawn` record and before the detached summariser is spawned. The
//! tests below assert the properties that make that safe:
//!
//! - exactly ONE `Compacting` per background spawn, carrying the visit's own
//!   fill so it agrees with the gate that requested the compaction;
//! - ZERO further `Compacting` on a visit that re-enters the gate while the
//!   summariser is in flight — the #1686/#1023 invariant, which session
//!   `a58b8714` paid 128 duplicate announcements to learn;
//! - the synchronous path still announces exactly once, unchanged.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use uuid::Uuid;

use crate::brain::agent::context::AgentContext;
use crate::brain::agent::service::compaction::{BudgetPhase, CompactionOutcome};
use crate::brain::agent::service::{AgentService, ProgressCallback, ProgressEvent};
use crate::brain::provider::{
    ContentBlock, LLMRequest, LLMResponse, Message, Provider, ProviderStream, StopReason,
    TokenUsage,
};
use crate::db::Database;
use crate::services::ServiceContext;

fn filler() -> String {
    "the quick brown fox jumps over the lazy dog ".repeat(200)
}

/// Every `Compacting` fill the callback recorded, in order.
fn compacting_pcts(events: &[ProgressEvent]) -> Vec<f64> {
    events
        .iter()
        .filter_map(|e| match e {
            ProgressEvent::Compacting { usage_pct, .. } => Some(*usage_pct),
            _ => None,
        })
        .collect()
}

fn progress_recorder() -> (ProgressCallback, Arc<Mutex<Vec<ProgressEvent>>>) {
    let log = Arc::new(Mutex::new(Vec::new()));
    let sink = log.clone();
    let cb: ProgressCallback = Arc::new(move |_sid, event| {
        sink.lock().unwrap().push(event);
    });
    (cb, log)
}

async fn service_with(provider: Arc<dyn Provider>, background: bool) -> AgentService {
    let db = Database::connect_in_memory().await.unwrap();
    db.run_migrations().await.unwrap();
    let mut config = crate::config::Config::default();
    config.agent.background_compaction = background;
    AgentService::new(provider, ServiceContext::new(db.pool().clone()), &config).await
}

/// A summariser that never answers, so a spawned task stays in flight for the
/// life of the test — the shape the #1023 fixtures use. Without it a second
/// visit could race the summariser onto a different branch.
struct PendingSummariser;

#[async_trait]
impl Provider for PendingSummariser {
    async fn complete(&self, _request: LLMRequest) -> crate::brain::provider::Result<LLMResponse> {
        std::future::pending::<crate::brain::provider::Result<LLMResponse>>().await
    }

    async fn stream(&self, _request: LLMRequest) -> crate::brain::provider::Result<ProviderStream> {
        std::future::pending::<crate::brain::provider::Result<ProviderStream>>().await
    }

    fn name(&self) -> &str {
        "pending-summariser"
    }

    fn default_model(&self) -> &str {
        "mock-model"
    }

    fn supported_models(&self) -> Vec<String> {
        vec!["mock-model".to_string()]
    }

    fn context_window(&self, _model: &str) -> Option<u32> {
        Some(200_000)
    }

    fn calculate_cost(&self, _model: &str, _input: u32, _output: u32) -> f64 {
        0.0
    }
}

/// A summariser that answers with a real continuation document, for the
/// synchronous path where the visit must actually summarise and return.
struct AnsweringSummariser;

impl AnsweringSummariser {
    fn answer(&self, request: LLMRequest) -> LLMResponse {
        LLMResponse {
            id: "compaction-notice-response".to_string(),
            model: request.model,
            resolved_model: None,
            content: vec![ContentBlock::Text {
                text: "## 1. Summary\n\nThe oldest span of this session was summarised \
                       before it was cut (#1047 test document)."
                    .to_string(),
            }],
            stop_reason: Some(StopReason::EndTurn),
            usage: TokenUsage {
                input_tokens: 10,
                output_tokens: 20,
                ..Default::default()
            },
            streaming_active_secs: None,
            tool_text_leak: false,
        }
    }
}

#[async_trait]
impl Provider for AnsweringSummariser {
    async fn complete(&self, request: LLMRequest) -> crate::brain::provider::Result<LLMResponse> {
        Ok(self.answer(request))
    }

    async fn stream(&self, request: LLMRequest) -> crate::brain::provider::Result<ProviderStream> {
        use crate::brain::provider::{
            ContentDelta, MessageDelta, Role, StreamEvent, StreamMessage,
        };

        // The full shape, not a shorthand: the summariser reads
        // `MessageDelta.stop_reason` to tell a complete answer from one cut at
        // its output limit (#994), so a stream without it would make the
        // fixture look like a truncated summary.
        let response = self.answer(request);
        let mut events = vec![Ok(StreamEvent::MessageStart {
            message: StreamMessage {
                id: response.id.clone(),
                model: response.model.clone(),
                role: Role::Assistant,
                usage: response.usage,
            },
        })];
        for (i, block) in response.content.iter().enumerate() {
            if let ContentBlock::Text { text } = block {
                events.push(Ok(StreamEvent::ContentBlockStart {
                    index: i,
                    content_block: ContentBlock::Text {
                        text: String::new(),
                    },
                }));
                events.push(Ok(StreamEvent::ContentBlockDelta {
                    index: i,
                    delta: ContentDelta::TextDelta { text: text.clone() },
                }));
                events.push(Ok(StreamEvent::ContentBlockStop { index: i }));
            }
        }
        events.push(Ok(StreamEvent::MessageDelta {
            delta: MessageDelta {
                stop_reason: response.stop_reason,
                stop_sequence: None,
            },
            usage: response.usage,
        }));
        events.push(Ok(StreamEvent::MessageStop));
        Ok(Box::pin(futures::stream::iter(events)))
    }

    fn name(&self) -> &str {
        "answering-summariser"
    }

    fn default_model(&self) -> &str {
        "mock-model"
    }

    fn supported_models(&self) -> Vec<String> {
        vec!["mock-model".to_string()]
    }

    fn context_window(&self, _model: &str) -> Option<u32> {
        Some(32_000)
    }

    fn calculate_cost(&self, _model: &str, _input: u32, _output: u32) -> f64 {
        0.0
    }
}

/// Fill a window into the band the background spawn is reached from: over the
/// 65% trigger and below the 80% back-pressure ceiling, so a MidLoop visit
/// spawns and hands the turn back rather than waiting.
///
/// Measured on `budget_usage_pct`, the ONE numerator the gate itself reads
/// (#1030) — the static request overhead it folds in is exactly what the raw
/// `usage_percentage()` drops, and a fixture built on the wrong one would sit
/// in a band the gate never visits.
async fn context_in_the_band(service: &AgentService, window: usize) -> AgentContext {
    let session_id = Uuid::new_v4();
    let mut context = AgentContext::new(session_id, window);
    let mut i = 0usize;
    while service.budget_usage_pct(session_id, &context) < 68.0 {
        context.add_message(Message::user(format!("msg-{i} {}", filler())));
        i += 1;
        assert!(i < 400, "fixture must reach the compaction band");
    }
    let fill = service.budget_usage_pct(session_id, &context);
    assert!(
        fill > 65.0 && fill < 80.0,
        "fixture must sit in the spawn band (65%..80%), got {fill:.1}%"
    );
    context
}

/// Fill past the old 90% floor, the fixture the synchronous head-cut path is
/// already proven against (`compaction_head_cut_test.rs`).
fn context_over_95(window: usize) -> AgentContext {
    let mut context = AgentContext::new(Uuid::new_v4(), window);
    let mut i = 0usize;
    while context.usage_percentage() <= 95.0 {
        context.add_message(Message::user(format!("msg-{i} {}", filler())));
        i += 1;
        assert!(i < 400, "fixture must reach the 90% floor");
    }
    context
}

// ---------------------------------------------------------------------------
// The defect: a background spawn must announce itself, exactly once
// ---------------------------------------------------------------------------

/// The regression #1047 asks for: with backgrounding on, the gate visit that
/// starts the summariser emits ONE `Compacting` carrying the visit's fill.
///
/// Fails on `2654b86da`: the spawn arm returns above the only `Compacting`
/// emit, so the log is empty and this test sees zero.
#[tokio::test]
async fn a_background_spawn_emits_exactly_one_compacting() {
    let service = service_with(Arc::new(PendingSummariser), true).await;
    let mut context = context_in_the_band(&service, 200_000).await;
    let session_id = context.session_id;
    let expected_pct = service.budget_usage_pct(session_id, &context);
    let (cb, log) = progress_recorder();

    let outcome = service
        .enforce_context_budget(
            session_id,
            &mut context,
            "mock-model",
            None,
            &Some(cb),
            BudgetPhase::MidLoop,
        )
        .await;

    // The whole point of backgrounding: the turn is handed back untouched.
    assert!(
        outcome.is_none(),
        "a background spawn must return without a synchronous outcome"
    );
    assert!(
        service.has_pending_compaction_for_tests(session_id),
        "the summariser must actually be in the slot"
    );

    let events = log.lock().unwrap();
    let pcts = compacting_pcts(&events);
    assert_eq!(
        pcts.len(),
        1,
        "one background spawn owes exactly one Compacting announcement: {events:?}"
    );
    assert!(
        (pcts[0] - expected_pct).abs() < 1e-9,
        "the announcement carries the visit's fill ({expected_pct:.4}%), not another \
         measurement: got {:.4}%",
        pcts[0]
    );
    assert!(
        pcts[0] > 65.0,
        "no Compacting line can sit below the gate that triggers compaction: {:.1}%",
        pcts[0]
    );
}

/// The #1686/#1023 regression guard: a visit that re-enters the gate while the
/// summariser is still in flight announces nothing further and starts no second
/// summariser.
///
/// This is the property session `a58b8714` paid 128 duplicate announcements to
/// learn. If the new emit were hoisted above the gate, this test goes red.
#[tokio::test]
async fn a_re_entering_visit_emits_no_further_compacting() {
    let service = service_with(Arc::new(PendingSummariser), true).await;
    let mut context = context_in_the_band(&service, 200_000).await;
    let session_id = context.session_id;
    let (cb, log) = progress_recorder();

    // Visit 1 — the spawn. Exactly one announcement.
    service
        .enforce_context_budget(
            session_id,
            &mut context,
            "mock-model",
            None,
            &Some(cb.clone()),
            BudgetPhase::MidLoop,
        )
        .await;
    assert_eq!(
        compacting_pcts(&log.lock().unwrap()).len(),
        1,
        "the spawn visit announces once"
    );
    let spawned_snapshot_len = service
        .pending_snapshot_len_for_tests(session_id)
        .expect("visit 1 must leave a pending summariser in the slot");

    // Visit 2 — the summariser is in flight. Above the trigger, below the
    // ceiling, MidLoop: the visit carries on and must stay silent.
    let outcome = service
        .enforce_context_budget(
            session_id,
            &mut context,
            "mock-model",
            None,
            &Some(cb),
            BudgetPhase::MidLoop,
        )
        .await;

    assert!(
        outcome.is_none(),
        "a visit that re-enters the gate while a summariser runs changes nothing"
    );
    assert_eq!(
        compacting_pcts(&log.lock().unwrap()).len(),
        1,
        "#1686/#1023: a re-entering visit must emit ZERO further Compacting — one \
         summariser, one announcement (session a58b8714 paid 128 for one)"
    );
    assert_eq!(
        service.pending_snapshot_len_for_tests(session_id),
        Some(spawned_snapshot_len),
        "the running summariser keeps its slot — no second one is started"
    );
}

/// The synchronous path is untouched: with backgrounding off, the single emit
/// below the gate still fires, once, and the visit actually summarises.
#[tokio::test]
async fn the_synchronous_path_still_announces_exactly_once() {
    let service = service_with(Arc::new(AnsweringSummariser), false).await;
    let mut context = context_over_95(32_000);
    let session_id = context.session_id;
    let (cb, log) = progress_recorder();

    let outcome = service
        .enforce_context_budget(
            session_id,
            &mut context,
            "mock-model",
            None,
            &Some(cb),
            BudgetPhase::MidLoop,
        )
        .await;

    assert!(
        matches!(outcome, Some(CompactionOutcome::Summarised(_))),
        "the blocking path must summarise, not truncate"
    );
    let events = log.lock().unwrap();
    assert_eq!(
        compacting_pcts(&events).len(),
        1,
        "the synchronous path keeps its single announcement: {events:?}"
    );
}

// ---------------------------------------------------------------------------
// The wiring, pinned at the source: the emit lives INSIDE the background spawn
// ---------------------------------------------------------------------------

/// A structural pin so the emit cannot drift back out of
/// `spawn_background_compaction`, or to the wrong side of the detached
/// summariser call, without a test noticing. The behavioural tests above prove
/// the event is emitted; this proves WHERE, which is the part a future
/// refactor is most likely to move for an unrelated reason.
#[test]
fn the_announcement_sits_inside_the_background_spawn() {
    const COMPACTION: &str = include_str!("../brain/agent/service/compaction.rs");

    let start = COMPACTION
        .find("async fn spawn_background_compaction(")
        .expect("spawn_background_compaction");
    // Bounded by the NEXT item, not a fixed byte count: a fixed window silently
    // stops covering the function the moment a line is added above the
    // assertion.
    let rest = &COMPACTION[start + 1..];
    let end = [
        rest.find("\n    async fn "),
        rest.find("\n    fn "),
        rest.find("\n    pub(crate) fn "),
        rest.find("\n    pub(super) fn "),
    ]
    .into_iter()
    .flatten()
    .min()
    .unwrap_or(rest.len());
    let body = &rest[..end];
    assert!(
        body.len() > 800,
        "the extracted body must be the real function, got {} bytes",
        body.len()
    );

    let spawn_event = body
        .find("CompactionEventStage::BackgroundSpawn")
        .expect("the BackgroundSpawn record");
    let emit = body
        .find("ProgressEvent::Compacting {")
        .expect("a background spawn must emit ProgressEvent::Compacting (#1047)");
    let call = body
        .find("self.spawn_background_compaction_with(")
        .expect("the detached summariser call");

    assert!(
        spawn_event < emit,
        "the announcement follows the spawn's own structured record"
    );
    assert!(
        emit < call,
        "and precedes the detached summariser, so the user hears about it before \
         the task that produces it starts"
    );
    assert_eq!(
        body.matches("ProgressEvent::Compacting {").count(),
        1,
        "exactly one emit per spawn — a second would double-announce"
    );
    assert!(
        body.contains("self.predicted_compaction_elapsed(session_id)"),
        "the ETA hint is grounded the same way the wait path grounds it"
    );
}

// ---------------------------------------------------------------------------
// #1065 — the announcement is LOGGED on both branches
// ---------------------------------------------------------------------------

/// Records every `tracing` event's fields, so a test can assert on the line the
/// deployed binary actually writes.
///
/// #1047 put the emit in place and logged nothing at the emit site, so the
/// owner's report could not be judged: "the emit fired but nobody was
/// listening" and "the emit never fired" were the same absence in the log. The
/// two tests below are the difference — the same code path, once with a bound
/// callback and once without, each owing exactly one line whose `listening`
/// field says which branch ran.
#[derive(Clone, Default)]
struct EventCapture {
    events: Arc<Mutex<Vec<String>>>,
}

impl EventCapture {
    /// Every captured event whose fields contain `needle`.
    fn matching(&self, needle: &str) -> Vec<String> {
        self.events
            .lock()
            .unwrap()
            .iter()
            .filter(|e| e.contains(needle))
            .cloned()
            .collect()
    }
}

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for EventCapture {
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let mut fields = String::new();
        event.record(&mut FieldVisitor { out: &mut fields });
        self.events.lock().unwrap().push(fields);
    }
}

struct FieldVisitor<'a> {
    out: &'a mut String,
}

impl tracing::field::Visit for FieldVisitor<'_> {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        use std::fmt::Write;
        let _ = write!(self.out, "{}={:?} ", field.name(), value);
    }

    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        use std::fmt::Write;
        let _ = write!(self.out, "{}=\"{}\" ", field.name(), value);
    }
}

/// Install the capture layer for the current thread. `#[tokio::test]` uses a
/// current-thread runtime, so the future is polled here and sees it; bind the
/// guard, do not discard it.
fn capture() -> (EventCapture, tracing::subscriber::DefaultGuard) {
    use tracing_subscriber::layer::SubscriberExt;
    let cap = EventCapture::default();
    let subscriber = tracing_subscriber::registry().with(cap.clone());
    let guard = tracing::subscriber::set_default(subscriber);
    (cap, guard)
}

const ANNOUNCEMENT: &str = "Background compaction announcement emitted";

/// The `Some(cb)` branch — the one a channel can render. The line must name the
/// session, the visit's fill, and `listening="some"`.
#[tokio::test]
async fn the_announcement_is_logged_as_listening_when_a_callback_is_bound() {
    let service = service_with(Arc::new(PendingSummariser), true).await;
    let mut context = context_in_the_band(&service, 200_000).await;
    let session_id = context.session_id;
    let expected_pct = service.budget_usage_pct(session_id, &context);
    let (cb, _log) = progress_recorder();

    let (cap, _guard) = capture();
    service
        .enforce_context_budget(
            session_id,
            &mut context,
            "mock-model",
            None,
            &Some(cb),
            BudgetPhase::MidLoop,
        )
        .await;

    let lines = cap.matching(ANNOUNCEMENT);
    assert_eq!(
        lines.len(),
        1,
        "one background spawn owes exactly one announcement line: {lines:?}"
    );
    let line = &lines[0];
    assert!(
        line.contains(&format!("session_id={session_id}")),
        "the line must be attributable to its session: {line}"
    );
    assert!(
        line.contains("listening=\"some\""),
        "a bound callback reads as `some`: {line}"
    );
    // Compared numerically, not as a rendered string: the gate recomputes the
    // fill on its own path and the two readings agree to within float noise
    // (the sibling emit test above uses the same 1e-9 tolerance), while an
    // exact string match would also be hostage to Debug-vs-Display formatting
    // of an f64.
    let logged: f64 = line
        .split("usage_pct=")
        .nth(1)
        .and_then(|rest| rest.split_whitespace().next())
        .expect("the announcement must carry usage_pct")
        .parse()
        .expect("usage_pct must parse as a float");
    assert!(
        (logged - expected_pct).abs() < 1e-9,
        "the line carries the visit's own fill ({expected_pct}), not another \
         measurement: {line}"
    );
}

/// The `None` branch — a headless or sub-agent session, which binds no progress
/// callback. The event reaches nobody, and THAT is the case the owner's report
/// lives in. It must still log, with `listening="none"`, because silence here
/// is indistinguishable from the emit never having fired.
#[tokio::test]
async fn the_announcement_is_logged_as_unheard_when_no_callback_is_bound() {
    let service = service_with(Arc::new(PendingSummariser), true).await;
    let mut context = context_in_the_band(&service, 200_000).await;
    let session_id = context.session_id;

    let (cap, _guard) = capture();
    service
        .enforce_context_budget(
            session_id,
            &mut context,
            "mock-model",
            None,
            &None,
            BudgetPhase::MidLoop,
        )
        .await;

    // `listening="none"` is a statement about the audience, never a reason to
    // skip the compaction itself.
    assert!(
        service.has_pending_compaction_for_tests(session_id),
        "a callback-less visit must still start the summariser"
    );

    let lines = cap.matching(ANNOUNCEMENT);
    assert_eq!(
        lines.len(),
        1,
        "the emit fires with or without a listener, and must be logged either \
         way: {lines:?}"
    );
    let line = &lines[0];
    assert!(
        line.contains(&format!("session_id={session_id}")),
        "the line must be attributable to its session: {line}"
    );
    assert!(
        line.contains("listening=\"none\""),
        "the unheard branch must be distinguishable from the emit never \
         firing: {line}"
    );
}
