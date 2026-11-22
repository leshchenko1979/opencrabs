//! Surface relevant memory without the model having to ask (#799).
//!
//! MEMORY.md was written constantly and read almost never. #800 made reading
//! cheap, but a cheap read still has to be chosen, and the case that hurts most
//! is the one where the model does not know there is anything to look up. It
//! cannot decide to recall a correction it has forgotten exists.
//!
//! So recall runs on the ENTRY path instead: the user's message is matched
//! against MEMORY.md, active skills, and project directives at turn start and
//! anything relevant rides along with it (#285).
//!
//! Deliberately conservative. This is paid on every single turn and competes
//! with the actual task for attention, so it stays silent unless the match is
//! good, and it never grows past a couple of short sections per source.

use std::collections::HashSet;
use std::path::Path;

use crate::brain::brain_sections::{self, Section, split_sections};
use crate::brain::section_rank::Ranked;

/// At most this many sections ride along with a user message per source.
pub(crate) const RECALL_MAX_SECTIONS: usize = 2;
/// Total character budget for injected recall per source.
pub(crate) const RECALL_MAX_CHARS: usize = 1200;
/// Minimum score for a section to ride along: length-normalized BM25 with the
/// IDF scale removed, so the number means the same thing in any workspace.
///
/// Raised from 0.35 when the heading boost landed, and the two numbers were
/// chosen together rather than one at a time. The boost makes the correct
/// section score HIGHER, which on its own would let more conversational
/// messages through; moving the floor to the lowest value that restores the
/// false-positive rate to a better-than-before tier is what keeps the pair a
/// net win on both halves.
///
/// Re-measured over BOTH corpora for #912 (`opencrabs eval recall --floor <v>`,
/// live artifact 2026-11-20). "fired" counts positives that returned a
/// non-empty ranking; "correct" counts those whose labelled section made it
/// through the production window:
///
/// | floor | Track B fired (of 60) | Track B correct | Track B FPR (15 neg) | memory-pair FPR (12 neg) |
/// |---|---|---|---|---|
/// | 0.25 | 47 | 10 | 0.067 | 0.500 |
/// | 0.30 | 26 | 9 | 0.000 | 0.250 |
/// | 0.36 | 17 | 7 | 0.000 | 0.250 |
/// | **0.38 (shipped)** | **14** | **5** | **0.000** | **0.167** |
/// | 0.45 | 6 | 2 | 0.000 | 0.083 |
///
/// On the large corpus the floor is a VOLUME knob, not a quality one: precision
/// among fired sections is flat across 0.30–0.45 (0.346 / 0.412 / 0.357 /
/// 0.333), so lowering it buys more answers at the same hit rate. 0.36 was
/// declined despite firing 3 more positives: it lands the memory pair exactly
/// on the `<= 0.25` contract, and #911 is about to change the ranker, so the
/// gate would break by arithmetic rather than by defect. 0.38 keeps the
/// headroom that move bought.
///
/// Track B recall is capped at 11/60 even at floor 0.05, where everything
/// fires: what limits it is the RANKING, which is #911's subject, not this
/// constant's.
pub(crate) const RECALL_MIN_SCORE: f64 = 0.38;

/// Lower bound the configurable floor is clamped to (#912).
///
/// Below this recall stops filtering at all, and a `0.0` typo would put an
/// unfiltered block on every turn — degradation with no error to notice. The
/// knob therefore cannot reach it.
pub(crate) const RECALL_MIN_SCORE_MIN: f64 = 0.05;

/// Upper bound the configurable floor is clamped to (#912).
///
/// 0.60 is already stricter than the memory pair's plateau, so anything larger
/// buys no precision — only silence.
pub(crate) const RECALL_MIN_SCORE_MAX: f64 = 0.60;

/// Clamp a configured floor into the usable band (#912).
///
/// `None` and NaN fall back to the compiled default; a value outside the band
/// is clamped rather than refused, because a config typo must never be able to
/// empty every ranking (too high) or disable the filter (too low) silently.
///
/// Pure, so the boundaries are testable without a config file.
pub(crate) fn clamp_recall_min_score(configured: Option<f64>) -> f64 {
    match configured {
        None => RECALL_MIN_SCORE,
        Some(v) if v.is_nan() => RECALL_MIN_SCORE,
        Some(v) if v < RECALL_MIN_SCORE_MIN => RECALL_MIN_SCORE_MIN,
        Some(v) if v > RECALL_MIN_SCORE_MAX => RECALL_MIN_SCORE_MAX,
        Some(v) => v,
    }
}

