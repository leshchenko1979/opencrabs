//! Review instructions resolution module.
//!
//! Resolves custom plan and code review instructions across a two-tier hierarchy:
//! 1. Project level: working directory and git repository root.
//! 2. Profile level: ~/.opencrabs/profiles/<profile>/ brain files (`CODE.md` -> `AGENTS.md`).
//! 3. Built-in fallback: default adversarial review prompt.

use std::path::{Path, PathBuf};

/// The kind of review being performed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewKind {
    /// Plan review (pre-execution, modifying plan markdown).
    Plan,
    /// Code / Implementation review (post-execution, inspecting git diff and deliverables).
    Implementation,
}

/// The origin of the resolved review instructions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviewSource {
    /// Sourced from a file in the project workspace (e.g., "PLAN_REVIEW.md", "CODE.md").
    ProjectFile(String),
    /// Sourced from a profile brain file (e.g., "profile/CODE.md", "profile/AGENTS.md").
    ProfileBrain(String),
    /// No custom instructions found; built-in prompt used.
    Builtin,
}

impl ReviewSource {
    /// Short label suitable for display in progress bars and status indicators.
    pub fn display_label(&self) -> String {
        match self {
            Self::ProjectFile(name) => name.clone(),
            Self::ProfileBrain(name) => format!("profile/{name}"),
            Self::Builtin => "built-in".to_string(),
        }
    }
}

/// Resolved review instructions ready for injection into a subagent brief.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedReviewInstructions {
    /// The source from which instructions were resolved.
    pub source: ReviewSource,
    /// Custom instructions text (if any was found and extracted).
    pub content: Option<String>,
}

/// Resolves review instructions for the specified review kind.
///
/// Probing precedence:
/// 1. Working directory (`working_dir`) for project candidates.
/// 2. Enclosing git repository root (if `working_dir` is inside a git repo and different from `working_dir`).
/// 3. Profile directory (`profile_home`) for profile brain files (`CODE.md` -> `AGENTS.md`).
/// 4. Built-in fallback if no candidate files exist.
pub fn resolve_review_instructions(
    kind: ReviewKind,
    working_dir: &Path,
    profile_home: &Path,
) -> ResolvedReviewInstructions {
    // 1. Search project locations
    let mut search_dirs = vec![working_dir.to_path_buf()];
    if let Some(git_root) = find_git_repo_root(working_dir) {
        if git_root != working_dir && !search_dirs.contains(&git_root) {
            search_dirs.push(git_root);
        }
    }

    let project_candidates: &[&str] = match kind {
        ReviewKind::Plan => &[
            "PLAN_REVIEW.md",
            "REVIEW.md",
            "CONVENTIONS.md",
            "AGENTS.md",
            "CODE.md",
            "CLAUDE.md",
        ],
        ReviewKind::Implementation => &[
            "CODE_REVIEW.md",
            "REVIEW.md",
            "CODE.md",
            "CONVENTIONS.md",
            "AGENTS.md",
            "CLAUDE.md",
        ],
    };

    for dir in &search_dirs {
        for &candidate in project_candidates {
            if let Some((path, matched_filename)) = probe_file_case_insensitive(dir, candidate) {
                if let Ok(raw_content) = std::fs::read_to_string(&path) {
                    let extracted = extract_review_section(kind, candidate, &raw_content);
                    if !extracted.trim().is_empty() {
                        return ResolvedReviewInstructions {
                            source: ReviewSource::ProjectFile(matched_filename),
                            content: Some(extracted),
                        };
                    }
                }
            }
        }
    }

    // 2. Search profile brain locations
    let profile_candidates: &[&str] = match kind {
        ReviewKind::Plan => &["CODE.md", "AGENTS.md"],
        ReviewKind::Implementation => &["CODE.md", "AGENTS.md"],
    };

    for &candidate in profile_candidates {
        if let Some((path, matched_filename)) = probe_file_case_insensitive(profile_home, candidate)
        {
            if let Ok(raw_content) = std::fs::read_to_string(&path) {
                let extracted = extract_review_section(kind, candidate, &raw_content);
                if !extracted.trim().is_empty() {
                    return ResolvedReviewInstructions {
                        source: ReviewSource::ProfileBrain(matched_filename),
                        content: Some(extracted),
                    };
                }
            }
        }
    }

    // 3. Built-in fallback
    ResolvedReviewInstructions {
        source: ReviewSource::Builtin,
        content: None,
    }
}

