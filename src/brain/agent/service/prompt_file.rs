//! Profile-scoped compaction prompt text (#929).
//!
//! Every word the compaction machinery hands to a model used to live in Rust
//! string literals — the summariser's system prompt, the continuation-document
//! body, the budget directive, the scope preludes, the three guidance blocks
//! and the ten post-compaction continuation bodies. Editing any of them
//! meant a recompile, so a profile could not tune its own compaction without
//! forking the binary.
//!
//! This module moves that text into a TOML file:
//!
//! - the checked-in default, `prompts/compaction.default.toml`, is
//!   `include_str!`-ed into the binary ([`DEFAULT_PROMPT_TOML`]);
//! - a profile may override it at `<profile_home>/prompts/compaction.toml`.
//!
//! # Fail open, never panic
//!
//! A compaction that cannot run ends the session; a compaction that runs with
//! a stale prompt is merely suboptimal. So every failure to load an override —
//! missing file, unreadable file, TOML parse error, failed validation — logs a
//! `tracing::warn!` naming the failure and the path, and serves the compiled-in
//! default. There is deliberately no last-known-good cache: the default IS the
//! known-good copy, and it ships in the binary.
//!
//! # Substitution is explicit, not `format!`
//!
//! `format!` cannot take a runtime string, so the body is a template whose
//! `{name}` tokens are substituted by [`render`]. The token set is declared
//! once in [`BODY_PLACEHOLDERS`]; [`CompactionPrompts::validate`] and
//! [`CompactionPrompts::body`] both derive from that single constant, so a
//! template that gains or loses a token fails validation instead of rendering
//! a stray `{name}` into a live prompt.

use std::path::Path;

use sha2::{Digest, Sha256};

use super::compaction_prompts::{CompactionKind, PlanRecovery};
use crate::brain::agent::context::CompactionScope;

/// The checked-in default prompt set, compiled into the binary.
///
/// Path: this file lives at `src/brain/agent/service/prompt_file.rs`, so
/// `../` → `src/brain/agent`, `../../` → `src/brain`, `../../../` → `src`,
/// `../../../../` → the repo root. Four levels up, then `prompts/`.
pub(crate) const DEFAULT_PROMPT_TOML: &str =
    include_str!("../../../../prompts/compaction.default.toml");

/// Where an override lives inside a profile home.
const PROMPT_FILE_REL: &str = "prompts/compaction.toml";

/// Every placeholder the continuation-document body must carry, and nothing
/// else. This is the single source of truth: [`CompactionPrompts::validate`]
/// rejects a body whose token set differs, and [`CompactionPrompts::body`]
/// supplies exactly these values.
pub(crate) const BODY_PLACEHOLDERS: &[&str] = &[
    "snapshot_usage_pct",
    "snapshot_token_count",
    "snapshot_max_tokens",
    "remaining_budget",
    "context_inventory",
    "tool_approval_status",
    "progress_guidance",
    "recovery_guidance",
    "next_step_guidance",
    "budget_directive",
    "budget",
    "ratio",
    "window",
];

/// The placeholder carried by `summariser.budget_directive`.
const BUDGET_PLACEHOLDER: &str = "summary_budget_tokens";

/// The placeholder carried by `continuation.shared.skill_stamp`.
const SKILL_STAMP_PLACEHOLDER: &str = "skills";

/// Where the prompt set in hand came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Source {
    /// The compiled-in default (`prompts/compaction.default.toml`).
    #[default]
    Default,
    /// A profile override at `<profile_home>/prompts/compaction.toml`.
    File,
}

/// `[meta]`
#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Meta {
    /// Schema version of the prompt file. `0` means the key was absent.
    #[serde(default)]
    pub version: u32,
    /// Human-readable description of the set; documentation only, never read
    /// at runtime (and deliberately excluded from the fingerprint).
    #[serde(default)]
    #[allow(dead_code)]
    pub description: String,
}

