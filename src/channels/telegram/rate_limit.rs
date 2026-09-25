//! Back off when Telegram says to (#814, #556).
//!
//! The plan card was writing often enough to trip flood control, and the error
//! was logged and dropped. Nothing recorded that the API had asked for a pause,
//! so the next refresh wrote again immediately and each rejected attempt kept
//! the window alive. Observed as roughly twenty create attempts across forty
//! seconds while the countdown ticked 40s down to 3s without ever elapsing.
//!
//! The card is chrome. Skipping an update is strictly better than being
//! throttled into a loop that also spams duplicates into the chat.
//!
//! #556 — obedience. Telegram's advertised window is a fact, and the cooldown
//! deadline is never shortened to fit an inline wait: the deadline is
//! `retry_after + RETRY_MARGIN`, full stop. What is bounded is the *sleep*, not
//! the deadline — a window longer than [`MAX_INLINE_RATE_LIMIT_WAIT`] is not
//! slept-and-sent, it is deferred, and the armed deadline carries the retry.
//! Sending early is never an option; deferring is strictly more obedient than
//! truncating the wait and retrying inside the ban that is still running.

use std::sync::RwLock;
use std::time::{Duration, Instant};

/// Process-wide 429 cooldown deadline.
///
/// When any Telegram request receives HTTP 429, the deadline is set to
/// `now + wait + margin`. All concurrent requests across all chats and topics
/// pause until the cooldown expires, preventing penalty escalation loops.
static GLOBAL_COOLDOWN: RwLock<Option<Instant>> = RwLock::new(None);

/// Seconds Telegram asked us to wait, from an error string.
///
/// teloxide surfaces flood control as text containing `Retry after N`, so this
/// matches on that rather than a typed variant, which keeps it working across
/// the several error shapes the same condition arrives in.
///
/// Returns `None` for anything else, so ordinary failures (a deleted message,
/// bad markup) are not mistaken for throttling and do not suppress writes.
pub(crate) fn parse_retry_after(error: &str) -> Option<Duration> {
    let lower = error.to_lowercase();
    let idx = lower.find("retry after")?;
    let rest = &lower[idx + "retry after".len()..];
    let digits: String = rest
        .chars()
        .skip_while(|c| !c.is_ascii_digit())
        .take_while(char::is_ascii_digit)
        .collect();
    if digits.is_empty() {
        return None;
    }
    let secs: u64 = digits.parse().ok()?;
    // A pause is only meaningful if it is positive; "Retry after 0" is not a
    // reason to stop writing.
    (secs > 0).then(|| Duration::from_secs(secs))
}

/// Extra margin added to the window Telegram gives.
///
/// Resuming on the exact second risks landing inside the same window and
/// renewing the penalty, which is the loop this exists to break.
pub(crate) const RETRY_MARGIN: Duration = Duration::from_secs(2);

/// Longest 429 wait any send path may sleep inline (#1064, #556).
///
/// This is an INLINE BOUND, not a cap on the cooldown deadline. It is the
/// longest a single call sleeps before returning, and the threshold above which
/// a window is deferred rather than slept-and-sent. The deadline itself (see
/// [`record_global_429`]) is never shortened to fit it — shortening the
/// deadline is what let every chat resume 13s inside a live 45s ban.
///
/// 60s because every window measured over five days (1 976 of them, 248 of
/// them on 2026-09-24) is 31–45s: a 60s bound sleeps every real window in full
/// and leaves the deferral branch for windows that are, by construction,
/// multi-minute bans. Telegram can hand out such windows (8288s observed on a
/// flooded chat, #1064) and sleeping one inline parked the whole agent turn for
/// hours, so it is not slept at all — the armed deadline carries the retry and
/// the caller is told to defer.
///
/// Every send path shares this policy through [`wait_out`].
pub(crate) const MAX_INLINE_RATE_LIMIT_WAIT: Duration = Duration::from_secs(60);

