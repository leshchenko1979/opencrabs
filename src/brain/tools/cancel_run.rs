//! Cancel Run Tool (#692)
//!
//! Stop a detached run. Before #692 a detached command could not be signalled
//! at all: the manager held a label and a start time, no pid, and the process
//! had `setsid()`-ed into its own group — so a runaway build kept holding its
//! file locks with nothing able to reach it. The manager now records the pid,
//! and `setsid` makes that pid a process-GROUP id, so one `killpg` reaches the
//! whole tree including a descendant that re-parented to init.

use super::error::Result;
use super::r#trait::{Tool, ToolCapability, ToolExecutionContext, ToolHints, ToolResult};
use crate::brain::agent::service::work_status::{WorkState, WorkStatus, status_path};
use async_trait::async_trait;
use serde_json::Value;

/// How long to wait for the run to record its terminal state after the signal.
///
/// A signal is delivered, not obeyed: the process still has to die, and the
/// continuation task still has to write the status file. Polling briefly lets
/// the tool report what actually happened instead of reporting the intent to
/// make it happen — but it is BOUNDED, because a process ignoring SIGTERM must
/// not hold the agent's turn open.
const SETTLE_MS: u64 = 2000;

/// Signal a detached run's process group.
#[derive(Default)]
pub struct CancelRunTool;

impl CancelRunTool {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Tool for CancelRunTool {
    fn name(&self) -> &str {
        "cancel_run"
    }

    fn description(&self) -> &str {
        "Stop a detached shell run started by the bash tool. Takes the run id \
         from tasks_list or from the handover message. Signals the run's whole \
         process group, so grandchildren (the actual build or test processes) \
         are reached too. Reports whether the run recorded its terminal state; \
         a delivered signal that does not settle is reported as such rather \
         than as a completed cancellation."
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "run_id": {
                    "type": "string",
                    "description": "Run id of the detached run to stop, as shown by tasks_list."
                }
            },
            "required": ["run_id"]
        })
    }

    fn capabilities(&self) -> Vec<ToolCapability> {
        vec![ToolCapability::ReadFiles, ToolCapability::ExecuteShell]
    }

    fn hints(&self) -> ToolHints {
        ToolHints {
            read_only: false,
            destructive: true,
            idempotent: false,
            open_world: false,
        }
    }

    async fn execute(&self, input: Value, context: &ToolExecutionContext) -> Result<ToolResult> {
        let Some(run_id) = input.get("run_id").and_then(|v| v.as_str()) else {
            return Ok(ToolResult::error(
                "run_id is required (the id shown by tasks_list)".to_string(),
            ));
        };

        let Some(mgr) = context.background_manager.as_ref() else {
            return Ok(ToolResult::error(
                "No background task manager on this surface, so no runs are addressable.".to_string(),
            ));
        };

        // Scoped to the CALLER's own runs (#191). The manager is process-global,
        // so an unscoped cancel would let any session kill any other session's
        // work — and this tool is destructive, which makes that a real hazard
        // rather than noise.
        let Some(handle) = mgr
            .handles_for(context.session_id)
            .into_iter()
            .find(|h| h.id == run_id)
        else {
            return Ok(ToolResult::error(format!(
                "No live run {run_id} in this session. Detached runs are addressable \
                 only by the session that started them, and only while they are running."
            )));
        };

        let pid = handle.pid;
        let label = handle.label.clone();
        let status_file = status_path(run_id).display().to_string();

        if let Err(e) = mgr.cancel(run_id) {
            // A cancel the agent believes landed but did not is worse than no
            // cancel at all, so the refusal is surfaced, not swallowed.
            return Ok(ToolResult::error(format!(
                "Could not signal run {run_id} ({label}): {e}"
            )));
        }

        // Poll for the terminal record. Bytes, not assumptions: the run's own
        // status file is the authority on whether it stopped.
        let observed = settle(run_id).await;
        let mut out = format!("Signalled run {run_id} ({label})");
        if let Some(p) = pid {
            out.push_str(&format!(", process group {p}"));
        }
        out.push_str(".\n");
        match observed {
            Some(status) => {
                let code = status
                    .finish
                    .as_ref()
                    .and_then(|f| f.code)
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| "none recorded".to_string());
                out.push_str(&format!(
                    "State: {} — recorded exit code {code}.\nThe run's capture is complete at \
                     this point; read it from the paths in tasks_list.\n",
                    state_word(&status.state),
                ));
            }
            None => {
                out.push_str(
                    "The signal was delivered and the run did not record a terminal state \
                     within 2s. It may be ignoring SIGTERM (some builds trap it), or still \
                     tearing down. Re-run tasks_list to check its state; a run that stays \
                     live has not stopped.\n",
                );
            }
        }
        out.push_str(&format!("status file: {status_file}"));
        Ok(ToolResult::success(out))
    }
}

/// Word for a work state, for the tool's human-facing line.
fn state_word(state: &WorkState) -> &'static str {
    match state {
        WorkState::Pending => "pending",
        WorkState::Running => "running",
        WorkState::AwaitingInput => "awaiting input",
        WorkState::Completed => "completed",
        WorkState::Failed => "failed",
        WorkState::Interrupted => "interrupted",
    }
}

/// Wait briefly for a run to record a terminal state, then report what was seen.
///
/// Returns `None` on timeout rather than the last non-terminal record: "still
/// running" and "stopped but unrecorded" are different answers, and the caller
/// has a separate branch for each.
async fn settle(run_id: &str) -> Option<WorkStatus> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(SETTLE_MS);
    loop {
        if let Some(status) = WorkStatus::read(run_id)
            && status.state.is_terminal()
        {
            return Some(status);
        }
        if std::time::Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}
