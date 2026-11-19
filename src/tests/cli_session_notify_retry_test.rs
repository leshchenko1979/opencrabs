//! CLI notify transport retry (#199 Leg B.3): a bounded retry on transport-class
//! failure only.
//!
//! The distinguishing property is what is NOT retried: a JSON-RPC `error` is a
//! DECISION — the gateway answered — so re-sending it would be pointless and, on
//! a `refused_in_flight`, actively wrong. Each test below fails loudly if the
//! wrong branch is taken, by arranging the SECOND scripted response to carry a
//! different outcome from the first: a retry that should not have happened
//! returns that second outcome and fails the assertion.

use crate::cli::session_notify::{post_jsonrpc, post_jsonrpc_with};
use std::time::{Duration, Instant};

/// The path the A2A gateway answers on. The `mock()` path and the dialled URL
/// MUST derive from this one string: `mockito::Server::url()` carries no path,
/// so a literal here and a different one on the wire leaves the request matching
/// NO mock, and mockito answers 501 — which `post_jsonrpc` correctly classifies
/// as bodiless (transport) and retries three times. The test then fails three
/// round-trips deep on a 501 detail instead of the scripted outcome (measured:
/// CI run 35549131537, both mockito tests in this file).
const GATEWAY_PATH: &str = "/a2a/v1";

/// The mocked gateway's endpoint — `Server::url()` plus the gateway path.
fn gateway_url(server: &mockito::ServerGuard) -> String {
    format!("{}{GATEWAY_PATH}", server.url())
}

/// Bind an ephemeral port and drop the listener, so the port is free to be
/// dialed but nothing is listening — the classic connection-refused case.
fn dead_endpoint() -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    let port = listener.local_addr().expect("local addr").port();
    drop(listener);
    format!("http://127.0.0.1:{port}{GATEWAY_PATH}")
}

fn rpc_result(outcome: &str) -> String {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "result": { "outcome": outcome, "detail": "scripted" },
    })
    .to_string()
}

fn rpc_error() -> String {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "error": { "code": -32602, "message": "session not found" },
    })
    .to_string()
}

fn notify_body() -> serde_json::Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "session/notify",
        "params": { "session_id": "00000000-0000-0000-0000-000000000000", "message": "x" },
    })
}

#[tokio::test]
async fn a_bodiless_gateway_response_is_retried_and_the_notify_lands() {
    // Attempt 1: a gateway mid-reload answers 502 with no JSON-RPC body — the
    // exact transient class this leg exists for. Attempt 2: a real answer.
    let mut server = mockito::Server::new_async().await;
    let first = server
        .mock("POST", GATEWAY_PATH)
        .with_status(502)
        .with_body("upstream unavailable")
        .expect(1)
        .create_async()
        .await;
    let second = server
        .mock("POST", GATEWAY_PATH)
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(rpc_result("delivered"))
        .expect(1)
        .create_async()
        .await;

    let result = post_jsonrpc(&gateway_url(&server), None, &notify_body())
        .await
        .expect("the retry must land the notify");

    assert_eq!(
        result.get("outcome").and_then(|v| v.as_str()),
        Some("delivered")
    );
    first.assert_async().await;
    second.assert_async().await;
}

#[tokio::test]
async fn a_jsonrpc_error_is_a_decision_and_is_not_retried() {
    // A parsed JSON-RPC error means the gateway ANSWERED. If the code wrongly
    // retried, the second scripted mock would answer with `deferred` and this
    // test would return Ok — so the Err assertion below is the discriminator,
    // not a formality.
    let mut server = mockito::Server::new_async().await;
    let decision = server
        .mock("POST", GATEWAY_PATH)
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(rpc_error())
        .expect(1)
        .create_async()
        .await;
    let _would_only_serve_a_retry = server
        .mock("POST", GATEWAY_PATH)
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(rpc_result("deferred"))
        .create_async()
        .await;

    let err = post_jsonrpc(&gateway_url(&server), None, &notify_body())
        .await
        .expect_err("a JSON-RPC error must surface, not be retried");

    assert!(err.contains("gateway error"), "unexpected detail: {err}");
    decision.assert_async().await;
}

/// A listener that ACCEPTS nothing but HOLDS the socket open: the kernel
/// completes the TCP handshake from the backlog, so a client's `connect`
/// succeeds and then waits forever for a reply. This is the "daemon is up but
/// not serving" case, and it must NOT be classified the same as a refused
/// connection (#857 criterion 3).
fn unresponsive_endpoint() -> (String, std::net::TcpListener) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    let port = listener.local_addr().expect("local addr").port();
    (
        format!("http://127.0.0.1:{port}{GATEWAY_PATH}"),
        listener,
    )
}

