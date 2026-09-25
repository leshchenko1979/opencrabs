//! Goal manager — CRUD and post-turn evaluation for session goals.

use crate::brain::goal::criteria::{derive_criteria_if_needed, parse_criteria, serialize_criteria};
use crate::brain::goal::evidence::GoalEvidence;
use crate::brain::goal::judge::{JudgeOutcome, judge_goal};
use crate::brain::goal::types::{
    CriterionEvaluation, CriterionStatus, DEFERRED_AWAIT_KIND, GoalDecision, GoalVerdict,
    MAX_CONSECUTIVE_UNCERTAIN, MAX_PARSE_FAILURES,
};
use crate::brain::provider::Provider;
use crate::db::SessionBindingRepository;
use crate::db::models::GoalState;
use crate::services::ServiceContext;
use chrono::Utc;
use rusqlite::OptionalExtension;
use uuid::Uuid;

/// Goal manager: handles goal lifecycle and post-turn evaluation.
pub struct GoalManager {
    ctx: ServiceContext,
}

impl GoalManager {
    pub fn new(ctx: ServiceContext) -> Self {
        Self { ctx }
    }

    /// Set a new goal for a session. Replaces any existing active goal.
    ///
    /// Criteria-less entry point: the goal declares no checkable conditions of
    /// its own, so the first [`Self::evaluate_after_turn`] derives them from the
    /// goal text. Callers that already know the criteria (the `/goal` slash
    /// command and the `goal_manage` tool) use
    /// [`Self::set_goal_with_criteria`] instead — this is a thin wrapper over it
    /// so the many callers with nothing to declare (cron pipeline, subagent and
    /// A2A notify) need not carry an argument they would pass `None` for.
    pub async fn set_goal(
        &self,
        session_id: Uuid,
        goal_text: String,
        channel: Option<String>,
        channel_chat_id: Option<String>,
        max_turns: Option<u32>,
    ) -> Result<GoalState, String> {
        self.set_goal_with_criteria(
            session_id,
            goal_text,
            None,
            channel,
            channel_chat_id,
            max_turns,
        )
        .await
    }

    /// Set a new goal with explicitly declared criteria (#299).
    ///
    /// `criteria` are the declared, checkable conditions the goal is judged
    /// against. Supplying them explicitly is the preferred path — they are
    /// stored verbatim, so the goal is judged against what the human or the
    /// agent actually asked for. `None` (or an empty list) leaves the column
    /// NULL and defers derivation to the first evaluation.
    ///
    /// If `max_turns` is None, falls back to `agent.goal_max_turns` from config,
    /// or `DEFAULT_MAX_TURNS` (20).
    pub async fn set_goal_with_criteria(
        &self,
        session_id: Uuid,
        goal_text: String,
        criteria: Option<Vec<String>>,
        channel: Option<String>,
        channel_chat_id: Option<String>,
        max_turns: Option<u32>,
    ) -> Result<GoalState, String> {
        // Clear any existing goal for this session first
        if let Err(e) = self.clear_goal(session_id).await {
            tracing::warn!(error = %e, session_id = %session_id, "failed to clear goal");
        }

        let effective_max_turns =
            max_turns.or_else(|| crate::config::Config::current().agent.goal_max_turns);
        let criteria_json = criteria
            .filter(|c| !c.is_empty())
            .map(|c| serialize_criteria(&c));

        let mut goal = GoalState::new(
            session_id,
            goal_text,
            channel,
            channel_chat_id,
            effective_max_turns,
        );
        goal.criteria = criteria_json.clone();

        let pool = self.ctx.pool();
        let conn = pool
            .get()
            .await
            .map_err(|e| format!("DB connection failed: {}", e))?;

        let id = goal.id.to_string();
        let sid = goal.session_id.to_string();
        let goal_text_clone = goal.goal_text.clone();
        let max_turns = goal.max_turns;
        let now = Utc::now().to_rfc3339();

        conn.interact(move |conn| {
            conn.execute(
                "INSERT INTO goal_state \
                 (id, session_id, goal_text, state, turns_used, max_turns, \
                  consecutive_parse_failures, consecutive_uncertain, criteria, \
                  created_at, updated_at) \
                 VALUES (?1, ?2, ?3, 'active', 0, ?4, 0, 0, ?5, ?6, ?6)",
                rusqlite::params![id, sid, goal_text_clone, max_turns, criteria_json, now],
            )
        })
        .await
        .map_err(|e| format!("DB interact failed: {}", e))?
        .map_err(|e| format!("DB insert failed: {}", e))?;

        tracing::info!("Goal set for session {}: {}", session_id, goal.goal_text);
        Ok(goal)
    }

