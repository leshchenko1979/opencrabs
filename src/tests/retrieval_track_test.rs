//! Tests for the retrieval ablation's metric layer (#981).
//!
//! Every gate here is hermetic: the metric module takes rankings, labels and a
//! corpus as arguments, so nothing below needs an embedder, a store or a
//! network. The embedder-dependent legs live in the CLI driver and are reported
//! rather than asserted, because a number that moves when the local model
//! changes cannot be a regression signal.
//!
//! The tests are named for the PROPERTY they protect, not for the function they
//! call: a reader who sees one fail should learn what broke without opening the
//! implementation.

use crate::eval::recall::QueryCase;
use crate::eval::retrieval_track::{
    BOOTSTRAP_RESAMPLES, BOOTSTRAP_SEED, CaseClass, Determinism, Mode, RetrievalDataset,
    class_split, derive_class, mcnemar_exact, paired_delta, parse_modes, score_mode, unique_hits,
};

/// The committed fixture pair, embedded exactly as the CLI driver embeds it, so
/// these tests score the artefact the command scores.
const FIXTURE_DATASET: &str = include_str!("../eval/fixtures/retrieval_v1_recall.json");
const FIXTURE_CORPUS: &str = include_str!("../eval/fixtures/retrieval_v1_corpus.md");

fn case(query: &str, gold: &[&str]) -> QueryCase {
    QueryCase {
        query: query.to_string(),
        relevant: gold.iter().map(|g| g.to_string()).collect(),
    }
}

fn ranking(ids: &[&str]) -> Vec<String> {
    ids.iter().map(|i| i.to_string()).collect()
}

// ── the corpus / label contract ────────────────────────────────────────────

/// The class split is derived from the shipped tokenizer, so a case that was
/// WRITTEN as a paraphrase but shares a term with its gold is scored as
/// lexical. Otherwise a mislabel would flatter the vector side, which is the
/// one error this experiment cannot afford.
#[test]
fn the_class_split_follows_the_shipped_tokenizer_not_the_label() {
    let corpus = "\
## doc-a - Kerning a display typeface
Kerning pairs tighten the spacing between two letters.

## doc-b - Feeding a sourdough starter
Flour, water and a refresh schedule.
";
    // Shares "typeface" with its gold: lexical, whatever it was intended as.
    assert_eq!(
        derive_class("typeface spacing question", "Kerning a display typeface"),
        CaseClass::Lexical
    );
    // Shares nothing: a genuine paraphrase. The gold text here is the section
    // as `class_split` sees it — heading AND body — so a query that only
    // avoids the heading still counts as lexical when the body carries it.
    assert_eq!(
        derive_class(
            "how close must glyphs sit",
            "## doc-a - Kerning a display typeface\n\
             Kerning pairs tighten the spacing between two letters."
        ),
        CaseClass::Paraphrase
    );

    let dataset = RetrievalDataset {
        name: "split".to_string(),
        note: String::new(),
        cases: vec![
            case(
                "typeface spacing question",
                &["doc-a - Kerning a display typeface"],
            ),
            case(
                "how close must glyphs sit",
                &["doc-a - Kerning a display typeface"],
            ),
            case("nothing at all should match this", &[]),
        ],
    };
    let split = class_split(&dataset, corpus).expect("labels resolve");
    assert_eq!(split.total, 3);
    assert_eq!(split.lexical, 1);
    assert_eq!(split.paraphrase, 1);
    assert_eq!(split.negatives, 1);
    assert_eq!(split.positives(), 2);
}

/// An unresolved gold id is a broken dataset, not a zero. Scoring it as a miss
/// would turn a typo into a plausible-looking retrieval score.
#[test]
fn a_gold_id_that_does_not_resolve_is_an_error() {
    let dataset = RetrievalDataset {
        name: "broken".to_string(),
        note: String::new(),
        cases: vec![case("anything", &["doc-missing - Not in the corpus"])],
    };
    let err = class_split(&dataset, "## doc-a - Present\nBody.\n").expect_err("must fail");
    assert!(
        err.contains("doc-missing"),
        "the error must name the id that failed, got: {err}"
    );
}

/// The committed fixture is internally consistent: every label resolves, the
/// document and case counts are the ones the generator emitted, and the
/// negatives are present so the false-positive rate has a denominator.
#[test]
fn the_committed_fixture_pair_is_consistent() {
    let dataset = RetrievalDataset::from_json(FIXTURE_DATASET).expect("fixture parses");
    assert_eq!(dataset.cases.len(), 72, "fixture case count");
    assert_eq!(dataset.negatives(), 12, "fixture negative count");

    let split = class_split(&dataset, FIXTURE_CORPUS).expect("every fixture label resolves");
    assert_eq!(split.total, 72);
    assert_eq!(split.negatives, 12);
    assert_eq!(split.positives(), 60);
    // Both classes must be populated, or the ablation has nothing to contrast.
    assert!(split.lexical > 0, "no lexical cases: {}", split.lexical);
    assert!(
        split.paraphrase > 0,
        "no paraphrase cases: {}",
        split.paraphrase
    );

    // The corpus carries one `#` title above the 60 `##` document sections, so
    // the index is 61 entries. The count that matters is the documents, and
    // counting them by prefix keeps this assertion about the fixture rather
    // than about whether a preamble happens to be present.
    let sections = crate::eval::retrieval_track::section_text_index(FIXTURE_CORPUS);
    let documents = sections.keys().filter(|id| id.starts_with("doc-")).count();
    assert_eq!(documents, 60, "fixture corpus document count");
}

