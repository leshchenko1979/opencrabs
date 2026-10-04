//! Tests for the skill glob gate (issue #150): match semantics, exemption,
//! fail-open laws, and pattern resolution.
//!
//! Extracted from an inline `#[cfg(test)]` block in
//! `src/brain/tools/skill_gate.rs`; project policy (CONTRIBUTING.md)
//! requires all tests under `src/tests/`.

use serde_json::json;
use uuid::Uuid;

use crate::brain::agent::service::tool_loop::DEFAULT_MAX_INLINE_TOOL_BYTES;
use crate::brain::skills::{Skill, SkillSource};
use crate::brain::tools::error::expand_tilde;
use crate::brain::tools::seen_skills;
use crate::brain::tools::skill_gate::{
    EXEMPT_TOOLS, GateVerdict, compile_pattern, gate_block_message, harvest_candidates,
};

fn skill(globs: &[&str]) -> Skill {
    let fm = format!(
        "---\nname: guard-skill\ndescription: gated\nglobs: {}\n---\nBODY-MARKER\n",
        globs.join(", ")
    );
    Skill::parse("guard-skill", &fm, SkillSource::Builtin).unwrap()
}

/// Mirror of `skill_gate::check` for tests — kept in step with it by hand,
/// so any new short-circuit in `check` must be added here too.
///
/// `headless` is the #405 leg: a surface with no live user (cron, CLI
/// one-shot, sub-agent) never blocks, because the remedy ("read this, then
/// re-issue") needs an interactive turn.
fn verdict_with_skills_opts(
    session: Uuid,
    tool: &str,
    input: serde_json::Value,
    skills: Vec<Skill>,
    enabled: bool,
    headless: bool,
) -> GateVerdict {
    if !enabled || headless || skills.is_empty() || EXEMPT_TOOLS.contains(&tool) {
        return GateVerdict::Pass;
    }
    let candidates = harvest_candidates(tool, &input, std::path::Path::new("/work"));
    for s in &skills {
        for g in &s.globs {
            let Ok(pattern) = compile_pattern(g, std::path::Path::new("/work")) else {
                continue;
            };
            let options = glob::MatchOptions {
                case_sensitive: false,
                require_literal_separator: true,
                require_literal_leading_dot: false,
            };
            for c in &candidates {
                if pattern.matches_path_with(c, options) {
                    if seen_skills::seen_since_compaction(session, &s.name) {
                        return GateVerdict::Pass;
                    }
                    return GateVerdict::Block {
                        skill: s.name.clone(),
                        matched_path: c.display().to_string(),
                        globs: s.globs.clone(),
                    };
                }
            }
        }
    }
    GateVerdict::Pass
}

/// The interactive (non-headless) surface — the common case.
fn verdict_with_skills(
    session: Uuid,
    tool: &str,
    input: serde_json::Value,
    skills: Vec<Skill>,
    enabled: bool,
) -> GateVerdict {
    verdict_with_skills_opts(session, tool, input, skills, enabled, false)
}

#[test]
fn match_blocks_and_names_the_skill() {
    // #405: the Block verdict still carries the skill slug and its globs —
    // they are what the notice is built from. The body no longer rides it.
    let s = skill(&["**/skills/guard-skill/**"]);
    let v = verdict_with_skills(
        Uuid::new_v4(),
        "edit_file",
        json!({"path": "/root/.opencrabs/skills/guard-skill/SKILL.md", "old_text": "a", "new_text": "b"}),
        vec![s],
        true,
    );
    let GateVerdict::Block {
        skill,
        matched_path,
        globs,
    } = v
    else {
        panic!("expected Block, got {v:?}");
    };
    assert_eq!(skill, "guard-skill");
    assert_eq!(globs, vec!["**/skills/guard-skill/**".to_string()]);
    assert_eq!(matched_path, "/root/.opencrabs/skills/guard-skill/SKILL.md");
}

#[test]
fn headless_surface_passes_where_interactive_blocks() {
    // #405: the same call, the same skills, the same config — only the
    // surface differs. A headless session must not be dead-ended by a
    // refusal whose remedy requires a human turn.
    let session = Uuid::new_v4();
    let input = json!({"path": "/x/guard/file.md", "content": "c"});
    let interactive = verdict_with_skills_opts(
        session,
        "write_file",
        input.clone(),
        vec![skill(&["**/guard/**"])],
        true,
        false,
    );
    let blocked = matches!(interactive, GateVerdict::Block { .. });
    assert!(blocked, "interactive must block, got {interactive:?}");
    let headless = verdict_with_skills_opts(
        Uuid::new_v4(),
        "write_file",
        input,
        vec![skill(&["**/guard/**"])],
        true,
        true,
    );
    assert_eq!(headless, GateVerdict::Pass);
}

