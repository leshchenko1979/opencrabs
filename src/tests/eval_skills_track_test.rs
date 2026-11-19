//! Track C scoring (#890 step 4): skill SELECTION, not skill presence.
//!
//! The properties worth pinning are the ones that make the scorecard honest:
//! the vacuous full-window baseline really is 1.000, a budgeted window really
//! does move it, an unresolved label is a MISS rather than a silent exclusion,
//! and a negative can actually be reported as "no skill" — a metric that cannot
//! fail is not a measurement.

use crate::eval::skills_track::{
    SkillDataset, SkillTask, index_block_chars, index_markdown, score,
};

/// Six entries with disjoint vocabulary, so a task's match is unambiguous and
/// a rank assertion is about the ranker rather than about word overlap.
const ENTRIES: &[(&str, &str)] = &[
    ("alpha", "rebuild the kerning of a display typeface"),
    ("beta", "measure the latency of a database replica"),
    ("gamma", "translate a spoken recording into subtitles"),
    ("delta", "audit the invoice arithmetic of a supplier ledger"),
    ("epsilon", "chart the rainfall of a monsoon season"),
    ("zeta", "excavate a trench for a garden irrigation pipe"),
];

fn dataset(tasks: Vec<(&str, Option<&str>)>) -> SkillDataset {
    SkillDataset {
        name: "unit".to_string(),
        note: String::new(),
        tasks: tasks
            .into_iter()
            .map(|(task, gold)| SkillTask {
                task: task.to_string(),
                gold: gold.map(str::to_string),
            })
            .collect(),
    }
}

fn run(ds: &SkillDataset, k: usize) -> crate::eval::skills_track::SkillReport {
    run_with(ENTRIES, ds, k)
}

/// Same scoring, arbitrary index — used where a test needs a rank it can
/// construct rather than one it hopes the BM25 will produce.
fn run_with(
    entries: &[(&str, &str)],
    ds: &SkillDataset,
    k: usize,
) -> crate::eval::skills_track::SkillReport {
    let md = index_markdown(entries.iter().copied());
    let chars = index_block_chars(entries.iter().copied());
    let names: Vec<String> = entries.iter().map(|(s, _)| s.to_string()).collect();
    score(ds, &md, &names, k, chars, 2)
}

/// The index is rendered as one markdown section per skill, in the order given,
/// because a rank only means something if the position is the prompt's own.
#[test]
fn index_markdown_is_one_section_per_entry_in_order() {
    let md = index_markdown(ENTRIES.iter().copied());
    let headings: Vec<&str> = md
        .lines()
        .filter(|l| l.starts_with("## "))
        .map(|l| l.trim_start_matches("## ").trim())
        .collect();
    assert_eq!(headings, vec!["alpha", "beta", "gamma", "delta", "epsilon", "zeta"]);
    // Every description survives verbatim — a truncated index would score a
    // ranker against text the agent never sees.
    for (_, desc) in ENTRIES {
        assert!(md.contains(desc), "description dropped from the index: {desc}");
    }
}

/// C2 counts the CHARACTERS the PROMPT pays: the same `- slug: desc` lines,
/// the same 120-char clip, the same header. A number measured against a
/// different shape would be a budget for a block that does not exist.
///
/// Both sides count chars — the metric's own unit (`index_block_chars`, and
/// the prompt builder's `clip` which uses `chars().count()`/`chars().take(n)`).
/// Counting the ellipsis in BYTES here is what failed this assertion against a
/// function that was right: `…` is one char and three bytes, so `len_utf8()`
/// inflated the expectation by two and the unit mismatch read as a metric bug.
#[test]
fn index_block_chars_matches_the_emitted_form_and_clips_at_120() {
    let header = "--- Available Commands & Skills ---\n";

    let short = index_block_chars([("a", "one line")]);
    assert_eq!(short, header.chars().count() + "- a: one line\n".chars().count() + 1);

    // A long description is clipped to 120 chars plus the ellipsis, so the
    // block does not grow past the clip however long the source is.
    let long_desc = "x".repeat(500);
    let clipped = index_block_chars([("a", long_desc.as_str())]);
    let expected_line = format!("- a: {}…\n", "x".repeat(120));
    assert_eq!(clipped, header.chars().count() + expected_line.chars().count() + 1);
}

/// The vacuous baseline: when every label resolves, C1 at the FULL window is
/// 1.000 — nothing can be missing from a list that holds everything. This is
/// the number the plan records as the pre-change baseline, and pinning it here
/// stops anyone "improving" it into a measurement it never was.
#[test]
fn c1_at_full_window_is_vacuously_one() {
    let ds = dataset(vec![
        ("how do I rebuild the kerning", Some("alpha")),
        ("measure replica latency", Some("beta")),
    ]);
    let r = run(&ds, 6);
    assert_eq!(r.c1_at_full, 1.0);
    assert_eq!(r.resolved, 2);
    assert_eq!(r.positives, 2);
}

