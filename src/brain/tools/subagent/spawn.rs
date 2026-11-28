//! spawn_agent tool — creates a child agent with forked context.
//!
//! Sub-agent progress is streamed to `~/.opencrabs/tmp/detached/<agent_id>.json`
//! so the main orchestrator can track status without session_search.

use super::manager::{SubAgent, SubAgentManager};
use crate::brain::agent::error::AgentError;
use crate::brain::agent::service::work_status::{AgentGrant, WorkState, WorkStatus};
use crate::brain::tools::error::{Result, ToolError};
use crate::brain::tools::r#trait::{Tool, ToolCapability, ToolExecutionContext, ToolResult};
use async_trait::async_trait;
use serde_json::Value;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// How much of a sub-agent's output the pushed message carries. Enough to act
/// on, short of pasting a long transcript into the parent's context.
const PUSHED_OUTPUT_LIMIT: usize = 4000;

/// How much of a long report the DURABLE history record carries (#1024).
///
/// Split into head and tail on purpose: `truncate_output` keeps only the last
/// 4,000 chars, so a 36k review already loses its opening before it reaches the
/// transcript — the framing and the headline finding are exactly what a reader
/// needs first. The middle of a long report is its least valuable part.
const HISTORY_HEAD_CHARS: usize = 1500;
const HISTORY_TAIL_CHARS: usize = 2500;

/// Build the message a finished sub-agent injects into the session that
/// spawned it. Pure, so the framing is testable without spawning anything.
///
/// `outcome` is the output on success or the error on failure.
pub(crate) fn completion_message(
    label: &str,
    agent_id: &str,
    outcome: std::result::Result<&str, &str>,
) -> crate::brain::agent::QueuedUserMessage {
    let (context_text, display_text, history_text) = match outcome {
        Ok(output) => {
            let full_report_hint = if output.chars().count() > PUSHED_OUTPUT_LIMIT {
                let path = crate::brain::agent::service::work_status::status_path(agent_id);
                format!(
                    "Preview truncated - the FULL untruncated report is persisted in {} \
                     (field output_full).\n",
                    path.display()
                )
            } else {
                String::new()
            };
            (
                format!(
                    "[System: the sub-agent you spawned has finished.\n\
                     Agent: {label} (id {agent_id})\n\
                     Status: completed\n\
                     Output:\n{}\n\n\
                     {full_report_hint}\
                     Report the result to the user and continue anything that was waiting on it. \
                     Do not re-spawn the agent — this IS its result.]",
                    truncate_output(output)
                ),
                format!("🤖 sub-agent finished: {label}"),
                durable_record(label, agent_id, "completed", &history_excerpt(output)),
            )
        }
        Err(error) => (
            format!(
                "[System: the sub-agent you spawned has failed.\n\
                 Agent: {label} (id {agent_id})\n\
                 Status: failed\n\
                 Error: {error}\n\n\
                 Report the failure to the user and decide what to do about it. Do not assume the \
                 work was completed.]"
            ),
            format!("🤖 sub-agent failed: {label}"),
            // A failure's reason is exactly what a later turn needs to avoid
            // re-running the same doomed work, so it is durable too.
            durable_record(label, agent_id, "failed", &history_excerpt(error)),
        ),
    };
    crate::brain::agent::QueuedUserMessage {
        context_text,
        display_text,
        origin: crate::brain::agent::PushOrigin::SubAgent,
        bg_meta: None,
        history_text: Some(history_text),
    }
}

/// Bound a report into the head+tail excerpt the durable record carries
/// (#1024). Pure, so the bound is testable without spawning anything.
///
/// A report short enough to fit the two windows is carried whole, with no
/// marker: a `…(truncated)` note on an untruncated text is its own small lie.
pub(crate) fn history_excerpt(output: &str) -> String {
    let total = output.chars().count();
    if total <= HISTORY_HEAD_CHARS + HISTORY_TAIL_CHARS {
        return output.to_string();
    }
    let head: String = output.chars().take(HISTORY_HEAD_CHARS).collect();
    let tail: String = output.chars().skip(total - HISTORY_TAIL_CHARS).collect();
    format!(
        "{head}\n…(truncated {omitted} chars)…\n{tail}",
        omitted = total - HISTORY_HEAD_CHARS - HISTORY_TAIL_CHARS
    )
}

/// The durable record a finished sub-agent leaves in the parent's transcript
/// (#1024).
///
/// `completion_message` builds two strings and history used to keep only the
/// tag, so a compaction erased the result from the parent's view while it sat
/// intact on disk — and the parent then reported the work as failed. This is
/// what persists instead: enough to act on, plus where the rest lives and how
/// long that pointer stays valid, so a reader who finds the path gone knows
/// why rather than concluding nothing was ever produced.
fn durable_record(label: &str, agent_id: &str, state: &str, detail: &str) -> String {
    let path = crate::brain::agent::service::work_status::status_path(agent_id);
    let days = crate::config::Config::current()
        .agent
        .tool_output_retention_days();
    // A pointer that expires in silence is the same lie one layer down, so the
    // window is printed rather than implied.
    let retention = if days == 0 {
        "kept until removed by hand".to_string()
    } else {
        format!("retained {days} day(s)")
    };
    let tag = if state == "failed" {
        "🤖 sub-agent failed"
    } else {
        "🤖 sub-agent finished"
    };
    format!(
        "{tag}: {label}\n\
         agent id: {agent_id}\n\
         status: {state}\n\
         full report: {} (field finish.output_full), {retention}\n\
         {detail}",
        path.display()
    )
}

/// Keep the tail of a long output: the conclusion matters more than the
/// opening, same as the detached-command completion path.
fn truncate_output(output: &str) -> String {
    if output.chars().count() <= PUSHED_OUTPUT_LIMIT {
        return output.to_string();
    }
    let skip = output.chars().count() - PUSHED_OUTPUT_LIMIT;
    let tail: String = output.chars().skip(skip).collect();
    format!("…(truncated)\n{tail}")
}

