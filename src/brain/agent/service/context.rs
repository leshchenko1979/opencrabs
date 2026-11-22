use super::builder::AgentService;
use super::prompt_file::{BodyValues, CompactionPrompts};
use crate::brain::agent::context::{AgentContext, CompactionScope};
use crate::brain::agent::error::{AgentError, Result};
use crate::brain::provider::{ContentBlock, LLMRequest, Message, Provider};
use crate::services::{MessageService, SessionService};
use std::path::PathBuf;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct ContextManifest {
    #[serde(default)]
    pub active_skills: Vec<String>,
    #[serde(default)]
    pub discard_skills: Vec<String>,
    #[serde(default)]
    pub required_tools: Vec<String>,
    /// Documents the session read whole that must survive the compaction
    /// (#831). Paths, never bodies: the re-injection at wake reads them again
    /// inside the retained-set budget, so a document that grew or was deleted
    /// between compaction and wake is handled by the read, not by a stale copy.
    #[serde(default)]
    pub retained_documents: Vec<String>,
}

/// Extract and parse a machine-readable `ContextManifest` from a compaction summary.
///
/// Searches for a fenced code block tagged `context-manifest` (or `context_manifest`),
/// and parses it either as YAML or JSON. Fails soft (returns `None`) if omitted or malformed.
pub fn parse_context_manifest(summary: &str) -> Option<ContextManifest> {
    let block = extract_manifest_block(summary)?;
    parse_manifest_text(&block)
}

fn extract_manifest_block(summary: &str) -> Option<String> {
    let mut in_block = false;
    let mut fence_char = '`';
    let mut fence_len = 3;
    let mut content = String::new();

    for line in summary.lines() {
        let trimmed = line.trim();
        if !in_block {
            if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
                let f_char = if trimmed.starts_with('`') { '`' } else { '~' };
                let flen = trimmed.chars().take_while(|&c| c == f_char).count();
                let tag = trimmed[flen..].trim().to_lowercase();
                if tag == "context-manifest" || tag == "context_manifest" {
                    in_block = true;
                    fence_char = f_char;
                    fence_len = flen;
                    continue;
                }
            }
        } else {
            let chars_count = trimmed.chars().take_while(|&c| c == fence_char).count();
            if chars_count >= fence_len && trimmed[chars_count..].trim().is_empty() {
                return Some(content);
            }
            content.push_str(line);
            content.push('\n');
        }
    }
    if in_block && !content.trim().is_empty() {
        return Some(content);
    }
    None
}

pub fn parse_manifest_text(text: &str) -> Option<ContextManifest> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Ok(manifest) = serde_json::from_str::<ContextManifest>(trimmed) {
        return Some(manifest);
    }

    let mut manifest = ContextManifest::default();
    let mut current_section: Option<&str> = None;
    let mut found_any_key = false;

    for line in trimmed.lines() {
        let line_trimmed = line.trim();
        if line_trimmed.is_empty() || line_trimmed.starts_with('#') {
            continue;
        }

        if let Some(rest) = line_trimmed.strip_prefix("active_skills:") {
            current_section = Some("active_skills");
            found_any_key = true;
            let rest_trimmed = rest.trim();
            if !rest_trimmed.is_empty() {
                parse_inline_items(rest_trimmed, &mut manifest.active_skills);
            }
        } else if let Some(rest) = line_trimmed.strip_prefix("discard_skills:") {
            current_section = Some("discard_skills");
            found_any_key = true;
            let rest_trimmed = rest.trim();
            if !rest_trimmed.is_empty() {
                parse_inline_items(rest_trimmed, &mut manifest.discard_skills);
            }
        } else if let Some(rest) = line_trimmed.strip_prefix("required_tools:") {
            current_section = Some("required_tools");
            found_any_key = true;
            let rest_trimmed = rest.trim();
            if !rest_trimmed.is_empty() {
                parse_inline_items(rest_trimmed, &mut manifest.required_tools);
            }
        } else if let Some(rest) = line_trimmed.strip_prefix("retained_documents:") {
            current_section = Some("retained_documents");
            found_any_key = true;
            let rest_trimmed = rest.trim();
            if !rest_trimmed.is_empty() {
                parse_inline_items(rest_trimmed, &mut manifest.retained_documents);
            }
        } else if let Some(item) = line_trimmed.strip_prefix('-') {
            let item_clean = item.trim().trim_matches('"').trim_matches('\'').trim();
            if !item_clean.is_empty() {
                match current_section {
                    Some("active_skills") => manifest.active_skills.push(item_clean.to_string()),
                    Some("discard_skills") => manifest.discard_skills.push(item_clean.to_string()),
                    Some("required_tools") => manifest.required_tools.push(item_clean.to_string()),
                    Some("retained_documents") => {
                        manifest.retained_documents.push(item_clean.to_string());
                    }
                    _ => {}
                }
            }
        }
    }

    if found_any_key { Some(manifest) } else { None }
}

fn parse_inline_items(s: &str, target: &mut Vec<String>) {
    let s = s.trim();
    if s.starts_with('[') && s.ends_with(']') {
        let inner = &s[1..s.len() - 1];
        for part in inner.split(',') {
            let clean = part.trim().trim_matches('"').trim_matches('\'').trim();
            if !clean.is_empty() {
                target.push(clean.to_string());
            }
        }
    } else {
        let clean = s.trim_matches('"').trim_matches('\'').trim();
        if !clean.is_empty() && clean != "[]" {
            target.push(clean.to_string());
        }
    }
}

/// Resolve the `ContextManifest` to apply after a compaction summary.
///
/// Unlike [`parse_context_manifest`], this never returns `None`. A document
/// that carries no `context-manifest` fence resolves to the pruning-safe
/// default — keep every active skill, discard none, activate no lazy tool —
/// and the substitution is warned about, because a missing fence is a
/// degradation the reload path must not swallow silently (#1933).
pub fn resolve_context_manifest(summary: &str) -> ContextManifest {
    if let Some(manifest) = parse_context_manifest(summary) {
        return manifest;
    }
    tracing::warn!(
        "compaction: continuation document carried no `context-manifest` fence; \
         substituting the pruning-safe default (keep-all-active, discard none) — \
         `required_tools` left EMPTY (not derivable from the document)"
    );
    ContextManifest::default()
}

/// One `## N.` section of a continuation document, heading and body verbatim.
struct SummarySection {
    /// The heading line as written, e.g. `## 1. Chronological Analysis`.
    heading: String,
    /// Heading + body, byte-for-byte.
    text: String,
    /// True for blocks a woken agent cannot recover from the document alone.
    must_keep: bool,
}

/// The sections a trimmed document must never lose: the obligation (0), the
/// recovery playbook (7), the next step (8), the continuation message (9) and
/// the manifest (10) — plus every UNNUMBERED section, which is the preamble
/// before the first heading and, more importantly, all three harness decoration
/// blocks (`## PLAN STATE (harness)`, `## Running Sub-Agents`, `## Documents
/// read pre-compaction`). Those carry state a woken agent cannot recover from
/// the document, so they are kept rather than guessed at.
///
/// Section 9 is here for the reason the trimmer's size ordering makes concrete
/// (#934): it is the SHORTEST load-bearing block, so sorting trimmable sections
/// by size descending reached it second, right after §1 — the guard destroyed
/// the wake-up message before it ever touched the prose the sessions ranked
/// least important. The prompt writes §9 before the prose as well, so a
/// `MaxTokens` cut cannot reach it either: two doors, both shut.
fn is_must_keep_section(number: Option<u32>) -> bool {
    matches!(
        number,
        None | Some(0) | Some(7) | Some(8) | Some(9) | Some(10)
    )
}

/// Split a continuation document at its level-2 (`## `) headings, numbered or
/// not. Lossless: the concatenation of the returned sections is the input, byte
/// for byte. Fence-aware — a `##` line inside a code block is content, not a
/// heading.
fn split_summary_sections(summary: &str) -> Vec<SummarySection> {
    let mut sections: Vec<SummarySection> = Vec::new();
    let mut heading = String::new();
    let mut number: Option<u32> = None;
    let mut cur = String::new();
    let mut open: Option<(char, usize)> = None;

    for line in summary.split_inclusive('\n') {
        let trimmed = line.trim_start();
        let fence = if trimmed.starts_with("```") {
            Some('`')
        } else if trimmed.starts_with("~~~") {
            Some('~')
        } else {
            None
        };
        let in_fence_before = open.is_some();
        match (open, fence) {
            (None, Some(c)) => {
                open = Some((c, trimmed.chars().take_while(|&x| x == c).count()));
            }
            (Some((c, len)), Some(c2))
                if c == c2 && trimmed.chars().take_while(|&x| x == c).count() >= len =>
            {
                open = None;
            }
            _ => {}
        }

        if !in_fence_before && fence.is_none() && is_section_heading(line) {
            if !cur.is_empty() {
                sections.push(SummarySection {
                    heading: std::mem::take(&mut heading),
                    text: std::mem::take(&mut cur),
                    must_keep: is_must_keep_section(number),
                });
            }
            number = parse_section_number(line);
            heading = line.trim_end().to_string();
        }
        cur.push_str(line);
    }
    if !cur.is_empty() {
        sections.push(SummarySection {
            heading,
            text: cur,
            must_keep: is_must_keep_section(number),
        });
    }
    sections
}

/// Any level-2 heading line, numbered or not.
///
/// The trimmer has to treat UNNUMBERED headings as section boundaries too,
/// because the harness appends its own blocks that way: `## PLAN STATE
/// (harness)`, `## Running Sub-Agents` and `## Documents read pre-compaction`.
/// Splitting on `## N.` alone absorbed those blocks into the span of the
/// preceding numbered section, so stubbing that section destroyed decoration
/// the trimmer is documented never to reach. Measured on the #934 gate: a
/// fixture whose last section was a trimmable §1 lost its entire documents
/// list, while production masked the bug because its last section (§10) happens
/// to be must-keep. `### ` is excluded — a sub-heading is content, not a
/// boundary.
fn is_section_heading(line: &str) -> bool {
    line.starts_with("## ") && !line.starts_with("### ")
}

/// `## 3. Title` → `Some(3)`. Any other line, `### 3.` included, is `None`.
fn parse_section_number(line: &str) -> Option<u32> {
    let rest = line.strip_prefix("## ")?;
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    if digits.is_empty() || !rest[digits.len()..].starts_with('.') {
        return None;
    }
    digits.parse().ok()
}

/// A dropped section's replacement: its heading, so the document still reads
/// as a document, and a pointer to where the content really lives.
fn section_stub(section: &SummarySection) -> String {
    if section.heading.is_empty() {
        return String::new();
    }
    format!(
        "{}\n[section omitted to fit the compaction budget — recover it from the session \
         store with `session_search`]\n\n",
        section.heading
    )
}

/// Remove the largest fenced code block from `text`, replacing it with a
/// one-line pointer. A fence is removed whole, never cut in half. `None`
/// when no fenced block remains.
fn remove_largest_fenced_block(text: &str) -> Option<String> {
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    let mut blocks: Vec<(usize, usize)> = Vec::new();
    let mut open: Option<(usize, char, usize)> = None;

    for (i, line) in lines.iter().enumerate() {
        let trimmed = line.trim_start();
        let fence = if trimmed.starts_with("```") {
            Some('`')
        } else if trimmed.starts_with("~~~") {
            Some('~')
        } else {
            None
        };
        match (open, fence) {
            (None, Some(c)) => {
                open = Some((i, c, trimmed.chars().take_while(|&x| x == c).count()));
            }
            (Some((start, c, len)), Some(c2))
                if c == c2 && trimmed.chars().take_while(|&x| x == c).count() >= len =>
            {
                blocks.push((start, i + 1));
                open = None;
            }
            _ => {}
        }
    }
    if let Some((start, _, _)) = open {
        blocks.push((start, lines.len()));
    }

    let (start, end) = blocks
        .into_iter()
        .max_by_key(|&(s, e)| lines[s..e].iter().map(|l| l.len()).sum::<usize>())?;

    let mut out = String::new();
    for (i, line) in lines.iter().enumerate() {
        if i == start {
            out.push_str(
                "[fenced block omitted to fit the compaction budget — read it from the file \
                 named above]\n",
            );
        } else if i < start || i >= end {
            out.push_str(line);
        }
    }
    Some(out)
}

