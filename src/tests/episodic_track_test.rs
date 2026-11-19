//! Track A (episodic recall) — dataset generation and scoring (#890 step 5).
//!
//! These tests run the leg END TO END against an in-memory store: 40 positives
//! and 20 negatives sampled from the corpus seeded beside the track
//! (`episodic_track::seeded_store`), then scored through the same
//! `search_slice` the CLI uses.
//!
//! Since #899 the corpus is deliberately HARD: its messages draw their words
//! from a handful of topical clusters, so a query matches a neighbourhood of a
//! dozen messages instead of exactly one. What these tests assert is that the
//! fixture can FAIL — recall and MRR strictly below 1.000, at least one
//! negative firing — because a leg that publishes 1.000/1.000/0.000 is
//! measuring its own construction, not the search.

use crate::eval::episodic_track::{
    NEGATIVES, POSITIVES, committed_report, sample, score, search_slice, seeded_store,
};

/// The committed report is what the baseline guard reads, so this is the
/// measurement the floors are set against. Both ranking figures must be
/// STRICTLY below perfect: a shared vocabulary that a query can never miss is
/// the tautology #899 removed.
#[test]
fn committed_report_scores_below_perfect() {
    let report = committed_report(10);
    let card = report.render();

    assert!(
        report.recall_at_k() < 1.0,
        "recall must be able to fail, got {}\n{card}",
        report.recall_at_k()
    );
    assert!(
        report.mrr < 1.0,
        "MRR must be able to fail, got {}\n{card}",
        report.mrr
    );
    // A leg that scores zero everywhere is equally uninformative — it would
    // mean the queries are unanswerable rather than merely hard.
    assert!(
        report.found_message > 0,
        "no positive was retrieved at all — the leg is impossible, not hard\n{card}"
    );
}

/// The published scorecard quotes exact figures, so the gate is where they are
/// MEASURED rather than asserted in prose (#899). A retriever change that
/// moves the numbers must move this test and the card together — which is the
/// whole point of publishing a number instead of a feeling.
#[test]
fn the_committed_card_publishes_the_numbers_the_scorecard_states() {
    let report = committed_report(10);
    let card = report.render();

    assert_eq!(report.positives, 40, "positive count\n{card}");
    assert_eq!(report.negatives, 20, "negative count\n{card}");
    // 32/40 positives land their source message somewhere in the top 10.
    assert_eq!(
        report.found_message, 32,
        "recall is not the published 0.800\n{card}"
    );
    assert_eq!(report.recall_at_k(), 0.8, "recall\n{card}");
    // 10/20 negatives fire: the near-miss class, and only it.
    assert_eq!(
        report.false_positives, 10,
        "FPR is not the published 0.500\n{card}"
    );
    assert_eq!(report.false_positive_rate(), 0.5, "FPR\n{card}");
    // MRR is a rank-weighted sum, so it is pinned at the precision the
    // scorecard quotes. If a ranker change moves it, the card and this
    // assertion move together.
    assert_eq!(
        format!("{:.3}", report.mrr),
        "0.227",
        "MRR is not the published 0.227\n{card}"
    );
}

/// Negatives are accepted in two classes (#899): near-misses that share
/// vocabulary with a non-gold message, and genuine absences. A set that could
/// only ever be silent measured nothing, so FPR must be above zero — and, so
/// the metric is a measurement rather than a constant, some negatives must
/// still stay quiet.
#[test]
fn near_miss_negatives_fire_and_absent_ones_stay_silent() {
    let report = committed_report(10);
    let card = report.render();

    assert!(
        report.false_positives > 0,
        "FPR is pinned at zero — the negatives cannot fail\n{card}"
    );
    assert!(
        report.false_positives < report.negatives,
        "every negative fired — the negatives cannot pass either\n{card}"
    );
    assert!(report.false_positive_rate() > 0.0);
}

