//! Await External Tool — the WRITE path for #344.
//!
//! A lane that ends its turn parked on an EXTERNAL completion — a CI run, a
//! peer lane, the owner's design gate — records that wait here, in the very
//! turn it parks. The record is durable: it lives on the session's
//! `session_bindings` row, so it survives the daemon restart that would
//! otherwise leave the lane comatose. On restart `turn_open_at` is cleared, the
//! topic's last message is the bot's own, and the freshness gate
//! (`WAKE_RECENT_SECS`) stops considering the binding — three arms of
//! `classify_recently_active` all miss, and nothing wakes the lane again.
//!
//! This is the only part of #344 that costs a lane a habit. The restart case
//! (A1) is derivable from the pending-request journal and needs no declaration;
//! the external wait (A2) is NOT derivable — nothing in the database
//! distinguishes "parked on run 377" from "finished and idle" — so the lane
//! itself has to say so. A lane that never declares records nothing, and this
//! tool does not pretend otherwise.
//!
//! Two readers consume what this writes, both keyed on `await_at IS NOT NULL`:
//! the boot classifier's `awaiting` bucket (`resume.rs`), which resumes the
//! lane at startup, and the periodic sweep (`await_sweep.rs`), the backstop for
//! the wait whose completion never arrives at all.

use super::error::Result;
use super::r#trait::{Tool, ToolCapability, ToolExecutionContext, ToolHints, ToolResult};
use crate::db::SessionBindingRepository;
use async_trait::async_trait;
use serde_json::Value;

/// The kinds of external completion a lane may park on (#344).
///
/// A closed set, validated rather than stored verbatim: the kind is written
/// into the boot log and the sweep's log line, and free text there would make
/// both unreadable at exactly the moment someone is trying to work out why a
/// lane went quiet.
const AWAIT_KINDS: &[&str] = &["ci_run", "peer_lane", "owner_gate", "external"];

/// Declares, and clears, a lane's durable await record.
pub struct AwaitExternalTool {
    repo: SessionBindingRepository,
}

impl AwaitExternalTool {
    pub fn new(repo: SessionBindingRepository) -> Self {
        Self { repo }
    }
}

#[async_trait]
impl Tool for AwaitExternalTool {
    fn name(&self) -> &str {
        "await_external"
    }

    fn description(&self) -> &str {
        "Declare that you are ending your turn waiting on an EXTERNAL completion, so a \
         daemon restart — or a completion that never arrives — cannot leave you silently \
         comatose. Call 'set' in the SAME turn you park, naming what you wait on: a CI run \
         ('ci_run', ref = the run id), a peer lane ('peer_lane', ref = the lane), the owner's \
         approval ('owner_gate'), or anything else external ('external', ref = a short \
         description). Then end your turn — you will be resumed when the daemon next starts, \
         and the periodic sweep will re-check you if the wait runs long. Call 'clear' as soon \
         as the completion lands (or you abandon the wait) so a stale record does not wake you \
         later. This is durable state on your session binding, not a timer: it is exactly what \
         makes a parked lane recoverable across a restart."
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["set", "clear"],
                    "description": "'set' records that you are parking on an external completion; 'clear' removes the record once it has landed or you have stopped waiting."
                },
                "kind": {
                    "type": "string",
                    // One source of truth: the same const validates `execute`,
                    // so the advertised enum can never drift from the accepted set.
                    "enum": AWAIT_KINDS,
                    "description": "What you are waiting on. Required for 'set'. Use 'ci_run' for a workflow run, 'peer_lane' for another session/lane, 'owner_gate' for an approval only the human owner can give, 'external' for anything else."
                },
                "ref": {
                    "type": "string",
                    "description": "Optional, for 'set'. A short identifier for the thing you await — the run id, the lane name, the issue number. It is written into the boot and sweep logs so the wait is identifiable without guessing, so prefer setting it."
                }
            },
            "required": ["action"]
        })
    }

    fn capabilities(&self) -> Vec<ToolCapability> {
        vec![ToolCapability::SystemModification]
    }

    fn hints(&self) -> ToolHints {
        // Records session-local scheduling state. It modifies the database, so
        // it is not read-only — but it is additive rather than destructive,
        // and re-declaring a wait is exactly the intended way to switch from
        // one CI run to the next, so it is idempotent. Nothing external is
        // touched, so it is not open-world. This keeps it off the approval
        // gate, which matters: a tool a lane must call every time it parks
        // cannot cost an approval round-trip.
        ToolHints {
            read_only: false,
            destructive: false,
            idempotent: true,
            open_world: false,
        }
    }

    async fn execute(&self, input: Value, context: &ToolExecutionContext) -> Result<ToolResult> {
        let session_id = context.session_id.to_string();
        let action = input
            .get("action")
            .and_then(|v| v.as_str())
            .unwrap_or_default();

        match action {
            "set" => {
                let kind = input
                    .get("kind")
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .unwrap_or("");
                if kind.is_empty() {
                    return Ok(ToolResult::error(
                        "The 'set' action requires 'kind' — what you are waiting on \
                         (ci_run, peer_lane, owner_gate, external)."
                            .into(),
                    ));
                }
                if !AWAIT_KINDS.contains(&kind) {
                    return Ok(ToolResult::error(format!(
                        "Unknown kind '{kind}'. Valid: {}.",
                        AWAIT_KINDS.join(", ")
                    )));
                }
                let reference = input
                    .get("ref")
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty());

                match self.repo.set_await(&session_id, kind, reference).await {
                    // The UPDATE is keyed on `session_id`, so zero rows means
                    // there was no binding to write to — a CLI one-shot, a cron
                    // turn, a sub-agent. Reporting success there would be the
                    // precise lie #344 exists to remove: the lane would believe
                    // it is parked while nothing can ever wake it.
                    Ok(0) => Ok(ToolResult::error(format!(
                        "Await NOT recorded: this session has no binding row, so nothing \
                         would ever wake it. A restart or the periodic sweep resumes a lane \
                         by its session binding, and this session has none (a CLI one-shot, \
                         a cron turn and a sub-agent have no topic to be resumed into). Do \
                         not end your turn expecting to be resumed — report the external \
                         dependency to whoever is waiting on you instead."
                    ))),
                    Ok(_) => {
                        let what = match reference {
                            Some(r) => format!("{kind} '{r}'"),
                            None => kind.to_string(),
                        };
                        Ok(ToolResult::success(format!(
                            "⏳ Await recorded: {what}.\n\nThis is durable — it survives a \
                             daemon restart, where the boot classifier will resume you, and \
                             the periodic sweep will re-check you if the wait runs long. End \
                             your turn now. Call 'clear' as soon as the completion lands (or \
                             you stop waiting), so a stale record does not wake you later."
                        )))
                    }
                    Err(e) => Ok(ToolResult::error(format!(
                        "Failed to record the await: {e}"
                    ))),
                }
            }
            "clear" => match self.repo.clear_await(&session_id).await {
                Ok(0) => Ok(ToolResult::success(
                    "No await record to clear — this session had none (either it never \
                     declared one, or it has no binding row at all). Nothing was waiting on \
                     the record."
                        .into(),
                )),
                Ok(_) => Ok(ToolResult::success(
                    "Await record cleared. You will no longer be resumed for this wait.".into(),
                )),
                Err(e) => Ok(ToolResult::error(format!(
                    "Failed to clear the await record: {e}"
                ))),
            },
            other => Ok(ToolResult::error(format!(
                "Unknown action '{other}'. Valid: set, clear"
            ))),
        }
    }
}
