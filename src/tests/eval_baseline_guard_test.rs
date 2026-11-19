//! The committed per-track baselines and the regression guard over them
//! (#890 step 6).
//!
//! Each track has ONE committed baseline in `src/eval/baselines/`. What those
//! files pin is a deliberate choice, and the choice is the point:
//!
//! * the dataset SHAPE — a shrink lifts every average by dropping the hard
//!   cases, and no other assertion in the suite would see it;
//! * the metric SURFACE — a leg that stops printing one metric while still
//!   reporting the others reads as green;
//! * a FLOOR on the ranking-sensitive figures, set where a ranker that returns
//!   nothing falls through it.
//!
//! They do NOT pin the scores themselves. A metric value is reproducible only
//! against a frozen corpus AND a frozen ranker, so a pinned figure would make
//! every legitimate improvement look like a regression, and it would be a
//! second copy of a number whose authority is a results document. The published
//! figures live in `src/eval/results/` (plan step 7).
//!
//! ## A floor nobody has tested is a floor that proves nothing
//!
//! Every floor here is checked against a BROKEN arm in the same test: the
//! ranking is emptied and the same check function must reject it. Without that
//! control a guard could pass on a leg that had stopped measuring anything —
//! the failure class it exists to catch is exactly the one where both sides of
//! the comparison are zero.
//!
//! ## Live legs stay OUT of CI, and that is declared, not implied
//!
//! Track A's real-store leg and Track C's C3 employment figure read the host's
//! own database through the CLI (`src/cli/eval_cmd.rs`, opened READ-ONLY). CI
//! has no such store and must not pretend to: each baseline names its live legs
//! in `live_legs`, and the last test here fails if a fixture leg ever starts
//! reading the live database.

use std::collections::BTreeMap;

use crate::eval::baseline::TrackBaseline;
use crate::eval::episodic_track::{NEGATIVES, POSITIVES, committed_report as episodic_report};
use crate::eval::recall::committed_report as recall_report;
use crate::eval::skills_track::committed_report_at as skills_report;

const TRACK_A: &str = include_str!("../eval/baselines/track-a.json");
const TRACK_B: &str = include_str!("../eval/baselines/track-b.json");
const TRACK_C: &str = include_str!("../eval/baselines/track-c.json");

fn baseline(json: &str, track: &str) -> TrackBaseline {
    let b = TrackBaseline::from_json(json).expect("the committed baseline parses");
    assert_eq!(b.track, track, "the baseline names the track it guards");
    b
}

/// Track A: the in-memory fixture leg. Its 90-message corpus has one unique
/// vocabulary per message, so a working retriever resolves EVERY known-item
/// query — which is why this track's floors are exact rather than a token 0.01.
#[test]
fn track_a_matches_its_committed_baseline() {
    let b = baseline(TRACK_A, "A");
    let report = episodic_report(b.window);
    let card = report.render();

    assert!(
        b.shape_violations(report.positives, report.negatives).is_empty(),
        "Track A dataset shape moved: {:?}",
        b.shape_violations(report.positives, report.negatives)
    );
    assert_eq!((report.positives, report.negatives), (POSITIVES, NEGATIVES));

    let missing = b.missing_metrics(&card);
    assert!(missing.is_empty(), "Track A stopped publishing {missing:?}\n{card}");

    let mut measured = BTreeMap::new();
    measured.insert("recall_at_k".to_string(), report.recall_at_k());
    measured.insert("mrr".to_string(), report.mrr);
    let violations = b.floor_violations(&measured);
    assert!(violations.is_empty(), "Track A regressed: {violations:?}\n{card}");
}

/// Track B: 60+15 over the committed corpus. Floors are set where silence
/// cannot reach them; the values themselves are published, not pinned.
#[test]
fn track_b_matches_its_committed_baseline() {
    let b = baseline(TRACK_B, "B");
    let report = recall_report(b.window);
    let card = report.render();

    assert!(
        b.shape_violations(report.positives(), report.negatives).is_empty(),
        "Track B dataset shape moved: {:?}",
        b.shape_violations(report.positives(), report.negatives)
    );
    assert_eq!(report.k, b.window, "scored at the baseline's window");

    let missing = b.missing_metrics(&card);
    assert!(missing.is_empty(), "Track B stopped publishing {missing:?}\n{card}");

    let mut measured = BTreeMap::new();
    measured.insert("recall".to_string(), report.aggregate.recall);
    measured.insert("mrr".to_string(), report.aggregate.mrr);
    measured.insert("ndcg".to_string(), report.aggregate.ndcg);
    let violations = b.floor_violations(&measured);
    assert!(violations.is_empty(), "Track B regressed: {violations:?}\n{card}");
}

