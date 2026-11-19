//! `opencrabs eval` — run a recall benchmark track against a labeled dataset.
//!
//! The eval framework in [`crate::eval`] already carries the metrics
//! ([`crate::eval::recall`]), the negatives convention (#996), a scorer (#622)
//! and baseline drift (#632). What it never had was a RUNNER SURFACE: the
//! framework was reachable only from unit tests, so no number could be
//! published or compared across runs (#890).
//!
//! This module is that surface. It is deliberately thin — every metric is the
//! shipped implementation, and the retrieval it scores is the shipped
//! implementation, so a change to either is measured rather than assumed.
//!
//! Judge-based metrics stay out of CI (#632), and the LIVE legs never run here:
//! this command is the fixture leg, whose corpus and labels are committed.

use anyhow::{Context, Result, bail};

use crate::brain::brain_sections::Matches;
use crate::brain::memory_recall::{RECALL_MAX_CHARS, RECALL_MAX_SECTIONS, RECALL_MIN_SCORE};
use crate::brain::section_rank::Ranked;
use crate::eval::recall::{RecallDataset, RecallReport};

use super::args::EvalCommands;

/// Which corpus a track scores against.
///
/// Track B is the fixture leg: a committed synthetic corpus plus committed
/// labels, so the number is comparable across weeks. Tracks A (episodic
/// messages) and C (skill selection) read the LIVE corpora and are therefore
/// not comparable across runs unless their targets still exist — they are
/// deliberately NOT wired into this command yet (#890 steps 4 and 5).
const SUPPORTED_TRACKS: &[&str] = &["a", "b"];

pub(crate) fn cmd_eval(cmd: EvalCommands) -> Result<()> {
    match cmd {
        EvalCommands::Recall {
            track,
            dataset,
            slice_days,
            corpus,
            k,
            json,
        } => run_recall(
            &track,
            dataset.as_deref(),
            corpus.as_deref(),
            k,
            json,
            slice_days,
        ),
        EvalCommands::Skills { dataset, k, json } => run_skills(&dataset, k, json),
    }
}

/// Track C — skill SELECTION over the live skills index (#890 step 4).
///
/// This is a LIVE leg by design: it reads the profile's real skills, so its
/// numbers are comparable only while those skills exist. That is why it is not
/// wired into CI (step 6 keeps only the fixture legs there) and why every
/// label that fails to resolve is reported rather than dropped.
fn run_skills(dataset_path: &str, k: Option<usize>, json: bool) -> Result<()> {
    use crate::eval::skills_track::{SkillDataset, index_block_chars, index_markdown, score};

    let raw = std::fs::read_to_string(dataset_path)
        .with_context(|| format!("eval skills: cannot read dataset {dataset_path}"))?;
    let dataset = SkillDataset::from_json(&raw)
        .with_context(|| format!("eval skills: {dataset_path} is not a valid skill dataset"))?;
    if dataset.tasks.is_empty() {
        bail!("eval skills: dataset {dataset_path} has no tasks");
    }

    // The index is read through the SAME loader the prompt builder uses, so
    // the thing measured is the thing the agent is handed — not a copy that
    // can drift from it.
    let skills = crate::brain::skills::load_all_skills();
    if skills.is_empty() {
        bail!("eval skills: no skills found — is a brain directory configured?");
    }
    let entries: Vec<(String, String)> = skills
        .iter()
        .map(|s| (s.name.clone(), s.description.clone()))
        .collect();
    let as_pairs: Vec<(&str, &str)> = entries
        .iter()
        .map(|(n, d)| (n.as_str(), d.as_str()))
        .collect();
    let index_md = index_markdown(as_pairs.iter().copied());
    let c2_chars = index_block_chars(as_pairs.iter().copied());
    let slugs: Vec<String> = entries.iter().map(|(n, _)| n.clone()).collect();

    // C3 — the honest denominator. `session_seen_skills` is the record of what
    // was actually loaded, so a skill with no row has never been used by any
    // session on this host: it occupies prompt space for nothing.
    let employed = employed_slugs(&slugs)?;
    let report = score(
        &dataset,
        &index_md,
        &slugs,
        k.unwrap_or(5),
        c2_chars,
        Some(employed.len()),
    );

    if json {
        let obj = serde_json::json!({
            "track": "C",
            "dataset": dataset.name,
            "ranker": "proxy: shipped BM25 over the emitted index (no skill ranker exists in the product)",
            "k": report.k,
            "full": report.full,
            "positives": report.positives,
            "negatives": report.negatives,
            "resolved": report.resolved,
            "c1_at_k": report.c1_at_k,
            "c1_at_full": report.c1_at_full,
            "c2_chars": report.c2_chars,
            "c3_employed": report.c3_employed,
            "c3_coverage": report.c3_coverage,
            "c4_false_selection": report.c4_false_selection,
            "employed": employed,
        });
        println!("{}", serde_json::to_string_pretty(&obj).unwrap_or_default());
    } else {
        println!("# eval skills — track C (selection)");
        println!("dataset: {}", dataset.name);
        println!(
            "index:   {} skills, emitted form (ranked by the shipped BM25 — a PROXY, \
             not the model's own choice)",
            report.full
        );
        println!();
        print!("{}", report.render());
    }
    Ok(())
}

