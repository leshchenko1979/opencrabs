//! Tasks List Tool (#1160)
//!
//! One agent-facing roster of BOTH background systems: spawned sub-agents
//! (previously visible only through wait_agent's error path) and detached
//! bash commands (previously invisible to the model entirely). Both managers
//! already ride in `ToolExecutionContext`, so this tool only reads.

use super::error::Result;
use super::r#trait::{Tool, ToolCapability, ToolExecutionContext, ToolHints, ToolResult};
use async_trait::async_trait;
use serde_json::Value;

/// Tool listing in-flight sub-agents and detached commands.
#[derive(Default)]
pub struct TasksListTool;

impl TasksListTool {
    pub fn new() -> Self {
        Self
    }
}

/// One sub-agent roster row, render-ready.
pub(crate) struct SubagentRow {
    pub id: String,
    pub label: String,
    pub state: String,
    /// Path of the agent's JSON status file, as built by
    /// [`subagent_status_file`], so the model can read live progress directly.
    pub status_file: Option<String>,
}

/// Advertised status-file path for a sub-agent — the same path writers use.
///
/// Resolved through [`crate::brain::agent::service::work_status`], the single
/// entry point every writer persists through (`subagent/spawn.rs` →
/// `work_status::status_path`). Extracted from `execute()` so that agreement
/// is expressed once and can be pinned by a test, rather than recomputed
/// inline where any drift in the resolver would silently hand the model a
/// path that reads ENOENT. An empty read from a wrong path is
/// indistinguishable from "this sub-agent never existed".
pub(crate) fn subagent_status_file(id: &str) -> String {
    crate::brain::agent::service::work_status::status_path(id)
        .display()
        .to_string()
}

/// One detached-command roster row.
///
/// Carries the run's address, not just its label: the id is what `task_output`
/// and `task_wait` take, the pgid is what stop it (owner directive 2026-09-29 —
/// the harness reports the number, the skill signals it), and the two paths are
/// where the live
/// streams are. A row with a label alone told the model WHAT was running but
/// left it no way to look at it or stop it (#692).
pub(crate) struct DetachedRow {
    pub id: String,
    pub label: String,
    pub elapsed_secs: u64,
    /// The run's process-GROUP id, when the platform gave us one. This is the
    /// stop handle: `setsid` made the child a leader, so `kill -- -<pgid>`
    /// reaps the whole tree, including a descendant that re-parented away.
    pub pid: Option<u32>,
    /// Live stdout capture. Present from spawn — a run that has produced no
    /// output yet still has an empty file, which is distinguishable from a run
    /// that never existed.
    pub output_out: Option<String>,
    /// Live stderr capture.
    pub output_err: Option<String>,
    /// The run's JSON status file.
    pub status_file: Option<String>,
}

/// Pure renderer so tests pin output shape without live managers.
///
/// Empty everything renders the explicit "No background tasks." line — the
/// model must never have to infer emptiness from absent sections.
pub(crate) fn render_tasks(subagents: &[SubagentRow], detached: &[DetachedRow]) -> String {
    if subagents.is_empty() && detached.is_empty() {
        return "No background tasks.".to_string();
    }
    let mut out = String::from("Background tasks:");
    if !subagents.is_empty() {
        out.push_str(&format!("\n\nSub-agents ({}):", subagents.len()));
        for a in subagents {
            out.push_str(&format!("\n- {} [{}] {}", a.id, a.label, a.state));
            if let Some(sf) = &a.status_file {
                out.push_str(&format!("\n  status file: {sf}"));
            }
        }
    }
    if !detached.is_empty() {
        out.push_str(&format!("\n\nDetached commands ({}):", detached.len()));
        for d in detached {
            out.push_str(&format!("\n- {} [{}] {}s", d.id, d.label, d.elapsed_secs));
            if let Some(p) = d.pid {
                out.push_str(&format!("\n  pgid: {p}"));
            }
            if let Some(f) = &d.output_out {
                out.push_str(&format!("\n  stdout: {f}"));
            }
            if let Some(f) = &d.output_err {
                out.push_str(&format!("\n  stderr: {f}"));
            }
            if let Some(f) = &d.status_file {
                out.push_str(&format!("\n  status file: {f}"));
            }
        }
    }
    out
}

fn state_label(state: &super::subagent::manager::SubAgentState) -> &'static str {
    use super::subagent::manager::SubAgentState as S;
    match state {
        S::Running => "running",
        S::AwaitingInput => "awaiting input",
        S::Completed => "completed",
        S::Failed(_) => "failed",
        S::Cancelled => "cancelled",
    }
}

#[async_trait]
impl Tool for TasksListTool {
    fn name(&self) -> &str {
        "tasks_list"
    }

    fn description(&self) -> &str {
        "List in-flight background work: spawned sub-agents (id, label, \
         state, status-file path) and detached shell commands (run id, label, \
         elapsed, stdout path, stderr path, status-file path). Read-only. Use \
         it to check what is running instead of re-spawning or busy-waiting; \
         results are pushed to you on completion either way. The run id works \
         with task_output (read live output) and task_wait (block on it); the \
         pgid is what you signal to stop one."
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {},
            "required": []
        })
    }

    fn capabilities(&self) -> Vec<ToolCapability> {
        vec![ToolCapability::ReadFiles]
    }

    fn hints(&self) -> ToolHints {
        ToolHints {
            read_only: true,
            destructive: false,
            idempotent: true,
            open_world: false,
        }
    }

    async fn execute(&self, _input: Value, context: &ToolExecutionContext) -> Result<ToolResult> {
        let mut subagents = Vec::new();
        if let Some(mgr) = context.subagent_manager.as_ref() {
            // Scoped to the CALLER's children (#191). The manager is
            // process-global (one instance per channel factory), so an
            // unfiltered `list()` reported other sessions' sub-agents as this
            // caller's in-flight work — and this tool's framing ("do not
            // spawn duplicates") makes that a silent suppressor, not just
            // noise. The detached half below is already scoped the same way.
            for (id, label, state) in mgr.list_for_parent(context.session_id) {
                let status_file = subagent_status_file(&id);
                subagents.push(SubagentRow {
                    id,
                    label,
                    state: state_label(&state).to_string(),
                    status_file: Some(status_file),
                });
            }
        }

        let mut detached = Vec::new();
        if let Some(bm) = context.background_manager.as_ref() {
            // Read the HANDLES, not the label-only roster: the id and the two
            // capture paths are the whole point of the row (#692), and
            // `running_tasks` predates them.
            for h in bm.handles_for(context.session_id) {
                detached.push(DetachedRow {
                    id: h.id.clone(),
                    label: h.label,
                    elapsed_secs: h.started.elapsed().as_secs(),
                    pid: h.pid,
                    output_out: Some(h.output_out.display().to_string()),
                    output_err: Some(h.output_err.display().to_string()),
                    // Same resolver every writer persists through, called
                    // directly rather than via the sub-agent helper: a run's
                    // status file and a sub-agent's share a directory and a
                    // naming scheme, but only one of them is a sub-agent, and
                    // borrowing that name here would imply otherwise.
                    status_file: Some(
                        crate::brain::agent::service::work_status::status_path(&h.id)
                            .display()
                            .to_string(),
                    ),
                });
            }
        }

        Ok(ToolResult::success(render_tasks(&subagents, &detached)))
    }
}
