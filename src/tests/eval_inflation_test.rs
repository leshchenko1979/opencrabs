//! The Track B inflation guard (#890 step 3).
//!
//! A retrieval dataset can score beautifully and mean nothing: if a query was
//! written by summarising the document it labels, lexical ranking finds that
//! document because the answer is already in the query. The scorecard cannot
//! see this — it reads as a good number — so the guard has to be a gate.
//!
//! Two kinds of assertion live here, and they answer different questions:
//!
//! 1. The guard WORKS — a query that quotes its target is rejected, and a
//!    scenario-phrased query is accepted.
//! 2. The committed dataset PASSES it — every one of the 60 positives, checked
//!    against the section it labels, plus every negative checked against the
//!    whole corpus (a negative that overlaps a section is not absent, it is a
//!    mislabelled positive).

use std::collections::{BTreeSet, HashMap};

use crate::brain::brain_sections::split_sections;
use crate::eval::inflation::{INFLATION_THRESHOLD, check, content_tokens};
use crate::eval::recall::RecallDataset;

const CORPUS: &str = include_str!("../eval/fixtures/track_b_corpus.md");
const DATASET: &str = include_str!("../eval/fixtures/track_b_recall.json");

/// Section heading -> body, the same way `Ranked::build` splits the corpus.
fn sections() -> HashMap<String, String> {
    split_sections(CORPUS)
        .into_iter()
        .map(|s| (s.heading.trim_start_matches('#').trim().to_string(), s.body))
        .collect()
}

fn dataset() -> RecallDataset {
    RecallDataset::from_json(DATASET).expect("the committed Track B dataset parses")
}

/// A query built from a section's own prose is exactly what the guard exists to
/// reject. This is the positive control for the guard itself: if it stopped
/// firing, every other assertion in this file would still pass while the
/// dataset quietly rotted.
#[test]
fn a_query_quoted_from_its_target_is_rejected() {
    let sections = sections();
    let body = sections
        .get("reference/ADDING_NEW_PROVIDERS · Checklist")
        .expect("the target section is in the corpus");
    // First two sentences of the target, verbatim: the failure mode stated
    // plainly — the query IS the document.
    let quoted: String = body
        .split_inclusive('.')
        .take(2)
        .collect::<String>()
        .trim()
        .to_string();
    assert!(
        quoted.len() > 40,
        "the quoted control needs real prose to quote, got {quoted:?}"
    );

    let report = check(&quoted, body, INFLATION_THRESHOLD);
    assert!(
        report.inflated,
        "a verbatim quote of its own target must be rejected: {}",
        report.describe(&quoted, INFLATION_THRESHOLD)
    );
}

/// The other direction: a query written from a scenario shares only the
/// vocabulary the subject genuinely needs, and must survive the guard.
#[test]
fn a_scenario_phrased_query_is_accepted() {
    let sections = sections();
    let body = sections
        .get("reference/templates/BOOT · Running as a Service (Daemon)")
        .expect("the target section is in the corpus");
    let query = "I want it running unattended after I log out";

    let report = check(query, body, INFLATION_THRESHOLD);
    assert!(
        !report.inflated,
        "scenario phrasing must survive the guard: {}",
        report.describe(query, INFLATION_THRESHOLD)
    );
}

/// Every committed positive, against the section it labels.
///
/// This is the assertion that makes the guard a rule rather than a habit: a new
/// query that quotes its target fails here, before it can inflate a scorecard.
#[test]
fn no_committed_query_quotes_its_own_label() {
    let sections = sections();
    let mut offenders = Vec::new();
    let mut positives = 0;

    for case in dataset().cases.iter().filter(|c| !c.is_negative()) {
        positives += 1;
        for label in &case.relevant {
            let body = sections.get(label).unwrap_or_else(|| {
                panic!(
                    "label {label:?} for query {:?} is not a section in the corpus — \
                     a label that resolves to nothing scores as a miss and hides a \
                     broken dataset",
                    case.query
                )
            });
            let report = check(&case.query, body, INFLATION_THRESHOLD);
            if report.inflated {
                offenders.push(report.describe(&case.query, INFLATION_THRESHOLD));
            }
        }
    }

    assert_eq!(positives, 60, "the dataset carries 60 positive queries");
    assert!(
        offenders.is_empty(),
        "{} query/queries quote their own target:\n{}",
        offenders.len(),
        offenders.join("\n")
    );
}

/// A negative is only a negative if the corpus cannot answer it. Checking the
/// query's overlap against EVERY section is the mechanical form of that claim:
/// a negative that shares most of its vocabulary with some section is a
/// positive wearing the wrong label, and it would inflate FPR for a reason that
/// has nothing to do with retrieval.
#[test]
fn no_negative_overlaps_any_section() {
    let sections = sections();
    let mut offenders = Vec::new();
    let mut negatives = 0;

    for case in dataset().cases.iter().filter(|c| c.is_negative()) {
        negatives += 1;
        let query_tokens = content_tokens(&case.query);
        assert!(
            !query_tokens.is_empty(),
            "negative {:?} has no content tokens — it cannot be scored",
            case.query
        );
        for (heading, body) in &sections {
            let report = check(&case.query, body, INFLATION_THRESHOLD);
            if report.inflated {
                offenders.push(format!(
                    "{} (against {heading})",
                    report.describe(&case.query, INFLATION_THRESHOLD)
                ));
            }
        }
    }

    assert_eq!(negatives, 15, "the dataset carries 15 negative queries");
    assert!(
        offenders.is_empty(),
        "{} negative/section pair(s) overlap too closely:\n{}",
        offenders.len(),
        offenders.join("\n")
    );
}

/// The dataset's own shape, so a silent truncation is a failure rather than a
/// smaller benchmark.
#[test]
fn the_dataset_is_the_committed_shape() {
    let ds = dataset();
    assert_eq!(ds.cases.len(), 75, "60 positives + 15 negatives");
    assert_eq!(
        ds.cases.iter().filter(|c| !c.is_negative()).count(),
        60,
        "the dataset carries 60 positive queries"
    );
    assert_eq!(
        ds.cases.iter().filter(|c| c.is_negative()).count(),
        15,
        "the dataset carries 15 negative queries"
    );

    let unique: BTreeSet<&str> = ds.cases.iter().map(|c| c.query.as_str()).collect();
    assert_eq!(
        unique.len(),
        ds.cases.len(),
        "duplicate queries would double-weight one label"
    );

    let sections = sections();
    assert!(
        sections.len() >= 250,
        "the corpus carries the reference documentation, got {} sections",
        sections.len()
    );
}