/// The §0 status line the prompt mandates, matched case-insensitively so a
/// capitalisation slip is not read as a lost obligation.
fn has_obligation_status(summary: &str) -> bool {
    summary.to_lowercase().contains("obligation status")
}

impl AgentService {
    /// The per-turn plan reminder pinned at the end of the prompt, keyed to
    /// the session's plan-mode state: an Active checklist gets the task
    /// nudge (`format_plan_reminder`), an Editing session (pre-init or
    /// design prose) gets the Editing rules (`format_editing_reminder`),
    /// and NoPlan gets nothing. Loaded through the shared plan store so
    /// legacy statuses map (and terminal ones resolve) first.
    /// The user message as the model should see it: the raw text plus the
    /// per-turn plan reminder and any relevant MEMORY.md recall.
    ///
    /// Both additions are context-only. The DB always stores the clean
    /// `user_message`, so neither can pollute chat history, and neither piles
    /// up across turns.
    ///
    /// Shared because the two message paths had drifted (#995): the tool loop
    /// appended plan reminder AND recall, `prepare_message_context` appended
    /// only the reminder, and the two blocks were otherwise identical. Whether
    /// memory surfaced therefore depended on which code path a session took,
    /// which is not a property of the memory.
    /// Augment the user message for LLM context:
    /// 1. Temporal grounding: inject [Current time: YYYY-MM-DD HH:MM:SS UTC (user: ...)] (#153).
    /// 2. Active plan reminder.
    /// 3. Memory & directive passive recall (#799, #285).
    ///
    /// Returns the augmented text PLUS what recall injected (#928). The summary
    /// is `None` whenever recall fired nothing, so a caller that renders a
    /// receipt can stay silent instead of printing an empty chrome row; this
    /// layer stays channel-agnostic — it names sections, it never formats for a
    /// channel.
    pub(crate) async fn augment_user_message(
        session_id: Uuid,
        user_message: &str,
        brain_dir: Option<&std::path::Path>,
        working_dir: Option<&std::path::Path>,
    ) -> (String, Option<crate::brain::memory_recall::RecallSummary>) {
        // Temporal grounding (#153): inject context-only time marker at turn start.
        let now = chrono::Utc::now();
        let tz_info = brain_dir
            .and_then(|dir| crate::brain::timezone::GLOBAL_TZ_CACHE.resolve_from_brain_dir(dir));
        let time_marker = match tz_info {
            Some(ref info) => format!("[Current time: {}]", info.format_dual_time(&now)),
            None => format!(
                "[Current time: {}]",
                crate::brain::timezone::format_utc_time(&now)
            ),
        };

        let mut out = format!("{time_marker}\n\n{user_message}");

        if let Some(reminder) = Self::active_plan_reminder(session_id).await {
            out.push_str("\n\n");
            out.push_str(&reminder);
        }

        // Ride relevant memory, active skills, and project directives along with the message (#799, #285).
        let mut recall_summary = None;
        if let Some(recall) =
            crate::brain::memory_recall::recall_for_session(session_id, working_dir, user_message)
                .await
        {
            tracing::info!(
                "Recalled {} chars for session {session_id}",
                recall.summary.chars
            );
            out.push_str("\n\n");
            out.push_str(&recall.text);
            recall_summary = Some(recall.summary);
        }
        (out, recall_summary)
    }

    pub(super) async fn active_plan_reminder(session_id: Uuid) -> Option<String> {
        use crate::utils::plan_files::{self, PlanModeState};
        match plan_files::plan_mode_state(session_id).await {
            PlanModeState::NoPlan => None,
            PlanModeState::PreInitEditing => Some(format_editing_reminder(None)),
            PlanModeState::PostInitEditing => {
                let md = plan_files::plan_md_path(session_id).await;
                // Checklist plans have no design document (#1145): don't
                // point the reminder at a file that does not exist.
                Some(if md.exists() {
                    format_editing_reminder(Some(md))
                } else {
                    format_checklist_editing_reminder().to_string()
                })
            }
            PlanModeState::Active => {
                let plan = plan_files::load_plan(session_id).await?;
                format_plan_reminder(&plan)
            }
        }
    }

    /// Helper to prepare message context for LLM requests
    ///
    /// This extracts the common setup logic shared between send_message() and
    /// send_message_streaming() to reduce code duplication.
    pub(super) async fn prepare_message_context(
        &self,
        session_id: Uuid,
        user_message: String,
        model: Option<String>,
    ) -> Result<(String, LLMRequest, MessageService, SessionService)> {
        self.prepare_message_context_with_display(session_id, user_message, None, model)
            .await
    }

    /// Like [`prepare_message_context`](Self::prepare_message_context) but
    /// persists `display_text` (when set) to the DB instead of the full
    /// `user_message`. The LLM context still receives `user_message`, so
    /// turn-scoped scaffolding (reaction guidance, steering prefaces) reaches
    /// the model without polluting session history or future context.
    pub(super) async fn prepare_message_context_with_display(
        &self,
        session_id: Uuid,
        user_message: String,
        display_text: Option<String>,
        model: Option<String>,
    ) -> Result<(String, LLMRequest, MessageService, SessionService)> {
        // Get or create session
        let session_service = SessionService::new(self.context.clone());
        let _session = session_service
            .get_session(session_id)
            .await
            .map_err(AgentError::db)?
            .ok_or(AgentError::SessionNotFound(session_id))?;

        // Load conversation context with budget-aware message trimming
        let message_service = MessageService::new(self.context.clone());
        // #854: the compaction boundary is pushed into SQL, so a session that has
        // already been compacted loads its tail instead of its whole history.
        let mut db_messages =
            Self::load_messages_since_last_compaction(self.context.pool(), session_id).await?;

        let model_name = model.unwrap_or_else(|| {
            self.provider_for_session(session_id)
                .default_model()
                .to_string()
        });
        let context_window = self.context_limit_for_session(session_id);

        // #813: the clean `run_tool_loop` already performs before it assembles
        // a context. This path never did it, so every persisted
        // `<!-- reasoning -->` block, every `<!-- tools-v2 -->` ledger, the
        // phantom-blocked sections and the compaction banner went to the
        // provider as prompt text on all the channel call sites that reach
        // here (model-switch confirmations, reaction guidance, interaction
        // replies). Rows are only ever appended to, so the DB keeps its
        // replay ledgers; only this in-memory copy is cleaned.
        //
        // `preserve_thinking` mirrors `run_tool_loop`: models that require the
        // reasoning returned as `reasoning_content` get it hoisted into the
        // `thinking` column instead of dropped (#654), and a CLI provider gets
        // no hoist at all, because another model's chain of thought must never
        // reach its prompt (#1522).
        let preserve_thinking = !self.provider_for_session(session_id).cli_handles_tools()
            && crate::brain::provider::custom_openai_compatible::preserves_thinking(
                self.provider_for_session(session_id).base_url(),
                &model_name,
            );
        super::context_rows::clean_rows_for_llm(&mut db_messages, preserve_thinking);

        let mut context =
            AgentContext::from_db_messages(session_id, db_messages, context_window as usize);

        // Add system brain if available (count its tokens for accurate tracking).
        // `live_system_brain` rebuilds from disk when a brain file changed so
        // edits take effect on the next turn without a restart (#213). The
        // session-aware variant patches the Runtime Info Model/Provider lines
        // to the session's resolved pair, not the startup default.
        if let Some(brain) = self.live_system_brain_for_session(session_id) {
            context.token_count += AgentContext::estimate_tokens(&brain);
            context.system_brain = Some(brain);
        }

        // Add user message. If a plan is actively executing, append a compact
        // reminder of its incomplete tasks so it rides at the END of the prompt
        // (best recall) every turn — without it the plan scrolls out of the
        // recency window in a long conversation and the model forgets it was
        // mid-plan (discussion #177). Regenerated each turn from the plan file;
        // the DB only ever stores the clean user message, so it never piles up.
        let brain_dir = self.brain_workspace_path();
        let working_directory = self.get_working_directory_for_session(session_id);
        // The recall summary is dropped here: this path has no progress
        // callback, so there is no flow roll to name it in (#928). The recall
        // text itself is unchanged.
        let (context_user_message, _recall) = Self::augment_user_message(
            session_id,
            &user_message,
            brain_dir.as_deref(),
            Some(&working_directory),
        )
        .await;
        let user_msg = Message::user(context_user_message);
        context.add_message(user_msg);

        // Save user message to database. When a display override is set,
        // history records the compact form; the full text was context-only.
        message_service
            .create_message(
                session_id,
                "user".to_string(),
                display_text.unwrap_or(user_message),
            )
            .await
            .map_err(AgentError::db)?;

        // Build base LLM request. The output reservation is bounded by this
        // session's active context window so a 200K route keeps input headroom.
        let request = LLMRequest::new(model_name.clone(), context.messages.clone())
            .with_max_tokens(self.request_max_tokens_for_session(session_id));

        // Surface a small "Recently accessed" anchor section so the
        // agent re-uses real paths from prior sessions / pre-compaction
        // turns instead of hallucinating directory layouts. Filtered
        // against the live messages so we don't double-list paths the
        // agent just touched in this same session.
        let working_directory = self.get_working_directory_for_session(session_id);
        let recent_paths = self.recent_paths_for_dir(&working_directory).await;
        let augmented_system = Self::augment_system_with_recent_paths(
            context.system_brain,
            &recent_paths,
            &context.messages,
        );

        let mut request = if let Some(system) = augmented_system {
            request.with_system(system)
        } else {
            request
        };

        // Pass working directory so proxy-aware providers can forward it
        request.working_directory = Some(working_directory.to_string_lossy().to_string());
        request.session_id = Some(session_id);

        Ok((model_name, request, message_service, session_service))
    }