/// The floor this process actually runs with (#912).
///
/// `[memory].recall_min_score` clamped, or the compiled default when unset.
/// **THE one reader**: the per-turn recall path and the eval harness both call
/// it, because a scorecard that read the constant while the box ran a
/// configured value would publish numbers about a configuration nobody uses.
pub(crate) fn resolve_recall_min_score() -> f64 {
    clamp_recall_min_score(crate::config::Config::current().memory.recall_min_score)
}

/// Recall relevant to `user_message`, formatted for context, or `None`.
///
/// Pure: takes the file content, so the decision is testable without a home
/// directory or a populated MEMORY.md.
pub fn recall_from(memory: &str, user_message: &str) -> Option<String> {
    if !worth_reading_for(user_message) {
        return None;
    }
    render_memory(&Ranked::build(memory).find_relevant(
        user_message,
        RECALL_MAX_SECTIONS,
        RECALL_MAX_CHARS,
        RECALL_MIN_SCORE,
    ))
}

/// Whether `user_message` can possibly produce a recall, decided without
/// touching the disk (#995).
fn worth_reading_for(user_message: &str) -> bool {
    if user_message.starts_with("[System:") {
        return false;
    }
    !brain_sections::query_terms(user_message).is_empty()
}

/// The parsed file behind the cache, with what it was parsed from.
struct Cached {
    stamp: (std::time::SystemTime, u64),
    indexed: Ranked,
}

static MEMORY_CACHE: std::sync::RwLock<Option<Cached>> = std::sync::RwLock::new(None);

/// Touch the epistemic beliefs for recalled sections (#1641).
///
/// When a section is injected into context via recall, its corresponding
/// belief gets a hit. Uses the same key format as `backfill_from_content`:
/// `"MEMORY.md##<heading>"`.
fn touch_recalled(matches: &brain_sections::Matches) {
    for section in &matches.sections {
        let heading = section.heading.trim().trim_start_matches('#').trim();
        if !heading.is_empty() {
            let key = format!("MEMORY.md##{}", heading);
            crate::brain::tools::epistemic::touch_belief(&key);
        }
    }
}

/// Recall relevant to `user_message` from MEMORY.md on disk.
///
/// `floor` is the resolved match threshold ([`resolve_recall_min_score`] at the
/// runtime entry point, an explicit value when the eval harness sweeps it).
pub async fn recall_for(user_message: &str, floor: f64) -> Option<String> {
    memory_matches(user_message, floor)
        .await
        .and_then(|m| render_memory(&m))
}

/// The MEMORY.md matches for `user_message`, or `None` when nothing matched.
///
/// The ONE computation behind both the injected text and the recall summary
/// (#928): `recall_for` renders it, `recall_for_session` also reads the
/// headings and the omitted count off the very same [`Matches`].
///
/// [`Matches`]: brain_sections::Matches
async fn memory_matches(user_message: &str, floor: f64) -> Option<brain_sections::Matches> {
    if !worth_reading_for(user_message) {
        return None;
    }

    let path = crate::config::opencrabs_home().join("MEMORY.md");
    let meta = tokio::fs::metadata(&path).await.ok()?;
    let stamp = (meta.modified().ok()?, meta.len());

    // Fast path: the file has not changed since it was parsed.
    {
        let cache = MEMORY_CACHE.read().ok()?;
        if let Some(c) = cache.as_ref()
            && c.stamp == stamp
        {
            let matches =
                c.indexed
                    .find_relevant(user_message, RECALL_MAX_SECTIONS, RECALL_MAX_CHARS, floor);
            touch_recalled(&matches);
            return Some(matches).filter(|m| !m.sections.is_empty());
        }
    }

    let memory = tokio::fs::read_to_string(&path).await.ok()?;
    let indexed = Ranked::build(&memory);
    let matches = indexed.find_relevant(user_message, RECALL_MAX_SECTIONS, RECALL_MAX_CHARS, floor);

    if let Ok(mut cache) = MEMORY_CACHE.write() {
        tracing::debug!(
            "MEMORY.md re-parsed for recall: {} sections, {} bytes",
            indexed.len(),
            memory.len()
        );
        *cache = Some(Cached { stamp, indexed });
    }

    touch_recalled(&matches);
    Some(matches).filter(|m| !m.sections.is_empty())
}

