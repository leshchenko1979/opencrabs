//! Retrieval-mode ablation: BM25-only vs vector-only vs hybrid (#981).
//!
//! Every retrieval number this project has published is a HYBRID number, so
//! "what do the vectors add on top of BM25?" has never been measured — only
//! their cost (a 300 MB model, a 372 MB store, an embedding round-trip per
//! query, a linear chunk scan). This module supplies the measurement: the same
//! labelled queries scored three ways over the same corpus, plus the two
//! counters that decide the question and that no shipped metric provides.
//!
//! ## Why this module holds no database and no I/O
//!
//! The numbers have to be reproducible on a machine with no model, no store and
//! no network, or they are not comparable across weeks. So everything here is a
//! pure function over ranked document-id lists: the CLI layer
//! (`cli::eval_cmd::run_retrieval`) owns the stores, the embedder and the
//! timing, and hands this module nothing but `Vec<Vec<String>>`. That split is
//! also what lets the tests pin a hybrid win, a BM25 win and a tie by
//! construction instead of by hoping a ranker produces one.
//!
//! ## The two counters
//!
//! Precision/recall/MRR/nDCG all average away the thing being asked. What
//! matters is the ASYMMETRY of unique hits — cases where hybrid found a gold
//! document within `k` that BM25 missed, versus the reverse — and whether the
//! paired difference is distinguishable from zero. The bootstrap interval is
//! seeded so two runs of the same data print the same interval, and McNemar's
//! exact test is reported alongside because a paired difference on 60 cases has
//! little power and the p-value says so honestly.

use std::collections::{HashMap, HashSet};

use crate::brain::brain_sections::{query_terms, split_sections};
use crate::eval::recall::{QueryCase, QueryMetrics, RecallReport, recall_at_k};

/// Fixed bootstrap seed: a scorecard whose interval moves between runs is a
/// scorecard nobody can cite. Chosen arbitrarily and never varied.
pub const BOOTSTRAP_SEED: u64 = 0x9810_5EED;

/// Resamples per bootstrap interval. Two thousand is the usual floor for a
/// stable 95% percentile interval and costs nothing on 60 cases.
pub const BOOTSTRAP_RESAMPLES: usize = 2000;

/// One retrieval mode under test.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Mode {
    /// The shipped lexical ranker (`brain::section_rank::Ranked`).
    Bm25,
    /// The shipped chunk vector search (`memory::vector_search::search_chunks`).
    Vector,
    /// The shipped fusion (`memory::search::hybrid_search_rrf`).
    Hybrid,
}

impl Mode {
    /// Parse a `--modes` entry. Accepts the short spellings an operator types.
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "bm25" | "fts" | "lexical" => Some(Self::Bm25),
            "vec" | "vector" => Some(Self::Vector),
            "hybrid" | "rrf" => Some(Self::Hybrid),
            _ => None,
        }
    }

    /// The name this mode is printed and keyed under.
    pub fn label(self) -> &'static str {
        match self {
            Self::Bm25 => "bm25",
            Self::Vector => "vec",
            Self::Hybrid => "hybrid",
        }
    }
}

/// Parse a comma-separated `--modes` list, rejecting an unknown name rather
/// than silently dropping it — a typo must not produce a one-mode scorecard
/// that looks like a three-mode one.
pub fn parse_modes(raw: &str) -> Result<Vec<Mode>, String> {
    let mut modes = Vec::new();
    for part in raw.split(',') {
        let trimmed = part.trim();
        if trimmed.is_empty() {
            continue;
        }
        let mode = Mode::parse(trimmed)
            .ok_or_else(|| format!("unknown retrieval mode {trimmed:?} (bm25, vec, hybrid)"))?;
        if !modes.contains(&mode) {
            modes.push(mode);
        }
    }
    if modes.is_empty() {
        return Err("no retrieval modes selected".to_string());
    }
    Ok(modes)
}

