//! Regression tests for #406 — load-time size warning for oversized skills.
//!
//! **The gap.** Loading an oversized skill silently degrades the session: no
//! signal at load time, and the operator only meets the cost later as reduced
//! focus and more frequent compaction, with nothing connecting the two. The
//! authoring budget was already law (`fleet-directives.md` LOC-delta check,
//! `CODE.md` file limits); what was missing was a RUNTIME warning.
//!
//! **The contract these tests pin.** One check at the single choke point every
//! load path converges on — [`active_skill_bodies`], called by the slash /
//! user-told path, the turn-start path, and the post-compaction manifest path
//! (`compaction.rs` and `tool_loop.rs` both go through it). The warning lives
//! INSIDE the injected section, so it survives compaction the way the
//! review-gate reminder does.
//!
//! **The boundary is exact and it is a LINE count.** 500 lines does not warn;
//! 501 does. The threshold is read from [`SKILL_LINE_WARN_THRESHOLD`] rather
//! than restated here, so the assertion cannot pass against a drifted constant.

use crate::brain::skills::{
    active_skill_bodies, AuxiliaryFile, Skill, SkillSource, REVIEW_GATE_REMINDER,
    SKILL_LINE_WARN_THRESHOLD,
};
use std::collections::{HashMap, HashSet};

