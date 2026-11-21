//! #929 — the compaction prompt/eval contract gate.
//!
//! The compaction eval used to grade a prompt nobody ran: `produce.rs` carried a
//! self-contained 5-section shadow prompt while production served a 10-section
//! one. Killing that drift (#929) leaves a second question — how does a gate
//! notice when the SHARED prompt loses a section?
//!
//! It cannot be noticed by scoring model output. `keyword_scorecard` grades a
//! SUMMARY, and a live model's summary is stochastic, so a score over it is not
//! deterministic and cannot gate a merge. The gate is therefore a CONTRACT test
//! between the prompt text and the fact dimensions the eval claims to measure —
//! two legs, both offline:
//!
//! - **Structural (leg A)** — every dimension the dataset probes must still be
//!   served by a section of the live prompt. [`DIMENSION_SECTIONS`] is that
//!   contract. A prompt that drops `## 6. Pending Tasks` fails with
//!   `pending_tasks` named, which is the whole point: the failure names the
//!   dimension that lost its instruction.
//! - **Discrimination (leg B)** — a committed faithful summary
//!   (`fixtures/compaction-reference-summary.md`) must still score full marks.
//!   This is the guard against the scorer rotting underneath the gate: a scorer
//!   that passes everything, or nothing, would make leg A vacuous.
//!
//! Neither leg measures prompt QUALITY. The seed is four probes; it is a smoke
//! gate, not a quality metric. Real coverage needs a dataset built from actual
//! compaction summaries, which is named as follow-up rather than claimed here.

use std::collections::BTreeMap;

use super::compaction::CompactionDataset;
use super::scorer::{BinaryQuestion, BinaryVerdict, Scorecard};

/// The contract: fact dimension -> the prompt heading that must carry its
/// instruction.
///
/// The headings are the production prompt's own section titles. They are DATA
/// here, not derived, because the point is that a rename in the prompt shows up
/// as a diff in the gate — a derived map would follow the rename silently and
/// the gate would pass while the dimension lost its instruction.
///
/// Add a row when the dataset gains a dimension; the gate fails until the
/// prompt serves it, which is the intended coupling.
pub const DIMENSION_SECTIONS: &[(&str, &str)] = &[
    ("user_prefs", "## 3. User Preferences & Constraints"),
    ("pending_tasks", "## 6. Pending Tasks"),
    ("files_modified", "## 2. Files Modified"),
    ("errors", "## 4. Errors & Corrections"),
];

/// The committed faithful summary, graded by leg B.
pub const REFERENCE_SUMMARY: &str = include_str!("fixtures/compaction-reference-summary.md");

/// One dimension's verdict on whether the prompt still serves it.
fn structural_verdicts(
    dataset: &CompactionDataset,
    prompt_body: &str,
) -> Vec<(BinaryQuestion, BinaryVerdict)> {
    let probed: Vec<&str> = dataset
        .probes
        .iter()
        .map(|p| p.dimension.as_str())
        .collect();

    DIMENSION_SECTIONS
        .iter()
        .filter(|(dim, _)| probed.contains(dim))
        .map(|(dim, heading)| {
            let present = prompt_body.contains(*heading);
            let explanation = if present {
                None
            } else {
                Some(format!(
                    "the live compaction prompt no longer contains the section heading {heading:?}, \
                     so the {dim} dimension has no instruction and its probes measure nothing"
                ))
            };
            (
                BinaryQuestion::new(
                    *dim,
                    format!("Does the live compaction prompt still serve the {dim} dimension?"),
                ),
                BinaryVerdict {
                    yes: present,
                    explanation,
                },
            )
        })
        .collect()
}

/// A dimension the dataset probes but the contract does not map. Reported rather
/// than silently skipped: an unmapped dimension would make leg A vacuous for it.
fn unmapped_dimensions(dataset: &CompactionDataset) -> Vec<(BinaryQuestion, BinaryVerdict)> {
    let mapped: Vec<&str> = DIMENSION_SECTIONS.iter().map(|(d, _)| *d).collect();
    dataset
        .probes
        .iter()
        .map(|p| p.dimension.as_str())
        .filter(|d| !mapped.contains(d))
        .map(|d| {
            (
                BinaryQuestion::new(
                    d,
                    format!(
                        "Is the {d} dimension mapped to a prompt section in DIMENSION_SECTIONS?"
                    ),
                ),
                BinaryVerdict {
                    yes: false,
                    explanation: Some(format!(
                        "the dataset probes {d} but DIMENSION_SECTIONS has no row for it, so no \
                         prompt section is checked for that dimension"
                    )),
                },
            )
        })
        .collect()
}

/// Both legs, plus the merged scorecard the gate and the CLI report.
pub struct GateOutcome {
    /// Leg A — does the live prompt still serve every probed dimension?
    pub structural: Scorecard,
    /// Leg B — does a known-faithful summary still score full marks?
    pub discrimination: Scorecard,
    /// The two legs merged: a dimension passes only if it passed BOTH.
    pub overall: Scorecard,
}

impl GateOutcome {
    pub fn holds(&self) -> bool {
        self.overall.overall() >= 1.0
    }

    /// The dimensions that failed, each with its explanation — what the gate
    /// names when it goes red.
    pub fn failures(&self) -> Vec<(String, String)> {
        self.overall
            .results
            .iter()
            .filter(|(_, v)| !v.yes)
            .map(|(q, v)| {
                (
                    q.dimension.clone(),
                    v.explanation
                        .clone()
                        .unwrap_or_else(|| "failed".to_string()),
                )
            })
            .collect()
    }
}

/// Run both legs. `prompt_body` is the LIVE compaction prompt body (loaded
/// through `prompt_file`, so an override file is gated too — not just the
/// compiled default). `reference_summary` is what leg B grades; pass
/// [`REFERENCE_SUMMARY`] for the gate, or a real summary for an operator run.
pub fn run(dataset: &CompactionDataset, prompt_body: &str, reference_summary: &str) -> GateOutcome {
    let mut structural_results = structural_verdicts(dataset, prompt_body);
    structural_results.extend(unmapped_dimensions(dataset));
    let structural = Scorecard::from_verdicts(structural_results);

    let discrimination = dataset.keyword_scorecard(reference_summary);

    // Merge: union of dimensions, passing only where BOTH legs passed. Building
    // it from the legs rather than re-deriving keeps the reported explanations.
    let mut merged: BTreeMap<String, (BinaryQuestion, BinaryVerdict)> = BTreeMap::new();
    for card in [&structural, &discrimination] {
        for (q, v) in &card.results {
            let entry = merged.entry(q.dimension.clone()).or_insert_with(|| {
                (
                    BinaryQuestion::new(q.dimension.clone(), q.text.clone()),
                    BinaryVerdict {
                        yes: true,
                        explanation: None,
                    },
                )
            });
            if !v.yes {
                entry.1.yes = false;
                if entry.1.explanation.is_none() {
                    entry.1.explanation = v.explanation.clone();
                }
            }
        }
    }
    let overall = Scorecard::from_verdicts(merged.into_values().collect());

    GateOutcome {
        structural,
        discrimination,
        overall,
    }
}
