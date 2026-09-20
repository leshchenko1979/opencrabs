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
///
/// `interrupt` (fork #393, owner order) is the URGENT tier: the same delivery
/// point as `turn-end`, but the notice carries precedence framing so the
/// target answers it instead of blending it into the plan it is already
/// executing. It is never deferred.
#[derive(Debug)]
pub enum DeliveryMode {
    /// Queue for the target's next tool-loop boundary (the default).
    TurnEnd,
    /// The urgent tier (#393, owner order): delivers at the same boundary as
    /// `TurnEnd` — the target's next tool-loop boundary — and adds precedence
    /// framing, so the target yields its current plan and answers the notice
    /// in that turn. Never deferred: unlike `Quiet` there is no idle wait and
    /// no starvation cap.
    ///
    /// This is NOT pre-emption. No boundary exists inside a running tool
    /// call, so a notice still cannot reach the middle of a long call; true
    /// mid-tool abort would be a separate hard-cancel mechanism.
    Interrupt,
    /// Defer until the target has been quiet for `quiet_for`; `max_delay`
    /// forces delivery into a busy turn (fork #43/#50).
    Quiet {
        quiet_for: Duration,
        max_delay: Duration,
    },
}

/// Precedence frame prepended to an `interrupt`-mode notice (#393, owner
/// order). It lives here, in the shared policy module, so the agent tool and
/// the A2A handler cannot drift apart on what the urgent tier says to the
/// target's model.
pub(crate) const URGENT_FRAME: &str =
    "⚡ URGENT — this notice takes precedence over your current plan.\n\n";

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
/// mode is retired). `turn-end` is the default; `interrupt` (#393) is the
/// URGENT tier — the same delivery point with precedence framing, never
/// deferred. `quiet` defers until the target has been idle for
/// `quiet_for_secs` (starvation cap `max_delay_secs` forces delivery into a
/// busy turn).
///
/// The `interrupt` argument is the LEGACY ALIAS for the urgent tier, read in
/// exactly two places: it contradicts `quiet` (which WAITS) and it UPGRADES a
/// non-quiet resolution to `Interrupt`. Absence and `false` select nothing —
/// an alias whose `true` maps to the ordinary tier is not an alias, and a
/// boolean whose absent value diverges from its `false` value cannot express
/// "not urgent".
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
        // The urgent tier (#393, owner order): the same delivery point as the
        // default — the target's next tool-loop boundary — but the notice
        // carries precedence framing, so the target answers it instead of
        // blending it into the plan it is already executing. Never deferred.
        Some("interrupt") => Ok(DeliveryMode::Interrupt),
        // The default: queue for the target's next tool-loop boundary. This
        // is identical to the retired `now` against an idle target, and
        // strictly better against a busy one, where `now` silently refused.
        None | Some("turn-end") => {
            // Legacy alias upgrade (#393): `interrupt=true` was the pre-#373
            // spelling of the urgent tier, and it must keep resolving onto
            // the mode so the A2A param, the tool property and the CLI flag
            // all mean the same thing. `false`/absent resolves to the
            // ordinary tier — the boolean requests the tier, it never
            // overrides an explicit non-quiet mode.
            if interrupt == Some(true) {
                Ok(DeliveryMode::Interrupt)
            } else {
                Ok(DeliveryMode::TurnEnd)
            }
        }
        Some("now") => Err(
            "delivery.mode 'now' is retired: it was identical to 'turn-end' for an idle \
             target and silently refused for a busy one. Deliveries queue for the target's \
             next tool-loop boundary by default — drop the mode, or use 'quiet' to wait for \
             the target to go idle."
                .into(),
        ),
        Some(other) => Err(format!(
            "delivery.mode '{other}' is not available yet — use 'turn-end', \
             'interrupt' or 'quiet'"
        )),
    }
}