/// Deliver a finished sub-agent's outcome to the session that spawned it.
pub(crate) fn push_result(
    parent_session_id: uuid::Uuid,
    label: &str,
    agent_id: &str,
    outcome: std::result::Result<&str, &str>,
) {
    use crate::brain::agent::service::session_routes::{Delivery, deliver_to_session};

    let msg = completion_message(label, agent_id, outcome);
    // interrupt=true (fork #13): a sub-agent completion is the parent's own
    // awaited work — it must reach the parent even mid-turn, exactly as
    // before the gate existed.
    match deliver_to_session(parent_session_id, msg, true) {
        Delivery::Delivered => {
            tracing::info!(
                "Sub-agent {agent_id} reported its result to session {parent_session_id}"
            );
        }
        Delivery::Parked => {
            // Not lost: it leaves when the owning channel claims the session.
            tracing::info!(
                "Sub-agent {agent_id}'s result is parked for session {parent_session_id} until \
                 its channel claims it"
            );
        }
        Delivery::NoRoute => {
            // The parent is waiting on this either way, so say so rather than
            // returning quietly.
            tracing::warn!(
                "Sub-agent {agent_id}'s result had nowhere to go for session \
                 {parent_session_id}; the parent will not hear about it"
            );
        }
        Delivery::RefusedInFlight { redirected_to } => {
            // Unreachable by construction: interrupt=true is passed above and
            // the fork #13 gate refuses only when interrupt is unset. Arm kept
            // explicit so a future call-site change cannot drop the outcome
            // silently (port seam: upstream's match has no catch-all).
            tracing::warn!(
                "Sub-agent {agent_id}'s result was refused by the interrupt gate \
                 for session {parent_session_id} (redirected to {redirected_to:?}); the \
                 parent will not hear about it"
            );
        }
        Delivery::Redirected { to } => {
            // The parent was replaced on its channel while the sub-agent ran
            // (fork #17), so the result was REDIRECTED to the session that
            // owns the channel now (fork #19) — delivered, not dropped.
            tracing::info!(
                "Sub-agent {agent_id}'s result was redirected from session \
                 {parent_session_id} to session {to}, which now owns its channel"
            );
        }
    }
}

/// Build the message a sub-agent injects when it cannot proceed without its
/// parent (#859). Pure, so the framing is testable without spawning anything.
///
/// Deliberately shaped NOT to read like `completion_message`: a parent that
/// mistakes a blocker for a result will report half-finished work to the user.
/// The closing line therefore says what is due and — just as importantly —
/// that nothing is due this second, so a parent mid-turn can finish its
/// thought instead of dropping everything.
/// `parked` says which of the two situations this is, because the parent's next
/// move differs and the old wording asserted the wrong one for half its callers
/// (#984). A child that parks at a round boundary is still `Running`/
/// `AwaitingInput` and takes `send_input`. A child that reports a blocker and
/// ends its turn has COMPLETED — `send_input` refuses a completed agent with
/// "use resume_agent to continue it", so naming `send_input` there sent the
/// parent at the one verb that cannot work.
pub(crate) fn attention_message(
    label: &str,
    agent_id: &str,
    kind: &str,
    text: &str,
    parked: bool,
) -> crate::brain::agent::QueuedUserMessage {
    let what = match kind {
        "question" => "has a question it cannot answer on its own",
        "blocker" => "is blocked and cannot continue",
        _ => "wants your attention",
    };
    let display = match kind {
        "question" => format!("❓ sub-agent asked: {label}"),
        "blocker" => format!("⛔ sub-agent blocked: {label}"),
        _ => format!("🔔 sub-agent needs you: {label}"),
    };
    let next_move = if parked {
        format!(
            "Nothing is blocked on you reading this now — it is parked, not finished, and will \
             wait. When you get to it, answer with `send_input {agent_id}` (it resumes from \
             where it stopped) or read its full history with `agent_progress {agent_id}`. Do \
             not report its work as done: it has not finished."
        )
    } else {
        format!(
            "Nothing is blocked on you reading this now. It ended its turn to tell you, so it \
             is no longer running: continue it with `resume_agent {agent_id}` (same context, no \
             re-brief) or read its history with `agent_progress {agent_id}`. Do not report its \
             work as done: it has not finished."
        )
    };
    let context_text = format!(
        "[System: the sub-agent you spawned needs your input.\n\
         Agent: {label} (id {agent_id})\n\
         Status: {kind} — {what}\n\
         It said:\n{text}\n\n\
         {next_move}]"
    );
    crate::brain::agent::QueuedUserMessage {
        context_text,
        display_text: display,
        origin: crate::brain::agent::PushOrigin::SubAgent,
        bg_meta: None,
        // #1024: an attention message is not a result — the child has not
        // finished, so the display tag IS the honest durable record. The
        // parent's next move is named in `context_text` either way.
        history_text: None,
    }
}

/// Tell a parent that its sub-agent cannot proceed without it (#859).
///
/// `interrupt = true` matches [`push_result`] and `session_notify` (#393), and
/// it is the load-bearing choice here. The fork #13 mid-turn gate refuses a
/// non-interrupt push while the target is mid-turn — and a parent that spawned
/// a child and kept working is mid-turn in the normal case, so the gate would
/// swallow exactly the message this exists to deliver, leaving the child
/// parked in silence. The message is framed as a `[System: …]` block that says
/// outright nothing is due this second, which is what keeps an interrupt from
/// derailing the turn it lands in.
pub(crate) fn push_attention(
    parent_session_id: uuid::Uuid,
    label: &str,
    agent_id: &str,
    kind: &str,
    text: &str,
    parked: bool,
) {
    use crate::brain::agent::service::session_routes::{Delivery, deliver_to_session};

    let msg = attention_message(label, agent_id, kind, text, parked);
    match deliver_to_session(parent_session_id, msg, true) {
        Delivery::Delivered => tracing::info!(
            "Sub-agent {agent_id} reported a {kind}; session {parent_session_id} was told"
        ),
        Delivery::Parked => tracing::info!(
            "Sub-agent {agent_id}'s {kind} is parked for session {parent_session_id} until its \
             channel claims it"
        ),
        Delivery::NoRoute => tracing::warn!(
            "Sub-agent {agent_id} reported a {kind} but session {parent_session_id} has no \
             route; its ledger entry is the only record"
        ),
        Delivery::RefusedInFlight { redirected_to } => tracing::warn!(
            "Sub-agent {agent_id}'s {kind} was refused by the interrupt gate for session \
             {parent_session_id} (redirected to {redirected_to:?}); the parent will not hear \
             about it"
        ),
        Delivery::Redirected { to } => tracing::info!(
            "Sub-agent {agent_id}'s {kind} was redirected from session {parent_session_id} to \
             session {to}, which now owns its channel"
        ),
    }
}