/// Machine-derived class of a labelled case.
///
/// A case is `Paraphrase` when the query shares NO content term with its gold
/// section, `Lexical` when it shares at least one. The label is never stored in
/// the dataset: it is derived here with the shipped tokenizer, so a case that
/// was *intended* as a paraphrase but happens to share a rare word is scored as
/// lexical and the vector side is not flattered by a mislabel (#981).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaseClass {
    /// The query shares a term with its gold section.
    Lexical,
    /// The query shares no term with its gold section.
    Paraphrase,
}

impl CaseClass {
    pub fn label(self) -> &'static str {
        match self {
            Self::Lexical => "lexical",
            Self::Paraphrase => "paraphrase",
        }
    }
}

/// How a labelled dataset splits between the two classes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ClassSplit {
    pub total: usize,
    pub lexical: usize,
    pub paraphrase: usize,
    pub negatives: usize,
}

impl ClassSplit {
    pub fn positives(&self) -> usize {
        self.lexical + self.paraphrase
    }
}

/// A labelled query set for the ablation.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct RetrievalDataset {
    pub name: String,
    #[serde(default)]
    pub note: String,
    pub cases: Vec<QueryCase>,
}

impl RetrievalDataset {
    pub fn from_json(raw: &str) -> serde_json::Result<Self> {
        serde_json::from_str(raw)
    }

    /// Cases asserting that retrieval should return nothing (#996).
    pub fn negatives(&self) -> usize {
        self.cases.iter().filter(|c| c.is_negative()).count()
    }

    pub fn positives(&self) -> usize {
        self.cases.len() - self.negatives()
    }
}

/// Derive the class of one case from the shipped tokenizer.
pub fn derive_class(query: &str, gold_text: &str) -> CaseClass {
    let query_terms: HashSet<String> = query_terms(query).into_iter().collect();
    let gold_terms: HashSet<String> = query_terms(gold_text).into_iter().collect();
    if query_terms.is_disjoint(&gold_terms) {
        CaseClass::Paraphrase
    } else {
        CaseClass::Lexical
    }
}

/// Section id -> the text the tokenizer sees, for every section of `corpus`.
///
/// The id is the heading with the leading hashes and surrounding whitespace
/// stripped — the same identity `cli::eval_cmd` emits for the BM25 leg and the
/// same string the harness indexes the store under, so a gold label resolves
/// identically in every leg.
pub fn section_text_index(corpus: &str) -> HashMap<String, String> {
    let mut index = HashMap::new();
    for section in split_sections(corpus) {
        let id = section
            .heading
            .trim()
            .trim_start_matches('#')
            .trim()
            .to_string();
        if id.is_empty() {
            continue;
        }
        index.insert(id, format!("{}\n{}", section.heading, section.body));
    }
    index
}

/// Compute the class split of `dataset` against `corpus`.
///
/// Returns an error when a gold id does not resolve to a section: an unresolved
/// label is a broken dataset, not a zero, and silently scoring it as a miss
/// would hide the mistake behind a plausible number.
pub fn class_split(dataset: &RetrievalDataset, corpus: &str) -> Result<ClassSplit, String> {
    let index = section_text_index(corpus);
    let mut split = ClassSplit::default();
    for case in &dataset.cases {
        split.total += 1;
        if case.is_negative() {
            split.negatives += 1;
            continue;
        }
        let mut class: Option<CaseClass> = None;
        for id in &case.relevant {
            let text = index.get(id).ok_or_else(|| {
                format!(
                    "gold id {id:?} for query {:?} does not resolve to a section of the corpus",
                    case.query
                )
            })?;
            let derived = derive_class(&case.query, text);
            // A case with several gold ids takes the class of the FIRST one;
            // the corpus is one gold section per case by construction.
            class = Some(class.unwrap_or(derived));
        }
        match class {
            Some(CaseClass::Paraphrase) => split.paraphrase += 1,
            _ => split.lexical += 1,
        }
    }
    Ok(split)
}

