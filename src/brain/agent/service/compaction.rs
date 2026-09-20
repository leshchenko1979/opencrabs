//! Context-budget enforcement (Tier 1 + Tier 2 compaction).
//!
//! Extracted from `tool_loop.rs` (was lines 134-330) as part of the
//! 2026-05-04 Linor-flagged refactor: `tool_loop.rs` was 4,047 lines.
//! Compaction logic is cohesive — one async method that decides between
//! the soft 65 % LLM-summarisation tier, the 90 % hard-truncate floor,
//! and the safety-net truncation when all attempts fail. Lives next to
//! the rest of `impl AgentService` in the same crate; callers still
//! invoke it as `self.enforce_context_budget(...)` exactly as before.
//!
//! Behaviour is unchanged from the pre-extraction version. The
//! exhaustive comments inside the function were preserved verbatim
//! because they document the regression history (pre-0f052250 shape,
//! cancellation race details, the failed async-spawn variant).

use super::builder::AgentService;
use super::compaction_notice::CompactionNotifier;
use super::types::{ProgressCallback, ProgressEvent};
use crate::brain::agent::context::{AgentContext, CompactionScope};
use uuid::Uuid;

/// What `enforce_context_budget` did to the context on this visit.
///
/// Every variant MUST be persisted as a DB marker row. The summarised case is
/// obvious. The truncated case is the one that bit us: dropping the oldest
/// messages without writing a marker leaves `messages_from_last_compaction`
/// pointing at the same old anchor, so the next restart reloads the history
/// that just overflowed and overflows again on the first turn. Two sessions
/// died that way on 2026-05-05 (397% and 372% context, 793k tokens still in
/// the DB, a fresh loop on every user message). A marker with no summary is
/// lossy; no marker at all is unrecoverable.
pub(crate) enum CompactionOutcome {
    /// A summary was produced and swapped into the live context.
    Summarised(String),
    /// Every summariser attempt failed, so the oldest messages were dropped
    /// to fit the window instead. Nothing before this point survives.
    Truncated,
    /// #438 A3: compaction was SUPPRESSED, not performed — a run of
    /// compactions produced no completed tool call between them, so another
    /// attempt would spend a provider call on a context that has not come
    /// down. The marker still has to land: the #226 harm was a session that
    /// looped in SILENCE, and a break the model never sees is that same
    /// silence at a lower bill.
    LoopGuard,
}

impl CompactionOutcome {
    /// The marker row to persist. `trigger` names what prompted this
    /// compaction (empty for the ordinary paths), so each call site keeps its
    /// own wording without copying the marker text five times.
    pub(crate) fn marker(&self, trigger: &str) -> String {
        match self {
            Self::Summarised(summary) => format!(
                "[CONTEXT COMPACTION — The conversation was automatically compacted{trigger}. \
                 Below is a structured summary of everything before this point.]\n\n{summary}"
            ),
            Self::Truncated => format!(
                "[CONTEXT COMPACTION — The conversation was automatically compacted{trigger}. \
                 No summary is available: every summariser attempt failed, so the oldest \
                 messages were dropped to fit the window. Nothing before this point \
                 survives. Ask the user to restate anything you need rather than guessing \
                 at what was lost.]"
            ),
            Self::LoopGuard => format!(
                "[CONTEXT COMPACTION SUSPENDED — The conversation crossed the compaction \
                 threshold{trigger}, but the last {streak} compactions ran with no completed \
                 tool call between them, so compacting again would spend another attempt on a \
                 context that has not come down. Nothing was summarised: this is the harness \
                 breaking a loop, not a fresh start. Anything the guard could free without a \
                 summary has already been freed, so finish the step you are on — or tell the \
                 user plainly what is blocking you — rather than re-summarising the \
                 conversation this turn.]",
                streak = COMPACTION_LOOP_GUARD_STREAK,
            ),
        }
    }
}

/// Per-session compaction TREND (#438 S1).
///
/// One definition, three readers: the compactor prompt reports the streak and
/// the window it started from (A4), the loop guard breaks a run of compactions
/// that produced no work (A3), and the shed decides whether the model already
/// ignored the budget (A5).
///
/// Lives in `AgentService`, NOT in `AgentContext`: the context is rebuilt from
/// the database at the start of every turn, so a counter kept there could never
/// observe two compactions in a row.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct CompactionState {
    /// Consecutive compactions with no completed tool call between them.
    /// Cleared by any committed tool result. A value that keeps climbing is
    /// the signature of the #226 loop: the floor is not coming down.
    pub(crate) compaction_streak: u32,
    /// Assistant turns since the last compaction. Reset on compaction.
    pub(crate) turns_since_compaction: u32,
    /// Window fill level (%) when the last compaction ran — the "before"
    /// number the next prompt measures its own outcome against.
    pub(crate) share_at_last_compaction: f64,
}

