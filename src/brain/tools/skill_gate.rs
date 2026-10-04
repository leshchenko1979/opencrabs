//! Skill glob gate (issue #150) — the registry-level sibling of
//! `plan_gate`.
//!
//! Cursor-style opt-in enforcement for skills: a `SKILL.md` that declares
//! a `globs:` frontmatter field guards its own topic. When the agent
//! attempts a tool call that references a path matching one of those
//! globs, and the skill body is NOT loaded (seen) in the current session
//! context — fresh sessions AND post-compaction (owner decision
//! 2026-09-10) — the call is rejected with a bounded NOTICE
//! (`gate_block_message`), not the body (#405). The notice names the
//! matched path, the skill, its globs and the route to the complete text.
//! The body itself used to ride the rejection verbatim: 23–47 KB per
//! trigger, and it arrived truncated at the harness cap
//! (`DEFAULT_MAX_INLINE_TOOL_BYTES`) anyway. The gate's job is to say a
//! relevant skill EXISTS and how to get it — the body answers a second
//! question the caller has not yet asked. The identical retry succeeds
//! either way: the retry is armed before the rejection returns.
//!
//! **A headless surface fails open (#405).** The remedy above — "read
//! this, then re-issue" — needs an interactive turn. A headless session
//! (cron, CLI one-shot, sub-agent) has none, so the refusal there is
//! terminal: an `ai-antispam` cron job's first `bash` call was refused on
//! a path mention, the script never ran, and the job self-disabled —
//! an outcome indistinguishable from a job that chose not to act. Where
//! no human can undo the damage, the gate does not block.
//!
//! Laws (design v2, decisions 6–9):
//! - **Fail-open:** any gate-internal error (pattern compile failure,
//!   malformed globs, missing skill file, registry error) → `Pass`. The
//!   gate must never dead-end an agent.
//! - **Opt-in:** no `globs` key → the skill is invisible to the gate.
//! - **Exempt recovery tools** must never be gated, or a blocked agent
//!   cannot self-serve (`load_brain_file`, `read_file`, `slash_command`,
//!   `session_search`, `tool_search`, `write_opencrabs_file`,
//!   `execute_code`).
//! - **Headless fails open:** a session with no live user surface (cron,
//!   CLI one-shot, sub-agent) is never blocked — the remedy needs an
//!   interactive turn, and a headless session has none.
//! - **Cursor match table:** `*` matches one path segment (never `/`),
//!   `**` matches recursively — `require_literal_separator: true`.
//!   Globs match against the normalized ABSOLUTE path; skill authors
//!   use `**/` prefixes (e.g. `**/skills/opencrabs-dev/**`).
//! - **Pattern resolution** ([`compile_pattern`]): `~` expands to home;
//!   a wildcard-led pattern (`**/x`) is unanchored and matched verbatim;
//!   any other relative pattern anchors at the session cwd. A glob is
//!   never blanket-prefixed with `cwd` — that silently disarms every
//!   documented `**/…` form.
//! - **Cheap + deterministic:** fast-exit when disabled / no loaded
//!   skill declares globs / tool is exempt.

use std::path::Path;

use serde_json::Value;
use uuid::Uuid;

use super::seen_skills;
use crate::brain::agent::service::tool_loop::DEFAULT_MAX_INLINE_TOOL_BYTES;

/// Tools the gate never touches — recovery paths a blocked agent must
/// keep available to read the skill body and re-arm itself.
pub(crate) const EXEMPT_TOOLS: &[&str] = &[
    "load_brain_file",
    "read_file",
    "slash_command",
    "session_search",
    "tool_search",
    "write_opencrabs_file",
    "execute_code",
];

/// Input keys harvested as candidate paths (decision 6). `grep`'s
/// `pattern` is excluded (regex, not a path); the `glob` tool's
/// `pattern` is excluded (a glob pattern string is not a path — finding
/// 24). Bash commands are harvested separately as path-like tokens.
const PATH_KEYS: &[&str] = &["path", "file_path", "filePath"];

/// Tools whose `pattern`-like keys are NOT harvested even when named in
/// [`PATH_KEYS`]-adjacent forms. (Kept explicit for the doc contract.)
const PATTERN_ONLY_TOOLS: &[&str] = &["grep", "glob"];

