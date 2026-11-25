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
use super::compaction_event::{
    CompactionContextSnapshot, CompactionEventFields, CompactionEventStage, CompactionOutcomeKind,
    CompactionPath, CompactionRunContext, CompactionTrigger, emit_compaction_event, snapshot_pair,
};
use super::compaction_notice::CompactionNotifier;
use super::prompt_file::CompactionPrompts;
use super::types::{ProgressCallback, ProgressEvent, ToolSize};
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
    /// A summary was produced and swapped into the live context. The String is
    /// the exact marker text the apply step welded into the context — banner
    /// plus summary — so the persisted row carries the same bytes the live
    /// context does (#1928).
    Summarised(String),
    /// #994: the summariser's document was CUT at its output limit
    /// (`finish_reason=length`), so there is no summary — but this is NOT the
    /// `Truncated` class below, and conflating the two would be a lie in both
    /// directions. The summariser did not fail (it answered, and its answer was
    /// cut short), and the marker text says so; `Truncated`'s text claims every
    /// attempt failed, which is false here.
    ///
    /// The String is the exact marker text the apply welded in — the
    /// truncated-summary notice, scope-worded (#994) — so the persisted row
    /// carries the same bytes the live context does (#1928).
    TruncatedSummary(String),
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
    /// #833: compaction was SUPPRESSED because the last
    /// [`COMPACTION_NON_REDUCING_BRAKE_STREAK`] compactions each left the
    /// context at least as full as they found it.
    ///
    /// A distinct arm from `LoopGuard` because the evidence is different and so
    /// is the advice: the loop guard says "you have made no progress, stop
    /// re-summarising", this one says "compaction itself is adding to the
    /// window", which is the only case where doing LESS is the fix.
    NonReducingBrake,
}