/// Whether a 429 window is too long to sleep inline.
///
/// The single predicate the whole policy keys on: `false` means sleep the
/// window in full and retry; `true` means sleep nothing and defer to the armed
/// cooldown.
pub(crate) fn exceeds_inline_bound(window: Duration) -> bool {
    window > MAX_INLINE_RATE_LIMIT_WAIT
}

/// Record a 429 cooldown globally across the entire process.
///
/// Extends the active cooldown deadline monotonically to
/// `max(existing, now + retry_after + margin)`.
///
/// The deadline is deliberately NOT clamped (#556). A 45s ban must set a 47s
/// deadline: a deadline shorter than the window Telegram asked for lets every
/// chat resume inside a ban that is still running, which is what produced the
/// retry loop this module exists to break. Obedience is a property of the
/// deadline; the inline sleep is bounded separately, in [`wait_out`].
///
/// `chat` names the chat that was throttled, or `None` where the call path
/// genuinely has no chat (it renders as `-`, the convention this crate already
/// uses for unknown fields). Without it a 429 cannot be attributed to a chat
/// after the fact.
pub(crate) fn record_global_429(retry_after: Duration, chat: Option<i64>) {
    let total_wait = retry_after + RETRY_MARGIN;
    let now = super::governor::gate_now();
    let new_deadline = now + total_wait;

    // #580: the event-time profile is read BEFORE the cooldown lock is taken,
    // so the two locks are never held together. It is what makes a 429
    // attributable to a RATE rather than to a bucket ceiling — the send log
    // cannot see `sendChatAction`, and no other instrument reports a sliding
    // window. Instrumentation only: this reads, it never gates.
    let profile = super::governor::recent_profile(chat);

    let mut lock = GLOBAL_COOLDOWN.write().unwrap_or_else(|e| e.into_inner());
    let active_deadline = match *lock {
        Some(existing) if existing > new_deadline => existing,
        _ => {
            *lock = Some(new_deadline);
            new_deadline
        }
    };

    let chat = chat.map_or_else(|| "-".to_string(), |c| c.to_string());
    if exceeds_inline_bound(retry_after) {
        tracing::warn!(
            "Telegram: Global 429 cooldown activated: {}s window exceeds the {}s inline bound \
             — deadline {}s out, chat={chat} likely flood-banned; inline waits will defer (#556) {profile}",
            retry_after.as_secs(),
            MAX_INLINE_RATE_LIMIT_WAIT.as_secs(),
            total_wait.as_secs()
        );
    } else {
        tracing::warn!(
            "Telegram: Global 429 cooldown activated: cooling down for {}s (deadline {:?}) chat={chat} {profile}",
            total_wait.as_secs(),
            active_deadline
        );
    }
}

/// Check if a global 429 cooldown is currently active.
///
/// Fast-path non-blocking check used by drop-eligible operations (typing indicators,
/// intermediate clock/status flow edits) to drop immediately without holding.
pub(crate) fn is_global_cooldown_active() -> bool {
    let now = super::governor::gate_now();
    let lock = GLOBAL_COOLDOWN.read().unwrap_or_else(|e| e.into_inner());
    match *lock {
        Some(deadline) => deadline > now,
        None => false,
    }
}

/// Await any active global 429 cooldown, sleeping at most `bound`.
///
/// Returns `true` when no cooldown is active after the sleep — the caller may
/// proceed — and `false` when the deadline is still in the future, meaning the
/// caller must DEFER and must not send through it.
///
/// The bound exists because the deadline is un-clamped (#556): without it, an
/// 8288s window would be slept here in full, which is the #1064 regression by
/// another door. The bound limits the SLEEP, never the deadline.
pub(crate) async fn wait_global_cooldown(bound: Duration) -> bool {
    let now = super::governor::gate_now();
    let remaining = {
        let lock = GLOBAL_COOLDOWN.read().unwrap_or_else(|e| e.into_inner());
        match *lock {
            Some(deadline) if deadline > now => Some(deadline.duration_since(now)),
            _ => None,
        }
    };

    let Some(remaining) = remaining else {
        return true;
    };

    let wait = remaining.min(bound);
    // The real sleep IS the clock here: this function is awaited by tests that
    // read wall time back (`global_pacer_burst_smoothing_and_cooldown` measures
    // the window it waited). It must NOT also advance the virtual offset — the
    // two together moved the clock by `2 x wait`, so a deadline still seconds
    // out read as cleared and a bounded wait reported success against a window
    // it never waited out (#556).
    tokio::time::sleep(wait).await;
    !is_global_cooldown_active()
}