/// A summariser running against a snapshot of a session's context while that
/// session keeps taking turns.
///
/// There is deliberately no cancel handle. Cancelling an in-flight summary to
/// make room is what produced the 2026-05-05 loop: the context was truncated,
/// the summary that would have justified the truncation was thrown away, and
/// no marker was written, so every restart replayed the overflow. When the
/// context outgrows the headroom the turn WAITS for this task instead.
pub(crate) struct PendingCompaction {
    handle: tokio::task::JoinHandle<crate::brain::agent::error::Result<String>>,
    /// Length of the message vector when the snapshot was taken. Everything
    /// after this index arrived while the summariser was thinking and is not
    /// described by the summary, so the swap re-appends it.
    snapshot_len: usize,
    /// What the summariser was pointed at (#1649): the whole window, the
    /// delta since the last marker, or the frozen segments. The apply path
    /// dispatches on this — a delta lands as a new frozen segment, a
    /// consolidation replaces the segments, a full window swaps everything.
    scope: CompactionScope,
    /// Fill level at spawn time, reported as the "before" on the receipt so
    /// the number reflects the context that was actually summarised.
    snapshot_usage_pct: f64,
    /// Token count at spawn time (#135) — absolute "before" for the
    /// receipt, same boundary as `snapshot_usage_pct`.
    snapshot_tokens: usize,
    started: std::time::Instant,
}

impl PendingCompaction {
    /// Stop a summariser whose subject no longer exists.
    ///
    /// Not the same move as cancelling to reclaim headroom, which is what
    /// looped two sessions in May: there the summary was still the thing that
    /// would have made the truncation recoverable. Here the context it
    /// describes has already been replaced by a synchronous compaction, so
    /// the result could only overwrite fresher state. Letting it run would
    /// spend a provider call on a summary nothing may apply.
    pub(super) fn abort(self) {
        self.handle.abort();
    }
}

/// Where in a turn a budget check is happening.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum BudgetPhase {
    /// About to answer a user. A summary in flight is waited for here: the
    /// reply would otherwise be composed against a context that is one swap
    /// away from being replaced, and the user is already waiting on a reply
    /// anyway, so the wait costs them nothing extra.
    TurnStart,
    /// Between provider calls inside the tool loop. A summary in flight keeps
    /// running unless the context has climbed to the ceiling.
    MidLoop,
}

/// What a visit found in the session's pending slot.
enum PendingState {
    /// Nothing in flight.
    Empty,
    /// Still thinking, and this visit is not obliged to wait for it.
    StillRunning,
    /// A summary landed and was swapped into the live context.
    Applied(String),
    /// The task finished without a summary. Spawning another would just
    /// repeat the same failing call on every visit, so the caller falls back
    /// to the blocking path and its attempt budget.
    Failed,
}

/// Fill level at which a turn stops running ahead of the summariser and waits
/// for it. Above this there is not enough headroom left to be confident the
/// next provider call fits, and blocking here is exactly what the code did
/// before compaction went to the background.
const BACKPRESSURE_CEILING_PCT: f64 = 80.0;

/// #438 A3: the streak at which a run of compactions stops being compaction and
/// starts being a loop.
///
/// Two, because compaction exists to buy the session room. The first one on a
/// context that never came down is a judgement call; the second, with no
/// completed tool call in between, is the #226 evidence — the floor is not
/// coming down on its own, and neither the in-flight path (`PendingState` has
/// no "abandoned" arm) nor Tier 2 can move a window pinned by one or two
/// outsized messages.
pub(crate) const COMPACTION_LOOP_GUARD_STREAK: u32 = 2;