/// The generator's contract, checked on the artefact rather than trusted: the
/// paraphrase set is exactly the cases that share no term with their gold. This
/// is the assertion that caught two bad cases while the fixture was written.
#[test]
fn the_paraphrase_set_is_exactly_the_cases_sharing_no_term_with_gold() {
    let dataset = RetrievalDataset::from_json(FIXTURE_DATASET).expect("fixture parses");
    let index = crate::eval::retrieval_track::section_text_index(FIXTURE_CORPUS);

    // Recount independently of `class_split`, so a bug in the split cannot
    // satisfy both counts.
    let mut paraphrase = 0usize;
    for c in dataset.cases.iter().filter(|c| !c.is_negative()) {
        let shares_a_term = c.relevant.iter().any(|id| {
            index
                .get(id)
                .map(|text| derive_class(&c.query, text) == CaseClass::Lexical)
                .unwrap_or(false)
        });
        if !shares_a_term {
            paraphrase += 1;
        }
    }

    let split = class_split(&dataset, FIXTURE_CORPUS).expect("labels resolve");
    assert_eq!(
        paraphrase, split.paraphrase,
        "the split and an independent recount disagree about the paraphrase set"
    );
    assert_eq!(
        split.lexical + split.paraphrase,
        split.positives(),
        "every positive is exactly one class"
    );
}

// ── scoring ────────────────────────────────────────────────────────────────

/// A negative case asserts silence. A mode that returns something is a false
/// positive, and that is counted rather than ignored — it is the only place the
/// ablation can see that vector search has no score floor.
#[test]
fn a_negative_case_returning_anything_is_a_false_positive() {
    let cases = vec![case("nothing here should match", &[])];
    let loud = score_mode(Mode::Vector, &cases, &[ranking(&["doc-a - Something"])], 10);
    assert_eq!(loud.report.false_positives, 1);
    assert_eq!(loud.report.negatives, 1);
    assert_eq!(loud.report.positives(), 0);

    let silent = score_mode(Mode::Vector, &cases, &[Vec::new()], 10);
    assert_eq!(silent.report.false_positives, 0);
}

/// The unique-hit table is what the pre-registered rule reads, so negatives must
/// not enter it: they have no gold, and counting them would let a mode "win" by
/// returning junk on the queries that ask for nothing.
#[test]
fn unique_hits_excludes_negatives_from_its_denominator() {
    let cases = vec![
        case("lexical one", &["g1"]),
        case("only the other mode finds this", &["g2"]),
        case("a negative", &[]),
    ];
    let baseline = vec![ranking(&["g1"]), ranking(&["x"]), ranking(&["junk"])];
    let other = vec![ranking(&["x"]), ranking(&["g2"]), ranking(&["junk"])];

    let hits = unique_hits(&cases, &baseline, &other, 10);
    assert_eq!(hits.positives, 2, "negatives must not be scored");
    assert_eq!(hits.baseline_only, 1);
    assert_eq!(hits.other_only, 1);
    assert_eq!(hits.both, 0);
    assert_eq!(hits.neither, 0);
    assert!((hits.baseline_rate() - 0.5).abs() < 1e-12);
    assert!((hits.other_rate() - 0.5).abs() < 1e-12);
}

/// Recall is cut at `k`, so a gold document below the cutoff is a miss. Without
/// this the cutoff would be decorative and every mode would look perfect.
#[test]
fn a_gold_document_below_the_cutoff_is_a_miss() {
    let cases = vec![case("find the third one", &["g3"])];
    let deep = vec![ranking(&["g1", "g2", "g3"])];
    assert_eq!(unique_hits(&cases, &deep, &deep, 3).both, 1);
    assert_eq!(
        unique_hits(&cases, &deep, &deep, 2).neither,
        1,
        "k=2 must not see g3 at rank 3"
    );
}

// ── the paired delta ───────────────────────────────────────────────────────

