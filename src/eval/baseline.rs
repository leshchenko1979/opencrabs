//! Regression baseline for scorecards (#624).
//!
//! Captures an eval run's scores into a [`Baseline`] (overall + per-dimension)
//! that can be serialized, stored, and compared against a later run. A score
//! that drops beyond a tolerance is reported as a [`Regression`], so a change
//! that quietly reduces fidelity or recall is caught rather than silently
//! accepted.

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use super::scorer::Scorecard;

/// Label used for the overall (whole-run) score in comparison output.
pub const OVERALL_KEY: &str = "overall";

/// A stored snapshot of an eval run's scores.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Baseline {
    pub label: String,
    pub overall: f64,
    pub dimensions: BTreeMap<String, f64>,
}

/// One dimension whose score dropped beyond tolerance.
#[derive(Debug, Clone, PartialEq)]
pub struct Regression {
    pub dimension: String,
    pub baseline: f64,
    pub current: f64,
    /// current - baseline (negative for a regression).
    pub delta: f64,
}

impl Baseline {
    /// Capture a scorecard's overall + per-dimension fractions.
    pub fn from_scorecard(label: impl Into<String>, card: &Scorecard) -> Self {
        let dimensions = card
            .per_dimension
            .iter()
            .map(|(k, v)| (k.clone(), v.fraction()))
            .collect();
        Self {
            label: label.into(),
            overall: card.overall(),
            dimensions,
        }
    }

    /// Capture the mean scores across several runs of the same eval — the
    /// stable signal to baseline when a live eval is non-deterministic.
    pub fn from_scorecards_mean(label: impl Into<String>, cards: &[Scorecard]) -> Self {
        if cards.is_empty() {
            return Self {
                label: label.into(),
                overall: 0.0,
                dimensions: BTreeMap::new(),
            };
        }
        let n = cards.len() as f64;
        let overall = cards.iter().map(|c| c.overall()).sum::<f64>() / n;
        let mut sums: BTreeMap<String, (f64, usize)> = BTreeMap::new();
        for card in cards {
            for (dim, score) in &card.per_dimension {
                let entry = sums.entry(dim.clone()).or_insert((0.0, 0));
                entry.0 += score.fraction();
                entry.1 += 1;
            }
        }
        let dimensions = sums
            .into_iter()
            .map(|(dim, (sum, count))| (dim, sum / count as f64))
            .collect();
        Self {
            label: label.into(),
            overall,
            dimensions,
        }
    }

    pub fn to_json(&self) -> serde_json::Result<String> {
        serde_json::to_string_pretty(self)
    }

    pub fn from_json(json: &str) -> serde_json::Result<Self> {
        serde_json::from_str(json)
    }

    /// Persist the baseline to disk as pretty JSON.
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        let json = self
            .to_json()
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        std::fs::write(path, json)
    }

    /// Load a baseline from disk.
    pub fn load(path: &Path) -> std::io::Result<Self> {
        let json = std::fs::read_to_string(path)?;
        Self::from_json(&json).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
    }

    /// Compare a later run against this baseline. Any overall or per-dimension
    /// score that fell more than `tolerance` below the baseline is reported.
    /// A dimension present in the baseline but absent in `current` is treated as
    /// a drop to 0.0 (the dimension stopped being measured or scored).
    pub fn regressions(&self, current: &Baseline, tolerance: f64) -> Vec<Regression> {
        let mut out = Vec::new();
        check(
            OVERALL_KEY,
            self.overall,
            current.overall,
            tolerance,
            &mut out,
        );
        for (dim, &base) in &self.dimensions {
            let cur = current.dimensions.get(dim).copied().unwrap_or(0.0);
            check(dim, base, cur, tolerance, &mut out);
        }
        out
    }

    /// True when `current` shows no regression beyond `tolerance`.
    pub fn holds(&self, current: &Baseline, tolerance: f64) -> bool {
        self.regressions(current, tolerance).is_empty()
    }
}

fn check(dim: &str, baseline: f64, current: f64, tolerance: f64, out: &mut Vec<Regression>) {
    if current < baseline - tolerance {
        out.push(Regression {
            dimension: dim.to_string(),
            baseline,
            current,
            delta: current - baseline,
        });
    }
}

/// A COMMITTED per-track contract baseline (#890 step 6).
///
/// Deliberately NOT a pinned score. A metric value is only reproducible against
/// a frozen corpus AND a frozen ranker, so a pinned figure would make every
/// legitimate ranking improvement look like a regression — and it would be a
/// second copy of a number whose authority is a results document. What is
/// pinned here is everything that must never move silently:
///
/// * the dataset SHAPE — a shrink lifts every average by dropping the hard
///   cases, and no other assertion in the suite would see it;
/// * the metric SURFACE — a leg that stops printing one metric while still
///   reporting the others reads as green;
/// * the `floors` — lower bounds on the ranking-sensitive figures. A floor is
///   a smoke bound, not a measurement: it says the leg still retrieves
///   something, and it is set where a BROKEN ranker (one that returns nothing)
///   falls through it. That sensitivity is proved in-run by the guard test's
///   own positive control, never assumed.
///
/// The live legs are named in `live_legs` and stay OUT of CI: they read the
/// real store through the CLI (`src/cli/eval_cmd.rs`), so CI cannot reproduce
/// them and must not pretend to.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrackBaseline {
    pub track: String,
    pub label: String,
    pub dataset: DatasetShape,
    pub window: usize,
    /// Substrings that must appear in the rendered scorecard.
    pub metrics: Vec<String>,
    /// Lower bounds on ranking-sensitive figures. Empty is legal: a track whose
    /// scored values need a live store has no CI-reproducible floor.
    #[serde(default)]
    pub floors: BTreeMap<String, f64>,
    /// Legs that need a live store, named so their absence from CI is DECLARED
    /// rather than implied.
    #[serde(default)]
    pub live_legs: Vec<String>,
}

/// The dataset shape a baseline pins.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct DatasetShape {
    pub positives: usize,
    pub negatives: usize,
}

impl TrackBaseline {
    pub fn from_json(json: &str) -> serde_json::Result<Self> {
        serde_json::from_str(json)
    }

    /// Metrics the rendered scorecard does NOT carry, sorted by their order in
    /// `metrics`. Empty means the leg still publishes everything it was
    /// recorded with.
    pub fn missing_metrics(&self, rendered: &str) -> Vec<String> {
        self.metrics
            .iter()
            .filter(|m| !rendered.contains(m.as_str()))
            .cloned()
            .collect()
    }

    /// Dataset-shape violations. A mismatch is a silent dataset edit, which is
    /// the one change that lifts every average without touching the code.
    pub fn shape_violations(&self, positives: usize, negatives: usize) -> Vec<String> {
        let mut out = Vec::new();
        if positives != self.dataset.positives {
            out.push(format!(
                "positives {} != baseline {}",
                positives, self.dataset.positives
            ));
        }
        if negatives != self.dataset.negatives {
            out.push(format!(
                "negatives {} != baseline {}",
                negatives, self.dataset.negatives
            ));
        }
        out
    }

    /// Floor violations, given the figures the leg measured.
    pub fn floor_violations(&self, measured: &BTreeMap<String, f64>) -> Vec<String> {
        let mut out = Vec::new();
        for (key, &floor) in &self.floors {
            match measured.get(key) {
                Some(&value) if value >= floor => {}
                Some(&value) => {
                    out.push(format!("{key} {value:.3} fell below its floor {floor:.3}"))
                }
                None => out.push(format!("{key} was not measured at all (floor {floor:.3})")),
            }
        }
        out
    }
}
