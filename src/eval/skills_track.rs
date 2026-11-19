//! Track C — skill SELECTION, not skill presence (#890 step 4).
//!
//! The skills index the prompt carries is a flat, alphabetically ordered list
//! of every skill the profile has. On the ops profile that is 35 entries: the
//! agent sees all of them, every turn, and picks one by reading. Measured as
//! "was the right skill present?" that scores **1.000 vacuously** — the answer
//! cannot be anything else, because nothing is ever left out.
//!
//! So this track measures the two things the flat index actually costs:
//!
//! - **C1** — *selection*: given a task, is the right skill near the top? The
//!   window `k` is the lever: at `k = 35` every skill is inside it (the
//!   vacuous 1.000 recorded as the pre-change baseline); at `k = 5` only five
//!   entries would fit, which is what a budgeted index would have to get right.
//! - **C4** — *false selection*: on a task that matches NO skill, does the
//!   index still offer a confident winner? A flat list always offers 35
//!   candidates; the question is whether the ranking separates them.
//!
//! C2 (index characters) and C3 (employment from `session_seen_skills`) are
//! read by the CLI, which owns the I/O: this module stays pure so the numbers
//! are testable without a database or a live profile.
//!
//! ## The ranker is a PROXY, and the report says so
//!
//! There is no skill ranker in the product — the model picks by reading. So C1
//! is computed with the SHIPPED BM25 implementation ([`Ranked`]) over the index
//! text rendered exactly as the prompt renders it. That makes the number
//! deterministic and comparable week to week, and it is labelled `proxy` in
//! every scorecard rather than passed off as the model's own choice.

use crate::brain::section_rank::Ranked;

/// One labelled task: what a user asks, and the skill that answers it.
///
/// `gold = None` is a NEGATIVE — a task no skill should claim. They exist so
/// C4 has something to be wrong about; a dataset of only positives cannot
/// distinguish "selects well" from "selects everything".
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct SkillTask {
    pub task: String,
    #[serde(default)]
    pub gold: Option<String>,
}

/// A committed dataset of tasks over a skill index.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct SkillDataset {
    pub name: String,
    #[serde(default)]
    pub note: String,
    pub tasks: Vec<SkillTask>,
}

impl SkillDataset {
    pub fn from_json(raw: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(raw)
    }

    pub fn positives(&self) -> usize {
        self.tasks.iter().filter(|t| t.gold.is_some()).count()
    }

    pub fn negatives(&self) -> usize {
        self.tasks.iter().filter(|t| t.gold.is_none()).count()
    }
}

/// Render a skill index as the markdown corpus [`Ranked`] consumes.
///
/// The ORDER is the caller's: pass the entries in the order the prompt emits
/// them (alphabetical today), so a rank below `k` means "inside the first `k`
/// lines the agent reads", which is the only sense in which a flat index has a
/// position at all.
pub fn index_markdown<'a, I>(entries: I) -> String
where
    I: IntoIterator<Item = (&'a str, &'a str)>,
{
    let mut out = String::new();
    for (slug, description) in entries {
        out.push_str("## ");
        out.push_str(slug);
        out.push('\n');
        out.push_str(description.trim());
        out.push_str("\n\n");
    }
    out
}

/// Characters of the emitted index BLOCK — the budget C2 measures.
///
/// Mirrors the prompt builder's own formatting rule (`- {slash}: {desc clipped
/// to 120}` plus the section header), because the number is meaningless unless
/// it is the bytes the agent actually pays for. Kept here rather than read off
/// the builder so the metric can be computed for a hypothetical index that has
/// not been assembled yet.
pub fn index_block_chars<'a, I>(entries: I) -> usize
where
    I: IntoIterator<Item = (&'a str, &'a str)>,
{
    let mut n = "--- Available Commands & Skills ---\n".chars().count();
    let mut any = false;
    for (slug, description) in entries {
        any = true;
        let desc = description.trim();
        let clipped: String = if desc.chars().count() <= 120 {
            desc.to_string()
        } else {
            format!("{}…", desc.chars().take(120).collect::<String>())
        };
        n += format!("- {slug}: {clipped}\n").chars().count();
    }
    if any {
        n += 1; // the trailing blank line the block ends with
    }
    n
}

/// The scorecard for one Track C run.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct SkillReport {
    /// The budgeted window C1 is measured at (the target metric).
    pub k: usize,
    /// The full-index window, i.e. every entry — the vacuous baseline.
    pub full: usize,
    pub positives: usize,
    pub negatives: usize,
    /// Positives whose gold slug exists in this index at all. A label that
    /// resolves nowhere is a MISS, never a silent exclusion: a dataset can
    /// outlive the skill it names, and hiding that would inflate the score.
    pub resolved: usize,
    /// C1@k — share of positives whose gold lands inside the top `k`.
    pub c1_at_k: f64,
    /// C1@full — the vacuous number, present so a reader can see that it is
    /// 1.000 and therefore not a measurement.
    pub c1_at_full: f64,
    /// C2 — characters the whole index costs in the prompt.
    pub c2_chars: usize,
    /// C3 — distinct slugs with at least one `session_seen_skills` row, and
    /// the share of the index that represents. The gap between this and
    /// `full` is the honest measure of how many of those lines ever earn
    /// their place.
    pub c3_employed: usize,
    pub c3_coverage: f64,
    /// C4 — share of NEGATIVES on which the proxy still returns a top hit.
    /// 1.0 means the ranking cannot say "none of these".
    pub c4_false_selection: f64,
}

