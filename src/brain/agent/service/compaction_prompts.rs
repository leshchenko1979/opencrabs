//! Post-compaction continuation prompts.
//!
//! When the loop auto-compacts a session it appends a system message
//! that tells the model how to resume. There are two emission modes:
//!
//! - **Fun** (default): the original POST-COMPACTION PROTOCOL prompts.
//!   They explicitly invite the model to drop a brief, in-character
//!   one-liner ("brain refresh — what was I doing?") before continuing.
//!   Users have flagged these moments as a delight feature — emergent
//!   personality lines, sometimes per-language (e.g. Russian мат in
//!   frustration moments), generate organic shareable screenshots that
//!   read as "this thing has a soul."
//!
//! - **Silent**: tells the model to resume without acknowledging the
//!   compaction at all. Right for formal / corporate / customer-facing
//!   deployments where dropping mid-session profanity is inappropriate.
//!   Selected by setting `[agent] silent_compaction = true` in
//!   `~/.opencrabs/config.toml`.
//!
//! Why a dedicated module: tool_loop.rs already carries the four
//! compaction sites + a lot of other state. Inlining the prompt
//! strings inflates each site and forces every prompt edit to touch
//! the loop. Centralising here keeps both variants side-by-side so
//! anyone editing the fun version is one screen away from the silent
//! mirror — they cannot accidentally drift apart.
//!
//! #929: the text itself no longer lives in this file. It is data now —
//! `prompts/compaction.default.toml` compiled into the binary, overridable
//! per profile at `<profile_home>/prompts/compaction.toml`, loaded and
//! validated by [`super::prompt_file`]. What stays here is the behaviour:
//! the [`CompactionKind`] matrix, the plan-state branch, and the order the
//! pieces are concatenated in. The two `_with` variants take an
//! already-loaded [`CompactionPrompts`]; the public entry points load one
//! themselves so their long-standing signatures (pinned by tests) hold.

use super::prompt_file::CompactionPrompts;

/// Which compaction trigger fired. Each kind has a slightly different
/// recovery story (mid-loop edits vs. provider 4xx vs. async budget),
/// so the prompts differ in detail even though the shape is the same.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactionKind {
    /// Async budget compaction that runs before the next turn starts.
    /// The model is not mid-tool-call; it just needs to know the older
    /// history was summarised.
    Regular,
    /// Compaction that fired between tool iterations inside the same
    /// turn. The model may have had a file open / mid-edit; the prompt
    /// reassures that re-reading is fine.
    MidLoop,
    /// Provider rejected the prompt with a context-length 4xx. The
    /// summary above is the recovered state. Used to be the home of
    /// the most-shared one-liner moments, so the fun variant keeps the
    /// "cursing allowed" energy explicit.
    Emergency,
    /// Compaction that fired after a tool call completed, with the
    /// model about to receive the result. Same shape as MidLoop but
    /// the recovered state already includes the tool result.
    PostTool,
    /// User-triggered `/compact` command. Brief continuation — the
    /// user explicitly asked for compaction, so the agent just needs
    /// a short "resume from summary" hint, not the full protocol.
    Manual,
}

/// Build the continuation text appended after a compaction marker.
///
/// `silent`: mirrors `AgentConfig.silent_compaction`. `true` selects
/// the silent variant; `false` (the default) selects the fun variant.
///
/// `auto_approve`: when false, we tail the prompt with the standard
/// tool-approval reminder so the model does not batch tool calls
/// after a fresh context.
///
/// Session plan-mode state relevant to compaction recovery: which (if
/// any) plan-recovery hint the continuation carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanRecovery {
    /// No plan artifacts: omit plan recovery entirely.
    NoPlan,
    /// Editing (pre-init or design prose): re-read the `.md`, never
    /// `start`, never edit project files.
    Editing,
    /// Active checklist: `plan start` (no args) resurfaces the task.
    Active,
}