/// Whether any gold id appears in the top `k` of a ranking.
fn hit_within(ranked: &[String], relevant: &HashSet<String>, k: usize) -> bool {
    ranked.iter().take(k).any(|id| relevant.contains(id))
}

/// The unique-hit table between two modes over the same cases.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct UniqueHits {
    /// Positive cases scored (negatives are excluded: they have no gold).
    pub positives: usize,
    /// Cases only the baseline found a gold document for, within `k`.
    pub baseline_only: usize,
    /// Cases only the other mode found a gold document for, within `k`.
    pub other_only: usize,
    /// Cases both modes found.
    pub both: usize,
    /// Cases neither mode found.
    pub neither: usize,
}

impl UniqueHits {
    /// Share of positive cases where only the baseline hit.
    pub fn baseline_rate(&self) -> f64 {
        if self.positives == 0 {
            0.0
        } else {
            self.baseline_only as f64 / self.positives as f64
        }
    }

    /// Share of positive cases where only the other mode hit. The pre-registered
    /// rule compares this against twice [`Self::baseline_rate`].
    pub fn other_rate(&self) -> f64 {
        if self.positives == 0 {
            0.0
        } else {
            self.other_only as f64 / self.positives as f64
        }
    }
}

/// Build the unique-hit table for `other` against `baseline`.
///
/// `cases`, `baseline` and `other` are positionally paired.
pub fn unique_hits(
    cases: &[QueryCase],
    baseline: &[Vec<String>],
    other: &[Vec<String>],
    k: usize,
) -> UniqueHits {
    let mut hits = UniqueHits::default();
    for ((case, base), oth) in cases.iter().zip(baseline.iter()).zip(other.iter()) {
        if case.is_negative() {
            continue;
        }
        let relevant: HashSet<String> = case.relevant.iter().cloned().collect();
        hits.positives += 1;
        match (
            hit_within(base, &relevant, k),
            hit_within(oth, &relevant, k),
        ) {
            (true, true) => hits.both += 1,
            (true, false) => hits.baseline_only += 1,
            (false, true) => hits.other_only += 1,
            (false, false) => hits.neither += 1,
        }
    }
    hits
}

/// A paired difference between two modes with a seeded bootstrap interval and
/// McNemar's exact test over the discordant pairs.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PairedDelta {
    /// Mean per-case recall@k for `other` minus the same for `baseline`.
    pub delta: f64,
    /// Lower bound of the 95% percentile bootstrap interval.
    pub ci_low: f64,
    /// Upper bound of the 95% percentile bootstrap interval.
    pub ci_high: f64,
    pub seed: u64,
    pub resamples: usize,
    /// Cases where `other` hit and `baseline` missed.
    pub other_only: usize,
    /// Cases where `baseline` hit and `other` missed.
    pub baseline_only: usize,
    /// Two-sided exact McNemar p-value over the discordant pairs.
    pub mcnemar_p: f64,
}

impl PairedDelta {
    /// Whether the interval excludes zero. When it does not, the difference is
    /// indistinguishable from noise and the pre-registered rule says the
    /// vectors have not earned their cost — not "inconclusive".
    pub fn excludes_zero(&self) -> bool {
        self.ci_low > 0.0 || self.ci_high < 0.0
    }
}

/// A small xorshift64 generator. Hand-rolled so the interval is reproducible
/// from the seed alone, independent of any dependency's version.
fn xorshift64(state: &mut u64) -> u64 {
    let mut x = *state;
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    *state = x;
    x
}