    /// Clear (delete) the goal for a session.
    pub async fn clear_goal(&self, session_id: Uuid) -> Result<(), String> {
        let pool = self.ctx.pool();
        let conn = pool
            .get()
            .await
            .map_err(|e| format!("DB connection failed: {}", e))?;
        let sid = session_id.to_string();

        conn.interact(move |conn| {
            conn.execute(
                "DELETE FROM goal_state WHERE session_id = ?1",
                rusqlite::params![sid],
            )
        })
        .await
        .map_err(|e| format!("DB interact failed: {}", e))?
        .map_err(|e| format!("DB delete failed: {}", e))?;

        Ok(())
    }

    /// Get the active goal for a session, if any.
    ///
    /// The column list must stay in sync with [`GoalState::from_row`], which
    /// reads every field **by name** — a column omitted here surfaces as a
    /// runtime `InvalidColumnName` on every goal read, not a compile error.
    pub async fn get_goal(&self, session_id: Uuid) -> Result<Option<GoalState>, String> {
        let pool = self.ctx.pool();
        let conn = pool
            .get()
            .await
            .map_err(|e| format!("DB connection failed: {}", e))?;
        let sid = session_id.to_string();

        let row: Option<GoalState> = conn
            .interact(move |conn| -> rusqlite::Result<Option<GoalState>> {
                let mut stmt = conn.prepare(
                    "SELECT id, session_id, goal_text, state, turns_used, max_turns, \
                     consecutive_parse_failures, consecutive_uncertain, criteria, \
                     criterion_evaluations, judge_verdict, judge_reason, \
                     channel, channel_chat_id, created_at, updated_at \
                     FROM goal_state WHERE session_id = ?1 LIMIT 1",
                )?;
                let result = stmt
                    .query_row(rusqlite::params![sid], GoalState::from_row)
                    .optional()?;
                Ok(result)
            })
            .await
            .map_err(|e| format!("DB interact failed: {}", e))?
            .map_err(|e| format!("DB query failed: {}", e))?;

        Ok(row)
    }

    /// Pause a goal.
    pub async fn pause_goal(&self, session_id: Uuid) -> Result<(), String> {
        self.set_state(session_id, "paused").await
    }

    /// Resume a paused goal.
    pub async fn resume_goal(&self, session_id: Uuid) -> Result<(), String> {
        self.set_state(session_id, "active").await
    }

    /// Update the goal state string.
    async fn set_state(&self, session_id: Uuid, state: &str) -> Result<(), String> {
        let pool = self.ctx.pool();
        let conn = pool
            .get()
            .await
            .map_err(|e| format!("DB connection failed: {}", e))?;
        let sid = session_id.to_string();
        let st = state.to_string();
        let now = Utc::now().to_rfc3339();

        conn.interact(move |conn| {
            conn.execute(
                "UPDATE goal_state SET state = ?1, updated_at = ?2 \
                 WHERE session_id = ?3",
                rusqlite::params![st, now, sid],
            )
        })
        .await
        .map_err(|e| format!("DB interact failed: {}", e))?
        .map_err(|e| format!("DB update failed: {}", e))?;

        Ok(())
    }

