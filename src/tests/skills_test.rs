//! Tests for the skill loader — frontmatter parsing, the built-in sidecar,
//! and user-directory overlay.

use crate::brain::skills::{
    Skill, SkillSource, load_all_skills, load_all_skills_from, resolve_skill,
};

/// The repo's built-in skill source of truth: the tree the release archives and
/// the deploy artifact copy into `skills/` beside the binary.
///
/// Tests read it directly because the built-in bodies are no longer compiled
/// into the executable. Production resolves `<exe_dir>/skills/`; a test binary's
/// exe dir is a cargo target directory, not an install, so pointing the loader
/// at this tree is how a test exercises the same disk-read path an install does.
pub(crate) fn shipped_skills_dir() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/docs/reference/templates/skills")
}

/// Load every skill with the built-in tier pointed at the shipped tree, the way
/// an install resolves it. Overlays still come from the active profile home.
pub(crate) fn load_shipped_skills() -> Vec<Skill> {
    load_all_skills_from(Some(shipped_skills_dir().as_path()))
}

#[test]
fn parses_minimal_frontmatter() {
    let raw = "---\nname: foo\ndescription: A test skill\n---\n\nBody here.\n";
    let skill = Skill::parse("foo", raw, SkillSource::Builtin).unwrap();
    assert_eq!(skill.name, "foo");
    assert_eq!(skill.description, "A test skill");
    assert_eq!(skill.body, "Body here.");
    assert_eq!(skill.source, SkillSource::Builtin);
}

#[test]
fn parses_crlf_line_endings() {
    let raw = "---\r\nname: foo\r\ndescription: windows\r\n---\r\n\r\nBody.\r\n";
    let skill = Skill::parse("foo", raw, SkillSource::Builtin).unwrap();
    assert_eq!(skill.description, "windows");
    assert_eq!(skill.body, "Body.");
}

#[test]
fn tolerates_utf8_bom() {
    let raw = "\u{FEFF}---\nname: foo\ndescription: bom\n---\n\nBody.\n";
    let skill = Skill::parse("foo", raw, SkillSource::Builtin).unwrap();
    assert_eq!(skill.description, "bom");
}

#[test]
fn strips_quotes_around_values() {
    let raw_double = "---\nname: foo\ndescription: \"quoted\"\n---\nBody.\n";
    let skill = Skill::parse("foo", raw_double, SkillSource::Builtin).unwrap();
    assert_eq!(skill.description, "quoted");

    let raw_single = "---\nname: foo\ndescription: 'single'\n---\nBody.\n";
    let skill = Skill::parse("foo", raw_single, SkillSource::Builtin).unwrap();
    assert_eq!(skill.description, "single");
}

#[test]
fn name_falls_back_to_argument_when_frontmatter_lacks_it() {
    // Frontmatter omits `name:` — the directory name is authoritative.
    let raw = "---\ndescription: no name field\n---\nBody.\n";
    let skill = Skill::parse("from-dir", raw, SkillSource::User).unwrap();
    assert_eq!(skill.name, "from-dir");
}

#[test]
fn missing_description_is_an_error() {
    let raw = "---\nname: foo\n---\nBody.\n";
    let err = Skill::parse("foo", raw, SkillSource::Builtin).unwrap_err();
    assert!(err.contains("description"), "got: {err}");
}

#[test]
fn missing_frontmatter_fence_is_an_error() {
    let raw = "Just a body, no fence.";
    let err = Skill::parse("foo", raw, SkillSource::Builtin).unwrap_err();
    assert!(err.contains("frontmatter"), "got: {err}");
}

#[test]
fn unmatched_open_fence_is_an_error() {
    // Opens with --- but never closes — should not silently swallow the body.
    let raw = "---\nname: foo\ndescription: leak\n";
    let err = Skill::parse("foo", raw, SkillSource::Builtin).unwrap_err();
    assert!(err.contains("frontmatter"), "got: {err}");
}

