//! Regression tests for #179 — canonical skill slug + tolerant ingestion.
//!
//! **The defect.** The compaction manifest documents its entries in the
//! `- <skill-slug>` (bare) form, and `parse_context_manifest` stores them
//! verbatim — but the re-injection matcher compared against
//! `Skill::slash_name`, which carries the invocation sigil. A manifest
//! written exactly as documented therefore registered `canarya`, failed
//! `contains("/canarya")`, and injected **nothing**. Silently: no error, no
//! log line, the skill simply stopped existing after the next compaction.
//!
//! **The contract these tests pin.** One canonical key (the bare slug) and a
//! tolerant ingestion edge, so *both* spellings resolve to the same entry.
//! The inversion test is `documented_manifest_spelling_selects_the_skill_body`
//! — it fails against the pre-fix matcher, which is the whole point.

use crate::brain::skills::{
    Skill, SkillSource, active_skill_bodies, active_skill_injection, normalize_skill_slug,
};
use std::collections::HashSet;

/// Minimal valid `SKILL.md` blob for `name`.
fn skill_md(name: &str, body: &str) -> String {
    format!("---\nname: {name}\ndescription: test skill {name}\n---\n\n{body}\n")
}

/// A parsed synthetic skill whose body is recognisable in assertions.
fn skill(name: &str) -> Skill {
    let body = format!("BODY-OF-{name}");
    Skill::parse(name, &skill_md(name, &body), SkillSource::Builtin)
        .expect("synthetic test skill should parse")
}

fn set_of(items: &[&str]) -> HashSet<String> {
    items.iter().map(|s| s.to_string()).collect()
}

// --- normalize_skill_slug: the ingestion edge -----------------------------

/// The invocation sigil is presentation, not identity: it is stripped.
#[test]
fn normalize_skill_slug_strips_the_invocation_sigil() {
    assert_eq!(normalize_skill_slug("/opencrabs-dev"), "opencrabs-dev");
    assert_eq!(normalize_skill_slug("opencrabs-dev"), "opencrabs-dev");
}

/// A manifest item can arrive with incidental whitespace; it is trimmed.
#[test]
fn normalize_skill_slug_trims_surrounding_whitespace() {
    assert_eq!(normalize_skill_slug("  opencrabs-dev  "), "opencrabs-dev");
    assert_eq!(normalize_skill_slug("\t/opencrabs-dev\n"), "opencrabs-dev");
}

/// Exactly ONE sigil is stripped, and the result is deliberately NOT
/// idempotent for a double sigil (`//x` -> `/x`, a second pass would give
/// `x`). Pinned so the behaviour is a decision rather than an accident:
/// a `//x` entry is a malformed manifest item, and silently collapsing it
/// to `x` would let a typo resolve to a real skill.
#[test]
fn normalize_skill_slug_strips_at_most_one_sigil() {
    let once = normalize_skill_slug("//canarya");
    assert_eq!(once, "/canarya");

    let twice = normalize_skill_slug(&once);
    assert_ne!(
        twice, once,
        "double-sigil normalisation must not be idempotent by accident"
    );
}

/// Empty and sigil-only inputs degrade to an empty slug rather than panicking.
#[test]
fn normalize_skill_slug_handles_degenerate_input() {
    assert_eq!(normalize_skill_slug(""), "");
    assert_eq!(normalize_skill_slug("/"), "");
    assert_eq!(normalize_skill_slug("   "), "");
}

// --- the identity invariant the matcher depends on ------------------------

/// Every shipped skill exposes a bare `name` and a sigil-prefixed
/// `slash_name` that normalises back to it. This is the invariant that makes
/// the documented manifest spelling work — if `name` ever carried a sigil,
/// the matcher would break again.
#[test]
fn builtin_skill_names_are_canonical_bare_slugs() {
    let mut checked = 0usize;
    for (raw_name, raw) in crate::brain::skills::BUILTIN_SKILLS_FOR_TEST {
        // A built-in that fails to parse is skipped by `load_all_skills`
        // (with an error log), so tolerate it here and assert on the rest.
        let Ok(s) = Skill::parse(raw_name, raw, SkillSource::Builtin) else {
            continue;
        };
        assert!(
            !s.name.starts_with('/'),
            "skill name must be the bare slug, got {:?}",
            s.name
        );
        assert_eq!(s.slash_name, format!("/{}", s.name));
        assert_eq!(
            normalize_skill_slug(&s.slash_name),
            s.name,
            "slash_name must normalise back to name for {:?}",
            s.name
        );
        checked += 1;
    }
    assert!(checked > 0, "no built-in skills parsed — test is vacuous");
}

