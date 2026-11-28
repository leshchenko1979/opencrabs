//! Live-sub-agent cap (#1084).
//!
//! Nothing used to count live children, so a model could fan out without bound —
//! a real orchestrator lane on this box spawned 13 children for 7 jobs because
//! no code refused it. `[agent] max_live_subagents` (default 8) now refuses a
//! spawn at admission, before a session, worktree or task exists, and the
//! refusal names the cap, the current live count and the two verbs that free a
//! slot.
//!
//! These tests pin the boundary itself and the two facts a cap must not get
//! wrong: `cap - 1` is still admitted, and a terminal child holds no slot.

use crate::brain::tools::subagent::manager::{SubAgent, SubAgentManager, SubAgentState};
use crate::brain::tools::subagent::spawn::live_cap_refusal;
use crate::config::AgentConfig;
use uuid::Uuid;

fn agent(id: &str, parent: Uuid, state: SubAgentState) -> SubAgent {
    SubAgent {
        state,
        input_tx: None,
        ..SubAgent::new(id.to_string(), "worker".to_string(), Uuid::new_v4(), parent)
    }
}

fn manager_with(parent: Uuid, states: &[(SubAgentState, &str)]) -> SubAgentManager {
    let manager = SubAgentManager::new();
    for (state, id) in states {
        manager.insert(agent(id, parent, state.clone()));
    }
    manager
}

// ─── The config key ────────────────────────────────────────────────────────

/// An untouched config must get the documented default, or every existing
/// install silently changes behaviour on upgrade.
#[test]
fn default_is_eight_live_children() {
    assert_eq!(AgentConfig::default().max_live_subagents, 8);
}

/// The key is on the wire under the name the docs and config.toml.example use.
#[test]
fn the_key_parses_from_the_agent_table() {
    let config: AgentConfig =
        toml::from_str("max_live_subagents = 3").expect("max_live_subagents must parse");
    assert_eq!(config.max_live_subagents, 3);

    // Absent → the default, so an old config keeps working unchanged.
    let absent: AgentConfig = toml::from_str("max_concurrent = 2").expect("must parse");
    assert_eq!(absent.max_live_subagents, 8);
}

// ─── The refusal message ───────────────────────────────────────────────────

/// The three things the caller needs to act: the cap, the count that hit it,
/// and the verbs that free a slot. A refusal that omits the count reads as a
/// bug to the model, which then retries the same call.
#[test]
fn the_refusal_names_the_cap_the_count_and_both_remedies() {
    let message = live_cap_refusal(8, 8);

    assert!(
        message.contains("cap of 8"),
        "the cap must be named:\n{message}"
    );
    assert!(
        message.contains("8 live sub-agent(s)"),
        "the CURRENT live count must be named:\n{message}"
    );
    assert!(
        message.contains("wait_agent"),
        "the first remedy must be named:\n{message}"
    );
    assert!(
        message.contains("stop_agent"),
        "the second remedy must be named:\n{message}"
    );
    assert!(
        message.contains("No child was created"),
        "a refusal must promise it cost nothing:\n{message}"
    );
}

// ─── The boundary ──────────────────────────────────────────────────────────

/// `cap - 1` live children still leaves room for one more. An off-by-one here
/// would refuse a legitimate fan-out one child early.
#[test]
fn at_cap_minus_one_a_spawn_is_admitted() {
    let parent = Uuid::new_v4();
    let manager = manager_with(
        parent,
        &[
            (SubAgentState::Running, "a1"),
            (SubAgentState::Running, "a2"),
        ],
    );

    assert_eq!(manager.live_count_for(parent), 2);
    assert!(
        manager.try_admit_child(parent, 3).is_ok(),
        "2 live under a cap of 3 must be admitted"
    );
}

/// At the cap the spawn is refused, and the refusal carries the occupancy that
/// caused it so the message can name the real count.
#[test]
fn at_cap_a_spawn_is_refused() {
    let parent = Uuid::new_v4();
    let manager = manager_with(
        parent,
        &[
            (SubAgentState::Running, "a1"),
            (SubAgentState::Running, "a2"),
            (SubAgentState::AwaitingInput, "a3"),
        ],
    );

    assert_eq!(
        manager.live_count_for(parent),
        3,
        "AwaitingInput is parked, not finished — it holds a slot"
    );
    assert_eq!(
        manager.try_admit_child(parent, 3).err(),
        Some(3),
        "a fourth child under a cap of 3 must be refused, and the refusal must \
         report the occupancy that caused it"
    );
}

// ─── Terminal children free their slot ────────────────────────────────────

/// Every terminal state must release its slot, or a parent that collects its
/// children would be locked out by its own history. `Failed` carries a reason,
/// so it is constructed rather than named.
#[test]
fn terminal_children_do_not_count_toward_the_cap() {
    let parent = Uuid::new_v4();
    let manager = manager_with(
        parent,
        &[
            (SubAgentState::Completed, "done"),
            (SubAgentState::Failed("boom".to_string()), "failed"),
            (SubAgentState::Cancelled, "stopped"),
        ],
    );

    assert_eq!(
        manager.live_count_for(parent),
        0,
        "Completed/Failed/Cancelled hold no slot"
    );
    assert!(
        manager.try_admit_child(parent, 1).is_ok(),
        "a cap of 1 is still free when every child is terminal"
    );
}

