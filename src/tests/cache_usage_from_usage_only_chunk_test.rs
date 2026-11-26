//! Regression guards for #1038: `messages.cache_read_tokens` was persisted as
//! 0 on every call served by a backend that inlines a GROSS `prompt_tokens`
//! with no cache detail on the finish_reason chunk (cb/deepseek-v4.1-flash{,-sg})
//! and reports the real cache read on the trailing usage-only chunk.
//!
//! Mechanism, in three steps:
//!   1. the finish_reason chunk carried `prompt_tokens` → the inline path
//!      emitted `MessageDelta{cache_read: 0}` + `MessageStop` immediately;
//!   2. `helpers.rs` breaks the stream loop on the FIRST `MessageStop`, so the
//!      usage-only chunk that followed was never read;
//!   3. `total_cache_read` stayed 0 and `tool_loop.rs` wrote `Some(0)`.
//!
//! These tests therefore model the CONSUMER, not a drain-to-end collector: a
//! test that keeps reading after the first `MessageStop` sees the second
//! (correct) delta and cannot discriminate the bug at all. `usage_as_consumer`
//! below mirrors `helpers.rs` — break at the first `MessageStop`, accumulate
//! with `max()` — which is exactly what reaches the database.

use crate::brain::provider::types::StopReason;
use crate::brain::provider::{
    LLMRequest, Message, OpenAIProvider, Provider, StreamEvent, TokenUsage,
};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::time::timeout;

async fn serve_once(listener: TcpListener, sse: String) {
    let (mut sock, _) = listener.accept().await.expect("accept");
    let mut buf = [0u8; 8192];
    let _ = timeout(Duration::from_secs(5), sock.read(&mut buf)).await;
    let resp = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        sse.len(),
        sse
    );
    sock.write_all(resp.as_bytes()).await.expect("write body");
    sock.flush().await.ok();
}

/// Every event the provider emitted, drained to exhaustion — used only to
/// count final delta/stop pairs (#1738).
async fn events_from_sse(sse: String) -> Vec<StreamEvent> {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(serve_once(listener, sse));
    let provider = OpenAIProvider::local(format!("http://127.0.0.1:{port}/chat/completions"));
    let req = LLMRequest::new("some-gateway-model", vec![Message::user("hi")]);
    let mut stream = timeout(Duration::from_secs(10), provider.stream(req))
        .await
        .expect("stream opens in time")
        .expect("stream opens");
    let mut events = Vec::new();
    timeout(Duration::from_secs(10), async {
        while let Some(ev) = futures::StreamExt::next(&mut stream).await {
            events.push(ev.expect("event ok"));
        }
    })
    .await
    .expect("stream drains in time");
    events
}

/// The usage a real consumer ends up with: `helpers.rs` breaks the loop on the
/// first `MessageStop`, so only deltas up to that point count. Accumulates with
/// `max()` exactly as `helpers.rs` does.
fn usage_as_consumer(events: &[StreamEvent]) -> TokenUsage {
    let mut acc = TokenUsage::default();
    for ev in events {
        match ev {
            StreamEvent::MessageDelta { usage, .. } => {
                if usage.input_tokens > acc.input_tokens {
                    acc.input_tokens = usage.input_tokens;
                }
                if usage.output_tokens > acc.output_tokens {
                    acc.output_tokens = usage.output_tokens;
                }
                if usage.reasoning_tokens > acc.reasoning_tokens {
                    acc.reasoning_tokens = usage.reasoning_tokens;
                }
                if usage.cache_read_tokens > acc.cache_read_tokens {
                    acc.cache_read_tokens = usage.cache_read_tokens;
                }
                if usage.cache_creation_tokens > acc.cache_creation_tokens {
                    acc.cache_creation_tokens = usage.cache_creation_tokens;
                }
            }
            // `helpers.rs:1044` — the consumer stops here.
            StreamEvent::MessageStop => break,
            _ => {}
        }
    }
    acc
}