impl CompactionOutcome {
    /// The marker row to persist. `trigger` names what prompted this
    /// compaction (empty for the ordinary paths), so each call site keeps its
    /// own wording without copying the marker text five times.
    pub(crate) fn marker(&self, trigger: &str) -> String {
        match self {
            // #1928: the payload IS the applied marker (banner + summary).
            // Persist those bytes; rebuilding the banner here is exactly how
            // the in-memory marker and the DB row drifted apart — a delta
            // segment's sentinel never reached the DB, so the loader counted
            // every segment as a fresh boundary and kept only the newest.
            // The trigger decoration rides the full-window banner's
            // "compacted." sentence; the delta and consolidation banners carry
            // no such sentence and are persisted unchanged.
            Self::Summarised(applied_marker) if trigger.is_empty() => applied_marker.clone(),
            Self::Summarised(applied_marker) => applied_marker.replacen(
                "automatically compacted.",
                &format!("automatically compacted{trigger}."),
                1,
            ),
            // #994: the payload IS the notice the apply welded in — the same
            // rule as `Summarised` (#1928), because a row rebuilt here could
            // drift from the bytes the live context carries. The full-window
            // notice opens with the same "automatically compacted." sentence,
            // so the trigger decoration applies to it and to nothing else.
            Self::TruncatedSummary(applied_marker) if trigger.is_empty() => applied_marker.clone(),
            Self::TruncatedSummary(applied_marker) => applied_marker.replacen(
                "automatically compacted.",
                &format!("automatically compacted{trigger}."),
                1,
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
            Self::NonReducingBrake => format!(
                "[CONTEXT COMPACTION SUSPENDED — The conversation crossed the compaction \
                 threshold{trigger}, but the last {streak} compactions each left the context at \
                 least as full as they found them: summarising was adding more than it removed. \
                 Nothing was summarised this time, on purpose — the harness is not repeating a \
                 move that measurably does not work. Finish or abandon the step you are on, and \
                 tell the user plainly what is blocking you, rather than re-summarising the \
                 conversation.]",
                streak = COMPACTION_NON_REDUCING_BRAKE_STREAK,
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
    /// Consecutive successful compactions that left the fill at least as high
    /// as they found it (#833).
    ///
    /// Deliberately separate from `compaction_streak`: that one counts
    /// compactions with no completed tool call between them, which a busy lane
    /// never accumulates — it fired once in 396 compactions. This one counts
    /// compactions that measurably bought nothing, which is the busy-lane loop
    /// and the reason the existing brake could not see it.
    pub(crate) non_reducing_streak: u32,
    /// Tokens the last continuation document cost this session. 0 until the
    /// first compaction lands; the scope decision uses it as its prior, so a
    /// session that writes 6k-token documents is judged against 6k, not
    /// against the measured median.
    pub(crate) last_summary_tokens: usize,
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
    handle: tokio::task::JoinHandle<
        crate::brain::agent::error::Result<super::context::CompactionDocument>,
    >,
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
    /// Token size of the input `scope` selected: the delta since the last
    /// marker, the joined segments, or the whole window (#833 follow-up).
    ///
    /// NOT the same quantity as `snapshot_tokens`, and the difference is the
    /// whole point of carrying both: `snapshot_tokens` is the window the
    /// receipt reports as "before", this is what the summariser actually read.
    /// On a `DeltaSinceMarker` pass the two differ by the frozen segments —
    /// which is exactly why the fill can climb while a delta runs.
    input_tokens: usize,
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
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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

impl BudgetPhase {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::TurnStart => "turn_start",
            Self::MidLoop => "mid_loop",
        }
    }
}

/// What a visit found in the session's pending slot.
pub(crate) enum PendingState {
    /// Nothing in flight.
    Empty,
    /// Still thinking, and this visit is not obliged to wait for it.
    StillRunning,
    /// A compaction landed and was swapped into the live context. Carries the
    /// OUTCOME (#994), not just the marker text: a background summariser can
    /// finish CUT, and the visit that applies it must hand the caller
    /// `TruncatedSummary` rather than `Summarised` — the class decides both the
    /// persisted variant and what the card says. The payload is the exact
    /// marker text the apply welded in (#1928).
    Applied(CompactionOutcome),
    /// The task finished without a summary. Spawning another would just
    /// repeat the same failing call on every visit, so the caller falls back
    /// to the blocking path and its attempt budget.
    Failed,
}

/// What the 65% gate owes a session that has just crossed it (#1686).
///
/// True only when this visit starts or performs a summariser. The gate is
/// re-entered on every tool iteration while a background summariser is in
/// flight, and the announcement used to sit above that dispatch, so session
/// `a58b8714` logged 128 gate WARNs. Across 2026-09-22 and 23 the ratio was
/// 241 gate hits against 27 compactions that actually applied: roughly nine
/// announcements, spawns and ledger rows per summariser.
/// Each repeat paid three times: a WARN line, a `tokio::spawn`, and a
/// feedback-ledger row asserting a compaction had started.
///
/// A `Failed` visit does announce. It falls back to the blocking path, which
/// is a second summariser rather than a repeat of the first.
pub(crate) fn gate_announces_compaction(state: &PendingState) -> bool {
    matches!(state, PendingState::Empty | PendingState::Failed)
}

/// The body of the gate WARN, extracted so tests can assert what actually
/// prints. The value is shown with one decimal and the trigger is stated as a
/// label: the integer format once turned a 65.4% fill into
/// "Context at 65% (>65%)", a sentence contradicting its own claim (#1733).
pub(crate) fn gate_warn_message(usage_pct: f64) -> String {
    format!(
        "Context at {:.1}% (threshold 65%) — triggering LLM compaction",
        usage_pct
    )
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

/// #833: the run of non-reducing compactions at which the harness stops
/// spending summarisers.
///
/// Two, for the same reason as the loop guard: the first non-reducing
/// compaction is a judgement call on a window that may be pinned by one
/// outsized message, and the second one with no improvement in between is a
/// measurement — the scope is wrong for this session, and a third attempt with
/// the same inputs is a prediction, not an experiment. With the scope decision
/// escalating on the first non-reducer, reaching two means consolidation was
/// tried and also bought nothing.
pub(crate) const COMPACTION_NON_REDUCING_BRAKE_STREAK: u32 = 2;

/// How far the fill must come down for a compaction to count as having
/// reduced anything.
///
/// A compaction that lands a fraction of a point lower has bought no room, and
/// counting it as success is exactly how a treadmill reads as progress: the
/// measured loop landed 65% → 64% repeatedly while the floor climbed.
const MIN_REDUCTION_PCT: f64 = 1.0;

/// When the gap between the provider's anchored count and our estimate at anchor
/// exceeds this fraction of the window, the anchor is dropped before emergency
/// truncation (inflated OpenAI-compat usage receipts).
pub(crate) const INFLATED_ANCHOR_GAP_FRACTION: f64 = 0.20;

/// #833 — did this compaction buy anything?
///
/// Pure, so the boundary is testable without a service: the caller supplies
/// the two levels it already reports on the receipt.
pub(crate) fn compaction_reduced(before_pct: f64, after_pct: f64) -> bool {
    after_pct + MIN_REDUCTION_PCT <= before_pct
}

/// Last-resort truncation to 80% of the window: the recovery that costs no
/// provider call.
///
/// Two callers share it — the exhausted-summariser safety net, and the #438 A3
/// loop guard, where compacting again would spend a call on a context that has
/// not come down. Returns whether messages were actually dropped, which is what
/// the caller's `truncated` bookkeeping and the persist-a-marker rule hang off.
///
/// The target is solved in EFFECTIVE space, not local space, because the 90%
/// floor that sends us here reads `AgentContext::effective_token_count()`
/// while `hard_truncate_to` only consumes `token_count`. Those two differ by
/// the anchor gap — `reported - estimated_at_anchor` — whenever a provider has
/// anchored the budget. Cutting the local count to 80% then leaves the
/// effective count at 80% plus that gap, and since `estimated_at_anchor` is a
/// one-shot snapshot the gap is constant across the cut: any gap wider than
/// 10% of the window re-trips the floor on the very next turn, every turn,
/// forever. So subtract the gap from the target and let the local cut run
/// deeper by exactly that much.
///
/// `saturating_sub` does double duty there: a zero gap (no anchor yet) leaves
/// the target at 80% unchanged, and a provider report LOWER than our own
/// estimate — a negative gap — clamps to zero rather than raising the target
/// above 80%.
///
/// Blind by design to the two-message floor in `AgentContext::hard_truncate_to`:
/// a window pinned by two outsized messages cannot be brought down this way, and
/// claiming otherwise would put a marker on a context that never shrank.
fn safety_truncate_to_80(context: &mut AgentContext, max_tokens: usize) -> bool {
    context.discard_inflated_provider_anchor(max_tokens, INFLATED_ANCHOR_GAP_FRACTION);
    let effective_target = (max_tokens as f64 * 0.80) as usize;
    let effective_tokens = context.effective_token_count();
    if effective_tokens <= effective_target {
        return false;
    }
    // How far the provider's view runs ahead of the local count. Constant for
    // this call: `estimated_at_anchor` does not move as we drop messages.
    let anchor_gap = effective_tokens.saturating_sub(context.token_count);
    let target = effective_target.saturating_sub(anchor_gap);
    let before_len = context.messages.len();
    context.hard_truncate_to(target);
    context.trim_to_fit(0);
    context.messages.len() < before_len
}

/// #805: describe what a Tier 2 truncation attempt actually did.
///
/// Split out of `enforce_context_budget` so both shapes can be pinned by a
/// test. The pair of lines this replaces announced the attempt and then printed
/// the **pre-cut** percentage under the words "Hard truncation complete", so a
/// successful cut read as a failure (`158718 tokens (91%)` immediately after
/// cutting to 80%) and a no-op was indistinguishable from a cut. On a live
/// session that cost a day of diagnosis: 347 WARNs, 118 of which dropped
/// literally nothing.
///
/// `cut` is false whenever no message was dropped, and the message then makes
/// no claim that anything was truncated. A window pinned by a couple of outsized
/// messages cannot be brought down this way, and announcing a truncation that
/// did not happen is worse than announcing nothing.
///
/// Returns `(cut, message)`.
fn describe_truncation(
    before_messages: usize,
    before_tokens: usize,
    before_pct: f64,
    context: &AgentContext,
    max_tokens: usize,
) -> (bool, String) {
    let after_messages = context.messages.len();
    let after_tokens = context.token_count;
    let after_pct = if max_tokens > 0 {
        context.effective_token_count() as f64 / max_tokens as f64 * 100.0
    } else {
        100.0
    };
    let cut = after_messages < before_messages;
    let message = if cut {
        format!(
            "Hard truncation cut {before_messages} -> {after_messages} messages, \
             {before_tokens} -> {after_tokens} tokens ({before_pct:.0}% -> {after_pct:.0}% of window)"
        )
    } else {
        format!(
            "Hard truncation freed nothing: still {after_messages} messages, \
             {after_tokens} tokens ({after_pct:.0}% of window) — the 80% target is \
             unreachable from here, the window is pinned by messages this path cannot drop"
        )
    };
    (cut, message)
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
        let static_overhead = if context.provider_anchor.is_none() {
            self.base_context_tokens_for_session(session_id) as usize
        } else {
            0
        };
        let budget_fill = |ctx: &AgentContext| ctx.budget_token_count(static_overhead);
        // The provider's own count when we have one. Measuring against the
        // local estimate is what let a request sail past a 1M limit while
        // being reported as 66% full.
        let mut effective_tokens = budget_fill(context);
        let mut usage_pct = if effective_max > 0 {
            (effective_tokens as f64 / effective_max as f64) * 100.0
        } else {
            100.0
        };

        tracing::trace!(
            "Context budget: {} tokens / {} max = {:.1}% (static_overhead={})",
            effective_tokens,
            effective_max,
            usage_pct,
            static_overhead,
        );

        // ── Tier 2: 90% hard floor — truncate to 80%, then fall through to Tier 1 compaction ──
        if usage_pct >= 90.0 {
            context.discard_inflated_provider_anchor(effective_max, INFLATED_ANCHOR_GAP_FRACTION);
            effective_tokens = budget_fill(context);
            usage_pct = if effective_max > 0 {
                (effective_tokens as f64 / effective_max as f64) * 100.0
            } else {
                100.0
            };
        }
        if usage_pct >= 90.0 {
            // #805: measured around the attempt, and reported after it. The old
            // shape announced "hard truncating" before knowing whether anything
            // would be dropped, then printed the PRE-cut percentage under
            // "Hard truncation complete" — a successful cut read as a failure
            // and a no-op looked identical to a cut.
            let before_snapshot = CompactionContextSnapshot::from_context(context);
            let before_messages = context.messages.len();
            let before_tokens = context.token_count;

            let dropped = safety_truncate_to_80(context, effective_max);
            truncated |= dropped;

            if let Some(cb) = progress_callback {
                cb(session_id, ProgressEvent::TokenCount(context.token_count));
            }

            let (cut, report) = describe_truncation(
                before_messages,
                before_tokens,
                usage_pct,
                context,
                effective_max,
            );
            // One source of truth: the report derives `cut` from the message
            // count it can see, and the cut reports what it dropped. If those
            // two ever disagree, the report is describing a different event.
            debug_assert_eq!(cut, dropped, "truncation report disagrees with the cut");
            if cut {
                tracing::info!("{}", report);
            } else {
                tracing::warn!("{}", report);
            }

            let after_snapshot = CompactionContextSnapshot::from_context(context);
            let (usage_pct_before, usage_pct_after, eff_before, eff_after, msg_before, msg_after) =
                snapshot_pair(before_snapshot, after_snapshot);
            emit_compaction_event(
                CompactionEventStage::TruncateOnly,
                session_id,
                &CompactionEventFields {
                    trigger: Some(CompactionTrigger::Tier2Truncate),
                    path: Some(CompactionPath::Sync),
                    phase: Some(phase),
                    usage_pct_before,
                    usage_pct_after,
                    effective_tokens_before: eff_before,
                    effective_tokens_after: eff_after,
                    messages_before: msg_before,
                    messages_after: msg_after,
                    ..Default::default()
                },
            );

            let usage_pct_now = if effective_max > 0 {
                (budget_fill(context) as f64 / effective_max as f64) * 100.0
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
            (budget_fill(context) as f64 / effective_max as f64) * 100.0
        } else {
            100.0
        };
        let pending_state = self
            .resolve_pending_compaction(session_id, context, phase, usage_pct, progress_callback)
            .await;
        if let PendingState::Applied(outcome) = pending_state {
            return Some(outcome);
        }

        // ── Tier 1: soft trigger at 65% - LLM compaction ──
        let usage_pct = if effective_max > 0 {
            (budget_fill(context) as f64 / effective_max as f64) * 100.0
        } else {
            100.0
        };
        if usage_pct <= 65.0 {
            // Below the compaction trigger. Try to emit the pre-compaction
            // pressure warning if usage is in the 55-64% band (#909), and
            // re-arm the throttle when usage has dropped below the floor.
            self.maybe_emit_pressure_warning(session_id, context, usage_pct);
            // #833: the gate is not firing, so whatever non-reducing run this
            // session had is over — the window came down by some other means
            // (truncation, a smaller turn, a fresh context). Reset HERE rather
            // than on the next successful compaction, which by definition only
            // happens once the problem is back and would leave a stale streak
            // to brake on.
            self.reset_compaction_non_reducing(session_id);
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
            emit_compaction_event(
                CompactionEventStage::Suppress,
                session_id,
                &CompactionEventFields {
                    trigger: Some(CompactionTrigger::LoopGuard),
                    path: Some(CompactionPath::Sync),
                    phase: Some(phase),
                    outcome: Some(CompactionOutcomeKind::LoopGuard),
                    usage_pct_before: Some(usage_pct),
                    ..Default::default()
                },
            );
            return Some(CompactionOutcome::LoopGuard);
        }

        // #833 — the non-reducing brake, immediately after the loop guard and
        // before any summariser is spawned.
        //
        // This is the same disease as the #438 loop with a different pulse: the
        // lane IS working, so `compaction_streak` above never accumulates and
        // the guard never fires (once in 396 compactions). What accumulates
        // instead is a run of compactions that each left the fill at least as
        // high as they found it. Measured Nov 13–15: 252 of 257 consecutive
        // delta compactions raised the fill, median +1,850 tokens, and one
        // session needed 20 of them to reach the consolidation that finally
        // reset it to 17%.
        //
        // One non-reducer escalates the scope instead of braking (see
        // `compaction_scope_with`); two means consolidation was tried and also
        // bought nothing, so a third attempt would be a prediction.
        let non_reducing_streak = self.compaction_state(session_id).non_reducing_streak;
        if non_reducing_streak >= COMPACTION_NON_REDUCING_BRAKE_STREAK {
            tracing::warn!(
                "Compaction non-reducing brake (#833): {} consecutive compactions left the \
                 context at least as full as they found it — suppressing compaction at {:.0}% \
                 rather than adding another document to a window that is growing by them",
                non_reducing_streak,
                usage_pct,
            );
            // The floor may still be movable without a summary, exactly as in
            // the loop guard above.
            if safety_truncate_to_80(context, effective_max) {
                tracing::warn!(
                    "Compaction non-reducing brake (#833): context still over 80% — \
                     safety-truncated without a summary"
                );
            }
            emit_compaction_event(
                CompactionEventStage::Suppress,
                session_id,
                &CompactionEventFields {
                    trigger: Some(CompactionTrigger::NonReducing),
                    path: Some(CompactionPath::Sync),
                    phase: Some(phase),
                    outcome: Some(CompactionOutcomeKind::NonReducingBrake),
                    usage_pct_before: Some(usage_pct),
                    ..Default::default()
                },
            );
            return Some(CompactionOutcome::NonReducingBrake);
        }

        tracing::warn!(
            "Context at {:.0}% (>65%) — triggering LLM compaction",
            usage_pct
        );
        emit_compaction_event(
            CompactionEventStage::GateFire,
            session_id,
            &CompactionEventFields {
                trigger: Some(CompactionTrigger::Auto65),
                path: Some(if self.background_compaction {
                    CompactionPath::Background
                } else {
                    CompactionPath::Sync
                }),
                phase: Some(phase),
                usage_pct_before: Some(usage_pct),
                effective_tokens_before: Some(context.effective_token_count()),
                messages_before: Some(context.messages.len()),
                model: Some(model_name.to_string()),
                ..Default::default()
            },
        );
        // Announce only on a visit that actually summarises. This used to sit
        // above the dispatch, so every tool iteration that re-entered the gate
        // while a background summariser was in flight re-announced the same
        // compaction (#1686).
        if !gate_announces_compaction(&pending_state) {
            // Work is already under way against this exact conversation, or a
            // summary already landed. A second summariser would burn a provider
            // call describing a context the first one is about to replace.
            return truncated.then_some(CompactionOutcome::Truncated);
        }

        tracing::warn!("{}", gate_warn_message(usage_pct));
        self.record_provider_feedback(
            session_id,
            "context_compaction",
            model_name,
            Some(&format!("proactive_65pct tokens={}", context.token_count)),
        );

        match pending_state {
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
            PendingState::StillRunning | PendingState::Applied(_) => {
                unreachable!("gate_announces_compaction returns false for both")
            }
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
        let mut summary_result: Option<super::context::AppliedCompaction> = None;
        const MAX_ATTEMPTS: u32 = 3;
        for attempt in 1..=MAX_ATTEMPTS {
            match self
                .compact_context(
                    session_id,
                    context,
                    model_name,
                    cancel_token,
                    CompactionNotifier::auto_from(session_id, progress_callback.as_ref()),
                    CompactionRunContext::auto_sync(phase),
                    attempt,
                )
                .await
            {
                Ok(applied) => {
                    summary_result = Some(applied);
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
            if let Ok(applied) = self
                .compact_context(
                    session_id,
                    context,
                    model_name,
                    cancel_token,
                    CompactionNotifier::auto_from(session_id, progress_callback.as_ref()),
                    CompactionRunContext::auto_sync(phase),
                    1,
                )
                .await
            {
                summary_result = Some(applied);
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
        if let Some(ref applied) = summary_result {
            self.note_compaction_success(
                session_id,
                context,
                applied,
                // The synchronous path is full-window by definition
                // (`compact_context`), so its input IS the window it reports
                // as "before" — the two numbers coincide here and only here.
                CompactionScope::FullWindow,
                before_pct,
                before_tokens,
                before_tokens,
                compact_started.elapsed(),
                progress_callback,
                CompactionPath::Sync,
                CompactionTrigger::Auto65,
            );
        }

        // Emit the token count the NEXT request will start with.
        if let Some(cb) = progress_callback {
            if let Some(ref applied) = summary_result {
                let marker_tokens = AgentContext::estimate_tokens(&applied.applied_marker) + 100;
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
            // #994: the outcome variant is decided by what was APPLIED — a cut
            // document yields `TruncatedSummary`, never `Summarised`, so no
            // downstream surface can mistake the notice for a summary.
            Some(ref applied) => Some(applied.outcome()),
            None if truncated => Some(CompactionOutcome::Truncated),
            None => None,
        }
    }

    /// Test seam for the #934 head-cut ordering regression.
    ///
    /// `enforce_context_budget` is `pub(super)` — every production caller lives
    /// inside the `service` module — so a test in `src/tests` cannot reach the
    /// gate. The invariant it enforces can only be observed by driving it with
    /// a provider that records what it was handed, so this forwards verbatim
    /// and adds no behaviour of its own. `#[cfg(test)]`: no shipped build sees
    /// it.
    #[cfg(test)]
    pub(crate) async fn enforce_context_budget_for_test(
        &self,
        session_id: Uuid,
        context: &mut AgentContext,
        model_name: &str,
        cancel_token: Option<&tokio_util::sync::CancellationToken>,
        progress_callback: &Option<ProgressCallback>,
        phase: BudgetPhase,
    ) -> Option<CompactionOutcome> {
        self.enforce_context_budget(
            session_id,
            context,
            model_name,
            cancel_token,
            progress_callback,
            phase,
        )
        .await
    }

    /// The two fill levels the waiting path reports (#1686).
    ///
    /// A background summariser is announced while the live context is still
    /// drifting down from Tier-2 trimming, so the live percentage and the
    /// percentage the gate requested are two different measurements of one
    /// event. Session `bee04b00` rendered a progress line reading 54% beside a
    /// receipt reading 66% -> 17%, and `cb1c7a07` rendered 21% beside 66% ->
    /// 16%: in both cases the progress figure sits under the 65% gate, so the
    /// line announced a compaction at a level that cannot have requested one.
    ///
    /// The progress line therefore always carries the requested level, which is
    /// the same `snapshot_usage_pct` the summary already reports as its
    /// "before". The live level stays in the log, where showing how far the
    /// context moved while the summariser ran is the whole point.
    pub(crate) fn waiting_report_levels(requested: f64, live: f64) -> (f64, f64) {
        (requested, live)
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
        applied: &super::context::AppliedCompaction,
        scope: CompactionScope,
        before_pct: f64,
        before_tokens: usize,
        input_tokens: usize,
        elapsed: std::time::Duration,
        progress_callback: &Option<ProgressCallback>,
        path: CompactionPath,
        trigger: CompactionTrigger,
    ) {
        if let Ok(mut map) = self.session_pressure_warned.write() {
            map.insert(session_id, false);
        }
        if let Ok(mut map) = self.last_compaction_elapsed.write() {
            map.insert(session_id, elapsed);
        }
        // #438 S1: a compaction just completed — raise this session's streak.
        // The streak is cleared by the next committed tool result, so a value
        // that keeps climbing means the compaction produced no work (the #226
        // loop).
        self.note_compaction_streak(session_id);
        // #833: what this compaction cost and whether it bought anything. The
        // document size becomes the prior for the next scope decision, and a
        // compaction that did not lower the fill escalates that decision
        // instead of being retried with the same inputs.
        //
        // Measured on the DECORATED summary — the string that was actually
        // welded into the window — so the prior describes the bytes the window
        // pays for, not the model's raw output.
        let after_pct = context.usage_percentage();
        // #994: a cut document leaves no summary to measure, so the notice the
        // apply welded in IS what the window pays for — measuring the empty
        // document would tell the next scope decision that compaction is free.
        let measured = if applied.truncated {
            applied.applied_marker.as_str()
        } else {
            applied.summary.as_str()
        };
        self.note_compaction_reduction(
            session_id,
            AgentContext::estimate_tokens(measured),
            compaction_reduced(before_pct, after_pct),
        );
        // #929: which prompt text produced this summary. The marker row cannot
        // carry it — the persisted row IS the live context (#1928), so a byte
        // added here is a byte every subsequent request pays for — and the
        // feedback ledger is the durable per-session record of compactions, so
        // the stamp goes there: `SELECT metadata FROM feedback_ledger WHERE
        // session_id = ? AND event_type = 'context_compaction'` answers "which
        // prompt wrote this" instead of leaving it to archaeology.
        //
        // Loaded here rather than threaded from the produce site: one small
        // file read per compaction, and the produce site's own load already
        // logged the same fingerprint at INFO (`compaction prompt: source=…
        // fingerprint=…`), so a mid-compaction file swap is visible in the log
        // as two differing lines rather than silently mis-attributed.
        let prompt_stamp =
            CompactionPrompts::load(&crate::config::profile::resolve_profile_home()).stamp();
        self.record_provider_feedback(
            session_id,
            "context_compaction",
            "summary",
            Some(
                &serde_json::json!({
                    "prompt": prompt_stamp,
                    "scope": scope.label(),
                    "before_tokens": before_tokens,
                    "after_tokens": context.token_count,
                    "input_tokens": input_tokens,
                    "elapsed_ms": elapsed.as_millis() as u64,
                })
                .to_string(),
            ),
        );
        if let Some(cb) = progress_callback {
            // The same basis the gate measures on and the meter logs at
            // service/context.rs:1011. Dividing the raw `token_count` by
            // `max_tokens` here dropped the provider anchor, so a context the
            // log recorded at 17% was receipted at 19% (session bee04b00,
            // #1686): one compaction, two different after-numbers.
            // #824: the breakdown's tools rows. Measured on the set the next
            // request actually carries (`tool_schemas_for_session`), never the
            // registry upper bound — the card describes what the window pays
            // for, and a lazy-tools session sends a filtered subset. Measured
            // PER TOOL, each on its own JSON, because the card prints a KB and
            // a share-of-window figure beside every name and a total that is
            // the sum of those rows.
            let tool_sizes: Vec<ToolSize> = self
                .tool_schemas_for_session(session_id)
                .iter()
                .map(|def| {
                    let json = serde_json::to_string(def).unwrap_or_default();
                    ToolSize {
                        name: def.name.clone(),
                        tokens: crate::brain::tokenizer::count_tokens(&json),
                        bytes: json.len(),
                    }
                })
                .collect();
            // #846: the documents this compaction's manifest names, resolved
            // against what the session actually read — the SAME `resolve` the
            // registration in `apply_compaction_continuation` will call, so the
            // card can never list a document the harness then refuses. Done
            // here because registration runs AFTER this emit.
            let (retained_documents, document_refusals) =
                crate::brain::agent::service::context::parse_context_manifest(&applied.summary)
                    .map(|m| {
                        crate::brain::tools::retained_docs::resolve(
                            session_id,
                            &m.retained_documents,
                        )
                    })
                    .unwrap_or_default();
            cb(
                session_id,
                ProgressEvent::CompactionSummary {
                    summary: applied.summary.clone(),
                    // #994: the card renders the outcome class, never a parts
                    // count for a note that is not a summary.
                    truncated: applied.truncated,
                    scope: scope.label(),
                    input_tokens,
                    before_pct,
                    after_pct,
                    before_tokens,
                    after_tokens: context.token_count,
                    tool_sizes,
                    retained_documents,
                    document_refusals,
                    elapsed,
                },
            );
        }
        let outcome = applied.outcome();
        emit_compaction_event(
            CompactionEventStage::Apply,
            session_id,
            &CompactionEventFields {
                trigger: Some(trigger),
                path: Some(path),
                scope: Some(scope),
                outcome: Some((&outcome).into()),
                usage_pct_before: Some(before_pct),
                usage_pct_after: Some(after_pct),
                effective_tokens_before: Some(before_tokens),
                effective_tokens_after: Some(context.effective_token_count()),
                messages_after: Some(context.messages.len()),
                duration_ms: Some(elapsed.as_millis() as u64),
                summary_tokens: Some(AgentContext::estimate_tokens(measured)),
                truncated: Some(applied.truncated),
                ..Default::default()
            },
        );
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
            // One event, two levels (#1686): the notice carries the fill the
            // compaction was requested at, the diagnostic keeps the live one
            // so the drift stays visible in the log where it belongs.
            let (notice_pct, log_pct) =
                Self::waiting_report_levels(pending.snapshot_usage_pct, usage_pct);
            tracing::warn!(
                "Waiting on background compaction at {:.0}% ({:?} elapsed) — {}",
                log_pct,
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
                        usage_pct: notice_pct,
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
            input_tokens,
            started,
        } = pending;

        match handle.await {
            Ok(Ok(document)) => {
                // #994: a cut document never becomes the marker body. The
                // notice takes its place through the same apply path, so the
                // tail that arrived while the summariser ran is preserved
                // either way — and nothing is persisted as a note.
                let applied_marker = if document.truncated {
                    tracing::warn!(
                        "Background compaction: summariser document was CUT at MaxTokens — \
                         welding the truncated-summary notice instead of a summary (#994)"
                    );
                    Self::apply_truncated_summary_notice_after(context, scope, snapshot_len)
                } else {
                    Self::apply_compaction_summary_after(
                        context,
                        scope,
                        &document.text,
                        snapshot_len,
                    )
                };
                // The field's contract: no document, no note text. A cut
                // fragment must not reach the manifest parser or the card —
                // both read `summary` and would describe a document that was
                // never finished (#994).
                let applied = super::context::AppliedCompaction {
                    summary: if document.truncated {
                        String::new()
                    } else {
                        document.text
                    },
                    applied_marker,
                    truncated: document.truncated,
                };
                self.note_compaction_success(
                    session_id,
                    context,
                    &applied,
                    scope,
                    snapshot_usage_pct,
                    snapshot_tokens,
                    input_tokens,
                    started.elapsed(),
                    progress_callback,
                    CompactionPath::Background,
                    CompactionTrigger::BackgroundApply,
                );
                if let Some(cb) = progress_callback {
                    cb(session_id, ProgressEvent::TokenCount(context.token_count));
                }
                PendingState::Applied(applied.outcome())
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
        // #1649/#833: scope-aware input. A session's first compaction summarises
        // the whole window; every later one summarises only the delta since
        // the last marker (frozen segments never re-enter the input), or the
        // segments themselves when the delta scope cannot move the floor.
        //
        // The decision reads THIS session's history: the size of the last
        // document it wrote (what a new one will cost) and whether its last
        // compaction actually lowered the fill (if not, escalate rather than
        // repeat).
        let trend = self.compaction_state(session_id);
        let expected_document = if trend.last_summary_tokens > 0 {
            trend.last_summary_tokens
        } else {
            AgentContext::DEFAULT_SUMMARY_TOKENS
        };
        let (scope, messages, token_count) =
            context.compaction_input_with(expected_document, trend.non_reducing_streak >= 1);
        // Captured before the spawn moves it: the receipt reports what the
        // summariser actually read, which is not `snapshot_tokens` on a delta
        // pass (#833 follow-up).
        let input_tokens = token_count;
        tracing::info!(
            "Spawning background compaction at {:.0}% ({snapshot_len} messages, {scope:?} scope)",
            usage_pct,
        );
        emit_compaction_event(
            CompactionEventStage::BackgroundSpawn,
            session_id,
            &CompactionEventFields {
                trigger: Some(CompactionTrigger::Auto65),
                path: Some(CompactionPath::Background),
                scope: Some(scope),
                usage_pct_before: Some(usage_pct),
                effective_tokens_before: Some(context.token_count),
                messages_before: Some(snapshot_len),
                model: Some(model_name.to_string()),
                ..Default::default()
            },
        );

        let provider = self.provider_for_session(session_id);
        let fallbacks = self.fallback_chain_snapshot();
        let max_tokens = context.max_tokens;
        let model = model_name.to_string();
        let max_output = super::request_budget::compaction_summary_request_allowance();
        let working_dir = self.get_working_directory_for_session(session_id);
        let auto_approve = self.auto_approve_tools;
        let subagents = self.subagent_manager.clone();
        let attempt_deadline = self.compaction_attempt_deadline(session_id);
        // #138 part 2: the inventory stamp lists the UNION (active ∪ seen).
        // Re-injection below stays active-only on purpose.
        let stamp_skills = crate::brain::tools::seen_skills::stamp_skills_for_session(session_id);
        let seen_aux = crate::brain::tools::seen_skills::aux_seen_for_session(session_id);
        let active_tools = self.tool_registry.active_tools(session_id);
        let (documents, documents_omitted) =
            crate::brain::tools::retained_docs::inventory_documents(session_id);
        let context_inventory = Self::format_context_inventory(
            max_tokens,
            &stamp_skills,
            &seen_aux,
            &active_tools,
            Some(&self.tool_registry),
            &documents,
            documents_omitted,
        );
        // Its own token: this task answers to session teardown, never to a
        // context that grew impatient.
        let cancel = tokio_util::sync::CancellationToken::new();
        let handle = tokio::spawn(async move {
            let document = Self::compute_compaction_summary(
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
                CompactionRunContext::background_auto(),
                1,
            )
            .await?;
            // #994: nothing to decorate or trim on the cut class — there is no
            // document, and the notice is built at apply time against the live
            // context (the snapshot may have outlived its turn by then).
            if document.truncated {
                return Ok(document);
            }
            // Named `summary` for the same reason as the synchronous path: the
            // #934 source pin reads this call site by argument name.
            let summary = document.text;
            Ok(super::context::CompactionDocument {
                text: Self::finalize_continuation_document(summary, session_id, subagents).await,
                truncated: false,
            })
        });

        if let Ok(mut map) = self.pending_compactions.lock() {
            map.insert(
                session_id,
                PendingCompaction {
                    handle,
                    snapshot_len,
                    scope,
                    snapshot_usage_pct: usage_pct,
                    // The WHOLE window, not `token_count`: on the background path that
                    // variable is the third return of `compaction_input()`, which for
                    // `DeltaSinceMarker` is only the delta since the last marker (821
                    // tokens on a 134K window), and the card rendered it as the
                    // before-state — `821 → 134K (+16179.7%)` (#824 follow-up).
                    snapshot_tokens: context.token_count,
                    input_tokens,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::brain::provider::Message;

    /// A message that estimates to a few thousand tokens, so a window can be
    /// filled with a handful of them and the drop loop has real work to do.
    fn filler() -> String {
        "the quick brown fox jumps over the lazy dog ".repeat(400)
    }

    /// A context filled to roughly `local_tokens` of local estimate, with more
    /// messages than the two-message floor so truncation is not blocked by it.
    fn context_with(window: usize, local_tokens: usize) -> AgentContext {
        let mut c = AgentContext::new(Uuid::new_v4(), window);
        while c.token_count < local_tokens {
            c.add_message(Message::user(filler()));
        }
        assert!(
            c.messages.len() > 2,
            "the two-message floor must not be what bounds this fixture"
        );
        c
    }

    #[test]
    fn truncation_lands_the_effective_count_under_80_percent() {
        // The loop this guards: the 90% floor reads `effective_token_count()`
        // while the cut only moves `token_count`. With an anchor gap wider than
        // 10% of the window, a local cut to 80% left the effective count back
        // over 90%, so the floor re-tripped on the next turn — every turn.
        let mut c = context_with(100_000, 85_000);
        let local = c.token_count;
        let gap = 10_000;
        c.record_provider_reported_tokens(local + gap);

        assert!(c.usage_percentage() > 90.0, "the fixture trips the floor");
        assert!(c.effective_token_count() > 80_000);

        assert!(safety_truncate_to_80(&mut c, 100_000));
        assert!(
            c.effective_token_count() <= 80_000,
            "the cut must be solved in effective space, or the floor re-trips \
             forever: local {} + gap {} is still over 80%",
            c.token_count,
            gap
        );
        assert!(c.usage_percentage() < 90.0, "and the floor stays untripped");
    }

    #[test]
    fn without_an_anchor_the_target_is_plain_80_percent() {
        let mut c = context_with(100_000, 85_000);
        assert_eq!(c.effective_token_count(), c.token_count);

        assert!(safety_truncate_to_80(&mut c, 100_000));
        assert!(c.effective_token_count() <= 80_000);
    }

    #[test]
    fn a_report_below_our_estimate_does_not_raise_the_target() {
        // Negative gap: the provider counted less than we did. Subtracting it
        // must clamp to zero, never lift the target above 80% of the window.
        let mut c = context_with(100_000, 90_000);
        let local = c.token_count;
        c.record_provider_reported_tokens(local - 3_000);
        assert!(
            c.effective_token_count() > 80_000,
            "the fixture must still trip, or this proves nothing"
        );

        assert!(safety_truncate_to_80(&mut c, 100_000));
        assert!(c.token_count <= 80_000, "the target stayed at plain 80%");
    }

    #[test]
    fn a_context_already_under_the_target_is_left_alone() {
        let mut c = context_with(100_000, 50_000);
        let before = c.messages.len();
        assert!(!safety_truncate_to_80(&mut c, 100_000));
        assert_eq!(c.messages.len(), before);
    }

    #[test]
    fn inflated_anchor_is_dropped_and_avoids_deep_truncation() {
        let window = 200_000;
        let mut c = context_with(window, 100_000);
        let local = c.token_count;
        let gap = 120_000;
        c.record_provider_reported_tokens(local + gap);
        assert!(
            c.effective_token_count() as f64 / window as f64 > 0.9,
            "fixture must trip the 90% floor on the inflated anchor"
        );
        let messages_before = c.messages.len();
        let dropped = safety_truncate_to_80(&mut c, window);
        assert!(
            c.provider_anchor.is_none(),
            "inflated anchor must be cleared before local evaluation"
        );
        assert!(
            !dropped,
            "local fill is under 80% after dropping the bad anchor"
        );
        assert_eq!(
            c.messages.len(),
            messages_before,
            "must not deep-cut messages when only the anchor was wrong"
        );
    }

    /// #805: a successful cut reports where it ENDED, not where it started.
    ///
    /// The line this replaces printed the PRE-cut percentage under the words
    /// "Hard truncation complete", so cutting 91% down to 80% logged
    /// `158718 tokens (91%)` — a success that read as a failure.
    #[test]
    fn a_cut_reports_the_post_cut_percentage() {
        let window = 100_000usize;
        let mut c = context_with(window, 92_000);
        let before_messages = c.messages.len();
        let before_tokens = c.token_count;
        let before_pct = c.effective_token_count() as f64 / window as f64 * 100.0;

        assert!(
            safety_truncate_to_80(&mut c, window),
            "the fixture must cut"
        );
        let (cut, message) =
            describe_truncation(before_messages, before_tokens, before_pct, &c, window);

        assert!(cut, "the fixture dropped messages, so the report says so");
        let after_pct = c.effective_token_count() as f64 / window as f64 * 100.0;
        assert!(
            message.contains(&format!("{after_pct:.0}%")),
            "the report must carry the post-cut percentage {after_pct:.0}%, \
             not the pre-cut {before_pct:.0}%: {message}"
        );
        assert!(
            !message.contains(&format!("({before_pct:.0}%)")),
            "the pre-cut percentage must never be presented as the result: {message}"
        );
        assert!(
            message.contains(&format!("{before_messages} -> {}", c.messages.len())),
            "and it must name the message count it moved: {message}"
        );
    }

    /// #805: a no-op must say it freed nothing, never wear the success wording.
    ///
    /// The two-message floor makes `hard_truncate_to` refuse to drop anything
    /// while the local count is still over the target, and the window is not
    /// exceeded, so `trim_to_fit` finds nothing either. That is the shape that
    /// logged "Hard truncation complete" 118 times on `359fe71b` without moving
    /// a single token.
    #[test]
    fn a_no_op_does_not_claim_a_truncation() {
        let window = 100_000usize;
        let mut c = AgentContext::new(Uuid::new_v4(), window);

        // Two messages only, and sized so the pair lands between 80% and 100%
        // of the window. Measured rather than assumed: the tokenizer decides,
        // and a fixture that guessed would pass for the wrong reason.
        let mut per_message = filler();
        while AgentContext::estimate_tokens(&per_message) * 2 <= window * 4 / 5 {
            per_message.push_str(&filler());
        }
        c.add_message(Message::user(per_message.clone()));
        c.add_message(Message::user(per_message));

        assert_eq!(
            c.messages.len(),
            2,
            "the two-message floor is the mechanism under test"
        );
        assert!(
            c.token_count > window * 4 / 5,
            "precondition: over the 80% target, or the early return fires and \
             this proves nothing (local {})",
            c.token_count
        );
        assert!(
            c.token_count <= window,
            "precondition: under the window, or trim_to_fit drops and this is \
             not the no-op shape (local {})",
            c.token_count
        );

        let before_messages = c.messages.len();
        let before_tokens = c.token_count;
        let before_pct = c.effective_token_count() as f64 / window as f64 * 100.0;
        let dropped = safety_truncate_to_80(&mut c, window);
        let (cut, message) =
            describe_truncation(before_messages, before_tokens, before_pct, &c, window);

        assert!(!dropped, "the two-message floor must block the cut");
        assert_eq!(
            c.messages.len(),
            before_messages,
            "and nothing else may drop them either, or this is not the no-op shape"
        );
        assert!(
            !cut,
            "nothing was dropped, so the report must not claim a cut"
        );
        assert!(
            message.contains("freed nothing"),
            "a no-op must say so plainly: {message}"
        );
        assert!(
            !message.contains("Hard truncation cut"),
            "a no-op must not wear the success wording: {message}"
        );
    }
}
