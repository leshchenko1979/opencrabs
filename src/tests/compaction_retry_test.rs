//! #778: a summariser that answers with NO document is asked once more.
//!
//! A reasoning model spends the summariser's whole output allowance thinking
//! and returns nothing usable. Measured 2026-10-15: 42 `MaxTokens` stops in a
//! single day, 8 of them empty. An empty document is worse than no document —
//! `enforce_summary_budget` can only re-attach the `context-manifest` fence and
//! the §0 obligation line from a document that already carries them, so a stub
//! lands fence-less and the resuming turn loses its manifest. One session
//! compacted four times in two minutes because an empty summary bought it no
//! room.
//!
//! The fallback chain never sees this failure: a provider that answers with
//! nothing is a SUCCESS to it. So the retry has to sit above the walk, in
//! `complete_compaction_request_with_retry` — which is what these tests pin.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use crate::brain::agent::service::AgentService;
use crate::brain::provider::{
    ContentBlock, ContentDelta, LLMRequest, LLMResponse, Message, Provider, ProviderStream, Role,
    StopReason, StreamEvent, StreamMessage, TokenUsage,
};

/// Long enough that no mock in this file can reach it: these tests are about
/// how many times the summariser is asked, not how long one attempt is given.
const ATTEMPT_DEADLINE: std::time::Duration = std::time::Duration::from_secs(300);

/// Answers `empties` documents with no text, then produces one. `usize::MAX`
/// models a provider that never produces a document at all.
struct EmptyFirstMock {
    name: String,
    empties: usize,
    calls: Arc<AtomicUsize>,
}

impl EmptyFirstMock {
    fn new(name: &str, empties: usize) -> Self {
        Self {
            name: name.to_string(),
            empties,
            calls: Arc::new(AtomicUsize::new(0)),
        }
    }

    fn call_counter(&self) -> Arc<AtomicUsize> {
        self.calls.clone()
    }

    /// Counts the call and reports whether THIS one comes back empty.
    fn is_empty_call(&self) -> bool {
        self.calls.fetch_add(1, Ordering::SeqCst) < self.empties
    }
}

#[async_trait]
impl Provider for EmptyFirstMock {
    async fn complete(
        &self,
        request: LLMRequest,
    ) -> crate::brain::provider::error::Result<LLMResponse> {
        let empty = self.is_empty_call();
        Ok(LLMResponse {
            id: format!("{}-response", self.name),
            model: request.model,
            content: if empty {
                Vec::new()
            } else {
                vec![ContentBlock::Text {
                    text: format!("summary from {}", self.name),
                }]
            },
            // The measured shape: the allowance was spent, the document was not
            // written. An empty stop is what the retry exists for.
            stop_reason: empty.then_some(StopReason::MaxTokens),
            usage: TokenUsage::default(),
            streaming_active_secs: None,
            tool_text_leak: false,
        })
    }

    /// Compaction asks a non-CLI provider through the stream path and folds the
    /// events into one response (#1519), so the mock answers both the same way.
    /// An empty document is a stream that never emits a text delta.
    async fn stream(
        &self,
        request: LLMRequest,
    ) -> crate::brain::provider::error::Result<ProviderStream> {
        let empty = self.is_empty_call();
        let mut events = vec![Ok(StreamEvent::MessageStart {
            message: StreamMessage {
                id: format!("{}-response", self.name),
                model: request.model,
                role: Role::Assistant,
                usage: TokenUsage::default(),
            },
        })];
        if !empty {
            events.push(Ok(StreamEvent::ContentBlockDelta {
                index: 0,
                delta: ContentDelta::TextDelta {
                    text: format!("summary from {}", self.name),
                },
            }));
        }
        events.push(Ok(StreamEvent::MessageStop));
        Ok(Box::pin(futures::stream::iter(events)))
    }

    fn name(&self) -> &str {
        &self.name
    }

    fn default_model(&self) -> &str {
        "mock-default"
    }

    fn supported_models(&self) -> Vec<String> {
        Vec::new()
    }

    fn context_window(&self, _model: &str) -> Option<u32> {
        Some(200_000)
    }

    fn calculate_cost(&self, _model: &str, _input_tokens: u32, _output_tokens: u32) -> f64 {
        0.0
    }
}

fn request(model: &str) -> LLMRequest {
    LLMRequest::new(model.to_string(), vec![Message::user("compact me")])
}

