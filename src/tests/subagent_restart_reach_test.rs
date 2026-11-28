//! Leg 4 of #852 — the reach chain that makes `resume_agent` work after a restart.
//!
//! Leg 3 proves the agent comes back with its grant. This leg proves the thing
//! the user actually asked for: that the RESUME PATH can get to it. The chain
//! is `resolve` → `get_state` (must be resumable) → `get_session_id` (the
//! transcript to continue) → `prepare_resume` (the map slot the run needs), and
//! every link used to fail at the first step because the map was empty.
//!
//! A resumed child also rebuilds its registry from the grant, so the grant is
//! asserted here a second time: from the resume path's own accessors, not from
//! the resolver's view.

use crate::brain::agent::service::work_status::test_override;
use crate::brain::agent::service::work_status::{AgentGrant, WorkStatus};
use crate::brain::tools::subagent::{SubAgentManager, SubAgentState};
use crate::brain::tools::r#trait::{Tool, ToolExecutionContext};
use serde_json::json;
use std::fs;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

fn isolate(tag: &str) {
    let dir = std::env::temp_dir().join(format!(
        "opencrabs-852-reach-{}-{}",
        tag,
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    test_override::set(dir.join("detached"));
}

/// The state a restart leaves behind: a status file, an empty map.
fn restart_left(id: &str, parent: Uuid, child: Uuid, grant: AgentGrant) -> Arc<SubAgentManager> {
    let mut status = WorkStatus::new_agent(
        id,
        "worker",
        &child.to_string(),
        "the original task",
        Some(&parent.to_string()),
        Some(&grant),
    )
    .expect("write status");
    status.mark_interrupted().expect("mark interrupted");
    Arc::new(SubAgentManager::new())
}

#[test]
fn resume_reaches_an_agent_the_restart_orphaned() {
    isolate("reach");
    let child = Uuid::new_v4();
    let parent = Uuid::new_v4();
    let manager = restart_left(
        "orphan-1",
        parent,
        child,
        AgentGrant::new(true, false, true),
    );

    // Before: this is the exact call `resume_agent` makes first, and it is
    // what used to produce "No sub-agent found with id: orphan-1".
    assert!(
        manager.get_state("orphan-1").is_none(),
        "precondition: a fresh manager knows nothing about the agent"
    );

    // The resume path's new first step.
    let view = manager
        .resolve("orphan-1", parent)
        .expect("the resume path must be able to reach the agent");

    // Link 1: the state gate. `Running`/`AwaitingInput` are refused; every
    // state that means "its turn ended" is resumable, and since #1080 that
    // includes `Interrupted` — which is precisely the state a restart leaves
    // behind, so it is the one this leg most needs to accept.
    let state = manager
        .get_state("orphan-1")
        .expect("the rehydrated agent has a state");
    assert!(
        state.is_resumable(),
        "the rehydrated state must be one the resume gate accepts, got {state:?}"
    );
    assert_eq!(
        state,
        SubAgentState::Interrupted,
        "a restart orphan rehydrates as Interrupted, not as a failure (#1080)"
    );

    // Link 2: the transcript to continue.
    assert_eq!(
        manager.get_session_id("orphan-1"),
        Some(view.session_id),
        "resume continues the child's OWN session, which is why its context \
         survives; a new session id would start from nothing"
    );

    // Link 3: the map slot the run needs.
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    assert!(
        manager.prepare_resume("orphan-1", CancellationToken::new(), tx),
        "prepare_resume must accept the rehydrated agent — this is the step \
         that used to be unreachable"
    );
    assert_eq!(
        manager.get_state("orphan-1"),
        Some(SubAgentState::Running),
        "after prepare_resume the agent is running again"
    );
}

#[test]
fn the_resumed_child_is_rebuilt_from_the_frozen_grant() {
    isolate("frozen_grant");
    let child = Uuid::new_v4();
    let parent = Uuid::new_v4();
    // Read-only AND no nesting: the two flags a widening bug would lose.
    let manager = restart_left(
        "ro-orphan",
        parent,
        child,
        AgentGrant::new(true, false, false),
    );

    manager.resolve("ro-orphan", parent).expect("resolve");

    // `resume_agent` reads exactly these two accessors to rebuild the child
    // registry, so asserting them here is asserting the rebuild.
    assert_eq!(
        manager.get_read_only("ro-orphan"),
        Some(true),
        "a resumed read-only child must stay read-only (#1173)"
    );
    assert!(
        !manager.nesting_allowed_for_session(child),
        "a child spawned without nesting must not be able to spawn after a resume"
    );
}

#[test]
fn a_completed_agent_is_also_reachable_after_a_restart() {
    isolate("completed_reach");
    let child = Uuid::new_v4();
    let parent = Uuid::new_v4();
    let manager = restart_left("done-1", parent, child, AgentGrant::legacy());
    // The restart did not interrupt it: it had already finished. Its answer is
    // in the transcript, so resuming is the natural follow-up ("now do X to
    // what you built") and must work exactly like the interrupted case.
    let path = crate::brain::agent::service::work_status::status_path("done-1");
    let data = fs::read_to_string(&path).expect("status file");
    let mut value: serde_json::Value = serde_json::from_str(&data).expect("status json");
    value["state"] = serde_json::json!("Completed");
    fs::write(&path, serde_json::to_string_pretty(&value).expect("encode")).expect("rewrite");

    let view = manager.resolve("done-1", parent).expect("resolve");
    assert_eq!(view.state, SubAgentState::Completed);
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    assert!(
        manager.prepare_resume("done-1", CancellationToken::new(), tx),
        "a completed agent is resumable"
    );
}

#[test]
fn an_unknown_id_is_still_unknown_after_the_resolver_runs() {
    isolate("unknown");
    let manager = SubAgentManager::new();
    // No file, no map entry. The resolver must not invent one, and the resume
    // path's error message must stay the honest "not found" — a resolver that
    // quietly created a slot would turn a typo into a broken run.
    assert!(
        manager.resolve("does-not-exist", Uuid::new_v4()).is_none(),
        "an id with no record must not resolve"
    );
    assert_eq!(
        manager.get_state("does-not-exist"),
        None,
        "and nothing may be inserted on the way out"
    );
}

#[test]
fn a_progress_ledger_written_before_the_restart_is_readable_after_it() {
    isolate("ledger_survives");
    let child = Uuid::new_v4();
    let parent = Uuid::new_v4();
    // The agent wrote its ledger, then the process died. The ledger is a file
    // beside the status record, so it is still there — and the parent that
    // rehydrates the agent must be able to read the story of what it did
    // before it died, not just that it died.
    crate::brain::tools::subagent::ledger::append(
        "ledger-1",
        &crate::brain::tools::subagent::ledger::LedgerEntry::new(
            crate::brain::tools::subagent::ledger::LedgerKind::Blocker,
            "the fixture has no auth header",
        ),
    );
    let manager = restart_left("ledger-1", parent, child, AgentGrant::legacy());

    manager.resolve("ledger-1", parent).expect("resolve");
    let entries = crate::brain::tools::subagent::ledger::read("ledger-1", 10);
    assert_eq!(
        entries.len(),
        1,
        "the ledger must outlive the process — it is the only account of what \
         the agent did before it was killed"
    );
    assert_eq!(
        entries[0].kind,
        crate::brain::tools::subagent::ledger::LedgerKind::Blocker,
        "and the kind must survive too, or the parent cannot tell a blocker \
         from narration"
    );
}

// ---------------------------------------------------------------------------
// Through the real tool, not just the manager
// ---------------------------------------------------------------------------

/// The chain above is the mechanism; this is the TOOL that uses it.
///
/// The two outcomes are cleanly distinguishable, which is what makes this a
/// real assertion rather than a smoke test: a refused id returns `Ok` with
/// `success: false`, while a reachable one proceeds past the gate and dies on
/// the next thing it needs (`service_context`, absent in a unit test) as an
/// `Err`. A regression that dropped the resolve call would flip the first
/// assertion back to the old "No sub-agent found".
#[tokio::test]
async fn the_resume_tool_reaches_a_rehydrated_agent_and_still_refuses_a_stranger() {
    isolate("tool_gate");
    use crate::brain::tools::ToolRegistry;
    use crate::brain::tools::subagent::ResumeAgentTool;

    let child = Uuid::new_v4();
    let parent = Uuid::new_v4();
    let manager = restart_left("tool-1", parent, child, AgentGrant::legacy());

    let tool = ResumeAgentTool::new(manager, Arc::new(ToolRegistry::new()));

    // (a) a stranger: refused, with the wording that names the scope.
    let stranger = ToolExecutionContext::new(Uuid::new_v4());
    let refused = tool
        .execute(
            json!({"agent_id": "tool-1", "prompt": "continue"}),
            &stranger,
        )
        .await
        .expect("a refusal is an answer, not a transport failure");
    assert!(
        !refused.success,
        "a foreign caller must not resume the agent"
    );
    assert!(
        refused
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("belongs to this session"),
        "the refusal must name the scope, got: {:?}",
        refused.error
    );

    // (b) the real parent: past the gate, and stopped only by the missing
    // service context — i.e. it got far enough to actually resume.
    let owner = ToolExecutionContext::new(parent);
    let err = tool
        .execute(json!({"agent_id": "tool-1", "prompt": "continue"}), &owner)
        .await
        .expect_err("the run cannot start without a service context");
    let message = err.to_string();
    assert!(
        message.contains("service context"),
        "the owner must get PAST the reachability gate and fail later, at the \
         first thing a unit test cannot supply; got: {message}"
    );
}