    /// Append a "Recently accessed" anchor section to `system_brain`,
    /// listing only the paths from the persistent recent-paths store
    /// that don't already appear verbatim in any of the live messages.
    /// Returns `base` unchanged when there's nothing to surface — keeps
    /// the prompt clean during normal uncompacted runs where the literal
    /// tool_call/tool_result blocks already mention the path.
    pub(crate) fn augment_system_with_recent_paths(
        base: Option<String>,
        recent_paths: &[String],
        messages: &[Message],
    ) -> Option<String> {
        if recent_paths.is_empty() {
            return base;
        }
        let context_blob: String = messages
            .iter()
            .flat_map(|m| m.content.iter())
            .filter_map(|b| match b {
                ContentBlock::Text { text } => Some(text.clone()),
                ContentBlock::ToolUse { input, .. } => Some(input.to_string()),
                ContentBlock::ToolResult { content, .. } => Some(content.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        let context_for_match = context_blob.to_lowercase();

        let surviving: Vec<&String> = recent_paths
            .iter()
            .filter(|p| !context_for_match.contains(&p.to_lowercase()))
            .collect();
        if surviving.is_empty() {
            return base;
        }
        let mut out = base.unwrap_or_default();
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str(
            "\n--- Recently accessed in this project ---\n\
             (Real paths previously confirmed by read/edit/grep/ls. Prefer these as anchors \
             over guessing from naming conventions.)\n",
        );
        for p in surviving {
            out.push_str("  - ");
            out.push_str(p);
            out.push('\n');
        }
        Some(out)
    }

    /// Load messages from the compaction boundary forward.
    ///
    /// #1649 frozen segments: delta-segment markers EXTEND the window (the
    /// earlier segments stay in force), so they are not boundaries. The
    /// boundary is the LAST marker whose banner does NOT carry the delta
    /// sentinel — a full-window compaction, a hard truncate, a consolidation,
    /// the RSI seal, a cron boundary or a user clear, all of which restart
    /// history. The sentinel is matched ANCHORED at the row start, never as a
    /// substring (#1928): a summary whose BODY quotes the sentinel is still a
    /// boundary, or the reload would skip a real restart and anchor on the
    /// newest segment instead.
    /// Result: `[boundary][segments…][tail]` reconstructs the live window
    /// exactly; a session with no segment markers reloads byte-identically
    /// to the classic last-marker behaviour.
    ///
    /// Anchored on `role == "user" && starts_with(..)`, NOT a substring match
    /// (#175). Every marker is written as a `user` row whose content BEGINS
    /// with the prefix: `CompactionOutcome::marker` (compaction.rs:43),
    /// `AgentContext::hard_truncate_to` (context.rs:346), the tool-loop
    /// persist path (tool_loop.rs:3791), the RSI cycle seal (rsi.rs:1172) and
    /// the cron boundary (scheduler.rs:1003). A plain `contains` instead
    /// re-anchored the window on any message that merely QUOTED the prefix —
    /// an assistant row echoing the banner, or a lane reading it out of a log
    /// or a compaction summary. Audit over four live DBs: 1813 `user` marker
    /// rows, all 1813 starting with the prefix; 243 `assistant` rows
    /// containing it, none of them a marker.
    pub fn messages_from_last_compaction(
        mut all_messages: Vec<crate::db::models::Message>,
    ) -> Vec<crate::db::models::Message> {
        use crate::brain::agent::context::{COMPACTION_MARKER_PREFIX, SEGMENT_SENTINEL};

        // The DELTA banner is the sentinel ANCHORED at the row start, not
        // merely present anywhere in it (#1928). A full-window summary whose
        // body quotes "DELTA SEGMENT." must still count as a boundary; a bare
        // `contains` skipped it, so the reload anchored on the newest segment
        // instead of the restart.
        let delta_banner = format!("{COMPACTION_MARKER_PREFIX} — {SEGMENT_SENTINEL}");

        // Walk backward to the last marker that RESTARTS history. Segment
        // markers (delta-bannered, #175 anchored prefix) are skipped: the
        // boundary they extend from is what the reload anchors on.
        let boundary_idx = all_messages.iter().rposition(|msg| {
            msg.role == "user"
                && msg.content.starts_with(COMPACTION_MARKER_PREFIX)
                && !msg.content.starts_with(&delta_banner)
        });

        if let Some(idx) = boundary_idx {
            let kept = all_messages.len() - idx;
            tracing::info!(
                "Found compaction boundary at message {}/{} — loading {} messages \
                 (boundary + segments + tail)",
                idx,
                all_messages.len(),
                kept,
            );
            // `split_off` returns the tail without copying it: the dropped
            // prefix is freed rather than cloned into a second Vec (#854).
            all_messages.split_off(idx)
        } else {
            all_messages
        }
    }

    /// Load ONLY the messages at or after the last compaction boundary, straight
    /// from SQL.
    ///
    /// The async counterpart of [`Self::messages_from_last_compaction`]: same
    /// boundary, same window, but the rows before the boundary are never
    /// materialised. On a session that has already been compacted that is the
    /// difference between reading 15,567 rows and reading 88 (#854).
    ///
    /// This is the single home of the compaction vocabulary on the async path —
    /// the repository only knows "anchored prefix" — so the two forms cannot
    /// drift apart without the pinning test in `compaction_boundary_test`
    /// failing, which asserts they agree over the same fixture.
    pub(crate) async fn load_messages_since_last_compaction(
        pool: crate::db::Pool,
        session_id: Uuid,
    ) -> Result<Vec<crate::db::models::Message>> {
        use crate::brain::agent::context::{COMPACTION_MARKER_PREFIX, SEGMENT_SENTINEL};

        // The DELTA banner is the sentinel ANCHORED at the row start, exactly as
        // `messages_from_last_compaction` matches it (#1928).
        let delta_banner = format!("{COMPACTION_MARKER_PREFIX} — {SEGMENT_SENTINEL}");

        let repo = crate::db::MessageRepository::new(pool);
        let (tail, total) = repo
            .find_since_last_marker(session_id, "user", COMPACTION_MARKER_PREFIX, &delta_banner)
            .await
            .map_err(AgentError::db)?;

        // A boundary exists exactly when the window is a strict suffix.
        if tail.len() < total {
            tracing::info!(
                "Found compaction boundary at message {}/{} — loading {} messages \
                 (boundary + segments + tail), skipping {}",
                total - tail.len(),
                total,
                tail.len(),
                total - tail.len(),
            );
        }
        Ok(tail)
    }

    /// Synchronous compaction: compute a summary and apply it to `context` in place.
    /// Used by the manual `/compact` command and the two emergency callsites that
    /// recover from "context too large" provider errors. The async path used by
    /// `enforce_context_budget` does NOT go through here — it spawns
    /// `compute_compaction_summary` directly and applies the result via
    /// `apply_compaction_summary` once the LLM call finishes.
    pub(super) async fn compact_context(
        &self,
        session_id: Uuid,
        context: &mut AgentContext,
        model_name: &str,
        cancel_token: Option<&CancellationToken>,
        notifier: Option<super::compaction_notice::CompactionNotifier>,
    ) -> Result<(String, String)> {
        // This call replaces the whole context, so a background summariser
        // still describing the pre-call conversation is describing something
        // that will not exist by the time it returns. Applying its result
        // later would overwrite the compaction happening right here.
        if let Some(superseded) = self.take_pending_compaction(session_id) {
            tracing::info!("Background compaction superseded by a synchronous one — aborting it");
            superseded.abort();
        }

        let provider = self.provider_for_session(session_id);
        let cancel = cancel_token.cloned().unwrap_or_default();

        // #138 part 2: the inventory stamp lists the UNION (active ∪ seen).
        // Re-injection (tool_loop) stays active-only on purpose.
        let stamp_skills = crate::brain::tools::seen_skills::stamp_skills_for_session(session_id);
        let seen_aux = crate::brain::tools::seen_skills::aux_seen_for_session(session_id);
        let active_tools = self.tool_registry.active_tools(session_id);
        let (documents, documents_omitted) =
            crate::brain::tools::retained_docs::inventory_documents(session_id);
        let context_inventory = Self::format_context_inventory(
            context.max_tokens,
            &stamp_skills,
            &seen_aux,
            &active_tools,
            Some(&self.tool_registry),
            &documents,
            documents_omitted,
        );

        let summary = Self::compute_compaction_summary(
            provider,
            self.fallback_chain_snapshot(),
            session_id,
            // Manual `/compact` and emergency recovery are full-window by
            // definition: the caller wants everything re-derived into one
            // marker now (#1649 scoping only applies to the background path).
            CompactionScope::FullWindow,
            context.messages.clone(),
            context.token_count,
            context.max_tokens,
            context.usage_percentage(),
            model_name.to_string(),
            super::request_budget::compaction_summary_request_allowance(),
            self.get_working_directory_for_session(session_id),
            self.auto_approve_tools,
            cancel,
            self.compaction_attempt_deadline(session_id),
            notifier,
            context_inventory,
        )
        .await?;

        let summary = Self::finalize_continuation_document(
            summary,
            session_id,
            self.subagent_manager.clone(),
        )
        .await;

        let applied_marker = Self::apply_compaction_summary(context, &summary);
        Ok((summary, applied_marker))
    }

    /// The documents block for the continuation document (#831).
    ///
    /// Paths only, never bodies: a body injected here is paid for on every
    /// subsequent turn of the new window, while a path costs one line and the
    /// agent can re-read what the pending work actually needs. Same cap and
    /// same newest-first order as the inventory table — one source, so the two
    /// surfaces cannot disagree about what this session read.
    ///
    /// `None` when nothing was read, so a session that reads no `.md` files
    /// gets a continuation document byte-identical to before.
    fn documents_block(session_id: Uuid) -> Option<String> {
        use crate::brain::tools::retained_docs::{DocStatus, inventory_documents};

        let (documents, omitted) = inventory_documents(session_id);
        if documents.is_empty() {
            return None;
        }
        let mut out = String::from(
            "## Documents read pre-compaction\n\
             Harness-written: `.md` files this session read whole. Bodies are not included — \
             re-read one with `read_file` when the pending work depends on it.\n",
        );
        for doc in &documents {
            let detail = match doc.status {
                DocStatus::Ok => format!("{} lines", doc.lines.unwrap_or(0)),
                DocStatus::TooLarge => {
                    format!("large: {} KiB", doc.bytes.unwrap_or(0).div_ceil(1024))
                }
                DocStatus::Unreadable => "unreadable".to_string(),
            };
            out.push_str(&format!("- {} ({detail})\n", doc.path.display()));
        }
        if omitted > 0 {
            out.push_str(&format!("- (+{omitted} more, older reads)\n"));
        }
        Some(out.trim_end().to_string())
    }

    /// Attach the state a model's prose summary cannot be trusted to carry.
    ///
    /// Three blocks are harness-written so they survive a summary that forgot
    /// them, and all three ride INSIDE the persisted marker rather than arriving
    /// as separate messages that can scroll away:
    ///
    /// - session plan artifacts, so the post-compaction agent resumes the plan
    ///   instead of rediscovering it;
    /// - live sub-agent IDs, so `wait_agent` / `send_input` / `resume_agent` /
    ///   `stop_agent` still have something to address (#936);
    /// - documents this session read whole, so project law it obeyed survives
    ///   the summary that dropped it (#831).
    ///
    /// Owns no `&self` and takes the manager by value: the background
    /// summariser calls it from a spawned task with nothing but a snapshot.
    pub(crate) async fn decorate_compaction_summary(
        summary: String,
        session_id: Uuid,
        subagents: Option<Arc<crate::brain::tools::subagent::SubAgentManager>>,
    ) -> String {
        let summary = match plan_state_block(session_id).await {
            Some(block) => format!("{summary}\n\n{block}"),
            None => summary,
        };
        let summary = match subagents.and_then(|m| m.format_running_for_compaction()) {
            Some(block) => format!("{summary}\n\n{block}"),
            None => summary,
        };
        match Self::documents_block(session_id) {
            Some(block) => format!("{summary}\n\n{block}"),
            None => summary,
        }
    }

    /// Decorate, then bound the WHOLE document (#934).
    ///
    /// `compute_compaction_summary` enforces the budget on the model's own
    /// output, because the #482 artifact correction must run against a trimmed
    /// document. But decoration is appended by the caller, AFTER that guard has
    /// already returned — so on 2026-11-21 the persisted document was
    /// budget-compliant in its generated half and over budget in total, by
    /// however much the plan block, the live sub-agent roster and the documents
    /// list happened to weigh. The overrun grew with the roster.
    ///
    /// Enforcement therefore runs a second time here, on the finished document.
    /// It is safe to trim at this point: every decoration block is an
    /// UNNUMBERED `##` heading, and `split_summary_sections` breaks on those
    /// too, so each becomes its own section whose `number` is `None` and which
    /// `is_must_keep_section` therefore protects — the trimmer cannot reach
    /// decoration, and the cut lands on the prose (§1, then §3, §5, §6) where
    /// the sessions ranked it. When decoration ALONE exceeds the budget the
    /// guard WARNs and ships it, which is the honest outcome: the roster is not
    /// something a woken agent can recover from the document.
    ///
    /// That boundary is load-bearing rather than incidental (#934): splitting
    /// on `## N.` alone absorbed the decoration into the span of whichever
    /// numbered section preceded it, so a document ending on a TRIMMABLE
    /// section lost its plan block, roster and documents list when that section
    /// was stubbed. Production masked it because the last numbered section is
    /// the manifest (§10, must-keep); a fixture ending at §1 did not.
    pub(crate) async fn finalize_continuation_document(
        summary: String,
        session_id: Uuid,
        subagents: Option<Arc<crate::brain::tools::subagent::SubAgentManager>>,
    ) -> String {
        let decorated = Self::decorate_compaction_summary(summary, session_id, subagents).await;
        Self::enforce_summary_budget(
            decorated,
            super::request_budget::compaction_summary_output_tokens() as usize,
        )
    }

    /// Send the summariser request, walking `[providers.fallback]` when the
    /// session's provider fails (#1247).
    ///
    /// Compaction used to call `provider.complete()` once and surface whatever
    /// came back. Every other request path in the process walks the chain, so
    /// a session whose primary was rate-limited or out of credit kept chatting
    /// happily via a fallback while `/compact` died on the dead primary — and
    /// with the context window full, a session that cannot compact cannot
    /// recover at all.
    ///
    /// Mirrors the tool loop's walk deliberately: skip the primary's own name,
    /// try every remaining entry in configured order (#1251 — no provider is
    /// ever dropped from the walk for having failed before), remap the model
    /// to each fallback's default when it doesn't carry the requested one,
    /// and report the whole ledger if everything dies.
    /// `pub(crate)` for the regression tests in `src/tests` — no caller outside
    /// Bound on a single summariser attempt for a session with no compaction
    /// history to scale from.
    ///
    /// Calibrated for real-world reasoning summarisers:
    /// TTFT (~10s) + ample reasoning headroom (60–90s) + generation of up to
    /// 8,000 output tokens at TPS=40 (200s) = ~270–300s.
    ///
    /// Not a guess: this covers the p95 of compaction durations across fleet
    /// operations while preventing premature truncation of reasoning models.
    pub(crate) const COMPACTION_ATTEMPT_FLOOR: std::time::Duration =
        std::time::Duration::from_secs(300);

    /// One summariser attempt, bounded.
    ///
    /// HTTP providers already cap a single request at
    /// `anthropic.rs::DEFAULT_TIMEOUT` (300s). CLI providers carry no timeout
    /// at all, so a summariser that stopped answering held the session for as
    /// long as the process felt like living, and the fallback chain below was
    /// never reached because the first attempt never returned (#1255). This
    /// extends the bound every HTTP provider already honours to the ones that
    /// do not, and a provider that blows it is handed on rather than waited
    /// out: `Timeout` is retryable, so `should_try_next_provider` walks.
    ///
    /// HTTP providers are asked through `stream()` and the stream is folded
    /// into one response by [`super::compaction_stream::collect_stream`]
    /// (#1519). The non-streaming request held the gateway's connection open
    /// until the whole summary existed, and a summary of a full window does
    /// not finish inside a gateway idle limit of about two minutes, so the
    /// request came back 504 on every retry while chat turns on the same
    /// model streamed fine. CLI providers keep `complete()`: there is no
    /// gateway in front of a subprocess, and their stream path is the one
    /// that runs tools.
    async fn compaction_attempt(
        provider: &Arc<dyn Provider>,
        request: LLMRequest,
        deadline: std::time::Duration,
    ) -> std::result::Result<
        crate::brain::provider::LLMResponse,
        crate::brain::provider::ProviderError,
    > {
        let attempt = async {
            if provider.cli_handles_tools() {
                provider.complete(request).await
            } else {
                let stream = provider.stream(request).await?;
                let idle_timeout = provider
                    .stream_idle_timeout()
                    .unwrap_or(std::time::Duration::from_secs(30));
                super::compaction_stream::collect_stream_with_timeout(stream, idle_timeout).await
            }
        };
        match tokio::time::timeout(deadline, attempt).await {
            Ok(result) => result,
            Err(_) => {
                tracing::warn!(
                    "Compaction: provider '{}' produced nothing in {:?} — moving on",
                    provider.name(),
                    deadline,
                );
                Err(crate::brain::provider::ProviderError::Timeout(
                    deadline.as_secs(),
                ))
            }
        }
    }

    /// compaction should send a summariser request.
    pub(crate) async fn complete_compaction_request(
        primary: &Arc<dyn Provider>,
        fallbacks: &[Arc<dyn Provider>],
        request: LLMRequest,
        cancel: &CancellationToken,
        attempt_deadline: std::time::Duration,
        notifier: Option<&super::compaction_notice::CompactionNotifier>,
    ) -> Result<crate::brain::provider::LLMResponse> {
        use super::compaction_notice::CompactionStep;
        use crate::brain::provider::error as provider_error;

        let primary_name = primary.name().to_string();
        let first_err = tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                tracing::info!("Compaction cancelled before completion");
                return Err(AgentError::Cancelled);
            }
            r = Self::compaction_attempt(primary, request.clone(), attempt_deadline) => match r {
                Ok(response) => return Ok(response),
                Err(e) => e,
            },
        };

        if fallbacks.is_empty() || !provider_error::should_try_next_provider(&first_err) {
            if let Some(n) = notifier {
                n.step(CompactionStep::Failed {
                    detail: format!(
                        "'{}': {}",
                        primary_name,
                        provider_error::short_error_reason(&first_err)
                    ),
                });
            }
            return Err(AgentError::Provider(first_err));
        }

        tracing::warn!(
            "Compaction: primary '{}' failed ({}) — walking fallback chain",
            primary_name,
            provider_error::short_error_reason(&first_err),
        );
        if let Some(n) = notifier {
            n.step(CompactionStep::AttemptFailed {
                provider: primary_name.clone(),
                reason: provider_error::short_error_reason(&first_err),
            });
        }

        // Size is the one failure where the order matters: a narrower window
        // than the one that just refused is a refusal spent before the
        // answer, so an overflow walks widest first (#1379). Everything else
        // keeps the configured order.
        let ordered: Vec<Arc<dyn Provider>> = if matches!(
            first_err,
            crate::brain::provider::ProviderError::ContextLengthExceeded(_)
        ) {
            crate::brain::provider::chain_order::widest_first(fallbacks)
        } else {
            fallbacks.to_vec()
        };

        let mut tried: Vec<String> = Vec::new();
        let mut last_err = first_err;

        for fallback in &ordered {
            let name = fallback.name().to_string();
            if name == primary_name {
                continue;
            }
            // A substitute runs its own configured model, not the one the
            // failed request carried; the same rule the chat chain applies,
            // through the same function (#1374).
            let fb_request =
                crate::brain::provider::fallback::substitute_request(fallback.as_ref(), &request);
            tracing::info!(
                "Compaction: trying fallback provider '{}' (model '{}')",
                name,
                fb_request.model
            );
            if let Some(n) = notifier {
                n.step(CompactionStep::TryingFallback {
                    provider: name.clone(),
                    model: fb_request.model.clone(),
                });
            }

            let err = tokio::select! {
                biased;
                _ = cancel.cancelled() => {
                    tracing::info!("Compaction cancelled while walking fallback chain");
                    return Err(AgentError::Cancelled);
                }
                r = Self::compaction_attempt(fallback, fb_request, attempt_deadline) => match r {
                    Ok(response) => {
                        tracing::info!("Compaction served by fallback '{}'", name);
                        return Ok(response);
                    }
                    Err(e) => e,
                },
            };

            tried.push(format!(
                "{}: {}",
                name,
                provider_error::short_error_reason(&err)
            ));
            if let Some(n) = notifier {
                n.step(CompactionStep::AttemptFailed {
                    provider: name.clone(),
                    reason: provider_error::short_error_reason(&err),
                });
            }
            last_err = err;
        }

        let summary = provider_error::chain_exhausted_summary(
            &primary_name,
            &provider_error::short_error_reason(&last_err),
            &tried,
        );
        tracing::error!("Compaction: fallback chain exhausted: {summary}");
        if let Some(n) = notifier {
            n.step(CompactionStep::Failed {
                detail: summary.clone(),
            });
        }
        Err(AgentError::Provider(provider_error::with_chain_summary(
            last_err, summary,
        )))
    }

    /// #778: one summariser attempt, retried ONCE if the provider answered but
    /// produced no document at all.
    ///
    /// An empty summary is worse than no summary. `enforce_summary_budget` can
    /// only re-attach the `context-manifest` fence and the §0 obligation line
    /// from a document that already carries them, so an empty stub lands as a
    /// fence-less document and the resuming turn loses its manifest. Measured
    /// 2026-10-15: 8 empty documents in a single day, one session compacting
    /// four times in two minutes because an empty summary bought it no room.
    ///
    /// Retries only an EMPTY document. An `Err` is the fallback walk's business
    /// inside `complete_compaction_request`, and re-driving it here would double
    /// the chain. A `MaxTokens` stop that still produced text is left alone: the
    /// document is cut but usable, and `warn_if_summary_truncated` (#1933)
    /// already reports it. The notifier rides both attempts — `CompactionStep::Failed`
    /// is emitted on the error path alone, so a successful retry cannot double-report.
    pub(crate) async fn complete_compaction_request_with_retry(
        primary: &Arc<dyn Provider>,
        fallbacks: &[Arc<dyn Provider>],
        request: LLMRequest,
        cancel: &CancellationToken,
        attempt_deadline: std::time::Duration,
        notifier: Option<&super::compaction_notice::CompactionNotifier>,
    ) -> Result<crate::brain::provider::LLMResponse> {
        let response = Self::complete_compaction_request(
            primary,
            fallbacks,
            request.clone(),
            cancel,
            attempt_deadline,
            notifier,
        )
        .await?;

        if !Self::extract_text_from_response(&response)
            .trim()
            .is_empty()
        {
            return Ok(response);
        }

        tracing::warn!(
            "compute_compaction_summary: the summariser returned an EMPTY document \
             (stop_reason={:?}, output_tokens={}, reasoning_tokens={}) — retrying once (#778)",
            response.stop_reason,
            response.usage.output_tokens,
            response.usage.reasoning_tokens
        );

        Self::complete_compaction_request(
            primary,
            fallbacks,
            request,
            cancel,
            attempt_deadline,
            notifier,
        )
        .await
    }

    /// #1930: the summariser's system prompt. Dense knowledge transfer under a
    /// hard budget — the document is trimmed mechanically if it overruns, so
    /// the instruction is completeness of FACTS, not volume of prose.
    ///
    /// #929: the text lives in the prompt file now; this reads the compiled-in
    /// default, which is what the source-pinned budget test asserts against.
    #[allow(dead_code)]
    pub(crate) fn compaction_system_prompt() -> String {
        CompactionPrompts::compiled_default()
            .system_prompt()
            .to_string()
    }

    /// #1930: the closing directive of the continuation-document prompt — the
    /// hard budget and the order in which it is spent.
    ///
    /// The prompt used to close by ordering maximum verbosity, with no stated
    /// size and nothing trimming the result: measured 2026-10-04 that produced a
    /// 27.8 KB mean (max 69.2 KB) against a 3 000-token budget. Retention is
    /// ordered so the blocks a woken agent cannot work without (obligation
    /// status, the `context-manifest` fence, the next step, the Recovery
    /// Playbook) are named MUST, and the cheapest content to drop (long
    /// snippets, long quotes) is named FIRST TO GO.
    ///
    /// #929: the text lives in the prompt file now; this reads the compiled-in
    /// default, which is what the source-pinned budget test asserts against.
    #[allow(dead_code)]
    pub(crate) fn compaction_budget_directive(summary_budget_tokens: u32) -> String {
        CompactionPrompts::compiled_default().budget_directive(summary_budget_tokens)
    }

    /// #1649: the scope header prepended to the summariser prompt. Empty for
    /// `FullWindow` — the first-compaction prompt stays byte-identical to the
    /// classic one (the parity probe pins this). Delta and consolidation get
    /// a short prelude that re-aims the same exhaustive body at their
    /// narrower input instead of duplicating the whole prompt per scope.
    ///
    /// #929: the text lives in the prompt file now; this reads the compiled-in
    /// default, which is what the scope-prelude test asserts against.
    #[allow(dead_code)]
    pub(crate) fn compaction_scope_prelude(scope: CompactionScope) -> String {
        CompactionPrompts::compiled_default().scope_prelude(scope)
    }

    /// Size cell for the inventory table. `None` means "we could not weigh this
    /// name" and renders `—` (#856): a fabricated number is worse than a dash,
    /// because the dash is visibly unknown and the number gets copied.
    fn inventory_size(tokens: Option<usize>) -> String {
        match tokens {
            Some(t) => t.to_string(),
            None => "—".to_string(),
        }
    }

    /// Percentage cell for the inventory table. `None` is unweighable → `—`.
    fn inventory_pct(tokens: Option<usize>, snapshot_max_tokens: usize) -> String {
        match tokens {
            None => "—".to_string(),
            Some(_) if snapshot_max_tokens == 0 => "0.0%".to_string(),
            Some(t) => format!("{:.1}%", (t as f64 / snapshot_max_tokens as f64) * 100.0),
        }
    }

    /// Format dynamic markdown table of currently active skills, consumed auxiliary
    /// skill documents, documents the session read whole, and lazy tools, their token
    /// sizes, and percentage of total context window for the compaction prompt.
    ///
    /// #856: a name that resolves to nothing is still LISTED (the model needs to
    /// see the vocabulary it used was wrong) but carries `—` for size and share
    /// and is not charged to the footer. Nothing is active, so nothing is billed.
    pub fn format_context_inventory(
        snapshot_max_tokens: usize,
        active_skills: &std::collections::HashSet<String>,
        seen_aux: &std::collections::HashMap<String, Vec<String>>,
        active_tools: &std::collections::HashSet<String>,
        tool_registry: Option<&std::sync::Arc<crate::brain::tools::ToolRegistry>>,
        documents: &[crate::brain::tools::retained_docs::DocEntry],
        documents_omitted: usize,
    ) -> String {
        let all_skills = crate::brain::skills::load_all_skills();
        // Cells are pre-rendered: a document row may carry `(skipped: 12 KiB)`
        // or `—` where a skill row carries a token count and a percentage.
        let mut rows: Vec<(String, &'static str, String, String)> = Vec::new();
        let mut total_tokens = 0usize;

        let mut sorted_skills: Vec<_> = active_skills.iter().collect();
        sorted_skills.sort();
        for skill_name in sorted_skills {
            // #179: one canonicalisation rule for the whole crate. The table
            // must name the same string the manifest rule tells the model to
            // write — it used to render "/canarya" beside a rule saying
            // "<skill-slug>", so the prompt argued with itself and the spelling
            // the model was told to follow was the one that did not work.
            let normalized = crate::brain::skills::normalize_skill_slug(skill_name);
            let skill = all_skills
                .iter()
                .find(|s| s.name == normalized || s.slash_name == *skill_name);
            // #856: an unresolved slug is a name the manifest invented — 33 of
            // 254 `active_skills` entries on 2026-11-15 (`coding` for
            // `coding-process`; `coding-standards` for a brain file that was
            // never a skill). Rendering it as a plausible 100-token row made
            // this table — the very vocabulary the prompt tells the model to
            // copy from — endorse the typo, so the summariser carried it into
            // every later manifest. The row stays, for continuity and so the
            // model can see the name it used was never real; but it carries no
            // size, no share and no charge to the footer, because nothing is
            // active. Same discipline the documents block below already
            // follows.
            let skill_tokens = skill.map(|s| AgentContext::estimate_tokens(&s.body));
            if let Some(t) = skill_tokens {
                total_tokens += t;
            } else {
                tracing::warn!(
                    slug = %normalized,
                    raw = %skill_name,
                    "#856: active skill slug resolves to no loaded skill — listed as \
                     (unknown skill), not billed"
                );
            }
            rows.push((
                match skill_tokens {
                    Some(_) => normalized.clone(),
                    None => format!("{normalized} (unknown skill)"),
                },
                "Skill",
                Self::inventory_size(skill_tokens),
                Self::inventory_pct(skill_tokens, snapshot_max_tokens),
            ));

            // Issue #216: account consumed auxiliary skill documents.
            if let Some(files) = seen_aux.get(&normalized) {
                for file_name in files {
                    // #856: the same fabrication one level down — an aux name
                    // that resolves to no real auxiliary file of a real skill
                    // was billed 100 tokens and rendered as a measured row.
                    let aux_tokens = skill.and_then(|s| {
                        s.auxiliary_files
                            .iter()
                            .find(|a| &a.name == file_name)
                            .map(|aux| AgentContext::estimate_tokens(&aux.body))
                    });
                    if let Some(t) = aux_tokens {
                        total_tokens += t;
                    } else {
                        tracing::warn!(
                            slug = %normalized,
                            file = %file_name,
                            "#856: aux document resolves to nothing — listed, not billed"
                        );
                    }
                    rows.push((
                        format!("{normalized}/{file_name}"),
                        "Skill aux",
                        Self::inventory_size(aux_tokens),
                        Self::inventory_pct(aux_tokens, snapshot_max_tokens),
                    ));
                }
            }
        }

        // #831: documents this session read whole. Listed, not injected — the
        // summariser decides which ones to carry into `retained_documents`.
        // Their tokens are deliberately NOT added to `total_tokens`: nothing is
        // active yet, and the footer counts active skills & tools.
        for doc in documents {
            let size_cell = match doc.status {
                crate::brain::tools::retained_docs::DocStatus::Ok => doc.tokens.to_string(),
                crate::brain::tools::retained_docs::DocStatus::TooLarge => {
                    format!("(skipped: {} KiB)", doc.bytes.unwrap_or(0).div_ceil(1024))
                }
                crate::brain::tools::retained_docs::DocStatus::Unreadable => {
                    "(unreadable)".to_string()
                }
            };
            let pct_cell = match doc.status {
                crate::brain::tools::retained_docs::DocStatus::Ok => {
                    let pct = if snapshot_max_tokens > 0 {
                        (doc.tokens as f64 / snapshot_max_tokens as f64) * 100.0
                    } else {
                        0.0
                    };
                    format!("{pct:.1}%")
                }
                _ => "—".to_string(),
            };
            rows.push((
                doc.path.display().to_string(),
                "Document",
                size_cell,
                pct_cell,
            ));
        }
        if documents_omitted > 0 {
            rows.push((
                format!("(+{documents_omitted} more documents read, older)"),
                "",
                String::new(),
                String::new(),
            ));
        }

        let mut sorted_tools: Vec<_> = active_tools.iter().collect();
        sorted_tools.sort();
        for tool_name in sorted_tools {
            // #856: same class — a name the registry does not know was billed a
            // fabricated 150 tokens and rendered as a real lazy tool. A `None`
            // registry means "not supplied", which is equally unweighable, so
            // both land on `—` rather than a guess.
            let tool_tokens = tool_registry
                .and_then(|reg| reg.get(tool_name))
                .map(|t| AgentContext::estimate_tokens(&t.input_schema().to_string()));
            if let Some(t) = tool_tokens {
                total_tokens += t;
            } else if tool_registry.is_some() {
                tracing::warn!(
                    tool = %tool_name,
                    "#856: active lazy tool resolves to no registered tool — listed as \
                     (unknown tool), not billed"
                );
            }
            rows.push((
                match tool_tokens {
                    Some(_) => tool_name.clone(),
                    None if tool_registry.is_some() => format!("{tool_name} (unknown tool)"),
                    None => tool_name.clone(),
                },
                "Lazy Tool",
                Self::inventory_size(tool_tokens),
                Self::inventory_pct(tool_tokens, snapshot_max_tokens),
            ));
        }

        if rows.is_empty() {
            return "*(No active skills or extended lazy tools currently loaded)*\n".to_string();
        }

        let mut out = String::new();
        out.push_str("| Item | Type | Size (Tokens) | % Context |\n");
        out.push_str("|---|---|---|---|\n");
        for (item, kind, tokens, pct) in rows {
            if kind.is_empty() {
                // Full-width continuation row: an omission has to be visible,
                // and it is not itself an item with a type and a size.
                out.push_str(&format!("| {item} | | | |\n"));
            } else {
                out.push_str(&format!("| `{item}` | {kind} | {tokens} | {pct} |\n"));
            }
        }

        let total_pct = if snapshot_max_tokens > 0 {
            (total_tokens as f64 / snapshot_max_tokens as f64) * 100.0
        } else {
            0.0
        };
        let target_tokens = crate::brain::skills::retained_set_budget_tokens(snapshot_max_tokens);
        out.push_str(&format!(
            "\nTotal active skills & tools: {} tokens ({:.1}% of context window, {} total tokens).\n\
             Guidance: aim to keep active skills and lazy tools <= 5% target (~{} tokens).\n",
            total_tokens, total_pct, snapshot_max_tokens, target_tokens
        ));
        if !documents.is_empty() {
            out.push_str(
                "Documents re-injected on wake count against the same 5% retained-set budget.\n",
            );
        }

        out
    }

    /// Compute a compaction summary from a snapshot of messages.
    ///
    /// This is the LLM-facing half of compaction. It does not touch any live
    /// session state — it operates entirely on the cloned snapshot — so it
    /// is safe to call from a background `tokio::spawn` task while the agent
    /// keeps appending new messages to the live context.
    ///
    /// Returns the raw summary text. Callers that want to apply it to a live
    /// context should call `apply_compaction_summary` once the future resolves.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn compute_compaction_summary(
        provider: Arc<dyn Provider>,
        fallbacks: Vec<Arc<dyn Provider>>,
        session_id: Uuid,
        scope: CompactionScope,
        snapshot_messages: Vec<Message>,
        snapshot_token_count: usize,
        snapshot_max_tokens: usize,
        snapshot_usage_pct: f64,
        model_name: String,
        max_output_tokens: u32,
        working_directory: PathBuf,
        auto_approve_tools: bool,
        cancel: CancellationToken,
        attempt_deadline: std::time::Duration,
        notifier: Option<super::compaction_notice::CompactionNotifier>,
        context_inventory: String,
    ) -> Result<String> {
        let remaining_budget = snapshot_max_tokens.saturating_sub(snapshot_token_count);

        // Skip any leading user messages that consist only of ToolResult blocks —
        // they are orphaned (their tool_use was removed by a prior trim) and would
        // cause the API to reject the request with a 400.
        let start = snapshot_messages
            .iter()
            .position(|m| {
                !(m.role == crate::brain::provider::Role::User
                    && !m.content.is_empty()
                    && m.content.iter().all(|b| {
                        matches!(b, crate::brain::provider::ContentBlock::ToolResult { .. })
                    }))
            })
            .unwrap_or(snapshot_messages.len());

        // Reserve room for the summariser's OUTPUT budget + prompt headroom.
        // Both derive from the SAME constant as the request's
        // `max_output_tokens` (#1930): a reserve that disagrees with the
        // allowance hands the summariser more input than the window can hold.
        let output_reserve = super::request_budget::compaction_summary_input_reserve();
        let max_input_budget = snapshot_max_tokens.saturating_sub(output_reserve);
        let all_msgs = &snapshot_messages[start..];
        let mut running_tokens = 0usize;
        let msgs_to_include: Vec<&Message> = all_msgs
            .iter()
            .rev()
            .take_while(|m| {
                let t = AgentContext::estimate_tokens_static(m);
                if running_tokens + t <= max_input_budget {
                    running_tokens += t;
                    true
                } else {
                    tracing::warn!(
                        "Compaction: dropping oldest messages to fit input budget ({}/{} tokens used)",
                        running_tokens,
                        max_input_budget,
                    );
                    false
                }
            })
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();

        tracing::info!(
            "Compaction: sending {} / {} messages to summarizer ({} / {} input tokens, reserving {} for output)",
            msgs_to_include.len(),
            all_msgs.len(),
            running_tokens,
            snapshot_max_tokens,
            output_reserve,
        );

        let mut summary_messages: Vec<Message> = msgs_to_include.into_iter().cloned().collect();

        // #929: the continuation-document prompt is data now — one load per
        // compaction (not per message), then explicit `{name}` substitution.
        // `format!` cannot take a runtime string, so `body()` renders the
        // template against `BODY_PLACEHOLDERS`, which validation pins.
        let prompts = CompactionPrompts::load(&crate::config::profile::resolve_profile_home());
        let base_prompt = prompts.body(&BodyValues {
            snapshot_usage_pct,
            snapshot_token_count,
            snapshot_max_tokens,
            remaining_budget,
            context_inventory: context_inventory.as_str(),
            auto_approve_tools,
            summary_budget_tokens: super::request_budget::COMPACTION_SUMMARY_MAX_TOKENS,
        });

        // #1649: the scope prelude rides in front of the unchanged body;
        // FullWindow gets an empty prelude, so the first-compaction prompt is
        // byte-identical to the classic one.
        let mut compaction_prompt = prompts.scope_prelude(scope);
        compaction_prompt.push_str(&base_prompt);
        summary_messages.push(Message::user(compaction_prompt));

        // Never send a {provider, model} pair the user didn't configure.
        // If the requested model isn't supported by this provider, remap to
        // the provider's own default — same invariant `stream_complete` enforces.
        let mut effective_model = model_name;
        let supported = provider.supported_models();
        if !supported.is_empty() && !supported.iter().any(|m| m == &effective_model) {
            let remapped = provider.default_model().to_string();
            tracing::warn!(
                "compute_compaction_summary: provider '{}' does not support model '{}' — remapping to '{}'",
                provider.name(),
                effective_model,
                remapped,
            );
            effective_model = remapped;
        }

        // #778: the summary document is the payload, and reasoning tokens are
        // charged to the same output allowance — a reasoning model that thinks
        // its way through all 5500 tokens returns no document at all. Measured
        // 2026-10-15: 42 `MaxTokens` stops in one day, 8 of them empty. The
        // summariser formats; it has nothing to reason about.
        let mut request = LLMRequest::new(effective_model, summary_messages)
            .with_max_tokens(max_output_tokens)
            .with_reasoning_disabled()
            .with_system(prompts.system_prompt().to_string());
        request.working_directory = Some(working_directory.to_string_lossy().to_string());
        request.session_id = Some(session_id);

        // The summariser's output never reaches a UI: the stream is folded
        // into one response inside `compaction_attempt`, no progress callback
        // is attached (#1519). `cancel` aborts the request mid-flight if the
        // caller signals (e.g. 90% hard-truncate firing on the same session).
        // #778: the retry wrapper, not the bare attempt — an empty document is
        // the one failure the fallback chain never sees (a provider that
        // answers with nothing is a SUCCESS to it), so the retry has to sit
        // here, above the walk.
        let response = Self::complete_compaction_request_with_retry(
            &provider,
            &fallbacks,
            request,
            &cancel,
            attempt_deadline,
            notifier.as_ref(),
        )
        .await?;

        // #1933: a `MaxTokens` stop means the request hit its output allowance
        // and the document is CUT, whatever its token count reads. The trim
        // guard cannot see this — a capped document sits exactly at the budget,
        // which the guard's `before > budget` test never inspects — so the
        // response's own verdict is read here, at the source.
        Self::warn_if_summary_truncated(response.stop_reason.clone(), &response.usage);

        let summary = Self::extract_text_from_response(&response);

        // #929: stamp the summary with the prompt that produced it. `prompts` is
        // the set this request was built from — the same load, not a fresh one.
        if let Err(e) = Self::save_compaction_summary_to_memory(&summary, &prompts.stamp()).await {
            tracing::warn!("Failed to save compaction summary to daily log: {}", e);
        }

        // Index the updated memory file in the background so memory_search picks it up.
        let memory_path = crate::config::opencrabs_home()
            .join("memory")
            .join(format!("{}.md", chrono::Local::now().format("%Y-%m-%d")));
        tokio::spawn(async move {
            match crate::memory::get_store() {
                Ok(store) => {
                    if let Err(e) = crate::memory::index_file(store, &memory_path).await {
                        tracing::warn!("Failed to index daily note after compaction: {e}");
                    }
                }
                Err(e) => tracing::warn!("Memory store unavailable, daily note not indexed: {e}"),
            }
        });

        // #1930: the prompt ASKS for a ~3 000-token document, but asking is not
        // enforcing — measured 2026-10-04, 208 of the 210 live markers exceeded
        // the budget (mean 27.8 KB, max 69.2 KB) because a 40 000-token output
        // allowance sat against a 9 000-token input reserve. Trim structurally
        // HERE, after generation and BEFORE the #482 correction below, so the
        // correction always survives into the persisted marker.
        let summary = Self::enforce_summary_budget(
            summary,
            super::request_budget::compaction_summary_output_tokens() as usize,
        );

        // #482: the summary is obeyed as the continuation document, so an
        // artifact it reports as complete is acted on — the incident claimed a
        // finished transcription, quoted the "exact text" of files that had
        // never been written, and ordered the woken agent to transcribe it.
        // The detectors that guard a live turn are not wired onto this path,
        // and this is the one surface whose output becomes durable context, so
        // the artifact-existence check runs against the very messages being
        // summarised.
        let summary = Self::flag_unbacked_artifacts(summary, &snapshot_messages);

        Ok(summary)
    }

    /// Enforce the continuation document's token budget, structurally (#1930).
    ///
    /// The summariser prompt *asks* for a document of `budget_tokens`, but
    /// asking is not enforcing: on 2026-10-04 the live markers read a mean of
    /// 27 831 B against an ~11.7 KB budget (max 69 154 B; 208 of 210 over), and
    /// a 40 000-token output allowance against a 9 000-token input reserve
    /// guaranteed a model could produce one. A summary that is not trimmed
    /// becomes the woken agent's ENTIRE context and is re-loaded every turn, so
    /// its cost compounds with the session's own length.
    ///
    /// Trimming is at SECTION granularity and never inside a fence: a fenced
    /// block is dropped whole or not at all. Five blocks are never candidates,
    /// because a woken agent cannot recover them from the document alone —
    /// section 0 (the obligation), section 7 (recovery), section 8 (next step),
    /// section 9 (the continuation message the user reads first on wake, #934)
    /// and the `context-manifest` fence — and the preamble before the first
    /// heading is kept rather than guessed at. If the must-keep set alone
    /// exceeds the budget the guard WARNs and ships it: that is a prompt
    /// defect, not a trimming one, and silently cutting the obligation out
    /// would be worse than an over-budget document.
    pub(crate) fn enforce_summary_budget(summary: String, budget_tokens: usize) -> String {
        let before = crate::brain::tokenizer::count_tokens(&summary);
        if before <= budget_tokens {
            // #1933: an under-budget document can still be INCOMPLETE. When the
            // summariser is cut at source (`stop_reason = MaxTokens`) the visible
            // document is short — it FITS the budget — while missing the §10
            // fence entirely. This early return is exactly why the one failure
            // that removes the fence was the one the guard never inspected, so
            // the invariants are checked on BOTH paths.
            Self::warn_on_summary_invariants(&summary);
            return summary;
        }

        let sections = split_summary_sections(&summary);
        let manifest = extract_manifest_block(&summary);

        // Largest trimmable section first; must-keep sections are never in this
        // list, so neither pass can reach the four load-bearing blocks.
        let mut trimmable: Vec<usize> = sections
            .iter()
            .enumerate()
            .filter(|(_, s)| !s.must_keep)
            .map(|(i, _)| i)
            .collect();
        trimmable.sort_by_key(|&i| std::cmp::Reverse(sections[i].text.len()));

        let mut rendered: Vec<String> = sections.iter().map(|s| s.text.clone()).collect();
        let mut total = before;

        // Pass 1 — fenced code blocks go first, the directive's own order
        // ("name the file and line instead of pasting the block").
        for &i in &trimmable {
            while total > budget_tokens {
                let Some(next) = remove_largest_fenced_block(&rendered[i]) else {
                    break;
                };
                let next_total = total - crate::brain::tokenizer::count_tokens(&rendered[i])
                    + crate::brain::tokenizer::count_tokens(&next);
                if next_total >= total {
                    break;
                }
                rendered[i] = next;
                total = next_total;
            }
            if total <= budget_tokens {
                break;
            }
        }

        // Pass 2 — drop whole trimmable sections, largest first, leaving the
        // heading plus a one-line pointer so the document stays navigable.
        for &i in &trimmable {
            if total <= budget_tokens {
                break;
            }
            let stub = section_stub(&sections[i]);
            let next_total = total - crate::brain::tokenizer::count_tokens(&rendered[i])
                + crate::brain::tokenizer::count_tokens(&stub);
            if next_total >= total {
                continue;
            }
            rendered[i] = stub;
            total = next_total;
        }

        let mut result = rendered.concat();

        // Section 10 keeps the manifest fence in the normal shape, but a sloppy
        // model may place it anywhere; re-attach it if trimming reached it.
        if let Some(block) = manifest
            && parse_context_manifest(&result).is_none()
        {
            result.push_str("\n```context-manifest\n");
            result.push_str(&block);
            if !block.ends_with('\n') {
                result.push('\n');
            }
            result.push_str("```\n");
        }

        let after = crate::brain::tokenizer::count_tokens(&result);

        // The two invariants this guard exists to protect. WARN rather than
        // ship a document that violates them.
        Self::warn_on_summary_invariants(&result);
        if after > budget_tokens {
            tracing::warn!(
                "enforce_summary_budget: must-keep sections alone are {} tokens against a {} \
                 budget — the summariser prompt is asking for more than it can keep (#1930)",
                after,
                budget_tokens
            );
        }

        tracing::info!(
            "enforce_summary_budget: trimmed: {} -> {} tokens (budget {})",
            before,
            after,
            budget_tokens
        );
        result
    }

    /// WARN when a continuation document is missing a block a woken agent cannot
    /// recover from anywhere else (#1930, #1933).
    ///
    /// Runs on the UNDER-budget path too, not only after a trim: a document cut
    /// at source is short AND incomplete at once, and the trim-only check could
    /// never see that combination. `parse_context_manifest` is
    /// position-independent, so this is a real presence test rather than a shape
    /// guess.
    fn warn_on_summary_invariants(document: &str) {
        if parse_context_manifest(document).is_none() {
            tracing::warn!(
                "enforce_summary_budget: continuation document carries no context-manifest fence (#1933)"
            );
        }
        if !has_obligation_status(document) {
            tracing::warn!(
                "enforce_summary_budget: continuation document carries no §0 obligation-status line (#1933)"
            );
        }
    }

    /// WARN when the summariser's own stop reason says the document is
    /// incomplete (#1933).
    ///
    /// A `MaxTokens` stop means the request hit its output allowance: the
    /// document is CUT, whatever its token count reads. On 2026-10-04 the cut
    /// landed before §10, so the persisted marker carried no `context-manifest`
    /// fence — and the trim guard never saw it, because a capped document sits
    /// exactly at the budget and the guard only inspected over-budget ones.
    /// This is the visibility leg: read the response's own verdict, not the
    /// document's size.
    pub(crate) fn warn_if_summary_truncated(
        stop_reason: Option<crate::brain::provider::StopReason>,
        usage: &crate::brain::provider::TokenUsage,
    ) {
        if matches!(
            stop_reason,
            Some(crate::brain::provider::StopReason::MaxTokens)
        ) {
            tracing::warn!(
                "compute_compaction_summary: summariser stopped at MaxTokens (output {} / reasoning {}) \
                 — the continuation document is TRUNCATED and may have lost its load-bearing blocks (#1933)",
                usage.output_tokens,
                usage.reasoning_tokens,
            );
        }
    }

    /// Append a harness-written correction for artifacts the summary reports as
    /// complete but which no tool call in the summarised conversation produced
    /// (#482).
    ///
    /// Rides inside the persisted marker, like `decorate_compaction_summary`'s
    /// blocks: a correction a summary cannot be trusted to carry has to arrive
    /// WITH it, not as a later message that can scroll away. The correction
    /// quotes each claim back, the posture of `nudge::unbacked_facts_nudge` —
    /// the model cannot reword its way past a name that appears nowhere.
    fn flag_unbacked_artifacts(summary: String, messages: &[Message]) -> String {
        let claims = crate::brain::agent::service::phantom::claimed_artifacts(&summary);
        if claims.is_empty() {
            return summary;
        }
        let evidence = Self::compaction_evidence(messages);
        let unbacked: Vec<String> = claims
            .into_iter()
            .filter(|claim| {
                crate::brain::agent::service::phantom::claimed_artifact_unbacked(claim, &evidence)
            })
            .collect();
        if unbacked.is_empty() {
            return summary;
        }
        let quoted = unbacked
            .iter()
            .map(|f| format!("`{f}`"))
            .collect::<Vec<_>>()
            .join(", ");
        tracing::warn!(
            "Compaction summary reports artifacts with no producing call: {}",
            quoted
        );
        format!(
            "{summary}\n\n[System: This summary reports {quoted} as completed, but no tool call in \
             the summarised conversation produced it — no `write_file`/`edit_file` names it and no \
             tool result contains it. Treat it as NOT produced and re-derive the work from the \
             conversation above; do not transcribe or extend anything the summary describes as \
             finished. An artifact is real only when a tool call in this conversation created it.]"
        )
    }

    /// Everything the summarised conversation observed: tool results, tool
    /// inputs (what was actually run — a path in a `write_file` argument is as
    /// real as one in its result) and the user's own text.
    ///
    /// The summariser's own output is deliberately absent: the summary is the
    /// claim under test, and letting it vouch for itself is the defect.
    fn compaction_evidence(messages: &[Message]) -> String {
        let mut evidence = String::new();
        for msg in messages {
            for block in &msg.content {
                match block {
                    ContentBlock::ToolResult { content, .. } => {
                        evidence.push_str(content);
                        evidence.push('\n');
                    }
                    ContentBlock::ToolUse { input, .. } => {
                        evidence.push_str(&input.to_string());
                        evidence.push('\n');
                    }
                    ContentBlock::Text { text }
                        if msg.role == crate::brain::provider::Role::User =>
                    {
                        evidence.push_str(text);
                        evidence.push('\n');
                    }
                    _ => {}
                }
            }
        }
        evidence
    }

    /// Apply a previously-computed compaction summary to a live `AgentContext`.
    ///
    /// Builds the recovered-brain preamble plus a snapshot of the most recent
    /// messages, then calls `AgentContext::compact_with_summary` to do the
    /// in-place swap (replace older messages with the summary, keep the recent
    /// tail within 55% of the window).
    /// Apply a summary that was computed in the background, keeping whatever
    /// the turn appended while the summariser was still thinking.
    ///
    /// `apply_compaction_summary` clears the whole message vector, which is
    /// right when the summariser blocked the turn: nothing could arrive in the
    /// meantime. A background summariser leaves a gap, and everything in that
    /// gap (the tool calls and results of the turn still running) is work the
    /// summary never saw and cannot describe. Clearing it would silently
    /// delete the most recent thing the agent did.
    pub(crate) fn apply_compaction_summary_after(
        context: &mut AgentContext,
        scope: CompactionScope,
        summary: &str,
        snapshot_len: usize,
    ) -> String {
        // The index addresses the message vector this turn is appending to.
        // A vector shorter than the snapshot cannot be that one: the context
        // is rebuilt from the database at the start of every turn, so this
        // means the pending entry outlived its turn. There is no delta to
        // keep, only a summary to apply.
        if snapshot_len > context.messages.len() {
            tracing::warn!(
                "Compaction snapshot ({snapshot_len} messages) outlived its context ({}) — \
                 applying the summary without a delta",
                context.messages.len(),
            );
            return Self::apply_scoped_compaction_summary(context, scope, summary);
        }

        let mut delta = context.messages.split_off(snapshot_len);
        // The summary replaces everything the snapshot covered, so it is
        // computed against exactly what it summarises — scoped (#1649): a
        // delta segment replaces only what followed the last marker, a
        // consolidation replaces the segment run, full window clears all.
        let applied_marker = Self::apply_scoped_compaction_summary(context, scope, summary);

        // #943: the carried tail is bounded, oldest dropped first. It used to
        // be unbounded — its size was whatever arrived while the summariser
        // ran, so a busy lane (cron fires, sub-agent wakes) could re-inflate
        // the window the summary had just cleared, with no token accounting
        // anywhere in this function.
        let tail_budget = context.verbatim_tail_tokens();
        let mut carried = 0usize;
        let mut keep_from = delta.len();
        for msg in delta.iter().rev() {
            let t = context.estimate_message_tokens(msg);
            if carried.saturating_add(t) > tail_budget {
                break;
            }
            carried += t;
            keep_from -= 1;
        }
        if keep_from > 0 {
            // WARN, not debug: these messages are work the summary was never
            // computed against, so dropping them loses facts. It is still the
            // right trade against an unbounded window, but it must be visible.
            tracing::warn!(
                "Compaction delta: dropping {keep_from} message(s) that arrived \
                 during the summariser call and exceeded the {tail_budget}-token \
                 verbatim tail — the summary predates them"
            );
            delta.drain(..keep_from);
        }

        // The summary lands as a user message. A delta opening with tool
        // results has lost the assistant tool_use that authorised them, and
        // the provider rejects that shape outright.
        let orphans = delta
            .iter()
            .position(|m| !AgentContext::is_orphaned_tool_result_msg(m))
            .unwrap_or(delta.len());
        if orphans > 0 {
            tracing::debug!("Compaction delta: dropping {orphans} orphaned tool results");
            delta.drain(..orphans);
        }

        let kept = delta.len();
        for msg in delta {
            context.add_message(msg);
        }
        tracing::info!("Compaction: kept {kept} messages appended during the summariser call");
        applied_marker
    }

    /// Returns the marker text the apply welded into the context (#1928).
    pub(super) fn apply_compaction_summary(context: &mut AgentContext, summary: &str) -> String {
        // The synchronous path (`/compact`, the two emergency recoveries):
        // full-window by definition — the summary was computed against the
        // whole window, so the apply clears everything and prepends one
        // marker. The background path goes through
        // `apply_compaction_summary_after` with the scope it spawned with.
        Self::apply_scoped_compaction_summary(context, CompactionScope::FullWindow, summary)
    }

    /// Scope-aware apply (#1649). Welds the recovered brain context onto the
    /// summary, then: full window clears all and prepends one marker
    /// (classic behaviour); delta appends a frozen segment after the existing
    /// markers; consolidation replaces the segment run with one superseding
    /// marker.
    pub(crate) fn apply_scoped_compaction_summary(
        context: &mut AgentContext,
        scope: CompactionScope,
        summary: &str,
    ) -> String {
        // No verbatim recent-pairs snapshot is welded onto the marker: the
        // summary prompt's "IMMEDIATE TASK" section already quotes the last
        // exchange from the history, so a second copy duplicated it inside
        // the marker and re-billed it on every subsequent compaction (#1649).
        // The raw tail a background compaction keeps
        // (`apply_compaction_summary_after`) covers continuation mechanically.
        // No brain files are welded on either: SOUL.md, USER.md and AGENTS.md
        // are injected into the system prompt on EVERY turn
        // (`prompt_builder::CORE_BRAIN_FILES` / `ALWAYS_LOADED_FILES`), and
        // `system_brain` survives compaction untouched. Welding them here
        // duplicated ~16k tokens onto the in-memory marker while the marker
        // persisted to the DB carried none, so the confirmation reported a
        // size the next request never had, and the background path re-sent
        // the duplicate on every turn until the next reload (#1676).
        let marker_body = summary.to_string();

        let applied_marker = match scope {
            CompactionScope::FullWindow => {
                // After compaction, the summary IS the conversation — it's
                // prepended as a single user message and the agent picks up
                // from there. The pre-compaction tail is NOT preserved at
                // full width: the summary already embeds the recent-snapshot
                // prose, so keeping the whole raw tail on top would duplicate
                // ~half the window and defeat the purpose of compacting.
                //
                // It is preserved at TAIL width (#943). This arm used to pass
                // `0`, which keeps literally nothing (`available` saturates to
                // zero, the reverse walk breaks on its first message, and the
                // `max(2)` floor is gated on a non-zero budget). So the exact
                // words the user is continuing from survived only as the
                // summariser's prose quotation — and `enforce_summary_budget`
                // then trims that quotation to the document budget.
                let tail = context.verbatim_tail_tokens();
                context.compact_with_verbatim_tail(marker_body, tail)
            }
            CompactionScope::DeltaSinceMarker => {
                // The prior frozen segments stay in force verbatim; only the
                // messages the delta summary described are replaced by this
                // new segment marker.
                context.compact_with_delta_summary(marker_body)
            }
            CompactionScope::SegmentConsolidation => {
                // The segments merge into ONE superseding marker; the tail
                // after the segment run is untouched.
                context.consolidate_segments(marker_body)
            }
        };

        tracing::info!(
            "Context compacted ({scope:?}): now at {:.0}% ({} tokens)",
            context.usage_percentage(),
            context.token_count
        );
        applied_marker
    }

    /// Save a compaction summary to a daily memory log at `~/.opencrabs/memory/YYYY-MM-DD.md`.
    ///
    /// Multiple compactions per day append to the same file. The brain workspace's
    /// `MEMORY.md` is left untouched — it stays as user-curated durable memory.
    ///
    /// `prompt_stamp` (`source:sha256-prefix`, from
    /// [`CompactionPrompts::stamp`]) rides in the header, so a summary on disk
    /// can be matched to the prompt text that produced it. #929 exists because
    /// nobody could answer "which prompt wrote this summary?"; the stamp is the
    /// cheap half of that answer. It is passed in rather than re-loaded here on
    /// purpose — a second `load()` could read a different file than the one that
    /// built the request, which is the misattribution the stamp prevents.
    pub(super) async fn save_compaction_summary_to_memory(
        summary: &str,
        prompt_stamp: &str,
    ) -> std::result::Result<(), String> {
        let memory_dir = crate::config::opencrabs_home().join("memory");

        std::fs::create_dir_all(&memory_dir)
            .map_err(|e| format!("Failed to create memory directory: {}", e))?;

        let date = chrono::Local::now().format("%Y-%m-%d");
        let memory_path = memory_dir.join(format!("{}.md", date));

        // Read existing content (if any — multiple compactions per day stack)
        let existing = std::fs::read_to_string(&memory_path).unwrap_or_default();

        let timestamp = chrono::Local::now().format("%Y-%m-%d %H:%M:%S");
        let new_content = format!(
            "{}\n\n---\n\n## Auto-Compaction Summary ({}) [prompt {}]\n\n{}\n",
            existing.trim(),
            timestamp,
            prompt_stamp,
            summary
        );

        std::fs::write(&memory_path, new_content.trim_start())
            .map_err(|e| format!("Failed to write daily memory log: {}", e))?;

        tracing::info!("Saved compaction summary to {}", memory_path.display());
        Ok(())
    }
}

/// Harness-written plan-state block appended to every compaction summary
/// while session plan artifacts exist: state (Editing / Active), the
/// absolute `.md` path when present, and one line on what to do next.
/// `None` when the session is NoPlan (no plan chatter in the summary).
pub(crate) async fn plan_state_block(session_id: Uuid) -> Option<String> {
    use crate::utils::plan_files::{self, PlanModeState};
    let md = plan_files::plan_md_path(session_id).await;
    match plan_files::plan_mode_state(session_id).await {
        PlanModeState::NoPlan => None,
        PlanModeState::PreInitEditing => Some(
            "[PLAN MODE — injected by the harness, not from the user]\n\
             \n\
             **State:** Pre-init (no plan yet)\n\
             \n\
             **Now:**\n\
             1. Explore with reads/search/bash\n\
             2. Call: `plan init mode='design' title='<3-8 words>'`\n\
             \n\
             **Next:** Harness creates .md with template for you to edit.\n\
             \n\
             **Constraints:**\n\
             - No project file edits\n\
             - No pasting plan in chat"
                .to_string(),
        ),
        PlanModeState::PostInitEditing => {
            // PostInitEditing either has a design `.md` to refine (design track)
            // or no `.md` at all — a checklist plan waiting on the user's
            // Approve (#1145: checklist init no longer writes the scaffold).
            // Don't point at a file that does not exist.
            if md.exists() {
                Some(format!(
                    "[PLAN MODE — injected by the harness, not from the user]\n\
                     \n\
                     **State:** Editing design prose\n\
                     **File:** {}\n\
                     \n\
                     **Now:**\n\
                     1. Re-read .md\n\
                     2. Refine design\n\
                     \n\
                     **Next:** Wait for `/execute` approval.\n\
                     \n\
                     **Constraints:**\n\
                     - No `plan start`/`complete`\n\
                     - No project file edits\n\
                     - No pasting plan in chat",
                    md.display()
                ))
            } else {
                Some(format_checklist_editing_reminder().to_string())
            }
        }
        PlanModeState::Active => {
            let (title, done, total) = plan_files::load_plan(session_id)
                .await
                .map(|p| {
                    let done = p
                        .tasks
                        .iter()
                        .filter(|t| {
                            matches!(
                                t.status,
                                crate::tui::plan::TaskStatus::Completed
                                    | crate::tui::plan::TaskStatus::Skipped
                            )
                        })
                        .count();
                    (p.title, done, p.tasks.len())
                })
                .unwrap_or_default();
            let md_line = if md.exists() {
                format!("\nPlan document (frozen): {}", md.display())
            } else {
                String::new()
            };
            Some(format!(
                "## PLAN STATE (harness)\n\
                 State: Active checklist \"{title}\" ({done}/{total} done).{md_line}\n\
                 Next: call plan start (no args) to resurface the in-progress task, \
                 then continue executing the checklist."
            ))
        }
    }
}

/// The Editing reminder for a CHECKLIST plan: the checklist is the
/// deliverable and there is no design document to refine (#1145 — checklist
/// init no longer writes the scaffold `.md`). Pure, like
/// [`format_editing_reminder`].
pub(crate) fn format_checklist_editing_reminder() -> &'static str {
    "[PLAN MODE — injected by the harness, not from the user]\n\
     \n\
     **State:** Checklist plan in Editing (no design document)\n\
     \n\
     **Now:**\n\
     1. The task list is already visible to the user in the plan card.\n\
     2. Do NOT restate the tasks in chat.\n\
     \n\
     **Next:** Wait for the user to approve (Approve button or `/execute`).\n\
     \n\
     **Constraints:**\n\
     - No `plan start`/`complete`\n\
     - No project file edits"
}

