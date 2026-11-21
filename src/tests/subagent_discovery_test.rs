//! Sentinel for `spawn_agent`'s reachability and its description (#861).
//!
//! Context (2026-11-17): `spawn_agent` was an EXTENDED tool, so a session only
//! saw it after calling `tool_search` — and nothing told it to. Measured over
//! seven days: 77 spawns in 42,011 tool calls (0.18%), while hard rule 26 in
//! AGENTS.md instructs every session to delegate long-running work. The tool
//! existed, the rule existed, and the two never met.
//!
//! The fix was to promote it into `CORE_TOOLS`, which makes the schema ship in
//! every request and removes the search step entirely. These tests pin the
//! promotion and the invariants it must not break.
//!
//! NOTE the deliberate asymmetry, and why the assertion below looks inverted:
//! `ToolRegistry::search_tools` (`registry.rs`) filters out `catalog::is_core`
//! tools, because a core tool's schema already ships and there is nothing for
//! `tool_search` to activate. So after the promotion `search_tools("subagent")`
//! must NOT return `spawn_agent` — asserting the opposite would either fail or
//! force the core filter's removal, which would break the lazy-tools model that
//! keeps a "reply yes" turn from shipping ~95 schemas. The discovery win is
//! structural, not a better score.

use std::collections::HashSet;
use std::sync::Arc;

use crate::brain::tools::Tool;
use crate::brain::tools::ToolRegistry;
use crate::brain::tools::subagent::{SpawnAgentTool, SubAgentManager, WaitAgentTool};

/// A registry holding the two sub-agent tools whose visibility differs: one
/// promoted to core, one still extended.
fn registry_with_subagent_tools() -> ToolRegistry {
    let manager = Arc::new(SubAgentManager::new());
    let reg = ToolRegistry::new();
    reg.register(Arc::new(SpawnAgentTool::new(
        manager.clone(),
        Arc::new(ToolRegistry::new()),
    )));
    reg.register(Arc::new(WaitAgentTool::new(manager)));
    reg
}

/// The point of #861: a session sees `spawn_agent` WITHOUT having discovered
/// it, while `wait_agent` still costs a `tool_search`. If this flips back, the
/// 0.18% adoption rate comes back with it.
#[test]
fn spawn_agent_ships_without_activation_while_extended_tools_do_not() {
    let reg = registry_with_subagent_tools();
    let shipped: Vec<String> = reg
        .get_tool_definitions_filtered(&HashSet::new())
        .into_iter()
        .map(|t| t.name)
        .collect();

    assert!(
        shipped.iter().any(|n| n == "spawn_agent"),
        "spawn_agent must be in the DEFAULT definitions (no tool_search needed) — \
         that is the whole fix for #861. Shipped set: {shipped:?}"
    );
    assert!(
        !shipped.iter().any(|n| n == "wait_agent"),
        "wait_agent must stay EXTENDED: it is only useful after a spawn, so shipping \
         its schema to every session is pure cost. Shipped set: {shipped:?}"
    );
}

/// The control that keeps the test above honest. `search_tools` hides
/// `spawn_agent` because it is core — so prove the query path still works by
/// finding the extended sibling with the same query family. Without this, a
/// broken `search_tools` would pass the previous test too.
#[test]
fn search_tools_still_reaches_extended_agent_tools_but_not_core_ones() {
    let reg = registry_with_subagent_tools();

    let found: Vec<String> = reg
        .search_tools("subagent", 10)
        .into_iter()
        .map(|(name, ..)| name)
        .collect();
    assert!(
        !found.iter().any(|n| n == "spawn_agent"),
        "spawn_agent is CORE now, and search_tools filters core tools out by design \
         (their schemas already ship). Returning it here would mean the filter broke. \
         Found: {found:?}"
    );

    let control: Vec<String> = reg
        .search_tools("agent", 10)
        .into_iter()
        .map(|(name, ..)| name)
        .collect();
    assert!(
        control.iter().any(|n| n == "wait_agent"),
        "CONTROL: search_tools must still find an extended agent tool, otherwise the \
         assertion above passes for the wrong reason (a dead search path). \
         Found: {control:?}"
    );
}

