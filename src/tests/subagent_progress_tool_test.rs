//! Leg 2 of #852 — the two progress tools, and the scope that separates them.
//!
//! The pair is asymmetric on purpose, and these tests pin the asymmetry:
//!
//! * `report_progress` is a CHILD's tool. A root session has no agent id, so
//!   the call is REFUSED rather than silently writing a ledger nothing reads.
//! * `agent_progress` is a PARENT's tool. It is scoped to the caller's own
//!   children, so one chat can never read another chat's agent (#191).
//!
//! Both are asserted against the real ledger on disk, because the whole point
//! of the feature is that the line survives the turn that wrote it.

use crate::brain::agent::service::work_status::test_override;
use crate::brain::tools::subagent::ledger::{self, LedgerKind};
use crate::brain::tools::subagent::{
    AgentProgressTool, ReportProgressTool, SubAgent, SubAgentManager,
};
use crate::brain::tools::r#trait::{Tool, ToolExecutionContext};
use serde_json::json;
use std::fs;
use std::sync::Arc;
use uuid::Uuid;

/// Per-test status dir. `test_override` is thread-local and the runner reuses
/// threads, so the tag must be unique per test, not per file.
fn isolate(tag: &str) {
    let dir = std::env::temp_dir().join(format!(
        "opencrabs-852-progress-{}-{}",
        tag,
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    test_override::set(dir.join("detached"));
}

/// A manager holding one live child, and the ids needed to talk to it.
fn one_child(id: &str) -> (Arc<SubAgentManager>, Uuid, Uuid) {
    let manager = Arc::new(SubAgentManager::new());
    let child_session = Uuid::new_v4();
    let parent_session = Uuid::new_v4();
    manager.insert(SubAgent::new(id, "worker", child_session, parent_session));
    (manager, child_session, parent_session)
}

// ---------------------------------------------------------------------------
// report_progress — the child's half
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_root_session_is_refused_rather_than_writing_an_orphan_ledger() {
    isolate("root_refused");
    let manager = Arc::new(SubAgentManager::new());
    let tool = ReportProgressTool::new(manager);
    // A session the manager has never seen: this is what a root chat looks
    // like from inside the tool. There is no agent id to write against, and
    // inventing one would produce a file no reader ever opens.
    let ctx = ToolExecutionContext::new(Uuid::new_v4());

    let err = tool
        .execute(json!({"kind": "note", "text": "hello"}), &ctx)
        .await
        .expect_err("a root session must be refused, not silently accepted");
    assert!(
        err.to_string()
            .contains("only available to a sub-agent session"),
        "the refusal must name the actual reason, got: {err}"
    );
}

#[tokio::test]
async fn a_child_append_lands_in_its_own_ledger() {
    isolate("child_append");
    let (manager, child_session, _parent) = one_child("child-append");
    let tool = ReportProgressTool::new(manager);

    let ctx = ToolExecutionContext::new(child_session);
    let result = tool
        .execute(
            json!({"kind": "blocker", "text": "waiting on the migration"}),
            &ctx,
        )
        .await
        .expect("a child session must be able to report");
    assert!(result.success, "the append reported failure: {result:?}");
    assert!(
        result.output.contains("child-append"),
        "the confirmation must name the agent it wrote against, got: {}",
        result.output
    );

    let entries = ledger::read("child-append", 10);
    assert_eq!(entries.len(), 1, "exactly one line was recorded");
    assert_eq!(
        entries[0].kind,
        LedgerKind::Blocker,
        "the kind must survive the round trip — it is what lets a parent \
         filter blockers without parsing prose"
    );
    assert_eq!(entries[0].text, "waiting on the migration");
}

#[tokio::test]
async fn an_unknown_kind_is_rejected_with_the_vocabulary() {
    isolate("bad_kind");
    let (manager, child_session, _parent) = one_child("child-kind");
    let tool = ReportProgressTool::new(manager);
    let ctx = ToolExecutionContext::new(child_session);

    let err = tool
        .execute(json!({"kind": "shrug", "text": "hi"}), &ctx)
        .await
        .expect_err("an unrecognised kind must not be coerced to a default");
    let message = err.to_string();
    assert!(
        message.contains("note") && message.contains("blocker"),
        "the error must list the accepted kinds so the model can retry \
         correctly, got: {message}"
    );
}

// ---------------------------------------------------------------------------
// agent_progress — the parent's half
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_parent_reads_its_childs_history_while_the_child_is_live() {
    isolate("parent_reads");
    let (manager, child_session, parent_session) = one_child("child-read");
    let reporter = ReportProgressTool::new(manager.clone());
    let child_ctx = ToolExecutionContext::new(child_session);

    for (kind, text) in [
        ("note", "started on the parser"),
        ("decision", "chose the streaming API over buffering"),
        ("blocker", "the fixture has no auth header"),
    ] {
        reporter
            .execute(json!({"kind": kind, "text": text}), &child_ctx)
            .await
            .expect("append");
    }

    let reader = AgentProgressTool::new(manager);
    let parent_ctx = ToolExecutionContext::new(parent_session);
    let result = reader
        .execute(json!({"agent_id": "child-read"}), &parent_ctx)
        .await
        .expect("the parent must be able to read its own child");
    assert!(result.success);

    // Header first, then the entries oldest-first: the parent needs the state
    // and the story, and the order is the story.
    let lines: Vec<&str> = result.output.lines().collect();
    assert!(
        lines[0].contains("child-read") && lines[0].contains("running"),
        "the header must name the agent and its live state, got: {}",
        lines[0]
    );
    let note = result.output.find("started on the parser");
    let decision = result.output.find("chose the streaming API");
    let blocker = result.output.find("the fixture has no auth header");
    assert!(
        note < decision && decision < blocker,
        "entries must read oldest first, got:\n{}",
        result.output
    );
}

#[tokio::test]
async fn a_parent_cannot_read_another_sessions_agent() {
    isolate("foreign_read");
    let manager = Arc::new(SubAgentManager::new());
    let other_parent = Uuid::new_v4();
    manager.insert(SubAgent::new(
        "someone-elses",
        "stranger",
        Uuid::new_v4(),
        other_parent,
    ));

    let reader = AgentProgressTool::new(manager);
    // A DIFFERENT parent, asking for an id it does not own. The manager is
    // process-global, so an unscoped read would hand one chat's agent ids to
    // another chat's model (#191).
    let ctx = ToolExecutionContext::new(Uuid::new_v4());
    let result = reader
        .execute(json!({"agent_id": "someone-elses"}), &ctx)
        .await
        .expect("the call itself succeeds; the ANSWER is the refusal");
    assert!(
        !result.success,
        "a foreign agent id must not resolve, got: {}",
        result.output
    );
    assert!(
        result
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("belongs to this session"),
        "the refusal must say whose scope was violated, got: {:?}",
        result.error
    );
}

#[tokio::test]
async fn an_agent_with_no_entries_says_so_instead_of_returning_nothing() {
    isolate("empty_ledger");
    let (manager, _child_session, parent_session) = one_child("quiet-child");
    let reader = AgentProgressTool::new(manager);
    let ctx = ToolExecutionContext::new(parent_session);

    let result = reader
        .execute(json!({"agent_id": "quiet-child"}), &ctx)
        .await
        .expect("an empty ledger is not an error");
    assert!(result.success);
    assert!(
        result.output.contains("no progress recorded yet"),
        "an empty history must be stated, not rendered as a bare header — \
         otherwise the parent cannot tell 'quiet' from 'broken', got:\n{}",
        result.output
    );
}

#[tokio::test]
async fn the_limit_returns_the_most_recent_entries_not_the_first_ones() {
    isolate("limit_tail");
    let (manager, child_session, parent_session) = one_child("chatty-child");
    let reporter = ReportProgressTool::new(manager.clone());
    let child_ctx = ToolExecutionContext::new(child_session);
    for i in 0..5 {
        reporter
            .execute(
                json!({"kind": "note", "text": format!("step {i}")}),
                &child_ctx,
            )
            .await
            .expect("append");
    }

    let reader = AgentProgressTool::new(manager);
    let ctx = ToolExecutionContext::new(parent_session);
    let result = reader
        .execute(json!({"agent_id": "chatty-child", "limit": 2}), &ctx)
        .await
        .expect("read");
    assert!(result.success);
    assert!(
        !result.output.contains("step 2"),
        "a limited read is a TAIL — a parent asking for the last 2 entries \
         wants the newest, got:\n{}",
        result.output
    );
    assert!(
        result.output.contains("step 3") && result.output.contains("step 4"),
        "the last two entries must both be present, got:\n{}",
        result.output
    );
}

/// `limit` arrives from a model, so it can be 0 or a million. Both ends are
/// clamped rather than rejected: a caller asking for 0 entries wanted "just the
/// latest" and a caller asking for a million wanted "all of them", and neither
/// deserves an error for saying so.
#[tokio::test]
async fn the_limit_is_clamped_at_both_ends() {
    isolate("limit_clamp");
    let (manager, child_session, parent_session) = one_child("clamp-child");
    let reporter = ReportProgressTool::new(manager.clone());
    let child_ctx = ToolExecutionContext::new(child_session);
    for i in 0..5 {
        reporter
            .execute(
                json!({"kind": "note", "text": format!("step {i}")}),
                &child_ctx,
            )
            .await
            .expect("append");
    }

    let reader = AgentProgressTool::new(manager);
    let ctx = ToolExecutionContext::new(parent_session);

    // 0 clamps UP to 1. Without the clamp a `take(0)` would render an empty
    // body, which the header alone cannot be told apart from a broken ledger.
    let zero = reader
        .execute(json!({"agent_id": "clamp-child", "limit": 0}), &ctx)
        .await
        .expect("read");
    assert!(zero.success);
    assert!(
        zero.output.contains("step 4") && !zero.output.contains("step 3"),
        "limit 0 must clamp to the single newest entry, got:\n{}",
        zero.output
    );

    // A huge limit clamps DOWN to 200 and simply returns everything there is.
    let huge = reader
        .execute(json!({"agent_id": "clamp-child", "limit": 99999}), &ctx)
        .await
        .expect("read");
    assert!(huge.success);
    assert!(
        huge.output.contains("step 0") && huge.output.contains("step 4"),
        "a limit past the cap must still return the whole ledger, got:\n{}",
        huge.output
    );
}

// ---------------------------------------------------------------------------
// Registry placement — the asymmetry, enforced
// ---------------------------------------------------------------------------

#[test]
fn a_child_registry_carries_report_progress_but_not_the_parent_reader() {
    use crate::brain::tools::ToolRegistry;
    use crate::brain::tools::subagent::build_child_registry;

    // An empty parent: everything the child has, it has by design, not by
    // inheritance. This is the assertion that keeps the two halves from
    // collapsing into one tool registered everywhere.
    let parent = ToolRegistry::new();
    let manager = Arc::new(SubAgentManager::new());
    let child = build_child_registry(&parent, &manager);

    assert!(
        child.has_tool("report_progress"),
        "the child must carry its channel home even when the parent has no \
         tools at all — it is registered explicitly, not copied"
    );
    assert!(
        !child.has_tool("agent_progress"),
        "a child must not read progress: it would only ever see its own \
         ledger, and a tool that can only fail is schema noise on every request"
    );
}

#[test]
fn a_read_only_child_keeps_report_progress() {
    use crate::brain::tools::ToolRegistry;
    use crate::brain::tools::subagent::build_child_registry;

    let parent = ToolRegistry::new();
    let manager = Arc::new(SubAgentManager::new());
    let child = build_child_registry(&parent, &manager);
    // The read-only grant strips every tool whose hints say it mutates. The
    // reporting tool must declare itself read-only or an explore-style child
    // loses the one way it can tell its parent what it found (#1173).
    crate::brain::tools::plan_gate::restrict_registry_to_read_only(&child);

    assert!(
        child.has_tool("report_progress"),
        "a read-only child is exactly the one that cannot otherwise show its \
         work — stripping this tool would leave it silent"
    );
}

// ---------------------------------------------------------------------------
// #960 — the parent can see the ASK, the TREE, and whether the child is alive
// ---------------------------------------------------------------------------

/// A child that never wrote a ledger line must still read as ALIVE.
///
/// The bug this pins: `no progress recorded yet` was printed for every child
/// whose ledger was empty, and measured on this box that was the majority —
/// 60 of 103 agents wrote exactly one entry and went silent. The progress
/// snapshot was already on disk, written by the child's own loop; only the
/// reader was blind, so the parent re-spawned work that was running fine.
#[tokio::test]
async fn an_empty_ledger_reads_the_progress_snapshot_instead_of_going_blank() {
    use crate::brain::agent::service::work_status::WorkStatus;

    isolate("snapshot_fallback");
    let (manager, _child_session, parent_session) = one_child("child-snapshot");

    // A live child with a status file and a snapshot, but no ledger at all.
    let mut status = WorkStatus::new_agent(
        "child-snapshot",
        "worker",
        "sess",
        "do the thing",
        None,
        None,
    )
    .expect("status");
    status
        .update_progress_tool_count(7, 23, Some("bash".to_string()), Some("running".to_string()))
        .expect("snapshot");

    let reader = AgentProgressTool::new(manager);
    let ctx = ToolExecutionContext::new(parent_session);
    let out = reader
        .execute(json!({"agent_id": "child-snapshot"}), &ctx)
        .await
        .expect("read")
        .output;

    assert!(
        !out.contains("no progress recorded yet"),
        "an empty ledger is not an empty child, got:\n{out}"
    );
    assert!(
        out.contains("iteration 7"),
        "iteration survives, got:\n{out}"
    );
    assert!(
        out.contains("23 tool call"),
        "tool count survives, got:\n{out}"
    );
    assert!(out.contains("bash"), "the last tool is named, got:\n{out}");
}

/// A record that genuinely holds nothing keeps the old bare string, so the
/// fallback does not invent liveness it never observed.
#[tokio::test]
async fn a_child_with_no_ledger_and_no_snapshot_still_says_so() {
    use crate::brain::agent::service::work_status::WorkStatus;

    isolate("no_snapshot");
    let (manager, _child_session, parent_session) = one_child("child-bare");
    WorkStatus::new_agent("child-bare", "worker", "sess", "do the thing", None, None)
        .expect("status");

    let reader = AgentProgressTool::new(manager);
    let ctx = ToolExecutionContext::new(parent_session);
    let out = reader
        .execute(json!({"agent_id": "child-bare"}), &ctx)
        .await
        .expect("read")
        .output;

    assert!(
        out.contains("no progress recorded yet"),
        "nothing recorded must say nothing recorded, got:\n{out}"
    );
}

/// A child that has BOTH a ledger entry AND a live snapshot must show BOTH
/// (#1066).
///
/// The bug this pins: `render_snapshot` had exactly one call site, inside the
/// empty-ledger branch, so every child that wrote even a single progress line
/// lost its liveness line. Measured on this box: 144 children, 286 entries, 73
/// (51%) wrote exactly one line and then read as if they had stopped existing —
/// the population #960 was written about, getting nothing.
///
/// The snapshot is not a fallback for a MISSING ledger; it is the other half of
/// the answer. The entries say what the child HAS done, the snapshot says
/// whether it is still doing anything, and a parent choosing between
/// `send_input` and `resume_agent` needs both.
///
/// Against the pre-#1066 code this fails on `iteration 7` being absent from the
/// output — the entry renders, the liveness line never does. It is not a
/// compile-error test: the code under it compiles either way.
#[tokio::test]
async fn a_child_with_a_ledger_entry_and_a_snapshot_shows_both() {
    use crate::brain::agent::service::work_status::WorkStatus;

    isolate("ledger_and_snapshot");
    let (manager, child_session, parent_session) = one_child("child-both");

    // One ledger line: the child has said WHAT it did.
    ReportProgressTool::new(manager.clone())
        .execute(
            json!({"kind": "note", "text": "parsed the settlement batch"}),
            &ToolExecutionContext::new(child_session),
        )
        .await
        .expect("append");

    // And a live snapshot: the child is still moving.
    let mut status =
        WorkStatus::new_agent("child-both", "worker", "sess", "do the thing", None, None)
            .expect("status");
    status
        .update_progress_tool_count(7, 23, Some("bash".to_string()), Some("running".to_string()))
        .expect("snapshot");

    let reader = AgentProgressTool::new(manager);
    let ctx = ToolExecutionContext::new(parent_session);
    let out = reader
        .execute(json!({"agent_id": "child-both"}), &ctx)
        .await
        .expect("read")
        .output;

    assert!(
        out.contains("parsed the settlement batch"),
        "the ledger entry is the WHAT and must survive, got:\n{out}"
    );
    assert!(
        out.contains("iteration 7"),
        "the liveness line is the WHETHER-IT-IS-STILL-MOVING and must survive \
         a non-empty ledger (#1066), got:\n{out}"
    );
    assert!(
        out.contains("23 tool call"),
        "the tool count is part of the liveness line, got:\n{out}"
    );
    // The trap guard. The snapshot text used to hardcode `no ledger entries; `
    // because its only caller was the empty-ledger branch. Reusing that
    // formatter here would print the sentence directly under real entries —
    // a claim the parent can check and find false.
    assert!(
        !out.contains("no ledger entries"),
        "the snapshot must not claim the ledger is empty when it just printed \
         an entry (#1066), got:\n{out}"
    );
    // A distinct TRAILING line, not text glued onto the last entry: the parent
    // reads entry lines as history, and a clock column that starts with
    // `last activity` would parse as a corrupted record.
    let liveness = out
        .lines()
        .find(|l| l.contains("iteration 7"))
        .expect("a liveness line");
    assert!(
        liveness.starts_with("last activity "),
        "the snapshot is its own line, not an entry suffix, got: {liveness:?}"
    );
    assert!(
        out.lines().last() == Some(liveness),
        "the snapshot is the TRAILING line, got:\n{out}"
    );
}

/// The brief the child was given is printed, so a parent that lost its own
/// context to compaction can check a report against the ask.
#[tokio::test]
async fn the_brief_is_visible_to_the_parent() {
    isolate("task_preview");
    let manager = Arc::new(SubAgentManager::new());
    let child_session = Uuid::new_v4();
    let parent_session = Uuid::new_v4();
    manager.insert(SubAgent {
        task: "review the settlement path for a missing notary signature".to_string(),
        ..SubAgent::new("child-task", "worker", child_session, parent_session)
    });

    let reader = AgentProgressTool::new(manager);
    let ctx = ToolExecutionContext::new(parent_session);
    let out = reader
        .execute(json!({"agent_id": "child-task"}), &ctx)
        .await
        .expect("read")
        .output;

    assert!(
        out.contains("asked: review the settlement path"),
        "the ask must be readable, got:\n{out}"
    );
}

/// A long brief is bounded and flattened: the parent wrote it, so the header
/// needs enough to recognise the ask, not a re-read of the whole prompt.
#[tokio::test]
async fn a_long_brief_is_flattened_and_bounded() {
    isolate("task_long");
    let manager = Arc::new(SubAgentManager::new());
    let child_session = Uuid::new_v4();
    let parent_session = Uuid::new_v4();
    let brief = format!("line one\nline two\n{}", "word ".repeat(400));
    manager.insert(SubAgent {
        task: brief,
        ..SubAgent::new("child-long", "worker", child_session, parent_session)
    });

    let reader = AgentProgressTool::new(manager);
    let ctx = ToolExecutionContext::new(parent_session);
    let out = reader
        .execute(json!({"agent_id": "child-long"}), &ctx)
        .await
        .expect("read")
        .output;

    let asked = out
        .lines()
        .find(|l| l.starts_with("asked: "))
        .expect("an asked line");
    assert!(
        !asked.contains('\n') && asked.len() < 500,
        "the header line stays one bounded line, got {} chars",
        asked.len()
    );
    assert!(
        asked.contains("line one line two"),
        "newlines fold to spaces rather than being escaped, got: {asked}"
    );
}

/// The worktree the child was given is shown, so its work in progress can be
/// reviewed without reconstructing the naming scheme from source.
#[tokio::test]
async fn the_worktree_is_visible_to_the_parent() {
    isolate("worktree_line");
    let manager = Arc::new(SubAgentManager::new());
    let child_session = Uuid::new_v4();
    let parent_session = Uuid::new_v4();
    manager.insert(SubAgent {
        worktree_branch: Some("subagent/child-tree".to_string()),
        worktree_path: Some("/tmp/trees/child-tree".to_string()),
        ..SubAgent::new("child-tree", "worker", child_session, parent_session)
    });

    let reader = AgentProgressTool::new(manager);
    let ctx = ToolExecutionContext::new(parent_session);
    let out = reader
        .execute(json!({"agent_id": "child-tree"}), &ctx)
        .await
        .expect("read")
        .output;

    assert!(
        out.contains("worktree: subagent/child-tree at /tmp/trees/child-tree"),
        "branch AND path, so the tree can be opened, got:\n{out}"
    );
}