/// Track A — episodic recall over the LIVE message store (#890 step 5).
///
/// The corpus is the host's own history, so this is a LIVE leg: its numbers
/// are comparable across runs only while the slice holds still, which is why
/// every figure is printed beside the slice it came from. The dataset is
/// generated (see [`crate::eval::episodic_track`]) rather than committed.
fn run_episodic(k: Option<usize>, json: bool, slice_days: u32) -> Result<()> {
    use crate::eval::episodic_track::{sample, score};
    use rusqlite::{Connection, OpenFlags};

    let config = crate::config::Config::load_for_cli(None)
        .context("eval recall: cannot load config to locate the message store")?;
    let db_path = &config.database.path;
    if !db_path.exists() {
        bail!(
            "eval recall: track A reads the message store, and {} does not exist — \
             reporting an empty store as a score would measure nothing",
            db_path.display()
        );
    }
    let conn = Connection::open_with_flags(db_path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("eval recall: cannot open {} read-only", db_path.display()))?;

    let dataset = sample(&conn, slice_days)?;
    // k defaults to the shipped search's own default page size, so the leg
    // scores the window a caller actually gets.
    let report = score(&conn, &dataset, k.unwrap_or(10))?;

    if json {
        let obj = serde_json::json!({
            "track": "A",
            "ranker": "shipped: LIKE recency order + tokenized AND fallback (no message ranker exists — #855)",
            "k": report.k,
            "slice_days": report.slice_days,
            "slice_messages": report.slice_messages,
            "corpus_messages": report.corpus_messages,
            "positives": report.positives,
            "negatives": report.negatives,
            "recall_at_k": report.recall_at_k(),
            "mrr": report.mrr,
            "found_message": report.found_message,
            "found_session": report.found_session,
            "false_positives": report.false_positives,
            "false_positive_rate": report.false_positive_rate(),
        });
        println!("{}", serde_json::to_string_pretty(&obj).unwrap_or_default());
    } else {
        println!("# eval recall — track A (episodic, live store)");
        println!("store:   {}", db_path.display());
        println!();
        print!("{}", report.render());
    }
    Ok(())
}