/// Build the pinned Editing reminder: the session is in Plan mode, so the
/// turn refines the SESSION PLAN instead of executing. `md_path` is the
/// design document once `plan init` created it (post-init); `None` means
/// pre-init (no document yet). Pure (no IO) so it's unit-testable.
pub(crate) fn format_editing_reminder(md_path: Option<std::path::PathBuf>) -> String {
    match md_path {
        Some(md) => format!(
            "[PLAN MODE — injected by the harness, not from the user]\n\
             \n\
             **State:** Editing design prose\n\
             **File:** {}\n\
             \n\
             **Now:**\n\
             1. Re-read .md\n\
             2. Refine design\n\
             \n\
             **Next:** Wait for `/execute` approval.\n\
             \n\
             **Constraints:**\n\
             - No `plan start`/`complete`\n\
             - No project file edits\n\
             - No pasting plan in chat",
            md.display()
        ),
        None => "[PLAN MODE — injected by the harness, not from the user]\n\
                 \n\
                 **State:** Pre-init (no plan yet)\n\
                 \n\
                 **Now:**\n\
                 1. Explore with reads/search/bash\n\
                 2. Call: `plan init mode='design' title='<3-8 words>'`\n\
                 \n\
                 **Next:** Harness creates .md with template for you to edit.\n\
                 \n\
                 **Constraints:**\n\
                 - No project file edits\n\
                 - No pasting plan in chat"
            .to_string(),
    }
}