// --- the defect, inverted -------------------------------------------------

/// **The regression that matters.** A manifest entry written in the
/// documented `- <skill-slug>` form must select that skill's body for
/// re-injection. Pre-fix this returned an EMPTY section, because the matcher
/// compared `slash_name` (`/canarya`) against a set holding bare `canarya`.
#[test]
fn documented_manifest_spelling_selects_the_skill_body() {
    let skills = vec![skill("canarya")];

    // Exactly what `parse_context_manifest` yields for a `- canarya` entry.
    let active = set_of(&["canarya"]);

    let section = active_skill_bodies(&active, &skills, &std::collections::HashMap::new());

    assert!(
        section.contains("BODY-OF-canarya"),
        "documented bare-slug spelling must inject the body, got: {section:?}"
    );
    assert!(
        section.contains("--- Active Skill: /canarya ---"),
        "the injected header keeps the display form, got: {section:?}"
    );
}

/// The sigil spelling — what the slash-command path passes — normalises to
/// the same key and therefore selects the same body.
#[test]
fn slash_spelling_normalises_to_the_same_identity() {
    let skills = vec![skill("canarya")];
    let active: HashSet<String> = [normalize_skill_slug("/canarya")].into_iter().collect();

    assert_eq!(active.iter().next().map(String::as_str), Some("canarya"));

    let section = active_skill_bodies(&active, &skills, &std::collections::HashMap::new());
    assert!(
        section.contains("BODY-OF-canarya"),
        "slash spelling must resolve to the same skill, got: {section:?}"
    );
}

/// Both spellings of the same skill collapse to ONE key — the split key space
/// that let a discard of one form leave the other behind.
#[test]
fn both_spellings_collapse_to_one_key() {
    let mut active = HashSet::new();
    active.insert(normalize_skill_slug("canarya"));
    active.insert(normalize_skill_slug("/canarya"));

    assert_eq!(active.len(), 1, "both spellings must key the same entry");
}

/// A skill that is NOT active contributes no body, and an unmatched set
/// yields an empty section (the injection path skips appending).
#[test]
fn inactive_skills_contribute_nothing() {
    let skills = vec![skill("canarya"), skill("miidas")];

    let none = active_skill_bodies(&set_of(&[]), &skills, &std::collections::HashMap::new());
    assert!(none.is_empty(), "empty active set must yield no section");

    let other = active_skill_bodies(
        &set_of(&["miidas"]),
        &skills,
        &std::collections::HashMap::new(),
    );
    assert!(!other.contains("BODY-OF-canarya"));
    assert!(other.contains("BODY-OF-miidas"));
}

#[test]
fn reinject_consumed_auxiliary_files() {
    let mut s = skill("opencrabs-dev");
    s.auxiliary_files = vec![
        crate::brain::skills::AuxiliaryFile {
            name: "editor.md".to_string(),
            body: "EDITOR-PROCEDURE".to_string(),
        },
        crate::brain::skills::AuxiliaryFile {
            name: "fleet-directives.md".to_string(),
            body: "FLEET-LAW".to_string(),
        },
    ];
    let skills = vec![s];
    let active = set_of(&["opencrabs-dev"]);

    let mut seen_aux = std::collections::HashMap::new();
    seen_aux.insert("opencrabs-dev".to_string(), vec!["editor.md".to_string()]);

    let section = active_skill_bodies(&active, &skills, &seen_aux);
    assert!(section.contains("--- Active Skill: /opencrabs-dev ---"));
    assert!(section.contains("--- Active Auxiliary: editor.md ---\nEDITOR-PROCEDURE"));
    assert!(
        !section.contains("fleet-directives.md"),
        "unconsumed aux must NOT be injected"
    );
}

