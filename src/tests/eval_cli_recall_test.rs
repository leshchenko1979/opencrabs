//! The `eval recall` runner surface and its regression guard (#890).
//!
//! Two things are locked here, and they are different questions:
//!
//! 1. The runner reproduces the COMMITTED scorecard from the committed fixture
//!    pair. If a change to the shipped retrieval moves the numbers, this fails
//!    and the scorecard has to be re-published deliberately rather than
//!    drifting silently.
//! 2. The leg is SENSITIVE to the ranking it scores. A suite that prints a
//!    stable number no matter what the ranker does is worse than no suite: it
//!    reads as a pass. The mutation test holds the dataset fixed and mutates
//!    only the ranking, so a blind leg fails here instead of in production.

use crate::brain::memory_recall::RECALL_MIN_SCORE;
use crate::cli::eval_cmd::{score_rankings, score_recall};
use crate::eval::recall::RecallDataset;

const CORPUS: &str = include_str!("../eval/fixtures/memory_corpus.md");
const DATASET: &str = include_str!("../eval/fixtures/memory_recall.json");

/// The committed cutoff, matching the published scorecard
/// (`src/eval/results/memory-recall-v2.md`).
const K: usize = 2;

fn dataset() -> RecallDataset {
    RecallDataset::from_json(DATASET).expect("the committed fixture parses")
}

#[test]
fn fixture_leg_reproduces_the_committed_scorecard() {
    let report = score_recall(&dataset(), CORPUS, K, RECALL_MIN_SCORE);

    assert!(
        (report.aggregate.precision - 1.000).abs() < 1e-3,
        "precision@{K} must reproduce the published 1.000, got {:.3}\n{}",
        report.aggregate.precision,
        report.render()
    );
    assert!(
        (report.false_positive_rate() - 0.167).abs() < 1e-3,
        "FPR must reproduce the published 0.167, got {:.3}\n{}",
        report.false_positive_rate(),
        report.render()
    );
    assert_eq!(
        report.positives(),
        12,
        "the fixture carries 12 positive cases"
    );
    assert_eq!(report.negatives, 12, "the fixture carries 12 negatives");
}

/// The leg must SEE a mutated ranking. Holding the dataset fixed and emptying
/// every result list is the harshest mutation: silence scores perfectly on
/// negatives (FPR 0.0) while every positive metric collapses to zero. A leg
/// that reported the baseline numbers here would be measuring nothing.
#[test]
fn a_mutated_ranking_moves_the_scorecard() {
    let ds = dataset();
    let baseline = score_recall(&ds, CORPUS, K, RECALL_MIN_SCORE);

    let silenced: Vec<Vec<String>> = ds.cases.iter().map(|_| Vec::new()).collect();
    let mutated = score_rankings(&ds, &silenced, K);

    assert!(
        mutated.aggregate.precision < baseline.aggregate.precision,
        "emptying every ranking must drop precision below {:.3}, got {:.3}",
        baseline.aggregate.precision,
        mutated.aggregate.precision
    );
    assert!(
        mutated.aggregate.recall < baseline.aggregate.recall,
        "emptying every ranking must drop recall below {:.3}, got {:.3}",
        baseline.aggregate.recall,
        mutated.aggregate.recall
    );
    assert_eq!(
        mutated.aggregate.mrr, 0.0,
        "with no results there is no reciprocal rank to average"
    );
    // The other direction: silence scores PERFECTLY on the negatives, which is
    // exactly why positives-only scoring cannot see over-firing (#996).
    assert_eq!(
        mutated.false_positive_rate(),
        0.0,
        "silence never fires, so FPR bottoms out — the number a positive-only \
         suite would have called healthy"
    );
}