/// #438 A3 — has this session's compaction run stopped being compaction?
///
/// Pure, over the streak S1 already maintains: no second counter, no clock, no
/// IO. The caller owns the break, so this stays testable without a service.
///
/// The guard, and what it is worth. `N` compactions with no completed tool call
/// between them means the context never came down, and `hard_truncate_to`
/// (`context.rs`) stopping at two messages is why: it cannot drop below two, so
/// a window held open by one or two outsized messages re-triggers compaction on
/// every visit. Compacting again cannot fix that, and the #226 harm was never
/// the compaction itself — it was the session looping in silence for ~7 h. So
/// the caller brakes visibly (`CompactionOutcome::LoopGuard`) instead of
/// summarising again.
///
/// What the brake cannot do matters as much as what it can. Keeping the floor
/// moving needs something the caller can reach that is NOT compaction, and
/// neither `PendingState` nor the safety truncation provides one: unresolved,
/// the turn proceeds unshrunken and the loop stays alive, spending turns rather
/// than provider calls. That is why `N` is 2 and why it must not be raised —
/// the only lever that would raise it (sparing the rare second, working
/// compaction via a `streak >= 1` tail) also admits the pathological
/// first-at-1-then-no-recovery run, and this predicate cannot tell those two
/// apart because it has no outcome to read. Sizing that bound is the escalation
/// half of #438 (a run that will not resolve must reach the owner); until it
/// lands, the guard brakes on the predicate it has.
pub(crate) fn compaction_loop_guard(compaction_streak: u32) -> bool {
    compaction_streak >= COMPACTION_LOOP_GUARD_STREAK
}

/// Last-resort truncation to 80% of the window: the recovery that costs no
/// provider call.
///
/// Two callers share it — the exhausted-summariser safety net, and the #438 A3
/// loop guard, where compacting again would spend a call on a context that has
/// not come down. Returns whether messages were actually dropped, which is what
/// the caller's `truncated` bookkeeping and the persist-a-marker rule hang off.
///
/// Blind by design to the two-message floor in `AgentContext::hard_truncate_to`:
/// a window pinned by two outsized messages cannot be brought down this way, and
/// claiming otherwise would put a marker on a context that never shrank.
fn safety_truncate_to_80(context: &mut AgentContext, max_tokens: usize) -> bool {
    let target = (max_tokens as f64 * 0.80) as usize;
    if context.token_count <= target {
        return false;
    }
    let before_len = context.messages.len();
    context.hard_truncate_to(target);
    context.trim_to_fit(0);
    context.messages.len() < before_len
}

/// Whether this visit has to stop and wait for an in-flight summariser.
///
/// Waiting, never cancelling. Two reasons to stop running ahead of it:
/// a reply is about to be composed and must not be composed against a context
/// one swap away from replacement, or the context has climbed past the point
/// where the next provider call comfortably fits.
pub(crate) fn must_wait_for_compaction(phase: BudgetPhase, usage_pct: f64) -> bool {
    phase == BudgetPhase::TurnStart || usage_pct >= BACKPRESSURE_CEILING_PCT
}