#[test]
fn unknown_keys_are_ignored_for_forward_compat() {
    let raw = "---\nname: foo\ndescription: ok\nmodel: claude-haiku-4-5\nfuture_field: whatever\n---\nBody.\n";
    let skill = Skill::parse("foo", raw, SkillSource::Builtin).unwrap();
    assert_eq!(skill.description, "ok");
}

#[test]
fn body_preserves_internal_blank_lines_and_markdown() {
    let raw =
        "---\nname: foo\ndescription: ok\n---\n\n# Heading\n\nParagraph 1.\n\n- item 1\n- item 2\n";
    let skill = Skill::parse("foo", raw, SkillSource::Builtin).unwrap();
    assert!(skill.body.starts_with("# Heading"));
    assert!(skill.body.contains("- item 1\n- item 2"));
}

#[test]
fn builtin_security_audit_loads_from_the_shipped_sidecar() {
    // The sidecar's security-audit skill must always resolve, from disk.
    let skill = load_shipped_skills()
        .into_iter()
        .find(|s| s.name == "security-audit")
        .expect("built-in 'security-audit' must exist");
    assert_eq!(skill.source, SkillSource::Builtin);
    assert!(
        skill.description.to_lowercase().contains("security"),
        "description should mention security"
    );
    assert!(!skill.body.is_empty());
}

#[test]
fn builtin_cost_estimate_loads_from_the_shipped_sidecar() {
    let skill = load_shipped_skills()
        .into_iter()
        .find(|s| s.name == "cost-estimate")
        .expect("built-in 'cost-estimate' must exist");
    assert_eq!(skill.source, SkillSource::Builtin);
    assert!(skill.description.to_lowercase().contains("cost"));
}

#[test]
fn unknown_skill_returns_none() {
    assert!(resolve_skill("does-not-exist-anywhere").is_none());
}

#[test]
fn parses_review_gate_flag_and_prepends_reminder() {
    // A flagged skill's slash prompt carries the review gate: output first,
    // side effects only after the user's explicit approval, yolo included.
    let raw =
        "---\nname: gated\ndescription: Gated skill\nreview_gate: true\n---\n\nDraft the thing.\n";
    let skill = Skill::parse("gated", raw, SkillSource::User).unwrap();
    assert!(skill.review_gate);
    let prompt = skill.prompt_body();
    assert!(
        prompt.contains("SKILL REVIEW GATE"),
        "reminder missing: {prompt}"
    );
    assert!(
        prompt.contains("Draft the thing."),
        "body must follow the reminder: {prompt}"
    );

    // Accepted truthy spellings.
    for spelling in ["true", "yes", "1", "on", "TRUE"] {
        let raw = format!("---\nname: g\ndescription: d\nreview_gate: {spelling}\n---\n\nBody.\n");
        let skill = Skill::parse("g", &raw, SkillSource::User).unwrap();
        assert!(
            skill.review_gate,
            "spelling '{spelling}' must parse as true"
        );
    }
}

#[test]
fn review_gate_defaults_false_and_prompt_is_untouched() {
    // Unflagged skills dispatch their body verbatim — exactly as before.
    let raw = "---\nname: plain\ndescription: Plain skill\n---\n\nBody.\n";
    let skill = Skill::parse("plain", raw, SkillSource::User).unwrap();
    assert!(!skill.review_gate);
    assert_eq!(skill.prompt_body(), "Body.");

    // False-ish and unknown values stay false.
    for spelling in ["false", "no", "0", "off", "maybe"] {
        let raw = format!("---\nname: p\ndescription: d\nreview_gate: {spelling}\n---\n\nBody.\n");
        let skill = Skill::parse("p", &raw, SkillSource::User).unwrap();
        assert!(
            !skill.review_gate,
            "spelling '{spelling}' must parse as false"
        );
        assert_eq!(skill.prompt_body(), "Body.");
    }
}

#[test]
fn parses_folded_block_scalar_description() {
    let raw = "---\nname: folded-skill\ndescription: >\n  This is line one.\n  This is line two.\n\n  This is paragraph two.\n---\nBody.\n";
    let skill = Skill::parse("folded-skill", raw, SkillSource::Builtin).unwrap();
    assert_eq!(
        skill.description,
        "This is line one. This is line two.\n\nThis is paragraph two."
    );
}

