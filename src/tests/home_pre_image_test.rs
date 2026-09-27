//! Issue #539: the generic write tools now leave a `.bak` pre-image for an
//! existing file under the profile home, and they say so when they do not.
//!
//! Both directions are pinned at the TOOL level — not only at the predicate —
//! because the defect this answers was the absence of a *signal*: a lane could
//! read a success message, assume a revert path existed, and have none. So the
//! message is asserted, not just the filesystem, and a guard that never fires
//! is asserted alongside the one that does.

use crate::brain::tools::brain_file_safety::PRE_IMAGE_MAX_BYTES;
use crate::brain::tools::edit::EditTool;
use crate::brain::tools::hashline::edit::HashlineEditTool;
use crate::brain::tools::hashline::hash::hash_line;
use crate::brain::tools::write::WriteTool;
use crate::brain::tools::{Tool, ToolExecutionContext};
use crate::config::profile::with_home_override_async;
use serde_json::json;
use std::path::{Path, PathBuf};
use tempfile::TempDir;
use uuid::Uuid;

/// The phrase a skipped pre-image is reported with. Named once so the
/// assertions cannot drift from the message they test.
const SKIP_NOTE: &str = "no .bak pre-image was written";

/// `<name>.<stamp>.bak` siblings of `path` — the pre-image scheme's own
/// naming (`brain_file_safety::backup_before_write`), not a glob that would
/// also catch an unrelated `.bak`.
fn pre_images_of(path: &Path) -> Vec<PathBuf> {
    let (Some(dir), Some(name)) = (path.parent(), path.file_name().and_then(|n| n.to_str())) else {
        return Vec::new();
    };
    let prefix = format!("{name}.");
    std::fs::read_dir(dir)
        .expect("read_dir")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with(&prefix) && n.ends_with(".bak"))
        })
        .collect()
}

fn context(dir: &Path) -> ToolExecutionContext {
    ToolExecutionContext::new(Uuid::new_v4()).with_working_directory(dir.to_path_buf())
}

/// A home directory inside a tempdir, ready to be the override target.
fn temp_home(temp: &TempDir) -> PathBuf {
    let home = temp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    home
}

// ── the signal: a skipped pre-image is stated, not silent ────────────────

#[tokio::test]
async fn write_reports_a_pre_image_skipped_by_the_cap() {
    let temp = TempDir::new().unwrap();
    let home = temp_home(&temp);
    let target = home.join("big.bin");

    // Sparse: `len()` clears the cap without writing 16 MiB to disk.
    let file = std::fs::File::create(&target).unwrap();
    file.set_len(PRE_IMAGE_MAX_BYTES + 1).unwrap();
    drop(file);

    let ctx = context(&home);
    let written = target.clone();
    let outcome = with_home_override_async(home, async move {
        WriteTool
            .execute(
                json!({
                    "path": written.to_str().unwrap(),
                    "content": "replacement",
                    "overwrite_read_confirm": true
                }),
                &ctx,
            )
            .await
            .unwrap()
    })
    .await;

    assert!(
        outcome.success,
        "the skip costs the revert path, never the write: {:?}",
        outcome.error
    );
    assert!(
        outcome.output.contains(SKIP_NOTE),
        "an over-cap home write must state the missing pre-image, got: {}",
        outcome.output
    );
    assert!(
        outcome
            .output
            .contains(&(PRE_IMAGE_MAX_BYTES + 1).to_string()),
        "and name the measured size, got: {}",
        outcome.output
    );
    assert_eq!(std::fs::read(&target).unwrap(), b"replacement");
    assert!(
        pre_images_of(&target).is_empty(),
        "an over-cap file is never copied"
    );
}

#[tokio::test]
async fn write_is_silent_when_no_pre_image_was_due() {
    let temp = TempDir::new().unwrap();
    let home = temp_home(&temp);
    let target = home.join("notes.md");
    std::fs::write(&target, "before").unwrap();

    let ctx = context(&home);
    let written = target.clone();
    let outcome = with_home_override_async(home, async move {
        WriteTool
            .execute(
                json!({
                    "path": written.to_str().unwrap(),
                    "content": "after",
                    "overwrite_read_confirm": true
                }),
                &ctx,
            )
            .await
            .unwrap()
    })
    .await;

    assert!(outcome.success, "{:?}", outcome.error);
    assert!(
        !outcome.output.contains(SKIP_NOTE),
        "a pre-image WAS taken here, so no skip note is owed, got: {}",
        outcome.output
    );
}

