//! team_create tool — spawn a named team of agents from a single command.

use super::manager::TeamManager;
use crate::brain::agent::service::work_status::AgentGrant;
use crate::brain::tools::error::{Result, ToolError};
use crate::brain::tools::subagent::manager::{SubAgent, SubAgentManager};
use crate::brain::tools::subagent::map_deprecated_agent_type;
use crate::brain::tools::subagent::worktree::{self, SubagentWorktree};
use crate::brain::tools::r#trait::{Tool, ToolCapability, ToolExecutionContext, ToolResult};
use async_trait::async_trait;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// The directory one team member is handed, and the private checkout behind it
/// when one was cut (#1064).
pub(crate) struct MemberWorkspace {
    /// What `with_working_directory` receives.
    pub(crate) dir: PathBuf,
    /// The checkout cut for this member, `None` when it works in the parent's
    /// tree — read-only members, or any member outside a git checkout.
    pub(crate) worktree: Option<SubagentWorktree>,
}

/// Resolve where a team member works (#1064).
///
/// `spawn_agent` gives every child its own `git worktree` (#1151); `team_create`
/// used to hand every member the parent's own directory, so a team collided by
/// construction while both surfaces' descriptions implied otherwise. This is
/// the same call the spawn path makes, keyed on the member's own agent id.
///
/// A READ-ONLY member deliberately keeps the parent's tree. It cannot write:
/// `restrict_registry_to_read_only` strips every tool whose `read_only` hint is
/// false — `write_file`, `edit_file`, `hashline_edit`, `bash`, `execute_code`
/// and every other mutator. And a reviewer must see the parent's UNCOMMITTED
/// work; a fresh checkout at the same commit would show it stale bytes and have
/// it review the wrong revision.
///
/// `worktree::create` returns `None` outside a git checkout, and that is a
/// normal outcome: the member runs where it always did, and the result says so.
pub(crate) fn member_workspace(
    parent_dir: &Path,
    agent_id: &str,
    read_only: bool,
) -> MemberWorkspace {
    let worktree = if read_only {
        None
    } else {
        worktree::create(parent_dir, agent_id)
    };
    let dir = worktree
        .as_ref()
        .map(|w| w.path.clone())
        .unwrap_or_else(|| parent_dir.to_path_buf());
    MemberWorkspace { dir, worktree }
}

/// One member's entry in what `team_create` returns (#1064).
///
/// The issue's second complaint is that "the parent cannot even tell that
/// isolation was not provided". Naming the directory — and, where one was cut,
/// the branch — is the whole answer: a parent reading the result can tell
/// isolated from shared without reconstructing the naming scheme from source.
pub(crate) fn member_report_line(
    label: &str,
    agent_id: &str,
    read_only: bool,
    workspace: &MemberWorkspace,
    grant_note: Option<&str>,
) -> String {
    let location = match (&workspace.worktree, read_only) {
        (Some(wt), _) => format!(
            "own checkout `{}` on branch `{}`",
            wt.path.display(),
            wt.branch
        ),
        (None, true) => format!(
            "your directory `{}` (read-only member: no checkout cut, so it sees \
             your uncommitted work)",
            workspace.dir.display()
        ),
        (None, false) => format!(
            "your directory `{}` (no checkout cut: not a git checkout, or git \
             refused — this member is NOT isolated)",
            workspace.dir.display()
        ),
    };
    format!(
        "  {} ({}) → {}{}\n    works in: {}",
        label,
        if read_only { "read-only" } else { "full" },
        agent_id,
        grant_note.map(|n| format!(" [{n}]")).unwrap_or_default(),
        location
    )
}

/// Tool that spawns a named team of sub-agents from a list of tasks.
pub struct TeamCreateTool {
    subagent_manager: Arc<SubAgentManager>,
    team_manager: Arc<TeamManager>,
    parent_registry: Arc<crate::brain::tools::ToolRegistry>,
}

impl TeamCreateTool {
    pub fn new(
        subagent_manager: Arc<SubAgentManager>,
        team_manager: Arc<TeamManager>,
        parent_registry: Arc<crate::brain::tools::ToolRegistry>,
    ) -> Self {
        Self {
            subagent_manager,
            team_manager,
            parent_registry,
        }
    }
}

#[async_trait]
impl Tool for TeamCreateTool {
    fn name(&self) -> &str {
        "team_create"
    }