/// The production text extractor is `pub(super)` inside the service module, so
/// mirror it here rather than widening its visibility for a test.
fn text_of(response: &LLMResponse) -> String {
    response
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Empty-at-MaxTokens on the primary must walk the fallback chain (#1007).
#[tokio::test]
async fn empty_document_is_retried_on_fallback() {
    let primary_mock = EmptyFirstMock::new("cr-empty-first", usize::MAX);
    let primary_calls = primary_mock.call_counter();
    let primary: Arc<dyn Provider> = Arc::new(primary_mock);
    let fallback_mock = EmptyFirstMock::new("cr-fallback-ok", 0);
    let fallback_calls = fallback_mock.call_counter();
    let chain: Vec<Arc<dyn Provider>> = vec![Arc::new(fallback_mock)];

    let response = AgentService::complete_compaction_request_with_retry(
        &primary,
        &chain,
        request("primary-model"),
        &CancellationToken::new(),
        ATTEMPT_DEADLINE,
        None,
        None,
    )
    .await
    .expect("#1007: the fallback must surface a usable document");

    assert_eq!(
        primary_calls.load(Ordering::SeqCst),
        1,
        "primary is tried once"
    );
    assert_eq!(
        fallback_calls.load(Ordering::SeqCst),
        1,
        "fallback is tried once"
    );
    assert_eq!(
        text_of(&response),
        "summary from cr-fallback-ok",
        "the fallback's document must reach the compaction path"
    );
}

/// The retry is for EMPTY documents only. A document that arrived on the first
/// attempt must not cost a second request — a summariser that answers costs the
/// user real tokens and real seconds.
#[tokio::test]
async fn non_empty_document_is_not_retried() {
    let mock = EmptyFirstMock::new("cr-first-try", 0);
    let calls = mock.call_counter();
    let primary: Arc<dyn Provider> = Arc::new(mock);

    let response = AgentService::complete_compaction_request_with_retry(
        &primary,
        &[],
        request("primary-model"),
        &CancellationToken::new(),
        ATTEMPT_DEADLINE,
        None,
        None,
    )
    .await
    .expect("a first-attempt document must be returned as-is");

    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "a non-empty document must not trigger the retry"
    );
    assert_eq!(text_of(&response), "summary from cr-first-try");
}

/// A provider that is simply broken must not be asked forever: exactly one
/// retry, then whatever came back is surfaced. The caller still has to decide
/// what an empty document means — silence here would be worse.
#[tokio::test]
async fn persistently_empty_document_stops_after_one_retry() {
    let mock = EmptyFirstMock::new("cr-never-answers", usize::MAX);
    let calls = mock.call_counter();
    let primary: Arc<dyn Provider> = Arc::new(mock);

    let response = AgentService::complete_compaction_request_with_retry(
        &primary,
        &[],
        request("primary-model"),
        &CancellationToken::new(),
        ATTEMPT_DEADLINE,
        None,
        None,
    )
    .await
    .expect("an empty document is not an error — it is returned, not raised");

    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "no fallback chain — primary only"
    );
    assert!(
        text_of(&response).is_empty(),
        "a persistently empty summariser surfaces the empty document it got"
    );
}

// ---------------------------------------------------------------------------
// #994: the CUT class.
//
// `finish_reason=length` WITH text present is not a document — it is a prefix
// of one. The old predicate classified it by text length alone, so a cut
// fragment came back "healthy" and was persisted as a note: it FITS the summary
// budget by construction, which is why the trimmer never inspected the class.
// Measured 2026-11-24: 9 cut attempts in one day, one of them persisted as an
// 848-byte note carrying a 21,904-character reasoning payload and zero text
// characters.
// ---------------------------------------------------------------------------

/// Builds the exact response shape the classifier reads, without a provider.
fn answer(text: &str, stop: Option<StopReason>) -> LLMResponse {
    LLMResponse {
        id: "truth-table".to_string(),
        model: "truth-table".to_string(),
        content: vec![ContentBlock::Text {
            text: text.to_string(),
        }],
        stop_reason: stop,
        usage: TokenUsage::default(),
        streaming_active_secs: None,
        tool_text_leak: false,
    }
}

