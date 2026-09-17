//! Unit tests for repeated bash loop 2-stage policy (nudge at 3, break at 5)
//! and file modification loop immediate break (#294).

use crate::brain::agent::error::format_user_error;
use crate::brain::agent::service::loop_break::loop_break_error;
use crate::brain::agent::service::tool_loop::{
    is_bash_call_signature, is_file_mod_call_signature, repeat_loop_action, RepeatLoopAction,
};

#[test]
fn test_tool_signature_classification() {
    // File modification tool signatures
    assert!(is_file_mod_call_signature("write_file:abcd1234"));
    assert!(is_file_mod_call_signature("edit_file:5678ef01"));
    assert!(is_file_mod_call_signature("hashline_edit:9999aaaa"));
    assert!(is_file_mod_call_signature("write_opencrabs_file:11112222"));
    assert!(is_file_mod_call_signature("write:oldprefix"));
    assert!(is_file_mod_call_signature("edit:oldprefix"));

    // Multi-tool signatures with a file mod
    assert!(is_file_mod_call_signature("read_file:1234,write_file:5678"));
    assert!(is_file_mod_call_signature("write_file:1234,bash:5678"));

    // Non-file mod tools
    assert!(!is_file_mod_call_signature("bash:12345678"));
    assert!(!is_file_mod_call_signature("read_file:12345678"));
    assert!(!is_file_mod_call_signature("grep:12345678"));
    assert!(!is_file_mod_call_signature("ls:12345678"));

    // Bash tool signatures
    assert!(is_bash_call_signature("bash:12345678"));
    assert!(is_bash_call_signature("read_file:1234,bash:5678"));
    assert!(!is_bash_call_signature("write_file:12345678"));
    assert!(!is_bash_call_signature("read_file:12345678"));
}

#[test]
fn test_bash_consecutive_loop_transitions() {
    let call = "bash:sleep_and_gh_run_view";
    let mut history: Vec<String> = Vec::new();

    const BASH_CONSECUTIVE_NUDGE: usize = 3;
    const BASH_CONSECUTIVE_BREAK: usize = 5;

    // Call 1
    history.push(call.to_string());
    let count1 = history.iter().rev().take_while(|c| *c == call).count();
    assert_eq!(count1, 1);
    assert!(count1 < BASH_CONSECUTIVE_NUDGE);

    // Call 2
    history.push(call.to_string());
    let count2 = history.iter().rev().take_while(|c| *c == call).count();
    assert_eq!(count2, 2);
    assert!(count2 < BASH_CONSECUTIVE_NUDGE);

    // Call 3 -> Nudge trigger
    history.push(call.to_string());
    let count3 = history.iter().rev().take_while(|c| *c == call).count();
    assert_eq!(count3, 3);
    assert!(count3 >= BASH_CONSECUTIVE_NUDGE);
    assert!(count3 < BASH_CONSECUTIVE_BREAK);

    // Call 4 (after nudge injected)
    history.push(call.to_string());
    let count4 = history.iter().rev().take_while(|c| *c == call).count();
    assert_eq!(count4, 4);
    assert!(count4 < BASH_CONSECUTIVE_BREAK);

    // Call 5 -> Break trigger
    history.push(call.to_string());
    let count5 = history.iter().rev().take_while(|c| *c == call).count();
    assert_eq!(count5, 5);
    assert!(count5 >= BASH_CONSECUTIVE_BREAK);

    // Interleaving resets consecutive count
    history.push("read_file:check_status".to_string());
    history.push(call.to_string());
    let count_reset = history.iter().rev().take_while(|c| *c == call).count();
    assert_eq!(count_reset, 1);
}

#[test]
fn test_file_mod_consecutive_break_threshold() {
    let call = "write_file:src_main_rs_hash";
    let mut history: Vec<String> = Vec::new();
    const FILE_MOD_CONSECUTIVE_BREAK: usize = 4;

    for _ in 0..3 {
        history.push(call.to_string());
        assert!(history.len() < FILE_MOD_CONSECUTIVE_BREAK);
    }

    history.push(call.to_string());
    assert_eq!(history.len(), FILE_MOD_CONSECUTIVE_BREAK);
    let last_n = &history[history.len() - FILE_MOD_CONSECUTIVE_BREAK..];
    assert!(last_n.iter().all(|c| c == call));
}

#[test]
fn test_repeat_loop_action_for_non_mod_tools() {
    let current = "grep:pattern_abc";
    let recent = vec![
        current.to_string(),
        "read_file:xyz".to_string(),
        current.to_string(),
        current.to_string(),
    ];

    // Window 8, nudge at 3, break at 4
    let action_before_nudge = repeat_loop_action(&recent, current, 8, 3, 4, false);
    assert_eq!(action_before_nudge, RepeatLoopAction::Nudge);

    let action_after_nudge_still_3 = repeat_loop_action(&recent, current, 8, 3, 4, true);
    assert_eq!(action_after_nudge_still_3, RepeatLoopAction::Continue);

    let mut recent_4 = recent.clone();
    recent_4.push(current.to_string());
    let action_after_nudge_hit_4 = repeat_loop_action(&recent_4, current, 8, 3, 4, true);
    assert_eq!(action_after_nudge_hit_4, RepeatLoopAction::Break);
}

#[test]
fn test_format_user_error_differentiation() {
    let bash_err = loop_break_error("repeated-bash loop", "bash", 5, 5, "bash sleep 20");
    let msg_bash = format_user_error(&bash_err);
    assert!(msg_bash.contains("repeating the same tool call"));
    assert!(!msg_bash.contains("announcing the same action"));

    let file_mod_err = loop_break_error(
        "file-modification loop",
        "write_file",
        4,
        4,
        "write_file test.rs",
    );
    let msg_file = format_user_error(&file_mod_err);
    assert!(msg_file.contains("repeating the same tool call"));
    assert!(!msg_file.contains("announcing the same action"));

    let announce_err = crate::brain::agent::error::AgentError::Internal(
        "Repetition detected: near-identical announcements repeated within the turn".to_string(),
    );
    let msg_announce = format_user_error(&announce_err);
    assert!(msg_announce.contains("announcing the same action"));

    let stream_err = crate::brain::agent::error::AgentError::Internal(
        "Repetition detected in stream output".to_string(),
    );
    let msg_stream = format_user_error(&stream_err);
    assert!(msg_stream.contains("repetitive text loop was detected"));
}
