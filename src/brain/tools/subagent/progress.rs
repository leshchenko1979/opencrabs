//! Sub-agent progress tools (#852).
//!
//! Two halves of one channel. [`ReportProgressTool`] is what the CHILD calls to
//! append a line to its own ledger; [`AgentProgressTool`] is what the PARENT
//! calls to read it back while the child is still running. Before this, a
//! parent's only view was `WorkStatus.progress` — one overwritten snapshot — so
//! it could see what the agent was doing and never what it had done.
//!
//! Neither tool is a state source (D3): `WorkStatus.state` still decides
//! liveness, and nothing here is read to answer "is this agent alive?".

use super::ledger::{self, LedgerEntry, LedgerKind};
use super::manager::{ResolvedAgent, SubAgentManager, SubAgentState};
use crate::brain::tools::error::{Result, ToolError};
use crate::brain::tools::r#trait::{
    Tool, ToolCapability, ToolExecutionContext, ToolHints, ToolResult,
};
use async_trait::async_trait;
use serde_json::Value;
use std::sync::Arc;

/// How many entries `agent_progress` returns when the caller names no limit.
const DEFAULT_LIMIT: usize = 20;
/// Upper bound on one read. A parent that wants the whole history of a
/// long-lived agent is asking for a transcript, not a status check.
const MAX_LIMIT: usize = 200;

/// Human label for a ledger kind, padded so the text column lines up.
fn kind_label(kind: LedgerKind) -> &'static str {
    match kind {
        LedgerKind::Note => "note     ",
        LedgerKind::Decision => "decision ",
        LedgerKind::Blocker => "blocker  ",
        LedgerKind::Question => "question ",
    }
}

/// One word for the agent's state, as the parent should read it.
fn state_label(state: &SubAgentState) -> String {
    match state {
        SubAgentState::Running => "running".to_string(),
        SubAgentState::AwaitingInput => "awaiting input".to_string(),
        SubAgentState::Completed => "completed".to_string(),
        SubAgentState::Failed(why) => format!("failed: {why}"),
        SubAgentState::Cancelled => "cancelled".to_string(),
    }
}

/// How long ago the agent spawned, in the coarsest useful unit.
fn elapsed_label(spawned_at: &str) -> String {
    let Ok(start) = chrono::DateTime::parse_from_rfc3339(spawned_at) else {
        return "?".to_string();
    };
    let secs = (chrono::Utc::now() - start.with_timezone(&chrono::Utc))
        .num_seconds()
        .max(0);
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m{:02}s", secs / 60, secs % 60)
    } else {
        format!("{}h{:02}m", secs / 3600, (secs % 3600) / 60)
    }
}

/// `HH:MM:SS` of an entry's own stamp. Falls back to the raw string rather
/// than dropping the entry: a ledger line with an odd timestamp is still a
/// fact the parent wants.
fn clock(ts: &str) -> String {
    chrono::DateTime::parse_from_rfc3339(ts)
        .map(|dt| dt.format("%H:%M:%S").to_string())
        .unwrap_or_else(|_| ts.to_string())
}

/// Render a resolved agent's header + its last `limit` entries.
fn render(view: &ResolvedAgent, limit: usize) -> String {
    let mut out = format!(
        "{} · {} · {} · {}\n",
        view.id,
        view.label,
        state_label(&view.state),
        elapsed_label(&view.spawned_at)
    );
    let entries = ledger::read(&view.id, limit);
    if entries.is_empty() {
        out.push_str("no progress recorded yet");
        return out;
    }
    for entry in entries {
        out.push_str(&format!(
            "{}  {}  {}\n",
            clock(&entry.ts),
            kind_label(entry.kind),
            entry.text
        ));
    }
    out
}

/// Child-side tool: append one line to the caller's own progress ledger.
pub struct ReportProgressTool {
    manager: Arc<SubAgentManager>,
}

impl ReportProgressTool {
    pub fn new(manager: Arc<SubAgentManager>) -> Self {
        Self { manager }
    }
}

#[async_trait]
impl Tool for ReportProgressTool {
    fn name(&self) -> &str {
        "report_progress"
    }

