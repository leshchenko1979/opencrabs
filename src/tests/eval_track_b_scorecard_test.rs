//! The Track B scorecard leg: 60 positive + 15 negative queries over the
//! committed reference corpus, scored at k=10 by the SHIPPED retrieval (#890
//! step 3).
//!
//! Three questions live here and they are different:
//!
//! 1. Is the dataset the shape the plan specifies? A silent shrink lifts every
//!    average by dropping the hard cases, and nothing else in the suite would
//!    notice.
//! 2. Does the leg PUBLISH the four metrics the plan asks for — recall@10, MRR,
//!    nDCG@10 and the false-positive rate — over that dataset? Pinned as a
//!    contract on the rendered scorecard, so the numbers cannot be dropped from
//!    the output while the suite still reads green.
//! 3. Does the leg SEE the ranking it scores? The mutation test holds the
//!    dataset fixed and empties every ranking: silence scores perfectly on
//!    negatives while every positive metric collapses. A leg that reported the
//!    baseline here would be measuring nothing.
//!
//! The numbers themselves are published in `src/eval/results/track-b-recall-v1.md`
//! (plan step 7) rather than pinned here: a constant in a test is a second copy
//! of a figure whose authority is the document, and the two drift.
//!
//! The dataset is the one the inflation guard polices (`eval_inflation_test`):
//! queries are written from a SCENARIO, never by summarising the section they
//! label, so a high score cannot come from the query quoting its own answer.

use crate::brain::memory_recall::{RECALL_MAX_CHARS, RECALL_MAX_SECTIONS, RECALL_MIN_SCORE};
use crate::brain::section_rank::Ranked;
use crate::cli::eval_cmd::score_rankings;
use crate::eval::recall::{RecallDataset, RecallReport};

const DATASET: &str = include_str!("../eval/fixtures/track_b_recall.json");

/// The published cutoff. Ten is the window the plan budgets, not the shipped
/// injection window (`RECALL_MAX_SECTIONS = 2`): the scorecard is measured at
/// the window a reader would actually page through.
const K: usize = 10;

fn dataset() -> RecallDataset {
    RecallDataset::from_json(DATASET).expect("the committed Track B dataset parses")
}

fn report() -> RecallReport {
    crate::eval::recall::committed_report(K)
}

/// The dataset is the shape the plan specifies: 60 positives + 15 negatives,
/// scored at k=10. A shrink here would lift every average by dropping the hard
/// cases, and no other assertion in the suite would see it.
#[test]
fn the_dataset_carries_60_positives_and_15_negatives() {
    let r = report();
    assert_eq!(r.positives(), 60, "60 positive queries");
    assert_eq!(r.negatives, 15, "15 negative queries");
    assert_eq!(r.k, K, "scored at k={K}");
}

/// The scorecard PUBLISHES all four metrics over this dataset. The renderer is
/// the only surface a reader or a later regression guard reads, so the four
/// numbers are pinned as a contract on that text: dropping one is a change to
/// what the suite reports, and it must fail here rather than pass silently.
#[test]
fn the_scorecard_publishes_recall_mrr_ndcg_and_fpr() {
    let rendered = report().render();

    for needle in ["recall@10", "MRR=", "nDCG=", "FPR="] {
        assert!(
            rendered.contains(needle),
            "the scorecard must publish `{needle}`\n{rendered}"
        );
    }
    assert!(
        rendered.contains("60 positive queries"),
        "the scorecard must state the positive count it was scored over\n{rendered}"
    );
    assert!(
        rendered.contains("/15 negative queries fired"),
        "the scorecard must state the negative count it was scored over\n{rendered}"
    );
}

/// The leg must SEE a mutated ranking. Emptying every result list is the
/// harshest mutation available without touching the dataset: positives collapse
/// to zero while the negatives score a perfect FPR, which is exactly the shape
/// a blind leg cannot distinguish from a healthy one.
#[test]
fn a_mutated_ranking_moves_the_scorecard() {
    let ds = dataset();
    let baseline = report();

    let empty: Vec<Vec<String>> = ds.cases.iter().map(|_| Vec::new()).collect();
    let mutated = score_rankings(&ds, &empty, K);

    assert!(
        mutated.aggregate.recall.abs() < f64::EPSILON,
        "an empty ranking must score zero recall, got {:.3}",
        mutated.aggregate.recall
    );
    assert!(
        mutated.aggregate.mrr.abs() < f64::EPSILON,
        "an empty ranking must score zero MRR, got {:.3}",
        mutated.aggregate.mrr
    );
    assert!(
        mutated.aggregate.recall < baseline.aggregate.recall,
        "the baseline ({:.3}) must outscore silence (0.000)",
        baseline.aggregate.recall
    );
    // Silence is perfect on negatives — the reason FPR alone is never a score.
    assert!(
        mutated.false_positive_rate().abs() < f64::EPSILON,
        "an empty ranking fires on no negative, got {:.3}",
        mutated.false_positive_rate()
    );
}

/// The shipped floor must not STARVE the large corpus (#912).
///
/// The floor is corpus calibration, not a product truth: the same number admits
/// a different fraction of a 13-section MEMORY.md and a 293-section corpus. This
/// pins the count of Track B positives that return a NON-EMPTY ranking at the
/// compiled default, so moving the constant — or changing the ranker under it —
/// fails here and forces a re-measure rather than silently emptying rankings.
///
/// Measured 14/60 at 0.38 (#912 sweep, recorded in `RECALL_MIN_SCORE`'s doc
/// comment). 0.45 starves it to 6/60, which the second assertion uses to prove
/// this pin can actually see starvation.
#[test]
fn the_shipped_floor_does_not_starve_track_b() {
    const CORPUS: &str = include_str!("../eval/fixtures/track_b_corpus.md");
    let ds = dataset();
    let ranked = Ranked::build(CORPUS);

    let fired = |floor: f64| -> usize {
        ds.cases
            .iter()
            .filter(|c| !c.is_negative())
            .filter(|c| {
                !ranked
                    .find_relevant(&c.query, RECALL_MAX_SECTIONS, RECALL_MAX_CHARS, floor)
                    .sections
                    .is_empty()
            })
            .count()
    };

    let shipped = fired(RECALL_MIN_SCORE);
    assert_eq!(
        shipped, 14,
        "the shipped floor {RECALL_MIN_SCORE} must leave 14 of 60 Track B positives \
         with a non-empty ranking; a change here is a floor or ranker move that owes \
         a re-measure of the sweep in RECALL_MIN_SCORE's doc comment"
    );

    // The pin has teeth: a starving floor reads visibly different, so this is
    // not an assertion that would pass on any input.
    assert!(
        fired(0.45) < shipped,
        "0.45 must starve Track B relative to the shipped floor, else this pin \
         cannot detect starvation at all"
    );
}