/// The gate's verdict on one tool call.
#[derive(Debug, Clone, PartialEq)]
pub enum GateVerdict {
    /// The call proceeds (also the verdict for every fail-open path).
    Pass,
    /// The call is blocked: the notice names the skill, its globs and the
    /// route to its body. The body itself does NOT ride the rejection
    /// (#405) — it cost 23–47 KB per trigger and arrived truncated at the
    /// harness cap anyway.
    Block {
        skill: String,
        matched_path: String,
        globs: Vec<String>,
    },
}

/// The gate's rejection text (#405): a bounded NOTICE carrying the matched
/// path, the skill, its globs, an honest statement of why the body is not
/// appended, and a durable route to the complete body.
///
/// `source_path` is resolved by the CALLER and passed in — `None` for a
/// built-in skill, which has no file on disk — so this formatter is pure
/// and testable without touching a live home. `load_brain_file` (with the
/// slug) is named in both arms: it is the one route that works for every
/// skill, and it marks the skill seen exactly like the gate's own
/// `mark_seen`.
///
/// The body is deliberately absent. Appending it cost 23–47 KB per trigger
/// (measured, #405) and `DEFAULT_MAX_INLINE_TOOL_BYTES` truncated it
/// anyway. This function's output must stay well under that cap — a
/// regression test pins the bound.
pub(crate) fn gate_block_message(
    skill: &str,
    matched_path: &str,
    globs: &[String],
    source_path: Option<&Path>,
) -> String {
    let route = match source_path {
        Some(path) => format!(
            "Read the complete body with read_file on '{}', or reload it with \
             load_brain_file '{}' — both are exempt from this gate, and \
             load_brain_file also takes query= for a section-filtered reload.",
            path.display(),
            skill
        ),
        None => format!(
            "Reload the complete body with load_brain_file '{}' (exempt from \
             this gate; it also takes query= for a section-filtered reload). This \
             skill is compiled in, so there is no file on disk to read.",
            skill
        ),
    };
    format!(
        "[SKILL GATE] This call touches '{}', which matches skill '{}' (globs: {}), and \
         that skill is not loaded in this session context. The skill body is NOT \
         appended here: it costs tens of KB per trigger, and tool output is capped at \
         {} bytes, so a full body would arrive as a head/tail preview anyway. {} \
         Re-issue the identical call once you have read it.",
        matched_path,
        skill,
        globs.join(", "),
        DEFAULT_MAX_INLINE_TOOL_BYTES,
        route
    )
}

/// Gate entry point. `enabled` is the `[agent] skill_glob_gate` master
/// switch; `cwd` is the session's working directory for resolving
/// relative candidate paths; `headless` marks a surface with no live user
/// (cron, CLI one-shot, sub-agent) where a refusal is terminal (#405).
pub fn check(
    session_id: Uuid,
    tool_name: &str,
    input: &Value,
    cwd: &std::path::Path,
    enabled: bool,
    headless: bool,
) -> GateVerdict {
    // Headless fails open (#405): the remedy ("read this, then re-issue")
    // needs an interactive turn, and a headless surface has none — so a
    // block there is a dead end no human can undo.
    if !enabled || headless || EXEMPT_TOOLS.contains(&tool_name) {
        return GateVerdict::Pass;
    }
    // Fast-exit: no loaded skill declares globs (decision 12 — zero cost).
    let skills = crate::brain::skills::skills_with_globs();
    if skills.is_empty() {
        return GateVerdict::Pass;
    }
    let candidates = harvest_candidates(tool_name, input, cwd);
    if candidates.is_empty() {
        return GateVerdict::Pass;
    }

    for skill in &skills {
        for glob_str in &skill.globs {
            let pattern = match compile_pattern(glob_str, cwd) {
                Ok(p) => p,
                Err(e) => {
                    // Malformed glob: warn once per (slug, glob) per
                    // process and skip — fail-open (decision 9).
                    warn_malformed_glob(&skill.name, glob_str, &e.to_string());
                    continue;
                }
            };
            let options = glob::MatchOptions {
                case_sensitive: false,
                require_literal_separator: true, // Cursor `*` = one segment (B5)
                require_literal_leading_dot: false,
            };
            for candidate in &candidates {
                if pattern.matches_path_with(candidate, options) {
                    if seen_skills::seen_since_compaction(session_id, &skill.name) {
                        return GateVerdict::Pass;
                    }
                    return GateVerdict::Block {
                        skill: skill.name.clone(),
                        matched_path: candidate.display().to_string(),
                        globs: skill.globs.clone(),
                    };
                }
            }
        }
    }
    GateVerdict::Pass
}