/// Reset the global 429 cooldown (used for testing).
#[cfg(test)]
pub(crate) fn reset_global_cooldown() {
    let mut lock = GLOBAL_COOLDOWN.write().unwrap_or_else(|e| e.into_inner());
    *lock = None;
}

/// What a [`wait_out`] call did with the window Telegram asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WaitOutcome {
    /// The window was slept in full; the caller may retry now.
    Slept,
    /// The window is longer than the inline bound: nothing was slept and the
    /// caller must NOT retry — the armed cooldown carries it.
    Deferred,
}

/// Record the 429 and either sleep the window in full or defer it.
///
/// One standardized warn+sleep site (#1085 P1b R1 — this block was copy-pasted
/// at four send sites with drifting wording). `what` names the send ("HTML
/// send", "edit", ...); `extra` carries per-site context such as an attempt
/// counter or message id into the line; `chat` is the throttled chat, or `None`
/// where the call path genuinely has no chat (renders as `-`).
///
/// #556: the window is slept in FULL when it is within the inline bound —
/// Telegram's advertised window is respected, not shortened to fit a cap. A
/// window over the bound is not slept at all and the caller is told to defer.
/// The old `capped` wording is gone with the truncated wait it described: a
/// window over the bound means the chat is flood-banned, and the correct
/// response is to stop sending, not to sleep 30s and try again inside the ban.
pub(crate) async fn wait_out(
    what: &str,
    window: Duration,
    extra: &str,
    chat: Option<i64>,
) -> WaitOutcome {
    // #580: a 429 with a known chat arms the per-chat step-2 pause as well as
    // the process-wide deadline. `note_429_pause` records the global itself,
    // so the known-chat arm goes through it; a 429 with no chat in scope keeps
    // the bare global recording. The two are independent: the process-wide
    // deadline de-synchronises every chat, while the per-chat pause stops the
    // OFFENDING chat's bucket banking quota it would spend the instant that
    // deadline expires (refill is frozen inside a declared window).
    match chat {
        Some(id) => super::governor::note_429_pause(teloxide::types::ChatId(id), window),
        None => record_global_429(window, None),
    }

    let chat = chat.map_or_else(|| "-".to_string(), |c| c.to_string());
    if exceeds_inline_bound(window) {
        tracing::warn!(
            "Telegram: {what} rate-limited{extra}: {}s window exceeds the {}s inline bound \
             — NOT sleeping, deferring to the next attempt after the cooldown expires, \
             chat={chat} (#556)",
            window.as_secs(),
            MAX_INLINE_RATE_LIMIT_WAIT.as_secs()
        );
        return WaitOutcome::Deferred;
    }

    tracing::warn!(
        "Telegram: {what} rate-limited{extra} — waiting {}s chat={chat}",
        window.as_secs()
    );

    // Both clocks are needed HERE: every caller of this function in tests runs
    // on a paused clock, which the virtual offset does not touch — the advance
    // moves `gate_now`, the sleep moves tokio's clock, and the callers read
    // both. (`wait_global_cooldown` is the opposite case: its callers read wall
    // time back, so it must sleep and must NOT advance — see its note.)
    #[cfg(test)]
    super::governor::test_support::advance(window.as_millis() as u64);

    tokio::time::sleep(window).await;
    WaitOutcome::Slept
}