/// Whether a cancelled token means the PARENT stopped this child, as opposed to
/// the daemon going down under it (#924).
///
/// A `CancellationToken` cannot tell the two apart on its own: the stop path
/// cancels the token and so does the shutdown path, and from inside the child
/// they look identical. `shutdown::is_shutting_down()` is the only thing that
/// separates them, and getting it wrong is expensive in both directions —
/// reporting a stop as a failure tells the parent its own decision crashed,
/// while reporting a shutdown as a deliberate stop marks `Cancelled` a child
/// that was really interrupted mid-work, which is precisely the state
/// `reconcile` exists to record.
pub(crate) fn is_deliberate_stop(token: &CancellationToken) -> bool {
    token.is_cancelled() && !crate::brain::agent::service::shutdown::is_shutting_down()
}

/// The live-child cap for this spawn (#1084): `[agent] max_live_subagents`,
/// clamped to at least 1 exactly as `[agent] max_concurrent` is at its own
/// read site (`(config.agent.max_concurrent as usize).max(1)`).
///
/// Read through `Config::current()` — the process-wide mirror the config
/// watcher refreshes, so a `max_live_subagents` edit applies without a restart
/// and the check costs an `Arc` clone instead of a parse. `0` therefore means
/// ONE, not "unlimited": a silent lockout of all spawning is a worse failure
/// than a floor, and "unlimited" is the defect this key exists to close.
pub(crate) fn live_cap() -> usize {
    (crate::config::Config::current().agent.max_live_subagents as usize).max(1)
}

/// The refusal a parent gets when it is at its live-child cap (#1084).
///
/// `live` is the occupancy the admission check measured: children in a live
/// state plus any spawn already admitted in the same parallel batch and not yet
/// registered. That is what is holding the slots, so it is what the message
/// reports — understating it would read as a cap that fires for no reason.
///
/// Pure, so the three things the message must carry — the cap, the CURRENT
/// live count, and the verbs that free a slot — are testable without a
/// manager, a session or a provider. It names the next verb the caller should
/// use, matching the house style for a refusal that has an obvious remedy
/// (`resume_agent`'s "Use wait_agent first or stop_agent to cancel", and this
/// file's own "refused: … nesting permissions").
///
/// It says "No child was created" on purpose: a refusal must cost nothing, and
/// the caller has to be able to trust that.
pub(crate) fn live_cap_refusal(cap: usize, live: usize) -> String {
    format!(
        "refused: this session already has {live} live sub-agent(s), at the \
         [agent] max_live_subagents cap of {cap}. No child was created, no session and no \
         worktree. Free a slot first: `wait_agent <id>` collects a child that has finished \
         its round, `stop_agent <id>` ends one you no longer need — a child that is \
         Completed, Failed or Cancelled holds no slot, only Running and AwaitingInput do. \
         Then spawn again, or do this piece of the work yourself."
    )
}

/// Tool that spawns a child agent to handle a sub-task.
pub struct SpawnAgentTool {
    manager: Arc<SubAgentManager>,
    parent_registry: Arc<crate::brain::tools::ToolRegistry>,
}

impl SpawnAgentTool {
    pub fn new(
        manager: Arc<SubAgentManager>,
        parent_registry: Arc<crate::brain::tools::ToolRegistry>,
    ) -> Self {
        Self {
            manager,
            parent_registry,
        }
    }
}

#[async_trait]
impl Tool for SpawnAgentTool {
    fn name(&self) -> &str {
        "spawn_agent"
    }