/// Warn-once set for malformed globs (decision 9).
fn warn_malformed_glob(slug: &str, glob_str: &str, err: &str) {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNT: AtomicU64 = AtomicU64::new(0);
    // Warn at most 8 times per process per signature to bound log noise
    // without adding a second map.
    if COUNT.fetch_add(1, Ordering::Relaxed) < 8 {
        tracing::warn!(
            "skill_gate: skill '{slug}' has malformed glob '{glob_str}' ({err}) — skipping (fail-open)"
        );
    }
}

/// Harvest candidate absolute paths from the tool input (decision 6).
pub(crate) fn harvest_candidates(
    tool_name: &str,
    input: &Value,
    cwd: &std::path::Path,
) -> Vec<std::path::PathBuf> {
    let mut out: Vec<String> = Vec::new();
    let obj = match input.as_object() {
        Some(o) => o,
        None => return Vec::new(),
    };

    if !PATTERN_ONLY_TOOLS.contains(&tool_name) {
        for key in PATH_KEYS {
            if let Some(Value::String(s)) = obj.get(*key) {
                out.push(s.clone());
            }
        }
    } else {
        // grep/glob: the `pattern` key is regex/glob text — never a path —
        // but their `path` argument IS a real search root and must still be
        // harvested, otherwise a gated skill under a grep'd directory never
        // fires. Only `pattern` is excluded (finding 24's actual scope).
        if let Some(Value::String(p)) = obj.get("path") {
            out.push(p.clone());
        }
    }

    // Bash: extract path-like tokens — words containing `/` or tilde
    // prefixes. Leaky by design (fail-open): we prefer deterministic-cheap
    // over exhaustive.
    if tool_name == "bash"
        && let Some(Value::String(cmd)) = obj.get("command")
    {
        for token in cmd.split_whitespace() {
            let tok = token.trim_matches(|c| c == '"' || c == '\'' || c == ';' || c == ',');
            if tok.contains('/') || tok.starts_with('~') {
                out.push(tok.to_string());
            }
        }
    }

    out.into_iter()
        .map(|raw| {
            let expanded = super::error::expand_tilde(&raw);
            let joined = if expanded.is_absolute() {
                expanded
            } else {
                cwd.join(expanded)
            };
            normalize(&joined)
        })
        .collect()
}

/// Compile a frontmatter glob into a matcher.
///
/// A glob is not a path, so the resolution is deliberately narrow — and the
/// narrowness is load-bearing:
///
/// 1. a leading `~` expands to the home directory (`~/repo/**`). Without
///    this the natural Cursor-style form compiles with a literal `~` and can
///    never match an absolute candidate path: the gate would be silently
///    inert for exactly the skills that opt in.
/// 2. a pattern that is absolute, or whose first component starts with a
///    wildcard (`**/x`, `*/x`, `[ab]/x`), is used VERBATIM. `**/…` is an
///    *unanchored* "match anywhere" pattern (`**/test` matches
///    `/one/two/test`); prefixing it with `cwd` pins it to a prefix it can
///    never satisfy and disarms the gate for every such skill.
/// 3. any other relative pattern (`src/**`) anchors at the tool's `cwd`,
///    mirroring how relative candidate paths are resolved — so a
///    root-relative author intent still matches, instead of failing open.
/// 4. the result is normalized (`.` / `..` / duplicate separators).
pub(crate) fn compile_pattern(
    glob_str: &str,
    cwd: &std::path::Path,
) -> Result<glob::Pattern, glob::PatternError> {
    let expanded = super::error::expand_tilde(glob_str);
    let anchored = if expanded.is_absolute() || is_wildcard_led(glob_str) {
        expanded
    } else {
        cwd.join(expanded)
    };
    glob::Pattern::new(&normalize(&anchored).to_string_lossy())
}

/// Whether a glob's first path component starts with a wildcard — i.e. the
/// pattern is anchored nowhere and must match at any depth.
fn is_wildcard_led(glob_str: &str) -> bool {
    matches!(glob_str.as_bytes().first(), Some(b'*' | b'?' | b'['))
}
/// Normalize a path for matching: resolve `.` / `..` lexically, strip
/// redundant separators. No filesystem access — purely string-level so
/// the gate cannot fail on missing paths.
fn normalize(path: &std::path::Path) -> std::path::PathBuf {
    let mut out = std::path::PathBuf::new();
    for comp in path.components() {
        match comp {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}