/// A budgeted window really does move the number. The rank is CONSTRUCTED
/// rather than hoped for: `beta` repeats the one query term four times, so it
/// outranks `alpha` no matter how the BM25 weights land. `alpha` is gold, so
/// k=1 must miss and k=2 must hit — a window that never loses anything is not
/// a window.
#[test]
fn c1_at_a_budgeted_window_moves_off_the_baseline() {
    let entries: &[(&str, &str)] = &[("alpha", "pipe"), ("beta", "pipe pipe pipe pipe")];
    let ds = dataset(vec![("pipe", Some("alpha"))]);
    let tight = run_with(entries, &ds, 1);
    let roomy = run_with(entries, &ds, 2);
    assert_eq!(tight.c1_at_full, 1.0, "the label resolves, so full-window is 1.0");
    assert_eq!(tight.c1_at_k, 0.0, "gold is rank 2, so k=1 must miss it");
    assert_eq!(roomy.c1_at_k, 1.0, "k=2 must contain it");
}

/// A label naming a skill the index does not carry is a MISS. Counting it as
/// "not applicable" would let the score RISE as the index shrank — the exact
/// inversion the resolved/total line exists to expose.
#[test]
fn an_unresolved_label_is_a_miss_not_an_exclusion() {
    let ds = dataset(vec![
        ("rebuild the kerning", Some("alpha")),
        ("a skill that does not exist here", Some("omega")),
    ]);
    let r = run(&ds, 6);
    assert_eq!(r.positives, 2);
    assert_eq!(r.resolved, 1);
    assert_eq!(r.c1_at_full, 0.5, "the unresolvable label must drag the score");
}

/// C4 has to be able to report a CLEAN negative. If every task counts as a
/// false selection, the metric reads 1.000 no matter what the ranking does.
#[test]
fn a_negative_with_no_overlap_is_not_a_false_selection() {
    let ds = dataset(vec![("quarterly payroll tax filing deadline", None)]);
    let r = run(&ds, 6);
    assert_eq!(r.negatives, 1);
    assert_eq!(r.c4_false_selection, 0.0);
}

/// ...and it has to be able to report a DIRTY one, or a clean result proves
/// nothing. The negative mashes two unrelated domains together: no single
/// skill claims it, yet the ranker returns a confident winner because nothing
/// in a flat index can say "this is two different things".
#[test]
fn a_negative_sharing_index_vocabulary_is_a_false_selection() {
    let entries: &[(&str, &str)] = &[
        ("alpha", "kerning display typeface rebuild"),
        ("beta", "replica latency database measure"),
    ];
    let ds = dataset(vec![(
        "kerning display typeface rebuild replica latency database measure",
        None,
    )]);
    let r = run_with(entries, &ds, 2);
    assert_eq!(r.c4_false_selection, 1.0);
}

/// Mutation guard: the metric must be wired to the LABELS, not to the text.
/// Swapping which skill is gold, with the tasks untouched, has to move C1@1 —
/// otherwise a wrong label would score the same as a right one.
#[test]
fn c1_at_one_follows_the_labels_not_the_tasks() {
    let right = dataset(vec![("rebuild the kerning of a typeface", Some("alpha"))]);
    let wrong = dataset(vec![("rebuild the kerning of a typeface", Some("zeta"))]);
    assert_eq!(run(&right, 1).c1_at_k, 1.0);
    assert_eq!(run(&wrong, 1).c1_at_k, 0.0);
}

/// C3 is a RATIO of the index, so it is reported against the index size — a
/// count alone cannot show that 19 of 35 lines ever earn their place.
#[test]
fn c3_coverage_is_relative_to_the_index() {
    let ds = dataset(vec![("rebuild the kerning", Some("alpha"))]);
    let r = run(&ds, 6);
    assert_eq!(r.full, 6);
    assert_eq!(r.c3_employed, 2);
    assert!((r.c3_coverage - (2.0 / 6.0)).abs() < 1e-9);
}

/// The rendered card carries the vacuous baseline NEXT TO the real number, so
/// a reader cannot mistake one for the other.
#[test]
fn the_rendered_card_labels_the_vacuous_baseline() {
    let ds = dataset(vec![("rebuild the kerning", Some("alpha"))]);
    let card = run(&ds, 5).render();
    assert!(card.contains("C1@5"), "{card}");
    assert!(card.contains("C1@6"), "{card}");
    assert!(card.contains("VACUOUS"), "{card}");
    assert!(card.contains("C4"), "{card}");
    assert!(card.contains("resolved"), "{card}");
}
