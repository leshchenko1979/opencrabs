//! Live artifact producers (live-L2, #631).
//!
//! Produces a REAL artifact from a live provider so an eval grades real model
//! output rather than a fixture. [`produce_compaction_summary`] asks a provider
//! to compact a conversation into a continuation document, then the compaction
//! dataset's probes grade what survived.
//!
//! The continuation-document prompt is NOT defined here. #929: this module used
//! to carry a self-contained 5-section shadow prompt while the compaction path
//! served a 10-section one, so every score the eval reported graded a prompt no
//! user ever ran. [`produce_compaction_summary`] now builds its request from
//! [`CompactionPrompts`] — the same loader production uses — so the text is
//! shared by construction and cannot drift again.

use crate::brain::agent::context::CompactionScope;
use crate::brain::agent::service::prompt_file::{BodyValues, CompactionPrompts};
use crate::brain::agent::service::request_budget::COMPACTION_SUMMARY_MAX_TOKENS;
use crate::brain::provider::{ContentBlock, LLMRequest, Message, Provider, Tool};

use super::compaction::CompactionDataset;
use super::scorer::{Judge, Scorecard};

/// The nominal context window the eval's budget preamble states.
///
/// A dataset has no window of its own; production substitutes the live model's.
/// The value only shapes how the prompt describes pressure — it never changes
/// which facts the probes ask for — so a fixed, documented number keeps the
/// eval reproducible instead of inheriting whatever box ran it.
const EVAL_NOMINAL_WINDOW: usize = 200_000;

/// Stands in for the live context inventory table.
///
/// Production substitutes the real skill/tool/brain inventory. An eval has no
/// session, and inventing a table would put fabricated skill names in front of
/// the model, so the section states its own emptiness instead.
const EVAL_CONTEXT_INVENTORY: &str = "_No live session: this eval substitutes an empty inventory._";

/// A crude token estimate for the budget preamble: ~4 bytes per token.
///
/// The eval has no tokenizer. The estimate feeds the prompt's runtime values,
/// never its words, so approximation is honest here and is documented rather
/// than presented as a measurement.
fn approximate_tokens(conversation: &[Message]) -> usize {
    serde_json::to_string(conversation)
        .map(|json| json.len() / 4)
        .unwrap_or(0)
}

/// A representative tool set for behavioral evals — the config tooling a
/// self-aware agent should reach for (config_manager, tool_search,
/// load_brain_file) plus the generic tools a reimplementing agent would misuse
/// (bash, write_file). Descriptions mirror the real tools so the model behaves
/// as it would at runtime. Not the full registry — just enough to discriminate
/// "configure the built-in" from "build a replacement".
pub fn eval_tool_set() -> Vec<Tool> {
    let tool = |name: &str, description: &str, schema: serde_json::Value| Tool {
        name: name.to_string(),
        description: description.to_string(),
        input_schema: schema,
    };
    vec![
        tool(
            "config_manager",
            "Read or write OpenCrabs configuration (config.toml): enable and configure providers, \
             compiled features, channels, STT/TTS, and models.",
            serde_json::json!({"type":"object","properties":{"operation":{"type":"string"},"section":{"type":"string"},"key":{"type":"string"},"value":{"type":"string"}}}),
        ),
        tool(
            "tool_search",
            "Discover available tools by task description; returns matching tool schemas to activate.",
            serde_json::json!({"type":"object","properties":{"query":{"type":"string"}},"required":["query"]}),
        ),
        tool(
            "load_brain_file",
            "Load an OpenCrabs brain file (TOOLS.md, CODE.md, SECURITY.md, ...) for guidance.",
            serde_json::json!({"type":"object","properties":{"name":{"type":"string"}}}),
        ),
        tool(
            "bash",
            "Run a shell command.",
            serde_json::json!({"type":"object","properties":{"command":{"type":"string"}},"required":["command"]}),
        ),
        tool(
            "write_file",
            "Create a new file with the given content.",
            serde_json::json!({"type":"object","properties":{"path":{"type":"string"},"content":{"type":"string"}}}),
        ),
    ]
}