/// Locates the root of the git repository containing `path`, if any.
fn find_git_repo_root(start: &Path) -> Option<PathBuf> {
    let mut current = if start.is_file() {
        start.parent()?.to_path_buf()
    } else {
        start.to_path_buf()
    };

    loop {
        if current.join(".git").exists() {
            return Some(current);
        }
        if !current.pop() {
            break;
        }
    }
    None
}

/// Case-insensitively checks if `filename` exists inside `dir`.
/// Returns the actual path on disk and the exact file name found.
fn probe_file_case_insensitive(dir: &Path, filename: &str) -> Option<(PathBuf, String)> {
    // Direct check first (fast path)
    let direct = dir.join(filename);
    if direct.is_file() {
        return Some((direct, filename.to_string()));
    }

    // Directory listing for case-insensitive match
    let entries = std::fs::read_dir(dir).ok()?;
    let target_lower = filename.to_lowercase();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_file() {
            if let Some(name_os) = path.file_name() {
                if let Some(name_str) = name_os.to_str() {
                    if name_str.to_lowercase() == target_lower {
                        return Some((path, name_str.to_string()));
                    }
                }
            }
        }
    }
    None
}

/// Extracts the relevant review section from a candidate file.
///
/// If the file is a dedicated review file (`PLAN_REVIEW.md`, `CODE_REVIEW.md`), returns the full content.
/// For multi-topic files (`REVIEW.md`, `CODE.md`, `AGENTS.md`, `CONVENTIONS.md`, `CLAUDE.md`),
/// attempts to find a heading matching the target review kind.
/// If no specific heading matches, falls back to the full file content.
fn extract_review_section(kind: ReviewKind, candidate_pattern: &str, content: &str) -> String {
    let is_dedicated = match kind {
        ReviewKind::Plan => candidate_pattern.eq_ignore_ascii_case("PLAN_REVIEW.md"),
        ReviewKind::Implementation => candidate_pattern.eq_ignore_ascii_case("CODE_REVIEW.md"),
    };

    if is_dedicated {
        return content.trim().to_string();
    }

    // Multi-topic files: attempt section extraction
    let target_headings: &[&str] = match kind {
        ReviewKind::Plan => &["plan review", "plan", "planning"],
        ReviewKind::Implementation => &[
            "code review",
            "implementation review",
            "code",
            "coding",
            "standards",
        ],
    };

    if let Some(section) = extract_heading_section(content, target_headings) {
        if !section.trim().is_empty() {
            return section.trim().to_string();
        }
    }

    // If no matching heading found, fall back to full content
    content.trim().to_string()
}

