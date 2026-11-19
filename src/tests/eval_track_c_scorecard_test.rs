//! The Track C scorecard over the COMMITTED dataset (#890 step 4).
//!
//! `eval_skills_track_test` pins the metric MODULE with synthetic entries; this
//! file scores the dataset that actually ships — 35 positives (one per skill on
//! the ops profile) + 12 negatives — and pins the two things the plan asks for:
//!
//! 1. **The scorecard reports all four metrics.** C1 at the budgeted window, C2
//!    (index characters), C3 (employment, wired from `session_seen_skills` by
//!    the CLI) and C4 on the negatives. The renderer is the only surface a
//!    reader sees, so dropping one has to fail here.
//! 2. **`k = 35` reproduces the vacuous 1.000** recorded as the pre-change
//!    baseline. This is arithmetic, not retrieval: at a window as wide as the
//!    index, every resolvable label is inside it. It is pinned precisely
//!    BECAUSE it is vacuous — a baseline that could drift is a baseline someone
//!    would later mistake for a measurement.
//!
//! The index here is built from the dataset's own 35 gold slugs. That is the
//! only index this module can construct without a live profile, and it is the
//! right one for these assertions: the baseline question is "does a full window
//! resolve every label", which depends on the LABELS, not on ranking quality.
//! Ranking quality is what `k = 5` and C4 measure, and those are covered by the
//! synthetic-entry tests where the vocabulary is controlled.

use std::collections::BTreeSet;

use crate::eval::skills_track::SkillDataset;

const DATASET: &str = include_str!("../eval/fixtures/track_c_skills.json");

fn dataset() -> SkillDataset {
    SkillDataset::from_json(DATASET).expect("the committed Track C dataset parses")
}

/// The 35 gold slugs, deduplicated and sorted — the index the dataset describes.
fn slugs() -> Vec<String> {
    let set: BTreeSet<String> = dataset()
        .tasks
        .iter()
        .filter_map(|t| t.gold.clone())
        .collect();
    set.into_iter().collect()
}

/// The fixture leg itself: one builder, in the module, so this test and the
/// baseline guard score the same index.
fn report_at(k: usize) -> crate::eval::skills_track::SkillReport {
    crate::eval::skills_track::committed_report_at(k)
}

/// The committed dataset is the shape the plan specifies: 35 skills + 12
/// negatives. A shrink here would lift C1 by dropping hard tasks.
#[test]
fn the_dataset_carries_35_positives_and_12_negatives() {
    let ds = dataset();
    assert_eq!(ds.positives(), 35, "one positive per skill");
    assert_eq!(ds.negatives(), 12, "12 negative tasks");
    assert_eq!(ds.tasks.len(), 47, "35 + 12");
    assert_eq!(slugs().len(), 35, "35 distinct gold slugs");
}

/// **The baseline.** `k = 35` — a window as wide as the index — resolves every
/// label, so C1 is 1.000 by construction. Recorded as the pre-change number the
/// budgeted window has to be judged against.
#[test]
fn k_35_reproduces_the_vacuous_1000_baseline() {
    let r = report_at(35);

    assert_eq!(r.full, 35, "the index is 35 entries");
    assert_eq!(
        r.resolved, 35,
        "every gold label must resolve against the index it names"
    );
    assert_eq!(
        r.c1_at_full,
        1.0,
        "C1@35 is the VACUOUS baseline and must be exactly 1.000, got {:.3}\n{}",
        r.c1_at_full,
        r.render()
    );
    assert_eq!(
        r.c1_at_k, 1.0,
        "at k=35 the budgeted window equals the full window, so C1@k must also be 1.000"
    );
}

/// The scorecard PUBLISHES all four metrics. C1 and C4 come from the ranking,
/// C2 from the emitted block, C3 from the live session record — different
/// sources, one card, and a reader quotes the card.
#[test]
fn the_scorecard_reports_c1_c2_c3_and_c4() {
    let r = report_at(5);
    let card = r.render();

    for needle in ["C1@5", "C2", "C3", "C4"] {
        assert!(
            card.contains(needle),
            "the scorecard must report `{needle}`\n{card}"
        );
    }
    assert!(
        card.contains("VACUOUS baseline"),
        "the card must label the full-window number as vacuous, or it reads as a measurement\n{card}"
    );
    assert!(
        card.contains("chars of prompt"),
        "C2 must be reported in the unit it measures\n{card}"
    );
    assert!(
        card.contains("12 negatives"),
        "C4 must state how many negatives it was wrong on\n{card}"
    );
    assert!(
        card.contains("resolved"),
        "the card must state how many labels resolved\n{card}"
    );
}

/// A budgeted window is a DIFFERENT number from the vacuous baseline, or the
/// budget is not being measured. With the gold slugs as the only vocabulary,
/// some task will fall outside the first five — if it did not, `k` would be
/// doing nothing and the target metric would be the baseline wearing a hat.
#[test]
fn k_5_is_not_the_vacuous_baseline() {
    let tight = report_at(5);
    let wide = report_at(35);

    assert_eq!(tight.k, 5, "the tight window is k=5");
    assert!(
        tight.c1_at_k <= wide.c1_at_k,
        "a narrower window cannot score better than the full one: k=5 {:.3} vs k=35 {:.3}",
        tight.c1_at_k,
        wide.c1_at_k
    );
    assert!(
        tight.c1_at_k < wide.c1_at_k,
        "k=5 must move off the vacuous 1.000 baseline, got {:.3} — if the budget \
         changes nothing, C1 is not measuring a window\n{}",
        tight.c1_at_k,
        tight.render()
    );
}