// --- #947: the injection pass must REPORT what it rendered ----------------
//
// The render leg had no observable of its own: the only signal was a
// whole-brain `System brain present: N chars`, emitted by one provider module
// and blind on every other. That produced a false FAIL on #947 — "no prompt
// growth of the body's size" was read as "the body was never injected". These
// tests pin the replacement: the pass reports its own coverage, so a later
// verdict can read it instead of inferring it from a total.

/// The happy path: a resolved slug is named in `injected`, counted once, and
/// the reported size is the size of the section actually rendered.
#[test]
fn injection_reports_the_slug_it_rendered_and_the_section_size() {
    let skills = vec![skill("canarya")];
    let out = active_skill_injection(
        &set_of(&["canarya"]),
        &skills,
        &std::collections::HashMap::new(),
    );

    assert_eq!(out.injected, vec!["canarya".to_string()]);
    assert!(
        out.unresolved.is_empty(),
        "a resolved slug must not be reported as unresolved: {:?}",
        out.unresolved
    );
    assert!(out.section.contains("BODY-OF-canarya"));
    assert!(
        out.section.len() > "BODY-OF-canarya".len(),
        "the rendered section carries its header too, not just the body: {:?}",
        out.section
    );
    let wrapper = active_skill_bodies(
        &set_of(&["canarya"]),
        &skills,
        &std::collections::HashMap::new(),
    );
    assert_eq!(
        out.section, wrapper,
        "the wrapper must return exactly the rendered section"
    );
}

/// The phantom class (#856) seen from the injection pass: an ACTIVE slug that
/// resolves to no skill is reported by NAME, and contributes no bytes.
#[test]
fn a_phantom_active_slug_is_reported_as_unresolved() {
    let skills = vec![skill("canarya")];
    let out = active_skill_injection(
        &set_of(&["canarya", "ghost"]),
        &skills,
        &std::collections::HashMap::new(),
    );

    assert_eq!(out.injected, vec!["canarya".to_string()]);
    assert_eq!(out.unresolved, vec!["ghost".to_string()]);
    assert!(
        !out.section.contains("ghost"),
        "a slug with no body must put nothing in the section: {:?}",
        out.section
    );
}

/// **The case that made the false verdict possible.** When NOTHING resolves,
/// the section is empty and the caller appends nothing — the old code emitted
/// no evidence at all, so "empty" and "never ran" were indistinguishable. The
/// pass still names the slugs it could not render.
#[test]
fn an_all_phantom_active_set_reports_every_slug_and_renders_nothing() {
    let skills = vec![skill("canarya")];
    let out = active_skill_injection(
        &set_of(&["ghost-a", "ghost-b"]),
        &skills,
        &std::collections::HashMap::new(),
    );

    assert!(out.section.is_empty(), "nothing resolved, nothing rendered");
    assert!(out.injected.is_empty());
    assert_eq!(
        out.unresolved,
        vec!["ghost-a".to_string(), "ghost-b".to_string()],
        "unresolved is sorted, so the log line is stable across runs"
    );
}

/// Auxiliary files are part of the ONE skill that carries them — they must not
/// inflate `injected`, or a count in the log would overstate coverage.
#[test]
fn auxiliary_files_do_not_add_phantom_injected_entries() {
    let mut s = skill("opencrabs-dev");
    s.auxiliary_files = vec![crate::brain::skills::AuxiliaryFile {
        name: "editor.md".to_string(),
        body: "EDITOR-PROCEDURE".to_string(),
    }];
    let mut seen_aux = std::collections::HashMap::new();
    seen_aux.insert("opencrabs-dev".to_string(), vec!["editor.md".to_string()]);

    let out = active_skill_injection(&set_of(&["opencrabs-dev"]), &[s], &seen_aux);

    assert_eq!(
        out.injected,
        vec!["opencrabs-dev".to_string()],
        "one skill is one injected entry regardless of how many aux files it carries"
    );
    assert!(out.section.contains("EDITOR-PROCEDURE"));
}