    fn description(&self) -> &str {
        "Spawn a subagent (sub-agent / child agent) for work that would otherwise burn this \
         session's context: long-running or multi-source research, bulk file processing, or \
         code development across several files. It gets its own session, runs in the \
         background, and is resumable by id. Do NOT spawn for a single command, one file \
         read, or a one-line edit — delegating costs more than doing. Independent items can \
         run in parallel. Returns an agent_id. Reading it back: the completion \
         wake is the normal path — you are woken with its result. Mid-run, the child reports \
         via report_progress and you read agent_progress <id>; steer a running child \
         with send_input <id>, and continue one that ended its turn with resume_agent <id>. \
         wait_agent is the EXCEPTION, for a genuine precondition such as before resume_agent \
         — never to wait for work you will be woken for. A child that goes QUIET is \
         watched too: when it has produced no new output for ten minutes the daemon tells \
         you in your own context, unprompted — you will be told, so do not poll it and do \
         not spawn a replacement for work that is still running. \
         There is also a hard cap on how many children this session may have ALIVE at once \
         (`[agent] max_live_subagents`, default 8): a spawn past it is refused with the \
         current count, so collect with wait_agent or free one with stop_agent before \
         spawning more. Finished children (completed / failed / stopped) hold no slot. \
         \n\nProvider and model resolution (highest priority first): \
         (1) the optional `provider` / `model` parameters on THIS call, \
         (2) the user's config.toml `[agent]` keys `subagent_provider` / `subagent_model`, \
         (3) the parent session's provider with that provider's default model. \
         Use the per-call params when a single skill orchestrates multiple steps that each \
         want a different model (for example: plan with one model, code with another, review \
         with a third). Use the config keys when every sub-agent in the session should share \
         the same routing. Use no override to let the child inherit the parent."
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "prompt": {
                    "type": "string",
                    "description": "The task/instruction for the child agent to execute"
                },
                "label": {
                    "type": "string",
                    "description": "Short human-readable label for this sub-agent (e.g., 'refactor-auth', 'test-runner')"
                },
                "allow_nested": {
                    "type": "boolean",
                    "description": "Whether THIS CHILD may spawn further sub-agents or background tasks (#1195). Default true. Set false for pure workers: any spawn_agent call it makes is refused, and its long bash commands run attached instead of detached. Plan workers are always spawned with false."
                },
                "read_only": {
                    "type": "boolean",
                    "description": "Spawn this child with a read-restricted tool registry (#1173): file reads, glob/grep/ls and web research only — no writes, no bash, no spawning. Use for exploration, code review, and research children. Omit or false for a full-capability worker (still minus recursive/dangerous tools)."
                },
                "include_brain": {
                    "type": "boolean",
                    "description": "Whether to pre-inject workspace brain files (SOUL.md, USER.md, AGENTS.md) into the child agent (#145). Default false (lean execution — sub-agents query workspace files on demand via load_brain_file). Set true when delegating full co-worker tasks that need immediate ambient access to all workspace rules and personality."
                },
                "provider": {
                    "type": "string",
                    "description": "Optional provider override for THIS spawn (e.g., 'zhipu', 'openrouter', 'custom:my-provider'). Highest precedence — overrides config.agent.subagent_provider and parent inheritance. Use to route this single sub-agent differently from the global subagent config."
                },
                "model": {
                    "type": "string",
                    "description": "Optional model override for THIS spawn (model id as the chosen provider accepts it, e.g., 'glm-5', 'deepseek-coder'). Highest precedence — overrides config.agent.subagent_model. Pair with `provider` when the model lives on a provider other than the parent session's."
                },
                "plan_session": {
                    "type": "string",
                    "description": "Optional session UUID whose plan state this child operates on (#908). When set, the child's plan tool resolves that session's plan (JSON, design .md, markers, task goal) instead of its own. Plan-driven execution passes the parent session id here so a task worker sees the parent's checklist; the child's own session stays fresh. Omit for normal sub-agents."
                }
            },
            "required": ["prompt"]
        })
    }

    fn capabilities(&self) -> Vec<ToolCapability> {
        vec![ToolCapability::SystemModification]
    }

    fn requires_approval(&self) -> bool {
        true
    }

    async fn execute(&self, input: Value, context: &ToolExecutionContext) -> Result<ToolResult> {
        // Nesting enforcement (#1195): children spawned with allow_nested=false
        // are pure workers. Root / unmanaged sessions pass freely.
        if let Some(ref mgr) = context.subagent_manager
            && !mgr.nesting_allowed_for_session(context.session_id)
        {
            return Ok(ToolResult::error(
                "refused: this agent was spawned without nesting permissions \
                 (allow_nested=false) and may not spawn further sub-agents"
                    .to_string(),
            ));
        }
        let prompt = input
            .get("prompt")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidInput("'prompt' is required".into()))?
            .to_string();

        let label = input
            .get("label")
            .and_then(|v| v.as_str())
            .unwrap_or("sub-agent")
            .to_string();

        // Resolve the child's capability grant (#1173). Explicit `read_only`
        // wins; a deprecated typed `agent_type` maps to its historical
        // effective grant (loudly); anything else defaults to full access.
        // Nesting grant (#1195): absent = true (backward compatible). A
        // non-boolean is a hard error, mirroring read_only above.
        let allow_nested = match input.get("allow_nested") {
            None | Some(serde_json::Value::Bool(_)) => input
                .get("allow_nested")
                .and_then(|v| v.as_bool())
                .unwrap_or(true),
            Some(_) => {
                return Ok(ToolResult::error(
                    "'allow_nested' must be a boolean (true = child may nest spawns/detach, false = pure worker)"
                        .to_string(),
                ));
            }
        };

        // A non-boolean `read_only` is a hard error, not a silent default —
        // the caller asked for a specific capability and must not get a
        // different one because of a type mistake.
        let explicit_read_only = match input.get("read_only") {
            Some(v) => Some(v.as_bool().ok_or_else(|| {
                ToolError::InvalidInput(
                    "'read_only' must be a boolean (true = restricted registry, false = full)"
                        .into(),
                )
            })?),
            None => None,
        };

        // Brain pre-injection flag (#145): absent = false (lean default).
        // Non-boolean is a hard error.
        let include_brain = match input.get("include_brain") {
            None | Some(serde_json::Value::Bool(_)) => input
                .get("include_brain")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
            Some(_) => {
                return Ok(ToolResult::error(
                    "'include_brain' must be a boolean (true = pre-inject workspace brain, false = lean)"
                        .to_string(),
                ));
            }
        };
        // The plan-review exception (#1075): a WRITE PRIVILEGE, read from the
        // TRUSTED execution context — never from `input`. The tool schema is
        // model-authored, so a privilege a model could request by emitting
        // `{"plan_review": true}` would be the very defect #1075 closed, with
        // the string swapped for a boolean. `context.plan_review_worker` is set
        // by code, in the one caller that legitimately mints this worker (the
        // plan card's Review button). A `plan_review` key in the model's tool
        // input is ignored by construction: nothing reads it.
        let plan_review = context.plan_review_worker;

        let deprecated_raw = input
            .get("agent_type")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        let (read_only, deprecation_note) = match (explicit_read_only, deprecated_raw) {
            (Some(ro), _) => (ro, None),
            (None, Some(raw)) => {
                let grant =
                    super::map_deprecated_agent_type(&raw).map_err(ToolError::InvalidInput)?;
                tracing::warn!(
                    "spawn_agent called with deprecated agent_type='{raw}'; \
                     mapped to read_only={grant}. Pass read_only explicitly (#1173)."
                );
                (
                    grant,
                    Some(format!(
                        "Deprecated agent_type '{raw}' resolved to read_only={grant}; pass read_only explicitly."
                    )),
                )
            }
            (None, None) => (false, None),
        };

        // Optional plan-state override (#908 option A): plan-driven
        // execution hands the child the PARENT's session id so the worker's
        // plan tool resolves the parent's checklist while the worker session
        // itself stays fresh. A malformed UUID is a hard error — silently
        // falling back to the child's own session would let the worker run
        // against an empty plan and report success on nothing.
        let plan_session_override = match input
            .get("plan_session")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            Some(raw) => Some(uuid::Uuid::parse_str(raw).map_err(|e| {
                ToolError::InvalidInput(format!("'plan_session' must be a valid UUID: {e}"))
            })?),
            None => None,
        };

        // Live-child admission cap (#1084). Placed here — after every input
        // check above, and above session creation, the worktree cut and the
        // spawn task: a refusal must cost nothing, and this is the last moment
        // at which nothing has been created. Nothing below this line runs
        // unless a slot was granted, so a malformed call still gets its own
        // error rather than a cap refusal it cannot act on.
        //
        // Counted on `self.manager` — the SAME roster the child is inserted
        // into at the bottom of this function — and scoped to this session's
        // own children, because the remedies named in the refusal
        // (`wait_agent`, `stop_agent`) resolve ids through `resolve`, which
        // refuses a child whose parent is another session. A process-wide
        // count would refuse a parent for slots it cannot free.
        //
        // `try_admit_child` also RESERVES the slot, because a parallel tool
        // batch can run several spawns at once and each would otherwise read
        // the same live count and all admit. The guard is held for the rest of
        // this call and released by its `Drop` — on the success path AND on
        // every `?` exit below — so a failed spawn never eats a slot.
        //
        // Held as `let _admission` (not a bare `_`), so the guard lives to the
        // end of the function: the register-before-spawn change at the bottom
        // of this function (#1082) then merges without touching this block.
        // The reservation therefore outlives `insert` by the receipt-formatting
        // tail, which over-counts by one for a few microseconds — the safe
        // direction (a spurious refusal, never a spurious admission).
        let cap = live_cap();
        let _admission = match self.manager.try_admit_child(context.session_id, cap) {
            Ok(guard) => guard,
            Err(occupancy) => {
                return Ok(ToolResult::error(live_cap_refusal(cap, occupancy)));
            }
        };

        // We need a ServiceContext to create a session for the child
        let service_context = context
            .service_context
            .as_ref()
            .ok_or_else(|| ToolError::Execution("No service context available".into()))?
            .clone();

        // Create a new session for the child agent. It records the spawning
        // lane's own session id as its parent (#192), so a child uuid resolves
        // back to the lane that owns it in one query — instead of notifying a
        // black hole or being reconstructed from a status file a restart may
        // have replaced.
        let session_service = crate::services::SessionService::new(service_context.clone());
        let child_session = session_service
            .create_session_with_provider(
                Some(format!("subagent: {}", label)),
                None,
                None,
                None,
                Some(context.session_id),
            )
            .await
            .map_err(|e| ToolError::Execution(format!("Failed to create child session: {}", e)))?;

        let child_session_id = child_session.id;
        let agent_id = SubAgentManager::generate_id();
        // Cut before the child is built so its working directory can point at
        // the new tree from the first turn.
        let worktree = super::worktree::create(&context.working_dir(), &agent_id);

        // Create cancel token and input channel for the child
        let cancel_token = CancellationToken::new();
        let (input_tx, input_rx) = mpsc::unbounded_channel::<String>();

        // Per-call provider / model overrides, read from the tool
        // call's input. Precedence (issue #152): per-call > config >
        // parent inheritance. Empty strings are treated as unset so an
        // optional schema field passed as "" doesn't accidentally
        // resolve to an invalid provider name.
        let call_provider = input
            .get("provider")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        let call_model = input
            .get("model")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);

        // Load config and extract model override before entering block scope
        let config = crate::config::Config::load()
            .map_err(|e| ToolError::Execution(format!("Config load failed: {}", e)))?;
        // Precedence: per-call > config > parent default, for provider and
        // model alike; the winning value is normalised so "custom:<name>"
        // or "<provider>/<model>" resolves to the provider it names (#1316).
        // When the model is None the child uses its provider's default.
        let child = super::provider_pair::child_pair(
            &config,
            call_provider.as_deref(),
            call_model.as_deref(),
        );
        let model_override = child.model.clone();
        let effective_provider_name = child.provider.clone();

        // Resolve the child's capability grant ONCE, before anything is built
        // (#1075, #1079). The registry and the persisted status file must
        // describe the SAME child: recording a grant derived from the caller's
        // `read_only` alone, while the registry had additionally been stripped
        // for an Editing parent, is precisely the drift that let `resume_agent`
        // hand a stripped child its write tools back.
        let grant = if plan_review {
            AgentGrant::plan_review_worker(read_only, include_brain, allow_nested)
        } else {
            AgentGrant::new(read_only, include_brain, allow_nested)
        };

        // What the child will ACTUALLY be able to do: the caller's grant OR the
        // #649 Editing-parent strip. Needed out here for the two pieces of PROSE
        // that describe the child — the capability note it reads and the
        // `Access:` line its parent reads — because both must describe the
        // registry it really holds, not the grant it asked for (#1075).
        //
        // Deliberately NOT folded into `grant`: the strip is re-derived on
        // every registry build, so a child whose parent leaves Editing later
        // regains write access (#1079), and freezing it into the persisted
        // grant would lock it out for life. `build_child_registry_gated` makes
        // the same decision again for the registry itself — it stays the single
        // authority for what the child can DO; this value only decides what we
        // SAY about it.
        let effective_read_only = read_only
            || crate::brain::tools::plan_gate::editing_parent_strips(
                context.session_id,
                plan_review,
            )
            .await;

        // Build a minimal AgentService for the child
        let child_service = {
            // Use the resolved per-call/config provider if any,
            // otherwise inherit parent's. The fallback-on-failure
            // path keeps a typo in the override from breaking the
            // spawn entirely — same shape as the prior config-only
            // resolution.
            let provider = if let Some(ref provider_name) = effective_provider_name {
                match crate::brain::provider::create_provider_by_name(&config, provider_name).await
                {
                    Ok(p) => {
                        let source = match child.source {
                            super::provider_pair::ProviderSource::PerCall => "per-call",
                            super::provider_pair::ProviderSource::Config => "config",
                        };
                        tracing::info!("Sub-agent using {source} provider '{provider_name}'");
                        p
                    }
                    Err(e) => {
                        tracing::warn!(
                            "Sub-agent provider '{}' failed: {e}, falling back to parent",
                            provider_name
                        );
                        crate::brain::provider::create_provider(&config)
                            .await
                            .map_err(|e| {
                                ToolError::Execution(format!("Failed to create provider: {}", e))
                            })?
                    }
                }
            } else {
                crate::brain::provider::create_provider(&config)
                    .await
                    .map_err(|e| {
                        ToolError::Execution(format!("Failed to create provider: {}", e))
                    })?
            };

            // Build the child's tool registry through the ONE gated
            // constructor (#1075, #1079). It applies, in order: the base child
            // registry, the caller's `read_only` grant, and the #649
            // Editing-parent strip unless this child is the typed plan-review
            // worker. The grant passed here is the SAME value persisted below,
            // so a resume rebuilds what the child actually ran with.
            let child_registry = super::build_child_registry_gated(
                &self.parent_registry,
                &self.manager,
                // The child's own session is fresh and resolves to NoPlan, so
                // the PARENT's plan state is the one that governs it (#649).
                context.session_id,
                &grant,
                // Typed, from the grant — never `label == PLAN_REVIEW_LABEL`.
                grant.plan_review,
            )
            .await;

            // A private checkout for this child (#1151). A fan-out spawns
            // several at once against one tree, so they collide by
            // construction rather than by coincidence. `None` is a normal
            // outcome — outside a repository, or if git refuses — and means
            // the child works where it always did.
            let child_dir = worktree
                .as_ref()
                .map(|w| w.path.clone())
                .unwrap_or_else(|| context.working_dir());

            let system_brain = super::brain::child_system_brain(
                include_brain,
                &child_dir,
                model_override.as_deref(),
                effective_provider_name.as_deref(),
            );

            let mut agent =
                crate::brain::agent::AgentService::new(provider, service_context.clone(), &config)
                    .await
                    .with_tool_registry(child_registry)
                    .with_auto_approve_tools(true) // children auto-approve (parent already approved spawn)
                    .with_working_directory(child_dir)
                    // #129: a sub-agent is a headless session (owner ruling C)
                    // — backstop flag so interactive-only tools hard-error.
                    .with_headless(true)
                    .with_plan_session_override(plan_session_override);

            if let Some(brain) = system_brain {
                agent = agent.with_system_brain(brain);
            }

            Arc::new(agent)
        };

        // Typed preambles are gone (#1173, Proposal B): a restricted child
        // gets one factual capability line derived from its actual grant —
        // no role-play text that could drift from what the registry truly
        // allows. Full-access children receive just the task.
        // #145: stacked capability + lean context note via child_prompt.
        // #129 (owner ruling A): a sub-agent is a headless session — the
        // self-containedness preamble rides on EVERY spawned child prompt,
        // below the capability note and the context note, above the task
        // (child_prompt emits notes-then-task, so the preamble slots
        // between them; byte-exact task preserved).
        let stacked = super::brain::child_prompt(effective_read_only, include_brain, &prompt);
        let notes_len = stacked.len() - prompt.len();
        let full_prompt = format!(
            "{}{}{}{}",
            &stacked[..notes_len],
            crate::cli::tool_setup::HEADLESS_PREAMBLE.trim_start_matches('\n'),
            // #859: the preamble just told this child it cannot ask anyone
            // anything. It can — its parent is one `report_progress` away —
            // and without this line a blocked child guesses instead of
            // parking. Slotted beside the preamble, above the task.
            crate::cli::tool_setup::SUBAGENT_ANSWER_CHANNEL.trim_start_matches('\n'),
            &stacked[notes_len..]
        );

        // Create the status file in Pending state before spawning. new()
        // writes the file; we don't need the returned handle, but we do
        // propagate any write error. The parent is captured FIRST so the
        // status file carries it: if a restart kills this agent mid-turn
        // and boot-resume revives its session, the revived result must
        // reach this session (#110). Kept as a `Uuid` too — the follow-up
        // loop and the completion path below capture it by value (#110).
        let parent_session_id = context.session_id;
        let parent_session_for_status = context.session_id.to_string();
        // The grant is recorded HERE, not only in the in-memory agent (#852):
        // a resumed agent is rebuilt from this file, so a grant that lived
        // only in the map would come back as whatever the rebuild assumed —
        // and a read-only child restored writable is indistinguishable from a
        // normal resume. `grant` was resolved once, above the registry build,
        // so the two can never disagree (#1079).
        let mut status = WorkStatus::new_agent(
            &agent_id,
            &label,
            &child_session_id.to_string(),
            &full_prompt,
            Some(&parent_session_for_status),
            Some(&grant),
        )
        .map_err(|e| ToolError::Execution(format!("Failed to create status file: {e}")))?;
        // #960: record the tree the child was actually given. The branch and
        // path were derived at spawn and thrown away, so the only way a parent
        // could find a child's work in progress was to guess the naming scheme.
        // Written from the SAME `SubagentWorktree` the child's working
        // directory comes from, never from a second format string.
        if let Some(tree) = worktree.as_ref()
            && let Err(e) = status.record_worktree(&tree.branch, &tree.path)
        {
            tracing::warn!("Failed to record worktree on status: {e}");
        }

        // Spawn background task with input loop
        let cancel_clone = cancel_token.clone();
        let manager = self.manager.clone();
        let agent_id_clone = agent_id.clone();
        let worktree_for_cleanup = worktree.clone();
        let prompt_clone = full_prompt;
        let label_clone = label.clone();
        let mut input_rx = input_rx;
        // `parent_session_id` was captured above the status-file write (#110).

        // Register BEFORE the task is spawned (#1082). Under a multi-threaded
        // runtime the task can start — and fail — before control returns here,
        // and `insert` used to run ~296 lines after `tokio::spawn`, so a child
        // that failed in that window was never registered: `mark_failed` found
        // no entry, returned `false` (so no push reached the parent), and the
        // insert that followed then re-added the agent as `Running` forever.
        // `wait_agent`, `agent_progress` and `tasks_list` all read this map, so
        // such a child was both unreachable and silent.
        //
        // Every field below is ALREADY known at this point — the grant flags
        // are resolved above, the cancel token and input sender were built with
        // them, and the worktree was cut before the child was built — so the
        // early record carries the REAL values and lacks only the handle. That
        // is why this is one early insert rather than a dummy placeholder
        // patched afterwards: nothing needs filling in later except
        // `join_handle`. The only statements between here and the returned
        // `Ok(...)` are infallible, so no early return can leave a phantom
        // `Running` entry behind.
        self.manager.insert(SubAgent {
            // The CALLER's grant, not the Editing-parent strip (#1079). The
            // strip is re-derived from plan state on every registry build, so a
            // child whose parent has left Editing since spawn legitimately
            // regains write access; freezing it here would lock the child out
            // for life. `plan_review` rides along so the re-derivation knows
            // about the one typed exception.
            read_only,
            include_brain,
            allow_nested,
            plan_review,
            cancel_token,
            join_handle: None,
            input_tx: Some(input_tx),
            // #960: the caller's own words, not the preamble-stacked copy —
            // a parent reading this back is checking the result against the
            // ask, and the preambles are our text, not theirs.
            task: prompt.clone(),
            // #960: same values the status file carries, so a live read and a
            // rehydrated read answer identically.
            worktree_branch: worktree.as_ref().map(|w| w.branch.clone()),
            worktree_path: worktree.as_ref().map(|w| w.path.display().to_string()),
            ..SubAgent::new(
                agent_id.clone(),
                label.clone(),
                child_session_id,
                context.session_id,
            )
        });

        let handle = tokio::spawn(async move {
            tracing::info!("Sub-agent {} starting: {}", agent_id_clone, prompt_clone);

            // Transition to Running state.
            let mut status = WorkStatus::read(&agent_id_clone).unwrap_or_else(|| {
                WorkStatus::new_agent(
                    &agent_id_clone,
                    &label_clone,
                    &child_session_id.to_string(),
                    &prompt_clone,
                    Some(&parent_session_for_status),
                    Some(&grant),
                )
                .expect("status file")
            });
            if !matches!(status.state, WorkState::Completed | WorkState::Failed)
                && let Err(e) = status.mark_running()
            {
                tracing::warn!("Failed to write running status: {e}");
            }

            // Reload with correct state.

            let mut current_prompt = prompt_clone;
            let mut iteration: usize = 0;
            let total_tool_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));

            // Run prompt → wait for input → run again loop.
            //
            // The loop yields an OUTCOME rather than a bare string (#924): a
            // child can now end because it finished, because it failed, or
            // because its parent stopped it, and those three must not be
            // collapsed into one exit path. Collapsing them is exactly the bug
            // — a cancelled child fell through to `mark_completed` and pushed
            // a "finished" result to the parent that had just stopped it.
            let outcome: std::result::Result<String, AgentError> = loop {
                iteration += 1;
                let agent_id_for_progress = agent_id_clone.clone();
                let tool_count_counter = total_tool_count.clone();
                let progress_cb = std::sync::Arc::new(
                    move |_sid: uuid::Uuid, event: crate::brain::agent::ProgressEvent| {
                        if let crate::brain::agent::ProgressEvent::ToolStarted { tool_name, .. } =
                            event
                            && let Some(mut st) = WorkStatus::read(&agent_id_for_progress)
                        {
                            let count = tool_count_counter
                                .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                                + 1;
                            let _ = st.update_progress_tool_count(
                                iteration,
                                count,
                                Some(tool_name),
                                None,
                            );
                        }
                    },
                );

                let result = child_service
                    .send_message_with_tools_and_callback(
                        child_session_id,
                        current_prompt,
                        model_override.clone(),
                        Some(cancel_clone.clone()),
                        None,
                        Some(progress_cb),
                        "subagent",
                        None,
                        None,
                    )
                    .await;

                match result {
                    Ok(response) => {
                        // Extract a short summary of what the agent did this turn.
                        let summary = if response.stop_reason
                            == Some(crate::brain::provider::types::StopReason::ToolUse)
                        {
                            "tool call(s) completed".to_string()
                        } else {
                            response.content.chars().take(120).collect::<String>()
                        };

                        // Re-read before writing (#187 D2). `status` is the
                        // handle taken at spawn; the progress callback has been
                        // writing `tool_count` through its own fresh read, and
                        // `update_progress` takes `prev_tool_count` from THIS
                        // copy — so writing from it reset the count to 0 at
                        // every round boundary. Reload first so the callback's
                        // count survives the round-end write.
                        status.reload();
                        status
                            .update_progress(iteration, None, Some(summary))
                            .unwrap_or_else(|e| tracing::warn!("status write failed: {e}"));

                        manager.update_output(&agent_id_clone, response.content.clone());
                        // Natural completion (#1184): the internal tool loop
                        // only returns with a pending tool call when the round
                        // is genuinely gated (approval prompt or iteration
                        // cap). Any other stop reason means the model finished
                        // its answer — completing here delivers the result to
                        // the parent instead of parking the agent as
                        // phantom-"Running" forever. Interactive follow-ups
                        // remain available via `resume_agent`, which accepts
                        // Completed agents.
                        if response.stop_reason
                            != Some(crate::brain::provider::types::StopReason::ToolUse)
                        {
                            tracing::info!(
                                "Sub-agent {} round {} complete naturally, delivering result",
                                agent_id_clone,
                                iteration
                            );
                            break Ok(response.content);
                        }
                        // Genuinely gated (pending approval / cap): park so
                        // wait_agent can observe round-boundary progress and
                        // the parent can nudge with input. The status file
                        // parks too (#1183): a parked agent reading
                        // `state: "Running"` with `completed_at: null` misled
                        // every consumer into waiting on finished work.
                        manager.mark_awaiting_input(&agent_id_clone);
                        if let Err(e) = status.mark_awaiting_input() {
                            tracing::warn!(
                                "Failed to write awaiting-input status for {}: {e}",
                                agent_id_clone
                            );
                        }
                        tracing::info!(
                            "Sub-agent {} round {} complete, waiting for input",
                            agent_id_clone,
                            iteration
                        );

                        // #859: the child is now parked, and before this it
                        // parked in total silence — nothing told the parent,
                        // so an agent waiting on an approval or a decision
                        // looked identical to one still working, and the
                        // parent either waited forever or reported its work
                        // unfinished. Throttled like every other attention
                        // push: a child that re-parks each round has one
                        // question, not one per round.
                        if manager.attention_allowed(&agent_id_clone) {
                            push_attention(
                                parent_session_id,
                                &label_clone,
                                &agent_id_clone,
                                "question",
                                &response.content.chars().take(600).collect::<String>(),
                                true,
                            );
                        }

                        // Wait for follow-up input or shutdown
                        let next = tokio::select! {
                            msg = input_rx.recv() => msg,
                            _ = cancel_clone.cancelled() => {
                                tracing::info!("Sub-agent {} cancelled while waiting for input", agent_id_clone);
                                None
                            }
                        };

                        match next {
                            Some(text) => {
                                manager.mark_running_again(&agent_id_clone);
                                if let Err(e) = status.mark_running() {
                                    tracing::warn!(
                                        "Failed to write running status for {}: {e}",
                                        agent_id_clone
                                    );
                                }
                                tracing::info!(
                                    "Sub-agent {} received follow-up input",
                                    agent_id_clone
                                );
                                current_prompt = text;
                            }
                            // The select's cancel arm fires on a deliberate
                            // stop AND on daemon shutdown; `is_deliberate_stop`
                            // is what tells them apart at the exit block.
                            //
                            // `None` is ALSO what a closed input channel gives,
                            // and that case is a real "finish with what I have".
                            // So the two are separated here rather than at the
                            // exit block: a cancel becomes `Err(Cancelled)`,
                            // which the exit block records as `Interrupted` when
                            // the process is going down (#1086) and as a stop
                            // when the parent asked for it. Breaking `Ok` for a
                            // shutdown cancel — what this used to do — marked a
                            // cut-off child `Completed` and pushed its
                            // half-finished round to the parent as a result.
                            None => {
                                if cancel_clone.is_cancelled() {
                                    break Err(AgentError::Cancelled);
                                }
                                break Ok(response.content);
                            }
                        }
                    }
                    Err(e) => {
                        // The reporting moves to the exit block, which every
                        // outcome now shares. `break` rather than `return` is
                        // what lets it run at all: the old `return` here
                        // skipped the worktree settle below, so a failed
                        // child's isolation tree was never cleaned up (#924).
                        tracing::error!("Sub-agent {} failed: {}", agent_id_clone, e);
                        break Err(e);
                    }
                }
            };

            // Every exit settles here, in ONE block (#924). Before this, the
            // cancelled path fell through to `mark_completed` and pushed a
            // "finished" result to the parent that had just stopped it, and the
            // failure path `return`ed early, skipping the worktree settle
            // entirely.
            let stopped = is_deliberate_stop(&cancel_clone);

            // A cancel that is NOT a deliberate stop is the daemon going down
            // (#1086). `is_deliberate_stop` has already claimed the
            // parent-stop case, so anything cancelled while the shutdown flag
            // is raised was cut off by the process exit.
            //
            // Both arms of `outcome` are covered, and they have to be: the
            // parked branch breaks `Ok` (the round's content, so the parent can
            // read it) and the tool-loop branch breaks `Err(Cancelled)`. Only
            // the second looks like a cancellation, and recording the first as
            // `Completed` marks a cut-off child finished AND pushes its
            // half-finished round to a parent that is itself going down.
            //
            // The token check is what keeps a genuine completion safe: a child
            // that finished its round naturally during a shutdown has an
            // uncancelled token, so its `Completed` record and its result both
            // stand.
            let interrupted_by_shutdown = cancel_clone.is_cancelled()
                && crate::brain::agent::service::shutdown::is_shutting_down();

            // Return the tree, unless the child left work in it. A tree with
            // a commit or an uncommitted edit survives: discarding a child's
            // work silently would be worse than the collisions the isolation
            // exists to prevent. Settle this BEFORE reporting, so a kept tree
            // is named in the result the parent reads rather than only in the
            // log, where nobody would find it.
            let settle_worktree = |outcome: String| -> String {
                match &worktree_for_cleanup {
                    Some(wt) => {
                        let removed = wt.cleanup();
                        match wt.parent_notice(removed) {
                            Some(note) => format!("{outcome}{note}"),
                            None => outcome,
                        }
                    }
                    None => outcome,
                }
            };

            if stopped {
                // A stop is the parent's decision, not a failure. Record it as
                // `Cancelled` and push NOTHING: the parent asked for this and
                // is not waiting on a report, so a completion here would be a
                // message about work it deliberately abandoned. `wait_agent`
                // waiters are already released by `manager.cancel()`, which
                // the stop path called before this.
                tracing::info!(
                    "Sub-agent {} stopped deliberately by its parent",
                    agent_id_clone
                );
                if let Err(write_err) = status.mark_cancelled() {
                    tracing::error!(
                        "Sub-agent {} was stopped but its cancelled status could not be written, \
                         so it will keep reading as running: {write_err}",
                        agent_id_clone
                    );
                }
                // The tree still settles: a stop is not a licence to throw away
                // uncommitted work, so a dirty tree survives. The notice is
                // logged rather than pushed, because silence is the point.
                let notice = settle_worktree(String::new());
                if !notice.is_empty() {
                    tracing::info!(
                        "Sub-agent {} stopped; worktree {}",
                        agent_id_clone,
                        notice.trim()
                    );
                }
            } else if interrupted_by_shutdown {
                // The process is going down under this child (#1086). Record
                // the truth — `Interrupted`, the state boot recovery writes for
                // exactly this situation — and deliver NOTHING: the parent is
                // exiting with us, and a completion here would hand it a
                // half-finished round as though the child had chosen to stop.
                //
                // This branch is checked before `outcome`, because the parked
                // path breaks `Ok` on a cancel: matching on the outcome first
                // would mark a cut-off child `Completed`.
                tracing::info!(
                    "Sub-agent {} was cut off by daemon shutdown; recorded interrupted",
                    agent_id_clone
                );
                // The tree still settles, on the same terms as a stop: a
                // shutdown is not a licence to throw away uncommitted work.
                let notice = settle_worktree(String::new());
                if !notice.is_empty() {
                    tracing::info!(
                        "Sub-agent {} interrupted by shutdown; worktree {}",
                        agent_id_clone,
                        notice.trim()
                    );
                }
                if let Err(write_err) = status.mark_interrupted() {
                    tracing::error!(
                        "Sub-agent {} was cut off by shutdown but its interrupted status could \
                         not be written, so it will keep reading as running: {write_err}",
                        agent_id_clone
                    );
                }
                // Roster transition without a push: `cancel` is idempotent, so
                // a state the drain already set is left alone.
                let _ = manager.cancel(&agent_id_clone);
            } else {
                match outcome {
                    Ok(output) => {
                        let output = settle_worktree(output);
                        // #147: persist the COMPLETE output — the 200-char stub
                        // lost every full report to restarts/compaction, and
                        // nothing in production ever read the summary's content.
                        if let Err(write_err) = status.mark_completed(output.clone()) {
                            tracing::error!(
                                "Sub-agent {} completed but its status could not be written, so \
                                 it will keep reading as running: {write_err}",
                                agent_id_clone
                            );
                        }
                        manager.complete_and_deliver(
                            &agent_id_clone,
                            output,
                            parent_session_id,
                            &label_clone,
                        );
                    }
                    Err(e) => {
                        // A dropped write here loses the record of a failure
                        // that already happened, and leaves the file reading
                        // `Running` forever. Proceed either way, but say so.
                        if let Err(write_err) = status.mark_failed(e.to_string()) {
                            tracing::error!(
                                "Sub-agent {} failed and its failure status could not be written, \
                                 so it will keep reading as running: {write_err}",
                                agent_id_clone
                            );
                        }
                        if manager.mark_failed(&agent_id_clone, e.to_string()) {
                            push_result(
                                parent_session_id,
                                &label_clone,
                                &agent_id_clone,
                                Err(&e.to_string()),
                            );
                        }
                    }
                }
            }
        });

        // Install the handle on the entry registered before the spawn (#1082).
        // The entry already exists, so the task may already have run and
        // marked this agent terminal by the time control reaches here;
        // `set_join_handle` only fills the handle slot and leaves the state
        // alone, so a fast failure is not overwritten back to `Running`.
        self.manager.set_join_handle(&agent_id, handle);

        // The verbs belong HERE, not only in the description (#921). A model
        // reads the schema before it spawns and then gets a receipt that used
        // to forget everything it had just been told, at the exact moment the
        // id it needs first exists. One vocabulary, stated where it is used:
        // send_input steers a live child, resume_agent continues one that
        // ended its turn, agent_progress reads, completion arrives as a new
        // turn.
        Ok(ToolResult::success(format!(
            "Spawned sub-agent '{}' with id: {}\nSession: {}\nAccess: {}\nBrain: {}\n\
             Talk to it: `send_input {}` steers a running child or answers one parked \
             mid-round; `resume_agent {}` continues one that ended its turn to ask; \
             `agent_progress {}` reads its progress without waiting; it wakes you with its \
             result when it finishes, so do not `wait_agent` for it.\nPrompt: {}{}",
            label,
            agent_id,
            child_session_id,
            if effective_read_only {
                "read-only (#1173): reads/search/research only"
            } else {
                "full (minus recursive/dangerous tools)"
            },
            super::brain::brain_status_label(include_brain),
            agent_id,
            agent_id,
            agent_id,
            prompt,
            deprecation_note
                .as_deref()
                .map(|n| format!("\nNote: {n}"))
                .unwrap_or_default()
        )))
    }
}