impl PlanRecovery {
    /// Derive from the session's plan files.
    pub async fn for_session(session_id: uuid::Uuid) -> Self {
        use crate::utils::plan_files::{PlanModeState, plan_mode_state};
        match plan_mode_state(session_id).await {
            PlanModeState::NoPlan => PlanRecovery::NoPlan,
            PlanModeState::PreInitEditing | PlanModeState::PostInitEditing => PlanRecovery::Editing,
            PlanModeState::Active => PlanRecovery::Active,
        }
    }
}

/// `plan_recovery`: branches the recovery hint by the session's plan
/// state. Telling an Editing session to `plan start` would corrupt the
/// design track, and a NoPlan session gets no plan chatter at all.
///
/// The text itself lives in [`CompactionPrompts`] (#929) — the profile's
/// `prompts/compaction.toml` when it loads, the compiled-in default
/// otherwise. This four-argument shape is pinned by two source-level
/// guards: `compaction_prompts_test.rs` calls it directly, and
/// `compaction_single_continuation_path_test.rs` requires the loop to
/// reach it exactly once — so the load happens here rather than at the
/// call site, and `continuation_prompt` keeps its single call.
pub fn build_continuation(
    kind: CompactionKind,
    silent: bool,
    auto_approve: bool,
    plan_recovery: PlanRecovery,
) -> String {
    let prompts = CompactionPrompts::load(&crate::config::profile::resolve_profile_home());
    build_continuation_with(&prompts, kind, silent, auto_approve, plan_recovery)
}

/// [`build_continuation`] against an already-loaded prompt set.
///
/// The continuation body, then section 0's status gate, then the
/// session-recovery hint, then — when approval is required — the
/// tool-approval reminder. Order is load-bearing: the status gate must
/// follow the body so a finished obligation is never re-blessed (#499).
pub(crate) fn build_continuation_with(
    prompts: &CompactionPrompts,
    kind: CompactionKind,
    silent: bool,
    auto_approve: bool,
    plan_recovery: PlanRecovery,
) -> String {
    let mut text = prompts.continuation_body(kind, silent).to_string();
    // Issue #499: section 0 carries an explicit status; every body
    // defers to it, so a finished obligation is never re-blessed.
    text.push_str(prompts.obligation_status_rule());
    // Session-recovery hint: applies to ALL variants (fun + silent).
    // Branches on plan state; the coding-standards hint rides along.
    text.push_str(prompts.session_recovery(plan_recovery));
    if !auto_approve {
        text.push_str(prompts.approval_tail());
    }
    text
}

/// Advisory skill-inventory stamp (#125): names the skills that were
/// active (slash-invoked) in this session before the compaction, framed
/// as CONSIDER-not-RELOAD — the session's focus may have shifted while
/// the context was compacting, so the waking agent weighs each skill's
/// relevance against the IMMEDIATE TASK instead of blindly re-reading
/// bodies it may no longer need. An empty slice produces NO stamp at
/// all: zero marginal tokens for sessions that never touched a skill.
///
/// The list rides the continuation prompt (mechanically composed from
/// in-memory session state at build time), so it is immune to the
/// summarizer's token-budget drops — the durable-state property the
/// research phase identified as the gap.
///
/// The stamp template lives in [`CompactionPrompts`] (#929), keyed
/// `continuation.shared.skill_stamp`; an empty slice skips the load
/// entirely.
pub fn append_skill_stamp(text: String, active_skills: &[String]) -> String {
    if active_skills.is_empty() {
        return text;
    }
    let prompts = CompactionPrompts::load(&crate::config::profile::resolve_profile_home());
    append_skill_stamp_with(&prompts, text, active_skills)
}

/// [`append_skill_stamp`] against an already-loaded prompt set.
pub(crate) fn append_skill_stamp_with(
    prompts: &CompactionPrompts,
    mut text: String,
    active_skills: &[String],
) -> String {
    if active_skills.is_empty() {
        return text;
    }
    let mut names: Vec<&str> = active_skills.iter().map(|s| s.as_str()).collect();
    names.sort();
    text.push_str(&prompts.skill_stamp(&names.join(", ")));
    text
}
