//! Goal system — autonomous task completion loop.
//!
//! A goal is a free-form user objective that persists across turns. After
//! each turn completes, a lightweight judge call asks the same LLM "is
//! this goal satisfied?". If not, a continuation prompt is injected and
//! the tool loop re-enters. The loop is bounded by a turn budget
//! (default 20) and auto-pauses on consecutive judge parse failures.
//!
//! #299 split the evaluation into pieces that all fail in the same direction
//! (never toward "done"):
//!
//! - [`criteria`] derives the goal's checkable criteria once, on the first
//!   evaluation, so the judge evaluates a fixed list instead of re-imagining the
//!   goal every turn.
//! - [`evidence`] collects the turn's mechanical facts (detached commands still
//!   running, plan tasks still open) into a pack the judge must reason from.
//! - [`judge`] evaluates each criterion against that pack; the authoritative
//!   verdict is aggregated in Rust by `types::aggregate_verdict`.

pub mod criteria;
pub mod evidence;
pub mod judge;
pub mod manager;
pub mod prompt;
pub mod types;

pub use manager::GoalManager;
pub use prompt::{goal_command_prompt, goal_usage_warning, is_bare};
pub use types::{GoalDecision, GoalVerdict, JudgeDecision};

/// Concatenate the text blocks of an LLM response.
///
/// Shared by every model call in this subsystem (criteria derivation and the
/// judge) so the two can never drift on what counts as the model's reply.
pub(crate) fn extract_text(response: &crate::brain::provider::LLMResponse) -> String {
    let mut text = String::new();
    for block in &response.content {
        if let crate::brain::provider::ContentBlock::Text { text: t } = block {
            text.push_str(t);
        }
    }
    text
}
