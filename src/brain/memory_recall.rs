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
pub(crate) const RECALL_MIN_SCORE: f64 = 0.35;

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
            return render_memory(c.indexed.find_relevant(
                user_message,
                RECALL_MAX_SECTIONS,
                RECALL_MAX_CHARS,
                RECALL_MIN_SCORE,
            ));
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

        // 2. Main skill prompt body
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