/// Percentile bootstrap interval for the mean of `values`.
fn bootstrap_mean_ci(values: &[f64], seed: u64, resamples: usize) -> (f64, f64) {
    let n = values.len();
    if n == 0 || resamples == 0 {
        return (0.0, 0.0);
    }
    let mut state = if seed == 0 { 1 } else { seed };
    let mut means: Vec<f64> = Vec::with_capacity(resamples);
    for _ in 0..resamples {
        let mut sum = 0.0;
        for _ in 0..n {
            let idx = (xorshift64(&mut state) % n as u64) as usize;
            sum += values[idx];
        }
        means.push(sum / n as f64);
    }
    means.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let low_idx = ((0.025 * resamples as f64) as usize).min(resamples - 1);
    let high_idx = ((0.975 * resamples as f64) as usize).min(resamples - 1);
    (means[low_idx], means[high_idx])
}

/// Two-sided exact McNemar test on the discordant pair counts.
///
/// Under the null the discordant pairs split binomially with p = 0.5, so the
/// two-sided p-value is twice the lower tail of `Binomial(b + c, 0.5)`. Computed
/// in f64 by recurrence; `2^n` is exact for the case counts this sees.
pub fn mcnemar_exact(other_only: usize, baseline_only: usize) -> f64 {
    let n = other_only + baseline_only;
    if n == 0 {
        return 1.0;
    }
    let m = other_only.min(baseline_only);
    let mut term = 1.0_f64;
    let mut tail = 1.0_f64;
    for i in 1..=m {
        term *= (n - i + 1) as f64 / i as f64;
        tail += term;
    }
    let p = 2.0 * tail / 2f64.powi(n as i32);
    if p > 1.0 { 1.0 } else { p }
}

/// Compute the paired difference of recall@k between `other` and `baseline`.
///
/// Per-case values are binary recall@k over the positives; negatives carry no
/// gold and are excluded, exactly as [`RecallReport`] excludes them.
pub fn paired_delta(
    cases: &[QueryCase],
    baseline: &[Vec<String>],
    other: &[Vec<String>],
    k: usize,
    seed: u64,
    resamples: usize,
) -> PairedDelta {
    let mut diffs: Vec<f64> = Vec::new();
    let (mut other_only, mut baseline_only) = (0usize, 0usize);
    for ((case, base), oth) in cases.iter().zip(baseline.iter()).zip(other.iter()) {
        if case.is_negative() {
            continue;
        }
        let relevant: HashSet<String> = case.relevant.iter().cloned().collect();
        let b = recall_at_k(base, &relevant, k);
        let o = recall_at_k(oth, &relevant, k);
        if o > b {
            other_only += 1;
        } else if b > o {
            baseline_only += 1;
        }
        diffs.push(o - b);
    }
    let delta = if diffs.is_empty() {
        0.0
    } else {
        diffs.iter().sum::<f64>() / diffs.len() as f64
    };
    let (ci_low, ci_high) = bootstrap_mean_ci(&diffs, seed, resamples);
    PairedDelta {
        delta,
        ci_low,
        ci_high,
        seed,
        resamples,
        other_only,
        baseline_only,
        mcnemar_p: mcnemar_exact(other_only, baseline_only),
    }
}

/// One mode's scorecard.
#[derive(Debug, Clone)]
pub struct ModeScore {
    pub mode: Mode,
    /// Precision, recall, MRR and nDCG at `k`, macro-averaged over positives,
    /// with the negative/false-positive counts carried alongside.
    pub report: RecallReport,
}

impl ModeScore {
    pub fn label(&self) -> &'static str {
        self.mode.label()
    }

    /// The four aggregate metrics in the order a reader expects them.
    pub fn metrics(&self) -> QueryMetrics {
        self.report.aggregate
    }
}

/// Score one mode's rankings.
pub fn score_mode(
    mode: Mode,
    cases: &[QueryCase],
    rankings: &[Vec<String>],
    k: usize,
) -> ModeScore {
    ModeScore {
        mode,
        report: RecallReport::compute(cases, rankings, k),
    }
}

/// The head-to-head between two modes.
#[derive(Debug, Clone, Copy)]
pub struct Comparison {
    pub baseline: Mode,
    pub other: Mode,
    pub unique: UniqueHits,
    pub paired: PairedDelta,
}

