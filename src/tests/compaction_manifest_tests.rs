use crate::brain::agent::service::AgentService;
use crate::brain::agent::service::context::{parse_context_manifest, resolve_context_manifest};
use std::collections::HashSet;
use uuid::Uuid;

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
        &[],
        0,
    );
    // The table must render the canonical bare slug. A slashed cell here would
    // contradict the `<skill-slug>` rule documented in the very same prompt —
    // that contradiction is #179 F2, so pin both directions.
    assert!(table.contains("| `cost-estimate` | Skill |"));
    assert!(!table.contains("| `/cost-estimate` | Skill |"));
    assert!(table.contains("| `telegram_send` | Lazy Tool |"));
    assert!(table.contains("Guidance: aim to keep active skills and lazy tools <= 5% target"));
}

/// #216 listing property, made hermetic by #856: the parent is a BUILT-IN
/// (`cost-estimate`) so the rows exist on any machine. The old fixture used
/// `opencrabs-dev`, which resolves only where that skill happens to be
/// installed — and it passed in CI solely because the unresolved row was billed
/// a fabricated 100 tokens. A test that only goes green because of the bug it
/// sits next to is not a control.
#[test]
fn test_format_context_inventory_with_auxiliary() {
    let mut active_skills = HashSet::new();
    active_skills.insert("cost-estimate".to_string());

    let mut seen_aux = std::collections::HashMap::new();
    seen_aux.insert("cost-estimate".to_string(), vec!["notes.md".to_string()]);

    let active_tools = HashSet::new();

    let table = AgentService::format_context_inventory(
        200_000,
        &active_skills,
        &seen_aux,
        &active_tools,
        None,
        &[],
        0,
    );
    assert!(
        table.contains("| `cost-estimate` | Skill |"),
        "got: {table}"
    );
    // The aux row is LISTED — that is #216 …
    assert!(
        table.contains("| `cost-estimate/notes.md` | Skill aux |"),
        "aux row missing, got: {table}"
    );
    // … and because a built-in carries no auxiliary files, it is honestly
    // unweighable rather than billed 100 (#856).
    assert!(
        table.contains("| `cost-estimate/notes.md` | Skill aux | — | — |"),
        "aux row must be unweighable, not fabricated, got: {table}"
    );
}

/// #856, the defect itself: an unresolved slug must not enter the table as a
/// plausible measured row, and must not move the footer total. The footer is the
/// model's budget signal — 100 invented tokens per ghost inflated it by 3,300
/// tokens across 2026-11-15's manifests.
#[test]
fn test_phantom_skill_is_listed_but_not_billed() {
    let mut real_only = HashSet::new();
    real_only.insert("cost-estimate".to_string());

    let mut with_phantom = real_only.clone();
    with_phantom.insert("coding".to_string());

    let empty_tools = HashSet::new();
    let render = |skills: &HashSet<String>| {
        AgentService::format_context_inventory(
            200_000,
            skills,
            &std::collections::HashMap::new(),
            &empty_tools,
            None,
            &[],
            0,
        )
    };

    let baseline = render(&real_only);
    let phantom = render(&with_phantom);

    // The built-in resolves, so the baseline row is a measured one.
    assert!(
        !baseline.contains("`cost-estimate (unknown skill)`"),
        "cost-estimate is a built-in and must resolve, got: {baseline}"
    );
    // The ghost is visible, marked, and unweighed.
    assert!(
        phantom.contains("| `coding (unknown skill)` | Skill | — | — |"),
        "phantom row must be marked and unweighed, got: {phantom}"
    );
    // …and it costs nothing: the footer total is identical to the run that
    // never mentioned it.
    let total_of = |t: &str| {
        t.lines()
            .find(|l| l.starts_with("Total active skills & tools:"))
            .map(str::to_string)
    };
    assert!(total_of(&baseline).is_some(), "footer total line missing");
    assert_eq!(
        total_of(&baseline),
        total_of(&phantom),
        "a phantom slug must not change the billed total"
    );
}

