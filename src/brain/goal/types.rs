//! Goal state types for autonomous task completion.

use serde::{Deserialize, Serialize};

/// The aggregated verdict on whether a goal is satisfied (#299).
///
/// Three values, not two. `Uncertain` exists so the judge is never forced to
/// guess between "done" and "not done" when the evidence is simply absent —
/// the state the old binary `DONE`/`CONTINUE` vocabulary had no room for.
///
/// The authoritative verdict is computed in Rust by [`aggregate_verdict`] over
/// the per-criterion evaluations; the judge model's own holistic `verdict`
/// field is advisory and only logged when it disagrees.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "UPPERCASE")]
pub enum GoalVerdict {
    /// Every declared criterion was met by evidence.
    Verified,
    /// No criterion was contradicted, but at least one lacks evidence — or the
    /// goal declared no criteria at all. The loop keeps working and demands
    /// evidence rather than terminating.
    Uncertain,
    /// At least one declared criterion was contradicted by evidence.
    Rejected,
}

impl GoalVerdict {
    /// The wire token: used for the `goal_state.judge_verdict` column and in
    /// the judge's own JSON payload.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Verified => "VERIFIED",
            Self::Uncertain => "UNCERTAIN",
            Self::Rejected => "REJECTED",
        }
    }

    /// Parse a verdict token, tolerating legacy and malformed input.
    ///
    /// `"DONE"` maps to [`GoalVerdict::Verified`] so `goal_state.judge_verdict`
    /// rows written before #299 keep their meaning on display. Every unknown
    /// token falls to [`GoalVerdict::Uncertain`] — never to `Verified`: an
    /// unparseable verdict must not be able to end a goal.
    pub fn from_str_lossy(s: &str) -> Self {
        match s.trim().to_uppercase().as_str() {
            "VERIFIED" | "DONE" => Self::Verified,
            "REJECTED" => Self::Rejected,
            _ => Self::Uncertain,
        }
    }
}

/// How one declared criterion fared against the evidence (#299).
///
/// `Default` is [`CriterionStatus::NoEvidence`] on purpose: every path that
/// has to invent a status invents the one that cannot verify a goal.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum CriterionStatus {
    /// Evidence shows the criterion holds.
    Met,
    /// Evidence shows the criterion does not hold — work is still outstanding
    /// or was done wrong.
    Unmet,
    /// The response neither proves nor disproves it. The honest answer when
    /// the judge cannot tell, and the reason `Uncertain` exists.
    #[default]
    NoEvidence,
}

impl CriterionStatus {
    /// The wire token, as the judge is asked to emit it.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Met => "MET",
            Self::Unmet => "UNMET",
            Self::NoEvidence => "NO_EVIDENCE",
        }
    }

    /// Parse a status token, tolerating casing and separators.
    ///
    /// Models drift between `MET`, `met`, `Met`, and `NO EVIDENCE`. Every form
    /// that is recognisably "met" must not silently become "no evidence", and
    /// — more importantly — nothing unrecognised may become `Met`. Unknown
    /// input falls to [`CriterionStatus::NoEvidence`].
    pub fn from_str_lossy(s: &str) -> Self {
        let norm: String = s
            .trim()
            .to_uppercase()
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
            .collect();
        match norm.trim_matches('_') {
            "MET" | "YES" | "SATISFIED" | "PASS" | "PASSED" => Self::Met,
            "UNMET" | "NO" | "NOT_MET" | "FAIL" | "FAILED" | "NOT_SATISFIED" => Self::Unmet,
            _ => Self::NoEvidence,
        }
    }
}

/// One criterion's evaluation by the judge (#299).
///
/// Every field carries a serde default: a judge reply that omits or misnames a
/// field degrades to an empty/unevidenced entry instead of failing the whole
/// parse, and an unevidenced entry can never aggregate to `Verified`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CriterionEvaluation {
    /// Stable criterion id (`c1`, `c2`, …) so the judge and the continuation
    /// prompt can name the same criterion.
    #[serde(default)]
    pub id: String,
    /// The criterion text, echoed back for display.
    #[serde(default)]
    pub criterion: String,
    /// Absent/garbled status degrades to `NO_EVIDENCE`.
    #[serde(default, deserialize_with = "de_status_lossy")]
    pub status: CriterionStatus,
    /// The quote or receipt that justifies the status. Free text, display-only.
    #[serde(default)]
    pub evidence: String,
}

