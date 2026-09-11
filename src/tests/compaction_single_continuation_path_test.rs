//! Every compaction site builds its continuation prompt through one path.
//!
//! There are five places a compaction can wake the agent (Regular, MidLoop,
//! Emergency, PostTool, Manual), and each used to call `build_continuation`
//! itself with the same four arguments. Anything that has to ride *every*
//! The continuation (continue-instructions, plan recovery) must reach the DB.
//! This guard pins that every compaction site passes persist=true.

const TOOL_LOOP: &str = "src/brain/agent/service/tool_loop.rs";

use std::path::Path;

/// Occurrences in real code, ignoring line comments so this guard's own
/// explanatory prose (and the loop's) does not count as a call.
fn code_occurrences(text: &str, needle: &str) -> Vec<usize> {
    text.lines()
        .enumerate()
        .filter(|(_, line)| line.split("//").next().unwrap_or("").contains(needle))
        .map(|(i, _)| i + 1)
        .collect()
}

#[test]
fn the_tool_loop_reaches_the_continuation_builder_exactly_once() {
    let text = std::fs::read_to_string(Path::new(TOOL_LOOP))
        .unwrap_or_else(|e| panic!("{TOOL_LOOP} must be readable ({e}); did the module move?"));

    let direct = code_occurrences(&text, "build_continuation(");

    assert_eq!(
        direct.len(),
        1,
        "{TOOL_LOOP} calls build_continuation on {} lines ({:?}), but the loop must reach it \
         only through continuation_prompt. A site calling it directly builds a valid prompt \
         with every rider missing, starting with the #125 skill stamp, and no unit test sees it.",
        direct.len(),
        direct
    );
}

#[test]
fn the_single_path_is_continuation_prompt() {
    let text = std::fs::read_to_string(Path::new(TOOL_LOOP))
        .unwrap_or_else(|e| panic!("{TOOL_LOOP} must be readable ({e}); did the module move?"));

    assert!(
        !code_occurrences(&text, "async fn continuation_prompt(").is_empty(),
        "the shared construction path is gone from {TOOL_LOOP}; the guard above is \
         measuring nothing"
    );
}
