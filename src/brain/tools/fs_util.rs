//! Filesystem utilities for brain tools.
//!
//! Provides atomic file write operations to prevent in-place truncation
//! and file corruption while processes may be executing scripts or reading files.

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::fs;
use tokio::io::AsyncWriteExt;

use crate::brain::tools::brain_file_safety::{
    PRE_IMAGE_MAX_BYTES, backup_before_write, is_snapshottable_size, is_under_home,
};
use crate::brain::tools::error::ToolError;

static TEMP_FILE_COUNTER: AtomicU64 = AtomicU64::new(0);

/// What the #539 pre-image leg did for a write — or why it did not.
///
/// The issue this answers is not the missing snapshot alone, it is that the
/// absence carried **no signal**: *"no signal that either protection was
/// skipped."* So the writer returns this to its caller, and the caller states
/// [`PreImage::note`] when there is something to state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreImage {
    /// No pre-image applies: the path is outside the profile home, or the file
    /// did not exist yet and so there was nothing to undo.
    NotNeeded,
    /// Snapshotted to a `<path>.<stamp>.bak` sibling before the write landed.
    BackedUp,
    /// A pre-image was due and was **not** taken. The reason is carried so the
    /// absence is reported rather than passing silently.
    Skipped { reason: String },
}

impl PreImage {
    /// The line a tool appends to its success text when a revert path the
    /// caller assumed existed is absent. `None` when there is nothing to say.
    pub fn note(&self) -> Option<String> {
        match self {
            PreImage::NotNeeded | PreImage::BackedUp => None,
            // House style for a note appended to a tool result (matches
            // `path_lock::contention_notice`): leading blank line, then "Note:".
            PreImage::Skipped { reason } => Some(format!(
                "\n\nNote: no .bak pre-image was written for this file ({reason}), so the \
                 previous content is not recoverable from a sibling backup."
            )),
        }
    }
}

/// The pure half of the pre-image leg: the two facts that decide it, and what
/// follows. Kept free of IO so the size boundary is testable without writing a
/// 16 MiB file in CI.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Plan {
    Snapshot,
    NotNeeded,
    Skip(String),
}

fn plan_pre_image(under_home: bool, existing_len: Option<u64>) -> Plan {
    if !under_home {
        return Plan::NotNeeded;
    }
    let Some(bytes) = existing_len else {
        // First write of the file: no prior content to lose.
        return Plan::NotNeeded;
    };
    // The boundary itself lives in `is_snapshottable_size`, so `<=` vs `>` is
    // encoded once and the cap's test pins it.
    if !is_snapshottable_size(bytes) {
        return Plan::Skip(format!(
            "existing file is {bytes} B, over the {PRE_IMAGE_MAX_BYTES} B pre-image cap"
        ));
    }
    Plan::Snapshot
}

/// Size of `path` if it exists as a regular file, else `None`.
///
/// A directory is deliberately excluded: the pre-image scheme copies files, and
/// `backup_before_write` would fail on a directory anyway.
fn existing_file_len(meta: Option<&std::fs::Metadata>) -> Option<u64> {
    meta.filter(|m| m.is_file()).map(|m| m.len())
}

struct TempFileGuard<'a> {
    path: &'a Path,
    active: bool,
}

impl<'a> TempFileGuard<'a> {
    fn new(path: &'a Path) -> Self {
        Self { path, active: true }
    }

    fn defuse(&mut self) {
        self.active = false;
    }
}

impl<'a> Drop for TempFileGuard<'a> {
    fn drop(&mut self) {
        if self.active {
            let _ = std::fs::remove_file(self.path);
        }
    }
}

