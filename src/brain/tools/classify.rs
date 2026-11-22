//! Shared read-only / destructive classifier for tool calls.
//!
//! Both the approval policy (`requires_approval` in `trait.rs`) and the
//! plan-mode gate (`check_plan_gate` in `plan_gate.rs`) call into this
//! module to answer the same question: "is this tool call destructive?"
//! Keeping the classifier in one place means the two decision layers
//! share a single source of truth for what counts as a side effect,
//! while each layer applies its own policy on top.
//!
//! Future: this is the natural slot for an LLM judge that inspects the
//! actual command text (e.g. `bash ls` = read-only, `bash rm` =
//! destructive) without touching either decision layer.

use super::r#trait::{ToolCapability, ToolHints};
use serde_json::Value;

/// Safety class used by restart recovery. Read-only calls can be replayed;
/// mutating calls need an execution receipt, and destructive calls require an
/// owner decision when their receipt is unknown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EffectClass {
    ReadOnly,
    Mutating,
    Destructive,
}

/// Classify a call from its tool name and arguments without invoking an LLM.
/// Tool hints provide the default for ordinary tools; shell and `oc-*` tools
/// need argument inspection because one name can represent either a read or a
/// side effect.
pub(crate) fn classify_call(tool_name: &str, args: &Value) -> EffectClass {
    if tool_name == "bash" {
        let command = args
            .get("command")
            .and_then(Value::as_str)
            .unwrap_or_default();
        return classify_shell_command(command);
    }

    if tool_name.starts_with("oc-") {
        let text = format!("{} {}", tool_name, args).to_ascii_lowercase();
        if [
            "ship",
            "deploy",
            "push",
            "reset --hard",
            "branch -d",
            "branch --delete",
            "restart",
        ]
        .iter()
        .any(|needle| text.contains(needle))
        {
            return EffectClass::Destructive;
        }
        if ["list", "show", "view", "status", "read", "check", "report"]
            .iter()
            .any(|needle| text.contains(needle))
        {
            return EffectClass::ReadOnly;
        }
        return EffectClass::Mutating;
    }

    match tool_name {
        "read_file" | "ls" | "glob" | "grep" | "web_search" | "exa_search" | "memory_search"
        | "http_request" | "session_search" | "agent_progress" | "tasks_list" | "task_output"
        | "task_wait" => EffectClass::ReadOnly,
        "write_file"
        | "edit_file"
        | "hashline_edit"
        | "config_manager"
        | "write_opencrabs_file"
        | "telegram_send"
        | "discord_send"
        | "slack_send"
        | "whatsapp_send"
        | "trello_send"
        | "a2a_send"
        | "spawn_agent"
        | "resume_agent"
        | "stop_agent"
        | "close_agent"
        | "send_input" => EffectClass::Mutating,
        "evolve" => EffectClass::Destructive,
        _ => EffectClass::ReadOnly,
    }
}

fn classify_shell_command(command: &str) -> EffectClass {
    let command = command.to_ascii_lowercase();
    if [
        "oc-ship-chain",
        "oc-deploy",
        "evolve",
        "git push",
        "git reset --hard",
        "git branch -d",
        "git branch --delete",
        "systemctl restart",
        "service restart",
    ]
    .iter()
    .any(|needle| command.contains(needle))
    {
        return EffectClass::Destructive;
    }
    if [
        "rm ",
        "mv ",
        "cp ",
        "mkdir",
        "touch ",
        "sed ",
        "python",
        "git commit",
        "git checkout",
        "git switch",
        "git merge",
        "git rebase",
        "curl ",
        "wget ",
        ">",
        "tee ",
    ]
    .iter()
    .any(|needle| command.contains(needle))
    {
        return EffectClass::Mutating;
    }
    EffectClass::ReadOnly
}

/// The three capability flags that mark a tool as destructive (having
/// side effects beyond reading).
const DESTRUCTIVE_CAPS: [ToolCapability; 3] = [
    ToolCapability::WriteFiles,
    ToolCapability::ExecuteShell,
    ToolCapability::SystemModification,
];

/// Returns `true` if the given capabilities include any destructive
/// (side-effecting) flag: `WriteFiles`, `ExecuteShell`, or
/// `SystemModification`.
pub(crate) fn is_destructive(capabilities: &[ToolCapability]) -> bool {
    capabilities
        .iter()
        .any(|cap| DESTRUCTIVE_CAPS.contains(cap))
}

/// Returns `true` if the given capabilities are purely read-only (no
/// destructive flags). Inverse of [`is_destructive`].
#[allow(dead_code)]
pub(crate) fn is_read_only(capabilities: &[ToolCapability]) -> bool {
    !is_destructive(capabilities)
}

// --- MCP-style behavioral risk classification -------------------------
//
// The Model Context Protocol annotates each tool with orthogonal
// behavioral hints (`readOnlyHint`, `destructiveHint`, `idempotentHint`,
// `openWorldHint`) rather than a flat capability taxonomy. `ToolHints`
// (in `trait.rs`) mirrors that model with pessimistic defaults. These
// hint-based classifiers are the forward path: the capability functions
// above remain only as a migration bridge while tools move from
// `capabilities()` to explicit `hints()`.

/// MCP-style: does this tool have destructive side effects?
///
/// A tool is destructive when it modifies its environment (`read_only ==
/// false`) AND the modification is not purely additive (`destructive ==
/// true`). A read-only tool is never destructive.
pub(crate) fn is_destructive_hints(hints: &ToolHints) -> bool {
    !hints.read_only && hints.destructive
}

/// MCP-style: is this tool purely read-only (no environment modification)?
#[allow(dead_code)]
pub(crate) fn is_read_only_hints(hints: &ToolHints) -> bool {
    hints.read_only
}
