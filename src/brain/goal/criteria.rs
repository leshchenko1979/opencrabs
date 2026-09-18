//! Criteria derivation (#299) — turn a free-form goal into checkable criteria.
//!
//! The judge is only ever as good as the criteria it is handed. A goal stated
//! as prose ("finish the upstream merge process") contains no checkable parts,
//! so the judge is left to invent them — and inventing them is exactly where
//! the retired prompt's sycophancy lived. This module asks the model for the
//! criteria ONCE, at goal-creation time; the goal stores them and every later
//! judge call evaluates that fixed list instead of re-imagining the goal.
//!
//! Fail-open in the same direction as the rest of the subsystem: every failure
//! path yields "no derived criteria" rather than an error. A goal that ends up
//! with no criteria can only ever aggregate to `UNCERTAIN`, which keeps the
//! loop asking for evidence instead of terminating.

use crate::brain::provider::{LLMRequest, Message, Provider};

/// Upper bound on derived criteria.
///
/// Five keeps a goal's outcome pinned without turning every judge reply into a
/// wall of per-criterion entries. A model that returns more is truncated.
pub const MAX_CRITERIA: usize = 5;

/// The derivation system prompt.
///
/// The load-bearing rule is the definition of *checkable*: a criterion is
/// checkable only when a third party could confirm it from observable
/// evidence. Intent, effort and quality are not checkable, and a criterion
/// built from them can only ever come back `NO_EVIDENCE`.
const CRITERIA_SYSTEM: &str = r#"You convert a goal statement into a short list of CHECKABLE criteria.

A criterion is checkable when a third party could confirm it from observable evidence: a command that ran and its exit code, a file that exists, a message that was sent, a test that passed, a value that was written. It is NOT checkable if it describes intent, effort, quality, or a feeling — "work carefully", "do a good job", "understand the system", "be thorough" are all unverifiable and must never be emitted.

Rules:
- Emit between 1 and 5 criteria. Fewer is better: cover the goal's OUTCOME, not its steps.
- Each criterion is one self-contained sentence stating a result that can be observed: "X exists", "Y exits 0", "Z is pushed to main".
- Never restate the goal verbatim as a criterion.
- Never emit a criterion that is already trivially true.
- Order the criteria by importance.

Respond with ONLY a JSON object, no markdown and no commentary:
{"criteria": ["first criterion", "second criterion"]}"#;

/// Derive 1-5 checkable criteria for a free-form goal.
///
/// One auxiliary LLM call. Non-fatal and fail-open: an empty goal, a provider
/// error, an empty reply, or an unparseable reply all return an EMPTY vector
/// (logged at warn). Callers that must store *something* use
/// [`derive_criteria_if_needed`].
pub async fn derive_criteria(provider: &dyn Provider, model: &str, goal_text: &str) -> Vec<String> {
    let goal = goal_text.trim();
    if goal.is_empty() {
        tracing::warn!("Criteria derivation skipped: empty goal text");
        return Vec::new();
    }

    let request = LLMRequest::new(
        model.to_string(),
        vec![Message::user(format!("GOAL:\n{}", goal))],
    )
    .with_system(CRITERIA_SYSTEM.to_string())
    .with_max_tokens(1024);

    match provider.complete(request).await {
        Ok(response) => {
            let raw = super::extract_text(&response);
            if raw.trim().is_empty() {
                tracing::warn!("Criteria derivation returned an empty response");
                return Vec::new();
            }
            let criteria = parse_criteria_reply(&raw);
            if criteria.is_empty() {
                tracing::warn!(
                    "Criteria derivation returned no usable criteria; raw: {}",
                    crate::utils::truncate_str(&raw, 200)
                );
            }
            criteria
        }
        Err(e) => {
            tracing::warn!("Criteria derivation LLM call failed: {}", e);
            Vec::new()
        }
    }
}

/// The criteria to store for a newly created goal.
///
/// Explicit criteria win: a caller-supplied list is returned untouched and no
/// model call is made. Otherwise the goal's criteria are derived; if derivation
/// is unavailable (provider error, empty or unparseable reply) the goal text
/// itself is stored as a single criterion, so a goal is never left with zero
/// criteria — an empty list caps the aggregate at `UNCERTAIN` forever, which
/// parks the goal on the uncertain cap without ever trying to prove anything.
pub async fn derive_criteria_if_needed(
    provider: &dyn Provider,
    model: &str,
    goal_text: &str,
    existing_criteria: &[String],
) -> Vec<String> {
    let explicit = sanitize(existing_criteria.iter().map(|c| c.as_str()));
    if !explicit.is_empty() {
        return explicit;
    }

    let derived = derive_criteria(provider, model, goal_text).await;
    if !derived.is_empty() {
        return derived;
    }

    let fallback = goal_text.trim();
    if fallback.is_empty() {
        return Vec::new();
    }
    tracing::warn!("Criteria derivation unavailable; storing the goal text as a single criterion");
    vec![fallback.to_string()]
}

