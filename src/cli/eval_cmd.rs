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
use crate::brain::memory_recall::{
    RECALL_MAX_CHARS, RECALL_MAX_SECTIONS, clamp_recall_min_score, resolve_recall_min_score,
};
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
            floor,
        } => run_recall(
            &track,
            dataset.as_deref(),
            corpus.as_deref(),
            k,
            json,
            slice_days,
            floor,
        ),
        EvalCommands::Skills {
            dataset,
            k,
            json,
            floor,
        } => run_skills(&dataset, k, json, floor),
        EvalCommands::Retrieval {
            dataset,
            corpus,
            modes,
            k,
            live,
            json,
        } => run_retrieval(dataset.as_deref(), corpus.as_deref(), &modes, k, live, json),
        EvalCommands::Compaction {
            dataset,
            summary,
            mode,
            baseline,
            gate,
            update_baseline,
            json,
        } => super::eval_compaction::run(
            dataset.as_deref(),
            summary.as_deref(),
            &mode,
            baseline.as_deref(),
            gate,
            update_baseline,
            json,
        ),
    }
}

/// Track C — skill SELECTION over the live skills index (#890 step 4).
///
/// This is a LIVE leg by design: it reads the profile's real skills, so its
/// numbers are comparable only while those skills exist. That is why it is not
/// wired into CI (step 6 keeps only the fixture legs there) and why every
/// label that fails to resolve is reported rather than dropped.
fn run_skills(dataset_path: &str, k: Option<usize>, json: bool, floor: Option<f64>) -> Result<()> {
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
    // C4 asks "does any skill clear the floor the product accepts a match at",
    // so it reads the SAME resolved floor the recall path does (#912).
    let floor = floor
        .map(|v| clamp_recall_min_score(Some(v)))
        .unwrap_or_else(resolve_recall_min_score);
    let report = score(
        &dataset,
        &index_md,
        &slugs,
        k.unwrap_or(5),
        c2_chars,
        Some(employed.len()),
        floor,
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
            "floor": floor,
            "employed": employed,
        });
        println!("{}", serde_json::to_string_pretty(&obj).unwrap_or_default());
    } else {
        println!("# eval skills — track C (selection)");
        println!("dataset: {}", dataset.name);
        println!("floor:   {floor}");
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
            "ranker": crate::relevance::episodic::RANKER_LABEL,
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
    floor: Option<f64>,
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
    //
    // The FLOOR is the shipped one unless `--floor` names a value (#912): the
    // point of publishing a number is that it describes the configuration the
    // box actually runs. An explicit value is clamped to the band the product
    // can run, so every point on a swept curve is a point that could ship.
    let floor = floor
        .map(|v| clamp_recall_min_score(Some(v)))
        .unwrap_or_else(resolve_recall_min_score);
    let k = k.unwrap_or(RECALL_MAX_SECTIONS);
    let report = score_recall(&dataset, &corpus_raw, k, floor);

    if json {
        print_json(&dataset.name, &track_norm, &report, floor);
    } else {
        println!("# eval recall — track {}", track_norm.to_ascii_uppercase());
        println!("dataset: {}", dataset.name);
        println!("corpus:  {}", corpus_path.display());
        println!("floor:   {floor}");
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
pub(crate) fn score_recall(
    dataset: &RecallDataset,
    corpus: &str,
    k: usize,
    floor: f64,
) -> RecallReport {
    score_recall_with(
        dataset,
        corpus,
        k,
        RECALL_MAX_SECTIONS,
        RECALL_MAX_CHARS,
        floor,
    )
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
    floor: f64,
) -> RecallReport {
    score_recall_with(dataset, corpus, k, k, usize::MAX, floor)
}

/// Shared body: retrieve with an explicit window, then score it at `k`.
fn score_recall_with(
    dataset: &RecallDataset,
    corpus: &str,
    k: usize,
    max_sections: usize,
    max_chars: usize,
    floor: f64,
) -> RecallReport {
    let ranked = Ranked::build(corpus);
    let results: Vec<Vec<String>> = dataset
        .cases
        .iter()
        .map(|case| {
            let matches: Matches =
                ranked.find_relevant(&case.query, max_sections, max_chars, floor);
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
fn print_json(dataset: &str, track: &str, report: &RecallReport, floor: f64) {
    let obj = serde_json::json!({
        "track": track.to_ascii_uppercase(),
        "dataset": dataset,
        "k": report.k,
        "floor": floor,
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

// ── Retrieval ablation — BM25 / vector / hybrid (#981) ──────────────────────
//
// #981 asks a question the codebase could not answer: does the vector half of
// hybrid search earn its keep? The framework could score a ranking but had no
// way to produce three of them over one corpus with one set of labels, so
// "hybrid is better" was a belief rather than a measurement. This driver
// produces exactly those three rankings, through the SHIPPED implementations,
// and hands them to `eval::retrieval_track` to score.
//
// The fixture pair is compiled in (`include_str!`) rather than read from the
// working directory: the fixture leg is the CI-gated one, and a gate that
// measures whatever directory the test happened to run in is not a gate.

/// The committed fixture pair, embedded so the leg cannot measure a different
/// corpus than the one the labels were written against.
const RETRIEVAL_FIXTURE_DATASET: &str = include_str!("../eval/fixtures/retrieval_v1_recall.json");
const RETRIEVAL_FIXTURE_CORPUS: &str = include_str!("../eval/fixtures/retrieval_v1_corpus.md");

/// Cutoff k when `--k` is not given.
const RETRIEVAL_DEFAULT_K: usize = 10;

/// The RRF constant, matching what `memory::search::search_core` passes.
///
/// Named rather than inlined so the two cannot drift apart silently: a fusion
/// scored with a different constant is a different algorithm, and the whole
/// point of this leg is to score the one that ships.
const RRF_K: usize = 60;

/// Everything one ranking pass needs, so the three modes cannot disagree about
/// which corpus, cutoff or collection they were scored against.
struct RetrievalCtx<'a> {
    /// Where the documents live, for `resolve_path`. On the fixture leg this is
    /// the temp dir; on the live leg it is the profile's memory dir.
    home: &'a std::path::Path,
    /// The database the vector half reads.
    db_path: &'a std::path::Path,
    collection: &'a str,
    k: usize,
    /// A handle to the store, when one is open. Used for the shipped tuple
    /// builder; `None` on the live leg, where tuples are built from the paths
    /// alone. The fixture store is leaked to `'static` because the API
    /// embedder twin (`embed_content_api`) takes a `&'static` handle (#981).
    store: Option<&'a std::sync::Mutex<crate::memory::Store>>,
}

fn run_retrieval(
    dataset_path: Option<&str>,
    corpus_path: Option<&str>,
    modes_raw: &str,
    k: Option<usize>,
    live: bool,
    json: bool,
) -> Result<()> {
    use crate::eval::retrieval_track::{
        Arm, BOOTSTRAP_RESAMPLES, BOOTSTRAP_SEED, CorpusFingerprint, Determinism, Mode,
        RetrievalDataset, Scorecard, class_split, compare, parse_modes, score_mode,
    };

    let modes = parse_modes(modes_raw).map_err(anyhow::Error::msg)?;
    let k = k.unwrap_or(RETRIEVAL_DEFAULT_K);
    if k == 0 {
        bail!("eval retrieval: --k must be at least 1");
    }

    // The API embedder is async while this command is synchronous, so one
    // current-thread runtime carries the embed calls. Building it
    // unconditionally costs microseconds and keeps the signatures simple.
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("eval retrieval: cannot build the embedding runtime")?;

    // The dataset. `--dataset` may name a live-label set; without it the
    // committed fixture is used, which is the pair the CI gate scores.
    let (dataset_raw, dataset_label) = match dataset_path {
        Some(p) => (
            std::fs::read_to_string(p)
                .with_context(|| format!("eval retrieval: cannot read dataset {p}"))?,
            p.to_string(),
        ),
        None => (
            RETRIEVAL_FIXTURE_DATASET.to_string(),
            "<fixture>".to_string(),
        ),
    };
    let corpus_raw = match corpus_path {
        Some(p) => std::fs::read_to_string(p)
            .with_context(|| format!("eval retrieval: cannot read corpus {p}"))?,
        // With an explicit dataset, prefer its sibling corpus: a labelled set
        // and its corpus are written together, and scoring one against the
        // other's fixture would silently measure a corpus the labels never
        // described. Only when no sibling exists does the embedded fixture
        // stand in — which is the default, path-less case.
        None => match dataset_path.and_then(sibling_retrieval_corpus) {
            Some(sibling) => std::fs::read_to_string(&sibling).with_context(|| {
                format!("eval retrieval: cannot read corpus {}", sibling.display())
            })?,
            None => RETRIEVAL_FIXTURE_CORPUS.to_string(),
        },
    };

    let dataset = RetrievalDataset::from_json(&dataset_raw)
        .with_context(|| format!("eval retrieval: {dataset_label} is not a valid dataset"))?;
    if dataset.cases.is_empty() {
        bail!("eval retrieval: dataset {dataset_label} has no cases");
    }

    let mut notes: Vec<String> = Vec::new();
    let mut unavailable: Vec<(Mode, String)> = Vec::new();
    let mut rankings: std::collections::HashMap<Mode, Vec<Vec<String>>> =
        std::collections::HashMap::new();
    let mut determinism = Determinism::NotChecked;
    let mut fingerprint: Option<CorpusFingerprint> = None;

    // The class split is derived from the corpus with the shipped tokenizer, so
    // it is only meaningful where the corpus and the labels are the same pair.
    // On the live leg the labels name live documents, so there is nothing to
    // split and the scorecard says so instead of inventing a split.
    let split = if live {
        None
    } else {
        Some(class_split(&dataset, &corpus_raw).map_err(anyhow::Error::msg)?)
    };

    if live {
        let home = crate::memory::store::memory_dir();
        let db_path = home.join("memory.db");
        if !db_path.is_file() {
            bail!(
                "eval retrieval: no live memory store at {} — nothing to measure",
                db_path.display()
            );
        }

        // The live leg never reindexes and never embeds: it reads the corpus
        // the profile already has, so the numbers describe what the agent
        // actually retrieves today. Both helpers open READ-ONLY connections,
        // so an accidental write is an error rather than a silent mutation.
        match crate::memory::db::Store::fingerprint_readonly(&db_path) {
            Ok((documents, vectors, model)) => {
                fingerprint = Some(CorpusFingerprint {
                    documents,
                    vectors,
                    model,
                });
            }
            Err(e) => notes.push(format!("corpus fingerprint unavailable: {e}")),
        }

        let ctx = RetrievalCtx {
            home: &home,
            db_path: &db_path,
            collection: crate::memory::COLLECTION_MEMORY,
            k,
            store: None,
        };

        for mode in &modes {
            match rank_mode_readonly(*mode, &dataset, &ctx, &mut determinism, &rt) {
                Ok(rows) => {
                    rankings.insert(*mode, rows);
                }
                Err(e) => unavailable.push((*mode, e)),
            }
        }
        notes.push(
            "live leg: labels must name live document paths; unresolved gold ids are counted as \
             misses, not dropped"
                .to_string(),
        );
    } else {
        // The lexical leg needs no store: `Ranked` is the shipped brain-file
        // ranker and the corpus is already in memory.
        if modes.contains(&Mode::Bm25) {
            rankings.insert(Mode::Bm25, bm25_rankings(&dataset, &corpus_raw, k));
            notes.push(
                "bm25 leg ranks with no score floor: the recall floor (#912) is a separate \
                 product decision, and applying it to one arm only would attribute its effect to \
                 BM25"
                    .to_string(),
            );
        }

        let needs_vectors = modes
            .iter()
            .any(|m| matches!(m, Mode::Vector | Mode::Hybrid));
        if needs_vectors {
            match build_fixture_store(&corpus_raw, &rt) {
                Ok((tmp, store)) => {
                    let db_path = tmp.path().join("memory.db");
                    let ctx = RetrievalCtx {
                        home: tmp.path(),
                        db_path: &db_path,
                        collection: crate::memory::COLLECTION_MEMORY,
                        k,
                        store: Some(store),
                    };
                    for mode in &modes {
                        if *mode == Mode::Bm25 {
                            continue;
                        }
                        match rank_mode_fixture(*mode, &dataset, &ctx, &mut determinism, &rt) {
                            Ok(rows) => {
                                rankings.insert(*mode, rows);
                            }
                            Err(e) => unavailable.push((*mode, e)),
                        }
                    }
                }
                Err(e) => {
                    for mode in &modes {
                        if *mode != Mode::Bm25 {
                            unavailable.push((*mode, e.clone()));
                        }
                    }
                }
            }
        }

        // A fixture leg whose embedder is not bit-stable is printed but never
        // gated: a score that moves between identical runs cannot be a
        // regression signal (#981).
        if determinism == Determinism::NotBitStable {
            notes.push(
                "embedder is not bit-stable: the fixture leg is published but not CI-gated"
                    .to_string(),
            );
        }
    }

    // "not checked (no embedder)" would be a lie when no embedding mode was
    // asked for: the check was never due, rather than unavailable.
    let ran_vectors = modes
        .iter()
        .any(|m| matches!(m, Mode::Vector | Mode::Hybrid));
    if determinism == Determinism::NotChecked && !ran_vectors {
        notes.push("no embedding mode ran, so embedder determinism was not checked".to_string());
    }

    // Score everything that ran. Order follows `--modes` so two runs of the
    // same flags print the same table.
    let mut scored = Vec::new();
    for mode in &modes {
        if let Some(rows) = rankings.get(mode) {
            scored.push(score_mode(*mode, &dataset.cases, rows, k));
        }
    }
    if scored.is_empty() {
        bail!(
            "eval retrieval: no mode could be measured ({} unavailable)",
            unavailable.len()
        );
    }

    // The head-to-head. BM25 is the baseline because it is the mode that ships
    // without an embedder: the question #981 asks is whether the extra
    // machinery beats it, so the delta is always "X minus bm25" and a reader
    // never has to work out which direction a number points.
    let comparison = rankings.get(&Mode::Bm25).and_then(|baseline| {
        modes
            .iter()
            .find(|m| **m != Mode::Bm25)
            .and_then(|other| rankings.get(other).map(|rows| (other, rows)))
            .map(|(other, rows)| {
                compare(
                    Arm {
                        mode: Mode::Bm25,
                        rankings: baseline,
                    },
                    Arm {
                        mode: *other,
                        rankings: rows,
                    },
                    &dataset.cases,
                    k,
                    BOOTSTRAP_SEED,
                    BOOTSTRAP_RESAMPLES,
                )
            })
    });
    if comparison.is_none() && modes.len() > 1 {
        notes.push(
            "no paired delta: bm25 is the baseline and must be among the modes that ran"
                .to_string(),
        );
    }

    let card = Scorecard {
        dataset: dataset_label,
        k,
        split,
        modes: scored,
        comparison,
        determinism,
        fingerprint,
        unavailable,
        notes,
    };

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&card.to_json()).unwrap_or_default()
        );
    } else {
        println!("# eval retrieval — BM25 / vector / hybrid ablation (#981)");
        print!("{}", card.render());
    }
    Ok(())
}

/// The corpus that sits beside a dataset file, if it is there.
///
/// `<stem>_recall.json` prefers `<stem>_corpus.md`, which is the naming the
/// committed pair uses; anything else falls back to `retrieval_v1_corpus.md`
/// beside it.
fn sibling_retrieval_corpus(dataset: &str) -> Option<std::path::PathBuf> {
    let dataset = std::path::Path::new(dataset);
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
    named.or_else(|| {
        let fallback = dir.join("retrieval_v1_corpus.md");
        fallback.is_file().then_some(fallback)
    })
}

/// BM25 rankings from the shipped brain-file ranker.
///
/// One `Ranked` for the whole corpus: the statistics (document frequencies,
/// average length) are corpus-wide, so rebuilding it per query would score
/// every case against a slightly different corpus.
fn bm25_rankings(
    dataset: &crate::eval::retrieval_track::RetrievalDataset,
    corpus: &str,
    k: usize,
) -> Vec<Vec<String>> {
    let ranked = Ranked::build(corpus);
    dataset
        .cases
        .iter()
        .map(|case| {
            ranked
                // `f64::MIN_POSITIVE`, not 0.0: a section that shares no term
                // with the query scores exactly 0.0, and a floor of 0.0 would
                // admit every section in the corpus as a "candidate". The
                // smallest positive score admits exactly the sections with at
                // least one matching term, which is what "BM25 returned this"
                // should mean.
                //
                // The shipped recall floor (#912) is deliberately NOT applied
                // here: it is a separate product decision, and giving it to one
                // arm only would score the floor's effect as BM25's.
                .ranked_indices(&case.query, k, usize::MAX, f64::MIN_POSITIVE)
                .0
                .into_iter()
                // `heading` already strips the `#` markers and trims.
                .map(|i| ranked.heading(i))
                .collect()
        })
        .collect()
}

/// Index the fixture corpus into a throwaway store and embed it, so the vector
/// and hybrid legs score a real database rather than a mock.
///
/// The documents are keyed by their section id, which is also the id the labels
/// use, so the three modes return names that are directly comparable.
fn build_fixture_store(
    corpus: &str,
    rt: &tokio::runtime::Runtime,
) -> Result<
    (
        tempfile::TempDir,
        &'static std::sync::Mutex<crate::memory::Store>,
    ),
    String,
> {
    use crate::memory::embedding::{embed_content, embed_content_api};
    use crate::memory::index::index_file_sync_keyed;

    // Both embedder paths the product supports. An earlier version checked only
    // the local GGUF engine, which declared the vector arms "unavailable" on
    // every box configured with `[memory.embedding]` — including this one,
    // where the product's own vector search works fine. A harness that cannot
    // see the embedder the product uses measures nothing.
    let api = crate::memory::embedding_api_configured();
    if !api && crate::memory::embedding::engine_if_ready().is_none() {
        return Err(
            "no embedder: neither `[memory.embedding]` nor the local model is available"
                .to_string(),
        );
    }

    let tmp = tempfile::TempDir::new().map_err(|e| format!("cannot create temp dir: {e}"))?;
    let db_path = tmp.path().join("memory.db");
    let store = crate::memory::Store::open(&db_path)
        .map_err(|e| format!("cannot open fixture store: {e}"))?;
    store
        .ensure_vector_table(crate::memory::embedding_dimensions())
        .map_err(|e| format!("cannot create vector table: {e}"))?;

    let sections = crate::eval::retrieval_track::section_text_index(corpus);
    // Sorted so the fixture store is byte-identical between runs; an unsorted
    // HashMap walk would change row order and, with it, any tie-break.
    let mut ids: Vec<&String> = sections.keys().collect();
    ids.sort();

    // Leaked to `'static`: `embed_content_api` takes that handle, and this is a
    // short-lived CLI process, so the store living until exit is the point.
    let store: &'static std::sync::Mutex<crate::memory::Store> =
        Box::leak(Box::new(std::sync::Mutex::new(store)));
    for id in ids {
        let body = sections
            .get(id)
            .ok_or_else(|| format!("fixture section {id} vanished"))?;
        {
            let guard = store
                .lock()
                .map_err(|e| format!("fixture store lock poisoned: {e}"))?;
            index_file_sync_keyed(&guard, crate::memory::COLLECTION_MEMORY, id, body)?;
        }
        if api {
            rt.block_on(embed_content_api(store, body))?;
        } else {
            embed_content(store, body)?;
        }
    }

    Ok((tmp, store))
}

/// Rank one case in one mode against the fixture store.
fn rank_mode_fixture(
    mode: crate::eval::retrieval_track::Mode,
    dataset: &crate::eval::retrieval_track::RetrievalDataset,
    ctx: &RetrievalCtx<'_>,
    determinism: &mut crate::eval::retrieval_track::Determinism,
    rt: &tokio::runtime::Runtime,
) -> Result<Vec<Vec<String>>, String> {
    let store = ctx
        .store
        .ok_or_else(|| "fixture ranking needs an open store".to_string())?;
    rank_mode(mode, dataset, ctx, Some(store), determinism, rt)
}

/// Rank one case in one mode against the live store, read-only.
fn rank_mode_readonly(
    mode: crate::eval::retrieval_track::Mode,
    dataset: &crate::eval::retrieval_track::RetrievalDataset,
    ctx: &RetrievalCtx<'_>,
    determinism: &mut crate::eval::retrieval_track::Determinism,
    rt: &tokio::runtime::Runtime,
) -> Result<Vec<Vec<String>>, String> {
    rank_mode(mode, dataset, ctx, None, determinism, rt)
}

/// The one ranking path both legs share.
///
/// `store` is `Some` whenever a handle is open. The hybrid leg prefers the
/// shipped `hybrid_candidate_lists` builder when it has one, and falls back to
/// resolving paths directly when it does not — `hybrid_search_rrf` keys on the
/// file path alone, so the two produce the same ranking, and the fixture leg's
/// identity check is what proves the fallback agrees with the shipped builder.
fn rank_mode(
    mode: crate::eval::retrieval_track::Mode,
    dataset: &crate::eval::retrieval_track::RetrievalDataset,
    ctx: &RetrievalCtx<'_>,
    store: Option<&std::sync::Mutex<crate::memory::Store>>,
    determinism: &mut crate::eval::retrieval_track::Determinism,
    rt: &tokio::runtime::Runtime,
) -> Result<Vec<Vec<String>>, String> {
    use crate::eval::retrieval_track::{Determinism, Mode};
    use crate::memory::search::{
        hybrid_candidate_lists, hybrid_search_rrf, resolve_path, sanitize_fts_query,
    };
    use crate::memory::vector_search::search_chunks;

    // The LEXICAL arm. The fixture leg ranks the markdown corpus with
    // `section_rank::Ranked` (see `bm25_rankings`); the live leg has no
    // markdown corpus, only the indexed store, so it ranks with the FTS5 index
    // — whose own scorer is BM25 — and that is the lexical half the shipped
    // hybrid actually fuses.
    //
    // The two are different implementations, so absolute numbers must never be
    // compared ACROSS legs. Each leg's delta is computed inside itself, which
    // is what the scorecard does.
    if mode == Mode::Bm25 {
        if store.is_some() {
            return Err("bm25 on the fixture leg goes through bm25_rankings".to_string());
        }
        let mut rows = Vec::with_capacity(dataset.cases.len());
        for case in &dataset.cases {
            let fts_query = sanitize_fts_query(&case.query);
            let hits = crate::memory::Store::search_fts_readonly(
                ctx.db_path,
                &fts_query,
                ctx.k,
                Some(ctx.collection),
            )?;
            rows.push(
                hits.iter()
                    .map(|r| resolve_path(ctx.home, &r.doc.collection_name, &r.doc.path))
                    .collect::<Vec<String>>(),
            );
        }
        return Ok(rows);
    }

    // Query embedding, honouring whichever embedder the product is configured
    // with. The API twin is async, so the caller's runtime carries it.
    let api = crate::memory::embedding_api_configured();
    let embed_query = |q: &str| -> Result<Vec<f32>, String> {
        if api {
            rt.block_on(crate::memory::embedding::embed_query_api(q))
        } else {
            let engine = crate::memory::embedding::engine_if_ready()
                .ok_or_else(|| "no embedder available".to_string())?;
            let mut guard = engine
                .lock()
                .map_err(|e| format!("embedding engine lock poisoned: {e}"))?;
            guard
                .embed_query(q)
                .map(|r| r.embedding)
                .map_err(|e| format!("embed_query failed: {e}"))
        }
    };

    let mut rows = Vec::with_capacity(dataset.cases.len());
    for case in &dataset.cases {
        let fts_query = sanitize_fts_query(&case.query);
        let query_embedding = embed_query(&case.query)?;

        // Bit-stability is checked once, on the first query, because a
        // non-deterministic embedder is a property of the engine rather than of
        // a particular case. Embedding every query twice would double the run
        // for no extra information.
        if *determinism == Determinism::NotChecked {
            let again = embed_query(&case.query)?;
            *determinism = if query_embedding == again {
                Determinism::BitStable
            } else {
                Determinism::NotBitStable
            };
        }

        let vec_hits = search_chunks(ctx.db_path, &query_embedding, ctx.k, Some(ctx.collection))?;

        let row = match mode {
            Mode::Vector => dedup_paths(
                vec_hits
                    .iter()
                    .map(|h| resolve_path(ctx.home, &h.collection, &h.path)),
            ),
            Mode::Hybrid => {
                // The lexical list comes from the store when one is open (the
                // shipped path) and from the read-only reader otherwise.
                let guard = match store {
                    Some(s) => Some(
                        s.lock()
                            .map_err(|e| format!("fixture store lock poisoned: {e}"))?,
                    ),
                    None => None,
                };
                let fts_results = match guard.as_deref() {
                    Some(s) => s.search_fts(&fts_query, ctx.k, Some(ctx.collection))?,
                    None => crate::memory::Store::search_fts_readonly(
                        ctx.db_path,
                        &fts_query,
                        ctx.k,
                        Some(ctx.collection),
                    )?,
                };
                let (fts_tuples, vec_tuples) = match guard.as_deref() {
                    Some(s) => {
                        hybrid_candidate_lists(s, ctx.home, &fts_results, &fts_query, &vec_hits)
                    }
                    None => (
                        fts_results
                            .iter()
                            .map(|r| {
                                let file =
                                    resolve_path(ctx.home, &r.doc.collection_name, &r.doc.path);
                                (file.clone(), file, String::new(), String::new())
                            })
                            .collect::<Vec<(String, String, String, String)>>(),
                        vec_hits
                            .iter()
                            .map(|h| {
                                let file = resolve_path(ctx.home, &h.collection, &h.path);
                                (file.clone(), file, String::new(), String::new())
                            })
                            .collect::<Vec<(String, String, String, String)>>(),
                    ),
                };
                // 60 is the RRF constant `search_core` passes. It is NOT
                // `ctx.k`: the cutoff and the fusion constant are different
                // numbers, and reusing one for both would measure a fusion the
                // product never runs.
                hybrid_search_rrf(fts_tuples, vec_tuples, RRF_K)
                    .into_iter()
                    .map(|r| r.file)
                    .collect()
            }
            Mode::Bm25 => unreachable!("guarded above"),
        };
        rows.push(row.into_iter().take(ctx.k).collect::<Vec<String>>());
    }

    // The fixture leg's identity check: the two tuple builders must agree on
    // the paths, or the fallback above is ranking something else than the
    // shipped builder. Only meaningful where a store is open.
    if mode == Mode::Hybrid
        && let Some(s) = store
        && let Some(first) = dataset.cases.first()
    {
        let fts_query = sanitize_fts_query(&first.query);
        let guard = s
            .lock()
            .map_err(|e| format!("fixture store lock poisoned: {e}"))?;
        let fts_results = guard.search_fts(&fts_query, ctx.k, Some(ctx.collection))?;
        let query_embedding = embed_query(&first.query)?;
        let vec_hits = search_chunks(ctx.db_path, &query_embedding, ctx.k, Some(ctx.collection))?;
        let (shipped_fts, shipped_vec) =
            hybrid_candidate_lists(&guard, ctx.home, &fts_results, &fts_query, &vec_hits);
        let mine_fts: Vec<String> = shipped_fts.into_iter().map(|t| t.0).collect();
        let resolved_fts: Vec<String> = fts_results
            .iter()
            .map(|r| resolve_path(ctx.home, &r.doc.collection_name, &r.doc.path))
            .collect();
        if mine_fts != resolved_fts {
            return Err(format!(
                "parity guard: the shipped tuple builder and resolve_path disagree on the \
                 lexical list ({:?} vs {:?})",
                mine_fts, resolved_fts
            ));
        }
        let shipped_vec: Vec<String> = shipped_vec.into_iter().map(|t| t.0).collect();
        let resolved_vec: Vec<String> = vec_hits
            .iter()
            .map(|h| resolve_path(ctx.home, &h.collection, &h.path))
            .collect();
        if shipped_vec != resolved_vec {
            return Err(format!(
                "parity guard: the shipped tuple builder and resolve_path disagree on the \
                 vector list ({:?} vs {:?})",
                shipped_vec, resolved_vec
            ));
        }
    }

    Ok(rows)
}

/// Drop repeated paths, keeping first-seen order.
///
/// `search_chunks` returns one hit per CHUNK, so a long document can occupy
/// several slots. Recall@k counts documents, so a repeated path would make one
/// document look like several and inflate the mode that chunks most finely.
fn dedup_paths<I: Iterator<Item = String>>(paths: I) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for p in paths {
        if seen.insert(p.clone()) {
            out.push(p);
        }
    }
    out
}