/// The whole truth table in one place: what the retry predicate decides for
/// every answer a summariser can give, and what the exhaustion predicate says
/// about the same answer.
#[test]
fn the_retry_predicate_classifies_every_answer() {
    // (label, text, stop_reason, expected `summariser_answer_is_retryable`)
    let table: &[(&str, &str, Option<StopReason>, bool)] = &[
        // #778: nothing was written at all. The measured reasoning-model shape.
        ("empty + MaxTokens", "", Some(StopReason::MaxTokens), true),
        ("empty + no stop reason", "", None, true),
        ("whitespace only", "  \n\t ", None, true),
        // #994: text was written and the provider says the allowance ended it.
        (
            "cut with partial text",
            "## 1. Summary\nThe lane was",
            Some(StopReason::MaxTokens),
            true,
        ),
        // A finished document must never cost a second request.
        (
            "stopped document",
            "## 1. Summary\nThe lane shipped.",
            Some(StopReason::EndTurn),
            false,
        ),
        (
            "document, no stop reason",
            "## 1. Summary\nThe lane shipped.",
            None,
            false,
        ),
        (
            "document stopped on a sequence",
            "## 1. Summary\nThe lane shipped.",
            Some(StopReason::StopSequence),
            false,
        ),
    ];

    for (label, text, stop, expected) in table {
        let response = answer(text, stop.clone());
        assert_eq!(
            AgentService::summariser_answer_is_retryable(&response),
            *expected,
            "summariser_answer_is_retryable misclassified: {label}"
        );
        // The exhaustion predicate is the `cut` half alone. An empty document
        // that stays empty is absent, not truncated, and the two get different
        // markers downstream — so this must NOT follow the retry column.
        assert_eq!(
            AgentService::completion_is_cut(&response),
            matches!(stop, Some(StopReason::MaxTokens)),
            "completion_is_cut misclassified: {label}"
        );
    }
}

/// One scripted summariser answer: the text written and the stop reason the
/// provider reports for it.
struct ScriptedMock {
    name: String,
    script: Vec<(&'static str, Option<StopReason>)>,
    calls: Arc<AtomicUsize>,
}

impl ScriptedMock {
    fn new(name: &str, script: Vec<(&'static str, Option<StopReason>)>) -> Self {
        assert!(
            !script.is_empty(),
            "a scripted mock needs at least one answer"
        );
        Self {
            name: name.to_string(),
            script,
            calls: Arc::new(AtomicUsize::new(0)),
        }
    }

    fn call_counter(&self) -> Arc<AtomicUsize> {
        self.calls.clone()
    }

    /// The answer for THIS call. The last entry repeats, so a provider that
    /// never improves is a one-entry script rather than a special case.
    fn next_answer(&self) -> (&'static str, Option<StopReason>) {
        let index = self.calls.fetch_add(1, Ordering::SeqCst);
        self.script
            .get(index)
            .or_else(|| self.script.last())
            .cloned()
            .expect("script is non-empty")
    }
}

#[async_trait]
impl Provider for ScriptedMock {
    async fn complete(
        &self,
        request: LLMRequest,
    ) -> crate::brain::provider::error::Result<LLMResponse> {
        let (text, stop_reason) = self.next_answer();
        Ok(LLMResponse {
            id: format!("{}-response", self.name),
            model: request.model,
            content: vec![ContentBlock::Text {
                text: text.to_string(),
            }],
            stop_reason,
            usage: TokenUsage::default(),
            streaming_active_secs: None,
            tool_text_leak: false,
        })
    }

    /// Compaction reaches a non-CLI provider through the stream path and folds
    /// the events into one response (#1519), so the mock has to report the stop
    /// reason the way a real stream does: on `MessageDelta`, after the text.
    async fn stream(
        &self,
        request: LLMRequest,
    ) -> crate::brain::provider::error::Result<ProviderStream> {
        let (text, stop_reason) = self.next_answer();
        let mut events = vec![Ok(StreamEvent::MessageStart {
            message: StreamMessage {
                id: format!("{}-response", self.name),
                model: request.model,
                role: Role::Assistant,
                usage: TokenUsage::default(),
            },
        })];
        if !text.is_empty() {
            events.push(Ok(StreamEvent::ContentBlockDelta {
                index: 0,
                delta: ContentDelta::TextDelta {
                    text: text.to_string(),
                },
            }));
        }
        events.push(Ok(StreamEvent::MessageDelta {
            delta: crate::brain::provider::MessageDelta {
                stop_reason,
                stop_sequence: None,
            },
            usage: TokenUsage::default(),
        }));
        events.push(Ok(StreamEvent::MessageStop));
        Ok(Box::pin(futures::stream::iter(events)))
    }