#[test]
fn parses_literal_block_scalar_description() {
    let raw = "---\nname: literal-skill\ndescription: |\n  Line one\n  Line two\n---\nBody.\n";
    let skill = Skill::parse("literal-skill", raw, SkillSource::Builtin).unwrap();
    assert_eq!(skill.description, "Line one\nLine two");
}

#[test]
fn parses_folded_strip_indicator_scalar_description() {
    let raw = "---\nname: folded-strip\ndescription: >-\n  First sentence.\n  Second sentence.\n---\nBody.\n";
    let skill = Skill::parse("folded-strip", raw, SkillSource::Builtin).unwrap();
    assert_eq!(skill.description, "First sentence. Second sentence.");
}

#[test]
fn load_all_includes_every_builtin() {
    let skills = load_shipped_skills();
    let names: Vec<&str> = skills.iter().map(|s| s.name.as_str()).collect();
    assert!(names.contains(&"security-audit"), "names: {:?}", names);
    assert!(names.contains(&"cost-estimate"), "names: {:?}", names);
}

/// Issue #231: a skill placed in a NAMED profile's own skills dir
/// (`~/.opencrabs/profiles/<name>/skills/<skill>/SKILL.md`) is discovered when
/// that profile is active, and is NOT visible from a different profile.
///
/// This pins the actual contract behind the report: profile skills resolve
/// through the profile-aware `opencrabs_home()`, so they load under the profile
/// where they are declared (`-p <name>`) — both at startup and on the live
/// reload path. They are deliberately invisible from other profiles, because a
/// profile is an isolation boundary, not a shared discovery tier. A global
/// scan of `profiles/*/skills/` would leak every profile's skills into every
/// other profile and is exactly what this test guards against.
#[tokio::test]
async fn profile_skill_is_discovered_under_its_own_profile_only() {
    use crate::config::profile::{home_for_profile, with_profile_home_async};

    let declared = format!("skill-decl-{}", uuid::Uuid::new_v4());
    let other = format!("skill-other-{}", uuid::Uuid::new_v4());

    // Declare a skill inside the `declared` profile's own skills directory.
    with_profile_home_async(Some(&declared), async {
        let dir = crate::config::opencrabs_home()
            .join("skills")
            .join("profile-only-skill");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            "---\nname: profile-only-skill\ndescription: Lives in one profile\n---\n\nBody.\n",
        )
        .unwrap();
    })
    .await;

    // Visible while that profile is active.
    let seen_here = with_profile_home_async(Some(&declared), async {
        load_all_skills()
            .iter()
            .any(|s| s.name == "profile-only-skill")
    })
    .await;

    // Invisible from a different profile (isolation holds).
    let seen_elsewhere = with_profile_home_async(Some(&other), async {
        load_all_skills()
            .iter()
            .any(|s| s.name == "profile-only-skill")
    })
    .await;

    let _ = std::fs::remove_dir_all(home_for_profile(Some(&declared)));
    let _ = std::fs::remove_dir_all(home_for_profile(Some(&other)));

    assert!(
        seen_here,
        "profile-declared skill must load under its own profile"
    );
    assert!(
        !seen_elsewhere,
        "profile-declared skill must NOT leak into a different profile"
    );
}

// --- Built-in sidecar guards -----------------------------------------------