fn run_recall(
    track: &str,
    dataset_path: Option<&str>,
    corpus_path: Option<&str>,
    k: Option<usize>,
    json: bool,
    slice_days: u32,
) -> Result<()> {
    let track_norm = track.trim().to_ascii_lowercase();
    if !SUPPORTED_TRACKS.contains(&track_norm.as_str()) {
        bail!(
            "eval recall: track {:?} is not wired (supported: {}). \
             Track C is `eval skills`; A and B run here.",
            track,
            SUPPORTED_TRACKS.join(", ")
        );
    }

    // Track A reads the LIVE store and generates its own cases, so it takes no
    // dataset path — a committed one would pin host-local message ids.
    if track_norm == "a" {
        return run_episodic(k, json, slice_days);
    }

    let Some(dataset_path) = dataset_path else {
        bail!(
            "eval recall: track {} needs --dataset <path>",
            track_norm.to_ascii_uppercase()
        );
    };

    let raw = std::fs::read_to_string(dataset_path)
        .with_context(|| format!("eval recall: cannot read dataset {dataset_path}"))?;
    let dataset = RecallDataset::from_json(&raw)
        .with_context(|| format!("eval recall: {dataset_path} is not a valid recall dataset"))?;
    if dataset.cases.is_empty() {
        bail!("eval recall: dataset {dataset_path} has no cases");
    }

    // The corpus defaults to the dataset's sibling so the committed fixture
    // pair runs with one flag, and the pairing cannot drift. A dataset named
    // `<stem>_recall.json` prefers `<stem>_corpus.md` (#890 step 3: the Track B
    // set carries its own 292-section corpus); anything else keeps the original
    // `memory_corpus.md` sibling, so the v1 fixture pair is unchanged.
    let corpus_path = corpus_path
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            let dataset = std::path::Path::new(dataset_path);
            let dir = dataset
                .parent()
                .unwrap_or_else(|| std::path::Path::new("."));
            let stem = dataset
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or_default();
            let named = stem
                .strip_suffix("_recall")
                .map(|base| dir.join(format!("{base}_corpus.md")))
                .filter(|p| p.is_file());
            named.unwrap_or_else(|| dir.join("memory_corpus.md"))
        });
    let corpus_raw = std::fs::read_to_string(&corpus_path)
        .with_context(|| format!("eval recall: cannot read corpus {}", corpus_path.display()))?;

    // The SHIPPED retrieval, not a copy of it: if the thresholds move, the
    // scorecard moves with them instead of scoring a configuration nobody runs.
    let k = k.unwrap_or(RECALL_MAX_SECTIONS);
    let report = score_recall(&dataset, &corpus_raw, k);

    if json {
        print_json(&dataset.name, &track_norm, &report);
    } else {
        println!("# eval recall — track {}", track_norm.to_ascii_uppercase());
        println!("dataset: {}", dataset.name);
        println!("corpus:  {}", corpus_path.display());
        println!();
        print!("{}", report.render());
        println!();
        println!(
            "positives={} negatives={} false_positives={} k={}",
            report.positives(),
            report.negatives,
            report.false_positives,
            report.k
        );
    }
    Ok(())
}

/// Score a dataset against a corpus with the SHIPPED retrieval and the SHIPPED
/// metrics.
///
/// Split out from the CLI so the fixture leg's exact numbers are asserted by a
/// test rather than only printed: a ranking change that moves a scorecard has
/// to move the test with it, or the gate goes RED (#890 step 5).
pub(crate) fn score_recall(dataset: &RecallDataset, corpus: &str, k: usize) -> RecallReport {
    score_recall_with(dataset, corpus, k, RECALL_MAX_SECTIONS, RECALL_MAX_CHARS)
}

/// Score the RANKER at depth `k`: retrieve `k` sections with no character
/// budget.
///
/// This is a different question from [`score_recall`], and conflating the two
/// is what made Track B unreadable (#899). `score_recall` models what
/// PRODUCTION injects — at most `RECALL_MAX_SECTIONS` sections inside a
/// `RECALL_MAX_CHARS` budget — which is the right thing to score when the
/// subject is the shipped configuration. It is the wrong thing to score when
/// the subject is ranking quality, because the window is narrower than any `k`
/// a scorecard asks for: a k=10 scorecard run through it measures two sections
/// and prints a ten.
///
/// So the window stays available where it is meaningful, and the scorecard leg
/// retrieves the depth it names.
///
/// `#[cfg(test)]` because the scorecard is its only caller: the CLI scores the
/// production window, and a lib build with no test harness would otherwise
/// carry this as dead code (the gate runs clippy with `-D warnings`).
#[cfg(test)]
pub(crate) fn score_recall_at_depth(
    dataset: &RecallDataset,
    corpus: &str,
    k: usize,
) -> RecallReport {
    score_recall_with(dataset, corpus, k, k, usize::MAX)
}