impl AgentService {
    /// Enforce context budget with non-blocking compaction.
    ///
    /// Tier 1 — soft trigger at 65%: spawns an async LLM compaction task in
    /// the background and returns immediately. The agent keeps processing
    /// turns. Subsequent visits to this function check whether the spawned
    /// task has finished and atomically swap the summary in when it has.
    ///
    /// Tier 2 — hard floor at 90%: if context grows past 90% (because growth
    /// outran compaction or compaction failed), emergency truncation cuts
    /// older messages back to 80%. This path NEVER fails. It also cancels
    /// any in-flight async compaction so a stale snapshot summary cannot
    /// later overwrite the now-truncated context.
    ///
    /// NOTE: 65% (~130k of 200k) is chosen because MiniMax (and likely other
    /// providers) start returning `400 Prompt exceeds max length` well below
    /// the documented limit, around 75-80% in practice. 65% gives enough
    /// headroom to summarise without bumping into the actual ceiling.
    ///
    /// Returns `Some(outcome)` on any visit that changed the context, and the
    /// caller MUST persist `outcome.marker(..)`. That includes the truncation
    /// path: a context that shrank without a marker cannot be reloaded.
    pub(super) async fn enforce_context_budget(
        &self,
        session_id: Uuid,
        context: &mut AgentContext,
        model_name: &str,
        cancel_token: Option<&tokio_util::sync::CancellationToken>,
        progress_callback: &Option<ProgressCallback>,
        phase: BudgetPhase,
    ) -> Option<CompactionOutcome> {
        // Set by every branch that drops messages. Read at each of the three
        // exits: dropping messages without telling the DB is what turns a
        // recoverable overflow into a restart loop.
        let mut truncated = false;
        // Restored to the pre-0f052250 shape (the version that ran fine for
        // months before the async-compaction refactor). Logic, in order:
        //
        //   Tier 2 (90% hard floor): truncate to 80% first, then FALL THROUGH
        //     to Tier 1. Doing the truncation first means the compaction
        //     summarizer below sees ≤80% of the window — well within tokenizer
        //     headroom — so it doesn't hit `400 Prompt exceeds max length`
        //     and there's no failed-summarizer-then-truncate cascade.
        //
        //   Tier 1 (65% soft trigger): up to 3 sync compact_context attempts.
        //     If any succeed, summary lands and the marker gets persisted by
        //     the caller. If still over 65% target after success, re-compact
        //     once more with the now-tighter budget.
        //
        //   Safety net: only if all 3 attempts totally failed AND we're still
        //     above 80%, hard-truncate to 80%. This is the LAST RESORT — it
        //     drops messages without a summary marker, but only fires when
        //     the LLM compaction path is entirely unavailable.
        //
        // No async spawn/swap, no cancel-pending-on-90%, no per-call hard-
        // truncate fallback in the error arm — those were the additions that
        // produced the cascade-and-loop behaviour.
        let effective_max = context.max_tokens;
        // The provider's own count when we have one. Measuring against the
        // local estimate is what let a request sail past a 1M limit while
        // being reported as 66% full.
        let effective_tokens = context.effective_token_count();
        let usage_pct = if effective_max > 0 {
            (effective_tokens as f64 / effective_max as f64) * 100.0
        } else {
            100.0
        };

        tracing::trace!(
            "Context budget: {} tokens / {} max = {:.1}%",
            context.token_count,
            effective_max,
            usage_pct,
        );

        // ── Tier 2: 90% hard floor — truncate to 80%, then fall through to Tier 1 compaction ──
        if usage_pct >= 90.0 {
            tracing::warn!(
                "Context at {:.0}% ({} tokens) — hard truncating to 80%",
                usage_pct,
                context.token_count,
            );

            truncated |= safety_truncate_to_80(context, effective_max);

            if let Some(cb) = progress_callback {
                cb(session_id, ProgressEvent::TokenCount(context.token_count));
            }

            tracing::info!(
                "Hard truncation complete: {} messages, {} tokens ({:.0}%)",
                context.messages.len(),
                context.token_count,
                effective_tokens as f64 / effective_max as f64 * 100.0,
            );

            let usage_pct_now = if effective_max > 0 {
                (context.effective_token_count() as f64 / effective_max as f64) * 100.0
            } else {
                100.0
            };
            tracing::debug!(
                "Post-truncation: {:.0}% — falling through to auto-compaction",
                usage_pct_now,
            );
        }

        // ── An in-flight summariser gets first refusal ──
        // Ahead of the 65% gate: a summary that finished on a previous visit
        // is free to apply, and a turn about to answer must not answer from a
        // context queued for replacement.
        let usage_pct = if effective_max > 0 {
            (context.effective_token_count() as f64 / effective_max as f64) * 100.0
        } else {
            100.0
        };
        let pending_state = self
            .resolve_pending_compaction(session_id, context, phase, usage_pct, progress_callback)
            .await;
        if let PendingState::Applied(summary) = pending_state {
            return Some(CompactionOutcome::Summarised(summary));
        }

        // ── Tier 1: soft trigger at 65% - LLM compaction ──
        let usage_pct = if effective_max > 0 {
            (context.effective_token_count() as f64 / effective_max as f64) * 100.0
        } else {
            100.0
        };
        if usage_pct <= 65.0 {
            // Below the compaction trigger. Try to emit the pre-compaction
            // pressure warning if usage is in the 55-64% band (#909), and
            // re-arm the throttle when usage has dropped below the floor.
            self.maybe_emit_pressure_warning(session_id, context, usage_pct);
            // Tier 2 may have already dropped messages to get us here. The
            // marker still has to land even though no summariser ran.
            return truncated.then_some(CompactionOutcome::Truncated);
        }

        // #438 A3 — the loop guard, and it sits HERE on purpose: after the
        // in-flight summariser had its first refusal, after the 65% gate, and
        // before the first compaction attempt. What it suppresses is the
        // provider call that cannot help; Tier 2's truncation above has already
        // run and the free truncation below still runs, so the recoveries that
        // cost nothing are untouched. See `compaction_loop_guard` for why a run
        // this long cannot be fixed by compacting again.
        let loop_guard_streak = self.compaction_state(session_id).compaction_streak;
        if compaction_loop_guard(loop_guard_streak) {
            tracing::warn!(
                "Compaction loop guard (#438): {} consecutive compactions with no completed \
                 tool call — suppressing compaction at {:.0}% rather than spending another \
                 summariser on a context that has not come down",
                loop_guard_streak,
                usage_pct,
            );
            // The floor may still be movable without a summary. When it is not
            // (two outsized messages pin the window), the marker the caller
            // persists is the whole break — deliberate, because a brake the
            // model cannot see is the silence #226 was about.
            if safety_truncate_to_80(context, effective_max) {
                tracing::warn!(
                    "Compaction loop guard (#438): context still over 80% — safety-truncated \
                     without a summary"
                );
            }
            return Some(CompactionOutcome::LoopGuard);
        }

        tracing::warn!(
            "Context at {:.0}% (>65%) — triggering LLM compaction",
            usage_pct
        );
        self.record_provider_feedback(
            session_id,
            "context_compaction",
            model_name,
            Some(&format!("proactive_65pct tokens={}", context.token_count)),
        );

        match pending_state {
            // Work is already under way against this exact conversation. A
            // second summariser would burn a provider call describing a
            // context the first one is about to replace.
            PendingState::StillRunning => {
                return truncated.then_some(CompactionOutcome::Truncated);
            }
            // Nothing in flight and backgrounding is on: start the summariser
            // and let the turn carry on. This is the whole point — the user
            // sees the receipt afterwards instead of a minute of nothing.
            PendingState::Empty if self.background_compaction => {
                self.spawn_background_compaction(session_id, context, model_name, usage_pct);
                return truncated.then_some(CompactionOutcome::Truncated);
            }
            // Backgrounding off, or the background attempt already failed:
            // block the turn, exactly as every compaction did before.
            PendingState::Empty | PendingState::Failed => {}
            PendingState::Applied(_) => unreachable!("returned above"),
        }

        // Signal channels that the next 10-60s will produce zero
        // streaming chunks so their typing-indicator pingers can keep
        // firing. Carries the fill level so channels can render a visible
        // "compacting" line (#29); the percentage is a LEVEL, not progress.
        // The Instant anchors the elapsed duration reported on the
        // CompactionSummary emit below.
        let compact_started = std::time::Instant::now();
        let before_pct = usage_pct;
        // Absolute context size the summariser was invoked on (#135) —
        // the "from" half of the receipt, same boundary as `before_pct`.
        let before_tokens = context.token_count;
        // E2 (#29): the ETA hint rides the event — the duration this
        // session's LAST successful compaction actually took. `None` on the
        // first compaction: no history, no prediction (a static guess is
        // never right — owner 2026-08-29).
        let predicted = self
            .last_compaction_elapsed
            .read()
            .ok()
            .and_then(|map| map.get(&session_id).copied());
        if let Some(cb) = progress_callback {
            cb(
                session_id,
                ProgressEvent::Compacting {
                    usage_pct,
                    predicted,
                },
            );
        }

        // Up to 3 attempts — transient summarizer errors (network blip,
        // tokenizer-edge 400) usually self-resolve on retry.
        let mut summary_result = None;
        const MAX_ATTEMPTS: u32 = 3;
        for attempt in 1..=MAX_ATTEMPTS {
            match self
                .compact_context(
                    session_id,
                    context,
                    model_name,
                    cancel_token,
                    CompactionNotifier::auto_from(session_id, progress_callback.as_ref()),
                )
                .await
            {
                Ok(summary) => {
                    summary_result = Some(summary);
                    break;
                }
                Err(e) => {
                    tracing::error!(
                        "LLM compaction failed (attempt {}/{}): {}",
                        attempt,
                        MAX_ATTEMPTS,
                        e
                    );
                }
            }
        }

        // If still over the 65% target after a successful compaction, run one
        // more pass with the tighter post-summary budget.
        let target_tokens = (effective_max as f64 * 0.65) as usize;
        if context.token_count > target_tokens && summary_result.is_some() {
            tracing::warn!(
                "Still at {} tokens after compaction (target {}), re-compacting",
                context.token_count,
                target_tokens,
            );
            if let Ok(summary) = self
                .compact_context(
                    session_id,
                    context,
                    model_name,
                    cancel_token,
                    CompactionNotifier::auto_from(session_id, progress_callback.as_ref()),
                )
                .await
            {
                summary_result = Some(summary);
            }
        }

        // Last resort: every compaction attempt failed.
        // If context usage is still over 80%, safety-truncate to 80% and mark
        // as truncated so the caller persists a truncation marker and keeps the
        // turn unblocked. If below 80%, proceed gracefully with existing context.
        if summary_result.is_none() {
            let safety_target = (effective_max as f64 * 0.80) as usize;
            if context.token_count > safety_target {
                tracing::warn!(
                    "Compaction exhausted, context at {} tokens (>{:.0}%) — safety truncation to 80%",
                    context.token_count,
                    usage_pct,
                );
                // Drops nothing when `hard_truncate_to`'s two-message floor is
                // what holds the window open — hence the returned flag rather
                // than an assumption that the truncation worked.
                truncated |= safety_truncate_to_80(context, effective_max);
            } else {
                tracing::warn!(
                    "Compaction exhausted, but context is at {} tokens (<=80%) — proceeding with turn uncompacted",
                    context.token_count,
                );
            }
        }

        // Success: surface the outcome to channels (first-ever user-visible
        // compaction receipt, #29) and clear the #909 pressure throttle so
        // the settled ctx footer never wears the ❕ until usage climbs the
        // 55% floor again.
        if let Some(ref summary) = summary_result {
            self.note_compaction_success(
                session_id,
                context,
                summary,
                before_pct,
                before_tokens,
                compact_started.elapsed(),
                progress_callback,
            );
        }

        // Emit the token count the NEXT request will start with.
        if let Some(cb) = progress_callback {
            if let Some(ref summary) = summary_result {
                let marker_tokens = AgentContext::estimate_tokens(summary) + 100;
                let brain_tokens = self
                    .default_system_brain
                    .as_deref()
                    .map(AgentContext::estimate_tokens)
                    .unwrap_or(0);
                cb(
                    session_id,
                    ProgressEvent::TokenCount(marker_tokens + brain_tokens),
                );
            } else {
                cb(session_id, ProgressEvent::TokenCount(context.token_count));
            }
        }

        match summary_result {
            Some(summary) => Some(CompactionOutcome::Summarised(summary)),
            None if truncated => Some(CompactionOutcome::Truncated),
            None => None,
        }
    }

