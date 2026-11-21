//! `opencrabs eval compaction` — the compaction prompt/eval gate (#929).
//!
//! Split out of `eval_cmd.rs` so the recall/skills surface stays as it was and
//! this leg can be read on its own. The scoring is
//! [`crate::eval::compaction_gate`]; this module is only the CLI envelope:
//! resolve inputs, map the outcome to an exit code, render.
//!
//! Exit codes (RC contract): `0` pass, `1` regression, `2` usage, `3` baseline
//! missing, `4` the prompt set itself cannot render. `3` is distinct on purpose —
//! a first run with no baseline yet is not a failure, and collapsing it into `1`
//! would make every fresh checkout red. `4` is distinct for the same reason: a
//! prompt that cannot render is not a scoring regression, and reporting it as one
//! would send the reader looking for a prompt EDIT when the defect is a missing
//! placeholder.

use anyhow::{Context, Result};
use std::io::Read;
use std::path::{Path, PathBuf};

use crate::eval::baseline::Baseline;
use crate::eval::compaction::CompactionDataset;
use crate::eval::compaction_gate::{self, GateOutcome, REFERENCE_SUMMARY};

/// The compiled-in baseline, so `--gate` works from any checkout without a
/// path argument. `--baseline <path>` overrides it.
const DEFAULT_BASELINE_JSON: &str = include_str!("../eval/baselines/compaction.json");

/// `--update-baseline` writes here when no `--baseline` path was named. A
/// compiled-in default cannot be rewritten, so an update without a path has to
/// name a real file; resolving it against the source tree keeps the diff in the
/// repo, which is the whole point of making the update explicit.
const DEFAULT_BASELINE_PATH: &str = "src/eval/baselines/compaction.json";

/// Exit code for "no baseline to gate against". Distinct from a regression.
const EXIT_BASELINE_MISSING: i32 = 3;

/// Exit code for "the loaded prompt set cannot render" — a missing placeholder
/// or an empty template. Distinct from a regression: nothing was scored.
const EXIT_PROMPT_INVALID: i32 = 4;

pub(crate) fn run(
    dataset_path: Option<&str>,
    summary_arg: Option<&str>,
    mode: &str,
    baseline_path: Option<&str>,
    gate: bool,
    update_baseline: bool,
    json: bool,
) -> Result<()> {
    let dataset = match dataset_path {
        Some(p) => {
            let raw = std::fs::read_to_string(p)
                .with_context(|| format!("reading compaction dataset {p}"))?;
            CompactionDataset::from_json(&raw)
                .with_context(|| format!("parsing compaction dataset {p}"))?
        }
        None => CompactionDataset::seed(),
    };

    let summary = match summary_arg {
        Some("-") => {
            let mut buf = String::new();
            std::io::stdin()
                .read_to_string(&mut buf)
                .context("reading the summary from stdin")?;
            buf
        }
        Some(p) => std::fs::read_to_string(p).with_context(|| format!("reading summary {p}"))?,
        None => REFERENCE_SUMMARY.to_string(),
    };

    match mode {
        "keyword" => {}
        "judge" => {
            anyhow::bail!(
                "--mode judge grades real model output and is not wired into this leg yet; \
                 it requires a configured provider and must never gate a merge. \
                 Use --mode keyword (the deterministic, offline gate)."
            );
        }
        other => anyhow::bail!("unknown --mode {other:?}; expected `keyword` or `judge`"),
    }

    // The prompt body is read from the LIVE loader, not from a compiled const,
    // so an operator override file is gated too. A broken override falls back to
    // the default (fail-open) and the fingerprint line says which text was read.
    // `resolve_profile_home()` — the same resolver the compaction path calls —
    // not `opencrabs_home()`, or the gate would read a different file than
    // production and reintroduce the very drift #929 removes.
    let prompts = crate::brain::agent::service::prompt_file::CompactionPrompts::load(
        &crate::config::profile::resolve_profile_home(),
    );
    preflight(&prompts);
    let outcome = compaction_gate::run(&dataset, prompts.raw_body(), &summary);

    if update_baseline {
        let path = PathBuf::from(baseline_path.unwrap_or(DEFAULT_BASELINE_PATH));
        let card = Baseline::from_scorecard("compaction-gate-seed", &outcome.overall);
        card.save(&path)
            .with_context(|| format!("writing baseline {}", path.display()))?;
        println!("baseline updated: {}", path.display());
    }

    let baseline = load_baseline(baseline_path);

    if json {
        let card = Baseline::from_scorecard("compaction-gate-seed", &outcome.overall);
        println!("{}", card.to_json()?);
    } else {
        render(
            &outcome,
            baseline.as_ref(),
            prompts.fingerprint(),
            prompts.source(),
        );
    }

    if gate {
        let Some(base) = baseline else {
            eprintln!(
                "no baseline to gate against ({}); run with --update-baseline to establish one",
                baseline_path.unwrap_or(DEFAULT_BASELINE_PATH)
            );
            std::process::exit(EXIT_BASELINE_MISSING);
        };
        let card = Baseline::from_scorecard("compaction-gate-seed", &outcome.overall);
        let regressions = base.regressions(&card, 0.0);
        if !regressions.is_empty() {
            eprintln!("GATE RED — {} regression(s):", regressions.len());
            for r in &regressions {
                eprintln!(
                    "  {}: baseline {:.2} -> {:.2} (delta {:.2})",
                    r.dimension, r.baseline, r.current, r.delta
                );
            }
            // Name the dimension's own explanation where the gate has one, so a
            // red run says WHAT was lost rather than only which number moved.
            for (dim, why) in outcome.failures() {
                eprintln!("  {dim}: {why}");
            }
            std::process::exit(1);
        }
        println!("GATE GREEN — no regression against the baseline");
    }

    Ok(())
}