/// The fixture the sampler reads must be large enough for a query to have
/// neighbours. A handful of messages would put the tautology back by accident.
#[test]
fn the_sampler_reads_a_thousand_message_store() {
    let conn = seeded_store(1000);
    let dataset = sample(&conn, 14).expect("sample");

    assert_eq!(
        dataset.slice_messages, 1000,
        "slice covers the seeded store"
    );
    assert_eq!(dataset.corpus_messages, 1000, "corpus counted");
    assert!(
        dataset.slice_messages >= 1000,
        "the fixture must be large enough to make a query ambiguous, got {}",
        dataset.slice_messages
    );
}

/// A positive query must select a NEIGHBOURHOOD, not a single row. This is the
/// positive control for the tautology: if every query returned exactly its own
/// message, recall would be 1.000 by construction.
#[test]
fn positive_queries_select_a_neighbourhood_not_a_row() {
    let conn = seeded_store(1000);
    let dataset = sample(&conn, 14).expect("sample");
    let since = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_secs() as i64
        - 14 * 86_400;

    let multi = dataset
        .positives
        .iter()
        .filter(|case| {
            search_slice(&conn, since, &case.query, 10)
                .expect("search")
                .len()
                > 1
        })
        .count();

    assert!(
        multi >= POSITIVES / 2,
        "only {multi}/{POSITIVES} queries matched more than one message — \
         the fixture is a lookup table again"
    );
}

/// Sampling is deterministic: the same slice yields the same dataset, query for
/// query, so a CI run and a local run score the same corpus.
#[test]
fn the_same_slice_yields_the_same_dataset() {
    let conn = seeded_store(1000);
    let first = sample(&conn, 14).expect("sample");
    let second = sample(&conn, 14).expect("sample");

    assert_eq!(first.positives.len(), second.positives.len());
    for (a, b) in first.positives.iter().zip(second.positives.iter()) {
        assert_eq!(a.message_id, b.message_id, "same gold message");
        assert_eq!(a.query, b.query, "same query for the same gold");
    }
    assert_eq!(first.negatives, second.negatives, "same negatives");

    let a = score(&conn, &first, 10).expect("score");
    let b = score(&conn, &second, 10).expect("score");
    assert_eq!(a.found_message, b.found_message);
    assert_eq!(a.false_positives, b.false_positives);
    assert!((a.mrr - b.mrr).abs() < f64::EPSILON, "MRR is stable");
}

#[test]
fn samples_forty_positives_and_twenty_negatives() {
    let conn = seeded_store(1000);
    let dataset = sample(&conn, 14).expect("sample");

    assert_eq!(dataset.positives.len(), POSITIVES, "positives sampled");
    assert_eq!(dataset.negatives.len(), NEGATIVES, "negatives sampled");
    assert_eq!(dataset.slice_days, 14);
    // The slice is reported beside the numbers, so it must be truthful.
    assert!(dataset.positives.iter().all(|c| !c.query.is_empty()));
    assert!(
        dataset
            .positives
            .iter()
            .all(|c| c.message_id.starts_with('m'))
    );
}

#[test]
fn too_small_a_slice_is_an_error_not_a_zero() {
    // Ten messages cannot yield forty cases. Reporting a score off them would
    // be a number with a silent denominator, which is the failure mode this
    // whole issue exists to remove.
    let conn = seeded_store(10);
    let err = sample(&conn, 14).expect_err("must refuse a slice too small to sample");
    assert!(
        err.to_string().contains("widen --slice-days"),
        "unexpected error: {err}"
    );
}

#[test]
fn slice_bound_excludes_older_messages() {
    let conn = seeded_store(90);
    // Push ten messages far into the past: they must leave the slice.
    conn.execute(
        "UPDATE messages SET created_at = created_at - 90 * 86400 WHERE id IN \
         (SELECT id FROM messages ORDER BY id LIMIT 10)",
        [],
    )
    .expect("age rows");
    let dataset = sample(&conn, 14).expect("sample");
    assert_eq!(dataset.corpus_messages, 90, "corpus is the whole table");
    assert_eq!(dataset.slice_messages, 80, "slice excludes the aged rows");
}