/// Extract the concatenated text of a provider response.
fn response_text(blocks: &[ContentBlock]) -> String {
    blocks
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Render a response for grading, including any TOOL CALLS the model made. A
/// tool-calling agent takes action by calling tools, not narrating — so the
/// grader must see the calls (e.g. `[tool_call] config_manager {…}`), or a
/// correct "call config_manager to enable local-stt" looks like an empty stub.
fn render_response(blocks: &[ContentBlock]) -> String {
    let mut out = String::new();
    for b in blocks {
        match b {
            ContentBlock::Text { text } if !text.trim().is_empty() => {
                out.push_str(text);
                out.push('\n');
            }
            ContentBlock::ToolUse { name, input, .. } => {
                out.push_str(&format!("[tool_call] {name} {input}\n"));
            }
            _ => {}
        }
    }
    out.trim().to_string()
}

/// Send a single user prompt to a live provider and return its rendered
/// response (text + any tool calls). When `tools` is non-empty the model may
/// call them — the whole point for a tool-calling agent — and the calls are
/// folded into the returned string so the grader sees the action taken. When
/// `system` is set (e.g. the real OpenCrabs system brain) it is attached. A
/// provider error returns a marked failure string, never a silent empty.
pub async fn produce_response(
    provider: &dyn Provider,
    model: &str,
    prompt: &str,
    system: Option<&str>,
    tools: &[Tool],
) -> String {
    let mut request = LLMRequest::new(model.to_string(), vec![Message::user(prompt.to_string())]);
    if let Some(sys) = system {
        request = request.with_system(sys);
    }
    if !tools.is_empty() {
        request = request.with_tools(tools.to_vec());
    }
    match provider.complete(request).await {
        Ok(resp) => render_response(&resp.content),
        Err(e) => format!("[produce failed: {e}]"),
    }
}

/// Ask a live provider to compact a conversation into a continuation document,
/// using the SAME prompt text the compaction path serves.
///
/// #929: the text comes from [`CompactionPrompts`], so the eval cannot grade a
/// prompt production never sends. What it does NOT reproduce is the runtime
/// substitution VALUES — there is no live session behind a fixed dataset, so the
/// window figures are approximated from the conversation and the inventory is
/// empty. That is a difference in the prompt's inputs, never in its words.
///
/// The scope prelude is [`CompactionScope::FullWindow`], which is empty: a
/// dataset is one whole conversation, so the classic first-compaction prompt is
/// the honest analogue. It is still requested through the accessor rather than
/// assumed empty, so a future non-empty full-window prelude reaches the eval.
///
/// `system` overrides the summariser system prompt when set (e.g. to attach the
/// real system brain); otherwise the shipped one is used. A provider error
/// returns a marked failure string (never a silent empty), so the grader scores
/// it as full fact loss rather than a spurious pass.
pub async fn produce_compaction_summary(
    provider: &dyn Provider,
    model: &str,
    conversation: &[Message],
    system: Option<&str>,
) -> String {
    // The compiled default, never `load()`: an eval that gated on the operator's
    // profile override would score differently on every box, and CI must gate
    // the prompt the binary ships. A profile's override is its own experiment.
    let prompts = CompactionPrompts::compiled_default();

    let snapshot_token_count = approximate_tokens(conversation);
    let mut prompt = prompts.scope_prelude(CompactionScope::FullWindow);
    prompt.push_str(&prompts.body(&BodyValues {
        snapshot_usage_pct: (snapshot_token_count as f64 / EVAL_NOMINAL_WINDOW as f64) * 100.0,
        snapshot_token_count,
        snapshot_max_tokens: EVAL_NOMINAL_WINDOW,
        remaining_budget: EVAL_NOMINAL_WINDOW.saturating_sub(snapshot_token_count),
        context_inventory: EVAL_CONTEXT_INVENTORY,
        // Production passes the session's own flag. The eval has no session; ON
        // is the configuration the overwhelming majority of runs use, and the
        // sentence it selects is the shorter of the two.
        auto_approve_tools: true,
        summary_budget_tokens: COMPACTION_SUMMARY_MAX_TOKENS,
    }));

    let mut messages = conversation.to_vec();
    messages.push(Message::user(prompt));
    // Same output allowance and reasoning posture as production (#778: reasoning
    // tokens are charged to the document's budget, so the summariser must not
    // think).
    let mut request = LLMRequest::new(model.to_string(), messages)
        .with_max_tokens(COMPACTION_SUMMARY_MAX_TOKENS)
        .with_reasoning_disabled();
    request = request.with_system(match system {
        Some(sys) => sys.to_string(),
        None => prompts.system_prompt().to_string(),
    });
    match provider.complete(request).await {
        Ok(resp) => response_text(&resp.content),
        Err(e) => format!("[compaction failed: {e}]"),
    }
}

/// End-to-end: produce a real compaction summary with `producer`, then grade the
/// dataset's probes against it with `judge`.
pub async fn run_compaction_eval(
    producer: &dyn Provider,
    producer_model: &str,
    judge: &dyn Judge,
    dataset: &CompactionDataset,
    system: Option<&str>,
) -> Scorecard {
    let summary =
        produce_compaction_summary(producer, producer_model, &dataset.messages(), system).await;
    dataset.judge_scorecard(judge, &summary).await
}