/// Build the pinned plan reminder from a plan document. `None` unless the
/// checklist is live (Active) with unresolved tasks — Editing (pre-init flag
/// or design prose) gets no execution nagging. Pure (no IO) so it's
/// unit-testable; `active_plan_reminder` does the file load.
pub(crate) fn format_plan_reminder(plan: &crate::tui::plan::PlanDocument) -> Option<String> {
    use crate::tui::plan::{PlanStatus, TaskStatus};

    if plan.status != PlanStatus::Active || plan.pre_init_editing {
        return None;
    }
    let total = plan.tasks.len();
    let done = plan
        .tasks
        .iter()
        .filter(|t| matches!(t.status, TaskStatus::Completed))
        .count();
    // Skipped tasks are intentionally resolved; once every task is done or
    // skipped there's nothing left to nag about — and nothing left to report
    // as open work. `unresolved_tasks` is the single definition of that
    // predicate; the goal evidence pack reads it too (#299).
    if unresolved_tasks(plan).is_empty() {
        return None;
    }

    let mut out = format!(
        "[ACTIVE PLAN REMINDER — injected by the harness, not from the user]\n\
         📋 Plan: \"{}\" ({done}/{total} done). Keep executing it; do not abandon it. \
         Use the plan tool's `complete` as you finish each task — it names the next eligible \
         one — then `start` that task; `start` also (re)surfaces its full details.\n\
         Do NOT repeat the plan title or this reminder text in your response — the \
         plan is already displayed to the user by the surface they are reading.\n",
        plan.title
    );
    let mut tasks: Vec<&crate::tui::plan::PlanTask> = plan.tasks.iter().collect();
    tasks.sort_by_key(|t| t.order);
    // Resolve the set of completed/skipped task ids once, so we can name a
    // blocked task's specific unmet dependencies.
    let resolved_ids: std::collections::HashSet<uuid::Uuid> = plan
        .tasks
        .iter()
        .filter(|t| matches!(t.status, TaskStatus::Completed | TaskStatus::Skipped))
        .map(|t| t.id)
        .collect();
    for t in &tasks {
        // [Type, N★] suffix gives the model the task shape at a glance.
        let meta = format!("[{}, {}★]", t.task_type, t.complexity.clamp(1, 5));
        match &t.status {
            TaskStatus::InProgress => {
                out.push_str(&format!("→ Task {}: {} {meta}\n", t.order, t.title));
                if !t.description.is_empty() {
                    let desc: String = t.description.chars().take(160).collect();
                    out.push_str(&format!("  {desc}\n"));
                }
                if !t.acceptance_criteria.is_empty() {
                    let crit = t.acceptance_criteria.join(" • ");
                    out.push_str(&format!("  Criteria: • {crit}\n"));
                }
            }
            TaskStatus::Pending => {
                // Pending but with unmet deps → call it out as blocked, naming them.
                let unmet: Vec<usize> = t
                    .dependencies
                    .iter()
                    .filter_map(|d| d.as_uuid())
                    .filter(|id| !resolved_ids.contains(id))
                    .filter_map(|id| plan.tasks.iter().find(|x| x.id == id).map(|x| x.order))
                    .collect();
                if unmet.is_empty() {
                    out.push_str(&format!("☐ Task {}: {} {meta}\n", t.order, t.title));
                } else {
                    let blockers = unmet
                        .iter()
                        .map(|o| o.to_string())
                        .collect::<Vec<_>>()
                        .join(", ");
                    out.push_str(&format!(
                        "⊘ Task {}: {} {meta} (blocked: {blockers})\n",
                        t.order, t.title
                    ));
                }
            }
            TaskStatus::Failed => out.push_str(&format!(
                "✗ Task {}: {} {meta} (failed — retry/fix)\n",
                t.order, t.title
            )),
            TaskStatus::Blocked(reason) => out.push_str(&format!(
                "⊘ Task {}: {} {meta} (blocked: {reason})\n",
                t.order, t.title
            )),
            TaskStatus::Completed | TaskStatus::Skipped => {}
        }
    }
    Some(out)
}

