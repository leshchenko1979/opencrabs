//! Sub-Agent Spawning Tools
//!
//! Provides 5 tools for multi-agent orchestration inspired by codex-rs:
//! spawn_agent, wait_agent, send_input, close_agent, resume_agent.
//!
//! Each child agent gets a forked context, its own CancellationToken,
//! and runs in a background tokio task. The parent can wait, send input,
//! close, or resume any child by agent_id.

pub mod agent_type;
pub mod brain;
mod close;
pub mod manager;
pub(crate) mod notify;
pub(crate) mod provider_pair;
pub mod reconcile;
mod resume;
mod send_input;
pub mod spawn;
pub mod status;
pub mod team;
mod wait;
pub(crate) mod worktree;

pub use agent_type::ALWAYS_EXCLUDED;
pub use agent_type::{build_child_registry, map_deprecated_agent_type};
pub use close::CloseAgentTool;
pub use manager::{SubAgent, SubAgentManager, SubAgentState};
pub use notify::SessionNotifyTool;
pub use resume::ResumeAgentTool;
pub use send_input::SendInputTool;
pub use spawn::SpawnAgentTool;
pub use status::ProgressSnapshot;
pub use team::{TeamBroadcastTool, TeamCreateTool, TeamDeleteTool, TeamManager};
pub use wait::WaitAgentTool;

/// Spawn label for the plan-review worker (#155). One constant shared by the
/// spawn path (which grants this label the single write exception to the
/// Editing-parent read-only rule) and the Telegram plan card (which sends it),
/// so the grant and its caller can never drift apart.
pub(crate) const PLAN_REVIEW_LABEL: &str = "plan-review";
