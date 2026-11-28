//! resume_agent tool — resumes a completed/failed child agent with new input.

use super::manager::{SubAgentManager, SubAgentState};
use crate::brain::tools::error::{Result, ToolError};
use crate::brain::tools::r#trait::{Tool, ToolCapability, ToolExecutionContext, ToolResult};
use async_trait::async_trait;
use serde_json::Value;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Tool that resumes a previously completed or failed sub-agent.
pub struct ResumeAgentTool {
    manager: Arc<SubAgentManager>,
    parent_registry: Arc<crate::brain::tools::ToolRegistry>,
}

impl ResumeAgentTool {
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
impl Tool for ResumeAgentTool {
    fn name(&self) -> &str {
        "resume_agent"
    }

    fn description(&self) -> &str {
        "Resume a completed, failed or stopped sub-agent with a new prompt. \
         The agent continues in the same session, preserving its prior context. \
         \n\nThis is the verb for a child whose TURN HAS ENDED — state Completed, Failed \
         or Cancelled. That includes a child that reported a `blocker` or a `question`: \
         reporting one ends its turn, so it is Completed and resumable, NOT gone. \
         `send_input` refuses those; `send_input` is for a child still Running or parked \
         mid-round. \
         \n\nProvider and model resolution follows the same precedence as spawn_agent: \
         (1) the optional `provider` / `model` parameters on THIS call, \
         (2) the user's config.toml `[agent]` keys `subagent_provider` / `subagent_model`, \
         (3) the parent session's provider. Resuming with a different model is useful when \
         the original spawn used a cheap/fast model for a draft and the resume should \
         escalate to a stronger model for a fix-up pass."
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "agent_id": {
                    "type": "string",
                    "description": "The ID of the sub-agent to resume"
                },
                "prompt": {
                    "type": "string",
                    "description": "New instruction/prompt for the resumed agent"
                },
                "provider": {
                    "type": "string",
                    "description": "Optional provider override for THIS resume (e.g., 'zhipu', 'openrouter', 'custom:my-provider'). Highest precedence — overrides config.agent.subagent_provider."
                },
                "model": {
                    "type": "string",
                    "description": "Optional model override for THIS resume (model id as the chosen provider accepts it). Highest precedence — overrides config.agent.subagent_model."
                }
            },
            "required": ["agent_id", "prompt"]
        })
    }

    fn capabilities(&self) -> Vec<ToolCapability> {
        vec![ToolCapability::SystemModification]
    }

    fn requires_approval(&self) -> bool {
        true
    }

    async fn execute(&self, input: Value, context: &ToolExecutionContext) -> Result<ToolResult> {
        let agent_id = input
            .get("agent_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidInput("'agent_id' is required".into()))?;

        let prompt = input
            .get("prompt")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidInput("'prompt' is required".into()))?
            .to_string();
        // #129: a resumed child is still a headless session — re-assert the
        // final-message law on every resume (the original spawn's preamble
        // was consumed by the first run's context).
        let prompt = format!(
            "{}{}\n\n{prompt}",
            crate::cli::tool_setup::HEADLESS_PREAMBLE.trim_start_matches('\n'),
            // #859: same channel as the original spawn — a resumed child is
            // just as able to park for an answer as a fresh one.
            crate::cli::tool_setup::SUBAGENT_ANSWER_CHANNEL.trim_start_matches('\n')
        );

        // #852: an agent that outlived the process is absent from the map but
        // present on disk, so resolve BEFORE the state gate. Rehydration
        // restores the agent's frozen grant from its status file, which is
        // what keeps this from widening a read-only child on resume (#1173);
        // the state it lands in is `Interrupted`, resumable by the gate below.
        // Scoped to the caller's own children — a stranger's id still misses.
        if self.manager.resolve(agent_id, context.session_id).is_none() {
            return Ok(ToolResult::error(format!(
                "no agent {agent_id} belongs to this session (after a restart an \
                 agent is reachable only by its full id)"
            )));
        }

        // Check agent exists and is in a resumable state
        match self.manager.get_state(agent_id) {
            None => {
                return Ok(ToolResult::error(format!(
                    "No sub-agent found with id: {}",
                    agent_id
                )));
            }
            Some(SubAgentState::Running) | Some(SubAgentState::AwaitingInput) => {
                return Ok(ToolResult::error(format!(
                    "Sub-agent {} is still running. Use wait_agent first or stop_agent to cancel.",
                    agent_id
                )));
            }
            // #924: a stopped agent is resumable. The stop ended its work, not
            // its identity — session, transcript and grant all survive.
            //
            // #1080: `Interrupted` is resumable for the same reason, and the
            // gate now says so through `is_resumable` rather than repeating the
            // list. It used to be accepted only because the disk projection
            // collapsed it into `Failed(_)`; when that projection was corrected
            // a second copy of this list would have started refusing exactly
            // the child whose work survived the restart.
            Some(state) if state.is_resumable() => {}
            Some(state) => {
                return Ok(ToolResult::error(format!(
                    "Sub-agent {} is in state {} and cannot be resumed.",
                    agent_id,
                    state.label()
                )));
            }
        }

        let session_id = self.manager.get_session_id(agent_id).ok_or_else(|| {
            ToolError::Execution(format!("No session found for sub-agent {}", agent_id))
        })?;

        let service_context = context
            .service_context
            .as_ref()
            .ok_or_else(|| ToolError::Execution("No service context available".into()))?
            .clone();

        // Create new cancel token and input channel
        let cancel_token = CancellationToken::new();
        let (input_tx, input_rx) = mpsc::unbounded_channel::<String>();

        // Prepare the agent for resumption
        let agent_id_str = agent_id.to_string();
        if !self
            .manager
            .prepare_resume(&agent_id_str, cancel_token.clone(), input_tx)
        {
            return Ok(ToolResult::error(format!(
                "Failed to prepare sub-agent {} for resumption",
                agent_id
            )));
        }

        // Per-call provider / model overrides (issue #152). Same
        // precedence as spawn_agent: per-call > config > parent.
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

        // Build a new AgentService for the resumed run
        let config = crate::config::Config::load()
            .map_err(|e| ToolError::Execution(format!("Config load failed: {}", e)))?;
        // Per-call > config, normalised the same way spawn resolves it (#1316).
        let child = super::provider_pair::child_pair(
            &config,
            call_provider.as_deref(),
            call_model.as_deref(),
        );
        let subagent_model = child.model.clone();
        let effective_provider_name = child.provider.clone();

        let child_service = {
            let provider = if let Some(ref provider_name) = effective_provider_name {
                match crate::brain::provider::create_provider_by_name(&config, provider_name).await
                {
                    Ok(p) => {
                        let source = match child.source {
                            super::provider_pair::ProviderSource::PerCall => "per-call",
                            super::provider_pair::ProviderSource::Config => "config",
                        };
                        tracing::info!(
                            "Resumed sub-agent using {source} provider '{provider_name}'"
                        );
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

            // Rebuild the child's registry from its FROZEN grant (#1173,
            // fixes F2): resume used to hand back a full General registry
            // regardless of how the child was originally spawned, silently
            // widening an explore-style child into full write access. The
            // stored read_only flag is the single source of truth for the
            // agent's whole life.
            let child_registry = super::build_child_registry(&self.parent_registry, &self.manager);
            if self.manager.get_read_only(agent_id).unwrap_or(false) {
                crate::brain::tools::plan_gate::restrict_registry_to_read_only(&child_registry);
                tracing::info!(
                    "Resumed sub-agent {agent_id} holds a read-only grant: \
                     registry rebuilt restricted (#1173)"
                );
            }

            let child_dir = context.working_dir();
            let include_brain = self.manager.get_include_brain(agent_id).unwrap_or(false);
            let system_brain = super::brain::child_system_brain(
                include_brain,
                &child_dir,
                subagent_model.as_deref(),
                effective_provider_name.as_deref(),
            );

            let mut builder =
                crate::brain::agent::AgentService::new(provider, service_context, &config)
                    .await
                    .with_tool_registry(child_registry)
                    .with_auto_approve_tools(true)
                    .with_working_directory(context.working_dir())
                    // #129: a resumed child is still headless (owner ruling C).
                    .with_headless(true);

            if let Some(brain) = system_brain {
                builder = builder.with_system_brain(brain);
            }

            Arc::new(builder)
        };

        // Spawn resumed task with input loop
        let cancel_clone = cancel_token.clone();
        let manager = self.manager.clone();
        let agent_id_clone = agent_id_str.clone();
        let prompt_clone = prompt.clone();
        // Delivery identity (#1197): on natural completion the parent must
        // be woken even though nobody registered a waiter for this resume.
        let parent_of_child = self.manager.get_parent_session_id(agent_id);
        let child_label = self.manager.get_label(agent_id);
        let model_override = subagent_model;
        let mut input_rx = input_rx;

        // #1080: the resumed round OWNS its status record, exactly as the spawn
        // task owns its own (spawn.rs:806-830). Before this, `resume_agent`
        // wrote no `WorkStatus` at all — `grep -c WorkStatus resume.rs` was 0 —
        // so a resumed child kept the PREVIOUS round's `state: Completed` and
        // its `output_full`, and every disk-based surface (`task_output`,
        // `tasks_list`, `agent_progress`) reported the last round's report as
        // this round's result. That is a silent wrong answer, not a crash, and
        // it is the class this step removes.
        //
        // `None` when there is no record to own: an agent whose file was swept,
        // or a `team_create` member, which never writes one. The round then has
        // no disk mirror, which is the pre-#1080 behaviour — deliberately not
        // "fixed" by inventing a record, because a fabricated file would make
        // the child visible to every disk-based surface as a spawned agent it
        // is not.
        let mut resume_status =
            crate::brain::agent::service::work_status::WorkStatus::read(&agent_id_str);
        if resume_status.is_none() {
            tracing::debug!(
                "Sub-agent {agent_id_str} resumed with no status record; the round will not \
                 be mirrored to disk"
            );
        }

        let handle = tokio::spawn(async move {
            tracing::info!("Sub-agent {} resuming: {}", agent_id_clone, prompt_clone);

            // #1080: begin the NEW round on disk before the first turn. This is
            // what stops the previous round's `output_full` being read as this
            // round's result, and what puts the record back on the unfinished
            // schedule so a resume killed by a restart reads `Interrupted`
            // rather than a completion that predates it.
            if let Some(status) = resume_status.as_mut()
                && let Err(e) = status.begin_round()
            {
                tracing::warn!(
                    "Sub-agent {} resumed but its status could not be marked running: {e}",
                    agent_id_clone
                );
            }

            let mut current_prompt = prompt_clone;
            let mut iteration: usize = 0;

            // Run prompt → wait for input → run again loop
            let final_output = loop {
                iteration += 1;
                let result = child_service
                    .send_message_with_tools_and_mode(
                        session_id,
                        current_prompt,
                        model_override.clone(),
                        Some(cancel_clone.clone()),
                    )
                    .await;

                match result {
                    Ok(response) => {
                        manager.update_output(&agent_id_clone, response.content.clone());
                        // #1080: record the round on disk, the way spawn.rs
                        // does at its own round boundary. `reload` first, so a
                        // field another writer touched between rounds is not
                        // clobbered by this handle's copy.
                        if let Some(status) = resume_status.as_mut() {
                            status.reload();
                            let summary = if response.stop_reason
                                == Some(crate::brain::provider::types::StopReason::ToolUse)
                            {
                                "tool call(s) completed".to_string()
                            } else {
                                response.content.chars().take(120).collect::<String>()
                            };
                            if let Err(e) = status.update_progress(iteration, None, Some(summary)) {
                                tracing::warn!(
                                    "Sub-agent {} status write failed: {e}",
                                    agent_id_clone
                                );
                            }
                        }
                        // Natural completion (#1184), same rule as spawn.rs:
                        // only a genuinely gated round keeps waiting; a
                        // finished answer completes the resumed run instead of
                        // re-parking it forever.
                        if response.stop_reason
                            != Some(crate::brain::provider::types::StopReason::ToolUse)
                        {
                            tracing::info!(
                                "Sub-agent {} resumed round complete naturally, delivering result",
                                agent_id_clone
                            );
                            break response.content;
                        }
                        // Genuinely gated: park for the next input round.
                        manager.mark_awaiting_input(&agent_id_clone);
                        if let Some(status) = resume_status.as_mut()
                            && let Err(e) = status.mark_awaiting_input()
                        {
                            tracing::warn!(
                                "Failed to write awaiting-input status for {}: {e}",
                                agent_id_clone
                            );
                        }
                        tracing::info!(
                            "Sub-agent {} round complete, waiting for input",
                            agent_id_clone
                        );

                        let next = tokio::select! {
                            msg = input_rx.recv() => msg,
                            _ = cancel_clone.cancelled() => {
                                tracing::info!(
                                    "Sub-agent {} cancelled while waiting for input",
                                    agent_id_clone
                                );
                                None
                            }
                        };

                        match next {
                            Some(text) => {
                                // Flip back to Running so the in-memory state
                                // matches the round now in flight — same as
                                // spawn.rs / team-create.rs (#1183). The file
                                // flips with it (#1080), or it keeps reading
                                // `AwaitingInput` while the round runs.
                                manager.mark_running_again(&agent_id_clone);
                                if let Some(status) = resume_status.as_mut()
                                    && let Err(e) = status.mark_running()
                                {
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
                            None => break response.content,
                        }
                    }
                    Err(e) => {
                        // #924: a cancelled turn surfaces here as an error, but
                        // a stop is the parent's decision and must not be
                        // recorded as a fault. Before #1080 this arm marked
                        // every stopped resume `Failed` — and now that the arm
                        // writes to disk, doing so would have put that lie in
                        // the durable record as well.
                        if super::spawn::is_deliberate_stop(&cancel_clone) {
                            tracing::info!(
                                "Sub-agent {} stopped deliberately by its parent mid-round",
                                agent_id_clone
                            );
                            if let Some(status) = resume_status.as_mut()
                                && let Err(write_err) = status.mark_cancelled()
                            {
                                tracing::error!(
                                    "Sub-agent {} was stopped but its cancelled status could \
                                     not be written, so it will keep reading as running: \
                                     {write_err}",
                                    agent_id_clone
                                );
                            }
                            return;
                        }
                        tracing::error!("Sub-agent {} resumed and failed: {}", agent_id_clone, e);
                        // #1080: the failure reaches disk too. A dropped write
                        // here would leave the record reading `Running` for
                        // ever, which every liveness reader takes as "still
                        // working".
                        if let Some(status) = resume_status.as_mut()
                            && let Err(write_err) = status.mark_failed(e.to_string())
                        {
                            tracing::error!(
                                "Sub-agent {} failed and its failure status could not be \
                                 written, so it will keep reading as running: {write_err}",
                                agent_id_clone
                            );
                        }
                        manager.mark_failed(&agent_id_clone, e.to_string());
                        return;
                    }
                }
            };

            // #924/#1080: a deliberate stop ends the WORK, not the agent, and
            // it must not be recorded as a completion — the parent asked for
            // it and is not waiting on a report. The file records `Cancelled`
            // (the same terminal state `stop_agent` writes) and nothing is
            // pushed. Without this the resumed child's exit wrote `Completed`
            // over the stop, both in memory and now on disk.
            if super::spawn::is_deliberate_stop(&cancel_clone) {
                tracing::info!(
                    "Sub-agent {} stopped deliberately by its parent",
                    agent_id_clone
                );
                if let Some(status) = resume_status.as_mut()
                    && let Err(write_err) = status.mark_cancelled()
                {
                    tracing::error!(
                        "Sub-agent {} was stopped but its cancelled status could not be \
                         written, so it will keep reading as running: {write_err}",
                        agent_id_clone
                    );
                }
                return;
            }

            match (parent_of_child, child_label) {
                (Some(parent), Some(label)) => {
                    // `complete_and_deliver` owns the terminal write (#1080):
                    // it persists `Completed` + this round's `output_full`
                    // before pushing, so the disk-based surfaces read the same
                    // answer the parent was just given. Deliberately NOT
                    // duplicated here — two writers of one terminal record is
                    // how the two halves drift apart.
                    manager.complete_and_deliver(&agent_id_clone, final_output, parent, &label);
                }
                _ => {
                    // Legacy entry with no recorded parent: nothing to deliver
                    // to, so this branch bypasses `complete_and_deliver` and
                    // writes the terminal record through the handle instead
                    // (#1080) — otherwise the file keeps reading `Running` for
                    // a child that finished.
                    if let Some(status) = resume_status.as_mut()
                        && let Err(write_err) = status.mark_completed(final_output.clone())
                    {
                        tracing::error!(
                            "Sub-agent {} completed but its status could not be written, so \
                             it will keep reading as running: {write_err}",
                            agent_id_clone
                        );
                    }
                    manager.mark_completed(&agent_id_clone, final_output);
                }
            }
        });

        self.manager.set_join_handle(&agent_id_str, handle);

        Ok(ToolResult::success(format!(
            "Resumed sub-agent {} with new prompt:\n{}",
            agent_id, prompt
        )))
    }
}