    /// Bookkeeping every successful compaction owes, whichever path produced
    /// it: clear the #909 pressure throttle so the ctx footer drops its
    /// marker, remember how long this took so the next compaction can quote a
    /// real ETA instead of a guess, and hand channels the receipt (#29).
    /// 8 args: the receipt itself carries 6 fields — allow rather than
    /// bundle into a struct the single caller would immediately unpack
    /// (same shape as the #134 continuation helper).
    #[allow(clippy::too_many_arguments)]
    fn note_compaction_success(
        &self,
        session_id: Uuid,
        context: &AgentContext,
        summary: &str,
        before_pct: f64,
        before_tokens: usize,
        elapsed: std::time::Duration,
        progress_callback: &Option<ProgressCallback>,
    ) {
        if let Ok(mut map) = self.session_pressure_warned.write() {
            map.insert(session_id, false);
        }
        if let Ok(mut map) = self.last_compaction_elapsed.write() {
            map.insert(session_id, elapsed);
        }
        // #438 S1: a compaction just completed — raise this session's streak and
        // restart the turns-since clock. The streak is cleared by the next
        // committed tool result, so a value that keeps climbing means the
        // compaction produced no work (the #226 loop).
        self.note_compaction_streak(session_id, before_pct);
        if let Some(cb) = progress_callback {
            let after_pct = if context.max_tokens > 0 {
                (context.token_count as f64 / context.max_tokens as f64) * 100.0
            } else {
                100.0
            };
            cb(
                session_id,
                ProgressEvent::CompactionSummary {
                    summary: summary.to_string(),
                    before_pct,
                    after_pct,
                    before_tokens,
                    after_tokens: context.token_count,
                    elapsed,
                },
            );
        }
    }

