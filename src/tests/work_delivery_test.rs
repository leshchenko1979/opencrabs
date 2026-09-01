//! #26 P3: ONE completion path for detached work — both kinds.
//!
//! Pins the unified surface: kind dispatch (origin per kind, #1221 bubble
//! policy), tail size per kind (50 lines vs 4000 chars), the DB kind-string
//! vocabulary, and the shared delivery function's NoRoute honesty.

use crate::brain::agent::service::background_tasks::CmdResult;
use crate::brain::agent::service::work_delivery::{
    WorkKind, WorkPayload, deliver_work_result, work_completion,
};
use uuid::Uuid;

fn command_payload(output: &str) -> WorkPayload {
    WorkPayload::Command {
        label: "cargo test".into(),
        command: "cargo test --all-features".into(),
        result: CmdResult {
            success: true,
            code: 0,
            output: output.into(),
        },
        elapsed_secs: 5.0,
    }
}

#[test]
fn kinds_dispatch_to_their_own_origin_per_the_bubble_policy() {
    // #1221: commands get the echo bubble (BackgroundTask origin + typed
    // receipt payload), agents stay silent (SubAgent origin, no meta).
    let cmd = work_completion(command_payload("ok"));
    assert!(matches!(
        cmd.origin,
        crate::brain::agent::PushOrigin::BackgroundTask
    ));
    assert!(cmd.bg_meta.is_some(), "commands carry the #15 receipt card");
    assert!(cmd.display_text.contains("🔧 background task"));

    let agent = work_completion(WorkPayload::Agent {
        label: "research".into(),
        agent_id: "abc123".into(),
        outcome: Ok("done".into()),
    });
    assert!(matches!(
        agent.origin,
        crate::brain::agent::PushOrigin::SubAgent
    ));
    assert!(agent.bg_meta.is_none(), "agents carry no receipt card");
    assert!(agent.display_text.contains("🤖 sub-agent finished"));
}

#[test]
fn tail_size_is_per_kind_50_lines_vs_4000_chars() {
    // Command: keeps the last 50 LINES, drops earlier ones.
    let many_lines = (1..=100)
        .map(|i| format!("line-{i}"))
        .collect::<Vec<_>>()
        .join("\n");
    let cmd = work_completion(command_payload(&many_lines));
    assert!(cmd.context_text.contains("line-100"));
    assert!(!cmd.context_text.contains("line-1\n"), "head is dropped");

    // Agent: keeps the last 4000 CHARS, marks the truncation.
    let long = std::iter::repeat_n('x', 5000)
        .chain("THE-CONCLUSION".chars())
        .collect::<String>();
    let agent = work_completion(WorkPayload::Agent {
        label: "big".into(),
        agent_id: "ghi789".into(),
        outcome: Ok(long.clone()),
    });
    assert!(agent.context_text.contains("THE-CONCLUSION"));
    assert!(agent.context_text.contains("truncated"));
}

#[test]
fn db_kind_strings_mirror_the_work_kind() {
    // The DB column vocabulary (P2) and the delivery-path kind must never
    // drift apart: recovery reads the row's kind, delivery frames by it.
    assert_eq!(WorkKind::Command.as_str(), crate::db::KIND_COMMAND);
    assert_eq!(WorkKind::Agent.as_str(), crate::db::KIND_AGENT);
}

#[test]
fn delivery_of_a_routed_out_work_result_reports_honestly() {
    // No route registered for this session: the ONE delivery function must
    // come back NoRoute (logged as a warn — the session will not hear about
    // the work otherwise), never silently claim success.
    let msg = work_completion(command_payload("ok"));
    let outcome = deliver_work_result(
        Uuid::new_v4(),
        WorkKind::Command,
        "cargo test",
        "",
        "background_task",
        msg,
    );
    assert!(matches!(
        outcome,
        crate::brain::agent::service::session_routes::Delivery::NoRoute
    ));
}
