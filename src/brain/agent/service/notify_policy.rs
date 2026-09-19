//! Notify delivery policy (fork #146, DRY): the ONE home of the v2
//! delivery policy — mode resolution, post-route confirmation, sender-label
//! validation — shared by every consumer of `deliver_to_session`: the agent
//! `session_notify` tool, the A2A `session/notify` method (and through it
//! the CLI verb), and the cron scheduler arm. Consumers map the policy's
//! plain-`String` errors onto their own surfaces (tool → `ToolError`,
//! A2A → JSON-RPC INVALID_PARAMS, CLI → transport exit); the policy itself
//! stays surface-agnostic.

use std::time::Duration;

use serde_json::Value;

/// Resolved v2 delivery policy (fork #50; the `now` mode retired by owner
/// order 2026-09-19, #373).
///
/// There is deliberately no "refuse while mid-turn" mode any more. `now` was
/// strictly dominated: against an IDLE target it delivered exactly as
/// `turn-end` does, and against a BUSY one it refused — which the sender
/// reported as a successful hand-off while the notice was dropped on the
/// floor. Every non-quiet delivery therefore queues for the target's next
/// tool-loop boundary, which is the `turn-end` behaviour.
#[derive(Debug)]
pub enum DeliveryMode {
    /// Queue for the target's next tool-loop boundary (the default).
    TurnEnd,
    /// Defer until the target has been quiet for `quiet_for`; `max_delay`
    /// forces delivery into a busy turn (fork #43/#50).
    Quiet {
        quiet_for: Duration,
        max_delay: Duration,
    },
}

/// Confirmation budget for `confirm: true`: how long the sender watches the
/// receiving machinery for a wake before falling back to an honest
/// "routed, unconfirmed" verdict.
pub(crate) const CONFIRM_CAP: Duration = Duration::from_secs(10);

/// Cap for a sender label: the label rides inside the
/// `[session-notify from=cli:<label>]` header and the receipt-card summary
/// line, so a pathological value must not eat the preview budget.
pub(crate) const SENDER_LABEL_MAX_CHARS: usize = 64;

/// Validate a caller-supplied sender label (fork #146, one copy for the
/// A2A handler and the CLI verb): the label rides inside
/// `[session-notify from=cli:<label>]`, so it may not contain the closing
/// bracket or newlines, and it is capped to keep the receipt-card summary
/// readable. Empty handling (default vs error) stays with the caller —
/// the surfaces differ there by design.
pub(crate) fn validate_sender_label(label: &str) -> Result<(), String> {
    if label.contains(']') || label.contains('\n') || label.contains('\r') {
        return Err("sender label must not contain ']' or newlines".into());
    }
    if label.chars().count() > SENDER_LABEL_MAX_CHARS {
        return Err(format!(
            "sender label must be at most {SENDER_LABEL_MAX_CHARS} chars"
        ));
    }
    Ok(())
}

/// Post-route confirmation (owner-approved state-diag, 2026-09-01): watch
/// the receiving machinery for a bounded budget instead of reporting
/// "delivered" as a mere queue hand-off. The wake path is verifiable
/// in-process — a channel-registered turn probe flips to true the moment
/// the target's loop starts — so the sender gets `woke` (idle target
/// started a turn), `queued_pending_drain` (already mid-turn; the message
/// injects at its next tool-loop boundary), or an honest `delivered`
/// (routed, but no wake observed within `cap`).
pub(crate) async fn confirm_route(
    target: uuid::Uuid,
    cap: Duration,
) -> (&'static str, String, &'static str) {
    use crate::brain::agent::service::session_routes::turn_probe;
    let mid_turn = |t| turn_probe(t).is_some_and(|probe| probe());

    if mid_turn(target) {
        return (
            "queued_pending_drain",
            "Confirmed queued: the target is mid-turn; the message injects at its next \
             tool-loop boundary."
                .into(),
            "mid_turn",
        );
    }
    let deadline = tokio::time::Instant::now() + cap;
    while tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(500)).await;
        if mid_turn(target) {
            return (
                "woke",
                "Confirmed end-to-end: the target was idle and has started a turn on the \
                 message."
                    .into(),
                "wake_confirmed",
            );
        }
    }
    (
        "delivered",
        format!(
            "Routed to session {target}, but no wake was observed within {}s — the \
             target may be parked (channel not claimed since boot) or slow to pick the \
             message up. Re-check via session_search before resending.",
            cap.as_secs()
        ),
        "unconfirmed",
    )
}

/// Resolve the v2 delivery policy (#373: `turn-end` is the default; the `now`
/// mode is retired). The `interrupt` argument is the legacy alias for the
/// retired mode and is accepted-but-inert: an alias whose absent value
/// diverges from its `false` value is not an alias, so `interrupt=false` no
/// longer requests the refusal behaviour. `quiet` defers until the target has
/// been idle for `quiet_for_secs` (starvation cap `max_delay_secs` forces
/// delivery into a busy turn).
///
/// Errors are plain strings; each consumer frames them for its own surface.
pub(crate) fn resolve_mode(
    mode: Option<&str>,
    interrupt: Option<bool>,
    delivery: Option<&Value>,
) -> Result<DeliveryMode, String> {
    fn secs(parent: Option<&Value>, key: &str, default: u64) -> Result<Duration, String> {
        match parent.and_then(|d| d.get(key)) {
            None => Ok(Duration::from_secs(default)),
            Some(v) => {
                let n = v
                    .as_u64()
                    .ok_or_else(|| format!("delivery.{key} must be a non-negative integer"))?;
                Ok(Duration::from_secs(n))
            }
        }
    }
    match mode {
        Some("quiet") => {
            // quiet contradicts interrupt=true by definition: quiet WAITS,
            // turn-end QUEUES. interrupt=false/unset is the natural form.
            if interrupt == Some(true) {
                return Err(
                    "delivery.mode 'quiet' and interrupt=true disagree — quiet defers, \
                     turn-end queues"
                        .into(),
                );
            }
            let quiet_for = secs(delivery, "quiet_for_secs", 60)?;
            let max_delay = secs(delivery, "max_delay_secs", 1800)?;
            Ok(DeliveryMode::Quiet {
                quiet_for,
                max_delay,
            })
        }
        // The default: queue for the target's next tool-loop boundary. This
        // is identical to the retired `now` against an idle target, and
        // strictly better against a busy one, where `now` silently refused.
        None | Some("turn-end") => Ok(DeliveryMode::TurnEnd),
        Some("now") => Err(
            "delivery.mode 'now' is retired: it was identical to 'turn-end' for an idle \
             target and silently refused for a busy one. Deliveries queue for the target's \
             next tool-loop boundary by default — drop the mode, or use 'quiet' to wait for \
             the target to go idle."
                .into(),
        ),
        Some(other) => Err(format!(
            "delivery.mode '{other}' is not available yet — use 'turn-end' or 'quiet'"
        )),
    }
}