    fn description(&self) -> &str {
        "Record one line of progress for yourself, visible to the parent that \
         spawned you while you are still working. Write one when a stage \
         lands, when you change approach (and why), and before you do \
         something expensive or irreversible — a gate dispatch, a push, a \
         build. If your last line is stale when you write your final message, \
         add one first: the silent tail of a long run is the part a parent \
         most needs and can least see. Keep each entry to one or two \
         sentences; this is a status line, not a report. A `blocker` or \
         `question` ALSO notifies the parent that you are stuck: record it, \
         then end your turn. The parent continues you with `resume_agent`, \
         which picks up in this same context."
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "kind": {
                    "type": "string",
                    "enum": ["note", "decision", "blocker", "question"],
                    "description": "note = what you are doing or found; decision = a choice and its reason; blocker = you cannot proceed until something changes; question = you need an answer to continue"
                },
                "text": {
                    "type": "string",
                    "description": "One or two sentences. Newlines are flattened and long text is truncated."
                }
            },
            "required": ["kind", "text"]
        })
    }

    fn capabilities(&self) -> Vec<ToolCapability> {
        vec![]
    }

    /// Read-only from the registry's point of view, and deliberately so: a
    /// `read_only` child has its mutating tools stripped by
    /// `restrict_registry_to_read_only`, and losing its only channel to the
    /// parent would be the worst possible restriction — a read-only child is
    /// exactly the one that cannot otherwise show what it found.
    fn hints(&self) -> ToolHints {
        ToolHints {
            read_only: true,
            destructive: false,
            idempotent: false,
            open_world: false,
        }
    }

    fn requires_approval(&self) -> bool {
        false
    }

    async fn execute(&self, input: Value, context: &ToolExecutionContext) -> Result<ToolResult> {
        let raw_kind = input
            .get("kind")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidInput("'kind' is required".into()))?;
        let kind: LedgerKind = serde_json::from_value(Value::String(raw_kind.to_string()))
            .map_err(|_| {
                ToolError::InvalidInput(format!(
                    "'kind' must be one of note, decision, blocker, question \
                     (got '{raw_kind}')"
                ))
            })?;

        let text = input
            .get("text")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .ok_or_else(|| ToolError::InvalidInput("'text' is required".into()))?;

        // A root session has no agent id, and inventing one would write a
        // ledger nothing ever reads. Refuse loudly instead: the model that
        // called this as a root has misunderstood where it is running.
        let Some(id) = self.manager.find_by_session(context.session_id) else {
            return Err(ToolError::InvalidInput(
                "report_progress is only available to a sub-agent session".into(),
            ));
        };

        ledger::append(&id, &LedgerEntry::new(kind, text));

        // #859: a blocker or a question is the child saying it cannot move
        // without the parent, and until now the ledger was write-only from its
        // side — the parent found out when it next happened to look, which for
        // a parked child is never. Notify on the two kinds that mean "stuck",
        // never on note/decision: those are progress, and waking a parent for
        // progress is the noise that makes it stop reading.
        if matches!(kind, LedgerKind::Blocker | LedgerKind::Question)
            && let Some(parent) = self.manager.get_parent_session_id(&id)
            && self.manager.attention_allowed(&id)
        {
            let label = self.manager.get_label(&id).unwrap_or_else(|| id.clone());
            // `false`: this child is about to END its turn, so it completes
            // rather than parking — the parent must be pointed at
            // `resume_agent`, which is the only verb that accepts a completed
            // agent (#984).
            super::spawn::push_attention(parent, &label, &id, kind.as_str(), text, false);
        }

        Ok(ToolResult::success(format!(
            "recorded ({}) on agent {id}",
            kind.as_str()
        )))
    }
}

/// Parent-side tool: read a child's progress ledger without blocking.
pub struct AgentProgressTool {
    manager: Arc<SubAgentManager>,
}

impl AgentProgressTool {
    pub fn new(manager: Arc<SubAgentManager>) -> Self {
        Self { manager }
    }
}

#[async_trait]
impl Tool for AgentProgressTool {
    fn name(&self) -> &str {
        "agent_progress"
    }

    fn description(&self) -> &str {
        "Read a sub-agent's progress history without waiting for it to finish. \
         Returns the agent's state and its most recent recorded entries, oldest \
         first. Use it to check on a long-running agent, or to see the blockers \
         and questions it recorded. This is the normal read-back while it works: \
         its completion reaches you as a fresh turn, so do not wait_agent for it. \
         To answer a question it recorded: `send_input <id>` while it is still \
         running or parked mid-round, `resume_agent <id>` when it ended its turn \
         to ask — a finished turn is COMPLETED, and send_input refuses those."
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "agent_id": {
                    "type": "string",
                    "description": "The ID returned by spawn_agent"
                },
                "limit": {
                    "type": "integer",
                    "description": "How many of the most recent entries to return (default 20, max 200)",
                    "default": DEFAULT_LIMIT
                }
            },
            "required": ["agent_id"]
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

    fn requires_approval(&self) -> bool {
        false
    }

    async fn execute(&self, input: Value, context: &ToolExecutionContext) -> Result<ToolResult> {
        let agent_id = input
            .get("agent_id")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| ToolError::InvalidInput("'agent_id' is required".into()))?;

        let limit = input
            .get("limit")
            .and_then(|v| v.as_u64())
            .map(|n| (n as usize).clamp(1, MAX_LIMIT))
            .unwrap_or(DEFAULT_LIMIT);

        // Scoped to the caller's own children, and the scope is the point: the
        // manager is process-global, so an unscoped read would hand one chat's
        // agent ids to another's model (#191).
        let Some(view) = self.manager.resolve(agent_id, context.session_id) else {
            return Ok(ToolResult::error(format!(
                "no agent '{agent_id}' belongs to this session — check the id with tasks_list"
            )));
        };

        Ok(ToolResult::success(render(&view, limit)))
    }
}