/// `[summariser]`
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Summariser {
    /// System prompt for the summariser request.
    pub system: String,
    /// The continuation-document body — a `{name}` template.
    pub body: String,
    /// Closing budget directive — carries `{summary_budget_tokens}`.
    pub budget_directive: String,
    /// `[summariser.scope]`
    pub scope: ScopePreludes,
}

/// `[summariser.scope]`
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ScopePreludes {
    /// Prepended for [`CompactionScope::DeltaSinceMarker`].
    pub delta: String,
    /// Prepended for [`CompactionScope::SegmentConsolidation`].
    pub consolidation: String,
}

/// `[guidance]`
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Guidance {
    /// Section 0's progress stamp (#817).
    pub progress_stamp: String,
    /// Section 7's recovery playbook (#817).
    pub recovery_playbook: String,
    /// Section 8's next step (#817).
    pub next_step: String,
}

/// `[continuation.<kind>]`
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ContinuationVariant {
    /// The fun variant (the default): explicitly invites a one-line roast.
    pub fun: String,
    /// The silent variant (`[agent] silent_compaction = true`).
    pub silent: String,
}

/// `[continuation.shared]` — the text appended to every continuation.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SharedSuffixes {
    /// Section-0 status gate (#499).
    pub obligation_status: String,
    /// Session-recovery hint when the session has no plan.
    pub session_recovery_noplan: String,
    /// Session-recovery hint when the plan is in Editing.
    pub session_recovery_editing: String,
    /// Session-recovery hint when the plan is Active.
    pub session_recovery_active: String,
    /// Appended when `auto_approve` is false.
    pub approval_tail: String,
    /// The #125 skill-inventory stamp — carries `{skills}`.
    pub skill_stamp: String,
}

/// `[continuation]`
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Continuation {
    /// Async budget compaction.
    pub regular: ContinuationVariant,
    /// Compaction between tool iterations.
    pub mid_loop: ContinuationVariant,
    /// Provider context-length 4xx.
    pub emergency: ContinuationVariant,
    /// Compaction after a tool call completed.
    pub post_tool: ContinuationVariant,
    /// User-triggered `/compact`.
    pub manual: ContinuationVariant,
    /// Text shared by every variant.
    pub shared: SharedSuffixes,
}

/// The whole prompt set: every compaction string the runtime emits.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CompactionPrompts {
    /// `[meta]`
    #[serde(default)]
    pub meta: Meta,
    /// `[summariser]`
    pub summariser: Summariser,
    /// `[guidance]`
    pub guidance: Guidance,
    /// `[continuation]`
    pub continuation: Continuation,
    /// Provenance; never read from the file.
    #[serde(skip)]
    source: Source,
}

/// The dynamic values the continuation-document body interpolates.
///
/// The seven remaining [`BODY_PLACEHOLDERS`] — `progress_guidance`,
/// `recovery_guidance`, `next_step_guidance`, `budget_directive`, `budget`,
/// `ratio`, `window` — are derived from these and from the prompt set itself,
/// so a caller cannot supply a guidance block that disagrees with the template
/// it is being substituted into.
pub(crate) struct BodyValues<'a> {
    /// Context-window usage percentage, rendered with zero decimals.
    pub snapshot_usage_pct: f64,
    /// Tokens currently in the window.
    pub snapshot_token_count: usize,
    /// The window size.
    pub snapshot_max_tokens: usize,
    /// Tokens left before the window is full.
    pub remaining_budget: usize,
    /// The pre-rendered context-inventory markdown table.
    pub context_inventory: &'a str,
    /// Whether tools run without approval.
    pub auto_approve_tools: bool,
    /// The document budget the directive states.
    pub summary_budget_tokens: u32,
}

