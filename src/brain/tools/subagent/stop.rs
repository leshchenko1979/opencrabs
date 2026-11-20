//! stop_agent tool — stops a running or parked child agent.

use super::manager::SubAgentManager;
use crate::brain::tools::error::{Result, ToolError};
use crate::brain::tools::r#trait::{Tool, ToolCapability, ToolExecutionContext, ToolResult};
use async_trait::async_trait;
use serde_json::Value;
use std::sync::Arc;

/// Tool that stops a spawned sub-agent without deleting it.
pub struct StopAgentTool {
    manager: Arc<SubAgentManager>,
}

impl StopAgentTool {
    pub fn new(manager: Arc<SubAgentManager>) -> Self {
        Self { manager }
    }
}

#[async_trait]
impl Tool for StopAgentTool {
    fn name(&self) -> &str {
        "stop_agent"
    }

    fn description(&self) -> &str {
        "Stop a sub-agent that is still running or parked waiting for input. \
         The agent keeps its id, its status file and its ledger — stopping work \
         is not deleting the agent — so resume_agent can continue it later with \
         a new prompt. An agent that already completed, failed or was stopped is \
         left untouched and the result says so. Only the caller's OWN children \
         can be stopped; an id belonging to another session is refused. Use this \
         when a sub-agent's work is no longer needed."
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "agent_id": {
                    "type": "string",
                    "description": "The ID of the sub-agent to stop"
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

    async fn execute(&self, input: Value, context: &ToolExecutionContext) -> Result<ToolResult> {
        let agent_id = input
            .get("agent_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidInput("'agent_id' is required".into()))?;

        // Scope and existence in ONE gate: `resolve` answers only for the
        // caller's own children (#191), and rehydrates a disk-only agent the
        // same way `resume_agent` does. An unknown id and a stranger's id are
        // deliberately indistinguishable — telling them apart would leak the
        // existence of another session's child.
        if self.manager.resolve(agent_id, context.session_id).is_none() {
            return Ok(ToolResult::error(format!(
                "No sub-agent '{agent_id}' belongs to this session. Check the id \
                 with tasks_list; a detached shell command is stopped by its pgid, \
                 not here."
            )));
        }

        // `cancel()` IS the receipt: it takes the write lock, re-checks the
        // state itself and returns false when nothing was live. Reading the
        // state first and cancelling after would leave a window in which the
        // agent finishes between the two calls and the tool reports a stop
        // that never happened.
        if self.manager.cancel(agent_id) {
            Ok(ToolResult::success(format!(
                "Sub-agent {agent_id} stopped."
            )))
        } else {
            Ok(ToolResult::success(format!(
                "Sub-agent {agent_id} was already finished — nothing to stop."
            )))
        }
    }
}