/// The bootstrap interval is seeded, so two runs of the same flags produce the
/// same interval. An unseeded interval would make a published number
/// unreproducible, and the whole point of the scorecard is that a later reader
/// can re-derive it.
#[test]
fn the_bootstrap_interval_is_reproducible_from_its_seed() {
    let cases = vec![
        case("a", &["g1"]),
        case("b", &["g2"]),
        case("c", &["g3"]),
        case("d", &["g4"]),
    ];
    let baseline = vec![
        ranking(&["g1"]),
        ranking(&["g2"]),
        ranking(&["x"]),
        ranking(&["y"]),
    ];
    let other = vec![
        ranking(&["g1"]),
        ranking(&["z"]),
        ranking(&["g3"]),
        ranking(&["g4"]),
    ];

    let first = paired_delta(
        &cases,
        &baseline,
        &other,
        10,
        BOOTSTRAP_SEED,
        BOOTSTRAP_RESAMPLES,
    );
    let second = paired_delta(
        &cases,
        &baseline,
        &other,
        10,
        BOOTSTRAP_SEED,
        BOOTSTRAP_RESAMPLES,
    );
    assert_eq!(first, second, "same seed must give the same interval");
    assert_eq!(first.seed, BOOTSTRAP_SEED);
    assert_eq!(first.resamples, BOOTSTRAP_RESAMPLES);

    assert_eq!(first.baseline_only, 1);
    assert_eq!(first.other_only, 2);
    // (2 - 1) / 4 cases.
    assert!((first.delta - 0.25).abs() < 1e-12, "delta {}", first.delta);
    // A percentile interval is not required to contain the point estimate, so
    // the assertion is what IS required: ordered bounds inside the attainable
    // range of a per-case recall difference ([-1, 1]).
    assert!(
        first.ci_low <= first.ci_high,
        "bounds out of order: {} > {}",
        first.ci_low,
        first.ci_high
    );
    assert!(
        first.ci_low >= -1.0 - 1e-12 && first.ci_high <= 1.0 + 1e-12,
        "interval escapes the attainable range: [{}, {}]",
        first.ci_low,
        first.ci_high
    );
}

/// McNemar's exact test on the discordant pairs. Hand-checkable values, because
/// the p-value is what decides whether a delta is noise.
#[test]
fn mcnemar_is_exact_on_hand_checkable_counts() {
    assert!((mcnemar_exact(0, 0) - 1.0).abs() < 1e-12);
    // 5 discordant pairs, all one way: 2 * (1/2^5).
    assert!((mcnemar_exact(5, 0) - 0.0625).abs() < 1e-12);
    // 6 discordant pairs, all one way: 2 * (1/2^6).
    assert!((mcnemar_exact(0, 6) - 0.031_25).abs() < 1e-12);
    // A balanced split is as null as it gets, and the two-sided p is clamped.
    assert!((mcnemar_exact(1, 1) - 1.0).abs() < 1e-12);
    // The test is symmetric in its arguments.
    assert!((mcnemar_exact(3, 1) - mcnemar_exact(1, 3)).abs() < 1e-12);
}

// ── mode parsing ───────────────────────────────────────────────────────────

/// A typo in `--modes` must be an error, not a silently dropped mode: a
/// one-mode scorecard that looks like a three-mode one is the failure mode this
/// guards.
#[test]
fn an_unknown_mode_is_rejected_rather_than_dropped() {
    assert!(parse_modes("bm25,nope").is_err());
    let parsed = parse_modes("bm25, vec ,hybrid").expect("known modes parse");
    assert_eq!(parsed, vec![Mode::Bm25, Mode::Vector, Mode::Hybrid]);
    // The short spellings an operator actually types.
    assert_eq!(Mode::parse("fts"), Some(Mode::Bm25));
    assert_eq!(Mode::parse("rrf"), Some(Mode::Hybrid));
}

/// A mode that could not be measured must not silently vanish from the
/// scorecard, or a run with no embedder would look like a clean BM25 win.
#[test]
fn the_determinism_verdict_names_what_was_actually_checked() {
    assert!(Determinism::BitStable.label().contains("bit-stable"));
    assert!(Determinism::NotChecked.label().contains("no embedder"));
    assert!(Determinism::NotBitStable.label().contains("not-bit-stable"));
}

// ── the fusion the driver scores ───────────────────────────────────────────

/// The driver falls back to building fusion tuples from paths alone when it has
/// no store handle. That fallback is only sound because the fusion keys on the
/// FILE PATH: title, display path and body must not move the ranking.
///
/// This is the property that lets the live leg reuse one ranking path, so it is
/// asserted rather than assumed.
#[test]
fn the_fusion_ranks_on_the_file_path_alone() {
    let with_bodies = vec![
        (
            "memory/doc-a".to_string(),
            "memory/doc-a".to_string(),
            "Doc A".to_string(),
            "a long body about kerning".to_string(),
        ),
        (
            "memory/doc-b".to_string(),
            "memory/doc-b".to_string(),
            "Doc B".to_string(),
            "an unrelated body".to_string(),
        ),
    ];
    let without_bodies: Vec<(String, String, String, String)> = with_bodies
        .iter()
        .map(|(f, _, _, _)| (f.clone(), f.clone(), String::new(), String::new()))
        .collect();

    let fused_a = crate::memory::hybrid_search_rrf(with_bodies, Vec::new(), 60);
    let fused_b = crate::memory::hybrid_search_rrf(without_bodies, Vec::new(), 60);
    let order_a: Vec<String> = fused_a.into_iter().map(|r| r.file).collect();
    let order_b: Vec<String> = fused_b.into_iter().map(|r| r.file).collect();
    assert_eq!(
        order_a, order_b,
        "an empty body must not change the fused ranking"
    );
}