/// Parse the model's derivation reply into criteria.
///
/// Tolerant by design — models wrap JSON in code fences, prepend a sentence,
/// or emit an array of objects instead of strings. Every one of those shapes is
/// recognised; anything unrecognised yields an empty vector (never a panic, and
/// never a half-parsed criterion that would silently weaken the judge).
pub fn parse_criteria_reply(raw: &str) -> Vec<String> {
    let unfenced = strip_code_fences(raw);
    let candidate = json_slice(unfenced).unwrap_or(unfenced);

    let Ok(value) = serde_json::from_str::<serde_json::Value>(candidate) else {
        return Vec::new();
    };

    let items = match &value {
        serde_json::Value::Object(map) => map.get("criteria").and_then(|v| v.as_array()),
        serde_json::Value::Array(arr) => Some(arr),
        _ => None,
    };

    match items {
        Some(arr) => sanitize(arr.iter().filter_map(criterion_from_value)),
        None => Vec::new(),
    }
}

/// Pull one criterion out of a reply element.
///
/// Accepts a bare string, or an object carrying the text under `criterion`
/// (some models mirror the judge's own schema back). Anything else is skipped.
fn criterion_from_value(value: &serde_json::Value) -> Option<&str> {
    match value {
        serde_json::Value::String(s) => Some(s.as_str()),
        serde_json::Value::Object(map) => map.get("criterion").and_then(|v| v.as_str()),
        _ => None,
    }
}

/// Trim, drop blanks, and cap the list.
///
/// Shared by every entry point so the stored list has one shape regardless of
/// whether it came from the model or from the caller.
fn sanitize<'a>(criteria: impl Iterator<Item = &'a str>) -> Vec<String> {
    criteria
        .map(|c| c.trim())
        .filter(|c| !c.is_empty())
        .take(MAX_CRITERIA)
        .map(|c| c.to_string())
        .collect()
}

/// Drop a surrounding markdown code fence, if present.
fn strip_code_fences(raw: &str) -> &str {
    let trimmed = raw.trim();
    let Some(rest) = trimmed.strip_prefix("```") else {
        return trimmed;
    };
    // Skip the language tag on the fence's opening line.
    let body = match rest.find('\n') {
        Some(i) => &rest[i + 1..],
        None => rest,
    };
    match body.rfind("```") {
        Some(end) => body[..end].trim(),
        None => body.trim(),
    }
}

/// Slice out the outermost JSON value, ignoring prose around it.
///
/// Models routinely preface the object ("Here are the criteria:") or trail a
/// sentence after it; `serde_json` rejects both, so the slice is what makes the
/// reply parseable without a repair pass.
fn json_slice(text: &str) -> Option<&str> {
    let start = text.find(['{', '['])?;
    let end = text.rfind(['}', ']'])?;
    if end <= start {
        return None;
    }
    Some(text[start..=end].trim())
}

/// Store criteria as JSON text for the `goal_state.criteria` column.
///
/// Total: serialization of a `Vec<String>` cannot fail, and a hypothetical
/// failure still yields a valid empty array rather than an empty string.
pub fn serialize_criteria(criteria: &[String]) -> String {
    serde_json::to_string(criteria).unwrap_or_else(|_| "[]".to_string())
}

/// Read criteria back from the `goal_state.criteria` column.
///
/// A value that is not JSON is treated as a single hand-written criterion
/// rather than dropped: the goal declared something, and discarding the
/// declaration would silently cap the goal at `UNCERTAIN`. An empty or missing
/// column yields an empty list, which the aggregate already handles.
pub fn parse_criteria(stored: &str) -> Vec<String> {
    let trimmed = stored.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }
    match serde_json::from_str::<Vec<String>>(trimmed) {
        Ok(criteria) => sanitize(criteria.iter().map(|c| c.as_str())),
        Err(_) => {
            tracing::warn!(
                "goal_state.criteria is not JSON; treating the stored text as one criterion"
            );
            vec![trimmed.to_string()]
        }
    }
}