/// Every directory under the templates tree is a skill the installer will
/// publish, and every one of them must load.
///
/// The templates tree IS the built-in set now — the release archive and the
/// deploy artifact copy it verbatim into `skills/`, and the loader reads
/// whatever is there. So the failure this guards is a `SKILL.md` that is
/// present but unparseable: it would ship in the archive and silently vanish
/// from every profile on the host, which is exactly the "shipped but loads for
/// nobody" shape the old name-list guard existed for.
///
/// The name must also match the directory: the directory is what an operator
/// sees, and a mismatch makes `/name` and the on-disk path disagree.
#[test]
fn every_shipped_skill_template_parses_and_names_its_own_dir() {
    let dir = shipped_skills_dir();
    let entries = std::fs::read_dir(&dir).expect("templates skills dir reads");

    let mut checked = 0usize;
    for entry in entries.flatten() {
        if !entry.path().is_dir() {
            continue;
        }
        let dir_name = entry.file_name().to_string_lossy().to_string();
        let raw = std::fs::read_to_string(entry.path().join("SKILL.md"))
            .unwrap_or_else(|e| panic!("{dir_name} has no readable SKILL.md: {e}"));
        let skill = Skill::parse(&dir_name, &raw, SkillSource::Builtin)
            .unwrap_or_else(|e| panic!("{dir_name}/SKILL.md does not parse: {e}"));
        assert_eq!(
            skill.name, dir_name,
            "{dir_name}/SKILL.md declares name {:?} — the frontmatter name must match \
             the directory, or the shipped slug and the on-disk path disagree",
            skill.name
        );
        assert!(
            !skill.description.is_empty(),
            "{dir_name} declares no description, so the LLM cannot decide when to invoke it"
        );
        assert!(!skill.body.is_empty(), "{dir_name} has an empty body");
        checked += 1;
    }
    assert!(
        checked > 0,
        "no skills under {} — test is vacuous",
        dir.display()
    );
}

/// A clean profile — no `~/.opencrabs/skills/`, no project overlays — still
/// loads all eight built-ins, badged `Builtin`.
///
/// This is the install contract: the bodies live in `skills/` beside the
/// executable, so a fresh profile that has never written a skill directory must
/// still see the whole set, and the TUI badge must attribute them to the
/// built-in tier rather than to the user overlay.
#[tokio::test]
async fn clean_profile_loads_all_eight_builtins_from_the_sidecar() {
    use crate::config::profile::{home_for_profile, with_profile_home_async};

    let profile = format!("test-builtins-{}", uuid::Uuid::new_v4());

    let skills = with_profile_home_async(Some(&profile), async {
        let home = crate::config::opencrabs_home();
        assert!(
            !home.join("skills").exists(),
            "fixture is not clean: {} already exists",
            home.join("skills").display()
        );
        load_shipped_skills()
    })
    .await;

    let _ = std::fs::remove_dir_all(home_for_profile(Some(&profile)));

    let builtins: Vec<&str> = skills
        .iter()
        .filter(|s| s.source == SkillSource::Builtin)
        .map(|s| s.name.as_str())
        .collect();
    assert_eq!(
        builtins,
        vec![
            "a2a-gateway",
            "browser-cdp",
            "cost-estimate",
            "dynamic-tools",
            "github",
            "multi-agent",
            "repo-audit",
            "security-audit",
        ],
        "a clean profile must load exactly the eight shipped built-ins"
    );
}

/// #949: a binary that was never installed still serves the built-in tier.
///
/// `cargo test` produces its binary in a cargo target directory, so
/// `<exe_dir>/skills` — the sidecar an install lays down — cannot exist there.
/// The loader used to return that absent path and serve ZERO built-ins, so
/// every consumer of the live registry lost the whole built-in tier at once
/// (the breakdown card, the context inventory, `load_brain_file`'s filename
/// form, the prompt builder). nextest's fail-fast then reported only the first
/// victim it scheduled, so one deterministic defect looked like a test that
/// alternated green/red.
///
/// This pins the invariant directly instead of the symptom: the resolved
/// built-in directory EXISTS and carries every shipped skill, and the
/// production entry point (`load_all_skills`) serves them from a clean profile.
#[test]
fn an_uninstalled_binary_still_serves_the_builtin_tier() {
    let dir = crate::brain::skills::builtin_skills_dir()
        .expect("a built-in skills dir must resolve for a test binary");
    assert!(
        dir.is_dir(),
        "the resolved built-in dir must exist, got {} — a test binary has no \
         installer, so the loader has to fall back to the shipped tree",
        dir.display()
    );

    // The expected set is read from the tree the archive is cut from, so the
    // guard cannot pass by enumerating its own constant.
    let shipped = shipped_skills_dir();
    let mut expected: Vec<String> = std::fs::read_dir(&shipped)
        .expect("the shipped templates tree reads")
        .flatten()
        .filter(|entry| entry.path().join("SKILL.md").is_file())
        .filter_map(|entry| entry.file_name().to_str().map(str::to_owned))
        .collect();
    expected.sort();
    assert!(
        !expected.is_empty(),
        "no skills under {} — test is vacuous",
        shipped.display()
    );

    for name in &expected {
        assert!(
            dir.join(name).join("SKILL.md").is_file(),
            "the resolved built-in dir must carry `{name}`: {}",
            dir.display()
        );
    }

    // And the production entry point serves them under a clean profile — the
    // exact read every failing test depended on.
    let home = tempfile::TempDir::new().unwrap();
    let loaded =
        crate::config::profile::with_home_override(home.path().to_path_buf(), load_all_skills);
    for name in &expected {
        assert!(
            loaded.iter().any(|skill| &skill.name == name),
            "load_all_skills() lost the built-in `{name}`; loaded: {:?}",
            loaded.iter().map(|skill| &skill.name).collect::<Vec<_>>()
        );
    }
}