    /// The ETA hint for this session, grounded in its last observed
    /// compaction. `None` on the first one: no history, no prediction.
    fn predicted_compaction_elapsed(&self, session_id: Uuid) -> Option<std::time::Duration> {
        self.last_compaction_elapsed
            .read()
            .ok()
            .and_then(|map| map.get(&session_id).copied())
    }

    /// How long one summariser attempt gets before the chain walks on.
    ///
    /// Scaled from what this session's compactions actually cost, tripled, so
    /// a conversation that habitually takes four minutes is not declared hung
    /// at five. A session with no history has nothing to scale from and gets
    /// the floor, which is the per-request bound every HTTP provider already
    /// enforces.
    pub(super) fn compaction_attempt_deadline(&self, session_id: Uuid) -> std::time::Duration {
        self.predicted_compaction_elapsed(session_id)
            .map(|observed| observed * 3)
            .unwrap_or(AgentService::COMPACTION_ATTEMPT_FLOOR)
            .max(AgentService::COMPACTION_ATTEMPT_FLOOR)
    }

    /// Take the session's in-flight summariser out of the map.
    ///
    /// Taken rather than borrowed because awaiting it needs ownership, and
    /// holding a `std::sync::Mutex` across an await would poison every other
    /// session's budget check.
    pub(super) fn take_pending_compaction(&self, session_id: Uuid) -> Option<PendingCompaction> {
        self.pending_compactions
            .lock()
            .ok()
            .and_then(|mut map| map.remove(&session_id))
    }