impl CompactionPrompts {
    /// The compiled-in default set, validated.
    ///
    /// Panics only if the checked-in TOML is malformed — a build-time defect
    /// pinned by this module's own tests, not a runtime condition. Every
    /// *file* path fails open through [`CompactionPrompts::load`] instead.
    pub(crate) fn compiled_default() -> Self {
        match Self::parse(DEFAULT_PROMPT_TOML) {
            Ok(mut prompts) => {
                prompts.source = Source::Default;
                prompts
            }
            Err(e) => panic!("compiled-in default compaction prompt is invalid: {e}"),
        }
    }

    fn parse(text: &str) -> Result<Self, String> {
        let prompts: Self = toml::from_str(text).map_err(|e| format!("toml parse error: {e}"))?;
        prompts.validate()?;
        Ok(prompts)
    }

    /// Load the prompt set for `profile_dir`, failing open to the default.
    ///
    /// Reads `<profile_dir>/prompts/compaction.toml`. Any failure — missing,
    /// unreadable, unparsable, invalid — is logged with the path and the
    /// reason, and the compiled-in default is served.
    pub(crate) fn load(profile_dir: &Path) -> Self {
        let path = profile_dir.join(PROMPT_FILE_REL);
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                tracing::warn!(
                    path = %path.display(),
                    "compaction prompt: override NOT FOUND — serving the compiled-in default"
                );
                return Self::fallback();
            }
            Err(e) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %e,
                    "compaction prompt: override UNREADABLE — serving the compiled-in default"
                );
                return Self::fallback();
            }
        };

        match Self::parse(&text) {
            Ok(mut prompts) => {
                prompts.source = Source::File;
                prompts.log_loaded();
                prompts
            }
            Err(reason) => {
                tracing::warn!(
                    path = %path.display(),
                    reason = %reason,
                    "compaction prompt: override REJECTED — serving the compiled-in default"
                );
                Self::fallback()
            }
        }
    }

    fn fallback() -> Self {
        let prompts = Self::compiled_default();
        prompts.log_loaded();
        prompts
    }

    fn log_loaded(&self) {
        tracing::info!(
            "compaction prompt: source={} version={} fingerprint={}",
            self.source(),
            self.meta.version,
            self.fingerprint()
        );
    }

    /// `"file"` when a profile override was accepted, `"default"` otherwise.
    pub(crate) fn source(&self) -> &'static str {
        match self.source {
            Source::File => "file",
            Source::Default => "default",
        }
    }

    /// sha256 over the prompt fields, in this fixed order, each field followed
    /// by a NUL byte so two different field sequences cannot collide:
    ///
    /// 1. `meta.version`
    /// 2. `summariser.system`
    /// 3. `summariser.body`
    /// 4. `summariser.budget_directive`
    /// 5. `summariser.scope.delta`
    /// 6. `summariser.scope.consolidation`
    /// 7. `guidance.progress_stamp`
    /// 8. `guidance.recovery_playbook`
    /// 9. `guidance.next_step`
    /// 10. `continuation.regular.{fun,silent}`
    /// 11. `continuation.mid_loop.{fun,silent}`
    /// 12. `continuation.emergency.{fun,silent}`
    /// 13. `continuation.post_tool.{fun,silent}`
    /// 14. `continuation.manual.{fun,silent}`
    /// 15. `continuation.shared.obligation_status`
    /// 16. `continuation.shared.session_recovery_noplan`
    /// 17. `continuation.shared.session_recovery_editing`
    /// 18. `continuation.shared.session_recovery_active`
    /// 19. `continuation.shared.approval_tail`
    /// 20. `continuation.shared.skill_stamp`
    ///
    /// `meta.description` is excluded: it is documentation, not prompt text.
    pub(crate) fn fingerprint(&self) -> String {
        fn feed(h: &mut Sha256, s: &str) {
            h.update(s.as_bytes());
            h.update([0u8]);
        }
        let mut h = Sha256::new();
        feed(&mut h, &self.meta.version.to_string());
        feed(&mut h, &self.summariser.system);
        feed(&mut h, &self.summariser.body);
        feed(&mut h, &self.summariser.budget_directive);
        feed(&mut h, &self.summariser.scope.delta);
        feed(&mut h, &self.summariser.scope.consolidation);
        feed(&mut h, &self.guidance.progress_stamp);
        feed(&mut h, &self.guidance.recovery_playbook);
        feed(&mut h, &self.guidance.next_step);
        for variant in [
            &self.continuation.regular,
            &self.continuation.mid_loop,
            &self.continuation.emergency,
            &self.continuation.post_tool,
            &self.continuation.manual,
        ] {
            feed(&mut h, &variant.fun);
            feed(&mut h, &variant.silent);
        }
        feed(&mut h, &self.continuation.shared.obligation_status);
        feed(&mut h, &self.continuation.shared.session_recovery_noplan);
        feed(&mut h, &self.continuation.shared.session_recovery_editing);
        feed(&mut h, &self.continuation.shared.session_recovery_active);
        feed(&mut h, &self.continuation.shared.approval_tail);
        feed(&mut h, &self.continuation.shared.skill_stamp);
        format!("{:x}", h.finalize())
    }

    /// The fingerprint's first 12 hex characters.
    ///
    /// Enough to identify the prompt in a log line or a memory-log header
    /// without pasting 64 characters there; the full hash stays available from
    /// [`CompactionPrompts::fingerprint`] when an exact match is needed.
    pub(crate) fn short_fingerprint(&self) -> String {
        self.fingerprint()[..12].to_string()
    }

    /// `source:sha256-prefix` — the stamp written onto every artifact this
    /// prompt produced, so a stored summary can be traced back to its text.
    pub(crate) fn stamp(&self) -> String {
        format!("{}:{}", self.source(), self.short_fingerprint())
    }

    /// Reject a prompt set that would misrender at runtime.
    ///
    /// Checks: `meta.version` present, every required string non-empty, the
    /// body's placeholder set exactly [`BODY_PLACEHOLDERS`], the budget
    /// directive's `{summary_budget_tokens}`, and the skill stamp's
    /// `{skills}`. Each failure names the offending key or token.
    pub(crate) fn validate(&self) -> Result<(), String> {
        if self.meta.version == 0 {
            return Err("meta.version is missing or 0".to_string());
        }

        let required: [(&str, &str); 18] = [
            ("summariser.system", &self.summariser.system),
            ("summariser.body", &self.summariser.body),
            (
                "summariser.budget_directive",
                &self.summariser.budget_directive,
            ),
            ("summariser.scope.delta", &self.summariser.scope.delta),
            (
                "summariser.scope.consolidation",
                &self.summariser.scope.consolidation,
            ),
            ("guidance.progress_stamp", &self.guidance.progress_stamp),
            (
                "guidance.recovery_playbook",
                &self.guidance.recovery_playbook,
            ),
            ("guidance.next_step", &self.guidance.next_step),
            ("continuation.regular.fun", &self.continuation.regular.fun),
            (
                "continuation.regular.silent",
                &self.continuation.regular.silent,
            ),
            ("continuation.mid_loop.fun", &self.continuation.mid_loop.fun),
            (
                "continuation.mid_loop.silent",
                &self.continuation.mid_loop.silent,
            ),
            (
                "continuation.emergency.fun",
                &self.continuation.emergency.fun,
            ),
            (
                "continuation.emergency.silent",
                &self.continuation.emergency.silent,
            ),
            (
                "continuation.post_tool.fun",
                &self.continuation.post_tool.fun,
            ),
            (
                "continuation.post_tool.silent",
                &self.continuation.post_tool.silent,
            ),
            ("continuation.manual.fun", &self.continuation.manual.fun),
            (
                "continuation.manual.silent",
                &self.continuation.manual.silent,
            ),
        ];
        let empty: Vec<&str> = required
            .iter()
            .filter(|(_, v)| v.trim().is_empty())
            .map(|(k, _)| *k)
            .collect();
        if !empty.is_empty() {
            return Err(format!("empty required key(s): {}", empty.join(", ")));
        }
        let shared: [(&str, &str); 6] = [
            (
                "continuation.shared.obligation_status",
                &self.continuation.shared.obligation_status,
            ),
            (
                "continuation.shared.session_recovery_noplan",
                &self.continuation.shared.session_recovery_noplan,
            ),
            (
                "continuation.shared.session_recovery_editing",
                &self.continuation.shared.session_recovery_editing,
            ),
            (
                "continuation.shared.session_recovery_active",
                &self.continuation.shared.session_recovery_active,
            ),
            (
                "continuation.shared.approval_tail",
                &self.continuation.shared.approval_tail,
            ),
            (
                "continuation.shared.skill_stamp",
                &self.continuation.shared.skill_stamp,
            ),
        ];
        let empty: Vec<&str> = shared
            .iter()
            .filter(|(_, v)| v.trim().is_empty())
            .map(|(k, _)| *k)
            .collect();
        if !empty.is_empty() {
            return Err(format!("empty required key(s): {}", empty.join(", ")));
        }

        // The body's token set must equal BODY_PLACEHOLDERS exactly.
        let found = placeholder_tokens(&self.summariser.body)
            .map_err(|e| format!("summariser.body: {e}"))?;
        let mut found_sorted = found.clone();
        found_sorted.sort();
        found_sorted.dedup();
        let mut want: Vec<&str> = BODY_PLACEHOLDERS.to_vec();
        want.sort();
        want.dedup();
        if found_sorted.len() != want.len()
            || found_sorted
                .iter()
                .map(String::as_str)
                .ne(want.iter().copied())
        {
            let missing: Vec<&str> = want
                .iter()
                .copied()
                .filter(|w| !found.iter().any(|f| f == w))
                .collect();
            let extra: Vec<&str> = found
                .iter()
                .map(String::as_str)
                .filter(|f| !want.contains(f))
                .collect();
            return Err(format!(
                "summariser.body placeholder set mismatch: missing [{}], unexpected [{}]",
                missing.join(", "),
                extra.join(", ")
            ));
        }

        let budget = placeholder_tokens(&self.summariser.budget_directive)
            .map_err(|e| format!("summariser.budget_directive: {e}"))?;
        if budget.len() != 1 || budget[0] != BUDGET_PLACEHOLDER {
            return Err(format!(
                "summariser.budget_directive must carry exactly `{{{BUDGET_PLACEHOLDER}}}`, found {budget:?}"
            ));
        }

        let stamp = placeholder_tokens(&self.continuation.shared.skill_stamp)
            .map_err(|e| format!("continuation.shared.skill_stamp: {e}"))?;
        if stamp.len() != 1 || stamp[0] != SKILL_STAMP_PLACEHOLDER {
            return Err(format!(
                "continuation.shared.skill_stamp must carry exactly `{{{SKILL_STAMP_PLACEHOLDER}}}`, found {stamp:?}"
            ));
        }

        Ok(())
    }

    /// The summariser's system prompt.
    pub(crate) fn system_prompt(&self) -> &str {
        &self.summariser.system
    }

    /// The closing budget directive, with the token budget substituted.
    pub(crate) fn budget_directive(&self, summary_budget_tokens: u32) -> String {
        let budget = summary_budget_tokens.to_string();
        render(
            &self.summariser.budget_directive,
            &[(BUDGET_PLACEHOLDER, budget.as_str())],
        )
    }

    /// The scope header prepended to the body; empty for
    /// [`CompactionScope::FullWindow`], which keeps the first-compaction
    /// prompt byte-identical to the classic one (#1649).
    pub(crate) fn scope_prelude(&self, scope: CompactionScope) -> String {
        match scope {
            CompactionScope::FullWindow => String::new(),
            CompactionScope::DeltaSinceMarker => self.summariser.scope.delta.clone(),
            CompactionScope::SegmentConsolidation => self.summariser.scope.consolidation.clone(),
        }
    }

    /// The continuation-document body *template*, unsubstituted.
    ///
    /// For consumers that grade or diff the shipped text rather than render
    /// it — the compaction eval gate reads this so an operator override file
    /// is gated too, not just the compiled default. Rendering still goes
    /// through [`CompactionPrompts::body`], which is the only path that
    /// substitutes.
    pub(crate) fn raw_body(&self) -> &str {
        &self.summariser.body
    }

    /// The `[summariser].system` template, unsubstituted.
    pub(crate) fn raw_system(&self) -> &str {
        &self.summariser.system
    }

    /// The `[summariser].budget_directive` template, unsubstituted.
    ///
    /// Distinct from [`CompactionPrompts::budget_directive`], which substitutes
    /// the token count — this one lets a gate assert the template still carries
    /// its `{summary_budget_tokens}` placeholder.
    pub(crate) fn raw_budget_directive(&self) -> &str {
        &self.summariser.budget_directive
    }

    /// The continuation-document body, with every [`BODY_PLACEHOLDERS`] token
    /// substituted. Validation guarantees the template carries exactly those
    /// tokens, so no stray `{name}` can survive into a live prompt.
    pub(crate) fn body(&self, values: &BodyValues<'_>) -> String {
        let usage_pct = format!("{:.0}", values.snapshot_usage_pct);
        let token_count = values.snapshot_token_count.to_string();
        let max_tokens = values.snapshot_max_tokens.to_string();
        let remaining = values.remaining_budget.to_string();
        let budget_directive = self.budget_directive(values.summary_budget_tokens);
        let budget = crate::brain::skills::retained_set_budget_tokens(values.snapshot_max_tokens)
            .to_string();
        let ratio = format!(
            "{:.0}",
            crate::brain::skills::RETAINED_SET_BUDGET_RATIO * 100.0
        );
        let window = values.snapshot_max_tokens.to_string();
        let approval = if values.auto_approve_tools {
            "AUTO-APPROVE ON (tools run freely)"
        } else {
            "AUTO-APPROVE OFF — tool approval is REQUIRED for every tool call"
        };

        render(
            &self.summariser.body,
            &[
                ("snapshot_usage_pct", usage_pct.as_str()),
                ("snapshot_token_count", token_count.as_str()),
                ("snapshot_max_tokens", max_tokens.as_str()),
                ("remaining_budget", remaining.as_str()),
                ("context_inventory", values.context_inventory),
                ("tool_approval_status", approval),
                ("progress_guidance", self.progress_stamp()),
                ("recovery_guidance", self.recovery_playbook()),
                ("next_step_guidance", self.next_step()),
                ("budget_directive", budget_directive.as_str()),
                ("budget", budget.as_str()),
                ("ratio", ratio.as_str()),
                ("window", window.as_str()),
            ],
        )
    }

    /// Section 0's progress stamp guidance (#817).
    pub(crate) fn progress_stamp(&self) -> &str {
        &self.guidance.progress_stamp
    }

    /// Section 7's recovery playbook guidance (#817).
    pub(crate) fn recovery_playbook(&self) -> &str {
        &self.guidance.recovery_playbook
    }

    /// Section 8's next-step guidance (#817).
    pub(crate) fn next_step(&self) -> &str {
        &self.guidance.next_step
    }

    /// The continuation body for `kind` in the requested emission mode.
    pub(crate) fn continuation_body(&self, kind: CompactionKind, silent: bool) -> &str {
        let variant = match kind {
            CompactionKind::Regular => &self.continuation.regular,
            CompactionKind::MidLoop => &self.continuation.mid_loop,
            CompactionKind::Emergency => &self.continuation.emergency,
            CompactionKind::PostTool => &self.continuation.post_tool,
            CompactionKind::Manual => &self.continuation.manual,
        };
        if silent {
            &variant.silent
        } else {
            &variant.fun
        }
    }

    /// Section-0 status gate, appended to every continuation (#499).
    pub(crate) fn obligation_status_rule(&self) -> &str {
        &self.continuation.shared.obligation_status
    }

    /// The session-recovery hint for the session's plan state.
    pub(crate) fn session_recovery(&self, plan: PlanRecovery) -> &str {
        match plan {
            PlanRecovery::NoPlan => &self.continuation.shared.session_recovery_noplan,
            PlanRecovery::Editing => &self.continuation.shared.session_recovery_editing,
            PlanRecovery::Active => &self.continuation.shared.session_recovery_active,
        }
    }

    /// The tool-approval reminder appended when approval is required.
    pub(crate) fn approval_tail(&self) -> &str {
        &self.continuation.shared.approval_tail
    }

    /// The #125 skill-inventory stamp with `names` substituted.
    pub(crate) fn skill_stamp(&self, names: &str) -> String {
        render(
            &self.continuation.shared.skill_stamp,
            &[(SKILL_STAMP_PLACEHOLDER, names)],
        )
    }
}

