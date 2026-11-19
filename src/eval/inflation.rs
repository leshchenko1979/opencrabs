//! Query/label inflation guard for retrieval datasets (#890).
//!
//! A dataset query generated from its own target document scores high for a
//! reason that has nothing to do with retrieval quality: lexical ranking
//! rewards the query that already contains the answer's words. The failure is
//! invisible in the scorecard — the numbers simply look good — which is why it
//! has to be a mechanical gate rather than a discipline the author remembers.
//!
//! Two rules, and only the second is code:
//!
//! 1. Queries are written from a SCENARIO ("a repo needs CI to skip the test leg
//!    on docs-only changes"), never by summarising the target document.
//! 2. [`check`] rejects any query sharing at least [`INFLATION_THRESHOLD`] of its
//!    own content tokens with the text of the section it labels. That overlap is
//!    the FTS-inflation signature: the query is quoting its answer.
//!
//! Content tokens are length-filtered and stop-worded because the ratio is the
//! measurement. Counting `the` and `with` would dilute the ratio in both
//! directions — a verbatim quote of a section full of stop words would look
//! clean, and a legitimately scenario-phrased query would look dirty.

use std::collections::BTreeSet;

use crate::brain::brain_sections::tokens;

/// Share of a query's content tokens that may also appear in its target.
///
/// Calibrated on the committed Track B set: scenario-phrased queries land
/// under 0.45, quoted-sentence queries land over 0.85, and nothing in the
/// corpus sat between 0.45 and 0.60. 0.60 is the middle of that empty band,
/// so the threshold is not a tuning knob sitting on top of live data.
pub const INFLATION_THRESHOLD: f64 = 0.60;

/// Words that carry no subject. Kept short deliberately: an aggressive list
/// starts removing the technical vocabulary that makes a query specific.
const STOP_WORDS: &[&str] = &[
    "about", "after", "again", "also", "and", "any", "are", "because", "been", "before", "being",
    "between", "both", "but", "can", "could", "did", "does", "doing", "down", "during", "each",
    "for", "from", "had", "has", "have", "here", "how", "into", "its", "itself", "just", "like",
    "made", "make", "many", "may", "more", "most", "much", "must", "not", "now", "off", "once",
    "only", "other", "our", "out", "over", "own", "same", "should", "some", "still", "such",
    "than", "that", "the", "their", "them", "then", "there", "these", "they", "this", "those",
    "through", "too", "under", "until", "very", "was", "were", "what", "when", "where", "which",
    "while", "who", "why", "will", "with", "would", "you", "your",
];

/// The subject-bearing tokens of a piece of text, deduplicated and sorted.
pub fn content_tokens(text: &str) -> BTreeSet<String> {
    tokens(text)
        .filter(|t| t.chars().count() >= 4)
        .filter(|t| !STOP_WORDS.contains(&t.as_str()))
        .collect()
}

/// What the guard found for one query/label pair.
#[derive(Debug, Clone, PartialEq)]
pub struct InflationReport {
    /// Shared content tokens divided by the query's own content tokens.
    pub overlap: f64,
    /// Whether `overlap` reached the threshold, i.e. the query quotes its target.
    pub inflated: bool,
    /// The shared tokens, sorted — so a rejection says WHICH words leaked.
    pub shared: Vec<String>,
}

impl InflationReport {
    /// A one-line explanation, for a test failure or a review note.
    pub fn describe(&self, query: &str, threshold: f64) -> String {
        format!(
            "{:.2} overlap (limit {:.2}) for {query:?}; shared: {}",
            self.overlap,
            threshold,
            if self.shared.is_empty() {
                "none".to_string()
            } else {
                self.shared.join(", ")
            }
        )
    }
}

/// Measure how much of `query` is already present in `target`.
///
/// A query with no content tokens scores 0.0 and is never inflated: an
/// all-stop-word query is a bad label for other reasons, and this guard is not
/// the place that judges it.
pub fn check(query: &str, target: &str, threshold: f64) -> InflationReport {
    let query_tokens = content_tokens(query);
    let target_tokens = content_tokens(target);
    if query_tokens.is_empty() {
        return InflationReport {
            overlap: 0.0,
            inflated: false,
            shared: Vec::new(),
        };
    }
    let shared: Vec<String> = query_tokens.intersection(&target_tokens).cloned().collect();
    let overlap = shared.len() as f64 / query_tokens.len() as f64;
    InflationReport {
        overlap,
        inflated: overlap >= threshold,
        shared,
    }
}
