use crate::brain::skills::user_skills_dir;
use crate::brain::tools::Tool;
use crate::brain::tools::ToolExecutionContext;
use crate::brain::tools::slash_command::*;
use crate::config::profile::{home_for_profile, with_profile_home_async};
use tokio;

#[test]
fn test_tool_metadata() {
    let tool = SlashCommandTool;
    assert_eq!(tool.name(), "slash_command");
    assert!(tool.requires_approval());
}

#[tokio::test]
async fn test_missing_slash() {
    // Since #1167 a missing leading '/' is autocorrected, so "cd" must
    // behave exactly like "/cd" instead of being rejected.
    let tool = SlashCommandTool;
    let ctx = ToolExecutionContext::new(uuid::Uuid::new_v4());
    let bare = tool
        .execute(serde_json::json!({"command": "cd"}), &ctx)
        .await
        .unwrap();
    let slashed = tool
        .execute(serde_json::json!({"command": "/cd"}), &ctx)
        .await
        .unwrap();
    assert_eq!(
        bare.success, slashed.success,
        "autocorrect makes 'cd' equivalent to '/cd'"
    );
    assert_eq!(bare.output, slashed.output);
}

#[tokio::test]
async fn test_models_returns_provider_info() {
    let tool = SlashCommandTool;
    let ctx = ToolExecutionContext::new(uuid::Uuid::new_v4());
    let result = tool
        .execute(serde_json::json!({"command": "/models"}), &ctx)
        .await
        .unwrap();
    assert!(result.success);
    assert!(result.output.contains("Providers"));
}

#[tokio::test]
async fn test_help_returns_commands() {
    let tool = SlashCommandTool;
    let ctx = ToolExecutionContext::new(uuid::Uuid::new_v4());
    let result = tool
        .execute(serde_json::json!({"command": "/help"}), &ctx)
        .await
        .unwrap();
    assert!(result.success);
    assert!(result.output.contains("/models"));
    assert!(result.output.contains("/usage"));
}

#[tokio::test]
async fn test_cd_no_args() {
    let tool = SlashCommandTool;
    let ctx = ToolExecutionContext::new(uuid::Uuid::new_v4());
    let result = tool
        .execute(serde_json::json!({"command": "/cd"}), &ctx)
        .await
        .unwrap();
    assert!(!result.success);
    assert!(result.error.unwrap().contains("No directory"));
}

#[tokio::test]
async fn test_unknown_command() {
    let tool = SlashCommandTool;
    let ctx = ToolExecutionContext::new(uuid::Uuid::new_v4());
    let result = tool
        .execute(serde_json::json!({"command": "/nonexistent"}), &ctx)
        .await
        .unwrap();
    assert!(!result.success);
    assert!(result.error.unwrap().contains("Unknown command"));
}

#[tokio::test]
async fn test_plan_lifecycle_commands_return_guidance_not_error() {
    // /plan, /execute, /discard, /show-plan are real channel/plan commands the
    // model sees in use; they must not hit the "Unknown command" error arm.
    // Instead they return actionable guidance as a successful result (#574).
    let tool = SlashCommandTool;
    let ctx = ToolExecutionContext::new(uuid::Uuid::new_v4());
    for cmd in ["/plan", "/execute", "/discard", "/show-plan"] {
        let result = tool
            .execute(serde_json::json!({ "command": cmd }), &ctx)
            .await
            .unwrap();
        assert!(
            result.success,
            "{cmd} should return guidance as success, not an Unknown-command error"
        );
        assert!(
            result.output.to_lowercase().contains("plan"),
            "{cmd} guidance should mention the plan: {}",
            result.output
        );
    }
}

#[tokio::test]
async fn test_new_and_cowork_return_guidance_not_error() {
    let tool = SlashCommandTool;
    let ctx = ToolExecutionContext::new(uuid::Uuid::new_v4());
    for cmd in ["/new", "/clear", "/cowork"] {
        let result = tool
            .execute(serde_json::json!({ "command": cmd }), &ctx)
            .await
            .unwrap();
        assert!(
            result.success,
            "{cmd} should return guidance as success, not an error"
        );
    }
}

/// #520 criterion 4: the `/<name>` tool path (`brain/tools/slash_command.rs`)
/// resolves a skill and hands its body to the model. It used to hand over the
/// BARE body — no review-gate reminder, no #406 size warning — while the channel
/// path (`channels/commands.rs`) already went through `prompt_body()`. It now
/// routes through the same method.
///
/// **What makes this test discriminating** (a probe that cannot fail on the
/// pre-fix code measures its own constants): the fixture declares
/// `review_gate: true`, and `prompt_body()` prepends the reminder while the bare
/// body does not — so the assertion below FAILS against the pre-fix path. SIZE
/// could not serve as the discriminator here: no built-in skill exceeds the
/// 500-line threshold (largest is 280), so a built-in-only fixture would pass
/// under BOTH implementations and prove nothing.
#[tokio::test]
async fn skill_arm_carries_prompt_body_not_the_bare_body() {
    let profile = format!("test-slash-skill-{}", uuid::Uuid::new_v4());

    let result = with_profile_home_async(Some(&profile), async {
        let skill_dir = user_skills_dir().join("gate-fixture");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: gate-fixture\ndescription: discriminating fixture\n\
             review_gate: true\n---\n\nDraft the thing.\n",
        )
        .unwrap();

        let tool = SlashCommandTool;
        let ctx = ToolExecutionContext::new(uuid::Uuid::new_v4());
        tool.execute(serde_json::json!({ "command": "/gate-fixture" }), &ctx)
            .await
            .unwrap()
    })
    .await;

    let _ = std::fs::remove_dir_all(home_for_profile(Some(&profile)));

    assert!(
        result.success,
        "the slash tool must resolve the user skill, got: {:?}",
        result.error
    );
    assert!(
        result.output.contains("SKILL REVIEW GATE"),
        "the slash arm must inherit prompt_body(); the bare body carries no \
         reminder. got: {}",
        result.output
    );
    assert!(
        result.output.contains("Draft the thing."),
        "the body must still arrive in full, got: {}",
        result.output
    );
}