/// Refuse to gate a prompt set that cannot render.
///
/// [`CompactionPrompts::load`] already falls back to the compiled default when an
/// override fails validation, so a broken OVERRIDE never reaches here. A broken
/// compiled DEFAULT would — and reporting "every dimension is served" over a
/// template that lost its `{summary_budget_tokens}` placeholder would be a green
/// verdict about the wrong artifact. The three templates the runtime substitutes
/// into are checked, because those are the ones a substitution bug would hit.
fn preflight(prompts: &crate::brain::agent::service::prompt_file::CompactionPrompts) {
    let checks: [(&str, &str, &str); 3] = [
        ("summariser.body", prompts.raw_body(), "{context_inventory}"),
        (
            "summariser.budget_directive",
            prompts.raw_budget_directive(),
            "{summary_budget_tokens}",
        ),
        (
            "summariser.system",
            prompts.raw_system(),
            // The system prompt substitutes nothing; it must simply exist.
            "",
        ),
    ];
    for (name, template, placeholder) in checks {
        if template.trim().is_empty() {
            eprintln!("PREFLIGHT FAILED — {name} is empty");
            std::process::exit(EXIT_PROMPT_INVALID);
        }
        if !placeholder.is_empty() && !template.contains(placeholder) {
            eprintln!(
                "PREFLIGHT FAILED — {name} no longer carries `{placeholder}`, so the runtime \
                 substitution has nothing to fill and the prompt would ship with a hole"
            );
            std::process::exit(EXIT_PROMPT_INVALID);
        }
    }
}

/// `--baseline <path>` when given; otherwise the compiled-in default. `None`
/// when a named path does not exist, so `--gate` can report "baseline missing"
/// distinctly from a regression.
fn load_baseline(path: Option<&str>) -> Option<Baseline> {
    match path {
        Some(p) => {
            if !Path::new(p).exists() {
                return None;
            }
            Baseline::load(Path::new(p)).ok()
        }
        None => Baseline::from_json(DEFAULT_BASELINE_JSON).ok(),
    }
}

fn render(outcome: &GateOutcome, baseline: Option<&Baseline>, fingerprint: &str, source: &str) {
    println!("compaction prompt: source={source} fingerprint={fingerprint}");
    println!();
    println!("leg A — structural (does the live prompt serve every probed dimension?)");
    for (q, v) in &outcome.structural.results {
        println!(
            "  [{}] {}",
            if v.yes { "PASS" } else { "FAIL" },
            q.dimension
        );
        if let Some(why) = &v.explanation {
            println!("         {why}");
        }
    }
    println!();
    println!("leg B — discrimination (does a faithful summary still score full marks?)");
    for (q, v) in &outcome.discrimination.results {
        println!(
            "  [{}] {}",
            if v.yes { "PASS" } else { "FAIL" },
            q.dimension
        );
        if let Some(why) = &v.explanation {
            println!("         {why}");
        }
    }
    println!();
    println!(
        "overall: {}/{} ({:.0}%)",
        outcome.overall.passed,
        outcome.overall.total,
        outcome.overall.overall() * 100.0
    );
    if let Some(base) = baseline {
        println!("baseline: {} (overall {:.2})", base.label, base.overall);
    } else {
        println!("baseline: none (run with --update-baseline to establish one)");
    }
}
