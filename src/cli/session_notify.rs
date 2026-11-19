//! `opencrabs session notify` — mechanical session notifications for
//! tooling (#23).
//!
//! The CLI is a SEPARATE PROCESS from the daemon that owns the in-memory
//! route table, so the verb posts over the profile's A2A gateway — the
//! daemon's HTTP surface — where the `session/notify` method hands the
//! message to `deliver_to_session`, the same route the agent's
//! session_notify tool uses (including #19 redirect-with-framing for
//! archived/replaced sessions and #1206 parking).
//!
//! Exit codes are the machine contract for tooling (oc-deploy fan-out, #24):
//!
//! | exit | meaning                                             |
//! |------|-----------------------------------------------------|
//! | 0    | delivered / redirected / parked — the message is safe |
//! | 2    | unknown or dead uuid — nothing sent, nothing created |
//! | 3    | RETIRED (#373): the refusal it reported is unreachable |
//! | 4    | transport/config: a2a disabled, unreachable, bad response |
//!
//! Exit 3 is retained as a constant but no longer reachable through this
//! verb: it reported "target mid-turn and `--interrupt` not set", and #373
//! made the default QUEUE in that situation instead of refusing. The refusal
//! still exists at the API level for callers that pass `interrupt=false`
//! explicitly, but no CLI flag produces it — `--interrupt` is now the legacy
//! ALIAS for the urgent tier (#393), not a way to request the refusal.
//! `--mode now` is likewise retired and now exits 4 with the retirement
//! message.
//!
//! Delivery modes (#393): `turn-end` (default) queues for the target's next
//! tool-loop boundary; `interrupt` is the URGENT tier — the same delivery
//! point, with precedence framing prepended so the target answers the notice
//! in that turn instead of blending it into the plan it is already executing;
//! `quiet` waits for the target to go idle. `interrupt` is never a default,
//! and it is not pre-emption — no boundary exists inside a running tool call.
//!
//! SENDER LABEL (#23, owner amendment "Overridable"): the CLI lane has no
//! sender session, so the recipient's echo shows the carried label —
//! default "CLI tooling", overridable with `--sender`.

use crate::cli::args::OutputFormat;
use crate::config::Config;
use anyhow::Result;
use std::time::Duration;

pub const EXIT_OK: i32 = 0;
pub const EXIT_NO_ROUTE: i32 = 2;
pub const EXIT_REFUSED: i32 = 3;
pub const EXIT_TRANSPORT: i32 = 4;

/// Bounded retry budget for a notify's transport leg (#199).
///
/// A `req.send()` that ERRORS is ambiguous — the request may have reached the
/// daemon and queued the notify (only the RESPONSE was lost), or it may never
/// have arrived. The CLI cannot tell which, so it re-sends the SAME request,
/// caller-minted `notify_id` included, and the gateway's reserve-before-deliver
/// guard turns the ambiguous case into exactly-once delivery: the duplicate
/// reports the original outcome instead of delivering a second copy.
///
/// Only transport-class failures are retried: a `req.send()` Err, or a reply
/// that carries no JSON-RPC body at all (a gateway mid-reload, a proxy's
/// 502/504) — neither is an answer. A reply that unpacks into a JSON-RPC error
/// or result IS an answer, a decision, and re-sending a decision is pointless.
/// `pub(crate)` so the bounds are assertable in tests.
/// How long one request attempt may take before it counts as unresponsive.
pub(crate) const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Wall-clock budget for the whole transport leg (#857).
///
/// #199 bounded the retry by ATTEMPT COUNT (3 attempts, 1.5 s of backoff),
/// which was sized against a healthy daemon. It cannot outlive a BOOT: the
/// gateway's `TcpListener::bind` is the last thing startup does, and a measured
/// 2026-11-15 boot spent **279.6 s** on blocked disk I/O before reaching it
/// (#858). Every notify inside that window burned its three attempts in under
/// two seconds and journalled `cannot reach … (exit 4)` — 69 of them on that
/// date alone, while nothing was wrong with the network.
///
/// So the bound is now a BUDGET in wall-clock time rather than a count, and it
/// is sized against the MEASURED window rather than against a healthy daemon:
/// the 11-15 boot spent 279.6 s before the bind, and the 11-13→11-15 population
/// ranged 167–767 s (median ~320 s). 300 s clears the low end of that range and
/// the single 279.6 s measurement with margin, which is what "outlive a boot
/// window" can honestly mean from the client side.
///
/// It deliberately does NOT try to cover the extreme of that range: a boot
/// running six minutes late is a STARTUP defect (#858 — two synchronous sweeps
/// of 28k status files on a saturated disk), and the remedy for it is to make
/// startup fast, not to make every caller wait ten minutes. The cause in the
/// journal is what tells the operator which of the two they are looking at.
///
/// Override with `OPENCRABS_NOTIFY_RETRY_BUDGET_SECS` (a caller that would
/// rather fail fast sets it to `0`).
pub(crate) const DEFAULT_RETRY_BUDGET: Duration = Duration::from_secs(300);

/// First retry delay; doubles each attempt up to [`BACKOFF_CAP`].
/// `pub(crate)` so the schedule's shape is assertable in tests.
pub(crate) const BACKOFF_START: Duration = Duration::from_millis(250);
pub(crate) const BACKOFF_CAP: Duration = Duration::from_secs(4);

/// Why a transport leg failed — a CLOSED vocabulary (#857 criterion 1).
///
/// Before this, every one of these collapsed into the single string
/// `cannot reach the A2A gateway at <url>`, which cannot distinguish a daemon
/// that is still booting from one that is wedged or gone. The three states have
/// three different remedies, so the journal records which one it was.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TransportCause {
    /// The connection was REFUSED: nothing is listening on the port. Either the
    /// daemon is mid-boot — its bind is the last step of startup — or it is
    /// stopped. A retry within the boot budget is the remedy.
    ListenerDead,
    /// The connection opened but no answer arrived inside the request timeout.
    /// The daemon is up but not serving (wedged event loop, saturated disk), or
    /// a middlebox swallowed the request. Retrying is still right: the socket
    /// is live, so the daemon is not merely absent.
    Unresponsive,
    /// An HTTP status arrived carrying no JSON-RPC body — a gateway mid-reload
    /// or a proxy answering 502/504. Not a decision, so it retries.
    Bodiless(u16),
}

