use crate::brain::agent::service::AgentService;
use crate::brain::agent::service::context::{parse_context_manifest, resolve_context_manifest};
use std::collections::HashSet;

#[test]
fn test_parse_context_manifest_yaml() {
    let summary = r#"
## 0. IMMEDIATE TASK
CONTINUE THIS TASK: Do something.

## 10. Context Manifest
```context-manifest
active_skills:
  - opencrabs-dev
  - coding-process
discard_skills:
  - grill-for-unknowns
required_tools:
  - telegram_send
  - browser_navigate
```
"#;

    let manifest = parse_context_manifest(summary).expect("should parse manifest");
    assert_eq!(
        manifest.active_skills,
        vec!["opencrabs-dev".to_string(), "coding-process".to_string()]
    );
    assert_eq!(
        manifest.discard_skills,
        vec!["grill-for-unknowns".to_string()]
    );
    assert_eq!(
        manifest.required_tools,
        vec!["telegram_send".to_string(), "browser_navigate".to_string()]
    );
}

#[test]
fn test_parse_context_manifest_json_fallback() {
    let summary = r#"
## 10. Context Manifest
```context-manifest
{
  "active_skills": ["meta-factory"],
  "discard_skills": ["a2a-gateway"],
  "required_tools": ["cron_manage"]
}
```
"#;

    let manifest = parse_context_manifest(summary).expect("should parse json manifest");
    assert_eq!(manifest.active_skills, vec!["meta-factory".to_string()]);
    assert_eq!(manifest.discard_skills, vec!["a2a-gateway".to_string()]);
    assert_eq!(manifest.required_tools, vec!["cron_manage".to_string()]);
}

#[test]
fn test_parse_context_manifest_inline_flow_yaml() {
    let summary = r#"
```context-manifest
active_skills: [opencrabs-dev, github]
discard_skills: []
required_tools: [telegram_send]
```
"#;

    let manifest = parse_context_manifest(summary).expect("should parse inline yaml");
    assert_eq!(
        manifest.active_skills,
        vec!["opencrabs-dev".to_string(), "github".to_string()]
    );
    assert!(manifest.discard_skills.is_empty());
    assert_eq!(manifest.required_tools, vec!["telegram_send".to_string()]);
}

#[test]
fn test_parse_context_manifest_missing_or_malformed_fails_soft() {
    let empty_summary = "No manifest here";
    assert_eq!(parse_context_manifest(empty_summary), None);

    let empty_block = "```context-manifest\n```";
    assert_eq!(parse_context_manifest(empty_block), None);

    let malformed_block = "```context-manifest\nrandom gibberish with no colons\n```";
    assert_eq!(parse_context_manifest(malformed_block), None);
}

#[test]
fn test_parse_skill_spec() {
    assert_eq!(
        crate::brain::skills::parse_skill_spec("opencrabs-dev"),
        ("opencrabs-dev".to_string(), None)
    );
    assert_eq!(
        crate::brain::skills::parse_skill_spec("/opencrabs-dev"),
        ("opencrabs-dev".to_string(), None)
    );
    assert_eq!(
        crate::brain::skills::parse_skill_spec("opencrabs-dev/editor.md"),
        ("opencrabs-dev".to_string(), Some("editor.md".to_string()))
    );
    assert_eq!(
        crate::brain::skills::parse_skill_spec("/opencrabs-dev/fleet-directives.md"),
        (
            "opencrabs-dev".to_string(),
            Some("fleet-directives.md".to_string())
        )
    );
}

