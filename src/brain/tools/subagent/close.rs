//! close_agent tool — terminates a running child agent.

use super::manager::{SubAgentManager, SubAgentState};
use crate::brain::tools::error::{Result, ToolError};
use crate::brain::tools::r#trait::{Tool, ToolCapability, ToolExecutionContext, ToolResult};
use async_trait::async_trait;
use serde_json::Value;
use std::sync::Arc;

/// Tool that closes/cancels a spawned sub-agent.
pub struct CloseAgentTool {
    manager: Arc<SubAgentManager>,
}

impl CloseAgentTool {
    pub fn new(manager: Arc<SubAgentManager>) -> Self {
        Self { manager }
    }
}

#[async_trait]
impl Tool for CloseAgentTool {
    fn name(&self) -> &str {
        "close_agent"
    }

    fn description(&self) -> &str {
        "Stop a sub-agent that is still running or parked waiting for input, and \
         optionally drop it from tracking (`remove: true`) so its id stops \
         resolving. An agent that already completed or failed is left untouched \
         — there is nothing to cancel, and the result says so. Use this when a \
         sub-agent's work is no longer needed."
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "agent_id": {
                    "type": "string",
                    "description": "The ID of the sub-agent to close"
                },
                "remove": {
                    "type": "boolean",
                    "description": "Also remove the agent from tracking (default: false)",
                    "default": false
                }
            },
            "required": ["agent_id"]
        })
    }

    fn capabilities(&self) -> Vec<ToolCapability> {
        vec![]
    }

    fn requires_approval(&self) -> bool {
        false
    }

    async fn execute(&self, input: Value, _context: &ToolExecutionContext) -> Result<ToolResult> {
        let agent_id = input
            .get("agent_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidInput("'agent_id' is required".into()))?;

        let remove = input
            .get("remove")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        if !self.manager.exists(agent_id) {
            return Ok(ToolResult::error(format!(
                "No sub-agent found with id: {}",
                agent_id
            )));
        }

        let state = self.manager.get_state(agent_id);

        // Cancel if running or paused waiting for input. Anything already
        // finished is left alone — and must SAY so (#921): reporting
        // "cancelled" for an agent that had already completed claims an action
        // that never happened, which reads to the caller as "I stopped live
        // work" when in truth nothing was running.
        let was_live = matches!(
            state,
            Some(SubAgentState::Running) | Some(SubAgentState::AwaitingInput)
        );
        if was_live {
            self.manager.cancel(agent_id);
        }

        // `remove` is independent of cancellation: it drops the record so the
        // id stops resolving, whether or not anything was live to stop.
        if remove {
            self.manager.remove(agent_id);
        }

        let status = match (was_live, remove) {
            (true, true) => "cancelled and removed from tracking",
            (true, false) => "cancelled",
            (false, true) => "was already finished — removed from tracking",
            (false, false) => "was already finished — nothing to cancel",
        };

        Ok(ToolResult::success(format!(
            "Sub-agent {} {}.",
            agent_id, status
        )))
    }
}