#[test]
fn seen_skill_passes() {
    let session = Uuid::new_v4();
    let s = skill(&["**/guard/**"]);
    seen_skills::mark_seen(session, "guard-skill");
    let v = verdict_with_skills(
        session,
        "write_file",
        json!({"path": "/x/guard/file.md", "content": "c"}),
        vec![s],
        true,
    );
    assert_eq!(v, GateVerdict::Pass);
}

#[test]
fn fresh_session_blocks() {
    let s = skill(&["**/guard/**"]);
    let v = verdict_with_skills(
        Uuid::new_v4(),
        "write_file",
        json!({"path": "/x/guard/file.md", "content": "c"}),
        vec![s],
        true,
    );
    assert!(matches!(v, GateVerdict::Block { .. }));
}

#[test]
fn exempt_tools_pass() {
    for tool in EXEMPT_TOOLS {
        let s = skill(&["**/**"]);
        let v = verdict_with_skills(
            Uuid::new_v4(),
            tool,
            json!({"path": "/x/guard/file.md"}),
            vec![s],
            true,
        );
        assert_eq!(v, GateVerdict::Pass, "tool {tool} must be exempt");
    }
}

#[test]
fn bash_command_with_matching_path_blocks() {
    let s = skill(&["**/secrets/**"]);
    let v = verdict_with_skills(
        Uuid::new_v4(),
        "bash",
        json!({"command": "cat /etc/secrets/key.pem"}),
        vec![s],
        true,
    );
    assert!(matches!(v, GateVerdict::Block { .. }));
}

#[test]
fn disabled_config_passes() {
    let s = skill(&["**/guard/**"]);
    let v = verdict_with_skills(
        Uuid::new_v4(),
        "write_file",
        json!({"path": "/x/guard/file.md", "content": "c"}),
        vec![s],
        false,
    );
    assert_eq!(v, GateVerdict::Pass);
}

#[test]
fn malformed_glob_fails_open() {
    let s = skill(&["[invalid"]);
    let v = verdict_with_skills(
        Uuid::new_v4(),
        "write_file",
        json!({"path": "/x/guard/file.md", "content": "c"}),
        vec![s],
        true,
    );
    assert_eq!(v, GateVerdict::Pass);
}

#[test]
fn relative_path_resolves_against_cwd() {
    let s = skill(&["/work/guard/**"]);
    let input = json!({"path": "guard/file.md", "content": "c"});
    let candidates = harvest_candidates("write_file", &input, std::path::Path::new("/work"));
    assert!(candidates.contains(&std::path::PathBuf::from("/work/guard/file.md")));
    let v = verdict_with_skills(Uuid::new_v4(), "write_file", input, vec![s], true);
    assert!(matches!(v, GateVerdict::Block { .. }));
}

#[test]
fn star_matches_one_segment_only() {
    // Cursor table via require_literal_separator: `*` must NOT cross `/`.
    let s = skill(&["/work/*"]);
    let v1 = verdict_with_skills(
        Uuid::new_v4(),
        "write_file",
        json!({"path": "/work/file.md", "content": "c"}),
        vec![s.clone()],
        true,
    );
    assert!(matches!(v1, GateVerdict::Block { .. }));
    let v2 = verdict_with_skills(
        Uuid::new_v4(),
        "write_file",
        json!({"path": "/work/sub/file.md", "content": "c"}),
        vec![s],
        true,
    );
    assert_eq!(v2, GateVerdict::Pass);
}

#[test]
fn grep_pattern_never_harvested() {
    let candidates = harvest_candidates(
        "grep",
        &json!({"pattern": "/etc/secrets/**", "path": "/tmp/x"}),
        std::path::Path::new("/work"),
    );
    assert_eq!(candidates, vec![std::path::PathBuf::from("/tmp/x")]);
}

#[test]
fn tilde_pattern_matches_home_path() {
    // Regression: the frontmatter form a skill author actually writes
    // (`~/repo/**`) must match — it must not compile to a literal `~`.
    let s = skill(&["~/guard/**"]);
    let v = verdict_with_skills(
        Uuid::new_v4(),
        "write_file",
        json!({"path": "~/guard/file.md", "content": "c"}),
        vec![s],
        true,
    );
    let GateVerdict::Block { matched_path, .. } = v else {
        panic!("expected Block for a `~/...` glob, got {v:?}");
    };
    let home = expand_tilde("~");
    assert_eq!(
        matched_path,
        home.join("guard").join("file.md").display().to_string()
    );
}

