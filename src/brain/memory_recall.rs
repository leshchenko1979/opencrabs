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
/// net win on both halves:
///
/// | | recall@2 | FPR |
/// |---|---|---|
/// | 0.35, no boost (shipped before) | 0.917 | 0.250 |
/// | 0.35, boost 4 | 1.000 | 0.250 |
/// | **0.38, boost 4 (shipped)** | **1.000** | **0.167** |
///
/// 0.38 is the bottom of the plateau that yields FPR 0.167 — every value from
/// here to ~0.45 scores the same on the memory pair, so taking the lowest one
/// keeps the most recall on the larger Track B corpus. It also buys headroom:
/// the previous 0.250 sat exactly on the `<= 0.25` contract, so any future
/// change to the ranker broke the gate by arithmetic rather than by defect.
pub(crate) const RECALL_MIN_SCORE: f64 = 0.38;

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

/// Recall relevant to `user_message`, formatted for context, or `None`.
///
/// Pure: takes the file content, so the decision is testable without a home
/// directory or a populated MEMORY.md.
pub fn recall_from(memory: &str, user_message: &str) -> Option<String> {
    if !worth_reading_for(user_message) {
        return None;
    }
    render_memory(Ranked::build(memory).find_relevant(
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
pub async fn recall_for(user_message: &str) -> Option<String> {
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
            let matches = c.indexed.find_relevant(
                user_message,
                RECALL_MAX_SECTIONS,
                RECALL_MAX_CHARS,
                RECALL_MIN_SCORE,
            );
            touch_recalled(&matches);
            return render_memory(matches);
        }
    }

    let memory = tokio::fs::read_to_string(&path).await.ok()?;
    let indexed = Ranked::build(&memory);
    let matches = indexed.find_relevant(
        user_message,
        RECALL_MAX_SECTIONS,
        RECALL_MAX_CHARS,
        RECALL_MIN_SCORE,
    );

    if let Ok(mut cache) = MEMORY_CACHE.write() {
        tracing::debug!(
            "MEMORY.md re-parsed for recall: {} sections, {} bytes",
            indexed.len(),
            memory.len()
        );
        *cache = Some(Cached { stamp, indexed });
    }

    touch_recalled(&matches);
    render_memory(matches)
}

/// Format matches from MEMORY.md.
fn render_memory(matches: brain_sections::Matches) -> Option<String> {
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

/// Recall relevant sections from active skills (#285).
/// Matches against active skills' auxiliary markdown files and body sections.
pub fn recall_from_active_skills(
    active_skills: &HashSet<String>,
    user_message: &str,
) -> Option<String> {
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
    let matched_indices = indexed.find_relevant_indices(
        user_message,
        RECALL_MAX_SECTIONS,
        RECALL_MAX_CHARS,
        RECALL_MIN_SCORE,
    );

    if matched_indices.is_empty() {
        return None;
    }

    let mut rendered_blocks = Vec::new();
    for i in matched_indices {
        let ns = &named_sections[i];
        rendered_blocks.push(format!(
            "─── from active {}, possibly relevant ───\n{}",
            ns.source_name,
            ns.section.render()
        ));
    }

    Some(rendered_blocks.join("\n\n"))
}

/// Recall relevant sections from project directive files (#285).
/// Discovers directives in `working_dir` (e.g. `CLAUDE.md`, `AGENTS.md`, `.cursor/rules/*.mdc`)
/// and matches relevant sections unprompted.
pub async fn recall_from_project_directives(
    working_dir: &Path,
    user_message: &str,
) -> Option<String> {
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
    let matched_indices = indexed.find_relevant_indices(
        user_message,
        RECALL_MAX_SECTIONS,
        RECALL_MAX_CHARS,
        RECALL_MIN_SCORE,
    );

    if matched_indices.is_empty() {
        return None;
    }

    let mut rendered_blocks = Vec::new();
    for i in matched_indices {
        let ns = &named_sections[i];
        rendered_blocks.push(format!(
            "─── from {}, possibly relevant ───\n{}",
            ns.source_name,
            ns.section.render()
        ));
    }

    Some(rendered_blocks.join("\n\n"))
}

/// Unified passive recall for a session (#285):
/// Aggregates recall from MEMORY.md, active skills, and project directives.
pub async fn recall_for_session(
    session_id: uuid::Uuid,
    working_dir: Option<&Path>,
    user_message: &str,
) -> Option<String> {
    if !worth_reading_for(user_message) {
        return None;
    }

    let mut parts: Vec<String> = Vec::new();

    // 0. Skill selection (#901) — "which skill applies" is the most actionable
    //    hint of the three, so it leads. The full index is still in the system
    //    prompt; this only ranks it against the message.
    if let Some(shortlist) = recall_skills(user_message) {
        parts.push(shortlist);
    }

    // 1. MEMORY.md
    if let Some(mem) = recall_for(user_message).await {
        parts.push(mem);
    }

    // 2. Active skills
    let active_skills = crate::brain::tools::seen_skills::active_for_session(session_id);
    if let Some(skill_recall) = recall_from_active_skills(&active_skills, user_message) {
        parts.push(skill_recall);
    }

    // 3. Project directives
    if let Some(wd) = working_dir
        && let Some(directive_recall) = recall_from_project_directives(wd, user_message).await
    {
        parts.push(directive_recall);
    }

    if parts.is_empty() {
        None
    } else {
        Some(parts.join("\n\n"))
    }
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
/// 1. **ORDER** — the whole index at floor `0.0`, exactly as the scorecard's
///    C1@5 orders it. If this path used the shipped floor to build the list, a
///    gold skill ranked third could vanish because nothing cleared the floor,
///    and C1@5 would then be a number about the harness rather than about the
///    product.
/// 2. **SILENCE** — whether to say anything at all, decided by the shipped
///    [`RECALL_MIN_SCORE`]. A shortlist invented from a weak match is noise paid
///    on every turn, which is the same trade the memory recall makes.
pub fn recall_skills_from(index_md: &str, user_message: &str) -> Option<String> {
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
    let any_clears = ranked.find_relevant(user_message, 1, usize::MAX, RECALL_MIN_SCORE);
    if any_clears.sections.is_empty()
        && !ranked.any_clears_raw(user_message, SKILL_RECALL_RAW_FLOOR)
    {
        return None;
    }

    // `usize::MAX` chars: the shortlist is about ORDER, and truncating it by a
    // character budget would drop entries by length rather than by score.
    let ordered = ranked.find_relevant(user_message, SKILL_RECALL_MAX, usize::MAX, 0.0);
    let names: Vec<String> = ordered
        .sections
        .iter()
        .map(|s| s.heading.trim().trim_start_matches('#').trim().to_string())
        .filter(|n| !n.is_empty())
        .collect();
    if names.is_empty() {
        return None;
    }
    let listed = names
        .iter()
        .map(|n| format!("/{n}"))
        .collect::<Vec<_>>()
        .join(", ");
    Some(format!(
        "─── skills matching this task, best first ───\n{listed}\n\
         [Ranked against your message; the full index is in the system prompt.]"
    ))
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
pub fn recall_skills(user_message: &str) -> Option<String> {
    // Cheap gate first: no query terms means no disk read at all.
    if !worth_reading_for(user_message) {
        return None;
    }
    recall_skills_from(&skill_index_markdown(), user_message)
}
