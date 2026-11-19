//! `opencrabs eval` — run a recall benchmark track against a labeled dataset.
//!
//! The eval framework in [`crate::eval`] already carries the metrics
//! ([`crate::eval::recall`]), the negatives convention (#996), a scorer (#622)
//! and baseline drift (#632). What it never had was a RUNNER SURFACE: the
//! framework was reachable only from unit tests, so no number could be
//! published or compared across runs (#890).
//!
//! This module is that surface. It is deliberately thin — every metric is the
//! shipped implementation, and the retrieval it scores is the shipped
//! implementation, so a change to either is measured rather than assumed.
//!
//! Judge-based metrics stay out of CI (#632), and the LIVE legs never run here:
//! this command is the fixture leg, whose corpus and labels are committed.

use anyhow::{Context, Result, bail};

use crate::brain::brain_sections::Matches;
use crate::brain::memory_recall::{RECALL_MAX_CHARS, RECALL_MAX_SECTIONS, RECALL_MIN_SCORE};
use crate::brain::section_rank::Ranked;
use crate::eval::recall::{RecallDataset, RecallReport};

use super::args::EvalCommands;

/// Which corpus a track scores against.
///
/// Track B is the fixture leg: a committed synthetic corpus plus committed
/// labels, so the number is comparable across weeks. Tracks A (episodic
/// messages) and C (skill selection) read the LIVE corpora and are therefore
/// not comparable across runs unless their targets still exist — they are
/// deliberately NOT wired into this command yet (#890 steps 4 and 5).
const SUPPORTED_TRACKS: &[&str] = &["b"];

pub(crate) fn cmd_eval(cmd: EvalCommands) -> Result<()> {
    match cmd {
        EvalCommands::Recall {
            track,
            dataset,
            corpus,
            k,
            json,
        } => run_recall(&track, &dataset, corpus.as_deref(), k, json),
    }
}

fn run_recall(
    track: &str,
    dataset_path: &str,
    corpus_path: Option<&str>,
    k: Option<usize>,
    json: bool,
) -> Result<()> {
    let track_norm = track.trim().to_ascii_lowercase();
    if !SUPPORTED_TRACKS.contains(&track_norm.as_str()) {
        bail!(
            "eval recall: track {:?} is not wired yet (supported: {}). \
             Tracks A and C read the live corpora and land in #890 steps 4-5.",
            track,
            SUPPORTED_TRACKS.join(", ")
        );
    }

    let raw = std::fs::read_to_string(dataset_path)
        .with_context(|| format!("eval recall: cannot read dataset {dataset_path}"))?;
    let dataset = RecallDataset::from_json(&raw)
        .with_context(|| format!("eval recall: {dataset_path} is not a valid recall dataset"))?;
    if dataset.cases.is_empty() {
        bail!("eval recall: dataset {dataset_path} has no cases");
    }

    // The corpus defaults to the dataset's sibling so the committed fixture
    // pair runs with one flag, and the pairing cannot drift. A dataset named
    // `<stem>_recall.json` prefers `<stem>_corpus.md` (#890 step 3: the Track B
    // set carries its own 292-section corpus); anything else keeps the original
    // `memory_corpus.md` sibling, so the v1 fixture pair is unchanged.
    let corpus_path = corpus_path
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            let dataset = std::path::Path::new(dataset_path);
            let dir = dataset.parent().unwrap_or_else(|| std::path::Path::new("."));
            let stem = dataset
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or_default();
            let named = stem
                .strip_suffix("_recall")
                .map(|base| dir.join(format!("{base}_corpus.md")))
                .filter(|p| p.is_file());
            named.unwrap_or_else(|| dir.join("memory_corpus.md"))
        });
    let corpus_raw = std::fs::read_to_string(&corpus_path)
        .with_context(|| format!("eval recall: cannot read corpus {}", corpus_path.display()))?;

    // The SHIPPED retrieval, not a copy of it: if the thresholds move, the
    // scorecard moves with them instead of scoring a configuration nobody runs.
    let k = k.unwrap_or(RECALL_MAX_SECTIONS);
    let report = score_recall(&dataset, &corpus_raw, k);

    if json {
        print_json(&dataset.name, &track_norm, &report);
    } else {
        println!("# eval recall — track {}", track_norm.to_ascii_uppercase());
        println!("dataset: {}", dataset.name);
        println!("corpus:  {}", corpus_path.display());
        println!();
        print!("{}", report.render());
        println!();
        println!(
            "positives={} negatives={} false_positives={} k={}",
            report.positives(),
            report.negatives,
            report.false_positives,
            report.k
        );
    }
    Ok(())
}

/// Score a dataset against a corpus with the SHIPPED retrieval and the SHIPPED
/// metrics.
///
/// Split out from the CLI so the fixture leg's exact numbers are asserted by a
/// test rather than only printed: a ranking change that moves a scorecard has
/// to move the test with it, or the gate goes RED (#890 step 5).
pub(crate) fn score_recall(dataset: &RecallDataset, corpus: &str, k: usize) -> RecallReport {
    let ranked = Ranked::build(corpus);
    let results: Vec<Vec<String>> = dataset
        .cases
        .iter()
        .map(|case| {
            let matches: Matches = ranked.find_relevant(
                &case.query,
                RECALL_MAX_SECTIONS,
                RECALL_MAX_CHARS,
                RECALL_MIN_SCORE,
            );
            section_ids(&matches)
        })
        .collect();
    score_rankings(dataset, &results, k)
}

/// Score an EXPLICIT set of rankings against a dataset.
///
/// The retrieval and the metrics are separated so a test can hold the dataset
/// fixed and mutate only the ranking: if the scorecard did not move, the leg
/// would be blind to the one thing it exists to catch (#890 step 5).
pub(crate) fn score_rankings(
    dataset: &RecallDataset,
    results: &[Vec<String>],
    k: usize,
) -> RecallReport {
    RecallReport::compute(&dataset.cases, results, k)
}

/// A returned section's id is its heading text with the leading hashes and
/// surrounding whitespace stripped — the same identity the committed dataset
/// labels use, so the labels and the retrieval cannot disagree about naming.
fn section_ids(m: &Matches) -> Vec<String> {
    m.sections
        .iter()
        .map(|s| s.heading.trim_start_matches('#').trim().to_string())
        .collect()
}

/// Machine-readable scorecard for the regression guard and any later
/// baseline diff. Flat by design: a nested shape would invite consumers to
/// depend on structure the framework does not promise.
fn print_json(dataset: &str, track: &str, report: &RecallReport) {
    let obj = serde_json::json!({
        "track": track.to_ascii_uppercase(),
        "dataset": dataset,
        "k": report.k,
        "positives": report.positives(),
        "negatives": report.negatives,
        "false_positives": report.false_positives,
        "precision": report.aggregate.precision,
        "recall": report.aggregate.recall,
        "mrr": report.aggregate.mrr,
        "ndcg": report.aggregate.ndcg,
        "fpr": report.false_positive_rate(),
    });
    println!("{}", serde_json::to_string_pretty(&obj).unwrap_or_default());
}