#[test]
fn relative_pattern_resolves_against_cwd() {
    // A bare relative pattern (`guard/**`) resolves against the tool cwd,
    // mirroring how candidate paths are resolved.
    let s = skill(&["guard/**"]);
    let v = verdict_with_skills(
        Uuid::new_v4(),
        "write_file",
        json!({"path": "guard/file.md", "content": "c"}),
        vec![s],
        true,
    );
    assert!(matches!(v, GateVerdict::Block { .. }));
}

// ---------------------------------------------------------------------------
// #458 — the rejection must not promise a body the harness cap truncates.
// #405 — and it must not CARRY that body either: the notice is bounded.
// ---------------------------------------------------------------------------

/// The regression pin for #405: a synthetically oversized skill (the real
/// ones measured 23–47 KB) must produce a Block message far under the
/// harness cap, and the body's own bytes must not appear in it.
///
/// FALSIFIER (pre-change): `gate_block_message` took the body and appended
/// it after a `\n\n---\n\n` separator, so for this input the returned
/// string was `body.len() + ~600` bytes — the `< 2000` assertion below
/// fails against that code, and the `!msg.contains(BODY-MARKER)` assertion
/// fails with it. Both must keep failing if the append ever returns.
#[test]
fn gate_message_is_bounded_for_an_oversized_skill() {
    // 40 KB body, comfortably past the harness cap — the class that made
    // every trigger cost tens of KB.
    let body = "BODY-MARKER\n".repeat(4_000);
    assert!(body.len() > DEFAULT_MAX_INLINE_TOOL_BYTES);

    let msg = gate_block_message(
        "guard-skill",
        "/work/guard/file.md",
        &["**/guard/**".to_string()],
        Some(std::path::Path::new("/root/.opencrabs/skills/guard-skill/SKILL.md")),
    );

    // The bound. 2000 bytes is ~2 orders of magnitude below the payload the
    // gate used to ship, and still ~8x under the harness cap.
    assert!(
        msg.len() < 2000,
        "Block payload must be bounded, got {} bytes:\n{msg}",
        msg.len()
    );
    assert!(
        !msg.contains("BODY-MARKER"),
        "the skill body must not ride the rejection (#405):\n{msg}"
    );
    // ...while the notice still does its job: slug, globs, and the route.
    assert!(msg.contains("guard-skill"));
    assert!(msg.contains("**/guard/**"));
    assert!(msg.contains("/work/guard/file.md"));
    assert!(msg.contains("load_brain_file 'guard-skill'"));
    assert!(
        msg.contains("/root/.opencrabs/skills/guard-skill/SKILL.md"),
        "the on-disk source path is the durable route:\n{msg}"
    );
    assert!(msg.contains(&DEFAULT_MAX_INLINE_TOOL_BYTES.to_string()));
    assert!(msg.contains("head/tail preview"));
}

#[test]
fn gate_message_names_the_source_path_and_the_reload_route() {
    let msg = gate_block_message(
        "guard-skill",
        "/work/guard/file.md",
        &["**/guard/**".to_string()],
        Some(std::path::Path::new("/root/.opencrabs/skills/guard-skill/SKILL.md")),
    );
    assert!(!msg.contains("The full skill body follows"));
    assert!(!msg.contains("is appended below"));
    assert!(msg.contains("/root/.opencrabs/skills/guard-skill/SKILL.md"));
    assert!(msg.contains("load_brain_file 'guard-skill'"));
    assert!(msg.contains(&DEFAULT_MAX_INLINE_TOOL_BYTES.to_string()));
    assert!(msg.contains("head/tail preview"));
}

#[test]
fn gate_message_builtin_arm_keeps_the_reload_route_without_a_file() {
    // A compiled-in skill has no source file to name, so the message must
    // drop the file route and keep the one that always works.
    let no_globs: &[String] = &[];
    let msg = gate_block_message("guard-skill", "/work/guard/file.md", no_globs, None);
    assert!(msg.contains("load_brain_file 'guard-skill'"));
    assert!(msg.contains("no file on disk to read"));
    assert!(!msg.contains("read_file"));
    assert!(!msg.contains("The full skill body follows"));
    assert!(!msg.contains("is appended below"));
}
