//! #624 regression: a line longer than `MAX_LINE_CHARS` (2 000) is truncated
//! for DISPLAY only, but the hash `read_file(hashline=true)` returns for it
//! must still be accepted by `hashline_edit`.
//!
//! Before the fix the hash was computed over the CLAMPED render, so the hash
//! the tool had just returned existed nowhere in the file. The edit was then
//! refused with `Hash #XXXX not found in file. The file may have changed since
//! your last read.` — a false staleness diagnosis blaming the caller for a
//! long line.
//!
//! Both read paths are covered: the small-file branch and the buffered /
//! ranged branch (`read_with_buffer`), because each builds its own render.
//!
//! Note the clamp announcement is METADATA (`warning`), not output — the rows
//! in `output` carry the hashes.

use crate::brain::tools::Tool;
use crate::brain::tools::ToolExecutionContext;
use crate::brain::tools::hashline::HashlineEditTool;
use crate::brain::tools::read::ReadTool;
use uuid::Uuid;

/// Seed a file whose middle line is longer than the display clamp, flanked by
/// short lines so the long one is interior. Returns the long line's content.
fn seed(path: &std::path::Path, long_chars: usize) -> String {
    let long = "a".repeat(long_chars);
    let body = format!("short first\n{long}\nshort last\n");
    std::fs::write(path, body).expect("seed fixture");
    long
}

/// The clamp announcement for a read, or `""` when nothing was clamped.
fn clamp_warning(result: &crate::brain::tools::ToolResult) -> String {
    result.metadata.get("warning").cloned().unwrap_or_default()
}

/// Pull the hash for the row whose content starts with `needle`.
fn hash_for(output: &str, needle: &str) -> String {
    output
        .lines()
        .filter_map(|row| {
            let (hash, content) = row.split_once('|')?;
            // A collision row is emitted as `COLLISION|<line>` and carries no
            // usable hash; the clamp note has no `|` at all and is filtered
            // out by `split_once`.
            if hash == "COLLISION" || !content.starts_with(needle) {
                return None;
            }
            Some(hash.to_string())
        })
        .next()
        .unwrap_or_else(|| panic!("no hashline row for {needle:?} in:\n{output}"))
}

async fn read_hashline(
    path: &std::path::Path,
    ctx: &ToolExecutionContext,
    ranged: bool,
) -> crate::brain::tools::ToolResult {
    let tool = ReadTool;
    let input = if ranged {
        serde_json::json!({
            "path": path.to_str().unwrap(),
            "hashline": true,
            "start_line": 0,
            "line_count": 50
        })
    } else {
        serde_json::json!({ "path": path.to_str().unwrap(), "hashline": true })
    };
    let result = tool.execute(input, ctx).await.unwrap();
    assert!(
        result.success,
        "hashline read must succeed: {}",
        result.output
    );
    result
}

async fn edit_with_hash(
    path: &std::path::Path,
    ctx: &ToolExecutionContext,
    hash: &str,
) -> crate::brain::tools::ToolResult {
    let tool = HashlineEditTool;
    tool.execute(
        serde_json::json!({
            "path": path.to_str().unwrap(),
            "edits": [{ "op": "replace", "pos": hash, "lines": "edited" }]
        }),
        ctx,
    )
    .await
    .unwrap()
}

#[tokio::test]
async fn a_hash_from_a_clamped_line_is_accepted_on_the_small_file_path() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("long_small.txt");
    let long = seed(&path, 3000);
    let ctx = ToolExecutionContext::new(Uuid::new_v4())
        .with_working_directory(dir.path().to_path_buf())
        .with_auto_approve(true);

    let read = read_hashline(&path, &ctx, false).await;
    assert!(
        clamp_warning(&read).contains("truncated for DISPLAY only"),
        "fixture must clamp, and the note must say the clamp is display-only, got: {:?}",
        clamp_warning(&read)
    );
    assert!(
        read.output.contains("line truncated:"),
        "the clamped render must still mark the cut:\n{}",
        read.output
    );
    let hash = hash_for(&read.output, "aaaa");

    let result = edit_with_hash(&path, &ctx, &hash).await;
    assert!(
        result.success,
        "the hash read_file just returned for a clamped line must be editable, got: {}",
        result.output
    );

    let after = std::fs::read_to_string(&path).unwrap();
    assert!(after.contains("edited"), "the edit must land");
    assert!(!after.contains(&long), "the long line must be replaced");
    assert!(after.contains("short first"), "neighbouring lines survive");
}

#[tokio::test]
async fn a_hash_from_a_clamped_line_is_accepted_on_the_ranged_read_path() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("long_ranged.txt");
    let long = seed(&path, 3000);
    let ctx = ToolExecutionContext::new(Uuid::new_v4())
        .with_working_directory(dir.path().to_path_buf())
        .with_auto_approve(true);

    let read = read_hashline(&path, &ctx, true).await;
    assert!(
        clamp_warning(&read).contains("truncated for DISPLAY only"),
        "ranged read must clamp too, got: {:?}",
        clamp_warning(&read)
    );
    let hash = hash_for(&read.output, "aaaa");

    let result = edit_with_hash(&path, &ctx, &hash).await;
    assert!(
        result.success,
        "a hash from a clamped line must be editable on the buffered path, got: {}",
        result.output
    );

    let after = std::fs::read_to_string(&path).unwrap();
    assert!(!after.contains(&long), "the long line must be replaced");
}

#[tokio::test]
async fn a_line_exactly_at_the_limit_is_not_clamped_and_edits() {
    // 2 000 is the clamp threshold: a line AT it must not clamp, and its hash
    // must be accepted — the un-clamped control arm for the two tests above.
    let dir = tempfile::TempDir::new().unwrap();
    let ctx = ToolExecutionContext::new(Uuid::new_v4())
        .with_working_directory(dir.path().to_path_buf())
        .with_auto_approve(true);

    let at = dir.path().join("at_limit.txt");
    let at_limit = seed(&at, 2_000);

    let read = read_hashline(&at, &ctx, false).await;
    assert!(
        !clamp_warning(&read).contains("truncated"),
        "a line exactly at the limit must not clamp, got: {:?}",
        clamp_warning(&read)
    );
    let result = edit_with_hash(&at, &ctx, &hash_for(&read.output, "aaaa")).await;
    assert!(result.success, "at-limit hash must edit: {}", result.output);
    assert!(!std::fs::read_to_string(&at).unwrap().contains(&at_limit));
}

#[tokio::test]
async fn a_hashline_read_of_an_empty_file_still_announces_itself() {
    // The synthesised empty-file note (#987) has no raw counterpart to pair
    // with, so the hashline pass must fall back to the render rather than
    // returning nothing. Silence is the most expensive thing a tool can
    // return, and a hashline read must not reintroduce it.
    let f = tempfile::Builder::new().suffix(".txt").tempfile().unwrap();
    let ctx = ToolExecutionContext::new(Uuid::new_v4());

    let tool = ReadTool;
    let result = tool
        .execute(
            serde_json::json!({ "path": f.path().to_str().unwrap(), "hashline": true }),
            &ctx,
        )
        .await
        .unwrap();

    assert!(result.success);
    assert!(
        result.output.contains("(file exists and is empty, 0 bytes)"),
        "an empty hashline read must still announce itself, got: {:?}",
        result.output
    );
}
