//! The #934 head-cut ordering invariant, driven through the real budget gate.
//!
//! Two ends of one window, two rules, and both are asserted here against the
//! production path (`enforce_context_budget`) rather than against a re-written
//! helper:
//!
//! 1. **The summariser reads the head.** When the gate hands the window to the
//!    summariser, the messages arrive in chronological order — oldest first —
//!    with the continuation-document directive appended last. The selection
//!    inside `compute_compaction_summary` walks the window from the newest end
//!    (`rev().take_while(..)`), which is easy to misread as "the summariser
//!    sees the tail first": it does not, it only *selects* from the tail when
//!    the input budget is short. What the provider receives is re-ordered back
//!    into conversation order. A reversal there hands the summariser the
//!    conversation backwards and the #934 document describes the wrong end of
//!    the session as "IMMEDIATE TASK".
//!
//! 2. **A truncation cuts the head.** The Tier-2 / safety floor
//!    (`safety_truncate_to_80` → `hard_truncate_to`) drops the OLDEST messages
//!    and leaves the newest ones byte-identical. The newest messages are the
//!    only context the woken turn has left, so a cut that ate the tail instead
//!    would preserve history nobody needs and destroy the exchange in progress.
//!
//! The two paths differ in one honest way, and the assertions state it rather
//! than papering over it: the synchronous summary path is FULL-WINDOW by
//! definition (`compact_context` → `apply_compaction_summary` →
//! `compact_with_summary(marker, 0)`), so the pre-compaction messages do not
//! survive verbatim in the context afterwards — the marker IS the context
//! (#1928). "Newest survives verbatim" is therefore an assertion about the
//! truncation path, and about what the summariser is *sent* on the summary
//! path.
//!
//! Every test runs under a throwaway profile home (`with_profile_home_async`):
//! a successful summariser writes its document to the profile's daily memory
//! log, which must never be the live home.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use uuid::Uuid;

use crate::brain::agent::context::AgentContext;
use crate::brain::agent::service::AgentService;
use crate::brain::agent::service::compaction::{BudgetPhase, CompactionOutcome};
use crate::brain::provider::{
    ContentBlock, LLMRequest, LLMResponse, Message, Provider, ProviderError, ProviderStream,
    StopReason, TokenUsage,
};
use crate::config::profile::{home_for_profile, with_profile_home_async};
use crate::db::Database;
use crate::services::ServiceContext;

const MODEL: &str = "mock-model";

/// The document the mock summariser returns. Deliberately free of file-like
/// claims: `flag_unbacked_artifacts` appends a harness note to a summary that
/// reports artifacts nothing produced, and the assertions below read the
/// marker's bytes.
const SUMMARY: &str = "SUMMARY-OK: the pre-compaction window was read head-first.";

/// Chars of filler per message. Real words, so tiktoken counts them like a
/// conversation rather than like one giant token.
const FILLER_CHARS: usize = 12_000;

fn filler(chars: usize) -> String {
    let mut out = String::with_capacity(chars + 64);
    while out.len() < chars {
        out.push_str("the window moved and the harness measured it again ");
    }
    out
}

/// `MSG-07` — the tag a message carries so the assertions can name it.
fn tag(index: usize) -> String {
    format!("MSG-{index:02}")
}

fn tagged(index: usize) -> Message {
    Message::user(format!("{} {}", tag(index), filler(FILLER_CHARS)))
}