/// The description is what the model reads when deciding to delegate. Pin the
/// trigger, the counter-trigger, and the order of the follow-ups.
#[test]
fn spawn_agent_description_names_trigger_counter_trigger_and_read_back_order() {
    let tool = SpawnAgentTool::new(
        Arc::new(SubAgentManager::new()),
        Arc::new(ToolRegistry::new()),
    );
    let desc = tool.description();

    for trigger in [
        "long-running",
        "research",
        "bulk",
        "code development",
        "parallel",
    ] {
        assert!(
            desc.contains(trigger),
            "spawn_agent's description must name `{trigger}` so the model can \
             recognise the work hard rule 26 sends to a sub-agent. \
             \nCurrent description: {desc}"
        );
    }

    assert!(
        desc.contains("Do NOT spawn"),
        "the description needs a COUNTER-trigger: without one, 'spawn a sub-agent' \
         reads as available for a single command or a one-line edit, where spawning \
         costs more than doing. \nCurrent description: {desc}"
    );

    // Hard rule 26 in one assertion: the completion wake is the normal read-back,
    // and `wait_agent` is the exception named last.
    let wake = desc
        .find("wake")
        .expect("description must name the completion wake");
    let wait = desc
        .find("wait_agent")
        .expect("description must name wait_agent so the exception is explicit");
    assert!(
        wake < wait,
        "the completion wake must be described BEFORE wait_agent: a child pushes its \
         result back and wakes the parent, so leading with wait_agent teaches sessions \
         to block a turn they did not need to block. \nCurrent description: {desc}"
    );
    for earlier in ["report_progress", "agent_progress", "send_input"] {
        let at = desc
            .find(earlier)
            .unwrap_or_else(|| panic!("description must name `{earlier}`"));
        assert!(
            at < wait,
            "`{earlier}` must be listed before `wait_agent` — it is the mid-run path, \
             and wait_agent is the last resort. \nCurrent description: {desc}"
        );
    }
}

/// `wait_agent`'s own description has to say what it is NOT for, because the
/// tool's name is an instruction in itself: "wait for the agent" reads as the
/// obvious way to collect a result.
#[test]
fn wait_agent_description_carries_the_do_not_block_caveat() {
    let tool = WaitAgentTool::new(Arc::new(SubAgentManager::new()));
    let desc = tool.description();

    assert!(
        desc.contains("Do NOT use this to wait for work you will be woken for"),
        "wait_agent must state the caveat in those words: children wake the parent on \
         completion, so blocking here only keeps the session out of the conversation. \
         \nCurrent description: {desc}"
    );
    assert!(
        desc.contains("resume_agent"),
        "the caveat is only actionable if it names the legitimate use — a genuine \
         precondition such as needing a child finished before resume_agent. \
         \nCurrent description: {desc}"
    );
}

/// Step 4(b) of #861: pin the promotion in the catalog itself, not just in the
/// registry it produces, and prove the headless roster still strips
/// `session_notify`. That assertion needs a control — `headless=false` must
/// still advertise it — otherwise an empty roster would pass for the wrong
/// reason.
///
/// `HEADLESS_UNADVERTISED` names two tools but only ONE of them is reachable
/// through this roster: `session_notify` is extended, `suggest_options` is CORE
/// (`catalog.rs:80`) and appears in no inventory group, so it is absent from
/// both rosters and asserting its presence on the interactive one is false by
/// construction. The first draft of this test did assert that and went red in
/// CI (11059 passed, 1 failed) — the roster is the wrong surface to observe a
/// core tool's headless filtering, and this test says so rather than leaving
/// the next reader to rediscover it.
#[test]
fn spawn_agent_is_core_and_the_headless_roster_still_strips_interactive_tools() {
    use crate::brain::tools::catalog;

    assert!(
        catalog::is_core("spawn_agent"),
        "spawn_agent must be CORE (#861): its schema ships in every request, which is \
         what removes the tool_search step a session had no reason to run."
    );
    for (category, names) in catalog::EXTENDED_TOOL_INVENTORY {
        assert!(
            !names.contains(&"spawn_agent"),
            "spawn_agent must NOT be advertised for tool_search in the {category:?} group: \
             its schema already ships, so advertising it promises a round-trip that does \
             nothing — the lie `tool_inventory_names_are_extended_not_core` refuses."
        );
    }

    let headless = catalog::tool_inventory_prompt(true);
    let interactive = catalog::tool_inventory_prompt(false);
    assert!(
        !headless.contains("session_notify"),
        "session_notify is unregistered on every headless surface (#129), so the \
         headless roster must not advertise it. Roster:\n{headless}"
    );
    assert!(
        interactive.contains("session_notify"),
        "CONTROL: session_notify must appear on an interactive roster, otherwise the \
         assertion above passes because the roster is empty rather than because it \
         filtered. Roster:\n{interactive}"
    );
    // `suggest_options` is the other half of HEADLESS_UNADVERTISED, but it is
    // CORE, so it is invisible here in BOTH directions — the roster is the wrong
    // surface for it. Pin the fact so nobody re-adds the assertion CI refuted.
    assert!(
        !interactive.contains("suggest_options") && !headless.contains("suggest_options"),
        "suggest_options is CORE (#129 filters it at the registry level, not the roster), \
         so it appears in neither roster. If this ever flips, the roster gained a core \
         name and `tool_inventory_names_are_extended_not_core` should be failing too."
    );
}