/// #834: the hold prompt injected when an Active checklist is still open at the
/// end of a text-only iteration.
///
/// Pure, like `format_plan_reminder` above: the wording stays unit-testable
/// without a provider. Whether it is injected at all is the loop's decision
/// (`tool_loop::plan_hold_allowed`) because that needs turn-local state.
///
/// The last sentence is load-bearing: the honest exit has to be cheaper than a
/// fabricated continuation, or the hold becomes a re-reporting loop.
pub(crate) fn format_plan_hold(unresolved: &[String]) -> Option<String> {
    if unresolved.is_empty() {
        return None;
    }
    let n = unresolved.len();
    let noun = if n == 1 { "task" } else { "tasks" };
    let mut out = format!(
        "[PLAN HOLD — injected by the harness, not from the user]\n\
         The checklist is still Active with {n} {noun} open:\n"
    );
    for task in unresolved {
        out.push_str(&format!("  {task}\n"));
    }
    out.push_str(
        "Advance the next eligible task NOW: `start` it, do its work, then `complete` it. \
         Do not summarise the plan, restate progress, or ask what to do next — the checklist \
         is already on the user's screen. If you genuinely cannot proceed, say what blocks \
         you and stop.\n",
    );
    Some(out)
}

/// The plan's unresolved tasks, as `"<order>. <title>"`, in plan order.
///
/// A task is unresolved unless it is `Completed` or `Skipped`: `Pending`,
/// `InProgress`, `Failed` and `Blocked` are all still outstanding work. This is
/// the ONE definition of that predicate — the plan reminder uses it to decide
/// whether to nag at all, and the goal evidence pack (#299) uses it to tell the
/// judge what is still open, so the two can never disagree about whether a plan
/// is finished.
pub(crate) fn unresolved_tasks(plan: &crate::tui::plan::PlanDocument) -> Vec<String> {
    use crate::tui::plan::TaskStatus;

    let mut tasks: Vec<&crate::tui::plan::PlanTask> = plan
        .tasks
        .iter()
        .filter(|t| !matches!(t.status, TaskStatus::Completed | TaskStatus::Skipped))
        .collect();
    tasks.sort_by_key(|t| t.order);
    tasks
        .into_iter()
        .map(|t| format!("{}. {} ({})", t.order, t.title, t.status))
        .collect()
}