/// Compare `other` against `baseline` over the same cases.
pub fn compare(
    baseline: Mode,
    other: Mode,
    cases: &[QueryCase],
    baseline_rankings: &[Vec<String>],
    other_rankings: &[Vec<String>],
    k: usize,
    seed: u64,
    resamples: usize,
) -> Comparison {
    Comparison {
        baseline,
        other,
        unique: unique_hits(cases, baseline_rankings, other_rankings, k),
        paired: paired_delta(cases, baseline_rankings, other_rankings, k, seed, resamples),
    }
}

/// Whether the embedder produced bit-identical vectors for the same query.
///
/// The fixture leg's numbers are only CI-gated when this is `BitStable`; a
/// non-deterministic embedder means a re-run can move a score without a code
/// change, so the leg is published but never asserted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Determinism {
    /// The same query embedded twice produced identical vectors.
    BitStable,
    /// The vectors differed; the fixture leg is printed but not gated.
    NotBitStable,
    /// No embedder was available, so nothing could be checked.
    NotChecked,
}

impl Determinism {
    pub fn label(self) -> &'static str {
        match self {
            Self::BitStable => "bit-stable",
            Self::NotBitStable => "not-bit-stable (fixture leg not CI-gated)",
            Self::NotChecked => "not-checked (no embedder)",
        }
    }
}

/// The corpus a LIVE run measured, so a later reader can tell whether a score
/// moved because the method changed or because the corpus grew.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CorpusFingerprint {
    pub documents: usize,
    pub vectors: usize,
    pub model: String,
}

/// The assembled scorecard: per-mode metrics, the head-to-head, the class
/// split, the determinism verdict and — on the live leg — the fingerprint.
#[derive(Debug, Clone)]
pub struct Scorecard {
    pub dataset: String,
    pub k: usize,
    /// `None` on the live leg, where there is no committed corpus to split.
    pub split: Option<ClassSplit>,
    pub modes: Vec<ModeScore>,
    pub comparison: Option<Comparison>,
    pub determinism: Determinism,
    pub fingerprint: Option<CorpusFingerprint>,
    /// Modes that could not be measured, with the reason (e.g. no embedder).
    pub unavailable: Vec<(Mode, String)>,
    /// Caveats the run discovered for itself — unresolved live labels, a parity
    /// check that could not run, a leg that was published but not gated.
    pub notes: Vec<String>,
}