#[tokio::test]
async fn an_unreachable_gateway_spends_the_whole_retry_budget() {
    // The transport branch: every attempt fails at `send`, so the loop must run
    // until the budget is gone and hand the caller a classified detail.
    //
    // The budget is passed explicitly so the test measures the SCHEDULE rather
    // than waiting out the production default. With a 1200 ms budget the
    // backoffs are 250 + 250 + 500 and the next sleep (1000) would overrun, so
    // the leg stops on its own well before the ceiling below.
    let started = Instant::now();
    let err = post_jsonrpc_with(
        &dead_endpoint(),
        None,
        &notify_body(),
        Duration::from_millis(1200),
        Duration::from_millis(150),
    )
    .await
    .expect_err("nothing is listening on that port");
    let elapsed = started.elapsed();

    assert!(
        err.contains("cannot reach the A2A gateway"),
        "unexpected detail: {err}"
    );
    assert!(
        !err.contains("1 attempt(s)"),
        "the transport leg must retry, not give up on the first refusal: {err}"
    );
    // The regression guard (#857): the old count-bound budget spent 1.5 s and
    // gave up, while the wall-clock default is 120 s. Passing an explicit
    // budget must actually bound the leg — a leg that ignored it and ran the
    // production default would blow this ceiling by two orders of magnitude.
    assert!(
        elapsed < Duration::from_secs(10),
        "the explicit budget must bound the leg; {elapsed:?} is far past it"
    );
}

#[tokio::test]
async fn an_unbound_port_and_a_wedged_port_classify_differently() {
    // #857 criterion 3, the whole point of the issue. Both legs are transport
    // failures with the same exit code; only the CAUSE distinguishes "the
    // daemon is booting" (retry, wait it out) from "the daemon is up but
    // wedged" (something is wrong with the process). Before this, both were the
    // single string `cannot reach the A2A gateway`.
    let refused = post_jsonrpc_with(
        &dead_endpoint(),
        None,
        &notify_body(),
        Duration::from_millis(300),
        Duration::from_millis(150),
    )
    .await
    .expect_err("an unbound port must fail");

    let (wedged_url, _held) = unresponsive_endpoint();
    let wedged = post_jsonrpc_with(
        &wedged_url,
        None,
        &notify_body(),
        Duration::from_millis(400),
        Duration::from_millis(150),
    )
    .await
    .expect_err("a bound but silent port must fail");

    assert!(
        refused.contains("cause=listener_dead"),
        "an unbound port must read as listener_dead: {refused}"
    );
    assert!(
        wedged.contains("cause=unresponsive"),
        "a bound-but-silent port must read as unresponsive: {wedged}"
    );
    assert_ne!(
        refused, wedged,
        "the two failure classes must not collapse into one string"
    );
}

#[test]
fn the_backoff_grows_is_capped_and_the_budget_outlives_a_boot() {
    use crate::cli::session_notify::{
        BACKOFF_CAP, BACKOFF_START, DEFAULT_RETRY_BUDGET, REQUEST_TIMEOUT, next_backoff,
    };

    assert_eq!(
        next_backoff(1),
        BACKOFF_START,
        "attempt 1 never retries, so its backoff is the base"
    );
    assert_eq!(next_backoff(2), BACKOFF_START, "the first retry waits the base");
    assert!(next_backoff(3) > next_backoff(2), "the schedule must grow");
    // Doubling must never escape the cap, however long the budget lets it run.
    for attempt in 1..200u32 {
        assert!(
            next_backoff(attempt) <= BACKOFF_CAP,
            "attempt {attempt} escaped the cap"
        );
    }
    // The regression guard, pinned to the MEASUREMENT rather than to a round
    // number: #858 measured a 279.6 s boot with the gateway's bind at the very
    // end, so #199's 3-attempt / 1.5 s budget is provably too small to outlive
    // a boot. A budget that only clears 60 s would satisfy the prose and fail
    // the criterion, so the assertion clears the measured window itself.
    assert!(
        DEFAULT_RETRY_BUDGET > Duration::from_millis(279_600),
        "the budget must outlive the measured 279.6 s boot window (#858), \
         not merely exceed a healthy daemon's response time"
    );
    // Bounded, not unbounded: a caller must never be able to wait forever.
    assert!(
        DEFAULT_RETRY_BUDGET <= Duration::from_secs(900),
        "the budget stays bounded — a quarter hour is the ceiling a caller can be asked to wait"
    );
    assert!(
        REQUEST_TIMEOUT < DEFAULT_RETRY_BUDGET,
        "one attempt must fit inside the budget"
    );
}
