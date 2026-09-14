use opencrabs::brain::agent::service::tool_loop::process_tool_results_capping;
use opencrabs::brain::provider::ContentBlock;
use serde_json::json;
use std::collections::HashMap;
use uuid::Uuid;

#[tokio::test]
async fn test_tool_output_spill_to_disk() {
    let session_id = Uuid::new_v4();
    let large_content = "line of text\n".repeat(2000); // ~26 KB > 16 KB default
    let call_id = "test-call-123";

    let mut tool_results = vec![ContentBlock::ToolResult {
        tool_use_id: call_id.to_string(),
        content: large_content.clone(),
        is_error: false,
    }];

    let mut tool_inputs = HashMap::new();
    let default_input = json!({});
    tool_inputs.insert(call_id, ("bash", &default_input));

    process_tool_results_capping(session_id, &mut tool_results, &tool_inputs).await;

    if let ContentBlock::ToolResult { content, .. } = &tool_results[0] {
        assert!(content.contains("[Output truncated:"));
        assert!(content.contains("Full output saved to: /tmp/opencrabs/tool_output/session_"));
        assert!(content.contains("[... 1920 lines omitted ...]"));
    } else {
        panic!("expected ToolResult");
    }
}

#[tokio::test]
async fn test_tool_output_clamp_inline_no_spill() {
    let session_id = Uuid::new_v4();
    let large_content = "x".repeat(20_000); // 20 KB > 16 KB
    let call_id = "test-call-456";

    let mut tool_results = vec![ContentBlock::ToolResult {
        tool_use_id: call_id.to_string(),
        content: large_content.clone(),
        is_error: false,
    }];

    let mut tool_inputs = HashMap::new();
    let no_spill_input = json!({
        "spill_to_disk": false,
        "max_output_bytes": 1000
    });
    tool_inputs.insert(call_id, ("bash", &no_spill_input));

    process_tool_results_capping(session_id, &mut tool_results, &tool_inputs).await;

    if let ContentBlock::ToolResult { content, .. } = &tool_results[0] {
        assert!(content.contains("spill_to_disk=false"));
        assert!(!content.contains("Full output saved to:"));
        assert!(content.len() < 2000);
    } else {
        panic!("expected ToolResult");
    }
}