    /// Persist the goal's declared criteria (#299).
    ///
    /// Called when criteria were derived lazily on the first evaluation, so the
    /// set is fixed from then on — later turns judge against the SAME criteria
    /// instead of re-deriving a fresh (and possibly easier) set each turn.
    async fn set_criteria(&self, session_id: Uuid, criteria: &[String]) -> Result<(), String> {
        let pool = self.ctx.pool();
        let conn = pool
            .get()
            .await
            .map_err(|e| format!("DB connection failed: {}", e))?;
        let sid = session_id.to_string();
        let json = serialize_criteria(criteria);
        let now = Utc::now().to_rfc3339();

        conn.interact(move |conn| {
            conn.execute(
                "UPDATE goal_state SET criteria = ?1, updated_at = ?2 \
                 WHERE session_id = ?3",
                rusqlite::params![json, now, sid],
            )
        })
        .await
        .map_err(|e| format!("DB interact failed: {}", e))?
        .map_err(|e| format!("DB update failed: {}", e))?;

        Ok(())
    }

    /// Record a judge verdict in the DB.
    async fn record_verdict(
        &self,
        session_id: Uuid,
        verdict: &GoalVerdict,
        reason: &str,
        criteria: &[CriterionEvaluation],
    ) -> Result<(), String> {
        let pool = self.ctx.pool();
        let conn = pool
            .get()
            .await
            .map_err(|e| format!("DB connection failed: {}", e))?;
        let sid = session_id.to_string();
        let v = verdict.as_str().to_string();
        let r = reason.to_string();
        // The per-criterion detail rides along with the verdict so `status` can
        // report how each criterion fared without re-running the judge. A
        // serialization failure must not sink the verdict write, so the column
        // degrades to NULL and the verdict still lands.
        let evals = serde_json::to_string(criteria).ok();
        let now = Utc::now().to_rfc3339();

        conn.interact(move |conn| {
            conn.execute(
                "UPDATE goal_state SET judge_verdict = ?1, judge_reason = ?2, \
                 criterion_evaluations = ?3, updated_at = ?4 \
                 WHERE session_id = ?5",
                rusqlite::params![v, r, evals, now, sid],
            )
        })
        .await
        .map_err(|e| format!("DB interact failed: {}", e))?
        .map_err(|e| format!("DB update failed: {}", e))?;

        Ok(())
    }

    /// Write the consecutive-`UNCERTAIN` streak (#299).
    ///
    /// The caller passes the value it computed from the goal row it already
    /// loaded, so no read-back is needed. `0` clears the streak, which is what
    /// a `VERIFIED` or `REJECTED` verdict does: both are evidence-backed
    /// answers, and the cap exists only for runs of no-evidence turns.
    async fn set_uncertain_streak(&self, session_id: Uuid, streak: i32) -> Result<(), String> {
        let pool = self.ctx.pool();
        let conn = pool
            .get()
            .await
            .map_err(|e| format!("DB connection failed: {}", e))?;
        let sid = session_id.to_string();
        let now = Utc::now().to_rfc3339();

        conn.interact(move |conn| {
            conn.execute(
                "UPDATE goal_state SET consecutive_uncertain = ?1, updated_at = ?2 \
                 WHERE session_id = ?3",
                rusqlite::params![streak, now, sid],
            )
        })
        .await
        .map_err(|e| format!("DB interact failed: {}", e))?
        .map_err(|e| format!("DB update failed: {}", e))?;

        Ok(())
    }

    /// Increment the turn counter and parse failure count.
    pub async fn increment_turns(
        &self,
        session_id: Uuid,
        parse_failed: bool,
    ) -> Result<(), String> {
        let pool = self.ctx.pool();
        let conn = pool
            .get()
            .await
            .map_err(|e| format!("DB connection failed: {}", e))?;
        let sid = session_id.to_string();
        let now = Utc::now().to_rfc3339();

        conn.interact(move |conn| {
            if parse_failed {
                conn.execute(
                    "UPDATE goal_state SET turns_used = turns_used + 1, \
                     consecutive_parse_failures = consecutive_parse_failures + 1, \
                     updated_at = ?1 WHERE session_id = ?2",
                    rusqlite::params![now, sid],
                )
            } else {
                conn.execute(
                    "UPDATE goal_state SET turns_used = turns_used + 1, \
                     consecutive_parse_failures = 0, updated_at = ?1 \
                     WHERE session_id = ?2",
                    rusqlite::params![now, sid],
                )
            }
        })
        .await
        .map_err(|e| format!("DB interact failed: {}", e))?
        .map_err(|e| format!("DB update failed: {}", e))?;

        Ok(())
    }