    fn description(&self) -> &str {
        "Create a named team by spawning multiple sub-agents at once. Each agent gets its own \
         task and optional type. Returns team name and all agent IDs. \
         \n\nISOLATION (same contract as spawn_agent): every WRITABLE member is cut its own \
         `git worktree` on a `subagent/<agent-id>` branch, so members of one team never \
         clobber each other or you. A member that leaves work behind keeps its checkout and \
         the branch is named in its result, so you can review and merge it. `read_only` \
         members deliberately stay in YOUR directory — they cannot write, and a reviewer must \
         see your uncommitted work rather than a stale checkout. Outside a git repository no \
         checkout can be cut and every member shares your directory; the result states, per \
         member, exactly which directory it was given and which branch (if any) it is on, so \
         you never have to guess whether isolation happened. \
         \n\nProvider and model resolution is PER MEMBER. Each entry in the `agents` array \
         can carry its own `provider` and `model` fields, with the same precedence as \
         spawn_agent: per-member > config.agent.subagent_* > parent. This lets one \
         team_create call spawn agents that each use a different model — useful when a \
         skill orchestrates a plan-with-GLM / code-with-Deepseek / review-with-Kimi flow \
         as one atomic team rather than chained spawn_agent calls. \
         \n\nEach member is watched like a spawned sub-agent: when one has produced no new \
         output for ten minutes the daemon tells you in your own context, unprompted — you \
         will be told, so do not poll the members and do not spawn replacements for work \
         that is still running."
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "team_name": {
                    "type": "string",
                    "description": "Unique name for this team (e.g., 'backend-refactor', 'test-suite')"
                },
                "agents": {
                    "type": "array",
                    "description": "List of agents to spawn",
                    "items": {
                        "type": "object",
                        "properties": {
                            "prompt": {
                                "type": "string",
                                "description": "Task for this agent"
                            },
                            "label": {
                                "type": "string",
                                "description": "Short label for this agent"
                            },
                            "allow_nested": {
                                "type": "boolean",
                                "description": "Whether THIS MEMBER may spawn further sub-agents or background tasks (#1195). Default true. false = pure worker."
                            },
                            "read_only": {
                                "type": "boolean",
                                "description": "Spawn this member with a read-restricted tool registry (#1173): reads/search/research only. Omit or false for full capability. A read-only member also keeps YOUR working directory instead of getting its own checkout (#1064) — it cannot write, and it must see your uncommitted work."
                            },
                            "include_brain": {
                                "type": "boolean",
                                "description": "Whether to pre-inject workspace brain files (SOUL.md, USER.md, AGENTS.md) into this team member (#145). Default false (lean execution). Set true when delegating full co-worker tasks."
                            },
                            "provider": {
                                "type": "string",
                                "description": "Optional per-member provider override (e.g., 'zhipu', 'openrouter', 'custom:my-provider'). Highest precedence — overrides config.agent.subagent_provider for THIS member only."
                            },
                            "model": {
                                "type": "string",
                                "description": "Optional per-member model override. Highest precedence — overrides config.agent.subagent_model for THIS member only."
                            }
                        },
                        "required": ["prompt"]
                    }
                }
            },
            "required": ["team_name", "agents"]
        })
    }

    fn capabilities(&self) -> Vec<ToolCapability> {
        vec![ToolCapability::SystemModification]
    }

    fn requires_approval(&self) -> bool {
        true
    }

    async fn execute(&self, input: Value, context: &ToolExecutionContext) -> Result<ToolResult> {
        // Nesting enforcement (#1195), mirroring spawn_agent.
        if let Some(ref mgr) = context.subagent_manager
            && !mgr.nesting_allowed_for_session(context.session_id)
        {
            return Ok(ToolResult::error(
                "refused: this agent was spawned without nesting permissions \
                 (allow_nested=false) and may not create teams"
                    .to_string(),
            ));
        }
        let team_name = input
            .get("team_name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidInput("'team_name' is required".into()))?
            .to_string();

        let agents_array = input
            .get("agents")
            .and_then(|v| v.as_array())
            .ok_or_else(|| ToolError::InvalidInput("'agents' must be an array".into()))?;

        if agents_array.is_empty() {
            return Err(ToolError::InvalidInput(
                "'agents' array cannot be empty".into(),
            ));
        }

        if self.team_manager.exists(&team_name) {
            return Err(ToolError::InvalidInput(format!(
                "Team '{}' already exists",
                team_name
            )));
        }

        let service_context = context
            .service_context
            .as_ref()
            .ok_or_else(|| ToolError::Execution("No service context available".into()))?
            .clone();

        let config = crate::config::Config::load()
            .map_err(|e| ToolError::Execution(format!("Config load failed: {}", e)))?;

        let mut spawned_ids = Vec::new();
        let mut spawn_results = Vec::new();
        // #1064: counted as members are placed, not re-derived from the
        // rendered text — a count read back out of a string is a count that
        // silently breaks the day the wording changes.
        let mut isolated = 0usize;

        for agent_def in agents_array {
            let prompt = agent_def
                .get("prompt")
                .and_then(|v| v.as_str())
                .ok_or_else(|| ToolError::InvalidInput("Each agent needs a 'prompt'".into()))?
                .to_string();

            let label = agent_def
                .get("label")
                .and_then(|v| v.as_str())
                .unwrap_or("team-member")
                .to_string();

            // Per-member capability grant (#1173): explicit read_only wins;
            // a deprecated typed agent_type maps to its historical grant,
            // loudly; unknown types fail closed for THIS member only.
            let member_read_only = match agent_def.get("read_only") {
                Some(v) => Some(v.as_bool().ok_or_else(|| {
                    ToolError::InvalidInput(format!(
                        "Team member '{label}': 'read_only' must be a boolean"
                    ))
                })?),
                None => None,
            };

            let deprecated_raw = agent_def
                .get("agent_type")
                .and_then(|v| v.as_str())
                .map(str::to_string);
            // Nesting grant (#1195): absent = true; non-boolean = hard error.
            let member_allow_nested = match agent_def.get("allow_nested") {
                None => true,
                Some(serde_json::Value::Bool(b)) => *b,
                Some(_) => {
                    return Ok(ToolResult::error(format!(
                        "Team member '{label}': 'allow_nested' must be a boolean"
                    )));
                }
            };

            // Brain pre-injection flag (#145): absent = false; non-boolean = hard error.
            let member_include_brain = match agent_def.get("include_brain") {
                None => false,
                Some(serde_json::Value::Bool(b)) => *b,
                Some(_) => {
                    return Ok(ToolResult::error(format!(
                        "Team member '{label}': 'include_brain' must be a boolean"
                    )));
                }
            };

            let (read_only, member_grant_note) = match (member_read_only, deprecated_raw) {
                (Some(ro), _) => (ro, None),
                (None, Some(raw)) => {
                    let grant = map_deprecated_agent_type(&raw)
                        .map_err(|e| ToolError::InvalidInput(format!("{label}: {e}")))?;
                    tracing::warn!(
                        "team_create member '{label}' uses deprecated agent_type='{raw}'; \
                         mapped to read_only={grant} (#1173)"
                    );
                    (
                        grant,
                        Some(format!("deprecated agent_type '{raw}' → read_only={grant}")),
                    )
                }
                (None, None) => (false, None),
            };

            // Per-member provider / model overrides (issue #152). Same
            // precedence as spawn_agent: per-member > config > parent.
            // Resolved here inside the loop so each team member can
            // route to a different model in a single team_create call
            // — that's the orchestration shape the issue requested.
            let member_provider = agent_def
                .get("provider")
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string);
            let member_model = agent_def
                .get("model")
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string);
            // Per-member > config, normalised the same way spawn resolves
            // it (#1316).
            let child = super::super::provider_pair::child_pair(
                &config,
                member_provider.as_deref(),
                member_model.as_deref(),
            );
            let model_override = child.model.clone();
            let effective_provider_name = child.provider.clone();

            // Create session for this agent
            let session_service = crate::services::SessionService::new(service_context.clone());
            let child_session = session_service
                .create_session(Some(format!("team:{}/{}", team_name, label)))
                .await
                .map_err(|e| ToolError::Execution(format!("Failed to create session: {}", e)))?;

            let child_session_id = child_session.id;
            let agent_id = SubAgentManager::generate_id();

            let cancel_token = CancellationToken::new();
            let (input_tx, mut input_rx) = mpsc::unbounded_channel::<String>();

            // Create provider — per-member override wins over config.
            let provider = if let Some(ref provider_name) = effective_provider_name {
                match crate::brain::provider::create_provider_by_name(&config, provider_name).await
                {
                    Ok(p) => {
                        let source = match child.source {
                            super::super::provider_pair::ProviderSource::PerCall => "per-member",
                            super::super::provider_pair::ProviderSource::Config => "config",
                        };
                        tracing::info!(
                            "Team member '{label}' using {source} provider '{provider_name}'"
                        );
                        p
                    }
                    Err(_) => crate::brain::provider::create_provider(&config)
                        .await
                        .map_err(|e| {
                            ToolError::Execution(format!(
                                "Fallback provider creation failed: {}",
                                e
                            ))
                        })?,
                }
            } else {
                crate::brain::provider::create_provider(&config)
                    .await
                    .map_err(|e| ToolError::Execution(format!("Provider creation failed: {}", e)))?
            };

            // Build the member's registry through the ONE gated constructor
            // (#1075, #1079). team_create used to call the raw strip behind
            // `if read_only` and never asked the parent's plan state at all, so
            // a team member of a Plan-mode Editing parent got the full registry
            // with write+bash while a spawned child of the same parent was
            // stripped — the #1075 escalation. Team members are never the
            // plan-review worker: that child is minted only by the plan card's
            // Review button, one at a time, and it is not a team.
            //
            // The grant records the CALLER's grant only, never the Editing
            // strip: resume re-derives that from the plan state, so a member
            // whose parent later leaves Editing regains write access instead of
            // being locked out for life (#1079).
            let member_grant =
                AgentGrant::new(read_only, member_include_brain, member_allow_nested);
            let child_registry = super::super::build_child_registry_gated(
                &self.parent_registry,
                &self.subagent_manager,
                // The member's own session is fresh (create_session above) and
                // resolves to NoPlan; the TEAM CREATOR's plan state is the one
                // that governs it.
                context.session_id,
                &member_grant,
                false,
            )
            .await;
            if read_only {
                tracing::info!(
                    "Team member '{label}' spawned read-only: registry restricted (#1173)"
                );
            }
            // Does the Editing-parent rule strip this member even though the
            // caller granted write access? Asked HERE, from the same predicate
            // the registry build used, so the checkout decision and the report
            // below cannot claim a capability the registry just removed
            // (#1075). Kept local and NOT folded into `member_grant`: see the
            // comment above.
            let effective_read_only = read_only
                || crate::brain::tools::plan_gate::editing_parent_strips(context.session_id, false)
                    .await;
            if effective_read_only && !read_only {
                tracing::info!(
                    "Team member '{label}' restricted to read-only: the creating session \
                     is in Plan-mode Editing (#649)"
                );
            }

            // A private checkout per WRITABLE member (#1064), the same call
            // spawn.rs:574 makes, keyed on this member's own id. Cut before
            // the child is built so its working directory points at the new
            // tree from the first turn. Read-only members keep the parent's
            // tree on purpose — see `member_workspace`.
            let workspace =
                member_workspace(&context.working_dir(), &agent_id, effective_read_only);

            let system_brain = super::super::brain::child_system_brain(
                member_include_brain,
                &workspace.dir,
                model_override.as_deref(),
                effective_provider_name.as_deref(),
            );

            let mut child_service_builder =
                crate::brain::agent::AgentService::new(provider, service_context.clone(), &config)
                    .await
                    .with_tool_registry(child_registry)
                    .with_auto_approve_tools(true)
                    .with_working_directory(workspace.dir.clone())
                    .with_headless(true);

            if let Some(brain) = system_brain {
                child_service_builder = child_service_builder.with_system_brain(brain);
            }

            let child_service = Arc::new(child_service_builder);

            // Typed preambles are gone (#1173, Proposal B): restricted
            // members get one factual capability line, full members just the
            // task.
            // #145: stacked capability + lean context note.
            //
            // `effective_read_only`, not the raw grant: the capability note is
            // read by the CHILD, so it must describe the registry the child
            // actually got. A member stripped by the #649 Editing-parent rule
            // holds no write tools, and telling it otherwise would have it
            // attempt writes it cannot perform (#1075).
            let full_prompt = super::super::brain::child_prompt(
                effective_read_only,
                member_include_brain,
                &prompt,
            );

            let cancel_clone = cancel_token.clone();
            let manager = self.subagent_manager.clone();
            let agent_id_clone = agent_id.clone();
            let model_clone = model_override.clone();
            // Delivery identity (#1197): team members must wake the parent
            // on completion, same contract as spawned agents.
            let parent_of_member = context.session_id;
            let member_label = label.clone();
            // The tree settles inside the task, so a member that finished can
            // return its checkout before the result is delivered (#1064).
            let worktree_for_cleanup = workspace.worktree.clone();

            let handle = tokio::spawn(async move {
                tracing::info!("Team agent {} starting", agent_id_clone);

                let mut current_prompt = full_prompt;

                let outcome: std::result::Result<String, String> = loop {
                    let result = child_service
                        .send_message_with_tools_and_mode(
                            child_session_id,
                            current_prompt,
                            model_clone.clone(),
                            Some(cancel_clone.clone()),
                        )
                        .await;

                    match result {
                        Ok(response) => {
                            manager.update_output(&agent_id_clone, response.content.clone());
                            // Natural completion (#1184), same rule as
                            // spawn.rs: only a genuinely gated round keeps
                            // waiting; a finished answer delivers instead of
                            // parking the agent forever.
                            if response.stop_reason
                                != Some(crate::brain::provider::types::StopReason::ToolUse)
                            {
                                tracing::info!(
                                    "Team agent {} round complete naturally, delivering result",
                                    agent_id_clone
                                );
                                break Ok(response.content);
                            }
                            manager.mark_awaiting_input(&agent_id_clone);
                            tracing::info!(
                                "Team agent {} round complete, waiting for input",
                                agent_id_clone
                            );

                            let next = tokio::select! {
                                msg = input_rx.recv() => msg,
                                _ = cancel_clone.cancelled() => {
                                    tracing::info!("Team agent {} cancelled while waiting for input", agent_id_clone);
                                    None
                                }
                            };

                            match next {
                                Some(text) => {
                                    // Flip back to Running so the in-memory
                                    // state matches the round now in flight —
                                    // without this a wait_agent during the new
                                    // round reads the parked state and returns
                                    // the PREVIOUS round's stale output (#1183).
                                    manager.mark_running_again(&agent_id_clone);
                                    current_prompt = text;
                                }
                                None => break Ok(response.content),
                            }
                        }
                        Err(e) => {
                            // `break`, not `return`: the settle below must run
                            // for a failed member too, exactly as spawn.rs:989
                            // learned in #924 — an early return here leaked the
                            // member's checkout.
                            break Err(e.to_string());
                        }
                    }
                };

                // Return the tree unless the member left work in it (#1064).
                // A dirty tree or a new commit survives: silently discarding a
                // member's work would be worse than the collisions this
                // isolation exists to prevent. Settled BEFORE delivery so a
                // kept tree is named in the result the parent reads.
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

                match outcome {
                    Ok(output) => {
                        let output = settle_worktree(output);
                        manager.complete_and_deliver(
                            &agent_id_clone,
                            output,
                            parent_of_member,
                            &member_label,
                        );
                    }
                    Err(e) => {
                        tracing::error!("Team agent {} failed: {}", agent_id_clone, e);
                        // Nothing is delivered on failure, so the notice is
                        // logged rather than pushed — but the tree still
                        // settles, or a clean failed checkout is never
                        // removed.
                        let notice = settle_worktree(String::new());
                        if !notice.is_empty() {
                            tracing::info!(
                                "Team agent {} failed; worktree {}",
                                agent_id_clone,
                                notice.trim()
                            );
                        }
                        manager.mark_failed(&agent_id_clone, e);
                    }
                }
            });

            // Register in subagent manager
            self.subagent_manager.insert(SubAgent {
                // The CALLER's grant, not the Editing-parent strip (#1079) —
                // resume re-derives that from the creating session's plan
                // state, so a member whose creator later leaves Editing regains
                // write access instead of staying stripped for life.
                read_only,
                include_brain: member_include_brain,
                allow_nested: member_allow_nested,
                // Team members are never the plan-review worker (#1075); the
                // exception belongs to the single child the plan card's Review
                // button mints, never to a fan-out.
                plan_review: false,
                cancel_token,
                join_handle: Some(handle),
                input_tx: Some(input_tx),
                // #960: same contract as a single spawn — the member's brief
                // survives compaction so the parent can check the result.
                task: prompt.clone(),
                // #1064/#960: the tree the member was ACTUALLY given, from the
                // same value its working directory comes from — so a live
                // `agent_progress` read can name the checkout without
                // reconstructing the naming scheme.
                worktree_branch: workspace.worktree.as_ref().map(|w| w.branch.clone()),
                worktree_path: workspace
                    .worktree
                    .as_ref()
                    .map(|w| w.path.display().to_string()),
                ..SubAgent::new(
                    agent_id.clone(),
                    label.clone(),
                    child_session_id,
                    context.session_id,
                )
            });

            spawned_ids.push(agent_id.clone());
            if workspace.worktree.is_some() {
                isolated += 1;
            }
            spawn_results.push(member_report_line(
                &label,
                &agent_id,
                // The EFFECTIVE capability, so the report cannot claim a write
                // grant the registry no longer holds (#1075).
                effective_read_only,
                &workspace,
                member_grant_note.as_deref(),
            ));
        }

        // Register team
        self.team_manager
            .create_team(team_name.clone(), spawned_ids.clone());

        Ok(ToolResult::success(format!(
            "Created team '{}' with {} agents ({isolated} in their own checkout, \
             {} in your directory):\n{}\n\
             Each member's result names any checkout it left behind; merge a branch \
             with `git merge <branch>` from your tree.",
            team_name,
            spawned_ids.len(),
            spawn_results.len() - isolated,
            spawn_results.join("\n")
        )))
    }
}