fn count_finals(events: &[StreamEvent]) -> (usize, usize) {
    let deltas = events
        .iter()
        .filter(|e| matches!(e, StreamEvent::MessageDelta { .. }))
        .count();
    let stops = events
        .iter()
        .filter(|e| matches!(e, StreamEvent::MessageStop))
        .count();
    (deltas, stops)
}

/// THE DEFECT (#1038). A finish_reason chunk with gross `prompt_tokens` and NO
/// cache detail, followed by the usage-only chunk that carries the real
/// `prompt_tokens_details.cached_tokens`. The consumer must end up with the
/// real cache read — not 0 — because the usage-only chunk is where the
/// provider put it.
#[tokio::test]
async fn cache_read_comes_from_usage_only_chunk_not_the_inline_zero() {
    let finish = r#"{"id":"c1","object":"chat.completion.chunk","model":"some-gateway-model","choices":[{"index":0,"delta":{"content":"answer"},"finish_reason":"stop"}],"usage":{"prompt_tokens":90359,"completion_tokens":42}}"#;
    let usage_chunk = r#"{"id":"c1","object":"chat.completion.chunk","model":"some-gateway-model","choices":[],"usage":{"prompt_tokens":90359,"completion_tokens":42,"prompt_tokens_details":{"cached_tokens":86912}}}"#;
    let sse = format!("data: {finish}\n\ndata: {usage_chunk}\n\ndata: [DONE]\n\n");
    let events = events_from_sse(sse).await;

    let usage = usage_as_consumer(&events);
    assert_eq!(
        usage.cache_read_tokens,
        86912,
        "consumer must see the real cache read from the usage-only chunk, got {} (full deltas: {:?})",
        usage.cache_read_tokens,
        events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::MessageDelta { usage, .. } => Some(usage.cache_read_tokens),
                _ => None,
            })
            .collect::<Vec<_>>()
    );
    // Gross prompt is netted against the cache read, per the TokenUsage contract.
    assert_eq!(usage.input_tokens, 90359 - 86912);
    assert_eq!(usage.output_tokens, 42);

    // ...and still exactly one final pair (#1738).
    let (deltas, stops) = count_finals(&events);
    assert_eq!(deltas, 1, "exactly one MessageDelta, got {deltas}");
    assert_eq!(stops, 1, "exactly one MessageStop, got {stops}");
}

/// NO REGRESSION (#1738 fast path). Backends that DO inline cache detail
/// (z-ai/glm-5.3-flash, cx/gpt-6.1-terra, grok-4.7-build-fast) must keep the
/// immediate emit: one final pair, correct cache read, no deferral.
#[tokio::test]
async fn inline_cache_detail_keeps_single_final_pair_and_real_cache_read() {
    let finish = r#"{"id":"c2","object":"chat.completion.chunk","model":"some-gateway-model","choices":[{"index":0,"delta":{"content":"answer"},"finish_reason":"stop"}],"usage":{"prompt_tokens":1000,"completion_tokens":20,"prompt_tokens_details":{"cached_tokens":768}}}"#;
    let sse = format!("data: {finish}\n\ndata: [DONE]\n\n");
    let events = events_from_sse(sse).await;

    let (deltas, stops) = count_finals(&events);
    assert_eq!(deltas, 1, "exactly one MessageDelta, got {deltas}");
    assert_eq!(stops, 1, "exactly one MessageStop, got {stops}");

    let usage = usage_as_consumer(&events);
    assert_eq!(
        usage.cache_read_tokens, 768,
        "inline cache detail must survive"
    );
    assert_eq!(usage.input_tokens, 1000 - 768);
    assert_eq!(usage.output_tokens, 20);
}