/// Deserialize a criterion `status` token leniently (see
/// [`CriterionStatus::from_str_lossy`]). Any non-string shape degrades to
/// `NO_EVIDENCE` rather than failing the entry.
fn de_status_lossy<'de, D>(deserializer: D) -> Result<CriterionStatus, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(match serde_json::Value::deserialize(deserializer)? {
        serde_json::Value::String(s) => CriterionStatus::from_str_lossy(&s),
        _ => CriterionStatus::NoEvidence,
    })
}

/// The judge's structured response after evaluating goal progress.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JudgeDecision {
    /// The model's own holistic verdict — **advisory only**. The authoritative
    /// verdict is [`aggregate_verdict`] over `criteria`; a disagreement is
    /// logged as `goal_judge_verdict_mismatch`. Defaults to `Uncertain` when
    /// the model omits it, so a partial reply cannot verify a goal.
    #[serde(default = "default_verdict", deserialize_with = "de_verdict_lossy")]
    pub verdict: GoalVerdict,
    pub reason: String,
    /// Optional corrections or guidance for the next continuation.
    #[serde(default)]
    pub corrections: Option<String>,
    /// Per-criterion evaluations, emitted by the judge before its aggregate.
    #[serde(default, deserialize_with = "de_criteria_lossy")]
    pub criteria: Vec<CriterionEvaluation>,
}

fn default_verdict() -> GoalVerdict {
    GoalVerdict::Uncertain
}

/// Deserialize the judge's `verdict` token leniently.
///
/// A `verdict` field that is absent, `null`, a number, or an unknown token must
/// NOT fail the whole reply: the reply still carries the per-criterion
/// evaluations, and discarding them over one bad token would throw away the
/// only evidence the judge produced. Every unusable shape degrades to
/// [`GoalVerdict::Uncertain`], which is advisory anyway.
fn de_verdict_lossy<'de, D>(deserializer: D) -> Result<GoalVerdict, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(match serde_json::Value::deserialize(deserializer)? {
        serde_json::Value::String(s) => GoalVerdict::from_str_lossy(&s),
        _ => GoalVerdict::Uncertain,
    })
}

/// Deserialize the judge's `criteria` array leniently.
///
/// One malformed entry must not invalidate its siblings: each element is
/// converted independently, and a conversion failure becomes
/// [`CriterionEvaluation::default`] — `NO_EVIDENCE`, never `MET`. Dropping the
/// element outright would be worse: the count would shrink below `declared` and
/// `aggregate_verdict` would read a *partial* reply as complete.
fn de_criteria_lossy<'de, D>(deserializer: D) -> Result<Vec<CriterionEvaluation>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = Vec::<serde_json::Value>::deserialize(deserializer)?;
    Ok(raw
        .into_iter()
        .map(|v| serde_json::from_value(v).unwrap_or_default())
        .collect())
}

impl JudgeDecision {
    /// Parse from raw JSON string. Fail-open: on parse error, return an
    /// `Uncertain` decision so the loop keeps going rather than dying — and so
    /// a malformed judge reply can never end a goal.
    pub fn parse_or_continue(raw: &str) -> Self {
        match serde_json::from_str::<JudgeDecision>(raw) {
            Ok(decision) => decision,
            Err(e) => {
                tracing::warn!(
                    "Goal judge returned unparseable JSON: {} — \
                     defaulting to UNCERTAIN (fail-open). Raw: {}",
                    e,
                    // Byte-offset slicing panics when the cut lands inside a
                    // multi-byte codepoint, and an unparseable judge reply is
                    // exactly where non-ASCII arrives. `truncate_str` snaps the
                    // cut back to a char boundary (same helper the channel
                    // handlers use for previews).
                    crate::utils::truncate_str(raw, 200)
                );
                JudgeDecision {
                    verdict: GoalVerdict::Uncertain,
                    reason: format!("judge parse error: {}", e),
                    corrections: None,
                    criteria: Vec::new(),
                }
            }
        }
    }
}