/// #856, the tool half: a registry MISS used to be billed a fabricated 150.
#[test]
fn test_phantom_tool_is_not_billed() {
    let registry = std::sync::Arc::new(crate::brain::tools::ToolRegistry::new());
    let mut tools = HashSet::new();
    tools.insert("definitely_not_a_tool".to_string());

    let table = AgentService::format_context_inventory(
        200_000,
        &HashSet::new(),
        &std::collections::HashMap::new(),
        &tools,
        Some(&registry),
        &[],
        0,
    );
    assert!(
        table.contains("| `definitely_not_a_tool (unknown tool)` | Lazy Tool | — | — |"),
        "phantom tool must be marked and unweighed, got: {table}"
    );
    assert!(
        table.contains("Total active skills & tools: 0 tokens"),
        "a phantom tool must not be billed, got: {table}"
    );
}

/// #831: documents a session read whole are LISTED for the summariser, newest
/// first, between the skill rows and the lazy-tool rows. Nothing is injected
/// here — the list is what the summariser chooses from.
#[test]
fn test_format_context_inventory_with_documents() {
    use crate::brain::tools::retained_docs::{DocEntry, DocStatus};

    let active_tools = HashSet::new();
    let documents = vec![
        DocEntry {
            path: std::path::PathBuf::from("/root/proj/CLAUDE.md"),
            tokens: 1_200,
            lines: Some(412),
            bytes: Some(48_000),
            status: DocStatus::Ok,
        },
        DocEntry {
            path: std::path::PathBuf::from("/root/proj/huge.md"),
            tokens: 0,
            lines: None,
            bytes: Some(300 * 1024),
            status: DocStatus::TooLarge,
        },
        DocEntry {
            path: std::path::PathBuf::from("/root/proj/gone.md"),
            tokens: 0,
            lines: None,
            bytes: None,
            status: DocStatus::Unreadable,
        },
    ];

    let table = AgentService::format_context_inventory(
        200_000,
        &HashSet::new(),
        &std::collections::HashMap::new(),
        &active_tools,
        None,
        &documents,
        3,
    );

    // Measured row, with its percentage of the window.
    assert!(
        table.contains("| `/root/proj/CLAUDE.md` | Document | 1200 | 0.6% |"),
        "measured document row missing, got: {table}"
    );
    // A file too large to weigh is listed with its size and no percentage.
    assert!(
        table.contains("| `/root/proj/huge.md` | Document | (skipped: 300 KiB) | — |"),
        "skipped row missing, got: {table}"
    );
    // An unreadable file is a row, never a failed compaction.
    assert!(
        table.contains("| `/root/proj/gone.md` | Document | (unreadable) | — |"),
        "unreadable row missing, got: {table}"
    );
    // Omission is visible, not silent.
    assert!(
        table.contains("| (+3 more documents read, older) | | | |"),
        "omission row missing, got: {table}"
    );
    // Newest-first order is the order the caller hands us.
    let claude = table.find("/root/proj/CLAUDE.md").unwrap();
    let huge = table.find("/root/proj/huge.md").unwrap();
    let gone = table.find("/root/proj/gone.md").unwrap();
    assert!(claude < huge && huge < gone, "document rows out of order");
    // The budget sentence appears only when there is something to budget for.
    assert!(
        table.contains(
            "Documents re-injected on wake take the retained-set leftover, with a floor of 10% of the context window."
        )
    );
}

/// Acceptance #3 of #831: with no documents read, the inventory is exactly what
/// it was before this feature — no extra row, no extra sentence.
#[test]
fn test_format_context_inventory_without_documents_is_unchanged() {
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
        &[],
        0,
    );

    assert!(!table.contains("| Document |"), "no document rows expected");
    assert!(
        !table.contains("more documents read"),
        "no omission row expected"
    );
    assert!(!table.contains("Documents re-injected on wake"));
    assert!(table.contains("| `cost-estimate` | Skill |"));
    assert!(table.contains("| `telegram_send` | Lazy Tool |"));
    assert!(table.ends_with(
        "Guidance: aim to keep active skills and lazy tools <= 5% target (~10000 tokens).\n"
    ));
}

// ---------------------------------------------------------------------
// Issue #499 — the continuation document's section 0 carries an explicit
// obligation status on its FIRST line, so a compaction can never present
// a completed obligation as the live task.
// ---------------------------------------------------------------------

