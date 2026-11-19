//! Why Track B published 0.017: the decomposition (#890 follow-up).
//!
//! The scorecard reported P@10 = R@10 = MRR = nDCG = 0.017 and said honestly
//! that it could not tell a weak ranker from a broken metric. It can, and this
//! file is that answer: the 0.017 was three defects stacked, each one able to
//! destroy the number on its own.
//!
//! The first fix (the heading boost in `section_rank`) lifted the same leg to
//! 0.083 with depth, corpus and threshold untouched. The decomposition below is
//! still the map of what remains: the injection window is still 2 sections
//! wide, and the floor still starves most of a 293-section corpus.
//!
//! 1. DEPTH — the leg asks for `RECALL_MAX_SECTIONS` (2) sections no matter
//!    what `k` says, then scores that two-element list at k=10. `k` is inert.
//! 2. THRESHOLD — with the shipped `RECALL_MIN_SCORE` on a 293-section corpus,
//!    most positives come back EMPTY. The 0.017 is the one query that survived.
//! 3. RANKER — with the floor removed and full depth, every gold is
//!    retrievable, but deep: the ranker is weak, not broken.
//!
//! Each assertion below pins one of those against the SHIPPED functions, so
//! the claim cannot drift into prose that nobody re-checks. The numbers are
//! properties of the committed fixture pair, which is why they are safe to pin:
//! changing the dataset or the corpus moves them and the gate says so.

use crate::brain::memory_recall::{RECALL_MAX_CHARS, RECALL_MAX_SECTIONS, RECALL_MIN_SCORE};
use crate::brain::section_rank::Ranked;
use crate::cli::eval_cmd::score_recall;
use crate::eval::recall::RecallDataset;

const DATASET: &str = include_str!("../eval/fixtures/track_b_recall.json");
const CORPUS: &str = include_str!("../eval/fixtures/track_b_corpus.md");

/// The published cutoff the scorecard is measured at.
const PUBLISHED_K: usize = 10;
/// The cutoff the pipeline actually injects at.
const INJECTED_K: usize = 2;

fn dataset() -> RecallDataset {
    RecallDataset::from_json(DATASET).expect("the committed Track B dataset parses")
}

/// Section ids as the scorecard names them: heading text without the hashes.
fn ids(sections: &[crate::brain::brain_sections::Section]) -> Vec<String> {
    sections
        .iter()
        .map(|s| s.heading.trim_start_matches('#').trim().to_string())
        .collect()
}

/// The shipped leg reproduces the CURRENT scorecard. If this moves, the
/// scorecard document is describing a number the code no longer produces.
///
/// It read `1/60` when this file was written: that was the figure published in
/// `track-b-recall-v1.md`, and the decomposition below is why. The heading
/// boost (`section_rank::HEADING_BOOST`) then lifted the same leg to `5/60`
/// without touching depth or the corpus, which is the smallest possible proof
/// that the ranker — not the metric — was the lever.
#[test]
fn the_shipped_leg_reproduces_the_current_scorecard() {
    let report = score_recall(&dataset(), CORPUS, PUBLISHED_K);
    // Five of sixty positives now retrieve their single gold section at rank
    // one, so every metric lands on exactly 5/60.
    let expected = 5.0 / 60.0;
    assert!(
        (report.aggregate.recall - expected).abs() < 1e-9,
        "current recall@10 was {:.4}, expected {:.4}",
        report.aggregate.recall,
        expected
    );
    assert!(
        (report.aggregate.mrr - expected).abs() < 1e-9,
        "current MRR was {:.4}, expected {:.4}",
        report.aggregate.mrr,
        expected
    );
    assert_eq!(report.positives(), 60, "60 positive queries");
    assert_eq!(report.negatives, 15, "15 negative queries");
}