// ── both directions on a temp home ───────────────────────────────────────

#[tokio::test]
async fn edit_file_under_the_home_leaves_one_pre_image_of_the_prior_text() {
    let temp = TempDir::new().unwrap();
    let home = temp_home(&temp);
    let target = home.join("notes.md");
    std::fs::write(&target, "keep this line\nchange me\n").unwrap();

    let ctx = context(&home);
    let edited = target.clone();
    let outcome = with_home_override_async(home, async move {
        EditTool
            .execute(
                json!({
                    "path": edited.to_str().unwrap(),
                    "operation": "replace",
                    "old_text": "change me",
                    "new_text": "changed"
                }),
                &ctx,
            )
            .await
            .unwrap()
    })
    .await;

    assert!(outcome.success, "the edit must land: {:?}", outcome.error);
    assert_eq!(
        std::fs::read_to_string(&target).unwrap(),
        "keep this line\nchanged\n"
    );

    // The strong assertion: the pre-image holds the PRE-edit bytes, which is
    // what makes it a revert path rather than a second copy of the write.
    let backups = pre_images_of(&target);
    assert_eq!(backups.len(), 1, "exactly one pre-image: {backups:?}");
    assert_eq!(
        std::fs::read_to_string(&backups[0]).unwrap(),
        "keep this line\nchange me\n",
        "the pre-image holds the content the edit replaced"
    );
}

#[tokio::test]
async fn write_outside_the_home_leaves_no_pre_image() {
    let temp = TempDir::new().unwrap();
    let home = temp_home(&temp);
    // A sibling of the home, NOT inside it: the scheme is scoped to the home.
    let outside = temp.path().join("outside.md");
    std::fs::write(&outside, "before").unwrap();

    let ctx = context(temp.path());
    let written = outside.clone();
    let outcome = with_home_override_async(home, async move {
        WriteTool
            .execute(
                json!({
                    "path": written.to_str().unwrap(),
                    "content": "after",
                    "overwrite_read_confirm": true
                }),
                &ctx,
            )
            .await
            .unwrap()
    })
    .await;

    assert!(outcome.success, "{:?}", outcome.error);
    assert!(
        !outcome.output.contains(SKIP_NOTE),
        "nothing was due outside the home, so nothing is owed: {}",
        outcome.output
    );
    assert_eq!(std::fs::read(&outside).unwrap(), b"after");
    assert!(
        pre_images_of(&outside).is_empty(),
        "no pre-image is taken outside the home"
    );
}

// ── the third generic path ───────────────────────────────────────────────

#[tokio::test]
async fn hashline_edit_under_the_home_leaves_a_pre_image() {
    let temp = TempDir::new().unwrap();
    let home = temp_home(&temp);
    let target = home.join("notes.md");
    std::fs::write(&target, "alpha\nbeta\n").unwrap();

    // The tool keys each op on `{line}#{hash}` of the ORIGINAL content.
    let pos = format!("1#{}", hash_line("alpha"));

    let ctx = context(&home);
    let edited = target.clone();
    let outcome = with_home_override_async(home, async move {
        HashlineEditTool
            .execute(
                json!({
                    "path": edited.to_str().unwrap(),
                    "edits": [{ "op": "replace", "pos": pos, "lines": "gamma" }]
                }),
                &ctx,
            )
            .await
            .unwrap()
    })
    .await;

    assert!(
        outcome.success,
        "the hashline edit must land: {:?}",
        outcome.error
    );
    let backups = pre_images_of(&target);
    assert_eq!(
        backups.len(),
        1,
        "hashline_edit writes through the shared chokepoint, so it leaves one too: {backups:?}"
    );
    assert_eq!(std::fs::read_to_string(&backups[0]).unwrap(), "alpha\nbeta\n");
}
