//! Leg for #921 — one vocabulary across every surface that teaches a session
//! how to talk to a sub-agent.
//!
//! The defect was not one wrong string, it was six surfaces disagreeing. Two of
//! them taught the exact anti-pattern the rest of the system forbids:
//! `agent_progress` said "For the final result use wait_agent" (rule 26 and
//! `wait_agent`'s own description say the opposite), and `report_progress` said
//! "wait to be resumed" when #859 made the park reachable by `send_input`.
//!
//! The behavioural half is `stop_agent`: it reported "cancelled" for an agent
//! that had already finished, claiming an action that never happened. That is
//! testable, so it is tested rather than pinned as source.
//!
//! Each test names the mutant it kills.

use crate::brain::tools::Tool;
use crate::brain::tools::ToolExecutionContext;
use crate::brain::tools::subagent::{StopAgentTool, SubAgent, SubAgentManager, SubAgentState};
use std::sync::Arc;
use uuid::Uuid;

fn source(rel: &str) -> String {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(rel);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// Kills the mutant that drops the verbs from the spawn receipt.
///
/// The description teaches `send_input` / `agent_progress` / `report_progress`,
/// but a model reads the description BEFORE the call and the receipt AFTER it —
/// so a receipt without the verbs is silent at the one moment the agent id
/// first exists.
#[test]
fn the_spawn_receipt_names_the_verbs_beside_the_id() {
    let src = source("src/brain/tools/subagent/spawn.rs");

    for needed in [
        "Talk to it:",
        "`send_input {}`",
        "`agent_progress {}`",
        "do not `wait_agent` for it",
    ] {
        assert!(
            src.contains(needed),
            "spawn receipt must name {needed:?} beside the id (#921)"
        );
    }
}

/// Kills the mutant that reorders the receipt so the parseable ack is no longer
/// first: `plan_card` extracts the child id by splitting on `with id: ` and
/// taking the next token (#155), and that only works while the ack leads.
#[test]
fn the_spawn_receipt_still_opens_with_the_parseable_ack() {
    let src = source("src/brain/tools/subagent/spawn.rs");
    assert!(
        src.contains(r#""Spawned sub-agent '{}' with id: {}\nSession: {}"#),
        "the receipt must still open with the ack `plan_card` parses (#155, #921)"
    );

    // And the parser tolerates the extra line: a real string in the shipped
    // shape must still yield the id, not the appended prose.
    let receipt = "Spawned sub-agent 'plan-review' with id: abc-123\n\
                   Session: 11111111-2222-3333-4444-555555555555\n\
                   Talk to it: `send_input abc-123` steers it.\n\
                   Prompt: review the card";
    assert_eq!(
        crate::channels::telegram::plan_card::plan_review_agent_id(receipt).as_deref(),
        Some("abc-123"),
        "appending the verbs must not break child-id extraction"
    );
}

/// Kills the mutant that leaves the child believing `report_progress` is only
/// an escape hatch. Routine `note` progress was taught nowhere, which is why the
/// tool had 8 calls all-time and essentially all of them blockers.
#[test]
fn the_child_is_told_to_record_routine_progress_too() {
    let channel = crate::cli::tool_setup::SUBAGENT_ANSWER_CHANNEL;

    assert!(
        channel.contains("note"),
        "the child's brief must name routine `note` progress (#921)"
    );
    assert!(
        channel.contains("agent_progress"),
        "the child must know the parent reads it with agent_progress (#921)"
    );
    assert!(
        channel.contains("END YOUR TURN"),
        "the parking instruction must survive the rewrite (#859)"
    );
}

/// Kills the mutant that reverts the answer tool to `resume_agent`: the parent
/// answers a parked child with `send_input`, which resumes it in the SAME
/// context. `resume_agent` is for an agent that already finished.
#[test]
fn the_child_is_told_the_parent_answers_with_send_input() {
    let channel = crate::cli::tool_setup::SUBAGENT_ANSWER_CHANNEL;

    assert!(
        channel.contains("send_input"),
        "the parked child must be told the answer arrives as send_input (#921)"
    );
    assert!(
        !channel.contains("wait to be resumed"),
        "`wait to be resumed` names the wrong reply tool (#921)"
    );
}

/// Kills the mutant that reinstates the wait_agent contradiction on the read
/// path — the one string that directly opposed rule 26 and `wait_agent`'s own
/// description.
#[test]
fn agent_progress_does_not_send_the_reader_to_wait_agent() {
    let src = source("src/brain/tools/subagent/progress.rs");

    assert!(
        !src.contains("For the final result use wait_agent"),
        "agent_progress must not prescribe the blocking wait (#921, rule 26)"
    );
    assert!(
        src.contains("reaches you as a fresh turn"),
        "it must state the real read-back: completion arrives as a new turn (#921)"
    );
}

/// Kills the mutant that leaves `send_input` described as "running sub-agent"
/// only — a parked child is the intended target, and the description is what
/// tells the parent so.
#[test]
fn send_input_describes_the_parked_case() {
    let src = source("src/brain/tools/subagent/send_input.rs");

    assert!(
        src.contains("parked"),
        "send_input must name the parked (AwaitingInput) case (#921)"
    );
    assert!(
        src.contains("resume_agent"),
        "it must still point a finished agent at resume_agent (#921)"
    );
}

/// Behavioural: an agent that already finished must NOT be reported as
/// cancelled. Before #921 this returned "Sub-agent X cancelled." for an agent
/// whose work had completed, which reads to the caller as "I stopped live
/// work".
#[tokio::test]
async fn stop_agent_reports_an_already_finished_agent_honestly() {
    let manager = Arc::new(SubAgentManager::new());
    let mut agent = SubAgent::new("done-1", "worker", Uuid::new_v4(), Uuid::new_v4());
    agent.state = SubAgentState::Completed;
    manager.insert(agent);

    let tool = StopAgentTool::new(manager.clone());
    let result = tool
        .execute(
            serde_json::json!({ "agent_id": "done-1" }),
            &ToolExecutionContext::new(Uuid::new_v4()),
        )
        .await
        .expect("close executes");

    let text = result.output;
    assert!(
        text.contains("already finished"),
        "must say nothing was cancelled, got: {text}"
    );
    assert!(
        !text.contains("cancelled."),
        "must not claim a cancellation that did not happen, got: {text}"
    );
    // Still tracked: no `remove` was asked for.
    assert!(
        manager.exists("done-1"),
        "an already-finished agent must stay tracked without remove:true"
    );
}

/// Behavioural control for the above: a genuinely live agent must still be
/// cancelled, or the honest wording would have been bought by breaking the
/// tool's actual job.
#[tokio::test]
async fn stop_agent_still_cancels_a_running_agent() {
    let manager = Arc::new(SubAgentManager::new());
    let agent = SubAgent::new("live-1", "worker", Uuid::new_v4(), Uuid::new_v4());
    let token = agent.cancel_token.clone();
    manager.insert(agent);

    let tool = StopAgentTool::new(manager.clone());
    let result = tool
        .execute(
            serde_json::json!({ "agent_id": "live-1" }),
            &ToolExecutionContext::new(Uuid::new_v4()),
        )
        .await
        .expect("close executes");

    let text = result.output;
    assert!(
        text.contains("cancelled"),
        "a running agent must be reported cancelled, got: {text}"
    );
    assert!(
        !text.contains("already finished"),
        "a running agent is not already finished, got: {text}"
    );
    assert!(token.is_cancelled(), "the cancel token must actually fire");
}

/// Behavioural: `remove` is independent of cancellation — dropping the record
/// must work even when there was nothing live to stop.
#[tokio::test]
async fn stop_agent_removes_a_finished_agent_when_asked() {
    let manager = Arc::new(SubAgentManager::new());
    let mut agent = SubAgent::new("done-2", "worker", Uuid::new_v4(), Uuid::new_v4());
    agent.state = SubAgentState::Failed("boom".into());
    manager.insert(agent);

    let tool = StopAgentTool::new(manager.clone());
    let result = tool
        .execute(
            serde_json::json!({ "agent_id": "done-2", "remove": true }),
            &ToolExecutionContext::new(Uuid::new_v4()),
        )
        .await
        .expect("close executes");

    let text = result.output;
    assert!(
        text.contains("removed from tracking"),
        "remove:true must report the removal, got: {text}"
    );
    assert!(
        !manager.exists("done-2"),
        "remove:true must drop the record so the id stops resolving"
    );
}