/// DEFECT 1 — `k` never reaches the retriever, so a k=10 scorecard is a
/// two-deep measurement wearing a ten label.
///
/// The proof is exact rather than approximate: because the returned list can
/// never exceed two entries, scoring it at k=10 and at k=2 produces the SAME
/// aggregate. A metric that is identical at two different cutoffs is not
/// measuring the cutoff it names.
#[test]
fn the_k10_score_is_identical_to_the_k2_score_because_k_is_inert() {
    let ds = dataset();
    let at_two = score_recall(&ds, CORPUS, INJECTED_K);
    let at_ten = score_recall(&ds, CORPUS, PUBLISHED_K);

    assert_eq!(
        at_ten.aggregate, at_two.aggregate,
        "k is inert: the leg caps the list at {RECALL_MAX_SECTIONS} sections regardless of k, \
         so k=10 and k=2 score identically"
    );
    assert_ne!(
        at_ten.k, at_two.k,
        "the report still records which k was asked for"
    );
}

/// DEFECT 1, second half: the cap is applied before the metrics, so no query
/// can ever return more than `RECALL_MAX_SECTIONS` sections.
#[test]
fn no_query_can_return_more_than_the_injection_window() {
    let ds = dataset();
    let ranked = Ranked::build(CORPUS);
    let mut longest = 0usize;
    for case in &ds.cases {
        let matches = ranked.find_relevant(
            &case.query,
            RECALL_MAX_SECTIONS,
            RECALL_MAX_CHARS,
            RECALL_MIN_SCORE,
        );
        longest = longest.max(matches.sections.len());
    }
    assert!(
        longest <= RECALL_MAX_SECTIONS,
        "a query returned {longest} sections against a cap of {RECALL_MAX_SECTIONS}"
    );
}

/// DEFECT 2 — the shipped floor starves the corpus.
///
/// On 293 sections, `RECALL_MIN_SCORE` rejects most positives outright: they
/// come back with an empty ranking, which scores zero on every metric. This is
/// the largest single contributor to the 0.017, and it is a PRODUCTION
/// threshold, so the same starvation applies to automatic recall on every turn.
#[test]
fn the_shipped_threshold_starves_most_positives_into_silence() {
    let ds = dataset();
    let ranked = Ranked::build(CORPUS);

    let mut positives = 0usize;
    let mut empty = 0usize;
    for case in &ds.cases {
        if case.is_negative() {
            continue;
        }
        positives += 1;
        let matches = ranked.find_relevant(
            &case.query,
            RECALL_MAX_SECTIONS,
            RECALL_MAX_CHARS,
            RECALL_MIN_SCORE,
        );
        if matches.sections.is_empty() {
            empty += 1;
        }
    }

    assert_eq!(positives, 60, "60 positives in the committed dataset");
    // 52 of 60 when this file was written; 46 after the heading boost. The
    // improvement is real and nowhere near sufficient, which is the point: the
    // floor is a PRODUCTION context budget on a 293-section corpus, and the
    // remainder of the gap is the ranker's, not the threshold's.
    assert!(
        empty >= 45,
        "the shipped floor must still starve most positives, saw {empty} of {positives}"
    );
}

/// DEFECT 3 — with the floor removed and the depth lifted, the gold is always
/// retrievable. So the ranker is weak, not broken: the retrieval defect is the
/// threshold and the depth, and the remaining gap is ranking quality.
#[test]
fn with_the_floor_removed_every_gold_is_retrievable_but_ranked_deep() {
    let ds = dataset();
    let ranked = Ranked::build(CORPUS);
    let depth = ranked.len();

    let mut positives = 0usize;
    let mut found = 0usize;
    let mut reciprocal_rank = 0.0f64;
    for case in &ds.cases {
        if case.is_negative() {
            continue;
        }
        positives += 1;
        let matches = ranked.find_relevant(&case.query, depth, usize::MAX, 0.0);
        let ranked_ids = ids(&matches.sections);
        if let Some(position) = ranked_ids.iter().position(|id| case.relevant.contains(id)) {
            found += 1;
            reciprocal_rank += 1.0 / (position as f64 + 1.0);
        }
    }

    assert_eq!(
        found, positives,
        "every gold must be retrievable at full depth"
    );
    let mrr = reciprocal_rank / positives as f64;
    assert!(
        mrr > 0.15,
        "gold is ranked deeper than the decomposition measured (MRR {mrr:.3})"
    );
    assert!(
        mrr < 0.40,
        "the ranker is not supposed to be good yet (MRR {mrr:.3}); if it improved, \
         the Track B scorecard and this decomposition both need re-measuring"
    );
}
