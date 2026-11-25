//! Summariser input strips thinking (#1002).

use crate::brain::agent::service::thinking_trim::strip_thinking_for_summariser;
use crate::brain::provider::{ContentBlock, Message, Role};

#[test]
fn summariser_input_has_no_thinking_blocks() {
    let messages = vec![
        Message::user("go"),
        Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::Thinking {
                    thinking: "chain".repeat(300),
                    signature: None,
                },
                ContentBlock::Text {
                    text: "result".into(),
                },
            ],
        },
    ];
    let stripped = strip_thinking_for_summariser(messages);
    assert_eq!(stripped.len(), 2);
    assert!(
        !stripped[1]
            .content
            .iter()
            .any(|b| matches!(b, ContentBlock::Thinking { .. }))
    );
}
