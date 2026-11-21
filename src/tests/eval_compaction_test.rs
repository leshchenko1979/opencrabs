//! Tests for the compaction-fidelity harness + dataset format (#621).

use crate::eval::compaction::CompactionDataset;
use crate::eval::replay::ReplayProvider;
use crate::eval::scorer::ProviderJudge;

#[test]
fn seed_dataset_loads_with_probes_and_messages() {
    let ds = CompactionDataset::seed();
    assert_eq!(ds.name, "seed-coding-session");
    assert_eq!(ds.probes.len(), 4);
    assert_eq!(ds.messages().len(), 5);
    // Every probe carries offline keywords.
    assert!(ds.probes.iter().all(|p| !p.expect_keywords.is_empty()));
}

#[test]
fn faithful_summary_scores_full_marks() {
    let ds = CompactionDataset::seed();
    // A summary that mentions every required fact keyword.
    let good = "Preferences: tabs indentation. Edited net/client.rs (retry refactor). \
                Hit error E0433 on the import. Pending: update the CHANGELOG.";
    let card = ds.keyword_scorecard(good);
    assert_eq!(card.overall(), 1.0);
    assert!(card.passes(1.0));
    assert_eq!(card.per_dimension.len(), 4);
}

#[test]
fn lossy_summary_is_penalized_per_dimension() {
    let ds = CompactionDataset::seed();
    // Drops the pending CHANGELOG task and the E0433 error.
    let lossy = "Preferences: tabs. Edited net/client.rs.";
    let card = ds.keyword_scorecard(lossy);
    assert_eq!(card.passed, 2);
    assert_eq!(card.total, 4);
    assert_eq!(card.per_dimension["pending_tasks"].passed, 0);
    assert_eq!(card.per_dimension["errors"].passed, 0);
    assert_eq!(card.per_dimension["user_prefs"].passed, 1);
    // The failing probe explains what was missing.
    let changelog = card
        .results
        .iter()
        .find(|(q, _)| q.dimension == "pending_tasks")
        .unwrap();
    assert!(
        changelog
            .1
            .explanation
            .as_deref()
            .unwrap()
            .contains("CHANGELOG")
    );
}

#[test]
fn empty_summary_survives_nothing() {
    let ds = CompactionDataset::seed();
    let card = ds.keyword_scorecard("");
    assert_eq!(card.overall(), 0.0);
}

#[tokio::test]
async fn judge_path_grades_summary_offline_via_replay() {
    let ds = CompactionDataset::seed();
    // Scripted judge verdicts (one per probe): YES, NO, YES, YES.
    let fixture = r#"{
        "model": "judge",
        "turns": [
            { "text": "YES" },
            { "text": "NO" },
            { "text": "YES" },
            { "text": "YES" }
        ]
    }"#;
    let provider = ReplayProvider::from_json(fixture).unwrap();
    let judge = ProviderJudge::new(&provider, "judge");
    let card = ds.judge_scorecard(&judge, "some produced summary").await;
    assert_eq!((card.passed, card.total), (3, 4));
    assert_eq!(provider.turns_consumed(), 4);
}

// ---- #929: the prompt/eval contract gate -------------------------------------

use crate::eval::baseline::Baseline;
use crate::eval::compaction_gate::{self, DIMENSION_SECTIONS, REFERENCE_SUMMARY};

/// A prompt body carrying every heading the contract maps. Deliberately a
/// FIXTURE, not the production prompt: the control must read a stable
/// declaration of where each dimension belongs, so renaming a heading in the
/// real prompt fails the gate instead of silently moving the control with it.
fn full_prompt_body() -> String {
    DIMENSION_SECTIONS
        .iter()
        .map(|(_, heading)| *heading)
        .collect::<Vec<_>>()
        .join("\n\n")
}

#[test]
fn gate_passes_when_prompt_serves_every_dimension() {
    let ds = CompactionDataset::seed();
    let out = compaction_gate::run(&ds, &full_prompt_body(), REFERENCE_SUMMARY);
    assert!(
        out.holds(),
        "expected the gate to hold; failures: {:?}",
        out.failures()
    );
    assert_eq!(out.structural.overall(), 1.0);
    assert_eq!(out.discrimination.overall(), 1.0);
    assert_eq!(out.overall.overall(), 1.0);
}

#[test]
fn dropping_a_prompt_section_fails_the_gate_and_names_the_dimension() {
    let ds = CompactionDataset::seed();
    // Remove exactly the pending-tasks heading — the section whose loss the
    // whole exercise is about.
    let weakened = full_prompt_body().replace("## 6. Pending Tasks", "## 6. Other Stuff");
    let out = compaction_gate::run(&ds, &weakened, REFERENCE_SUMMARY);

    assert!(!out.holds(), "a weakened prompt MUST fail the gate");
    assert_eq!(out.structural.per_dimension["pending_tasks"].passed, 0);

    let named: Vec<&str> = out.failures().iter().map(|(d, _)| d.as_str()).collect();
    assert!(
        named.contains(&"pending_tasks"),
        "the failing dimension must be named, got {named:?}"
    );

    // The explanation says WHAT was lost, not merely that a number moved.
    let (_, why) = out
        .failures()
        .into_iter()
        .find(|(d, _)| d == "pending_tasks")
        .expect("pending_tasks must be among the failures");
    assert!(why.contains("## 6. Pending Tasks"), "got: {why}");
}

