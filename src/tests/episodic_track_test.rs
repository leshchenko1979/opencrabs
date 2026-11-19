//! Track A (episodic recall) — dataset generation and scoring (#890 step 5).
//!
//! These tests run the leg END TO END against an in-memory store: 40
//! positives + 20 negatives sampled from a corpus seeded beside the track
//! (`episodic_track::seeded_store`), then scored
//! through the same `search_slice` the CLI uses. The corpus is synthetic and
//! deliberately tiny; what is asserted is the MECHANISM (sampling counts,
//! known-item resolution, silence on negatives), never a quality number,
//! because the live leg's numbers belong to the host's own history.

use crate::eval::episodic_track::{NEGATIVES, POSITIVES, sample, score, search_slice, seeded_store};

#[test]
fn samples_forty_positives_and_twenty_negatives() {
    let conn = seeded_store(90);
    let dataset = sample(&conn, 14).expect("sample");

    assert_eq!(dataset.positives.len(), POSITIVES, "positives sampled");
    assert_eq!(dataset.negatives.len(), NEGATIVES, "negatives sampled");
    assert_eq!(dataset.slice_messages, 90, "slice covers the seeded store");
    assert_eq!(dataset.corpus_messages, 90, "corpus counted");
    assert_eq!(dataset.slice_days, 14);
    // The slice is reported beside the numbers, so it must be truthful.
    assert!(dataset.positives.iter().all(|c| !c.query.is_empty()));
    assert!(dataset.positives.iter().all(|c| c.message_id.starts_with('m')));
}

#[test]
fn known_item_queries_resolve_to_their_source_session() {
    let conn = seeded_store(90);
    let dataset = sample(&conn, 14).expect("sample");
    let report = score(&conn, &dataset, 10).expect("score");

    assert_eq!(report.positives, POSITIVES);
    assert_eq!(report.negatives, NEGATIVES);
    // Every case names a message that exists in the slice, and its query
    // shares no word with any other message — so retrieval must land in the
    // source session for every one of them.
    assert_eq!(
        report.found_session, POSITIVES,
        "every known-item query must resolve to its source session, got {}/{}",
        report.found_session, POSITIVES
    );
    assert_eq!(report.found_message, POSITIVES, "source message retrieved");
    assert!(report.mrr > 0.99, "unique queries should rank first, MRR={}", report.mrr);
    assert_eq!(report.false_positives, 0, "negatives must stay silent");
    assert_eq!(report.false_positive_rate(), 0.0);
}

#[test]
fn negatives_are_silent_and_positives_are_not() {
    let conn = seeded_store(90);
    let dataset = sample(&conn, 14).expect("sample");
    let since = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_secs() as i64
        - 14 * 86_400;

    let case = &dataset.positives[0];
    let hits = search_slice(&conn, since, &case.query, 10).expect("search");
    assert!(
        hits.iter().any(|r| r.message_id == case.message_id),
        "the sampled query must retrieve its own message"
    );

    for query in dataset.negatives.iter().take(5) {
        let hits = search_slice(&conn, since, query, 10).expect("search");
        assert!(hits.is_empty(), "negative {query:?} returned {} rows", hits.len());
    }
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
