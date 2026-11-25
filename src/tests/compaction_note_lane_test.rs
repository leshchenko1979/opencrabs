//! Parent #1000 — note-lane fixes #1017–#1019 (apply shedding, fail-closed gate,
//! live-background harness block).

use std::fs;
use std::sync::Arc;

use uuid::Uuid;

use crate::brain::agent::service::AgentService;
use crate::brain::agent::service::background_tasks::BackgroundTaskManager;
use crate::brain::agent::service::context::{
    CompactionDecorateOpts, CompactionHarnessAttach, continuation_document_complete,
};
use crate::brain::agent::service::request_budget::COMPACTION_SUMMARY_MAX_TOKENS;
use crate::brain::agent::service::work_status::{self, CommandPaths, WorkStatus};
use crate::brain::provider::{ContentBlock, Message, Role};

fn minimal_complete_summary() -> String {
    concat!(
        "**Obligation status: OPEN**\n\n",
        "## 0. IMMEDIATE TASK\nKeep going.\n\n",
        "## 9. Continuation Message\n**Hi.**\n\n",
        "## 1. Chronological Analysis\nShort.\n\n",
        "```context-manifest\n",
        "active_skills: []\n",
        "discard_skills: []\n",
        "required_tools: []\n",
        "```\n",
    )
    .to_string()
}

fn decorate_opts<'a>(
    sid: Uuid,
    harness: CompactionHarnessAttach,
    tail: Option<&'a [Message]>,
) -> CompactionDecorateOpts<'a> {
    CompactionDecorateOpts {
        session_id: sid,
        harness,
        tail_messages: tail,
    }
}

#[test]
fn thin_continuation_documents_fail_the_complete_gate() {
    assert!(!continuation_document_complete(
        "just prose, no obligation line"
    ));
    assert!(!continuation_document_complete(
        "**Obligation status: OPEN**\n\n## 0. x\n\nno fence"
    ));
    assert!(continuation_document_complete(&minimal_complete_summary()));
}

#[tokio::test]
async fn the_gate_rejects_a_thin_document_after_finalize() {
    let sid = Uuid::new_v4();
    let thin = "## 1. Chronological Analysis\nTiny note with no §0 and no manifest.".to_string();
    let opts = decorate_opts(sid, CompactionHarnessAttach::default(), None);
    let (_text, ok) = AgentService::gate_finalized_continuation_document(thin, &opts).await;
    assert!(!ok, "a thin document must not pass the gate (#1018)");
}

#[tokio::test]
async fn live_background_work_block_lists_running_shells_and_pending_tools() {
    let sid = Uuid::new_v4();
    let mgr = Arc::new(BackgroundTaskManager::new());
    mgr.mirror_started(sid, "cargo build --release");

    let tail = vec![
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: "tu_pending".into(),
                name: "bash".into(),
                input: serde_json::json!({ "command": "sleep 60" }),
            }],
        },
        Message::user("waiting on sleep"),
    ];

    let opts = decorate_opts(
        sid,
        CompactionHarnessAttach {
            background: Some(mgr),
            ..CompactionHarnessAttach::default()
        },
        Some(&tail),
    );
    let decorated =
        AgentService::decorate_compaction_summary(minimal_complete_summary(), &opts).await;

    assert!(
        decorated.contains("## Live Background Work"),
        "expected harness block: {decorated}"
    );
    assert!(
        decorated.contains("cargo build --release"),
        "running background shell must be named: {decorated}"
    );
    assert!(
        decorated.contains("tu_pending"),
        "pending tool call must be listed: {decorated}"
    );
}

#[test]
fn live_background_work_block_survives_summary_budget_trim() {
    let mut doc = minimal_complete_summary();
    doc.push_str("\n\n## Live Background Work\n\n");
    doc.push_str("| `mirror:abc` | ship | background shell | running | — |\n");
    doc.push_str(&format!(
        "\n## 1. Chronological Analysis\n{}",
        "Long prose filler. ".repeat(2_000)
    ));

    let trimmed = AgentService::enforce_summary_budget(doc, COMPACTION_SUMMARY_MAX_TOKENS as usize);

    assert!(
        trimmed.contains("## Live Background Work") && trimmed.contains("ship"),
        "must-keep harness block was trimmed away (#1019): {trimmed}"
    );
}

#[tokio::test]
async fn work_status_rows_appear_in_the_live_background_block() {
    let dir = std::env::temp_dir().join(format!(
        "opencrabs-note-lane-{}-{}",
        std::process::id(),
        Uuid::new_v4()
    ));
    let _ = fs::remove_dir_all(&dir);
    work_status::test_override::set(dir.join("detached"));

    let sid = Uuid::new_v4();
    let session = sid.to_string();
    WorkStatus::new_command(
        "cmd-9",
        &session,
        "build",
        "cargo test --all-features",
        CommandPaths::none(),
    )
    .unwrap();

    let opts = decorate_opts(sid, CompactionHarnessAttach::default(), None);
    let block = AgentService::decorate_compaction_summary(minimal_complete_summary(), &opts).await;

    assert!(
        block.contains("cmd-9") && block.contains("detached command"),
        "in-flight work_status command must be listed: {block}"
    );
    work_status::test_override::clear();
}
