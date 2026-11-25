//! Drop-on-receipt thinking trim (#1000 F9): OpenAI-compat providers must not
//! accumulate in-loop `Thinking` blocks on the wire between tool iterations.

use crate::brain::agent::service::thinking_trim::strip_thinking_from_assistant_message;
use crate::brain::provider::custom_openai_compatible::{
    keep_thinking_through_turn, preserves_thinking,
};
use crate::brain::provider::{ContentBlock, Message, Role};

fn iteration_request_prefix(messages: &[Message]) -> Vec<u8> {
    serde_json::to_vec(messages).expect("messages serialize")
}

fn assistant_with_thinking_and_tool() -> Message {
    Message {
        role: Role::Assistant,
        content: vec![
            ContentBlock::Thinking {
                thinking: "plan the next tool".repeat(40),
                signature: None,
            },
            ContentBlock::Text {
                text: "calling bash".into(),
            },
            ContentBlock::ToolUse {
                id: "tu_1".into(),
                name: "bash".into(),
                input: serde_json::json!({"command": "true"}),
            },
        ],
    }
}

#[test]
fn openai_base_drops_thinking_so_second_iteration_prefix_matches_no_thinking_fixture() {
    let base = Some("https://api.openai.com/v1");
    let model = "gpt-4.1";
    assert!(!preserves_thinking(base, model));
    assert!(!keep_thinking_through_turn(base, model));

    let user = Message::user("run a command");
    let tool_result = Message {
        role: Role::User,
        content: vec![ContentBlock::ToolResult {
            tool_use_id: "tu_1".into(),
            content: "ok".into(),
            is_error: None,
        }],
    };

    let mut with_drop = assistant_with_thinking_and_tool();
    strip_thinking_from_assistant_message(&mut with_drop);
    let dropped_prefix = iteration_request_prefix(&[user.clone(), with_drop, tool_result.clone()]);

    let baseline_assistant = Message {
        role: Role::Assistant,
        content: vec![
            ContentBlock::Text {
                text: "calling bash".into(),
            },
            ContentBlock::ToolUse {
                id: "tu_1".into(),
                name: "bash".into(),
                input: serde_json::json!({"command": "true"}),
            },
        ],
    };
    let baseline_prefix = iteration_request_prefix(&[user, baseline_assistant, tool_result]);

    assert_eq!(
        dropped_prefix, baseline_prefix,
        "drop-on-receipt must match a turn that never carried thinking"
    );
}

#[test]
fn qwen_keeps_thinking_through_turn() {
    let base = Some("https://dashscope.aliyuncs.com/compatible-mode/v1");
    let model = "qwen3-max";
    assert!(preserves_thinking(base, model));
    assert!(keep_thinking_through_turn(base, model));
}