/// Format matches from MEMORY.md.
fn render_memory(matches: &brain_sections::Matches) -> Option<String> {
    if matches.sections.is_empty() {
        return None;
    }
    let body = matches
        .sections
        .iter()
        .map(brain_sections::Section::render)
        .collect::<Vec<_>>()
        .join("\n\n");
    Some(format!(
        "─── from your MEMORY.md, possibly relevant ───\n{body}\n\
         [Recalled automatically. Load MEMORY.md with a query for more.]"
    ))
}

/// One section source for multi-source recall.
struct NamedSection {
    source_name: String,
    section: Section,
}

/// One source's contribution to a turn's recall: the text that rides along,
/// plus what it was made of, for the turn-start flow line (#928).
///
/// The names and the omitted count are collected in the SAME loop that renders
/// the blocks, so the summary cannot describe a ranking other than the one
/// whose text was injected.
struct SourceRecall {
    text: String,
    /// Display names in injection order, deduplicated.
    names: Vec<String>,
    /// Sections this source matched but did not inject.
    omitted: usize,
}

/// Recall relevant sections from active skills (#285).
/// Matches against active skills' auxiliary markdown files and body sections.
pub fn recall_from_active_skills(
    active_skills: &HashSet<String>,
    user_message: &str,
    floor: f64,
) -> Option<String> {
    active_skill_recall(active_skills, user_message, floor).map(|r| r.text)
}

/// [`recall_from_active_skills`] plus the names and the dropped-match count.
fn active_skill_recall(
    active_skills: &HashSet<String>,
    user_message: &str,
    floor: f64,
) -> Option<SourceRecall> {
    if !worth_reading_for(user_message) || active_skills.is_empty() {
        return None;
    }

    let all_skills = crate::brain::skills::load_all_skills();
    let mut named_sections: Vec<NamedSection> = Vec::new();

    for skill in all_skills {
        if !active_skills.contains(&skill.name) {
            continue;
        }

        // 1. Auxiliary files in the skill directory (e.g. editor.md, fleet-directives.md)
        for aux in &skill.auxiliary_files {
            for sec in split_sections(&aux.body) {
                named_sections.push(NamedSection {
                    source_name: format!("skill {}/{}", skill.name, aux.name),
                    section: sec,
                });
            }
        }

        // 2. Main skill prompt body.
        //
        // DELIBERATE EXCLUSION (#520, HQ ruling 2026-09-23): this path injects
        // SECTIONS (`split_sections`), never a whole body, so it does not route
        // through `prompt_body()` and the #406 size warning is absent here BY
        // DESIGN, not by oversight. A whole-skill size warning attached to one
        // section would read as "this section is too long" when it is the SKILL
        // that is too long — a wrong signal. The warning's home is
        // `prompt_body()`, which every whole-body injection path shares.
        for sec in split_sections(&skill.body) {
            named_sections.push(NamedSection {
                source_name: format!("skill {}", skill.name),
                section: sec,
            });
        }
    }

    if named_sections.is_empty() {
        return None;
    }

    let raw_sections = named_sections
        .iter()
        .map(|ns| ns.section.clone())
        .collect::<Vec<_>>();
    let indexed = Ranked::from_sections(raw_sections);
    let (matched_indices, total) =
        indexed.ranked_indices(user_message, RECALL_MAX_SECTIONS, RECALL_MAX_CHARS, floor);

    if matched_indices.is_empty() {
        return None;
    }

    let mut rendered_blocks = Vec::new();
    let mut names: Vec<String> = Vec::new();
    for i in matched_indices {
        let ns = &named_sections[i];
        rendered_blocks.push(format!(
            "─── from active {}, possibly relevant ───\n{}",
            ns.source_name,
            ns.section.render()
        ));
        let name = ns
            .source_name
            .strip_prefix("skill ")
            .unwrap_or(&ns.source_name)
            .to_string();
        if !names.contains(&name) {
            names.push(name);
        }
    }

    let omitted = total.saturating_sub(rendered_blocks.len());
    Some(SourceRecall {
        text: rendered_blocks.join("\n\n"),
        names,
        omitted,
    })
}

/// Recall relevant sections from project directive files (#285).
/// Discovers directives in `working_dir` (e.g. `CLAUDE.md`, `AGENTS.md`, `.cursor/rules/*.mdc`)
/// and matches relevant sections unprompted.
pub async fn recall_from_project_directives(
    working_dir: &Path,
    user_message: &str,
    floor: f64,
) -> Option<String> {
    project_directive_recall(working_dir, user_message, floor)
        .await
        .map(|r| r.text)
}