/// Collect the `{name}` tokens of a template, in order of appearance.
///
/// Errors on an unclosed `{`, an empty `{}`, a stray `}`, or a token that
/// carries `format!`-style spec syntax (`{:.0}`, `{ratio:.0}`) — the body is
/// substituted by [`render`], which knows nothing about format specs, so those
/// must be spelled as bare names.
fn placeholder_tokens(template: &str) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        let after = &rest[open + 1..];
        let Some(close) = after.find('}') else {
            return Err("unclosed `{`".to_string());
        };
        let name = &after[..close];
        if name.is_empty() {
            return Err("empty `{}` placeholder".to_string());
        }
        if name
            .chars()
            .any(|c| matches!(c, ':' | '.' | ' ' | '{' | '-' | '+' | '#' | '0' | '*'))
        {
            return Err(format!(
                "placeholder `{{{name}}}` uses format-spec syntax; use a bare `{{name}}`"
            ));
        }
        out.push(name.to_string());
        rest = &after[close + 1..];
    }
    if rest.contains('}') {
        return Err("unmatched `}`".to_string());
    }
    Ok(out)
}

/// Substitute `{name}` tokens in `template` from `values`.
///
/// Unknown tokens are copied through verbatim rather than dropped: validation
/// has already rejected them, so reaching that branch means a caller rendered
/// a template it did not validate, and silently swallowing the token would
/// hide it. No `format!` is involved — `template` is a runtime string.
fn render(template: &str, values: &[(&str, &str)]) -> String {
    let mut out = String::with_capacity(template.len() + 256);
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        let Some(close) = after.find('}') else {
            out.push_str(&rest[open..]);
            return out;
        };
        let name = &after[..close];
        match values.iter().find(|(k, _)| *k == name) {
            Some((_, value)) => out.push_str(value),
            None => {
                out.push('{');
                out.push_str(name);
                out.push('}');
            }
        }
        rest = &after[close + 1..];
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compiled_default_parses_and_validates() {
        let prompts = CompactionPrompts::compiled_default();
        assert_eq!(prompts.source(), "default");
        assert_eq!(prompts.meta.version, 1);
        assert_eq!(prompts.fingerprint().len(), 64);
    }

    #[test]
    fn body_substitutes_every_placeholder() {
        let prompts = CompactionPrompts::compiled_default();
        let rendered = prompts.body(&BodyValues {
            snapshot_usage_pct: 91.6,
            snapshot_token_count: 183_200,
            snapshot_max_tokens: 200_000,
            remaining_budget: 16_800,
            context_inventory: "| row | 1 | 0.5% |",
            auto_approve_tools: false,
            summary_budget_tokens: 3_000,
        });
        for token in BODY_PLACEHOLDERS {
            assert!(
                !rendered.contains(&format!("{{{token}}}")),
                "`{{{token}}}` survived substitution"
            );
        }
        assert!(
            rendered.contains("at 92% capacity (183200 / 200000 tokens, 16800 tokens remaining)")
        );
        assert!(rendered.contains("| row | 1 | 0.5% |"));
        assert!(rendered.contains("AUTO-APPROVE OFF"));
        assert!(rendered.contains("~3000 tokens"));
        assert!(!rendered.contains('{'), "a stray `{{` survived: {rendered}");
    }

    #[test]
    fn budget_directive_states_the_budget() {
        let prompts = CompactionPrompts::compiled_default();
        let directive = prompts.budget_directive(3_000);
        assert!(directive.contains("~3000 tokens"));
        assert!(directive.contains("FIRST TO GO"));
    }

    #[test]
    fn continuation_bodies_keep_their_sentinels() {
        let prompts = CompactionPrompts::compiled_default();
        let fun = prompts.continuation_body(CompactionKind::Regular, false);
        assert!(fun.contains("POST-COMPACTION PROTOCOL"));
        let silent = prompts.continuation_body(CompactionKind::Regular, true);
        assert!(silent.contains("Silently continue"));
        assert!(!silent.contains("POST-COMPACTION PROTOCOL"));
        assert!(
            prompts
                .approval_tail()
                .contains("Tool approval is REQUIRED")
        );
        assert!(
            prompts
                .session_recovery(PlanRecovery::Active)
                .contains("SESSION RECOVERY")
        );
        assert_eq!(
            prompts.skill_stamp("grafana, session_search"),
            "\n\nSKILLS LOADED PRE-COMPACTION: grafana, session_search. Session focus may have \
             shifted — consider whether each is still relevant to the IMMEDIATE TASK; reload \
             only those that are."
        );
    }

    #[test]
    fn a_missing_body_placeholder_fails_validation() {
        let mut prompts = CompactionPrompts::compiled_default();
        let body = prompts.summariser.body.replace("{window}", "the window");
        prompts.summariser.body = body;
        let err = prompts
            .validate()
            .expect_err("a dropped placeholder must be rejected");
        assert!(
            err.contains("window"),
            "the error must name the token: {err}"
        );
    }

    #[test]
    fn an_extra_body_placeholder_fails_validation() {
        let mut prompts = CompactionPrompts::compiled_default();
        let mut body = prompts.summariser.body.clone();
        body.push_str(" {surprise}");
        prompts.summariser.body = body;
        let err = prompts
            .validate()
            .expect_err("an extra placeholder must be rejected");
        assert!(
            err.contains("surprise"),
            "the error must name the token: {err}"
        );
    }

    #[test]
    fn a_format_spec_placeholder_fails_validation() {
        let mut prompts = CompactionPrompts::compiled_default();
        let body = prompts
            .summariser
            .body
            .replace("{snapshot_usage_pct}", "{snapshot_usage_pct:.0}");
        prompts.summariser.body = body;
        let err = prompts
            .validate()
            .expect_err("a format spec must be rejected");
        assert!(err.contains("format-spec"), "got: {err}");
    }

    #[test]
    fn load_falls_back_when_the_override_is_absent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let prompts = CompactionPrompts::load(dir.path());
        assert_eq!(prompts.source(), "default");
        assert_eq!(prompts.meta.version, 1);
    }

    #[test]
    fn load_accepts_a_valid_override_and_rejects_a_broken_one() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(dir.path().join("prompts")).expect("mkdir");

        std::fs::write(
            dir.path().join("prompts/compaction.toml"),
            DEFAULT_PROMPT_TOML,
        )
        .expect("write");
        let prompts = CompactionPrompts::load(dir.path());
        assert_eq!(prompts.source(), "file");
        assert_eq!(
            prompts.fingerprint(),
            CompactionPrompts::compiled_default().fingerprint()
        );

        std::fs::write(
            dir.path().join("prompts/compaction.toml"),
            "[meta]\nversion = 1\n",
        )
        .expect("write");
        let prompts = CompactionPrompts::load(dir.path());
        assert_eq!(
            prompts.source(),
            "default",
            "an incomplete override must fall back, not panic"
        );
    }
}
