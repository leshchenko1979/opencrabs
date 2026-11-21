//! Leg 3 of #852 — resolving an agent that outlived the process.
//!
//! Before this, `SubAgentManager.agents` was the ONLY record of an agent, and
//! it is an in-memory map. A restart therefore made every agent unreachable
//! even though its status file was on disk the whole time: `resume_agent`
//! answered "No sub-agent found" for an agent whose session, transcript and
//! grant were all still there.
//!
//! The dangerous half is the GRANT. `resume_agent` rebuilds the child registry
//! from `read_only` (#1173), so a rehydration that restored a read-only child
//! as writable would be a silent privilege escalation — the exact bug class
//! #1173 fixed for the in-process case. These tests pin the grant to disk.

use crate::brain::agent::service::work_status::test_override;
use crate::brain::agent::service::work_status::{AgentGrant, WorkStatus};
use crate::brain::tools::subagent::{SubAgent, SubAgentManager, SubAgentState};
use std::fs;
use std::sync::Arc;
use uuid::Uuid;

fn isolate(tag: &str) {
    let dir = std::env::temp_dir().join(format!(
        "opencrabs-852-resolve-{}-{}",
        tag,
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    test_override::set(dir.join("detached"));
}

/// Write a status record for an agent that has no live entry in the map —
/// i.e. exactly what a restart leaves behind.
fn on_disk(id: &str, parent: Uuid, child: Uuid, grant: Option<AgentGrant>) -> WorkStatus {
    WorkStatus::new_agent(
        id,
        "worker",
        &child.to_string(),
        "do the thing",
        Some(&parent.to_string()),
        grant.as_ref(),
    )
    .expect("write status")
}

/// Set a record's state the way the real writers do.
///
/// `WorkStatus::write` is private, so the test cannot mutate the handle it
/// built. `mark_interrupted` is the production path for the restart case; for
/// `Completed` the file is patched in place, which is also how a record written
/// by an older build looks on disk.
fn set_state(id: &str, state: &str) {
    let path = crate::brain::agent::service::work_status::status_path(id);
    let data = fs::read_to_string(&path).expect("status file");
    let mut value: serde_json::Value = serde_json::from_str(&data).expect("status json");
    value["state"] = serde_json::json!(state);
    fs::write(&path, serde_json::to_string_pretty(&value).expect("encode"))
        .expect("rewrite status");
}

// ---------------------------------------------------------------------------
// The happy path
// ---------------------------------------------------------------------------

#[test]
fn a_live_agent_resolves_from_the_map() {
    isolate("live");
    let manager = SubAgentManager::new();
    let child = Uuid::new_v4();
    let parent = Uuid::new_v4();
    manager.insert(SubAgent::new("live-1", "worker", child, parent));

    let view = manager
        .resolve("live-1", parent)
        .expect("a live agent must resolve");
    assert_eq!(view.id, "live-1");
    assert_eq!(view.session_id, child);
    assert_eq!(view.state, SubAgentState::Running);
}

#[test]
fn an_agent_that_outlived_the_process_resolves_from_disk() {
    isolate("rehydrate");
    let child = Uuid::new_v4();
    let parent = Uuid::new_v4();
    on_disk(
        "gone-1",
        parent,
        child,
        Some(AgentGrant::new(false, false, true)),
    );

    // A fresh manager: this is the post-restart state, where nothing is in
    // the map but the status file is still there.
    let manager = SubAgentManager::new();
    assert!(
        manager.get_state("gone-1").is_none(),
        "precondition: the map must NOT hold the agent"
    );

    let view = manager
        .resolve("gone-1", parent)
        .expect("an agent with a status file must resolve");
    assert_eq!(
        view.session_id, child,
        "the recorded session is the identity"
    );
    assert_eq!(
        manager.get_session_id("gone-1"),
        Some(child),
        "resolving must re-insert the agent, or the caller still cannot use it"
    );
}

// ---------------------------------------------------------------------------
// The grant — the reason this feature is not a one-liner
// ---------------------------------------------------------------------------

#[test]
fn a_read_only_agent_is_rehydrated_read_only() {
    isolate("grant_preserved");
    let child = Uuid::new_v4();
    let parent = Uuid::new_v4();
    on_disk(
        "ro-1",
        parent,
        child,
        Some(AgentGrant::new(true, true, false)),
    );

    let manager = SubAgentManager::new();
    manager.resolve("ro-1", parent).expect("resolve");

    // These three are what `resume_agent` reads to rebuild the child registry.
    // If any is lost, the resumed child gets permissions its spawn never had.
    assert_eq!(
        manager.get_read_only("ro-1"),
        Some(true),
        "a read-only child restored writable is a silent privilege escalation (#1173)"
    );
    assert_eq!(
        manager.get_include_brain("ro-1"),
        Some(true),
        "brain injection is part of the frozen grant (#145)"
    );
    assert!(
        !manager.nesting_allowed_for_session(child),
        "a child spawned without nesting rights must not regain them on resume"
    );
}

#[test]
fn a_legacy_record_is_rehydrated_under_the_pre_852_grant() {
    isolate("legacy_grant");
    let child = Uuid::new_v4();
    let parent = Uuid::new_v4();
    // No grant recorded: the file predates the field.
    on_disk("legacy-1", parent, child, None);

    let manager = SubAgentManager::new();
    manager.resolve("legacy-1", parent).expect("resolve");

    assert_eq!(
        manager.get_read_only("legacy-1"),
        Some(false),
        "a legacy record was not read-only"
    );
    assert!(
        manager.nesting_allowed_for_session(child),
        "a legacy record could nest — assuming otherwise would strip a \
         permission the agent actually had"
    );
}

// ---------------------------------------------------------------------------
// What must NOT resolve
// ---------------------------------------------------------------------------

#[test]
fn another_sessions_agent_does_not_resolve() {
    isolate("foreign");
    let child = Uuid::new_v4();
    let parent = Uuid::new_v4();
    on_disk("theirs-1", parent, child, Some(AgentGrant::legacy()));

    let manager = SubAgentManager::new();
    // A different parent asking for an id it does not own. The map is
    // process-global, so without this check a rehydrated agent would become
    // another chat's agent (#191).
    assert!(
        manager.resolve("theirs-1", Uuid::new_v4()).is_none(),
        "a stranger must not gain authority over someone else's agent"
    );
    assert!(
        manager.get_state("theirs-1").is_none(),
        "a refused resolve must not insert the agent either"
    );
}

#[test]
fn a_legacy_record_with_no_parent_is_refused_rather_than_adopted() {
    isolate("no_parent");
    let child = Uuid::new_v4();
    WorkStatus::new_agent(
        "orphan-1",
        "worker",
        &child.to_string(),
        "do the thing",
        None, // no recorded parent
        None,
    )
    .expect("write status");

    let manager = SubAgentManager::new();
    assert!(
        manager.resolve("orphan-1", Uuid::new_v4()).is_none(),
        "with no recorded parent there is no way to prove ownership, so the \
         agent is refused — adopting it would hand a caller an agent it never spawned"
    );
}

#[test]
fn a_detached_command_never_rehydrates_as_an_agent() {
    isolate("command");
    // Commands share the status directory and carry a session id, so a naive
    // reader would happily resurrect one as an agent with no grant.
    WorkStatus::new_command(
        "cmd-1",
        &Uuid::new_v4().to_string(),
        "cargo test",
        "cargo test --all-features",
        crate::brain::agent::service::work_status::CommandPaths::none(),
    )
    .expect("write status");

    let manager = SubAgentManager::new();
    assert!(
        manager.resolve("cmd-1", Uuid::new_v4()).is_none(),
        "a command is not an agent: it has no grant, no parent session and no transcript"
    );
}

// ---------------------------------------------------------------------------
// State mapping — a dead agent must never read as alive
// ---------------------------------------------------------------------------

#[test]
fn an_interrupted_agent_rehydrates_as_resumable_not_running() {
    isolate("interrupted");
    let child = Uuid::new_v4();
    let parent = Uuid::new_v4();
    let mut status = on_disk("int-1", parent, child, Some(AgentGrant::legacy()));
    status.mark_interrupted().expect("mark interrupted");

    let manager = SubAgentManager::new();
    let view = manager.resolve("int-1", parent).expect("resolve");

    // `Running` would be a lie with teeth: `alive_counts_for` and every
    // liveness read would report a dead agent as working. `Failed` is what
    // `prepare_resume` accepts, so the agent stays resumable.
    match &view.state {
        SubAgentState::Failed(why) => assert!(
            why.contains("restart"),
            "the reason must say the restart killed it, got: {why}"
        ),
        other => panic!("an interrupted agent must rehydrate as Failed, got {other:?}"),
    }
    assert_eq!(manager.get_state("int-1"), Some(view.state.clone()));
}

#[test]
fn a_completed_agent_rehydrates_completed() {
    isolate("completed");
    let child = Uuid::new_v4();
    let parent = Uuid::new_v4();
    on_disk("done-1", parent, child, Some(AgentGrant::legacy()));
    set_state("done-1", "Completed");

    let manager = SubAgentManager::new();
    let view = manager.resolve("done-1", parent).expect("resolve");
    assert_eq!(
        view.state,
        SubAgentState::Completed,
        "a finished agent must not be reported as failed — resume behaves the \
         same for both, but the parent's report does not"
    );
}

/// The record nobody cleaned up: the process died with the agent still marked
/// `Running` (or `Pending`), and `reconcile_orphaned_agents` never ran.
///
/// Rehydrating that as `Running` would be the worst of both worlds — `resume`
/// refuses a running agent ("use wait_agent first") and nothing is running it,
/// so the agent becomes permanently unreachable with a message that tells the
/// parent to wait for work that will never finish. It rehydrates as `Failed`,
/// which the resume gate accepts.
#[test]
fn a_stale_non_terminal_record_never_rehydrates_as_running() {
    isolate("stale_running");
    let parent = Uuid::new_v4();

    for (id, state) in [
        ("stale-running-1", "Running"),
        ("stale-pending-1", "Pending"),
    ] {
        let child = Uuid::new_v4();
        on_disk(id, parent, child, Some(AgentGrant::legacy()));
        set_state(id, state);

        let manager = SubAgentManager::new();
        let view = manager
            .resolve(id, parent)
            .unwrap_or_else(|| panic!("{state} record must still resolve"));

        assert!(
            !matches!(view.state, SubAgentState::Running),
            "a {state} record has nothing executing it; reporting Running would \
             make resume refuse it as busy forever"
        );
        match &view.state {
            SubAgentState::Failed(why) => assert!(
                why.contains("stale"),
                "the reason must distinguish a stale record from a clean \
                 restart interrupt, got: {why}"
            ),
            other => panic!("a {state} record must rehydrate as Failed, got {other:?}"),
        }
        assert_eq!(
            manager.get_state(id),
            Some(view.state.clone()),
            "the map must hold the same verdict the view reported"
        );
    }
}

#[test]
fn resolving_twice_keeps_one_entry_and_the_original_spawn_time() {
    isolate("idempotent");
    let child = Uuid::new_v4();
    let parent = Uuid::new_v4();
    let status = on_disk("twice-1", parent, child, Some(AgentGrant::legacy()));

    let manager = Arc::new(SubAgentManager::new());
    let first = manager.resolve("twice-1", parent).expect("first resolve");
    let second = manager.resolve("twice-1", parent).expect("second resolve");

    assert_eq!(
        first.spawned_at, second.spawned_at,
        "the spawn time comes from the record, not from the moment of rehydration"
    );
    // Compared as INSTANTS, not as strings: the field carries the moment an
    // agent started, and two renderings of one instant are still one instant.
    // Pinning the string would fail on a format tidy-up while the thing being
    // guarded — that a rehydration reports when the agent ACTUALLY started
    // rather than `Utc::now()` — stayed intact. The truncation that used to
    // make this comparison fail is gone: the view now keeps the record's full
    // precision, because a truncated instant is a different instant.
    let view_at = chrono::DateTime::parse_from_rfc3339(&second.spawned_at)
        .expect("the view's spawn time is RFC3339");
    let record_at = chrono::DateTime::parse_from_rfc3339(&status.spawned_at)
        .expect("the record's spawn time is RFC3339");
    assert_eq!(
        view_at, record_at,
        "a rehydrated agent keeps the time it actually started"
    );
    assert_eq!(
        manager.resolve("twice-1", parent).map(|v| v.session_id),
        Some(child),
        "a second resolve must be a no-op, not a replacement"
    );
}