/// THE HAZARD. A provider that inlines usage WITHOUT cache detail and then
/// sends `[DONE]` with no usage-only chunk must still finalize with the
/// provider's real output and reasoning counts. A naive "just defer" would
/// fall through to the `[DONE]` estimate and zero both — trading a wrong cache
/// number for lost output, a strictly worse bug.
#[tokio::test]
async fn deferred_usage_without_usage_only_chunk_keeps_output_and_reasoning() {
    let finish = r#"{"id":"c3","object":"chat.completion.chunk","model":"some-gateway-model","choices":[{"index":0,"delta":{"content":"answer"},"finish_reason":"stop"}],"usage":{"prompt_tokens":5000,"completion_tokens":250,"completion_tokens_details":{"reasoning_tokens":100}}}"#;
    let sse = format!("data: {finish}\n\ndata: [DONE]\n\n");
    let events = events_from_sse(sse).await;

    let (deltas, stops) = count_finals(&events);
    assert_eq!(deltas, 1, "exactly one MessageDelta, got {deltas}");
    assert_eq!(stops, 1, "exactly one MessageStop, got {stops}");

    let usage = usage_as_consumer(&events);
    assert_eq!(
        usage.output_tokens, 250,
        "output must be the provider's real count, not the [DONE] zero"
    );
    assert_eq!(
        usage.reasoning_tokens, 100,
        "reasoning must be the provider's real count, not the [DONE] zero"
    );
    // No cache detail was offered anywhere, so 0 is the honest answer here.
    assert_eq!(usage.cache_read_tokens, 0);
    assert_eq!(usage.input_tokens, 5000);
}

/// Same hazard, but the provider closes the connection without `[DONE]` and
/// without a usage-only chunk (dialagram/qwen do this). The deferred counts
/// must still reach the consumer — before the deferral they were emitted at
/// finish_reason, so dropping them would be a regression.
#[tokio::test]
async fn deferred_usage_survives_stream_close_without_done() {
    let finish = r#"{"id":"c4","object":"chat.completion.chunk","model":"some-gateway-model","choices":[{"index":0,"delta":{"content":"answer"},"finish_reason":"stop"}],"usage":{"prompt_tokens":4000,"completion_tokens":175,"completion_tokens_details":{"reasoning_tokens":60}}}"#;
    let sse = format!("data: {finish}\n\n");
    let events = events_from_sse(sse).await;

    let usage = usage_as_consumer(&events);
    assert_eq!(usage.output_tokens, 175, "output must survive stream close");
    assert_eq!(
        usage.reasoning_tokens, 60,
        "reasoning must survive stream close"
    );
    assert_eq!(usage.input_tokens, 4000);

    let (deltas, stops) = count_finals(&events);
    assert_eq!(deltas, 1, "exactly one MessageDelta, got {deltas}");
    assert_eq!(stops, 1, "exactly one MessageStop, got {stops}");
}

/// #694 preserved: a stream that never reported usage anywhere still gets the
/// `[DONE]` fallback with the EndTurn default, so a finished turn terminates
/// instead of retrying forever.
#[tokio::test]
async fn no_usage_anywhere_still_finalizes_on_done() {
    let finish = r#"{"id":"c5","object":"chat.completion.chunk","model":"some-gateway-model","choices":[{"index":0,"delta":{"content":"done"},"finish_reason":"stop"}]}"#;
    let sse = format!("data: {finish}\n\ndata: [DONE]\n\n");
    let events = events_from_sse(sse).await;
    let (deltas, stops) = count_finals(&events);
    assert_eq!(deltas, 1);
    assert_eq!(stops, 1);
    for e in &events {
        if let StreamEvent::MessageDelta { delta, .. } = e {
            assert_eq!(delta.stop_reason, Some(StopReason::EndTurn));
        }
    }
}