    /// Evaluate the goal after a turn completes. This is the core hook.
    ///
    /// Order of gates (#299), cheapest and most certain first:
    ///
    /// 1. **State / budget / parse-failure** gates — a paused, completed or
    ///    over-budget goal is answered without a model call.
    /// 2. **Mechanical short-circuit** — if the evidence pack shows a background
    ///    command still running or a plan task still open, the goal is provably
    ///    unfinished and the judge is never called. This is the fix for the
    ///    original defect: a status report claiming "done" cannot outvote a
    ///    process the harness can see running.
    /// 3. **The judge** — criteria + evidence pack + last response, aggregated
    ///    in Rust by [`crate::brain::goal::types::aggregate_verdict`].
    ///
    /// Returns a `GoalDecision` telling the tool loop whether to continue or
    /// stop.
    ///
    /// **Why criteria are derived here and not in `set_goal`:** derivation needs
    /// a provider and a model, and no `set_goal` caller holds one — the
    /// `/goal` slash command, the `goal_manage` tool and the cron pipeline all
    /// run without a provider in hand. Evaluation is the first point where both
    /// are available, so an unset criteria list is filled in on the first
    /// evaluation and persisted, fixing the set for every later turn.
    pub async fn evaluate_after_turn(
        &self,
        provider: &dyn Provider,
        model: &str,
        session_id: Uuid,
        evidence: &GoalEvidence,
        last_response: &str,
    ) -> GoalDecision {
        // Load current goal
        let goal = match self.get_goal(session_id).await {
            Ok(Some(g)) => g,
            Ok(None) => {
                return GoalDecision::Done {
                    reason: "no active goal".to_string(),
                };
            }
            Err(e) => {
                tracing::error!("Failed to load goal state: {}", e);
                return GoalDecision::Done {
                    reason: format!("goal load error: {}", e),
                };
            }
        };

        // Any evaluation that reaches here is NOT a deferral: the wait this
        // goal declared (#567) is over, or the goal has ended. Clearing first
        // and re-setting only on the deferral path below means a stale wait can
        // never outlive the deferral it belonged to — and the clear is scoped to
        // our own kind, so a `ci_run` wait declared through `await_external` in
        // this same session is untouched (#344).
        self.clear_deferral_await(session_id).await;

        // Check if goal is already completed/failed
        if goal.state != "active" {
            return GoalDecision::Done {
                reason: format!("goal state is '{}'", goal.state),
            };
        }

        // Check turn budget
        if goal.turns_used >= goal.max_turns {
            tracing::info!(
                "Goal turn budget exhausted ({}/{}) — pausing",
                goal.turns_used,
                goal.max_turns
            );
            if let Err(e) = self.set_state(session_id, "paused").await {
                tracing::warn!(error = %e, session_id = %session_id, "failed to set goal state to paused");
            }
            return GoalDecision::Paused {
                reason: format!(
                    "Goal turn budget exhausted ({}/{} turns used). The goal has been paused. Use `/goal resume` or increase the turn budget to continue.",
                    goal.turns_used, goal.max_turns
                ),
            };
        }

        // Check consecutive parse failures
        if goal.consecutive_parse_failures >= MAX_PARSE_FAILURES as i32 {
            tracing::warn!(
                "Goal auto-paused after {} consecutive parse failures",
                goal.consecutive_parse_failures
            );
            if let Err(e) = self.set_state(session_id, "paused").await {
                tracing::warn!(error = %e, session_id = %session_id, "failed to set goal state to paused");
            }
            return GoalDecision::Paused {
                reason: format!(
                    "auto-paused after {} consecutive judge parse failures",
                    goal.consecutive_parse_failures
                ),
            };
        }

        let turns_used = goal.turns_used + 1;

        // The collector knows the mechanical facts; the goal row owns the turn
        // budget. Attach it here so every render below carries the true count
        // (this turn included — it has already run by the time we evaluate it).
        let evidence = evidence.with_budget(turns_used.max(0) as u32, goal.max_turns.max(0) as u32);
        let evidence = &evidence;

        // ── Mechanical short-circuit ────────────────────────────────────────
        // Work the harness can SEE is unfinished needs no judge. Skipping the
        // model call here is both cheaper and strictly more reliable than
        // asking a model to weigh a status report against a running process.
        if let Some(hold) = mechanical_hold(evidence) {
            // #567: the hold conflated two different states. When a PLAN TASK is
            // open there is work the agent can actually advance, so the #299
            // re-prompt is right and stays. When the only blocker is a running
            // background task, the agent is *waiting on the task that is the
            // goal* — the continuation prompt asks for work that does not exist,
            // the turn can only re-report, and billing it burns the very budget
            // the goal needs to survive the wait. Defer instead: end the turn,
            // keep the goal active, spend no budget.
            if evidence.unresolved_tasks.is_empty() {
                let reason = format!(
                    "{} background task(s) still running: {}",
                    evidence.running_tasks.len(),
                    evidence.running_tasks.join("; ")
                );
                tracing::info!(
                    session_id = %session_id,
                    turns_used,
                    max_turns = goal.max_turns,
                    "Goal deferred — turn ends with budget intact: {}",
                    reason
                );
                let wake_ref = evidence.running_tasks.join("; ");
                self.record_deferral_await(session_id, &wake_ref).await;
                return GoalDecision::Deferred { reason, wake_ref };
            }

            tracing::info!(
                session_id = %session_id,
                "Goal mechanical hold — judge skipped: {}",
                hold
            );
            if let Err(e) = self.increment_turns(session_id, false).await {
                tracing::warn!(error = %e, session_id = %session_id, "failed to increment goal turns");
            }
            return GoalDecision::Continue {
                continuation_prompt: build_mechanical_prompt(
                    &goal.goal_text,
                    &hold,
                    evidence,
                    turns_used,
                    goal.max_turns,
                ),
                corrections: None,
            };
        }

        // ── Criteria ────────────────────────────────────────────────────────
        // Fixed on first evaluation, then reused: a goal judged against a fresh
        // set every turn could quietly get easier.
        let mut criteria = parse_criteria(goal.criteria.as_deref().unwrap_or(""));
        if criteria.is_empty() {
            criteria = derive_criteria_if_needed(provider, model, &goal.goal_text, &[]).await;
            if !criteria.is_empty()
                && let Err(e) = self.set_criteria(session_id, &criteria).await
            {
                tracing::warn!(error = %e, session_id = %session_id, "failed to persist derived criteria");
            }
        }

        // ── Judge ───────────────────────────────────────────────────────────
        let outcome = judge_goal(
            provider,
            model,
            &goal.goal_text,
            &criteria,
            &evidence.render(),
            last_response,
        )
        .await;

        let _ = self
            .record_verdict(
                session_id,
                &outcome.verdict,
                &outcome.reason,
                &outcome.criteria,
            )
            .await;

        if let Err(e) = self.increment_turns(session_id, outcome.parse_failed).await {
            tracing::warn!(error = %e, session_id = %session_id, "failed to increment goal turns");
        }

        match outcome.verdict {
            GoalVerdict::Verified => {
                let _ = self.set_uncertain_streak(session_id, 0).await;
                if let Err(e) = self.set_state(session_id, "completed").await {
                    tracing::warn!(error = %e, session_id = %session_id, "failed to set goal state to completed");
                }
                GoalDecision::Done {
                    reason: outcome.reason,
                }
            }
            GoalVerdict::Rejected => {
                // Evidence-backed "not done" — the streak resets, the loop
                // continues with the failing criteria named.
                let _ = self.set_uncertain_streak(session_id, 0).await;
                GoalDecision::Continue {
                    continuation_prompt: build_continuation_prompt(
                        &goal.goal_text,
                        &outcome,
                        evidence,
                        turns_used,
                        goal.max_turns,
                    ),
                    corrections: outcome.corrections,
                }
            }
            GoalVerdict::Uncertain => {
                let streak = goal.consecutive_uncertain + 1;
                let _ = self.set_uncertain_streak(session_id, streak).await;

                if streak >= MAX_CONSECUTIVE_UNCERTAIN as i32 {
                    tracing::warn!(
                        session_id = %session_id,
                        streak,
                        "Goal auto-paused — evidence budget exhausted"
                    );
                    if let Err(e) = self.set_state(session_id, "paused").await {
                        tracing::warn!(error = %e, session_id = %session_id, "failed to set goal state to paused");
                    }
                    return GoalDecision::Paused {
                        reason: format!(
                            "Evidence budget exhausted: {} consecutive UNCERTAIN verdicts with no \
                             criterion proven. The goal has been paused with {}/{} turns used. \
                             Unproven criteria: {}. Use `/goal resume` to continue, or restate the \
                             goal with checkable criteria.",
                            streak,
                            turns_used,
                            goal.max_turns,
                            unproven_list(&outcome.criteria)
                        ),
                    };
                }

                GoalDecision::Continue {
                    continuation_prompt: build_continuation_prompt(
                        &goal.goal_text,
                        &outcome,
                        evidence,
                        turns_used,
                        goal.max_turns,
                    ),
                    corrections: outcome.corrections,
                }
            }
        }
    }

