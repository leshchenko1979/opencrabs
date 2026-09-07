//! Headless tool-surface gate (fork #129).
//!
//! Pins three owner-ruled invariants:
//! 1. `register_core_agent_tools(headless = true)` does NOT register
//!    `session_notify` or `suggest_options` — headless runs have no TUI and
//!    no channel state, so both can only mislead (a "success" verdict while
//!    the message parks and dies with the process; a suggestion that renders
//!    nowhere).
//! 2. The interactive surface (`headless = false`) still has both.
//! 3. Sub-agent children NEVER get `session_notify` (owner ruling: children
//!    report through the harness's final-message relay), regardless of the
//!    parent's surface.

use std::sync::Arc;

use crate::brain::tools::registry::ToolRegistry;
use crate::brain::tools::subagent::build_child_registry;
use crate::cli::tool_setup::register_core_agent_tools;
use crate::config::Config;
use crate::db::Database;

async fn interactive_registry() -> Arc<ToolRegistry> {
    let db = Database::connect_in_memory().await.expect("in-memory db");
    db.run_migrations().await.expect("migrations");
    let config = Config::default();
    let registry = Arc::new(ToolRegistry::new());
    let _subagent_manager = register_core_agent_tools(&registry, &db, &config, false);
    registry
}

async fn headless_registry() -> Arc<ToolRegistry> {
    let db = Database::connect_in_memory().await.expect("in-memory db");
    db.run_migrations().await.expect("migrations");
    let config = Config::default();
    let registry = Arc::new(ToolRegistry::new());
    let _subagent_manager = register_core_agent_tools(&registry, &db, &config, true);
    registry
}

#[tokio::test]
async fn headless_registry_excludes_session_notify_and_suggest_options() {
    let registry = headless_registry().await;

    assert!(
        !registry.has_tool("session_notify"),
        "headless registry must NOT register session_notify (#129): no in-process \
         routes exist, delivery can only Park, and the run dies with the message"
    );
    assert!(
        !registry.has_tool("suggest_options"),
        "headless registry must NOT register suggest_options (#129): no TUI or \
         channel sink exists, so suggestions render nowhere"
    );

    // The core workhorse set must survive the gate — the gate is scoped to
    // the two dead-surface tools, not a headless capability downgrade.
    for name in ["bash", "read_file", "write_file", "plan", "tool_search"] {
        assert!(
            registry.has_tool(name),
            "headless gate must not strip core tool '{name}'"
        );
    }
}

#[tokio::test]
async fn interactive_registry_keeps_session_notify_and_suggest_options() {
    let registry = interactive_registry().await;

    assert!(
        registry.has_tool("session_notify"),
        "interactive registry must keep session_notify (cross-session push, #1203)"
    );
    assert!(
        registry.has_tool("suggest_options"),
        "interactive registry must keep suggest_options (TUI/channel sink exists)"
    );
}

#[tokio::test]
async fn child_registry_excludes_session_notify_from_both_parent_surfaces() {
    for parent in &[interactive_registry().await, headless_registry().await] {
        let child = build_child_registry(parent);
        assert!(
            !child.has_tool("session_notify"),
            "sub-agent children must NEVER get session_notify (#129 owner ruling): \
             children report through the harness's final-message relay"
        );
        assert!(child.has_tool("bash"), "full-capability child keeps bash");
    }
}
