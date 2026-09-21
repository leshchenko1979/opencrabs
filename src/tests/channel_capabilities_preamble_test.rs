use crate::brain::agent::AgentContext;
use crate::brain::agent::context::CompactionScope;
use crate::brain::agent::service::AgentService;
use crate::brain::prompt_builder::{
    BrainLoader, has_telegram_channel_capabilities, inject_telegram_channel_capabilities,
    telegram_channel_capabilities,
};
use tempfile::TempDir;
use uuid::Uuid;

#[test]
fn test_inject_telegram_channel_capabilities_explicit() {
    let brain_with_runtime = "You are OpenCrabs.\n\n--- Runtime Info ---\nModel: test\n";
    let injected = inject_telegram_channel_capabilities(brain_with_runtime);
    assert!(has_telegram_channel_capabilities(&injected));
    assert!(injected.contains("- Mermaid diagrams: native vertical rendering"));
    assert!(injected.contains("- Markdown tables: GFM tables rendered natively"));
    assert!(injected.contains("- HTML glyphs / formatting"));
    assert!(injected.contains("- Image includes: Markdown syntax"));

    // Ensure it was placed before Runtime Info
    let cap_pos = injected
        .find("--- TELEGRAM CHANNEL CAPABILITIES ---")
        .unwrap();
    let runtime_pos = injected.find("--- Runtime Info ---").unwrap();
    assert!(cap_pos < runtime_pos);

    // Idempotent injection
    let injected_again = inject_telegram_channel_capabilities(&injected);
    assert_eq!(injected, injected_again);
}

#[test]
fn test_inject_telegram_channel_capabilities_without_runtime_info() {
    let plain_brain = "You are OpenCrabs.\n";
    let injected = inject_telegram_channel_capabilities(plain_brain);
    assert!(has_telegram_channel_capabilities(&injected));
    assert!(injected.starts_with("You are OpenCrabs."));
}

#[test]
fn test_brain_loader_does_not_inject_channel_capabilities() {
    // Channel awareness is a per-session runtime decision (#295), not a
    // property of the startup brain: one process serves Telegram, Discord,
    // Slack and cron sessions alike, so the loader must stay channel-agnostic.
    let temp_dir = TempDir::new().unwrap();
    let loader = BrainLoader::new(temp_dir.path().to_path_buf());

    assert!(!has_telegram_channel_capabilities(
        &loader.build_core_brain(None)
    ));
    assert!(!has_telegram_channel_capabilities(
        &loader.build_system_brain(None)
    ));
}

#[test]
fn test_compaction_recovers_telegram_capabilities_if_in_brain() {
    let mut context = AgentContext::new(Uuid::new_v4(), 100_000);
    context.system_brain = Some(format!(
        "You are OpenCrabs.\n\n{}\n\n--- Runtime Info ---\n",
        telegram_channel_capabilities()
    ));

    AgentService::apply_compaction_summary_after(
        &mut context,
        CompactionScope::FullWindow,
        "Summary of previous tasks.",
        0,
    );

    let first_msg = context.messages.first().expect("summary message present");
    let text = match &first_msg.content[0] {
        crate::brain::provider::ContentBlock::Text { text } => text,
        _ => panic!("Expected text block"),
    };

    assert!(text.contains("--- TELEGRAM CHANNEL CAPABILITIES ---"));
    assert!(text.contains("Summary of previous tasks."));
}

#[test]
fn test_compaction_skips_capabilities_for_non_telegram_session() {
    let mut context = AgentContext::new(Uuid::new_v4(), 100_000);
    context.system_brain = Some("You are OpenCrabs.\n\n--- Runtime Info ---\n".to_string());

    AgentService::apply_compaction_summary_after(
        &mut context,
        CompactionScope::FullWindow,
        "Summary of previous tasks.",
        0,
    );

    let first_msg = context.messages.first().expect("summary message present");
    let text = match &first_msg.content[0] {
        crate::brain::provider::ContentBlock::Text { text } => text,
        _ => panic!("Expected text block"),
    };

    assert!(!text.contains("--- TELEGRAM CHANNEL CAPABILITIES ---"));
}