#[test]
fn an_empty_prompt_fails_every_structural_dimension() {
    let ds = CompactionDataset::seed();
    let out = compaction_gate::run(&ds, "", REFERENCE_SUMMARY);
    assert!(!out.holds());
    assert_eq!(out.structural.passed, 0);
    assert_eq!(out.structural.total, DIMENSION_SECTIONS.len());
    // Leg B is unaffected: the reference summary is still faithful, so the
    // failure is attributable to the prompt and nothing else.
    assert_eq!(out.discrimination.overall(), 1.0);
}

#[test]
fn a_probed_dimension_with_no_contract_row_is_reported_not_skipped() {
    // A dataset probing a dimension the contract does not map. Without this the
    // gate would pass that dimension vacuously.
    let json = r#"{
        "name": "unmapped-dimension",
        "conversation": [{ "role": "user", "text": "hello" }],
        "probes": [
            { "dimension": "brand_new_dimension", "question": "q?", "expect_keywords": ["hello"] }
        ]
    }"#;
    let ds = CompactionDataset::from_json(json).unwrap();
    let out = compaction_gate::run(&ds, &full_prompt_body(), "hello");

    assert!(!out.holds(), "an unmapped dimension must not pass silently");
    let named: Vec<&str> = out.failures().iter().map(|(d, _)| d.as_str()).collect();
    assert!(named.contains(&"brand_new_dimension"), "got {named:?}");
}

#[test]
fn a_rotted_scorer_is_caught_by_the_discrimination_leg() {
    // Leg A passes (the prompt is complete) but the graded summary is empty, so
    // leg B fails. This is the guard that stops a scorer which passes nothing
    // from making leg A look healthy.
    let ds = CompactionDataset::seed();
    let out = compaction_gate::run(&ds, &full_prompt_body(), "");
    assert!(!out.holds());
    assert_eq!(out.structural.overall(), 1.0);
    assert_eq!(out.discrimination.overall(), 0.0);
}

#[test]
fn the_committed_reference_summary_is_actually_faithful() {
    // If the fixture ever loses a fact, every gate run would go red for a reason
    // unrelated to the prompt — so the fixture is asserted directly.
    let ds = CompactionDataset::seed();
    assert_eq!(ds.keyword_scorecard(REFERENCE_SUMMARY).overall(), 1.0);
}

#[test]
fn the_shipped_prompt_serves_every_dimension() {
    // THE load-bearing test. Every other case above grades a fixture body, so
    // without this one the shipped prompt could lose a section while the whole
    // suite stays green — a gate that never reads the artifact it guards.
    //
    // `compiled_default()` rather than `load()`: CI must gate the prompt the
    // binary carries, not whatever override the box happens to have.
    let prompts = crate::brain::agent::service::prompt_file::CompactionPrompts::compiled_default();
    let ds = CompactionDataset::seed();
    let out = compaction_gate::run(&ds, prompts.raw_body(), REFERENCE_SUMMARY);
    assert!(
        out.holds(),
        "the SHIPPED prompt fails the contract gate — a dimension lost its section. \
         Fix prompts/compaction.default.toml (or move the row in DIMENSION_SECTIONS if the \
         heading was legitimately renamed). Failures: {:?}",
        out.failures()
    );
    assert_eq!(
        out.structural.total,
        DIMENSION_SECTIONS.len(),
        "every contract row must be exercised against the shipped prompt"
    );
}

#[test]
fn the_committed_baseline_matches_the_reference_run() {
    // The committed baseline must be what a healthy run produces, or the gate
    // is red on a clean tree the moment anyone turns it on. Derived from the
    // SHIPPED prompt, which is what the CLI's `--update-baseline` writes, so
    // the two paths agree.
    let prompts = crate::brain::agent::service::prompt_file::CompactionPrompts::compiled_default();
    let ds = CompactionDataset::seed();
    let out = compaction_gate::run(&ds, prompts.raw_body(), REFERENCE_SUMMARY);
    let current = Baseline::from_scorecard("compaction-gate-seed", &out.overall);
    let committed = Baseline::from_json(
        &std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("src/eval/baselines/compaction.json"),
        )
        .expect("the committed baseline must exist"),
    )
    .expect("the committed baseline must parse");
    assert!(
        committed.holds(&current, 0.0),
        "a healthy run regressed against the committed baseline: {:?}",
        committed.regressions(&current, 0.0)
    );
}

#[test]
fn a_lost_dimension_is_a_regression_even_when_overall_barely_moves() {
    // The load-bearing clause: 3/4 dimensions intact is an overall drop of 25%
    // but the SPECIFIC loss is what matters, and a baseline dimension absent
    // from the current run counts as a drop to zero.
    let ds = CompactionDataset::seed();
    let weakened = full_prompt_body().replace("## 6. Pending Tasks", "## 6. Other Stuff");
    let out = compaction_gate::run(&ds, &weakened, REFERENCE_SUMMARY);
    let current = Baseline::from_scorecard("compaction-gate-seed", &out.overall);
    let baseline = Baseline::from_scorecard("compaction-gate-seed", &{
        let healthy = compaction_gate::run(&ds, &full_prompt_body(), REFERENCE_SUMMARY);
        healthy.overall
    });

    let regressions = baseline.regressions(&current, 0.0);
    let named: Vec<&str> = regressions.iter().map(|r| r.dimension.as_str()).collect();
    assert!(
        named.contains(&"pending_tasks"),
        "pending_tasks must be named as a regression, got {named:?}"
    );
}