    fn name(&self) -> &str {
        &self.name
    }

    fn default_model(&self) -> &str {
        "mock-default"
    }

    fn supported_models(&self) -> Vec<String> {
        Vec::new()
    }

    fn context_window(&self, _model: &str) -> Option<u32> {
        Some(200_000)
    }

    fn calculate_cost(&self, _model: &str, _input_tokens: u32, _output_tokens: u32) -> f64 {
        0.0
    }
}

/// A cut document costs exactly one more request, and the whole document the
/// second attempt writes is what surfaces — not the fragment.
#[tokio::test]
async fn cut_document_with_partial_text_is_retried_once() {
    let primary_mock = Arc::new(ScriptedMock::new(
        "cr-cut-primary",
        vec![("## 1. Summary\nThe lane was", Some(StopReason::MaxTokens))],
    ));
    let primary_calls = primary_mock.call_counter();
    let primary: Arc<dyn Provider> = primary_mock;
    let fallback_mock = Arc::new(ScriptedMock::new(
        "cr-cut-fallback",
        vec![(
            "## 1. Summary\nThe lane shipped.",
            Some(StopReason::EndTurn),
        )],
    ));
    let fallback_calls = fallback_mock.call_counter();
    let chain: Vec<Arc<dyn Provider>> = vec![fallback_mock];

    let response = AgentService::complete_compaction_request_with_retry(
        &primary,
        &chain,
        request("primary-model"),
        &CancellationToken::new(),
        ATTEMPT_DEADLINE,
        None,
        None,
    )
    .await
    .expect("#994/#1007: a cut document walks the fallback chain");

    assert_eq!(primary_calls.load(Ordering::SeqCst), 1);
    assert_eq!(fallback_calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        text_of(&response),
        "## 1. Summary\nThe lane shipped.",
        "the fallback must surface a whole document"
    );
    assert!(
        !AgentService::completion_is_cut(&response),
        "the second answer is a whole document, so the caller must not mark it truncated"
    );
}

/// When the retry is cut too, the caller gets the fragment WITH the verdict
/// attached: that is what turns it into an honest marker instead of a note.
#[tokio::test]
async fn cut_document_after_retry_is_classified_cut() {
    let mock = Arc::new(ScriptedMock::new(
        "cr-always-cut",
        vec![("## 1. Summary\nThe lane was", Some(StopReason::MaxTokens))],
    ));
    let calls = mock.call_counter();
    let primary: Arc<dyn Provider> = mock;

    let response = AgentService::complete_compaction_request_with_retry(
        &primary,
        &[],
        request("primary-model"),
        &CancellationToken::new(),
        ATTEMPT_DEADLINE,
        None,
        None,
    )
    .await
    .expect("a persistently cut document is not an error — it is returned");

    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "no fallback chain — primary only"
    );
    assert!(
        AgentService::completion_is_cut(&response),
        "an exhausted retry must still carry the provider's cut verdict, or the \
         fragment gets persisted as a summary"
    );
    assert_eq!(
        text_of(&response),
        "## 1. Summary\nThe lane was",
        "the cut fragment is handed back for the log line, not silently dropped"
    );
}

/// The other half of the contract: a healthy document is asked ONCE. Widening
/// the predicate must not turn every compaction into two summariser calls.
#[tokio::test]
async fn a_stopped_document_with_text_is_not_retried() {
    let mock = Arc::new(ScriptedMock::new(
        "cr-clean-first",
        vec![(
            "## 1. Summary\nThe lane shipped.",
            Some(StopReason::EndTurn),
        )],
    ));
    let calls = mock.call_counter();
    let primary: Arc<dyn Provider> = mock;

    let response = AgentService::complete_compaction_request_with_retry(
        &primary,
        &[],
        request("primary-model"),
        &CancellationToken::new(),
        ATTEMPT_DEADLINE,
        None,
        None,
    )
    .await
    .expect("a healthy document is returned");

    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "a whole document must never cost a second request"
    );
    assert_eq!(text_of(&response), "## 1. Summary\nThe lane shipped.");
}