// --- globs frontmatter (issue #150) ---

#[test]
fn globs_comma_string_form() {
    let raw =
        "---\nname: foo\ndescription: d\nglobs: skills/opencrabs-dev/**, README.md\n---\nBody.\n";
    let skill = Skill::parse("foo", raw, SkillSource::Builtin).unwrap();
    assert_eq!(skill.globs, vec!["skills/opencrabs-dev/**", "README.md"]);
}

#[test]
fn globs_inline_flow_form() {
    let raw = "---\nname: foo\ndescription: d\nglobs: [a/**, b.md]\n---\nBody.\n";
    let skill = Skill::parse("foo", raw, SkillSource::Builtin).unwrap();
    assert_eq!(skill.globs, vec!["a/**", "b.md"]);
}

#[test]
fn globs_block_list_form() {
    let raw = "---\nname: foo\ndescription: d\nglobs:\n  - one/**\n  - \"two.md\"\n  - 'three.md'\n---\nBody.\n";
    let skill = Skill::parse("foo", raw, SkillSource::Builtin).unwrap();
    assert_eq!(skill.globs, vec!["one/**", "two.md", "three.md"]);
}

#[test]
fn globs_quotes_stripped_comma_string() {
    let raw = "---\nname: foo\ndescription: d\nglobs: \"a/**\", 'b.md'\n---\nBody.\n";
    let skill = Skill::parse("foo", raw, SkillSource::Builtin).unwrap();
    assert_eq!(skill.globs, vec!["a/**", "b.md"]);
}

#[test]
fn globs_empty_when_absent() {
    let raw = "---\nname: foo\ndescription: d\n---\nBody.\n";
    let skill = Skill::parse("foo", raw, SkillSource::Builtin).unwrap();
    assert!(skill.globs.is_empty());
}

#[test]
fn globs_nested_metadata_ignored() {
    // A nested `metadata:` block is an unknown-key region for the
    // top-level parser; its `globs:` line must NOT be picked up.
    let raw = "---\nname: foo\ndescription: d\nmetadata:\n  globs: nested/**\n---\nBody.\n";
    let skill = Skill::parse("foo", raw, SkillSource::Builtin).unwrap();
    assert!(skill.globs.is_empty());
}

#[test]
fn globs_block_list_closed_by_next_key() {
    let raw = "---\nname: foo\ndescription: d\nglobs:\n  - a/**\nreview_gate: true\n---\nBody.\n";
    let skill = Skill::parse("foo", raw, SkillSource::Builtin).unwrap();
    assert_eq!(skill.globs, vec!["a/**"]);
    assert!(skill.review_gate);
}

#[test]
fn globs_empty_items_skipped() {
    let raw = "---\nname: foo\ndescription: d\nglobs: a/**, , b.md,\n---\nBody.\n";
    let skill = Skill::parse("foo", raw, SkillSource::Builtin).unwrap();
    assert_eq!(skill.globs, vec!["a/**", "b.md"]);
}