#[test]
fn test_parse_context_manifest_with_aux_files_and_service_wiring() {
    let summary = r#"
## 10. Context Manifest
```context-manifest
active_skills:
  - opencrabs-dev/editor.md
  - /opencrabs-dev/fleet-directives.md
  - miidas-factory
discard_skills:
  - opencrabs-dev/hq.md
  - /a2a-gateway
required_tools:
  - telegram_send
```
"#;

    let manifest = parse_context_manifest(summary).expect("should parse manifest with aux paths");
    assert_eq!(
        manifest.active_skills,
        vec![
            "opencrabs-dev/editor.md".to_string(),
            "/opencrabs-dev/fleet-directives.md".to_string(),
            "miidas-factory".to_string(),
        ]
    );
    assert_eq!(
        manifest.discard_skills,
        vec![
            "opencrabs-dev/hq.md".to_string(),
            "/a2a-gateway".to_string(),
        ]
    );

    let session_id = uuid::Uuid::new_v4();

    for active in &manifest.active_skills {
        let (slug, aux_file) = crate::brain::skills::parse_skill_spec(active);
        crate::brain::tools::seen_skills::mark_seen(session_id, &slug);
        crate::brain::tools::seen_skills::mark_active(session_id, &slug);
        if let Some(file) = aux_file {
            crate::brain::tools::seen_skills::mark_aux_seen(session_id, &slug, &file);
        }
    }

    let active = crate::brain::tools::seen_skills::active_for_session(session_id);
    assert!(active.contains("opencrabs-dev"));
    assert!(active.contains("miidas-factory"));

    let aux = crate::brain::tools::seen_skills::aux_seen_for_session(session_id);
    let dev_aux = aux.get("opencrabs-dev").expect("opencrabs-dev aux files");
    assert_eq!(
        dev_aux,
        &vec!["editor.md".to_string(), "fleet-directives.md".to_string()]
    );

    // Discard single aux file
    let (slug, aux_file) = crate::brain::skills::parse_skill_spec("opencrabs-dev/editor.md");
    if let Some(file) = aux_file {
        crate::brain::tools::seen_skills::unmark_aux_seen(session_id, &slug, &file);
    }
    let aux_after = crate::brain::tools::seen_skills::aux_seen_for_session(session_id);
    let dev_aux_after = aux_after
        .get("opencrabs-dev")
        .expect("opencrabs-dev aux files");
    assert_eq!(dev_aux_after, &vec!["fleet-directives.md".to_string()]);
    // Skill itself remains active
    assert!(
        crate::brain::tools::seen_skills::active_for_session(session_id).contains("opencrabs-dev")
    );

    // Discard entire skill
    let (slug, aux_file) = crate::brain::skills::parse_skill_spec("opencrabs-dev");
    if aux_file.is_none() {
        crate::brain::tools::seen_skills::clear_aux_seen(session_id, &slug);
        crate::brain::tools::seen_skills::unmark_seen(session_id, &slug);
        crate::brain::tools::seen_skills::unmark_active(session_id, &slug);
    }
    assert!(
        !crate::brain::tools::seen_skills::active_for_session(session_id).contains("opencrabs-dev")
    );
    let aux_final = crate::brain::tools::seen_skills::aux_seen_for_session(session_id);
    assert!(!aux_final.contains_key("opencrabs-dev"));
}

#[test]
fn test_format_context_inventory() {
    let mut active_skills = HashSet::new();
    active_skills.insert("cost-estimate".to_string());

    let mut active_tools = HashSet::new();
    active_tools.insert("telegram_send".to_string());

    let table = AgentService::format_context_inventory(
        200_000,
        &active_skills,
        &std::collections::HashMap::new(),
        &active_tools,
        None,
    );
    // The table must render the canonical bare slug. A slashed cell here would
    // contradict the `<skill-slug>` rule documented in the very same prompt —
    // that contradiction is #179 F2, so pin both directions.
    assert!(table.contains("| `cost-estimate` | Skill |"));
    assert!(!table.contains("| `/cost-estimate` | Skill |"));
    assert!(table.contains("| `telegram_send` | Lazy Tool |"));
    assert!(table.contains("Guidance: aim to keep active skills and lazy tools <= 5% target"));
}

#[test]
fn test_format_context_inventory_with_auxiliary() {
    let mut active_skills = HashSet::new();
    active_skills.insert("opencrabs-dev".to_string());

    let mut seen_aux = std::collections::HashMap::new();
    seen_aux.insert("opencrabs-dev".to_string(), vec!["editor.md".to_string()]);

    let active_tools = HashSet::new();

    let table = AgentService::format_context_inventory(
        200_000,
        &active_skills,
        &seen_aux,
        &active_tools,
        None,
    );
    assert!(table.contains("| `opencrabs-dev` | Skill |"));
    assert!(table.contains("| `opencrabs-dev/editor.md` | Skill aux |"));
}

// ---------------------------------------------------------------------
// Issue #499 — the continuation document's section 0 carries an explicit
// obligation status on its FIRST line, so a compaction can never present
// a completed obligation as the live task.
// ---------------------------------------------------------------------

/// The section-0 spec is prose handed to the summariser model — there is
/// no Rust generator to call, so the production artifact IS the spec text
/// in `context.rs`. Asserting against a fixture this file builds would be
/// a control arm that cannot fail (it would only restate its own input),
/// so the status requirements are pinned on the real spec, house-style
/// string sentinels: the wording may drift, the requirement may not.
const CONTEXT_SRC: &str = include_str!("../brain/agent/service/context.rs");

/// Section 0 must demand an explicit status on its FIRST LINE, naming all
/// three tokens. A token the spec never names is a status the summariser
/// cannot write.
#[test]
fn section_0_spec_requires_the_status_on_its_first_line() {
    assert!(
        CONTEXT_SRC.contains("FIRST LINE OF THIS SECTION"),
        "section 0 must place the status on its first line"
    );
    for status in ["OPEN", "DONE", "UNKNOWN"] {
        assert!(
            CONTEXT_SRC.contains(&format!("**Obligation status: {status}**")),
            "the spec must name the {status} token or the summariser cannot emit it"
        );
    }
}

