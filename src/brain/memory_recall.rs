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
/// Below this the shortlist stops filtering at all, and a `0.0` typo would put
/// an unfiltered list on every turn — degradation with no error to notice. The
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

/// Raw (length-unnormalized) BM25 score an entry must reach before a
/// multi-topic message may summon the skill shortlist through the raw door.
///
/// [`RECALL_MIN_SCORE`] is divided by query length — right for ranking, and
/// wrong as the only silence gate: a message naming eight topics scores
/// every entry it genuinely matches at a fraction of what a focused message
/// scores, so matching MORE skills reads as matching NONE. This door drops
/// the normalization. With IDF normalized to at most 1.0, one shared term
/// scores 1.0 raw at typical entry length (2.5 / (1 + K1), K1 = 1.5) and
/// stays under ~1.3 short of pathological length ratios, so 1.5 sits above
/// the single-coincidence shape and at or below where two distinct shared
/// terms land (~2.0). Pinned by the battery:
/// `the_shortlist_never_exceeds_the_cap` (the eight-topic shape) must stay
/// loud, and `an_unmatched_message_produces_no_shortlist` (zero overlap)
/// must stay silent.
pub(crate) const SKILL_RECALL_RAW_FLOOR: f64 = 1.5;

/// At least this many DISTINCT query terms must agree on an entry before it
/// may be named in the skill shortlist (#939).
///
/// One shared word is a coincidence, not a match: a 15-word message shares some
/// stemmed term with most of a 44-entry skill index, and that is precisely how
/// the list used to fill all five slots. Two is the smallest number that
/// requires agreement rather than a single coincidence.
pub(crate) const SKILL_RECALL_MIN_TERMS: usize = 2;

/// How much information, summed over the distinct matched terms, an entry must
/// carry to be named (#939).
///
/// This is the weighting answer to `query_terms` having no stopword handling,
/// and it deliberately is NOT a wordlist: idf is measured against the corpus
/// being ranked, so a word that is filler HERE (`the`, `skill`, `fix` — idf
/// 0.19, 0.49, 0.75 on the live index) contributes almost nothing, while a term
/// carried by one entry contributes its full weight (1.0). `1.5` therefore
/// means "two terms, at least one of them specific", which is what separates a
/// message about a skill from a message that merely contains words the index
/// also contains.
pub(crate) const SKILL_RECALL_MIN_SUMMED_IDF: f64 = 1.5;

/// How close to the best admitted entry's score another entry must be to be
/// named alongside it (#939).
///
/// Absolute floors cannot do this job: scores are normalized by query length
/// and are not comparable across messages, so a floor that trims a five-entry
/// list on one turn silences a genuine match on another. A share of the best
/// score is query-relative by construction.
///
/// Measured on the live 44-entry index (`opencrabs eval skills` reports 44
/// entries, C1@5 0.657) for the message in #939: at `0.5` two entries survive
/// and the second matches only `the`/`skill`/`get`; at `0.7` exactly one
/// survives — the entry that shares the message's rare term — and the mean
/// list length over the 35 labelled Track C positives drops from 5.00 to 1.40.
/// The cost is real: the shortlist's own gold-recall moves 0.657 → 0.514
/// (C1@5 as the scorecard computes it does not move, because `score()` still
/// ranks at floor `0.0`), while false selection on the 12 negatives moves
/// 1.000 → 0.083. Both figures, and the sweep that shows they trade against
/// each other, are recorded in `src/eval/results/skill-shortlist-floor-939.md`.
pub(crate) const SKILL_RECALL_RATIO: f64 = 0.7;

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

    // 0. Skill selection (#901) — "which skill applies" is the most actionable
    //    hint of the three, so it leads. The full index is still in the system
    //    prompt; this only ranks it against the message.
    if let Some(names) = skill_shortlist_names(&skill_index_markdown(), user_message, floor) {
        parts.push(render_skill_shortlist(&names));
        summary.skills = RecallSource { names, omitted: 0 };
    }

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

// ── Skill SELECTION (#901) ────────────────────────────────────────────────
//
// `recall_from_active_skills` above ranks sections of skills that are ALREADY
// active. Nothing selected which skill to activate: the system prompt emits
// every index entry and leaves the choice to the model, which is why the
// scorecard's C1@5 read 0.371 while no shipped code path consumed it.
//
// This ranks the SAME index the scorecard scores against the user's message and
// rides the best few along with it. The full index stays in the system prompt:
// it is stable there, so it stays cacheable, and a skill that misses the top
// few remains reachable in that block. Only this HINT is volatile.

/// At most this many skills ride along as a ranked shortlist.
pub(crate) const SKILL_RECALL_MAX: usize = 5;

/// How long the rendered index is reused before the skill files are re-read.
///
/// `load_all_skills()` parses every `SKILL.md`. The prompt builder already pays
/// that once per turn; this path must not double it. A minute is short enough
/// that a skill added at runtime still shows up promptly.
const SKILLS_CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(60);

struct CachedSkills {
    at: std::time::Instant,
    index_md: String,
}

static SKILLS_CACHE: std::sync::RwLock<Option<CachedSkills>> = std::sync::RwLock::new(None);

