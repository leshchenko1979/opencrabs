//! Goal judge — evaluates whether a goal is satisfied after each turn.
//!
//! #299 changed what this module is allowed to conclude. The judge no longer
//! emits a holistic yes/no: it evaluates each **declared criterion** against a
//! **mechanical evidence pack**, and the authoritative verdict is aggregated in
//! Rust by [`aggregate_verdict`]. The model's own holistic verdict is advisory
//! and only logged when it disagrees.
//!
//! Two failure modes motivate the shape, both observed in production:
//!
//! 1. **Sycophancy.** The retired prompt told the judge to give the assistant
//!    credit whenever its output merely *addressed* the goal. A turn that only
//!    *described* work in progress therefore read as addressing the goal, and a
//!    status report ended a goal mid-flight.
//! 2. **Overconfidence.** Asked for a binary verdict, a model will pick one
//!    even when the evidence is absent. `UNCERTAIN` gives it the honest option,
//!    and the aggregate prefers it over `VERIFIED`.

use crate::brain::goal::types::{
    CriterionEvaluation, GoalVerdict, JudgeDecision, aggregate_verdict,
};
use crate::brain::provider::{LLMRequest, Message, Provider};

/// The judge system prompt.
///
/// Hardened for #299: the "be generous" licence is gone, verbosity is
/// explicitly not evidence, and the per-criterion array is required *before*
/// the aggregate verdict so the model cannot rationalise a verdict it has
/// already committed to.
pub(crate) const JUDGE_SYSTEM: &str = r#"You are a goal-evaluation judge. You evaluate DECLARED CRITERIA against EVIDENCE. You do not decide whether the work "feels" done.

You will receive:
- The GOAL: what the user wants accomplished
- The CRITERIA: the numbered, checkable criteria the goal must satisfy
- The EVIDENCE: mechanical facts collected from the session, in five sections — RUNNING BACKGROUND TASKS, OPEN PLAN TASKS, TOOL RECEIPTS (one line per tool call this turn, marked "ok" or "FAILED"), FILES TOUCHED, and TURN BUDGET. The pack covers THIS TURN ONLY: an absent receipt is not evidence the work never happened, and it is never a licence to guess MET.
- The LAST RESPONSE: the assistant's most recent output

Respond with ONLY a JSON object (no markdown, no code fences, no extra text):
{
  "criteria": [
    {"id": "c1", "criterion": "the criterion text", "status": "MET" | "UNMET" | "NO_EVIDENCE", "evidence": "the specific receipt that justifies this status"}
  ],
  "verdict": "VERIFIED" | "UNCERTAIN" | "REJECTED",
  "reason": "brief explanation of the aggregate verdict",
  "corrections": "optional guidance for what the assistant should do next (only when not VERIFIED)"
}

Emit the "criteria" array FIRST, one entry per declared criterion, using the SAME ids you were given. Then the aggregate "verdict". The aggregate is checked against your per-criterion entries and disagreement is recorded.

Rules for status:
- MET: the EVIDENCE shows the criterion holds. The receipt must be a real, observable fact — a tool result, an exit code, a file that exists, a test that ran. Name it in "evidence".
- UNMET: the EVIDENCE shows the criterion does not hold, or shows the work was done incorrectly. The receipt goes in "evidence".
- NO_EVIDENCE: neither the evidence nor the last response proves or disproves it. This is the correct answer when you cannot tell. Do NOT guess MET.

Rules for the aggregate verdict:
- VERIFIED: every criterion is MET.
- REJECTED: at least one criterion is UNMET.
- UNCERTAIN: no criterion is UNMET, but at least one is NO_EVIDENCE.

Anti-verbosity rule — this is the rule that matters most:
- Prose is NOT evidence. A status report, a plan, a summary of what was done, a review of the work, a description of intent, or a statement that the task "is complete" does NOT satisfy an ACTION criterion. Action criteria are satisfied by executed actions with observable receipts.
- The assistant saying it will do something is not doing it. Stated intent is NO_EVIDENCE at best.
- An in-progress state is never MET. If the assistant reports it is still working, still merging, still waiting, still mid-task, then any criterion depending on that work being finished is UNMET or NO_EVIDENCE — never MET.
- Do not infer success from a confident tone. A confident claim without a receipt is NO_EVIDENCE.

Other rules:
- If the assistant says it cannot do something and explains the block, that does not make the criteria MET — evaluate each criterion on the evidence you have.
- The evidence pack is authoritative over the last response where they disagree: it records what actually happened.
- Return ONLY the JSON object. Nothing else."#;

/// The judge's conclusion: the model's per-criterion evaluation, plus the
/// authoritative aggregate computed in Rust.
#[derive(Debug, Clone)]
pub struct JudgeOutcome {
    /// The model's own holistic verdict — advisory only, never authoritative.
    pub model_verdict: GoalVerdict,
    /// The authoritative verdict: [`aggregate_verdict`] over `criteria`.
    pub verdict: GoalVerdict,
    /// Human-readable reason for the aggregate verdict.
    pub reason: String,
    /// Optional guidance for the next continuation.
    pub corrections: Option<String>,
    /// Per-criterion evaluations as parsed from the judge reply.
    pub criteria: Vec<CriterionEvaluation>,
    /// True when the judge reply could not be parsed at all. The caller uses
    /// this to drive the parse-failure auto-pause.
    pub parse_failed: bool,
}