/// The section-0 spec is prose handed to the summariser model — there is
/// no Rust generator to call, so the production artifact IS the spec text.
/// Since #929 that text lives in `prompts/compaction.default.toml` (the
/// compiled-in prompt set), not in `context.rs`. Asserting against a fixture
/// this file builds would be a control arm that cannot fail (it would only
/// restate its own input), so the status requirements are pinned on the real
/// spec, house-style string sentinels: the wording may drift, the requirement
/// may not.
const PROMPT_SPEC: &str = include_str!("../../prompts/compaction.default.toml");

/// Section 0 must demand an explicit status on its FIRST LINE, naming all
/// three tokens. A token the spec never names is a status the summariser
/// cannot write.
#[test]
fn section_0_spec_requires_the_status_on_its_first_line() {
    assert!(
        PROMPT_SPEC.contains("FIRST LINE OF THIS SECTION"),
        "section 0 must place the status on its first line"
    );
    for status in ["OPEN", "DONE", "UNKNOWN"] {
        assert!(
            PROMPT_SPEC.contains(&format!("**Obligation status: {status}**")),
            "the spec must name the {status} token or the summariser cannot emit it"
        );
    }
}

/// The DONE branch must forbid restating the finished obligation as a
/// directive — that re-blessing is the defect. The `CONTINUE THIS TASK`
/// format must therefore be scoped to OPEN, never offered unconditionally.
#[test]
fn done_branch_forbids_restating_the_obligation_as_a_directive() {
    let spec = PROMPT_SPEC;
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
        PROMPT_SPEC.contains("do NOT carry a status forward from an earlier summary"),
        "the status must be re-derived every compaction, never inherited"
    );
    assert!(
        PROMPT_SPEC.contains("completion evidence you record in section 1"),
        "the status must key off section 1's own completion field"
    );
}

/// #886 ask 1: a next step with a SIDE EFFECT must carry its carrier state
/// and the store that decides it. A bare imperative for a send, comment,
/// close or push is what re-executes on resume — so the spec must demand the
/// stamp, and must forbid the bare imperative it replaces.
#[test]
fn side_effecting_next_step_must_carry_its_carrier_state() {
    assert!(
        PROMPT_SPEC.contains("A next step with a SIDE EFFECT"),
        "#886: the spec must name the side-effecting step class explicitly"
    );
    for marker in [
        "state at [snapshot time]",
        "re-executing. If it already landed",
        "state: UNKNOWN — check [store] first",
    ] {
        assert!(
            PROMPT_SPEC.contains(marker),
            "#886: the carrier rule must carry `{marker}` — a step whose carrier \
             state cannot be established is UNKNOWN, never a bare imperative"
        );
    }
    let rule_at = PROMPT_SPEC
        .find("A next step with a SIDE EFFECT")
        .expect("the carrier rule must exist");
    // Bound the slice on a FIND, never a byte offset: the rule carries em
    // dashes, so an arbitrary index can land mid-codepoint and panic.
    let rule_tail = &PROMPT_SPEC[rule_at..];
    let rule_end = rule_tail
        .find("This is the MOST IMPORTANT section")
        .expect("the carrier rule must precede the section-0 closer");
    let rule = &rule_tail[..rule_end];
    assert!(
        rule.contains("never a bare imperative"),
        "the carrier rule must forbid the bare imperative it replaces: {rule}"
    );
}

/// #886 ask 3: a status the summariser cannot evidence is UNKNOWN. Without
/// this the UNKNOWN arm is decorative — the same pass writes the status and
/// the evidence, so they always agree and prove nothing about the world.
#[test]
fn status_without_a_cited_command_is_unknown() {
    assert!(
        PROMPT_SPEC.contains("A status MUST"),
        "#886: the spec must require the status to cite its evidence"
    );
    assert!(
        PROMPT_SPEC.contains("A status with no such citation is UNKNOWN, never"),
        "#886: an unevidenced status must default to UNKNOWN, never OPEN"
    );
}

