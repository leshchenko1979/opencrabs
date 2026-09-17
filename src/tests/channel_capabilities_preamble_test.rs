use crate::brain::agent::service::AgentService;
use crate::brain::agent::AgentContext;
use crate::brain::prompt_builder::{
    inject_telegram_channel_capabilities, BrainLoader, RuntimeInfo,
    TELEGRAM_CHANNEL_CAPABILITIES,
};
use tempfile::TempDir;
use uuid::Uuid;

#[test]
fn test_inject_telegram_channel_capabilities_explicit() {
    let brain_with_runtime = "You are OpenCrabs.\n\n--- Runtime Info ---\nModel: test\n";
    let injected = inject_telegram_channel_capabilities(brain_with_runtime);
    assert!(injected.contains("--- TELEGRAM CHANNEL CAPABILITIES ---"));
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
    assert!(injected.contains("--- TELEGRAM CHANNEL CAPABILITIES ---"));
    assert!(injected.starts_with("You are OpenCrabs."));
}

#[test]
fn test_runtime_info_channel_field_and_brain_loader() {
    let temp_dir = TempDir::new().unwrap();
    let loader = BrainLoader::new(temp_dir.path());

    let mut info_telegram = RuntimeInfo::default();
    info_telegram.channel = Some("telegram".to_string());
    info_telegram.model = Some("test-model".to_string());

    let brain_tg = loader.build_core_brain(Some(&info_telegram));
    assert!(brain_tg.contains("--- TELEGRAM CHANNEL CAPABILITIES ---"));
    assert!(brain_tg.contains("Channel: telegram"));

    let mut info_discord = RuntimeInfo::default();
    info_discord.channel = Some("discord".to_string());
    let brain_discord = loader.build_core_brain(Some(&info_discord));
    assert!(!brain_discord.contains("--- TELEGRAM CHANNEL CAPABILITIES ---"));
    assert!(brain_discord.contains("Channel: discord"));

    let brain_system_tg = loader.build_system_brain(Some(&info_telegram));
    assert!(brain_system_tg.contains("--- TELEGRAM CHANNEL CAPABILITIES ---"));
    assert!(brain_system_tg.contains("Channel: telegram"));
}

#[test]
fn test_compaction_recovers_telegram_capabilities_if_in_brain() {
    let mut context = AgentContext::new(Uuid::new_v4(), "test-model".to_string(), 100_000);
    context.system_brain = Some(format!(
        "You are OpenCrabs.\n\n{}\n\n--- Runtime Info ---\n",
        TELEGRAM_CHANNEL_CAPABILITIES
    ));

    AgentService::apply_compaction_summary(&mut context, "Summary of previous tasks.");

    let first_msg = context.messages.first().expect("summary message present");
    let text = match &first_msg.content[0] {
        crate::brain::provider::ContentBlock::Text { text } => text,
        _ => panic!("Expected text block"),
    };

    assert!(text.contains("--- TELEGRAM CHANNEL CAPABILITIES ---"));
    assert!(text.contains("Summary of previous tasks."));
}

