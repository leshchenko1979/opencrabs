//! #692 task_wait — the incremental stream scanner and the tool's contract.
//!
//! The scanner is the load-bearing new logic in `task_wait`: everything else is
//! plumbing around it. Its one semantic promise is that a "match" is a whole
//! LINE, never a fragment of one, because a caller that wakes on a fragment has
//! been told something that was not yet true.

use crate::brain::agent::service::background_tasks::scan_stream_from;
use crate::brain::tools::Tool;
use crate::brain::tools::task_wait::TaskWaitTool;
use regex::Regex;
use tempfile::TempDir;
use tokio::io::AsyncWriteExt;

/// Append bytes to a path, creating it if absent.
async fn append(path: &std::path::Path, text: &str) {
    let mut f = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .await
        .expect("open stream");
    f.write_all(text.as_bytes()).await.expect("write stream");
}

#[test]
fn tool_identity_and_contract() {
    let tool = TaskWaitTool::new();
    assert_eq!(tool.name(), "task_wait");
    let schema = tool.input_schema();
    assert_eq!(schema["required"][0], "run_id");
    // `pattern` is optional BY DESIGN: omitting it means "wait for it to finish",
    // which is the behaviour a caller wants half the time.
    assert!(schema["properties"]["pattern"].is_object());
    assert_eq!(schema["properties"]["stream"]["enum"][0], "both");
    // Waiting mutates nothing: it is a read of a run's capture.
    assert!(tool.hints().read_only);
    assert!(!tool.hints().destructive);
}

#[tokio::test]
async fn partial_line_does_not_match_and_is_not_consumed() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("r.out");
    // The third line matches but has no newline yet: it is a fragment.
    append(&path, "building\nMATCH-ONE\nMATCH-TW").await;

    let re = Regex::new("MATCH").unwrap();
    let scan = scan_stream_from(&path, 0, Some(&re)).await;

    assert_eq!(scan.matches, vec!["MATCH-ONE".to_string()]);
    // Offset stops after the last COMPLETE line, so the fragment is re-read next
    // poll and matched when it is whole.
    assert_eq!(scan.next_offset, "building\nMATCH-ONE\n".len() as u64);
    // The tail DOES include the fragment: a status report should show what the
    // command has said so far, even mid-line.
    assert!(scan.tail.ends_with("MATCH-TW"), "tail was {:?}", scan.tail);
}

#[tokio::test]
async fn scan_is_incremental_and_finds_only_new_lines() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("r.out");
    append(&path, "MATCH-ONE\n").await;
    let re = Regex::new("MATCH").unwrap();

    let first = scan_stream_from(&path, 0, Some(&re)).await;
    assert_eq!(first.matches.len(), 1);

    // Nothing new: the same offset must yield nothing, which is what stops a
    // poll loop from re-reporting the same line forever.
    let again = scan_stream_from(&path, first.next_offset, Some(&re)).await;
    assert!(again.matches.is_empty(), "re-matched: {:?}", again.matches);

    append(&path, "MATCH-TWO\n").await;
    let third = scan_stream_from(&path, first.next_offset, Some(&re)).await;
    assert_eq!(third.matches, vec!["MATCH-TWO".to_string()]);
}

#[tokio::test]
async fn without_a_pattern_the_tail_still_advances() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("r.out");
    append(&path, "hello\n").await;

    let scan = scan_stream_from(&path, 0, None).await;
    assert!(scan.matches.is_empty(), "no matcher means no matches");
    assert_eq!(scan.tail, "hello\n");
    assert_eq!(scan.next_offset, 6);
}

#[tokio::test]
async fn absent_file_is_an_empty_scan_not_an_error() {
    let dir = TempDir::new().unwrap();
    let missing = dir.path().join("never-created.out");
    let re = Regex::new("x").unwrap();

    let scan = scan_stream_from(&missing, 7, Some(&re)).await;
    // Offset is left alone: an absent stream must not silently rewind a caller
    // that already consumed bytes from a file which later appears.
    assert_eq!(scan.next_offset, 7);
    assert!(scan.matches.is_empty());
    assert!(scan.tail.is_empty());
    assert_eq!(scan.skipped_bytes, 0);
}

#[tokio::test]
async fn a_rewritten_shorter_file_does_not_rewind_the_offset() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("r.out");
    append(&path, "aaaa\n").await;
    let re = Regex::new("a").unwrap();
    let first = scan_stream_from(&path, 0, Some(&re)).await;
    assert_eq!(first.next_offset, 5);

    // A caller holding an offset past EOF (the file was truncated by something
    // else) must not receive phantom bytes from the middle of the new content.
    let past = scan_stream_from(&path, 99, Some(&re)).await;
    assert_eq!(past.next_offset, 99);
    assert!(past.matches.is_empty());
}