/// A body of exactly `n` lines (no trailing newline — `lines()` counts `n`).
fn body_of_lines(n: usize) -> String {
    (1..=n)
        .map(|i| format!("line {i}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Build a real `Skill` through the production parser, so the fixture cannot
/// disagree with the code under test about what a body is.
fn skill_with_body(name: &str, body: &str) -> Skill {
    let raw = format!("---\nname: {name}\ndescription: size-warning test skill\n---\n\n{body}\n");
    Skill::parse(name, &raw, SkillSource::Builtin).expect("synthetic skill should parse")
}

fn active(items: &[&str]) -> HashSet<String> {
    items.iter().map(|s| s.to_string()).collect()
}

fn inject(skills: &[Skill], seen_aux: &HashMap<String, Vec<String>>) -> String {
    active_skill_bodies(&active(&["canarya"]), skills, seen_aux)
}

/// The fixture is only meaningful if its line count is what it claims — guard
/// the generator itself, or every boundary assertion below is vacuous.
#[test]
fn fixture_line_count_is_exact() {
    assert_eq!(body_of_lines(500).lines().count(), 500);
    assert_eq!(body_of_lines(501).lines().count(), 501);
}

/// Exactly at the threshold: silent. This is the off-by-one guard.
#[test]
fn at_the_threshold_does_not_warn() {
    let skills = vec![skill_with_body("canarya", &body_of_lines(SKILL_LINE_WARN_THRESHOLD))];

    let section = inject(&skills, &HashMap::new());

    assert!(
        section.contains("--- Active Skill: /canarya ---"),
        "the skill must still be injected at the boundary, got: {section:?}"
    );
    assert!(
        !section.contains("SKILL SIZE WARNING"),
        "{SKILL_LINE_WARN_THRESHOLD} lines is within budget and must not warn, got: {section:?}"
    );
}

/// The measurement is the skill's OWN body — a gated skill's `prompt_body()`
/// prepends the review-gate reminder, so measuring the injected string would
/// warn on a file that is genuinely within budget. The count an operator reads
/// must match the file they would actually split.
#[test]
fn gated_skill_measures_its_own_body_not_the_reminder() {
    let raw = format!(
        "---\nname: canarya\ndescription: gated skill\nreview_gate: true\n---\n\n{}\n",
        body_of_lines(SKILL_LINE_WARN_THRESHOLD)
    );
    let skills = vec![Skill::parse("canarya", &raw, SkillSource::Builtin).expect("gated skill")];

    let section = inject(&skills, &HashMap::new());

    assert!(
        section.contains(REVIEW_GATE_REMINDER),
        "the gated skill must still carry its reminder, got: {section:?}"
    );
    assert!(
        !section.contains("SKILL SIZE WARNING"),
        "a gated skill at exactly the threshold must not warn — the reminder is \
         not part of the skill's body, got: {section:?}"
    );
}

/// One line over: warns, naming the skill, the actual count, and the threshold.
#[test]
fn one_line_over_the_threshold_warns_and_names_the_count() {
    let skills = vec![skill_with_body("canarya", &body_of_lines(SKILL_LINE_WARN_THRESHOLD + 1))];

    let section = inject(&skills, &HashMap::new());

    assert!(
        section.contains("SKILL SIZE WARNING"),
        "one line over budget must warn, got: {section:?}"
    );
    assert!(
        section.contains("is 501 lines"),
        "the warning must name the ACTUAL line count, got: {section:?}"
    );
    assert!(
        section.contains("threshold 500"),
        "the warning must name the threshold, got: {section:?}"
    );
    assert!(
        section.contains("/canarya"),
        "the warning must name the skill, got: {section:?}"
    );
    assert!(
        section.contains("line 501"),
        "the oversized body must still be injected in full — warn, never gate, got: {section:?}"
    );
}

/// The auxiliary arm: a main body within budget must not mask an oversized
/// aux, and the aux warning uses the same terms.
#[test]
fn over_threshold_auxiliary_warns_on_the_same_terms() {
    let mut skill = skill_with_body("canarya", &body_of_lines(10));
    skill.auxiliary_files = vec![AuxiliaryFile {
        name: "fleet-directives.md".to_string(),
        body: body_of_lines(SKILL_LINE_WARN_THRESHOLD + 1),
    }];

    let mut seen_aux = HashMap::new();
    seen_aux.insert("canarya".to_string(), vec!["fleet-directives.md".to_string()]);

    let section = inject(&[skill], &seen_aux);

    assert!(
        section.contains("--- Active Auxiliary: fleet-directives.md ---"),
        "the aux must be reinjected, got: {section:?}"
    );
    assert!(
        section.contains("fleet-directives.md is 501 lines"),
        "an oversized aux must warn on the same terms as a main body, got: {section:?}"
    );
    assert!(
        !section.contains("/canarya is"),
        "the within-budget main body must stay silent, got: {section:?}"
    );
}

/// An auxiliary within budget stays silent — the warning must not fire on size
/// it did not measure.
#[test]
fn auxiliary_within_the_threshold_stays_silent() {
    let mut skill = skill_with_body("canarya", &body_of_lines(10));
    skill.auxiliary_files = vec![AuxiliaryFile {
        name: "editor.md".to_string(),
        body: body_of_lines(SKILL_LINE_WARN_THRESHOLD),
    }];

    let mut seen_aux = HashMap::new();
    seen_aux.insert("canarya".to_string(), vec!["editor.md".to_string()]);

    let section = inject(&[skill], &seen_aux);

    assert!(section.contains("--- Active Auxiliary: editor.md ---"));
    assert!(
        !section.contains("SKILL SIZE WARNING"),
        "a {SKILL_LINE_WARN_THRESHOLD}-line aux is within budget, got: {section:?}"
    );
}

/// The POST-COMPACTION path: the manifest carries the bare slug and the body is
/// re-injected on the next turn. That injection is this exact call, so a
/// warning present here is a warning present after compaction.
#[test]
fn warning_survives_the_compaction_manifest_injection_path() {
    let skills = vec![skill_with_body("canarya", &body_of_lines(SKILL_LINE_WARN_THRESHOLD + 1))];
    // What `parse_context_manifest` yields for a documented `- canarya` entry,
    // plus the aux list the compaction path hands over.
    let manifest_active: HashSet<String> = ["canarya".to_string()].into_iter().collect();
    let mut seen_aux = HashMap::new();
    seen_aux.insert("canarya".to_string(), vec!["editor.md".to_string()]);

    let section = active_skill_bodies(&manifest_active, &skills, &seen_aux);

    assert!(
        section.contains("SKILL SIZE WARNING"),
        "the warning must be re-injected with the body after compaction, got: {section:?}"
    );
    assert!(
        section.contains("--- Active Auxiliary: editor.md ---"),
        "the compaction path also carries aux files, got: {section:?}"
    );
}

/// Ordering: the review-gate reminder is a hard behavioural brake and must keep
/// the first line of the injected section. The size warning is appended.
#[test]
fn review_gate_reminder_keeps_the_first_line() {
    let raw = format!(
        "---\nname: canarya\ndescription: gated skill\nreview_gate: true\n---\n\n{}\n",
        body_of_lines(SKILL_LINE_WARN_THRESHOLD + 1)
    );
    let skills = vec![Skill::parse("canarya", &raw, SkillSource::Builtin).expect("gated skill")];

    let section = inject(&skills, &HashMap::new());

    let reminder_at = section
        .find(REVIEW_GATE_REMINDER)
        .expect("review gate reminder must be present");
    let warning_at = section
        .find("SKILL SIZE WARNING")
        .expect("oversized gated skill must still warn");
    assert!(
        reminder_at < warning_at,
        "the review-gate reminder must precede the size warning, got: {section:?}"
    );
    let header = "--- Active Skill: /canarya ---\n";
    assert_eq!(
        reminder_at,
        section.find(header).expect("header must be present") + header.len(),
        "the reminder must still be the first thing after the header"
    );
}