/// [`recall_from_project_directives`] plus the names and the dropped-match count.
async fn project_directive_recall(
    working_dir: &Path,
    user_message: &str,
    floor: f64,
) -> Option<SourceRecall> {
    if !worth_reading_for(user_message) {
        return None;
    }

    let root = if let Some(s) = working_dir.to_str() {
        crate::brain::tools::error::expand_tilde(s)
    } else {
        working_dir.to_path_buf()
    };
    if !root.is_dir() {
        return None;
    }

    let directives = crate::brain::directives::discover(&root);
    if directives.is_empty() {
        return None;
    }

    let mut named_sections: Vec<NamedSection> = Vec::new();

    for d in directives {
        let file_path = root.join(&d.rel_path);
        let Ok(content) = tokio::fs::read_to_string(&file_path).await else {
            continue;
        };
        let body = crate::brain::skills::split_frontmatter(&content)
            .map(|(_, b)| b)
            .unwrap_or(&content);
        for sec in split_sections(body.trim()) {
            named_sections.push(NamedSection {
                source_name: format!("project directive {}", d.rel_path),
                section: sec,
            });
        }
    }

    if named_sections.is_empty() {
        return None;
    }

    let raw_sections = named_sections
        .iter()
        .map(|ns| ns.section.clone())
        .collect::<Vec<_>>();
    let indexed = Ranked::from_sections(raw_sections);
    let (matched_indices, total) =
        indexed.ranked_indices(user_message, RECALL_MAX_SECTIONS, RECALL_MAX_CHARS, floor);

    if matched_indices.is_empty() {
        return None;
    }

    let mut rendered_blocks = Vec::new();
    let mut names: Vec<String> = Vec::new();
    for i in matched_indices {
        let ns = &named_sections[i];
        rendered_blocks.push(format!(
            "─── from {}, possibly relevant ───\n{}",
            ns.source_name,
            ns.section.render()
        ));
        let name = ns
            .source_name
            .strip_prefix("project directive ")
            .unwrap_or(&ns.source_name)
            .to_string();
        if !names.contains(&name) {
            names.push(name);
        }
    }

    let omitted = total.saturating_sub(rendered_blocks.len());
    Some(SourceRecall {
        text: rendered_blocks.join("\n\n"),
        names,
        omitted,
    })
}

/// One turn's passive recall: the text injected into the prompt, plus what it
/// was made of (#928).
///
/// Both halves come out of ONE ranking per source. The flow line reads
/// [`summary`](Self::summary); nothing recomputes a match to describe the text
/// it already has, because a second pass could name a section that was never
/// injected.
#[derive(Debug, Clone)]
pub struct Recalled {
    /// The recall text appended to the user message.
    pub text: String,
    /// Headings, skill names and the injected size, for the turn-start line.
    pub summary: RecallSummary,
}

/// Unified passive recall for a session (#285):
/// Aggregates recall from MEMORY.md, active skills, and project directives.
pub async fn recall_for_session(
    session_id: uuid::Uuid,
    working_dir: Option<&Path>,
    user_message: &str,
) -> Option<Recalled> {
    if !worth_reading_for(user_message) {
        return None;
    }

    // Resolved ONCE per turn (#912): the configured floor, clamped, or the
    // compiled default. Every source below reads this same value, so the box
    // cannot run two thresholds at once.
    let floor = resolve_recall_min_score();

    let mut parts: Vec<String> = Vec::new();
    let mut summary = RecallSummary::default();

    // 1. MEMORY.md
    if let Some(matches) = memory_matches(user_message, floor).await
        && let Some(text) = render_memory(&matches)
    {
        summary.memory = RecallSource {
            names: headings(&matches.sections),
            omitted: matches.omitted,
        };
        parts.push(text);
    }

    // 2. Active skills
    let active_skills = crate::brain::tools::seen_skills::active_for_session(session_id);
    if let Some(recall) = active_skill_recall(&active_skills, user_message, floor) {
        summary.active_skills = RecallSource {
            names: recall.names,
            omitted: recall.omitted,
        };
        parts.push(recall.text);
    }

    // 3. Project directives
    if let Some(wd) = working_dir
        && let Some(recall) = project_directive_recall(wd, user_message, floor).await
    {
        summary.directives = RecallSource {
            names: recall.names,
            omitted: recall.omitted,
        };
        parts.push(recall.text);
    }

    if parts.is_empty() {
        return None;
    }

    let text = parts.join("\n\n");
    summary.chars = text.chars().count();
    Some(Recalled { text, summary })
}

