//! Team Orchestration — named groups of sub-agents with batch operations.
//!
//! Provides 3 tools: team_create, team_delete, team_broadcast.
//! Teams are tracked by TeamManager; individual agents still live in SubAgentManager.

mod broadcast;
mod create;
mod delete;
pub mod manager;

pub use broadcast::TeamBroadcastTool;
pub use create::TeamCreateTool;
// #1064: the placement decision is a pure function so the isolation a team
// member receives can be asserted without standing up a provider and a daemon.
pub(crate) use create::{member_report_line, member_workspace};
pub use delete::TeamDeleteTool;
pub use manager::TeamManager;