/// #886 ask 2: section 1 must record a receipt, and a task without one is
/// "unverified" rather than "pending". Without the receipt slot the status
/// rests on nothing but the same pass's own prose.
#[test]
fn section_1_records_a_receipt_and_defaults_to_unverified() {
    assert!(
        PROMPT_SPEC.contains("- RECEIPT:"),
        "#886: section 1 must carry a receipt slot"
    );
    assert!(
        PROMPT_SPEC.contains("and section 0 must say so"),
        "#886: a task with no receipt must not default to pending"
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

// ---------------------------------------------------------------------
// Issue #831 — documents the session read whole ride the continuation
// document as a harness-written block, after the plan-state and sub-agent
// blocks, so a summary that forgot the project law cannot drop it.
// ---------------------------------------------------------------------

/// Register one whole-file `.md` read for a fresh session, the way `ReadTool`
/// does, and hand back the temp dir the file lives in.
fn register_one_document(lines: usize) -> (Uuid, std::path::PathBuf, std::path::PathBuf) {
    let dir = std::env::temp_dir().join(format!("manifest_docs_{}", Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("CLAUDE.md");
    let body: String = (0..lines).map(|i| format!("line {i}\n")).collect();
    std::fs::write(&path, body).unwrap();
    let sid = Uuid::new_v4();
    crate::brain::tools::read_state::mark_document_read(sid, &path);
    (sid, path, dir)
}

#[tokio::test]
async fn documents_block_lists_paths_and_line_counts() {
    let (sid, path, dir) = register_one_document(7);
    let summary = "## 0. IMMEDIATE TASK\nContinue.\n".to_string();

    let out = {
        let opts = crate::brain::agent::service::context::CompactionDecorateOpts {
            session_id: sid,
            harness: Default::default(),
            tail_messages: None,
        };
        AgentService::decorate_compaction_summary(summary.clone(), &opts).await
    };

    assert!(
        out.contains("## Documents read pre-compaction"),
        "got: {out}"
    );
    assert!(
        out.contains("Harness-written: `.md` files this session read whole"),
        "got: {out}"
    );
    assert!(
        out.contains(&format!(
            "- {} (7 lines)",
            path.canonicalize().unwrap().display()
        )),
        "path + line count missing, got: {out}"
    );
    // The block is appended, never substituted for the summary.
    assert!(out.starts_with(&summary));

    std::fs::remove_dir_all(&dir).ok();
}

/// Nothing read ⇒ the continuation document is byte-identical to before #831.
#[tokio::test]
async fn documents_block_is_absent_when_nothing_was_read() {
    let sid = Uuid::new_v4();
    let summary = "## 0. IMMEDIATE TASK\nContinue.\n".to_string();

    let out = {
        let opts = crate::brain::agent::service::context::CompactionDecorateOpts {
            session_id: sid,
            harness: Default::default(),
            tail_messages: None,
        };
        AgentService::decorate_compaction_summary(summary.clone(), &opts).await
    };

    assert_eq!(out, summary);
}

/// A degraded summary — one that ignored the section spec and emitted no
/// `context-manifest` fence at all — still carries the block, because the
/// harness writes it after the model's prose, not inside it.
#[tokio::test]
async fn documents_block_survives_a_degraded_summary() {
    let (sid, path, dir) = register_one_document(3);
    let degraded = "I could not follow the format. Here is a prose summary.\n".to_string();
    assert!(!degraded.contains("context-manifest"));

    let out = {
        let opts = crate::brain::agent::service::context::CompactionDecorateOpts {
            session_id: sid,
            harness: Default::default(),
            tail_messages: None,
        };
        AgentService::decorate_compaction_summary(degraded.clone(), &opts).await
    };

    assert!(out.starts_with(&degraded));
    assert!(out.contains("## Documents read pre-compaction"));
    assert!(out.contains(&path.canonicalize().unwrap().display().to_string()));

    std::fs::remove_dir_all(&dir).ok();
}

/// #831: the manifest SPEC is the only thing that tells the summariser the
/// `retained_documents` key exists. It lives inside `compute_compaction_summary`
/// (an LLM-calling fn), so it cannot be reached at runtime — pin it at the
/// source level instead. Deleting the rule or the YAML example reddens this.
#[test]
fn manifest_spec_documents_the_retained_documents_key() {
    // #929: the spec text moved to the compiled-in prompt set. The needle is
    // the TOML's own text, not a Rust escape sequence.
    const SRC: &str = include_str!("../../prompts/compaction.default.toml");

    assert!(
        SRC.contains("`retained_documents`: absolute paths"),
        "the manifest rules must document the retained_documents key"
    );
    assert!(
        SRC.contains("retained_documents:\n- /abs/path/to/CLAUDE.md"),
        "the manifest YAML example must show the retained_documents key"
    );
}
