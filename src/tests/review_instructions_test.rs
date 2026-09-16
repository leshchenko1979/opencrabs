//! Comprehensive tests for review instructions resolution (#256).

use std::fs;
use tempfile::tempdir;

use opencrabs::brain::review_instructions::{
    resolve_review_instructions, ReviewKind, ReviewSource,
};

#[test]
fn test_plan_review_precedence_in_project_workspace() {
    let work = tempdir().unwrap();
    let prof = tempdir().unwrap();

    // 1. If only AGENTS.md exists, it matches
    let agents_md = work.path().join("AGENTS.md");
    fs::write(&agents_md, "## Planning\nRequire clear steps.").unwrap();
    let res1 = resolve_review_instructions(ReviewKind::Plan, work.path(), prof.path());
    assert_eq!(
        res1.source,
        ReviewSource::ProjectFile("AGENTS.md".to_string())
    );
    assert_eq!(
        res1.content.as_deref(),
        Some("## Planning\nRequire clear steps.")
    );

    // 2. If CONVENTIONS.md is added, it outranks AGENTS.md
    let conv_md = work.path().join("CONVENTIONS.md");
    fs::write(&conv_md, "## Plan Review\nConventions rule.").unwrap();
    let res2 = resolve_review_instructions(ReviewKind::Plan, work.path(), prof.path());
    assert_eq!(
        res2.source,
        ReviewSource::ProjectFile("CONVENTIONS.md".to_string())
    );
    assert_eq!(
        res2.content.as_deref(),
        Some("## Plan Review\nConventions rule.")
    );

    // 3. If REVIEW.md is added, it outranks CONVENTIONS.md
    let rev_md = work.path().join("REVIEW.md");
    fs::write(&rev_md, "## Plan Review\nReview md rule.").unwrap();
    let res3 = resolve_review_instructions(ReviewKind::Plan, work.path(), prof.path());
    assert_eq!(
        res3.source,
        ReviewSource::ProjectFile("REVIEW.md".to_string())
    );
    assert_eq!(
        res3.content.as_deref(),
        Some("## Plan Review\nReview md rule.")
    );

    // 4. If PLAN_REVIEW.md is added, it outranks everything
    let plan_rev_md = work.path().join("PLAN_REVIEW.md");
    fs::write(&plan_rev_md, "Dedicated plan review directives.").unwrap();
    let res4 = resolve_review_instructions(ReviewKind::Plan, work.path(), prof.path());
    assert_eq!(
        res4.source,
        ReviewSource::ProjectFile("PLAN_REVIEW.md".to_string())
    );
    assert_eq!(
        res4.content.as_deref(),
        Some("Dedicated plan review directives.")
    );
}

#[test]
fn test_code_review_precedence_in_project_workspace() {
    let work = tempdir().unwrap();
    let prof = tempdir().unwrap();

    // 1. If only AGENTS.md exists, it matches
    let agents_md = work.path().join("AGENTS.md");
    fs::write(&agents_md, "## Standards\nNever write unhandled unwraps.").unwrap();
    let res1 = resolve_review_instructions(ReviewKind::Implementation, work.path(), prof.path());
    assert_eq!(
        res1.source,
        ReviewSource::ProjectFile("AGENTS.md".to_string())
    );
    assert_eq!(
        res1.content.as_deref(),
        Some("## Standards\nNever write unhandled unwraps.")
    );

    // 2. If CODE.md is added, it outranks AGENTS.md
    let code_md = work.path().join("CODE.md");
    fs::write(&code_md, "## Code Review\nVerify test coverage.").unwrap();
    let res2 = resolve_review_instructions(ReviewKind::Implementation, work.path(), prof.path());
    assert_eq!(
        res2.source,
        ReviewSource::ProjectFile("CODE.md".to_string())
    );
    assert_eq!(
        res2.content.as_deref(),
        Some("## Code Review\nVerify test coverage.")
    );

    // 3. If CODE_REVIEW.md is added, it outranks CODE.md
    let code_rev_md = work.path().join("CODE_REVIEW.md");
    fs::write(&code_rev_md, "Dedicated implementation review directives.").unwrap();
    let res3 = resolve_review_instructions(ReviewKind::Implementation, work.path(), prof.path());
    assert_eq!(
        res3.source,
        ReviewSource::ProjectFile("CODE_REVIEW.md".to_string())
    );
    assert_eq!(
        res3.content.as_deref(),
        Some("Dedicated implementation review directives.")
    );
}

#[test]
fn test_profile_fallback_precedence() {
    let work = tempdir().unwrap();
    let prof = tempdir().unwrap();

    // In profile home: AGENTS.md matches if CODE.md has no match
    let prof_agents = prof.path().join("AGENTS.md");
    fs::write(&prof_agents, "## Standards\nProfile level standards.").unwrap();

    let res1 = resolve_review_instructions(ReviewKind::Implementation, work.path(), prof.path());
    assert_eq!(
        res1.source,
        ReviewSource::ProfileBrain("AGENTS.md".to_string())
    );
    assert_eq!(res1.source.display_label(), "profile/AGENTS.md");
    assert_eq!(
        res1.content.as_deref(),
        Some("## Standards\nProfile level standards.")
    );

    // If profile CODE.md is created, it outranks profile AGENTS.md
    let prof_code = prof.path().join("CODE.md");
    fs::write(&prof_code, "## Code Review\nProfile code review rules.").unwrap();

    let res2 = resolve_review_instructions(ReviewKind::Implementation, work.path(), prof.path());
    assert_eq!(
        res2.source,
        ReviewSource::ProfileBrain("CODE.md".to_string())
    );
    assert_eq!(res2.source.display_label(), "profile/CODE.md");
    assert_eq!(
        res2.content.as_deref(),
        Some("## Code Review\nProfile code review rules.")
    );
}

#[test]
fn test_git_root_discovery_when_working_in_subdirectory() {
    let work_root = tempdir().unwrap();
    let prof = tempdir().unwrap();

    // Create .git in work_root
    fs::create_dir_all(work_root.path().join(".git")).unwrap();

    // Create a sub-directory
    let sub_dir = work_root.path().join("src/nested");
    fs::create_dir_all(&sub_dir).unwrap();

    // Put PLAN_REVIEW.md in work_root
    let plan_file = work_root.path().join("PLAN_REVIEW.md");
    fs::write(&plan_file, "Root level plan review.").unwrap();

    // Run resolution with sub_dir as working_dir
    let res = resolve_review_instructions(ReviewKind::Plan, &sub_dir, prof.path());
    assert_eq!(
        res.source,
        ReviewSource::ProjectFile("PLAN_REVIEW.md".to_string())
    );
    assert_eq!(res.content.as_deref(), Some("Root level plan review."));
}