/// HAZARD GUARD for the deferral itself (not a #1038 regression — this shape
/// is already correct on `origin/main`). Deferring moves the final delta onto
/// the usage-only chunk, so that chunk's completeness now decides the recorded
/// counts. Some gateways send `prompt_tokens` ALONE there — no
/// `completion_tokens`, no reasoning detail. Reading its zeroes literally
/// would drop output the provider had already reported inline, i.e. the fix
/// would trade a wrong cache number for a lost output count, which is a
/// strictly worse bug. Observed on the live box: 3 of 2358 usage-only
/// finalizes on 2026-11-26 carried `input>0, output=0` while the same call
/// had a real completion count.
#[tokio::test]
async fn usage_only_chunk_without_completion_tokens_keeps_inline_output() {
    let finish = r#"{"id":"c6","object":"chat.completion.chunk","model":"some-gateway-model","choices":[{"index":0,"delta":{"content":"answer"},"finish_reason":"stop"}],"usage":{"prompt_tokens":5000,"completion_tokens":250,"completion_tokens_details":{"reasoning_tokens":100}}}"#;
    let partial = r#"{"id":"c6","object":"chat.completion.chunk","model":"some-gateway-model","choices":[],"usage":{"prompt_tokens":5000}}"#;
    let sse = format!("data: {finish}\n\ndata: {partial}\n\ndata: [DONE]\n\n");
    let events = events_from_sse(sse).await;

    let usage = usage_as_consumer(&events);
    assert_eq!(
        usage.output_tokens, 250,
        "the usage-only chunk omitted completion_tokens; the inline count must survive"
    );
    assert_eq!(
        usage.reasoning_tokens, 100,
        "reasoning must not be zeroed by a usage-only chunk that omits it"
    );
    assert_eq!(usage.input_tokens, 5000);
    // Neither chunk offered cache detail, so 0 is the honest answer here.
    assert_eq!(usage.cache_read_tokens, 0);

    let (deltas, stops) = count_finals(&events);
    assert_eq!(deltas, 1, "exactly one MessageDelta, got {deltas}");
    assert_eq!(stops, 1, "exactly one MessageStop, got {stops}");
}

/// HAZARD GUARD, the mirror image: a usage-only chunk that reports
/// `completion_tokens` but is silent on `prompt_tokens` must not zero the
/// input figure the inline chunk already supplied.
#[tokio::test]
async fn usage_only_chunk_without_prompt_tokens_keeps_inline_input() {
    let finish = r#"{"id":"c8","object":"chat.completion.chunk","model":"some-gateway-model","choices":[{"index":0,"delta":{"content":"answer"},"finish_reason":"stop"}],"usage":{"prompt_tokens":7000,"completion_tokens":90}}"#;
    let partial = r#"{"id":"c8","object":"chat.completion.chunk","model":"some-gateway-model","choices":[],"usage":{"completion_tokens":90,"prompt_tokens_details":{"cached_tokens":6000}}}"#;
    let sse = format!("data: {finish}\n\ndata: {partial}\n\ndata: [DONE]\n\n");
    let events = events_from_sse(sse).await;

    let usage = usage_as_consumer(&events);
    assert_eq!(
        usage.input_tokens, 1000,
        "7000 gross - 6000 cached; the inline prompt count must survive a \
         usage-only chunk that omits prompt_tokens"
    );
    assert_eq!(usage.cache_read_tokens, 6000);
    assert_eq!(usage.output_tokens, 90);
}

/// A finish_reason chunk that reports cache CREATION has told us something
/// about caching, so it must NOT be deferred: the fast path stays exactly as
/// it is today, and the creation figure is recorded rather than replaced by a
/// later chunk's silence.
#[tokio::test]
async fn inline_cache_creation_detail_is_not_deferred() {
    let finish = r#"{"id":"c7","object":"chat.completion.chunk","model":"some-gateway-model","choices":[{"index":0,"delta":{"content":"answer"},"finish_reason":"stop"}],"usage":{"prompt_tokens":900,"completion_tokens":30,"cache_creation_input_tokens":400}}"#;
    let sse = format!("data: {finish}\n\ndata: [DONE]\n\n");
    let events = events_from_sse(sse).await;

    let (deltas, stops) = count_finals(&events);
    assert_eq!(deltas, 1, "exactly one MessageDelta, got {deltas}");
    assert_eq!(stops, 1, "exactly one MessageStop, got {stops}");

    let usage = usage_as_consumer(&events);
    assert_eq!(
        usage.cache_creation_tokens, 400,
        "inline cache-creation detail must be recorded, not deferred away"
    );
    assert_eq!(usage.input_tokens, 900 - 400);
    assert_eq!(usage.output_tokens, 30);
}