    /// Register the durable await record for a deferred goal (#567, #344).
    ///
    /// A goal is a continuation mechanism and cannot start a turn by itself
    /// (#480), so a deferral needs an external waker. The primary one is the
    /// background-task completion enqueue, which pushes a turn for a bound
    /// session. This record is the BACKSTOP for the wake that is lost: it is the
    /// same `await_kind` / `await_ref` / `await_at` handle #344 built for CI
    /// waits, read by the boot classifier on restart and by the await sweep for
    /// a wait that outlives `await_stale_secs`.
    ///
    /// A session with no binding row cannot be woken by either reader — the
    /// UPDATE matches nothing. That is logged rather than swallowed, because a
    /// deferral that believes it is parked while nothing can wake it is the
    /// exact failure #344 exists to remove. The deferral itself is still
    /// correct in that case: the goal stays active and the boot classifier is
    /// the remaining backstop.
    async fn record_deferral_await(&self, session_id: Uuid, wake_ref: &str) {
        let repo = SessionBindingRepository::new(self.ctx.pool());
        match repo
            .set_await(
                &session_id.to_string(),
                DEFERRED_AWAIT_KIND,
                Some(wake_ref),
            )
            .await
        {
            Ok(0) => tracing::warn!(
                session_id = %session_id,
                "Deferred goal has no session binding — the await record did not land, \
                 so only the boot classifier can wake this session"
            ),
            Ok(_) => {}
            Err(e) => tracing::warn!(
                error = %e,
                session_id = %session_id,
                "failed to record the deferral await"
            ),
        }
    }