/// The transition is what frees the slot, not the passage of time: a running
/// child blocks admission, and the same child completing unblocks it.
#[test]
fn a_child_completing_frees_its_slot() {
    let parent = Uuid::new_v4();
    let manager = manager_with(parent, &[(SubAgentState::Running, "only")]);

    assert!(
        manager.try_admit_child(parent, 1).is_err(),
        "one live child at a cap of 1 leaves no room"
    );
    manager.mark_completed("only", "done".to_string());
    assert!(
        manager.try_admit_child(parent, 1).is_ok(),
        "the completed child must free the slot immediately"
    );
}

// ─── Scoping ───────────────────────────────────────────────────────────────

/// The manager is process-global, so another session's children must not
/// consume this parent's slots — those are children this parent cannot collect
/// (`wait_agent` / `stop_agent` refuse an id whose parent differs).
#[test]
fn another_sessions_children_do_not_consume_this_cap() {
    let mine = Uuid::new_v4();
    let theirs = Uuid::new_v4();
    let manager = manager_with(theirs, &[(SubAgentState::Running, "theirs-1")]);

    assert_eq!(manager.live_count_for(mine), 0);
    assert!(
        manager.try_admit_child(mine, 1).is_ok(),
        "another chat's live child must not hold this parent's slot"
    );
}

// ─── Reservations ─────────────────────────────────────────────────────────

/// A spawn reserves its slot before the child is registered, and a parallel
/// tool batch can run several spawns at once. Without the reservation each
/// would read the same live count and all admit, so the cap would leak by up to
/// `max_concurrent - 1`.
#[test]
fn concurrent_admissions_reserve_slots_against_each_other() {
    let parent = Uuid::new_v4();
    let manager = SubAgentManager::new();

    let first = manager
        .try_admit_child(parent, 2)
        .expect("first slot is free");
    let second = manager
        .try_admit_child(parent, 2)
        .expect("second slot is free");
    assert_eq!(
        manager.try_admit_child(parent, 2).err(),
        Some(2),
        "two unregistered reservations already fill a cap of 2"
    );

    // Releasing one reservation (an early return in the spawn path) frees it.
    drop(second);
    assert!(
        manager.try_admit_child(parent, 2).is_ok(),
        "a released reservation must free the slot"
    );
    drop(first);
}

/// A reservation must never outlive the spawn that made it, even on an error
/// path: `Drop` is the only release, so a leaked guard would ratchet the cap
/// down for the life of the process.
#[test]
fn dropping_every_guard_restores_the_full_cap() {
    let parent = Uuid::new_v4();
    let manager = SubAgentManager::new();

    {
        let _guards: Vec<_> = (0..3)
            .map(|_| manager.try_admit_child(parent, 3).expect("slot free"))
            .collect();
    }

    assert_eq!(manager.live_count_for(parent), 0);
    for _ in 0..3 {
        assert!(
            manager.try_admit_child(parent, 3).is_ok(),
            "the cap must be fully available again"
        );
    }
}

// ─── Discoverability ──────────────────────────────────────────────────────

/// The model only ever sees the tool schema, so a cap it cannot read about is
/// a refusal it cannot explain or avoid. The key is named in the description
/// (the schema itself is per-call input, and the cap is a config key, not a
/// per-call parameter).
#[test]
fn the_spawn_description_names_the_cap_key() {
    use crate::brain::tools::Tool;
    use crate::brain::tools::subagent::SpawnAgentTool;
    use std::sync::Arc;

    let tool = SpawnAgentTool::new(
        Arc::new(SubAgentManager::new()),
        Arc::new(crate::brain::tools::ToolRegistry::new()),
    );
    let description = tool.description();
    assert!(
        description.contains("max_live_subagents"),
        "spawn_agent must name the cap key, or the refusal reads as a bug:\n{description}"
    );

    let schema = tool.input_schema();
    let required: Vec<&str> = schema
        .get("required")
        .and_then(|v| v.as_array())
        .expect("schema must have required")
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    assert_eq!(
        required,
        vec!["prompt"],
        "the cap must not add a required spawn parameter"
    );
}

// ─── Documentation ────────────────────────────────────────────────────────

/// #1668's rule applied to this key: a live config knob with no README entry
/// is a knob users cannot find, and the whole point of the cap is that a
/// caller who hits it can read why. Bundled at compile time so a doc tidy-up
/// that drops the line fails here instead of in the field.
#[test]
fn the_readme_documents_the_cap_key() {
    const README: &str = include_str!("../../README.md");
    assert!(
        README.contains("max_live_subagents"),
        "README must document `[agent] max_live_subagents`, or the refusal names \
         a key the user cannot look up"
    );
    assert!(
        README.contains("wait_agent") && README.contains("stop_agent"),
        "the README entry must name both remedies, since that is what the \
         refusal tells the caller to do"
    );
}

/// The annotated example ships the key too, commented at its default, so a
/// fresh install discovers it while reading the file it is about to edit.
#[test]
fn the_example_config_carries_the_key() {
    const EXAMPLE: &str = include_str!("../../config.toml.example");
    assert!(
        EXAMPLE.contains("# max_live_subagents = 8"),
        "config.toml.example must show the cap at its default, commented out"
    );
}