/// Rank the skill index against `user_message` and render the shortlist.
///
/// Pure: takes the index markdown `index_markdown` produces, so the ranking is
/// testable without a brain directory, and the corpus ranked here is the same
/// one the Track C scorecard scores rather than a second rendering that can
/// drift from it.
///
/// Two decisions, kept separate on purpose:
///
/// 1. **ADMISSION** — which entries are allowed on the list at all, decided by
///    [`Ranked::admitted_indices`] at the constants below (#939). This is
///    deliberately NOT the scorecard's floor-`0.0` ordering: that ordering
///    admits every entry sharing one stemmed term, so with a cap of
///    [`SKILL_RECALL_MAX`] the list filled all five slots on almost every turn
///    and named skills the message had nothing to do with.
/// 2. **SILENCE** — whether to say anything at all, decided by the resolved
///    floor (#912: `[memory].recall_min_score`, defaulting to
///    [`RECALL_MIN_SCORE`]). A shortlist invented from a weak match is noise
///    paid on every turn, which is the same trade the memory recall makes.
///
/// The two used to disagree about what "matched" meant — the silence door
/// accepted one rare term while the ordering door accepted anything at all —
/// and that gap is what #939 measured. They still answer different questions
/// (may I speak / what may I name), but the naming side is now a selection,
/// not an ordering.
pub fn recall_skills_from(index_md: &str, user_message: &str, floor: f64) -> Option<String> {
    skill_shortlist_names(index_md, user_message, floor).map(|names| render_skill_shortlist(&names))
}

/// The ranked shortlist as NAMES, best first — the one ranking behind both the
/// injected block and the recall line's `skills` segment (#928).
fn skill_shortlist_names(index_md: &str, user_message: &str, floor: f64) -> Option<Vec<String>> {
    if !worth_reading_for(user_message) {
        return None;
    }
    let ranked = Ranked::build(index_md);

    // Silence unless something genuinely matches. Two doors: the shipped
    // length-normalized floor for focused messages, and — because that
    // normalization makes a message naming many topics score each entry it
    // genuinely matches at a fraction of a focused message's strength — a
    // raw unnormalized floor ([`SKILL_RECALL_RAW_FLOOR`]) for multi-topic
    // ones. Matching more skills must never read as matching none.
    let any_clears = ranked.find_relevant(user_message, 1, usize::MAX, floor);
    if any_clears.sections.is_empty()
        && !ranked.any_clears_raw(user_message, SKILL_RECALL_RAW_FLOOR)
    {
        return None;
    }

    // ADMISSION, not ordering (#939). The character budget is irrelevant here:
    // the list is capped by `SKILL_RECALL_MAX` entries and every admitted
    // entry has already earned its place on content, so a length cut would
    // drop entries by size rather than by evidence.
    let admitted = ranked.admitted_indices(
        user_message,
        SKILL_RECALL_MAX,
        SKILL_RECALL_MIN_TERMS,
        SKILL_RECALL_MIN_SUMMED_IDF,
        SKILL_RECALL_RATIO,
    );
    let names: Vec<String> = admitted
        .iter()
        .map(|&i| ranked.heading(i))
        .filter(|n| !n.is_empty())
        .collect();
    if names.is_empty() {
        return None;
    }
    Some(names)
}

/// Render the shortlist block that rides along in the prompt.
fn render_skill_shortlist(names: &[String]) -> String {
    let listed = names
        .iter()
        .map(|n| format!("/{n}"))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "─── skills matching this task, best first ───\n{listed}\n\
         [Ranked against your message; the full index is in the system prompt.]"
    )
}

/// The live skill index, re-read at most once per [`SKILLS_CACHE_TTL`].
fn skill_index_markdown() -> String {
    if let Ok(cache) = SKILLS_CACHE.read()
        && let Some(c) = cache.as_ref()
        && c.at.elapsed() < SKILLS_CACHE_TTL
    {
        return c.index_md.clone();
    }

    let skills = crate::brain::skills::load_all_skills();
    let index_md = crate::brain::skills::index_markdown(
        skills
            .iter()
            .map(|s| (s.name.as_str(), s.description.as_str())),
    );

    if let Ok(mut cache) = SKILLS_CACHE.write() {
        *cache = Some(CachedSkills {
            at: std::time::Instant::now(),
            index_md: index_md.clone(),
        });
    }
    index_md
}

/// Load the live skill index and rank it against `user_message` (#901).
pub fn recall_skills(user_message: &str, floor: f64) -> Option<String> {
    // Cheap gate first: no query terms means no disk read at all.
    if !worth_reading_for(user_message) {
        return None;
    }
    recall_skills_from(&skill_index_markdown(), user_message, floor)
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
    fn segment(&self, label: &str, slash: bool) -> Option<String> {
        if self.names.is_empty() {
            return None;
        }
        let shown = self
            .names
            .iter()
            .take(RECALL_LINE_NAMES)
            .map(|n| {
                if slash {
                    format!("/{n}")
                } else {
                    format!("«{n}»")
                }
            })
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
    /// Skills named by the index shortlist (#901), best first.
    pub skills: RecallSource,
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
        self.skills.is_empty()
            && self.memory.is_empty()
            && self.active_skills.is_empty()
            && self.directives.is_empty()
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
        for (label, source, slash) in [
            ("skills", &self.skills, true),
            ("memory", &self.memory, false),
            ("active skills", &self.active_skills, false),
            ("directives", &self.directives, false),
        ] {
            if let Some(segment) = source.segment(label, slash) {
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