impl SkillReport {
    pub fn render(&self) -> String {
        let mut s = String::new();
        s.push_str(&format!(
            "C1@{k}   {:.3}   (gold inside the first {k} of {full})\n",
            self.c1_at_k,
            k = self.k,
            full = self.full
        ));
        s.push_str(&format!(
            "C1@{full}  {:.3}   (VACUOUS baseline — every entry is inside it)\n",
            self.c1_at_full,
            full = self.full
        ));
        s.push_str(&format!(
            "C2      {} chars of prompt\n",
            self.c2_chars
        ));
        s.push_str(&format!(
            "C3      {}/{} skills ever employed ({:.3})\n",
            self.c3_employed, self.full, self.c3_coverage
        ));
        s.push_str(&format!(
            "C4      {:.3} false selection on {} negatives\n",
            self.c4_false_selection, self.negatives
        ));
        s.push_str(&format!(
            "labels  {}/{} resolved against this index\n",
            self.resolved, self.positives
        ));
        s
    }
}

/// Score a dataset against a rendered index.
///
/// `index_md` must be built with [`index_markdown`] from the SAME entries, in
/// the same order, so a section's position is its position in the prompt.
pub fn score(
    dataset: &SkillDataset,
    index_md: &str,
    slugs: &[String],
    k: usize,
    c2_chars: usize,
    c3_employed: usize,
) -> SkillReport {
    let ranked = Ranked::build(index_md);
    let full = slugs.len();
    let k = k.clamp(1, full.max(1));

    let mut report = SkillReport {
        k,
        full,
        positives: dataset.positives(),
        negatives: dataset.negatives(),
        c2_chars,
        c3_employed,
        c3_coverage: if full == 0 {
            0.0
        } else {
            c3_employed as f64 / full as f64
        },
        ..Default::default()
    };

    let mut hits = 0usize;
    let mut resolved = 0usize;
    let mut false_selections = 0usize;

    for case in &dataset.tasks {
        // `full` sections and an unbounded char budget: this asks the shipped
        // ranker to order the WHOLE index, so a miss is a real miss rather
        // than a truncation artefact.
        let top = ranked.find_relevant(&case.task, full.max(1), usize::MAX, 0.0);
        let ordered: Vec<String> = top
            .sections
            .iter()
            .map(|s| s.heading.trim_start_matches('#').trim().to_string())
            .collect();

        match &case.gold {
            Some(gold) => {
                let present = slugs.iter().any(|s| s == gold);
                if !present {
                    continue; // unresolved label: counted as a miss below
                }
                resolved += 1;
                if ordered.iter().take(k).any(|s| s == gold) {
                    hits += 1;
                }
            }
            None => {
                // A negative is only meaningfully "not selected" if the ranking
                // can say so. Asking for the whole index with a zero floor
                // always returns everything (every score clears 0.0), which
                // would pin C4 at 1.000 forever — so the negative is judged
                // against the SHIPPED floor instead: does any skill clear the
                // threshold the product itself uses to accept a match?
                let cleared = ranked.find_relevant(
                    &case.task,
                    full.max(1),
                    usize::MAX,
                    crate::brain::memory_recall::RECALL_MIN_SCORE,
                );
                if !cleared.sections.is_empty() {
                    false_selections += 1;
                }
            }
        }
    }

    report.resolved = resolved;
    // The denominator is EVERY positive, not just the resolved ones: a label
    // naming a skill this index does not carry is a miss, and excluding it
    // would let the score rise as the index shrank.
    report.c1_at_k = if report.positives == 0 {
        0.0
    } else {
        hits as f64 / report.positives as f64
    };
    report.c1_at_full = if report.positives == 0 {
        0.0
    } else {
        resolved as f64 / report.positives as f64
    };
    report.c4_false_selection = if report.negatives == 0 {
        0.0
    } else {
        false_selections as f64 / report.negatives as f64
    };

    report
}

/// The COMMITTED Track C fixture leg (#890 step 6).
///
/// One builder, two consumers — the scorecard test and the baseline guard —
/// because two builders would drift and a baseline recorded against one would
/// then be checked against the other. The index is built from the dataset's own
/// 35 gold slugs with the slug as its description: that is the only index this
/// module can construct without a live profile, and it is the right one for the
/// full-window question, which depends on the LABELS rather than on prose.
#[cfg(test)]
pub fn committed_report_at(k: usize) -> SkillReport {
    use std::collections::BTreeSet;

    let dataset = SkillDataset::from_json(include_str!("fixtures/track_c_skills.json"))
        .expect("the committed Track C dataset parses");
    let slugs: Vec<String> = dataset
        .tasks
        .iter()
        .filter_map(|t| t.gold.clone())
        .collect::<BTreeSet<String>>()
        .into_iter()
        .collect();
    let entries: Vec<(String, String)> = slugs.iter().map(|s| (s.clone(), s.clone())).collect();
    let md = index_markdown(entries.iter().map(|(s, d)| (s.as_str(), d.as_str())));
    let chars = index_block_chars(entries.iter().map(|(s, d)| (s.as_str(), d.as_str())));
    let names: Vec<String> = entries.iter().map(|(s, _)| s.clone()).collect();
    // C3 is a LIVE figure (it needs `session_seen_skills`), so this scoring
    // passes 0 and the card asserts the C3 SURFACE rather than its value.
    score(&dataset, &md, &names, k, chars, 0)
}