/// Take the pre-image leg before a write lands.
///
/// A failed copy is **tolerated and reported**, never fatal: the shipped home
/// tool already treats a snapshot failure as non-blocking
/// (`write_opencrabs_file.rs` does `.ok().flatten()`), and refusing a
/// legitimate write over a backup hiccup would invent a new failure mode
/// instead of providing a revert path.
fn take_pre_image(path: &Path, existing_len: Option<u64>) -> PreImage {
    match plan_pre_image(is_under_home(path), existing_len) {
        Plan::NotNeeded => PreImage::NotNeeded,
        Plan::Skip(reason) => PreImage::Skipped { reason },
        Plan::Snapshot => match backup_before_write(path) {
            Ok(Some(_)) => PreImage::BackedUp,
            // The stat saw a file and the copy did not: it was removed between
            // the two, so there is no prior content to have lost.
            Ok(None) => PreImage::NotNeeded,
            Err(e) => PreImage::Skipped {
                reason: format!("snapshot copy failed: {e}"),
            },
        },
    }
}

/// Atomically write `content` to `path` using a sibling temporary file and rename.
///
/// If `path` already exists, its file permissions are preserved on the replaced file.
/// Writes and flushes to disk before atomically replacing the destination via `fs::rename`.
///
/// Returns what the #539 pre-image leg did, so a caller writing under the
/// profile home can state a skipped snapshot instead of leaving the absence
/// silent. Callers that do not care about the revert path ignore it.
pub async fn atomic_write_file(path: &Path, content: &[u8]) -> Result<PreImage, ToolError> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));

    // One stat serves both the preserved permissions and the pre-image
    // decision. `metadata` errors on an absent path, which is the same "no
    // existing file" answer `try_exists` used to give.
    let existing = fs::metadata(path).await.ok();
    let existing_permissions = existing.as_ref().map(|m| m.permissions());

    let pre_image = take_pre_image(path, existing_file_len(existing.as_ref()));

    let file_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("file");

    let tmp_path = parent.join(format!(
        ".tmp_{file_name}.{}.{}",
        std::process::id(),
        TEMP_FILE_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));

    let mut guard = TempFileGuard::new(&tmp_path);

    {
        let mut file = fs::File::create(&tmp_path).await.map_err(ToolError::Io)?;
        file.write_all(content).await.map_err(ToolError::Io)?;
        file.flush().await.map_err(ToolError::Io)?;
        file.sync_all().await.map_err(ToolError::Io)?;
    }

    if let Some(perms) = existing_permissions {
        let _ = fs::set_permissions(&tmp_path, perms).await;
    }

    if let Err(e) = fs::rename(&tmp_path, path).await {
        let _ = fs::remove_file(&tmp_path).await;
        return Err(ToolError::Io(e));
    }

    guard.defuse();
    Ok(pre_image)
}

#[cfg(test)]
mod tests {
    use super::{Plan, PreImage, atomic_write_file, plan_pre_image};
    use crate::brain::tools::brain_file_safety::PRE_IMAGE_MAX_BYTES;
    use crate::config::profile::with_home_override_async;
    use std::path::Path;
    use tempfile::TempDir;