impl Scorecard {
    /// Stable human-readable summary. Every figure a reader needs to apply the
    /// pre-registered rule is on one screen.
    pub fn render(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!("dataset: {}\n", self.dataset));
        out.push_str(&format!("k:       {}\n", self.k));
        if let Some(split) = self.split {
            out.push_str(&format!(
                "class:   total={} lexical={} paraphrase={} negative={}\n",
                split.total, split.lexical, split.paraphrase, split.negatives
            ));
        }
        out.push_str(&format!("embed:   {}\n", self.determinism.label()));
        if let Some(fp) = &self.fingerprint {
            out.push_str(&format!(
                "corpus:  documents={} vectors={} model={}\n",
                fp.documents, fp.vectors, fp.model
            ));
        }
        out.push('\n');
        for score in &self.modes {
            let m = score.metrics();
            out.push_str(&format!(
                "{:<7} P={:.3} R={:.3} MRR={:.3} nDCG={:.3} FPR={:.3} (n={} neg={} fp={})\n",
                score.label(),
                m.precision,
                m.recall,
                m.mrr,
                m.ndcg,
                score.report.false_positive_rate(),
                score.report.positives(),
                score.report.negatives,
                score.report.false_positives,
            ));
        }
        for (mode, reason) in &self.unavailable {
            out.push_str(&format!("{:<7} unavailable: {reason}\n", mode.label()));
        }
        for note in &self.notes {
            out.push_str(&format!("note:    {note}\n"));
        }
        if let Some(cmp) = &self.comparison {
            out.push('\n');
            out.push_str(&format!(
                "unique hits ({} vs {}): {}_only={} {}_only={} both={} neither={} (positives={})\n",
                cmp.other.label(),
                cmp.baseline.label(),
                cmp.other.label(),
                cmp.unique.other_only,
                cmp.baseline.label(),
                cmp.unique.baseline_only,
                cmp.unique.both,
                cmp.unique.neither,
                cmp.unique.positives,
            ));
            out.push_str(&format!(
                "paired recall@{k} delta={delta:+.3} CI=[{lo:+.3}, {hi:+.3}] \
                 (seed={seed}, {resamples} resamples) McNemar p={p:.4} {verdict}\n",
                k = self.k,
                delta = cmp.paired.delta,
                lo = cmp.paired.ci_low,
                hi = cmp.paired.ci_high,
                seed = cmp.paired.seed,
                resamples = cmp.paired.resamples,
                p = cmp.paired.mcnemar_p,
                verdict = if cmp.paired.excludes_zero() {
                    "distinguishable from zero"
                } else {
                    "indistinguishable from zero"
                },
            ));
        }
        out
    }

    /// Machine-readable scorecard. Flat by design: a nested shape invites
    /// consumers to depend on structure this framework does not promise.
    pub fn to_json(&self) -> serde_json::Value {
        let modes: serde_json::Map<String, serde_json::Value> = self
            .modes
            .iter()
            .map(|score| {
                let m = score.metrics();
                let value = serde_json::json!({
                    "precision": m.precision,
                    "recall": m.recall,
                    "mrr": m.mrr,
                    "ndcg": m.ndcg,
                    "positives": score.report.positives(),
                    "negatives": score.report.negatives,
                    "false_positives": score.report.false_positives,
                    "fpr": score.report.false_positive_rate(),
                });
                (score.label().to_string(), value)
            })
            .collect();

        let comparison = self.comparison.as_ref().map(|cmp| {
            serde_json::json!({
                "baseline": cmp.baseline.label(),
                "other": cmp.other.label(),
                "unique": {
                    "positives": cmp.unique.positives,
                    "baseline_only": cmp.unique.baseline_only,
                    "other_only": cmp.unique.other_only,
                    "both": cmp.unique.both,
                    "neither": cmp.unique.neither,
                    "baseline_rate": cmp.unique.baseline_rate(),
                    "other_rate": cmp.unique.other_rate(),
                },
                "paired": {
                    "delta": cmp.paired.delta,
                    "ci_low": cmp.paired.ci_low,
                    "ci_high": cmp.paired.ci_high,
                    "seed": cmp.paired.seed,
                    "resamples": cmp.paired.resamples,
                    "other_only": cmp.paired.other_only,
                    "baseline_only": cmp.paired.baseline_only,
                    "mcnemar_p": cmp.paired.mcnemar_p,
                    "excludes_zero": cmp.paired.excludes_zero(),
                },
            })
        });

        let unavailable: Vec<serde_json::Value> = self
            .unavailable
            .iter()
            .map(|(mode, reason)| serde_json::json!({ "mode": mode.label(), "reason": reason }))
            .collect();

        let fingerprint = self.fingerprint.as_ref().map(|fp| {
            serde_json::json!({
                "documents": fp.documents,
                "vectors": fp.vectors,
                "model": fp.model,
            })
        });

        serde_json::json!({
            "dataset": self.dataset,
            "k": self.k,
            "split": self.split.map(|s| serde_json::json!({
                "total": s.total,
                "lexical": s.lexical,
                "paraphrase": s.paraphrase,
                "negatives": s.negatives,
            })),
            "determinism": self.determinism.label(),
            "fingerprint": fingerprint,
            "modes": modes,
            "comparison": comparison,
            "unavailable": unavailable,
            "notes": self.notes,
        })
    }
}