/// Shared body: retrieve with an explicit window, then score it at `k`.
fn score_recall_with(
    dataset: &RecallDataset,
    corpus: &str,
    k: usize,
    max_sections: usize,
    max_chars: usize,
) -> RecallReport {
    let ranked = Ranked::build(corpus);
    let results: Vec<Vec<String>> = dataset
        .cases
        .iter()
        .map(|case| {
            let matches: Matches =
                ranked.find_relevant(&case.query, max_sections, max_chars, RECALL_MIN_SCORE);
            section_ids(&matches)
        })
        .collect();
    score_rankings(dataset, &results, k)
}

/// Score an EXPLICIT set of rankings against a dataset.
///
/// The retrieval and the metrics are separated so a test can hold the dataset
/// fixed and mutate only the ranking: if the scorecard did not move, the leg
/// would be blind to the one thing it exists to catch (#890 step 5).
pub(crate) fn score_rankings(
    dataset: &RecallDataset,
    results: &[Vec<String>],
    k: usize,
) -> RecallReport {
    RecallReport::compute(&dataset.cases, results, k)
}

/// A returned section's id is its heading text with the leading hashes and
/// surrounding whitespace stripped — the same identity the committed dataset
/// labels use, so the labels and the retrieval cannot disagree about naming.
fn section_ids(m: &Matches) -> Vec<String> {
    m.sections
        .iter()
        .map(|s| s.heading.trim_start_matches('#').trim().to_string())
        .collect()
}

/// Machine-readable scorecard for the regression guard and any later
/// baseline diff. Flat by design: a nested shape would invite consumers to
/// depend on structure the framework does not promise.
fn print_json(dataset: &str, track: &str, report: &RecallReport) {
    let obj = serde_json::json!({
        "track": track.to_ascii_uppercase(),
        "dataset": dataset,
        "k": report.k,
        "positives": report.positives(),
        "negatives": report.negatives,
        "false_positives": report.false_positives,
        "precision": report.aggregate.precision,
        "recall": report.aggregate.recall,
        "mrr": report.aggregate.mrr,
        "ndcg": report.aggregate.ndcg,
        "fpr": report.false_positive_rate(),
    });
    println!("{}", serde_json::to_string_pretty(&obj).unwrap_or_default());
}

/// Slugs from `index` that appear at least once in `session_seen_skills`.
///
/// This is the ONE piece of I/O Track C needs, so it lives with the CLI rather
/// than in [`crate::eval::skills_track`]: the metric module stays pure and
/// testable without a database, and the surface that has a config and a
/// connection does the reading.
///
/// An unreadable database is an ERROR, never a zero. `c3_employed = 0` would
/// read as "no skill has ever been used" — the strongest possible claim about
/// the index — and it would be produced by a missing file.
fn employed_slugs(index: &[String]) -> Result<Vec<String>> {
    use rusqlite::{Connection, OpenFlags};

    let config = crate::config::Config::load_for_cli(None)
        .context("eval skills: cannot load config to locate the database")?;
    let db_path = &config.database.path;
    if !db_path.exists() {
        bail!(
            "eval skills: database not found at {} — C3 needs the session record, \
             and reporting 0 would claim no skill was ever used",
            db_path.display()
        );
    }
    let conn = Connection::open_with_flags(db_path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("eval skills: cannot open {} read-only", db_path.display()))?;
    let mut stmt = conn
        .prepare("SELECT DISTINCT slug FROM session_seen_skills")
        .context("eval skills: session_seen_skills is not queryable")?;
    let seen: std::collections::HashSet<String> = stmt
        .query_map([], |row| row.get::<_, String>(0))
        .context("eval skills: cannot read session_seen_skills")?
        .filter_map(|r| r.ok())
        .collect();

    Ok(index
        .iter()
        .filter(|s| seen.contains(*s))
        .cloned()
        .collect())
}