    /// A `.bak` sibling next to `path`, if one was taken.
    fn pre_images(path: &Path) -> Vec<std::path::PathBuf> {
        let Some(dir) = path.parent() else {
            return Vec::new();
        };
        std::fs::read_dir(dir)
            .expect("read_dir")
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.to_string_lossy().ends_with(".bak"))
            .collect()
    }

    // ── the planner: two facts in, one decision out ──────────────────────

    #[test]
    fn planner_is_not_needed_outside_the_home() {
        // An existing file, but outside the home: the pre-image convenience
        // does not extend there, and no note is owed for its absence.
        assert_eq!(plan_pre_image(false, Some(4096)), Plan::NotNeeded);
    }

    #[test]
    fn planner_is_not_needed_for_a_first_write() {
        // Nothing existed, so there is no prior content to lose — still not a
        // "skip", because nothing was due.
        assert_eq!(plan_pre_image(true, None), Plan::NotNeeded);
    }

    #[test]
    fn planner_covers_a_file_exactly_at_the_cap() {
        let cap = PRE_IMAGE_MAX_BYTES;
        assert_eq!(plan_pre_image(true, Some(1)), Plan::Snapshot);
        assert_eq!(
            plan_pre_image(true, Some(cap)),
            Plan::Snapshot,
            "the cap is the largest covered file (`<=`), not the first excluded"
        );
    }

    #[test]
    fn planner_skips_over_the_cap_and_names_the_size() {
        let over = PRE_IMAGE_MAX_BYTES + 1;
        match plan_pre_image(true, Some(over)) {
            Plan::Skip(reason) => {
                assert!(
                    reason.contains(&over.to_string()),
                    "the reason carries the measured size: {reason}"
                );
                assert!(reason.contains("cap"), "and names the cap: {reason}");
            }
            other => panic!("one byte over the cap is excluded, got {other:?}"),
        }
    }

    // ── the note: the absence must be sayable ────────────────────────────

    #[test]
    fn note_is_silent_when_a_pre_image_was_taken_or_not_due() {
        assert!(PreImage::NotNeeded.note().is_none());
        assert!(PreImage::BackedUp.note().is_none());
    }

    #[test]
    fn note_reports_a_skip() {
        let note = PreImage::Skipped {
            reason: "over cap".to_string(),
        }
        .note()
        .expect("a skipped pre-image owes a note");
        assert!(note.contains(".bak"), "it says what is missing: {note}");
        assert!(note.contains("over cap"), "and why: {note}");
    }

    // ── end to end: planned is not the same as done ──────────────────────

    #[tokio::test]
    async fn atomic_write_under_a_temp_home_leaves_a_pre_image() {
        let temp = TempDir::new().unwrap();
        let home = temp.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let target = home.join("notes.md");
        std::fs::write(&target, b"before").unwrap();
        let written = target.clone();

        with_home_override_async(home, async move {
            let outcome = atomic_write_file(&written, b"after").await.unwrap();
            assert_eq!(outcome, PreImage::BackedUp, "a pre-image is due here");
            assert!(outcome.note().is_none(), "a taken pre-image owes no note");
        })
        .await;

        assert_eq!(std::fs::read(&target).unwrap(), b"after");

        // The strong assertion: the .bak holds the PRE-edit bytes, which is
        // what makes it a revert path rather than a second copy of the write.
        let backups = pre_images(&target);
        assert_eq!(backups.len(), 1, "exactly one .bak sibling: {backups:?}");
        assert_eq!(
            std::fs::read(&backups[0]).unwrap(),
            b"before",
            "the .bak holds the content the write replaced"
        );
    }

    #[tokio::test]
    async fn atomic_write_under_a_temp_home_reports_an_over_cap_skip() {
        let temp = TempDir::new().unwrap();
        let home = temp.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let target = home.join("big.bin");

        // Sparse: `len()` clears the cap without writing 16 MiB to disk, and
        // the skip path copies nothing.
        let file = std::fs::File::create(&target).unwrap();
        file.set_len(PRE_IMAGE_MAX_BYTES + 1).unwrap();
        drop(file);
        let written = target.clone();

        with_home_override_async(home, async move {
            let outcome = atomic_write_file(&written, b"small").await.unwrap();
            assert!(
                matches!(outcome, PreImage::Skipped { .. }),
                "over the cap is skipped, got {outcome:?}"
            );
            assert!(
                outcome.note().is_some(),
                "and the skip is reportable, not silent"
            );
        })
        .await;

        // The write still lands — the skip costs the revert path, never the write.
        assert_eq!(std::fs::read(&target).unwrap(), b"small");
        assert!(
            pre_images(&target).is_empty(),
            "an over-cap file is never copied"
        );
    }

    #[tokio::test]
    async fn atomic_write_outside_the_home_takes_no_pre_image() {
        let temp = TempDir::new().unwrap();
        let target = temp.path().join("outside.md");
        std::fs::write(&target, b"before").unwrap();

        // No override: this resolves the live profile home, and a tempdir is
        // not under it. The predicate is read-only, so nothing live is touched.
        let outcome = atomic_write_file(&target, b"after").await.unwrap();
        assert_eq!(
            outcome,
            PreImage::NotNeeded,
            "outside the home nothing is due"
        );
        assert!(outcome.note().is_none(), "and nothing is owed");

        assert_eq!(std::fs::read(&target).unwrap(), b"after");
        assert!(
            pre_images(&target).is_empty(),
            "no .bak is written outside the home"
        );
    }
}
