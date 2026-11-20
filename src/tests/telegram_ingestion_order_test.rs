//! #909: the turn OPENER must follow send order, not ingestion completion.
//!
//! A burst of voice notes ingests concurrently, and STT latency varies wildly:
//! in the measured burst note `102140` (sent first) took 32.8 s while note
//! `102142` (sent 1 s later) took 2.4 s, so `102142` reached the opener first
//! and opened the turn ahead of the note it was answering. Telegram message ids
//! are monotonic per chat, which makes the opener decidable: a completed
//! message may open a turn only when no SMALLER id in its lane is still
//! ingesting. #893's ordering starts at the queue — after the turn is already
//! open — so it structurally cannot fix this.
//!
//! These tests pin the registry contract, including its bound: a genuinely
//! stuck ingestion must not hold the turn forever.
//!
//! Every blocking assertion is wrapped in `tokio::time::timeout` so a
//! regression FAILS here instead of hanging the suite (the gate has a 480 s
//! stall detector).

use std::sync::Arc;
use std::time::Duration;

use crate::channels::telegram::TelegramState;

/// The chat from the #909 receipt; ids are monotonic within it.
const CHAT: i64 = -1003936827469;

/// Short wait used where the test expects an immediate release.
const SHORT: Duration = Duration::from_millis(200);

#[tokio::test]
async fn earlier_in_flight_blocks_later() {
    let state = Arc::new(TelegramState::new());

    // 100 was sent first and is still ingesting; 101 finished early.
    let first = state.begin_ingestion(CHAT, None, 100);
    let _second = state.begin_ingestion(CHAT, None, 101);

    // 101 must NOT proceed while 100 is in flight.
    let early = tokio::time::timeout(
        Duration::from_millis(50),
        state.await_ingestion_order(CHAT, None, 101, SHORT),
    )
    .await;
    assert!(
        early.is_err(),
        "101 opened the turn while the earlier message 100 was still ingesting"
    );

    // 100 finishes ingesting → 101 is released.
    drop(first);
    let proceeded = tokio::time::timeout(
        Duration::from_secs(2),
        state.await_ingestion_order(CHAT, None, 101, SHORT),
    )
    .await
    .expect("101 must be released once 100 deregisters, not wait for the deadline");
    assert!(
        proceeded,
        "101 must proceed immediately once no smaller id is in flight"
    );
}

#[tokio::test]
async fn no_smaller_in_flight_proceeds_immediately() {
    let state = Arc::new(TelegramState::new());

    // Only 101 is ingesting — and it is 101's OWN registration, which must
    // never block it: the gate compares strictly smaller ids.
    let _only = state.begin_ingestion(CHAT, None, 101);

    let started = std::time::Instant::now();
    let proceeded = tokio::time::timeout(
        Duration::from_secs(2),
        state.await_ingestion_order(CHAT, None, 101, SHORT),
    )
    .await
    .expect("must not block when nothing smaller is in flight");
    assert!(proceeded, "a message's own registration must not block it");
    assert!(
        started.elapsed() < Duration::from_millis(50),
        "unexpected wait of {:?} with no smaller id in flight",
        started.elapsed()
    );
}

#[tokio::test]
async fn deadline_expires_when_earlier_never_completes() {
    let state = Arc::new(TelegramState::new());

    // 100 never finishes ingesting — the guard is held for the whole test.
    let _stuck = state.begin_ingestion(CHAT, None, 100);
    let _later = state.begin_ingestion(CHAT, None, 101);

    let started = std::time::Instant::now();
    let proceeded = tokio::time::timeout(
        Duration::from_secs(2),
        state.await_ingestion_order(CHAT, None, 101, Duration::from_millis(100)),
    )
    .await
    .expect("the deadline must bound the wait — a stuck ingestion cannot hang the turn");
    assert!(
        !proceeded,
        "deadline expiry must report `false` so the caller warns and proceeds"
    );
    assert!(
        started.elapsed() >= Duration::from_millis(100),
        "returned after {:?}, before the 100 ms deadline",
        started.elapsed()
    );
}

#[tokio::test]
async fn different_topic_does_not_block() {
    let state = Arc::new(TelegramState::new());

    // A forum topic is its own lane: 100 in topic 1 says nothing about
    // ordering in topic 2, whose ids are an independent sequence.
    let _topic_one = state.begin_ingestion(CHAT, Some(1), 100);

    let proceeded = tokio::time::timeout(
        Duration::from_secs(2),
        state.await_ingestion_order(CHAT, Some(2), 101, SHORT),
    )
    .await
    .expect("another topic's in-flight message must not gate this lane");
    assert!(
        proceeded,
        "101 in topic 2 must proceed while topic 1 ingests 100"
    );
}