/// Track C: the full window is the vacuous baseline, and it is pinned at
/// exactly 1.000 BECAUSE it is vacuous — a figure that could drift is one a
/// reader would later mistake for a measurement.
#[test]
fn track_c_matches_its_committed_baseline() {
    let b = baseline(TRACK_C, "C");
    let report = skills_report(b.window);
    let card = report.render();

    assert!(
        b.shape_violations(report.positives, report.negatives).is_empty(),
        "Track C dataset shape moved: {:?}",
        b.shape_violations(report.positives, report.negatives)
    );

    let missing = b.missing_metrics(&card);
    assert!(missing.is_empty(), "Track C stopped publishing {missing:?}\n{card}");

    let mut measured = BTreeMap::new();
    measured.insert("c1_at_full".to_string(), report.c1_at_full);
    let violations = b.floor_violations(&measured);
    assert!(violations.is_empty(), "Track C regressed: {violations:?}\n{card}");
}

/// **The positive control.** A guard is only worth what it can reject, so the
/// broken arm runs here: the figures a ranker-that-returns-nothing produces must
/// fall through every track's floor. If this test passes while the floors above
/// also pass, the floors are measuring nothing.
///
/// The control drives the CHECKER with those figures rather than re-running a
/// broken retriever, because that is the half under test here — whether the
/// guard rejects a collapsed measurement. That the leg actually PRODUCES
/// collapsed figures from a mutated ranking is the other half, and it is proved
/// by `a_mutated_ranking_moves_the_scorecard` over Track B: mutated rankings →
/// zeroed metrics → rejected here → the gate goes RED.
#[test]
fn a_broken_ranking_falls_through_every_floor() {
    // Track A: a ranker that returns nothing retrieves no known item.
    let mut silent = BTreeMap::new();
    silent.insert("recall_at_k".to_string(), 0.0);
    silent.insert("mrr".to_string(), 0.0);
    let a = baseline(TRACK_A, "A");
    let a_broken = a.floor_violations(&silent);
    assert!(
        !a_broken.is_empty(),
        "Track A's floors did not reject silence — they guard nothing"
    );

    // Track B: emptying every ranking collapses the positive metrics to zero.
    let b = baseline(TRACK_B, "B");
    let mut empty = BTreeMap::new();
    empty.insert("recall".to_string(), 0.0);
    empty.insert("mrr".to_string(), 0.0);
    empty.insert("ndcg".to_string(), 0.0);
    assert!(
        !b.floor_violations(&empty).is_empty(),
        "Track B's floors did not reject silence — they guard nothing"
    );

    // Track C: a ranker that returns nothing resolves no label at all, so the
    // vacuous 1.000 drops to 0.000 and the pin must catch it.
    let c = baseline(TRACK_C, "C");
    let mut unresolved = BTreeMap::new();
    unresolved.insert("c1_at_full".to_string(), 0.0);
    assert!(
        !c.floor_violations(&unresolved).is_empty(),
        "Track C's pin did not reject an unresolved index — it guards nothing"
    );

    // And a figure the leg never measured is a violation too, not a pass.
    assert!(
        !c.floor_violations(&BTreeMap::new()).is_empty(),
        "an unmeasured figure must not pass a floor"
    );
}

/// The live legs are DECLARED, and no fixture leg may quietly become one.
///
/// CI has no live profile database, so a fixture leg that started reading one
/// would either fail for the wrong reason or — worse — pass on whatever the
/// runner happened to have. The live legs are the CLI commands, which is why
/// their names are recorded in the baselines rather than left to inference.
#[test]
fn live_legs_are_declared_and_stay_out_of_ci() {
    let a = baseline(TRACK_A, "A");
    let c = baseline(TRACK_C, "C");
    let b = baseline(TRACK_B, "B");

    assert!(
        !a.live_legs.is_empty(),
        "Track A's real-store leg must be declared, or its absence from CI is implied rather than stated"
    );
    assert!(
        !c.live_legs.is_empty(),
        "Track C's C3 employment needs `session_seen_skills` and must be declared"
    );
    assert!(
        b.live_legs.is_empty(),
        "Track B is fully committed (corpus + dataset), so it has no live leg to declare"
    );

    // The live legs live in the CLI, which opens the profile database. A
    // baseline naming it would make a CI artifact depend on the runner's own
    // store, which is the change this assertion exists to catch.
    for (name, src) in [("track-a", TRACK_A), ("track-b", TRACK_B), ("track-c", TRACK_C)] {
        assert!(
            !src.contains("opencrabs.db"),
            "baseline {name} must not name the live database: it is a CI artifact"
        );
    }
}