/// Extracts the section under the first heading matching any of `target_headings` (case-insensitive).
/// Scans until the next heading of equal or higher level (fewer or equal `#` characters).
fn extract_heading_section(content: &str, target_headings: &[&str]) -> Option<String> {
    let lines: Vec<&str> = content.lines().collect();
    let mut matched_heading_level = None;
    let mut section_lines = Vec::new();
    let mut capturing = false;

    for line in lines {
        let trimmed = line.trim_start();
        if trimmed.starts_with('#') {
            let hash_count = trimmed.chars().take_while(|&c| c == '#').count();
            let heading_text = trimmed[hash_count..].trim().to_lowercase();

            if capturing {
                // If we encounter a heading of equal or higher level, stop capturing
                if let Some(start_level) = matched_heading_level {
                    if hash_count <= start_level {
                        break;
                    }
                }
            } else {
                // Check if this heading matches any of target_headings
                for &target in target_headings {
                    // Match exact word or prefix (e.g., "plan review", "code review guidelines")
                    if heading_text == target || heading_text.starts_with(target) {
                        capturing = true;
                        matched_heading_level = Some(hash_count);
                        break;
                    }
                }
            }
        }

        if capturing {
            section_lines.push(line);
        }
    }

    if section_lines.is_empty() {
        None
    } else {
        Some(section_lines.join("\n"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn test_builtin_fallback_when_empty() {
        let work = tempdir().unwrap();
        let prof = tempdir().unwrap();

        let res = resolve_review_instructions(ReviewKind::Plan, work.path(), prof.path());
        assert_eq!(res.source, ReviewSource::Builtin);
        assert!(res.content.is_none());
        assert_eq!(res.source.display_label(), "built-in");
    }

    #[test]
    fn test_dedicated_plan_review_file() {
        let work = tempdir().unwrap();
        let prof = tempdir().unwrap();

        let plan_file = work.path().join("PLAN_REVIEW.md");
        fs::write(&plan_file, "Always require verification commands.").unwrap();

        let res = resolve_review_instructions(ReviewKind::Plan, work.path(), prof.path());
        assert_eq!(
            res.source,
            ReviewSource::ProjectFile("PLAN_REVIEW.md".to_string())
        );
        assert_eq!(
            res.content.as_deref(),
            Some("Always require verification commands.")
        );
        assert_eq!(res.source.display_label(), "PLAN_REVIEW.md");
    }

    #[test]
    fn test_case_insensitive_dedicated_file() {
        let work = tempdir().unwrap();
        let prof = tempdir().unwrap();

        let code_file = work.path().join("code_review.md");
        fs::write(&code_file, "Check lock safety and error handling.").unwrap();

        let res = resolve_review_instructions(ReviewKind::Implementation, work.path(), prof.path());
        assert_eq!(
            res.source,
            ReviewSource::ProjectFile("code_review.md".to_string())
        );
        assert_eq!(
            res.content.as_deref(),
            Some("Check lock safety and error handling.")
        );
    }

    #[test]
    fn test_section_extraction_from_multi_topic_file() {
        let work = tempdir().unwrap();
        let prof = tempdir().unwrap();

        let review_md = work.path().join("REVIEW.md");
        let content = r#"# Project Review Guidelines

## Plan Review
Verify all acceptance criteria have runnable commands.

## Code Review
Verify no unhandled unwrap() calls exist.
"#;
        fs::write(&review_md, content).unwrap();

        let plan_res = resolve_review_instructions(ReviewKind::Plan, work.path(), prof.path());
        assert_eq!(
            plan_res.source,
            ReviewSource::ProjectFile("REVIEW.md".to_string())
        );
        assert!(plan_res
            .content
            .as_ref()
            .unwrap()
            .contains("Verify all acceptance criteria"));
        assert!(!plan_res.content.as_ref().unwrap().contains("unwrap()"));

        let code_res =
            resolve_review_instructions(ReviewKind::Implementation, work.path(), prof.path());
        assert_eq!(
            code_res.source,
            ReviewSource::ProjectFile("REVIEW.md".to_string())
        );
        assert!(code_res
            .content
            .as_ref()
            .unwrap()
            .contains("Verify no unhandled unwrap()"));
        assert!(!code_res
            .content
            .as_ref()
            .unwrap()
            .contains("acceptance criteria"));
    }

    #[test]
    fn test_profile_fallback_when_project_has_none() {
        let work = tempdir().unwrap();
        let prof = tempdir().unwrap();

        let prof_code = prof.path().join("CODE.md");
        fs::write(
            &prof_code,
            "## Standards\nAll tests must pass in temporary directories.",
        )
        .unwrap();

        let res = resolve_review_instructions(ReviewKind::Implementation, work.path(), prof.path());
        assert_eq!(
            res.source,
            ReviewSource::ProfileBrain("CODE.md".to_string())
        );
        assert!(res
            .content
            .as_ref()
            .unwrap()
            .contains("All tests must pass"));
        assert_eq!(res.source.display_label(), "profile/CODE.md");
    }

    #[test]
    fn test_profile_agents_fallback() {
        let work = tempdir().unwrap();
        let prof = tempdir().unwrap();

        let prof_agents = prof.path().join("AGENTS.md");
        fs::write(
            &prof_agents,
            "## Planning\nNever schedule multi-step refactors without a design gate.",
        )
        .unwrap();

        let res = resolve_review_instructions(ReviewKind::Plan, work.path(), prof.path());
        assert_eq!(
            res.source,
            ReviewSource::ProfileBrain("AGENTS.md".to_string())
        );
        assert!(res
            .content
            .as_ref()
            .unwrap()
            .contains("Never schedule multi-step refactors"));
        assert_eq!(res.source.display_label(), "profile/AGENTS.md");
    }
}