/// The DONE branch must forbid restating the finished obligation as a
/// directive — that re-blessing is the defect. The `CONTINUE THIS TASK`
/// format must therefore be scoped to OPEN, never offered unconditionally.
#[test]
fn done_branch_forbids_restating_the_obligation_as_a_directive() {
    let spec = CONTEXT_SRC;
    let open_at = spec.find("- OPEN ").expect("the OPEN branch must exist");
    let done_at = spec.find("- DONE ").expect("the DONE branch must exist");
    let unknown_at = spec
        .find("- UNKNOWN ")
        .expect("the UNKNOWN branch must exist");
    assert!(
        open_at < done_at && done_at < unknown_at,
        "the three status branches must appear in OPEN, DONE, UNKNOWN order"
    );
    let open_arm = &spec[open_at..done_at];
    let done_arm = &spec[done_at..unknown_at];

    // The DONE branch must forbid the re-blessing this issue is about.
    assert!(
        done_arm.contains("Do NOT restate the completed obligation as a directive"),
        "the DONE branch must forbid the re-blessing this issue is about: {done_arm}"
    );
    // The defect in one assertion: a DONE obligation must NOT carry the
    // continue directive, and must not be told to write one. Slice the arm
    // on its own branch markers: the source separates branches with a
    // line-continuation, so a bare newline search would return the whole
    // file and the assertion would test nothing.
    assert!(
        !done_arm.contains("CONTINUE THIS TASK"),
        "a DONE obligation must NOT carry the continue directive: {done_arm}"
    );
    assert!(
        !done_arm.contains("Write the DIRECTIVE"),
        "a DONE obligation must NOT be told to write a directive: {done_arm}"
    );
    // ...and the directive is scoped to OPEN, the one branch still owed.
    assert!(
        open_arm.contains("Write the DIRECTIVE exactly as specified below"),
        "the directive must be scoped to the OPEN branch: {open_arm}"
    );
}

/// A carried-forward status is the defect wearing a new coat: the summary
/// would still be blessing yesterday's obligation. The spec must demand the
/// status be derived from section 1's completion evidence, not inherited.
#[test]
fn status_must_be_derived_not_carried_forward() {
    assert!(
        CONTEXT_SRC.contains("do NOT carry a status forward from an earlier summary"),
        "the status must be re-derived every compaction, never inherited"
    );
    assert!(
        CONTEXT_SRC.contains("completion evidence you record in section 1"),
        "the status must key off section 1's own completion field"
    );
}

/// Behavioral half: a section 0 that carries the status line must not break
/// manifest parsing — the status is inserted ahead of the fenced block, so a
/// parser that keys on section offsets would silently lose the manifest.
#[test]
fn a_status_bearing_section_0_still_parses_its_manifest() {
    for status in ["OPEN", "DONE", "UNKNOWN"] {
        let doc = format!(
            "## 0. IMMEDIATE TASK (CRITICAL — MOST IMPORTANT SECTION)\n\
             **Obligation status: {status}**\n\
             body text\n\n\
             ## 10. Context Manifest\n\
             ```context-manifest\n\
             active_skills:\n  - opencrabs-dev\n\
             ```\n"
        );
        let m = parse_context_manifest(&doc)
            .unwrap_or_else(|| panic!("status={status}: the status line broke manifest parsing"));
        assert!(
            m.active_skills.iter().any(|s| s.contains("opencrabs-dev")),
            "status={status}: manifest content must survive intact"
        );
    }
}

// ---------------------------------------------------------------------
// Issue #1933 — a document that carries no `context-manifest` fence must
// resolve to the pruning-safe default (never `None`), so the reload path
// can warn about the degradation instead of silently doing nothing.
// ---------------------------------------------------------------------

/// No fence at all → the resolver still yields a manifest, and that manifest
/// is the pruning-safe default: keep every active skill, discard none,
/// activate no lazy tool.
#[test]
fn test_resolve_context_manifest_falls_back_to_the_pruning_safe_default() {
    let doc = "## 0. IMMEDIATE TASK\n**Obligation status: OPEN**\nno fence here\n";
    let manifest = resolve_context_manifest(doc);
    assert!(
        manifest.active_skills.is_empty(),
        "keep-all-active: nothing is discarded or re-activated"
    );
    assert!(
        manifest.discard_skills.is_empty(),
        "keep-all-active: nothing is pruned"
    );
    assert!(
        manifest.required_tools.is_empty(),
        "required_tools is not derivable from a document with no fence"
    );
}

/// A present fence is still parsed verbatim — the default is a fallback,
/// never an override.
#[test]
fn test_resolve_context_manifest_prefers_a_present_fence() {
    let doc = "## 10. Context Manifest\n\
               ```context-manifest\n\
               active_skills: [opencrabs-dev]\n\
               required_tools: [telegram_send]\n\
               ```\n";
    let manifest = resolve_context_manifest(doc);
    assert_eq!(manifest.active_skills, vec!["opencrabs-dev".to_string()]);
    assert_eq!(manifest.required_tools, vec!["telegram_send".to_string()]);
    assert!(manifest.discard_skills.is_empty());
}