    /// Clear a deferral await record left by [`Self::record_deferral_await`].
    ///
    /// Scoped to [`DEFERRED_AWAIT_KIND`], so it can never un-park a lane whose
    /// wait was declared through `await_external` (#344).
    async fn clear_deferral_await(&self, session_id: Uuid) {
        let repo = SessionBindingRepository::new(self.ctx.pool());
        if let Err(e) = repo
            .clear_await_of_kind(&session_id.to_string(), DEFERRED_AWAIT_KIND)
            .await
        {
            tracing::warn!(
                error = %e,
                session_id = %session_id,
                "failed to clear the deferral await"
            );
        }
    }
}

/// The mechanical reason a goal cannot be done yet, if there is one (#299).
///
/// Checked before any model call: a running background command or an open plan
/// task is direct harness-held evidence of unfinished work, which outranks
/// anything the assistant wrote about being finished.
fn mechanical_hold(evidence: &GoalEvidence) -> Option<String> {
    if !evidence.running_tasks.is_empty() {
        return Some(format!(
            "{} background task(s) still running: {}",
            evidence.running_tasks.len(),
            evidence.running_tasks.join("; ")
        ));
    }
    if !evidence.unresolved_tasks.is_empty() {
        return Some(format!(
            "{} plan task(s) still open: {}",
            evidence.unresolved_tasks.len(),
            evidence.unresolved_tasks.join("; ")
        ));
    }
    None
}