impl JudgeOutcome {
    /// A fail-open outcome: `UNCERTAIN`, no criteria, reason attached.
    ///
    /// Every error path in this module funnels here. `UNCERTAIN` keeps the loop
    /// alive and asking for evidence; it can never end a goal.
    fn uncertain(reason: String, parse_failed: bool) -> Self {
        Self {
            model_verdict: GoalVerdict::Uncertain,
            verdict: GoalVerdict::Uncertain,
            reason,
            corrections: None,
            criteria: Vec::new(),
            parse_failed,
        }
    }
}

/// Run the goal judge for one turn.
///
/// `criteria` are the goal's declared criteria (empty when the goal declared
/// none, which caps the aggregate at `UNCERTAIN`). `evidence_pack` is the
/// rendered mechanical evidence collected for this turn — see
/// [`super::evidence`]. `last_response` is the assistant's own output, which is
/// deliberately the *weakest* input: the judge is told the evidence pack
/// outranks it.
///
/// Retries once on parse failure (empty response or unparseable JSON) before
/// falling back to a fail-open `UNCERTAIN`. This handles transient issues like
/// reasoning models burning all tokens on CoT.
pub async fn judge_goal(
    provider: &dyn Provider,
    model: &str,
    goal: &str,
    criteria: &[String],
    evidence_pack: &str,
    last_response: &str,
) -> JudgeOutcome {
    // Truncate last_response to avoid blowing the judge's context window.
    // The last 4k bytes is usually enough to determine completion.
    //
    // The cut MUST land on a UTF-8 char boundary. `len()` is bytes, and the
    // accumulated assistant text routinely carries 3-4 byte codepoints
    // (emoji, `→`, Cyrillic), so a raw byte offset panics with
    // "byte index N is not a char boundary". `ceil_char_boundary` snaps the
    // start FORWARD to the next boundary (std; total — clamps at `len`), the
    // same treatment `utils::sanitize` applies to its redaction cursor.
    let truncated_response = if last_response.len() > 4000 {
        &last_response[last_response.ceil_char_boundary(last_response.len() - 4000)..]
    } else {
        last_response
    };

    let user_prompt = build_user_prompt(goal, criteria, evidence_pack, truncated_response);

    // Try up to 2 times: original call + one retry on parse/empty failure.
    for attempt in 1..=2 {
        let request = LLMRequest::new(model.to_string(), vec![Message::user(user_prompt.clone())])
            .with_system(JUDGE_SYSTEM.to_string())
            .with_max_tokens(4096);

        match provider.complete(request).await {
            Ok(response) => {
                let raw = super::extract_text(&response);
                if raw.trim().is_empty() {
                    tracing::warn!("Goal judge returned empty response (attempt {}/2)", attempt);
                    if attempt < 2 {
                        continue;
                    }
                    return JudgeOutcome::uncertain(
                        "judge returned empty response".to_string(),
                        true,
                    );
                }

                let decision = JudgeDecision::parse_or_continue(&raw);
                let parse_failed = decision.reason.starts_with("judge parse error");

                // Retry once on parse error (unparseable JSON)
                if parse_failed {
                    tracing::warn!(
                        "Goal judge parse failed (attempt {}/2): {}",
                        attempt,
                        decision.reason
                    );
                    if attempt < 2 {
                        continue;
                    }
                }

                return finalize(decision, criteria.len(), parse_failed);
            }
            Err(e) => {
                tracing::warn!("Goal judge LLM call failed (attempt {}/2): {}", attempt, e);
                if attempt < 2 {
                    continue;
                }
                return JudgeOutcome::uncertain(format!("judge call error: {}", e), true);
            }
        }
    }

    // Unreachable — the loop always returns. Satisfies the compiler.
    JudgeOutcome::uncertain("judge unreachable".to_string(), true)
}

/// Compute the authoritative verdict from a parsed judge decision.
///
/// Split out from the network path so it is directly testable: given a
/// decision and the number of declared criteria, it produces the outcome the
/// manager acts on. The mismatch log lives here too — the model's holistic
/// verdict is recorded when it disagrees with the aggregate, but never
/// consulted.
fn finalize(decision: JudgeDecision, declared: usize, parse_failed: bool) -> JudgeOutcome {
    let verdict = aggregate_verdict(&decision.criteria, declared);

    if !parse_failed && decision.verdict != verdict {
        tracing::info!(
            target: "goal_judge_verdict_mismatch",
            model_verdict = decision.verdict.as_str(),
            aggregate_verdict = verdict.as_str(),
            declared,
            evaluated = decision.criteria.len(),
            "goal_judge_verdict_mismatch: judge's holistic verdict disagrees with the \
             per-criterion aggregate; the aggregate is authoritative"
        );
    }

    tracing::info!(
        "Goal judge verdict: {:?} (declared={}, evaluated={}) — {}",
        verdict,
        declared,
        decision.criteria.len(),
        decision.reason
    );

    JudgeOutcome {
        model_verdict: decision.verdict,
        verdict,
        reason: decision.reason,
        corrections: decision.corrections,
        criteria: decision.criteria,
        parse_failed,
    }
}

/// Assemble the judge's user prompt: goal, declared criteria, evidence, response.
fn build_user_prompt(
    goal: &str,
    criteria: &[String],
    evidence_pack: &str,
    truncated_response: &str,
) -> String {
    let criteria_block = if criteria.is_empty() {
        "(none declared — the aggregate verdict is capped at UNCERTAIN)".to_string()
    } else {
        criteria
            .iter()
            .enumerate()
            .map(|(i, c)| format!("c{}: {}", i + 1, c))
            .collect::<Vec<_>>()
            .join("\n")
    };

    format!(
        "GOAL:\n{}\n\nCRITERIA:\n{}\n\nEVIDENCE:\n{}\n\nLAST RESPONSE:\n{}",
        goal, criteria_block, evidence_pack, truncated_response
    )
}