fn text_of(message: &Message) -> String {
    message
        .content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text { text } => Some(text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// What the mock summariser should do with a request.
enum Reply {
    Summary(String),
    Fail,
}

/// A provider that records every message list it is asked to summarise.
///
/// The compaction path asks HTTP-shaped providers through `stream()` (only CLI
/// providers go through `complete()`), so both entry points record — a test
/// that asserted on `complete()` alone would observe nothing.
struct RecordingSummariser {
    reply: Reply,
    requests: Mutex<Vec<Vec<Message>>>,
}

impl RecordingSummariser {
    fn new(reply: Reply) -> Self {
        Self {
            reply,
            requests: Mutex::new(Vec::new()),
        }
    }

    fn requests(&self) -> Vec<Vec<Message>> {
        self.requests
            .lock()
            .expect("requests lock poisoned")
            .clone()
    }

    fn respond(&self, request: LLMRequest) -> crate::brain::provider::Result<LLMResponse> {
        self.requests
            .lock()
            .expect("requests lock poisoned")
            .push(request.messages.clone());
        match &self.reply {
            Reply::Fail => Err(ProviderError::InvalidApiKey),
            Reply::Summary(text) => Ok(LLMResponse {
                id: "recording-summary".to_string(),
                model: MODEL.to_string(),
                content: vec![ContentBlock::Text { text: text.clone() }],
                stop_reason: Some(StopReason::EndTurn),
                usage: TokenUsage {
                    input_tokens: 100,
                    output_tokens: 40,
                    ..Default::default()
                },
                streaming_active_secs: None,
                tool_text_leak: false,
            }),
        }
    }
}

#[async_trait]
impl Provider for RecordingSummariser {
    async fn complete(&self, request: LLMRequest) -> crate::brain::provider::Result<LLMResponse> {
        self.respond(request)
    }

    async fn stream(&self, request: LLMRequest) -> crate::brain::provider::Result<ProviderStream> {
        use crate::brain::provider::{ContentDelta, MessageDelta, StreamEvent, StreamMessage};

        let response = self.respond(request)?;
        let mut events = vec![Ok(StreamEvent::MessageStart {
            message: StreamMessage {
                id: response.id.clone(),
                model: response.model.clone(),
                role: crate::brain::provider::Role::Assistant,
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
        "recording-summariser"
    }

    fn default_model(&self) -> &str {
        MODEL
    }

    fn supported_models(&self) -> Vec<String> {
        vec![MODEL.to_string()]
    }

    fn context_window(&self, _model: &str) -> Option<u32> {
        Some(200_000)
    }

    fn calculate_cost(&self, _model: &str, _input: u32, _output: u32) -> f64 {
        0.0
    }
}

/// A service whose budget gate runs the SYNCHRONOUS compaction lane.
///
/// `background_compaction` must be off: with it on, the 65% gate spawns the
/// summariser and returns before the provider has recorded anything, so the
/// ordering assertions would race the spawn. `Config::default()` carries the
/// setting, so the test builds the service from an explicit config rather than
/// mutating a private field after construction.
async fn service_with(provider: Arc<dyn Provider>) -> AgentService {
    let db = Database::connect_in_memory().await.unwrap();
    db.run_migrations().await.unwrap();
    let context = ServiceContext::new(db.pool().clone());

    let mut config = crate::config::Config::default();
    config.agent.background_compaction = false;
    // Same pin as `AgentService::new_for_test`: the default policy
    // (`auto-always`) would silently auto-approve every gated tool.
    config.agent.approval_policy = "ask".to_string();
    AgentService::new(provider, context, &config).await
}

/// Grow the context until it crosses `floor_pct`, tagging each message so the
/// assertions can name the head and the tail without hard-coding a count.
fn fill_to(ctx: &mut AgentContext, floor_pct: f64) -> Vec<Message> {
    let mut index = 0usize;
    while ctx.usage_percentage() < floor_pct {
        ctx.add_message(tagged(index));
        index += 1;
        assert!(
            index < 200,
            "sizing loop never reached {floor_pct}% — filler is not being counted"
        );
    }
    ctx.messages.clone()
}

/// The summary path: the summariser is handed the window oldest-first, the
/// directive closes it, and the applied marker becomes the whole context.
#[tokio::test]
async fn the_summariser_is_handed_the_window_oldest_first() {
    let profile = format!("test_head_cut_summary_{}", Uuid::new_v4());
    let home = home_for_profile(Some(&profile));

    with_profile_home_async(Some(&profile), async {
        let provider = Arc::new(RecordingSummariser::new(Reply::Summary(
            SUMMARY.to_string(),
        )));
        let service = service_with(provider.clone()).await;

        // Tier 1 only: above the 65% gate, below the 90% floor, so nothing is
        // truncated before the summariser runs and the whole window is in
        // play. 100k tokens leaves the summariser input budget
        // (100k - 6.5k) far above the window, so nothing is dropped for size
        // either — the ordering under test is the only thing the assertions
        // can be reading.
        let max_tokens = 100_000usize;
        let mut ctx = AgentContext::new(Uuid::new_v4(), max_tokens);
        let before = fill_to(&mut ctx, 70.0);
        let usage = ctx.usage_percentage();
        assert!(
            usage > 65.0 && usage < 90.0,
            "the fixture must sit in the Tier-1 band, got {usage:.1}%"
        );
        let newest = tag(before.len() - 1);

        let outcome = service
            .enforce_context_budget_for_test(
                ctx.session_id,
                &mut ctx,
                MODEL,
                None,
                &None,
                BudgetPhase::TurnStart,
            )
            .await;

        // ── 1. A summary landed, not a truncation ──
        let marker = match &outcome {
            Some(CompactionOutcome::Summarised(marker)) => marker.clone(),
            Some(_) => {
                panic!("a working summariser must produce Summarised, not the truncation/guard arm")
            }
            None => panic!("the gate returned None — it never attempted the summary"),
        };

        // ── 2. What the summariser was handed ──
        let requests = provider.requests();
        assert_eq!(
            requests.len(),
            1,
            "the synchronous lane makes exactly one summariser call here: the \
             applied marker sits far below the 65% target, so the re-compaction \
             pass must not fire"
        );
        let sent = &requests[0];
        assert_eq!(
            sent.len(),
            before.len() + 1,
            "the summariser must receive the whole window plus the directive; a \
             shorter list means the input budget cut messages the test expected \
             to be there"
        );
        // Every message verbatim, in conversation order. This is the invariant:
        // a reversal or a tail-first slice fails on the first mismatch.
        for (i, (sent_msg, original)) in sent[..before.len()].iter().zip(before.iter()).enumerate()
        {
            assert_eq!(
                text_of(sent_msg),
                text_of(original),
                "summariser input position {i} is not the message at that point in \
                 the conversation — the window reached the summariser out of order"
            );
        }
        assert!(
            text_of(&sent[0]).starts_with(&tag(0)),
            "the head of the window must reach the summariser first"
        );
        assert!(
            text_of(&sent[before.len() - 1]).starts_with(&newest),
            "the newest message must be the last conversation message the \
             summariser sees"
        );
        // ── 3. The directive closes the request, after the conversation ──
        let directive = text_of(sent.last().expect("the request is non-empty"));
        assert!(
            directive.contains("COMPREHENSIVE CONTINUATION DOCUMENT"),
            "the last message must be the continuation-document directive, got: {}",
            directive.lines().next().unwrap_or("<empty>")
        );
        assert!(
            !sent[..before.len()]
                .iter()
                .any(|m| text_of(m).contains("COMPREHENSIVE CONTINUATION DOCUMENT")),
            "the directive must not be mixed into the conversation window"
        );

        // ── 4. The applied marker is the whole context (#1928) ──
        // Full-window semantics: the summary replaces the window, so the
        // pre-compaction messages are gone by design and the marker's bytes are
        // exactly what the next request pays for.
        assert_eq!(
            ctx.messages.len(),
            1,
            "a full-window compaction must collapse the context to the marker"
        );
        assert_eq!(
            text_of(&ctx.messages[0]),
            marker,
            "#1928: the marker returned to the caller must be the bytes welded \
             into the live context"
        );
        assert!(
            marker.starts_with("[CONTEXT COMPACTION"),
            "the marker must open with the canonical banner, got: {}",
            marker.lines().next().unwrap_or("<empty>")
        );
        assert!(
            marker.contains(SUMMARY),
            "the applied marker must carry the summariser's document"
        );
        assert!(
            !marker.contains(&newest),
            "the newest message must live on only inside the summary, not \
             verbatim beside it — a verbatim copy is the #1676 duplicate"
        );
    })
    .await;

    let _ = std::fs::remove_dir_all(&home);
}

/// The truncation path: when every summariser attempt fails, the oldest
/// messages are dropped and the newest ones survive byte-identical.
#[tokio::test]
async fn a_failed_summariser_truncates_the_head_not_the_tail() {
    let profile = format!("test_head_cut_truncate_{}", Uuid::new_v4());
    let home = home_for_profile(Some(&profile));

    with_profile_home_async(Some(&profile), async {
        let provider = Arc::new(RecordingSummariser::new(Reply::Fail));
        let service = service_with(provider.clone()).await;

        // Above the 90% floor, so Tier 2 truncates to 80% first and the
        // summariser then fails on the already-cut window.
        let max_tokens = 40_000usize;
        let mut ctx = AgentContext::new(Uuid::new_v4(), max_tokens);
        let before = fill_to(&mut ctx, 95.0);
        let total = before.len();
        assert!(total > 4, "the fixture needs enough messages to cut into");
        let newest = tag(total - 1);

        let outcome = service
            .enforce_context_budget_for_test(
                ctx.session_id,
                &mut ctx,
                MODEL,
                None,
                &None,
                BudgetPhase::TurnStart,
            )
            .await;

        // ── 1. Every attempt failed, so the outcome is Truncated ──
        match &outcome {
            Some(CompactionOutcome::Truncated) => {}
            Some(_) => panic!(
                "a summariser that always errors must fall back to Truncated, not \
                 report a summary or a suppressed guard"
            ),
            None => panic!("the gate returned None — it never reported the truncation"),
        }
        assert_eq!(
            provider.requests().len(),
            3,
            "the gate makes up to three attempts before it truncates"
        );

        // ── 2. The newest messages survive verbatim, in order ──
        let kept = ctx.messages.len();
        assert!(
            kept >= 2,
            "`hard_truncate_to` keeps at least the most recent pair"
        );
        assert!(
            kept < total,
            "nothing was dropped — the fixture did not actually need a cut"
        );
        let start = total - kept;
        assert!(
            start > 0,
            "the cut must have removed messages from the head"
        );
        for (j, msg) in ctx.messages.iter().enumerate() {
            assert_eq!(
                text_of(msg),
                text_of(&before[start + j]),
                "kept message {j} is not the message that occupied that position — \
                 the truncation reordered the window"
            );
        }
        assert!(
            !text_of(&ctx.messages[0]).starts_with(&tag(0)),
            "the oldest message must be the one the cut removes"
        );
        assert_eq!(
            text_of(ctx.messages.last().expect("non-empty")),
            text_of(&before[total - 1]),
            "the newest message must survive byte-identical"
        );

        // ── 3. The cut reached its target: 80% of the window ──
        let target = (max_tokens as f64 * 0.80) as usize;
        assert!(
            ctx.token_count <= target,
            "the head cut must land at or below 80% of the window, got {} > {target}",
            ctx.token_count
        );

        // ── 4. The summariser saw the same head-cut window ──
        // The failed attempts are still evidence: the window they were handed
        // starts after the dropped head and ends at the newest message.
        for request in provider.requests() {
            let all: String = request.iter().map(text_of).collect::<Vec<_>>().join("\n");
            assert!(
                all.contains(&newest),
                "every summariser attempt must include the newest message"
            );
            assert!(
                !all.contains(&tag(0)),
                "the dropped head must not reappear in the summariser input"
            );
        }
    })
    .await;

    let _ = std::fs::remove_dir_all(&home);
}