/// Continuation prompt used when the judge was skipped on a mechanical hold.
fn build_mechanical_prompt(
    goal_text: &str,
    hold: &str,
    evidence: &GoalEvidence,
    turns_used: i32,
    max_turns: i32,
) -> String {
    format!(
        "[GOAL CONTINUATION — mechanically unfinished, not judged]\n\n\
         Goal: {}\n\n\
         Mechanical gate: {}\n\n\
         Turn {} of {}.\n\n\
         Evidence for this turn:\n{}\n\n\
         The harness can see work in flight, so no completion claim is being \
         accepted yet. Keep working the goal to a state the harness can verify: \
         a finished background task with its result read, and every plan task \
         resolved. The goal will be re-evaluated after your response.",
        goal_text,
        hold,
        turns_used,
        max_turns,
        evidence.render()
    )
}

/// Continuation prompt carrying the judge's per-criterion findings (#299).
fn build_continuation_prompt(
    goal_text: &str,
    outcome: &JudgeOutcome,
    evidence: &GoalEvidence,
    turns_used: i32,
    max_turns: i32,
) -> String {
    let mut prompt = String::new();
    prompt.push_str("[GOAL CONTINUATION — not yet verified]\n\n");
    prompt.push_str(&format!("Goal: {}\n\n", goal_text));
    prompt.push_str(&format!(
        "Verdict: {} — {}\n",
        outcome.verdict.as_str(),
        outcome.reason
    ));
    prompt.push_str(&format!("Turn {} of {}.\n", turns_used, max_turns));

    prompt.push_str("\nCriteria still unproven:\n");
    let unproven: Vec<&CriterionEvaluation> = outcome
        .criteria
        .iter()
        .filter(|c| c.status != CriterionStatus::Met)
        .collect();
    if unproven.is_empty() {
        prompt.push_str(
            "- (none recorded) — the judge returned no per-criterion evaluation. Address each \
             declared criterion explicitly and attach a receipt for it.\n",
        );
    } else {
        for c in &unproven {
            let id = if c.id.is_empty() {
                "?".to_string()
            } else {
                c.id.clone()
            };
            prompt.push_str(&format!(
                "- [{}] {} — {}\n",
                id,
                c.criterion,
                status_label(c.status)
            ));
        }
    }

    prompt.push_str("\nEvidence for this turn:\n");
    prompt.push_str(&evidence.render());
    prompt.push('\n');

    if let Some(ref corrections) = outcome.corrections {
        prompt.push_str(&format!("\nJudge guidance: {}\n", corrections));
    }

    prompt.push_str(
        "\nContinue working toward the goal. For each unproven criterion produce a checkable \
         receipt — a command result, a file on disk, a live probe — rather than a restatement of \
         prior work. The goal will be re-evaluated after your response.",
    );
    prompt
}

/// Short label for a criterion status, as shown in the continuation prompt.
fn status_label(status: CriterionStatus) -> &'static str {
    match status {
        CriterionStatus::Met => "MET",
        CriterionStatus::Unmet => "UNMET",
        CriterionStatus::NoEvidence => "NO EVIDENCE",
    }
}

/// Compact `c1 UNMET, c2 NO EVIDENCE` rendering of the unproven criteria.
fn unproven_list(criteria: &[CriterionEvaluation]) -> String {
    let items: Vec<String> = criteria
        .iter()
        .filter(|c| c.status != CriterionStatus::Met)
        .map(|c| {
            let id = if c.id.is_empty() { "?" } else { c.id.as_str() };
            format!("{} {}", id, status_label(c.status))
        })
        .collect();
    if items.is_empty() {
        "(none recorded)".to_string()
    } else {
        items.join(", ")
    }
}