/// Aggregate per-criterion evaluations into the goal verdict (#299).
///
/// Pure and total: no I/O, no model, no clock. The precedence is deliberate —
/// a contradiction outranks missing evidence, and missing evidence outranks
/// success, so **one** unevidenced criterion is enough to withhold `Verified`.
///
/// `declared` is the number of criteria the goal actually declared. When the
/// judge evaluated fewer than were declared (a truncated or partial reply) the
/// unevaluated remainder counts as `NO_EVIDENCE`; a short reply must not be
/// able to verify a goal. `declared == 0` is the empty-criteria case: with
/// nothing to prove, the cap is `Uncertain` — "no criteria" is never a licence
/// to terminate.
pub fn aggregate_verdict(criteria: &[CriterionEvaluation], declared: usize) -> GoalVerdict {
    if declared == 0 {
        return GoalVerdict::Uncertain;
    }
    if criteria.iter().any(|c| c.status == CriterionStatus::Unmet) {
        return GoalVerdict::Rejected;
    }
    if criteria.len() < declared {
        return GoalVerdict::Uncertain;
    }
    if criteria.iter().all(|c| c.status == CriterionStatus::Met) {
        return GoalVerdict::Verified;
    }
    GoalVerdict::Uncertain
}

/// Decision from the goal manager after evaluating a turn.
#[derive(Debug, Clone)]
pub enum GoalDecision {
    /// Goal is satisfied, turn should complete normally.
    Done { reason: String },
    /// Goal needs more work, inject continuation and re-enter loop.
    Continue {
        continuation_prompt: String,
        corrections: Option<String>,
    },
    /// Goal budget exhausted, evidence budget exhausted, or auto-paused due to
    /// parse failures.
    Paused { reason: String },
    /// The goal is waiting on harness-visible work it cannot advance (#567).
    ///
    /// The turn ends here and the goal stays `state='active'`. The turn budget
    /// is deliberately NOT consumed: no goal work occurred, and billing the
    /// turn is what converted a wait into *"budget exhausted while waiting"* —
    /// the #567 symptom itself.
    ///
    /// Reachable ONLY when the sole mechanical blocker is a running background
    /// task AND no plan task is open. A hold on open plan tasks is work the
    /// agent *can* advance, so it keeps the #299 re-prompt.
    ///
    /// **#299 invariant, preserved:** this is never a path to `Done`. A running
    /// process still outranks any completion claim, and a goal cannot be marked
    /// done while a task it started is alive. The deferral delays the judgement
    /// until the harness can see the work finish; it does not skip it.
    Deferred { reason: String, wake_ref: String },
}

/// The `await_kind` a deferred goal registers on its session binding (#567).
///
/// Written by the goal manager on the deferral path and read by the two
/// existing wait readers — the boot classifier and the await sweep — so a
/// deferral whose completion wake is lost still has a backstop (#344).
pub const DEFERRED_AWAIT_KIND: &str = "background_task";

/// Maximum consecutive judge parse failures before auto-pause.
/// Protects against models that can't produce valid JSON.
pub const MAX_PARSE_FAILURES: u32 = 3;

/// Maximum consecutive `Uncertain` verdicts before the goal parks (#299).
///
/// `Uncertain` means "no evidence either way". A loop that keeps returning it
/// is not converging — it is burning turns against a goal nothing can prove —
/// so after this many in a row the goal pauses and names the missing evidence
/// instead of spinning until the turn budget runs out.
pub const MAX_CONSECUTIVE_UNCERTAIN: u32 = 3;

/// Default maximum turns for a goal.
pub const DEFAULT_MAX_TURNS: u32 = 20;