    /// Decide what to do with an in-flight summariser, and swap it in if it is
    /// this visit's job to.
    async fn resolve_pending_compaction(
        &self,
        session_id: Uuid,
        context: &mut AgentContext,
        phase: BudgetPhase,
        usage_pct: f64,
        progress_callback: &Option<ProgressCallback>,
    ) -> PendingState {
        let Some(pending) = self.take_pending_compaction(session_id) else {
            return PendingState::Empty;
        };

        // Two reasons to stop running ahead of the summariser: the turn is
        // about to answer a user, or the context has climbed past the point
        // where the next provider call comfortably fits. Neither cancels it.
        if !pending.handle.is_finished() && !must_wait_for_compaction(phase, usage_pct) {
            if let Ok(mut map) = self.pending_compactions.lock() {
                map.insert(session_id, pending);
            }
            return PendingState::StillRunning;
        }

        if !pending.handle.is_finished() {
            tracing::warn!(
                "Waiting on background compaction at {:.0}% ({:?} elapsed) — {}",
                usage_pct,
                pending.started.elapsed(),
                if phase == BudgetPhase::TurnStart {
                    "a reply must not be composed against a context queued for replacement"
                } else {
                    "context reached the back-pressure ceiling"
                },
            );
            // The only place backgrounded compaction is announced: here the
            // user really is waiting, and silence reads as a hang.
            if let Some(cb) = progress_callback {
                cb(
                    session_id,
                    ProgressEvent::Compacting {
                        usage_pct,
                        predicted: self.predicted_compaction_elapsed(session_id),
                    },
                );
            }
        }

        let PendingCompaction {
            handle,
            snapshot_len,
            scope,
            snapshot_usage_pct,
            snapshot_tokens,
            started,
        } = pending;

        match handle.await {
            Ok(Ok(summary)) => {
                Self::apply_compaction_summary_after(context, scope, &summary, snapshot_len);
                self.note_compaction_success(
                    session_id,
                    context,
                    &summary,
                    snapshot_usage_pct,
                    snapshot_tokens,
                    started.elapsed(),
                    progress_callback,
                );
                if let Some(cb) = progress_callback {
                    cb(session_id, ProgressEvent::TokenCount(context.token_count));
                }
                PendingState::Applied(summary)
            }
            Ok(Err(e)) => {
                tracing::error!(
                    "Background compaction failed after {:?}: {e}",
                    started.elapsed()
                );
                // A background summariser that dies used to die in the log
                // only; the user saw the context climb and nothing else
                // (#1521).
                if let Some(cb) = progress_callback {
                    cb(
                        session_id,
                        ProgressEvent::SelfHealingAlert {
                            message: format!("Background compaction failed: {e}"),
                        },
                    );
                }
                PendingState::Failed
            }
            Err(e) => {
                tracing::error!("Background compaction task did not finish: {e}");
                PendingState::Failed
            }
        }
    }