impl TransportCause {
    /// The stable token written into the journal and printed to the caller.
    /// Greppable: `session-notify.journal` can be split by cause instead of
    /// lumping every failure under one phrase.
    pub(crate) fn label(self) -> String {
        match self {
            Self::ListenerDead => "listener_dead".to_string(),
            Self::Unresponsive => "unresponsive".to_string(),
            Self::Bodiless(status) => format!("bodiless_http_{status}"),
        }
    }

    /// What the operator should conclude, in one clause.
    fn meaning(self) -> &'static str {
        match self {
            Self::ListenerDead => "nothing is listening on the port (daemon booting or stopped)",
            Self::Unresponsive => "the port accepted a connection but never answered",
            Self::Bodiless(_) => {
                "the gateway answered with no JSON-RPC body (mid-reload or a proxy)"
            }
        }
    }
}

/// Classify a `reqwest` failure into the closed vocabulary.
///
/// `is_timeout()` is checked FIRST: a connect that times out also reports
/// `is_connect()`, and calling that "refused" would send the operator looking
/// for a dead daemon when the socket was merely slow to open.
pub(crate) fn classify_transport_error(e: &reqwest::Error) -> TransportCause {
    if e.is_timeout() {
        return TransportCause::Unresponsive;
    }
    if e.is_connect() {
        // Walk the source chain for the io::Error that carries the errno. A
        // refused connection is the ONE case we can name precisely, and it is
        // the boot-window case, so it is worth the walk.
        let mut source: Option<&(dyn std::error::Error + 'static)> = std::error::Error::source(e);
        while let Some(cause) = source {
            if let Some(io) = cause.downcast_ref::<std::io::Error>()
                && io.kind() == std::io::ErrorKind::ConnectionRefused
            {
                return TransportCause::ListenerDead;
            }
            source = cause.source();
        }
        // Belt and braces: reqwest's source chain is an implementation detail
        // that has changed shape across versions (hyper → hyper-util), so the
        // errno is ALSO read off the rendered message. ECONNREFUSED is 111 on
        // Linux and 61 on macOS; both renderers are matched, plus the generic
        // libc wording. A false negative here would silently downgrade the
        // boot-window case to `unresponsive`, which is the misclassification
        // this whole issue exists to prevent — so it is worth the redundancy.
        let rendered = e.to_string();
        if rendered.contains("Connection refused")
            || rendered.contains("os error 111")
            || rendered.contains("os error 61")
            || rendered.contains("ECONNREFUSED")
        {
            return TransportCause::ListenerDead;
        }
        // A connect failure that is not a refusal (unreachable, filtered,
        // reset): the socket never came up, but not because nobody was there.
        return TransportCause::Unresponsive;
    }
    // Request/body/decode failures: the peer was reached and the exchange
    // broke. Same remedy as a wedged daemon.
    TransportCause::Unresponsive
}

/// Retry budget, honouring the `OPENCRABS_NOTIFY_RETRY_BUDGET_SECS` override.
pub(crate) fn retry_budget() -> Duration {
    std::env::var("OPENCRABS_NOTIFY_RETRY_BUDGET_SECS")
        .ok()
        .and_then(|raw| raw.trim().parse::<u64>().ok())
        .map(Duration::from_secs)
        .unwrap_or(DEFAULT_RETRY_BUDGET)
}

/// One POST to the A2A gateway, retried on transport-class failures until the
/// wall-clock budget is spent (#857). Shared by the send and status verbs (one
/// copy, no drift): both dial the same endpoint with the same client, timeout
/// and auth, and differ only in the method they call and how they read the
/// outcome back.
///
/// `Ok(result)` = the JSON-RPC result object. `Err(msg)` = the caller's journal
/// detail for a transport failure, carrying the classified cause.
///
/// `pub(crate)` so the retry leg is testable in-process (#199) — the
/// behavioral test drives a real HTTP server through it.
pub(crate) async fn post_jsonrpc(
    url: &str,
    api_key: Option<&str>,
    body: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    post_jsonrpc_with(url, api_key, body, retry_budget(), REQUEST_TIMEOUT).await
}

/// The budget-explicit form: `budget` bounds the whole leg in wall-clock time,
/// `request_timeout` bounds one attempt. Split out so tests can drive both ends
/// of the classification in milliseconds instead of minutes.
pub(crate) async fn post_jsonrpc_with(
    url: &str,
    api_key: Option<&str>,
    body: &serde_json::Value,
    budget: Duration,
    request_timeout: Duration,
) -> Result<serde_json::Value, String> {
    let started = std::time::Instant::now();
    let mut attempts: u32 = 0;
    let mut first_cause: Option<TransportCause> = None;

    // The loop YIELDS the last cause: it is the only value the caller needs
    // after the budget runs out, so it is carried out by `break` rather than
    // through a variable that is reassigned every iteration (clippy
    // `unused_assignments`, -D warnings).
    let cause = loop {
        attempts += 1;
        let mut req = reqwest::Client::new()
            .post(url)
            .timeout(request_timeout)
            .json(body);
        if let Some(key) = api_key {
            req = req.bearer_auth(key);
        }

        // Every non-diverging arm below yields the CAUSE of the failure; the
        // arms that got an answer return from the function instead, so a
        // decision is never retried.
        let cause = match req.send().await {
            Err(e) => classify_transport_error(&e),
            Ok(resp) => {
                let status = resp.status();
                match resp.json::<crate::a2a::types::JsonRpcResponse>().await {
                    Ok(rpc) => {
                        // A JSON-RPC error IS a decision: never retried.
                        if let Some(err) = rpc.error {
                            return Err(format!("gateway error {}: {}", err.code, err.message));
                        }
                        return match rpc.result {
                            Some(result) => Ok(result),
                            None => Err("gateway response carried neither result nor error".into()),
                        };
                    }
                    // A response with no JSON-RPC body is NOT a decision: the
                    // usual cause is a gateway mid-reload or a proxy answering
                    // 502/504 — transient by nature, and the very case this leg
                    // exists for. So it retries like a transport error instead
                    // of failing the notify on the first attempt.
                    Err(_) => TransportCause::Bodiless(status.as_u16()),
                }
            }
        };

        first_cause.get_or_insert(cause);

        // Back off, but only if the budget can absorb both the sleep and a
        // further attempt's timeout — a retry we cannot finish is not a retry.
        let backoff = next_backoff(attempts);
        let spent = started.elapsed();
        if spent + backoff >= budget {
            break cause;
        }
        tracing::debug!(
            attempt = attempts,
            cause = %cause.label(),
            backoff_ms = backoff.as_millis() as u64,
            spent_secs = spent.as_secs_f64(),
            "retrying A2A transport"
        );
        tokio::time::sleep(backoff).await;
    };

    let elapsed = started.elapsed();
    let mut detail = format!(
        "cannot reach the A2A gateway at {url}: cause={} — {}; {attempts} attempt(s) over {:.1}s",
        cause.label(),
        cause.meaning(),
        elapsed.as_secs_f64()
    );
    // A leg that saw MORE THAN ONE cause is the diagnostic gold: a refused
    // connection that later turns into a timeout means the daemon came up
    // mid-budget and then wedged.
    if let Some(first) = first_cause
        && first != cause
    {
        detail.push_str(&format!(" (first attempt: {})", first.label()));
    }
    Err(detail)
}

/// Exponential backoff, capped. `attempt` is 1-based, so the first RETRY (the
/// second attempt) sleeps [`BACKOFF_START`] and each subsequent one doubles.
pub(crate) fn next_backoff(attempt: u32) -> Duration {
    let shift = attempt.saturating_sub(2).min(6);
    (BACKOFF_START * 2u32.pow(shift)).min(BACKOFF_CAP)
}

/// The gateway endpoint and auth for this profile's config — one derivation,
/// used by both verbs. A bind of `0.0.0.0`/`::` is a LISTENING address, not a
/// connectable one: same-box callers always dial loopback.
fn gateway_endpoint(config: &Config) -> (String, Option<String>) {
    let host = match config.a2a.bind.as_str() {
        "0.0.0.0" | "::" | "[::]" => "127.0.0.1",
        other => other,
    };
    (
        format!("http://{}:{}/a2a/v1", host, config.a2a.port),
        config.a2a.api_key.clone(),
    )
}

/// Read `outcome`/`detail` out of a gateway result object, with the shared
/// fallback wording.
fn result_outcome(result: &serde_json::Value) -> (String, String) {
    (
        result
            .get("outcome")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown")
            .to_string(),
        result
            .get("detail")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_string(),
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn run(
    config: &Config,
    id_raw: &str,
    text: Option<&str>,
    title: Option<&str>,
    sender: Option<&str>,
    interrupt: bool,
    mode: Option<&str>,
    quiet_for_secs: Option<u64>,
    max_delay_secs: Option<u64>,
    confirm: bool,
    status: bool,
    format: OutputFormat,
) -> Result<()> {
    // Status mode (fork #146): poll a notification receipt by id instead of
    // sending. Rides the same A2A surface (); the id
    // is passed as the positional arg,  is unused.
    if status {
        let Some(notify_id) = text else {
            return finish(
                format,
                id_raw,
                "usage_error",
                EXIT_TRANSPORT,
                "--status requires the notification id as the <ID> argument",
            );
        };
        return run_status(config, notify_id, id_raw, format).await;
    }

    // Local usage errors: the gateway would reject these with INVALID_PARAMS
    // anyway, but failing here keeps the journal honest about who noticed.
    let target: uuid::Uuid = match id_raw.parse() {
        Ok(id) => id,
        Err(_) => {
            return finish(
                format,
                id_raw,
                "no_route",
                EXIT_NO_ROUTE,
                &format!("'{id_raw}' is not a valid session UUID"),
            );
        }
    };
    let Some(text) = text else {
        return finish(
            format,
            &target.to_string(),
            "usage_error",
            EXIT_TRANSPORT,
            "--text is required for sending (or pass --status to poll a receipt id)",
        );
    };
    if text.trim().is_empty() {
        return finish(
            format,
            &target.to_string(),
            "transport_error",
            EXIT_TRANSPORT,
            "--text must not be empty",
        );
    }
    // Shared sender validation (fork #146): the SAME policy rules the A2A
    // handler and the agent tool run — one copy, no drift. Failing here
    // keeps the journal honest about who noticed.
    if let Some(raw) = sender {
        let label = raw.trim();
        if label.is_empty() {
            return finish(
                format,
                &target.to_string(),
                "transport_error",
                EXIT_TRANSPORT,
                "--sender must not be empty",
            );
        }
        if let Err(e) = crate::brain::agent::service::notify_policy::validate_sender_label(label) {
            return finish(
                format,
                &target.to_string(),
                "transport_error",
                EXIT_TRANSPORT,
                &format!("--sender: {e}"),
            );
        }
    }
    if !config.a2a.enabled {
        return finish(
            format,
            &target.to_string(),
            "transport_error",
            EXIT_TRANSPORT,
            "the [a2a] gateway is disabled in this profile's config — the daemon cannot be reached",
        );
    }

    let (url, api_key) = gateway_endpoint(config);

    // #199: the id is minted ONCE per invocation, before the transport leg, so
    // every retry of THIS notify carries the identical id and the gateway's
    // reserve-before-deliver guard can recognise it as a retry rather than a
    // second notify. Minting inside the retry loop would defeat the whole leg.
    let notify_id = uuid::Uuid::new_v4();
    let mut params = serde_json::json!({
        "session_id": target.to_string(),
        "message": text,
        "notify_id": notify_id.to_string(),
    });
    // Omit `interrupt` unless explicitly true (fork #158): the CLI flag
    // defaults to false, and absent or false selects nothing, so sending it at
    // all would be noise. Since #393 `--interrupt` is the alias for the urgent
    // 'interrupt' tier, so an explicit true UPGRADES a non-quiet resolution
    // instead of being inert; kept because older tooling still passes it.
    if interrupt {
        params["interrupt"] = serde_json::json!(true);
    }
    if let Some(t) = title {
        params["title"] = serde_json::json!(t);
    }
    if let Some(s) = sender {
        params["sender"] = serde_json::json!(s.trim());
    }
    // Delivery policy (fork #146): the full v2 ontology rides to the
    // A2A method, which resolves it through the shared policy module.
    if interrupt || mode.is_some() || quiet_for_secs.is_some() || max_delay_secs.is_some() {
        let mut delivery = serde_json::json!({});
        if let Some(m) = mode {
            delivery["mode"] = serde_json::json!(m);
        }
        if let Some(q) = quiet_for_secs {
            delivery["quiet_for_secs"] = serde_json::json!(q);
        }
        if let Some(m) = max_delay_secs {
            delivery["max_delay_secs"] = serde_json::json!(m);
        }
        params["delivery"] = delivery;
    }
    if confirm {
        params["confirm"] = serde_json::json!(true);
    }
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "session/notify",
        "params": params,
    });

    let (outcome, exit_code, detail) = match post_jsonrpc(&url, api_key.as_deref(), &body).await {
        Err(detail) => ("transport_error".to_string(), EXIT_TRANSPORT, detail),
        Ok(result) => {
            let (outcome, detail) = result_outcome(&result);
            let code = match outcome.as_str() {
                "delivered" | "parked" | "deferred" => EXIT_OK,
                "no_route" => EXIT_NO_ROUTE,
                "refused_in_flight" => EXIT_REFUSED,
                _ => EXIT_TRANSPORT,
            };
            (outcome, code, detail)
        }
    };

    finish(format, &target.to_string(), &outcome, exit_code, &detail)
}

/// Status mode (fork #146): POST the notify id to the A2A
/// `session/notify-status` method and render the receipt lifecycle.
/// Exit codes: 0 = injected (consumed by the machinery) or queued-but-live;
/// 2 = unknown id (not tracked — in-memory receipts die with the process);
/// 4 = transport.
async fn run_status(
    config: &Config,
    notify_id: &str,
    id_raw: &str,
    format: OutputFormat,
) -> Result<()> {
    if notify_id.parse::<uuid::Uuid>().is_err() {
        return finish(
            format,
            id_raw,
            "unknown_id",
            EXIT_NO_ROUTE,
            &format!("'{notify_id}' is not a valid notification UUID"),
        );
    }
    if !config.a2a.enabled {
        return finish(
            format,
            id_raw,
            "transport_error",
            EXIT_TRANSPORT,
            "the [a2a] gateway is disabled in this profile's config — the daemon cannot be reached",
        );
    }
    let (url, api_key) = gateway_endpoint(config);
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "session/notify-status",
        "params": { "notify_id": notify_id },
    });
    let (outcome, exit_code, detail) = match post_jsonrpc(&url, api_key.as_deref(), &body).await {
        Err(detail) => ("transport_error".to_string(), EXIT_TRANSPORT, detail),
        Ok(result) => {
            let (outcome, detail) = result_outcome(&result);
            let code = match outcome.as_str() {
                "injected" | "queued" => EXIT_OK,
                "unknown_id" => EXIT_NO_ROUTE,
                _ => EXIT_TRANSPORT,
            };
            (outcome, code, detail)
        }
    };
    finish(format, id_raw, &outcome, exit_code, &detail)
}

/// Journal + output + exit. One append-only journal line per invocation,
/// written BEFORE the process exits — the journal, not the exit code, is the
/// durable record (tool-logging law).
fn finish(
    format: OutputFormat,
    target: &str,
    outcome: &str,
    exit_code: i32,
    detail: &str,
) -> Result<()> {
    let caller = std::env::var("USER").unwrap_or_else(|_| "unknown".to_string());
    crate::brain::agent::service::notify_journal::record(
        &caller, target, outcome, exit_code, detail,
    );
    match format {
        OutputFormat::Json => {
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "ok": exit_code == EXIT_OK,
                    "target": target,
                    "outcome": outcome,
                    "detail": detail,
                    "exit": exit_code,
                }))?
            );
        }
        _ => {
            if exit_code == EXIT_OK {
                if outcome == "parked" || outcome == "deferred" {
                    println!("⚠️ {outcome}: {detail}");
                } else {
                    println!("✅ {outcome}: {detail}");
                }
            } else {
                eprintln!("❌ {outcome}: {detail} (exit {exit_code})");
            }
        }
    }
    std::process::exit(exit_code)
}