// ── Recall summary (#928) ─────────────────────────────────────────────────
//
// Recall runs on every turn and used to leave no trace anyone could see: the
// only record was a `Recalled N chars` log line. The owner reading the Telegram
// flow roll could not tell that MEMORY.md had fired, let alone which section.
// These types carry WHAT was recalled out of the same ranking that produced the
// injected text, and render it as one chrome line.

/// Longest the recall line may be, in chars.
///
/// It shares the flow roll with the turn's real chrome (tool rows, retry
/// counters, the compaction notice), so it never gets to be a paragraph.
pub(crate) const RECALL_LINE_MAX_CHARS: usize = 160;

/// How many names a single source may show before the `+K` marker.
pub(crate) const RECALL_LINE_NAMES: usize = 2;

/// The prefix every recall line opens with.
const RECALL_LINE_PREFIX: &str = "🧠 recall";

/// One source's selected names, for the recall line.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RecallSource {
    /// Display names, best first, in injection order.
    pub names: Vec<String>,
    /// Sections this source matched but did not inject.
    pub omitted: usize,
}

impl RecallSource {
    fn is_empty(&self) -> bool {
        self.names.is_empty()
    }

    /// `label /a, /b +K`, or `None` when the source did not fire.
    fn segment(&self, label: &str) -> Option<String> {
        if self.names.is_empty() {
            return None;
        }
        let shown = self
            .names
            .iter()
            .take(RECALL_LINE_NAMES)
            .map(|n| format!("«{n}»"))
            .collect::<Vec<_>>()
            .join(", ");
        let mut segment = format!("{label} {shown}");
        let hidden = self.omitted + self.names.len().saturating_sub(RECALL_LINE_NAMES);
        if hidden > 0 {
            segment.push_str(&format!(" +{hidden}"));
        }
        Some(segment)
    }
}

/// What a turn's passive recall injected (#928).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RecallSummary {
    /// MEMORY.md section headings, best first.
    pub memory: RecallSource,
    /// Sources of the active-skill sections that rode along.
    pub active_skills: RecallSource,
    /// Project directive files whose sections rode along.
    pub directives: RecallSource,
    /// Characters of recall text injected.
    pub chars: usize,
}

impl RecallSummary {
    /// Whether any source fired. An empty summary renders NO line at all.
    pub fn is_empty(&self) -> bool {
        self.memory.is_empty() && self.active_skills.is_empty() && self.directives.is_empty()
    }

    /// The turn-start receipt: WHAT was recalled, never what it said.
    ///
    /// `None` when nothing was recalled — the flow roll must never carry an
    /// empty chrome row.
    pub fn render_line(&self) -> Option<String> {
        if self.is_empty() {
            return None;
        }
        let mut segments: Vec<String> = Vec::new();
        for (label, source) in [
            ("memory", &self.memory),
            ("active skills", &self.active_skills),
            ("directives", &self.directives),
        ] {
            if let Some(segment) = source.segment(label) {
                segments.push(segment);
            }
        }

        let suffix = format!("{}c", group_thousands(self.chars));
        // Drop whole sources from the TAIL before cutting a name in half: a
        // clipped «Open Questi…» reads as a section that does not exist, while
        // a missing trailing source is visibly absent from the line.
        loop {
            let body = format!("{RECALL_LINE_PREFIX} · {}", segments.join(" · "));
            let full = format!("{body} · {suffix}");
            if full.chars().count() <= RECALL_LINE_MAX_CHARS || segments.len() <= 1 {
                return Some(truncate_chars(&full, RECALL_LINE_MAX_CHARS));
            }
            segments.pop();
        }
    }
}

/// `1140` → `1,140`: the flow roll reads sizes at a glance.
fn group_thousands(n: usize) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// Truncate to at most `max` chars.
///
/// Char-boundary safe BY CONSTRUCTION — the cut is taken over `chars()`, so a
/// multi-byte heading can never be split into invalid UTF-8 — and marked with
/// an ellipsis so a clipped name is not read as the whole name.
fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// Section headings as the line shows them: no `#`, no blanks, deduplicated.
fn headings(sections: &[Section]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for section in sections {
        let heading = section
            .heading
            .trim()
            .trim_start_matches('#')
            .trim()
            .to_string();
        if !heading.is_empty() && !out.contains(&heading) {
            out.push(heading);
        }
    }
    out
}