    /// Start a summariser against a snapshot and let the turn carry on.
    ///
    /// Nothing about the live context is touched here. The task reads a clone
    /// and the swap happens on a later visit, so the turn keeps streaming
    /// while the summary is written.
    fn spawn_background_compaction(
        &self,
        session_id: Uuid,
        context: &AgentContext,
        model_name: &str,
        usage_pct: f64,
    ) {
        let snapshot_len = context.messages.len();
        // #1649: scope-aware input. A session's first compaction summarises
        // the whole window; every later one summarises only the delta since
        // the last marker (frozen segments never re-enter the input), or the
        // segments themselves when they crowd the window past half.
        let (scope, messages, token_count) = context.compaction_input();
        tracing::info!(
            "Spawning background compaction at {:.0}% ({snapshot_len} messages, {scope:?} scope)",
            usage_pct,
        );

        let provider = self.provider_for_session(session_id);
        let fallbacks = self.fallback_chain_snapshot();
        let max_tokens = context.max_tokens;
        let model = model_name.to_string();
        let max_output = self.request_max_tokens_for_session(session_id);
        let working_dir = self.get_working_directory_for_session(session_id);
        let auto_approve = self.auto_approve_tools;
        let subagents = self.subagent_manager.clone();
        let attempt_deadline = self.compaction_attempt_deadline(session_id);
        // #138 part 2: the inventory stamp lists the UNION (active ∪ seen).
        // Re-injection below stays active-only on purpose.
        let stamp_skills = crate::brain::tools::seen_skills::stamp_skills_for_session(session_id);
        let seen_aux = crate::brain::tools::seen_skills::aux_seen_for_session(session_id);
        let active_tools = self.tool_registry.active_tools(session_id);
        let context_inventory = Self::format_context_inventory(
            max_tokens,
            &stamp_skills,
            &seen_aux,
            &active_tools,
            Some(&self.tool_registry),
        );
        // Its own token: this task answers to session teardown, never to a
        // context that grew impatient.
        let cancel = tokio_util::sync::CancellationToken::new();
        // #438 A4: read the trend BEFORE the task runs, so the prompt reports
        // the compactions that came before this one.
        let compaction_trend = self.compaction_state(session_id);

        let handle = tokio::spawn(async move {
            let summary = Self::compute_compaction_summary(
                provider,
                fallbacks,
                session_id,
                scope,
                messages,
                token_count,
                max_tokens,
                usage_pct,
                model,
                max_output,
                working_dir,
                auto_approve,
                cancel,
                attempt_deadline,
                None,
                context_inventory,
                compaction_trend,
            )
            .await?;
            Ok(Self::decorate_compaction_summary(summary, session_id, subagents).await)
        });

        if let Ok(mut map) = self.pending_compactions.lock() {
            map.insert(
                session_id,
                PendingCompaction {
                    handle,
                    snapshot_len,
                    scope,
                    snapshot_usage_pct: usage_pct,
                    snapshot_tokens: token_count,
                    started: std::time::Instant::now(),
                },
            );
        }
    }

    /// Pre-compaction pressure-warning gate (#909).
    ///
    /// Called from `enforce_context_budget` when usage is at or below 65% (i.e.
    /// compaction is NOT firing this turn). Decides whether to append the
    /// behavioural "persist your state" nudge to the system brain:
    ///
    /// - usage **below** the 55% floor: clear the per-session throttle flag so
    ///   the next entry into the band warns again. No nudge.
    /// - usage **in** the 55-64% band AND not yet emitted this entry: append the
    ///   nudge to `system_brain`, count its tokens, set the throttle flag.
    /// - usage **in** the band but already emitted: no-op (once-per-entry).
    ///
    /// The nudge is transient: `system_brain` is rebuilt from disk every turn
    /// (`live_system_brain_for_session`), so the suffix never persists to the
    /// DB and cannot compound across turns.
    pub(super) fn maybe_emit_pressure_warning(
        &self,
        session_id: Uuid,
        context: &mut AgentContext,
        usage_pct: f64,
    ) {
        use super::nudge::should_emit_pressure_warning;

        // Re-arm the throttle once usage drops below the band floor so the
        // next climb back into the band warns again.
        if usage_pct < super::nudge::PRESSURE_WARN_FLOOR {
            if let Ok(mut map) = self.session_pressure_warned.write() {
                map.insert(session_id, false);
            }
            return;
        }

        let already_emitted = self
            .session_pressure_warned
            .read()
            .ok()
            .and_then(|map| map.get(&session_id).copied())
            .unwrap_or(false);

        if let Some(warning) = should_emit_pressure_warning(usage_pct, already_emitted) {
            if let Some(ref mut brain) = context.system_brain {
                tracing::info!(
                    "Context at {:.0}% - emitting pre-compaction pressure warning",
                    usage_pct
                );
                brain.push_str(warning);
                context.token_count += AgentContext::estimate_tokens(warning);
            }
            // Mark emitted regardless of whether a brain existed, so a
            // missing brain doesn't retry every turn until the band ends.
            if let Ok(mut map) = self.session_pressure_warned.write() {
                map.insert(session_id, true);
            }
        }
        // else: in-band but already emitted -> no-op (once-per-entry).
    }

    /// True while the pre-compaction pressure nudge (#909, 55–64% band) is
    /// active for this session. The settled-flow ctx segment wears the quiet
    /// ❕ marker exactly when the hint is in the prompt (#29); the flag is
    /// cleared on compaction success and re-arms below the 55% floor.
    pub fn pressure_warning_active(&self, session_id: Uuid) -> bool {
        self.session_pressure_warned
            .read()
            .ok()
            .and_then(|map| map.get(&session_id).copied())
            .unwrap_or(false)
    }
}
