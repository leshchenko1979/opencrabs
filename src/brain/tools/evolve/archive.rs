//! In-memory extraction from a release archive (`.tar.gz` on Linux/macOS,
//! `.zip` on Windows).
//!
//! Two kinds of member come out of one archive: the binary itself, and the
//! built-in skills sidecar — `skills/<name>/SKILL.md`, which is DATA beside the
//! executable now (#946) rather than bytes compiled into it. An update that
//! took the binary and ignored the tree would leave every profile on the host
//! with no built-in skills at all, so both halves are extracted here.

use std::path::{Component, Path, PathBuf};

use super::super::error::{Result, ToolError};

/// Extract a named file from a .tar.gz archive in memory.
pub(super) fn extract_from_tar_gz(data: &[u8], file_name: &str) -> Result<Vec<u8>> {
    use std::io::Read;

    let decoder = flate2::read::GzDecoder::new(data);
    let mut archive = tar::Archive::new(decoder);

    for entry in archive
        .entries()
        .map_err(|e| ToolError::Execution(format!("Failed to read archive: {}", e)))?
    {
        let mut entry =
            entry.map_err(|e| ToolError::Execution(format!("Failed to read entry: {}", e)))?;

        let path = entry
            .path()
            .map_err(|e| ToolError::Execution(format!("Invalid path in archive: {}", e)))?
            .to_path_buf();

        if path.file_name().and_then(|n| n.to_str()) == Some(file_name) {
            let mut buf = Vec::new();
            entry
                .read_to_end(&mut buf)
                .map_err(|e| ToolError::Execution(format!("Failed to extract: {}", e)))?;
            return Ok(buf);
        }
    }

    Err(ToolError::Execution(format!(
        "'{}' not found in archive",
        file_name
    )))
}

/// Extract a named file from a .zip archive in memory.
pub(super) fn extract_from_zip(data: &[u8], file_name: &str) -> Result<Vec<u8>> {
    use std::io::Read;

    let reader = std::io::Cursor::new(data);
    let mut archive = zip::ZipArchive::new(reader)
        .map_err(|e| ToolError::Execution(format!("Failed to read zip: {}", e)))?;

    for i in 0..archive.len() {
        let mut file = archive
            .by_index(i)
            .map_err(|e| ToolError::Execution(format!("Failed to read zip entry: {}", e)))?;

        if file.name().ends_with(file_name) {
            let mut buf = Vec::new();
            file.read_to_end(&mut buf)
                .map_err(|e| ToolError::Execution(format!("Failed to extract: {}", e)))?;
            return Ok(buf);
        }
    }

    Err(ToolError::Execution(format!(
        "'{}' not found in zip",
        file_name
    )))
}

// --- the skills sidecar (#946) ---------------------------------------------
//
// The release archive carries `skills/<name>/SKILL.md` next to the binary. The
// loader reads that tree from `<exe_dir>/skills/`, so the update path has to
// write it out — extracting the binary alone would leave the tree as it was,
// and on a fresh host that is nothing at all.

/// One file recovered from an archive subtree: where it sits below the
/// requested prefix, and its bytes.
pub(crate) type TreeEntry = (PathBuf, Vec<u8>);

/// Strip `dir_prefix` from an archive member path, rejecting anything that
/// could escape the destination when written back out.
///
/// The archive is checksum-verified before it reaches this code, but a member
/// path is still a write primitive: `..`, an absolute path or a Windows prefix
/// is DROPPED rather than sanitised, because a sanitised traversal is one
/// filename-mangling bug away from escaping again.
pub(crate) fn relative_under(path: &Path, dir_prefix: &str) -> Option<PathBuf> {
    let mut components = path.components();
    match components.next()? {
        Component::Normal(first) if first.to_str() == Some(dir_prefix) => {}
        _ => return None,
    }
    let rest = components.as_path();
    if rest.as_os_str().is_empty() {
        return None;
    }
    if rest.components().all(|c| matches!(c, Component::Normal(_))) {
        Some(rest.to_path_buf())
    } else {
        None
    }
}

/// Extract every regular file under `dir_prefix/` from a `.tar.gz` archive.
///
/// An archive without that subtree yields an empty vector, not an error: a
/// release built before the sidecar existed must still update the binary.
pub(crate) fn extract_tree_from_tar_gz(data: &[u8], dir_prefix: &str) -> Result<Vec<TreeEntry>> {
    use std::io::Read;

    let decoder = flate2::read::GzDecoder::new(data);
    let mut archive = tar::Archive::new(decoder);
    let mut out = Vec::new();

    for entry in archive
        .entries()
        .map_err(|e| ToolError::Execution(format!("Failed to read archive: {e}")))?
    {
        let mut entry =
            entry.map_err(|e| ToolError::Execution(format!("Failed to read entry: {e}")))?;
        if !entry.header().entry_type().is_file() {
            continue;
        }
        let path = entry
            .path()
            .map_err(|e| ToolError::Execution(format!("Invalid path in archive: {e}")))?
            .to_path_buf();
        let Some(relative) = relative_under(&path, dir_prefix) else {
            continue;
        };
        let mut buf = Vec::new();
        entry
            .read_to_end(&mut buf)
            .map_err(|e| ToolError::Execution(format!("Failed to extract {path:?}: {e}")))?;
        out.push((relative, buf));
    }
    Ok(out)
}

/// Extract every regular file under `dir_prefix/` from a `.zip` archive.
///
/// Same contract as [`extract_tree_from_tar_gz`]: a missing subtree is empty,
/// never an error.
pub(crate) fn extract_tree_from_zip(data: &[u8], dir_prefix: &str) -> Result<Vec<TreeEntry>> {
    use std::io::Read;

    let reader = std::io::Cursor::new(data);
    let mut archive = zip::ZipArchive::new(reader)
        .map_err(|e| ToolError::Execution(format!("Failed to read zip: {e}")))?;
    let mut out = Vec::new();

    for i in 0..archive.len() {
        let mut file = archive
            .by_index(i)
            .map_err(|e| ToolError::Execution(format!("Failed to read zip entry: {e}")))?;
        if file.is_dir() {
            continue;
        }
        let Some(relative) = relative_under(Path::new(file.name()), dir_prefix) else {
            continue;
        };
        let mut buf = Vec::new();
        file.read_to_end(&mut buf)
            .map_err(|e| ToolError::Execution(format!("Failed to extract zip entry: {e}")))?;
        out.push((relative, buf));
    }
    Ok(out)
}

/// Write an extracted tree to `dest`, replacing whatever is there.
///
/// Staged in a sibling directory and renamed into place, so the loader never
/// reads a half-written tree — it parses `SKILL.md` on every turn, and a
/// truncated file there drops that skill with only a warning. A failure after
/// the old tree was displaced puts it back, because the alternative is an
/// install with NO built-in skills at all.
///
/// Returns the number of files written.
pub(crate) fn install_tree(dest: &Path, entries: &[TreeEntry]) -> std::io::Result<usize> {
    let parent = dest.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("{} has no parent directory", dest.display()),
        )
    })?;
    std::fs::create_dir_all(parent)?;

    let pid = std::process::id();
    let staging = parent.join(format!(".skills.evolve-{pid}"));
    let retired = parent.join(format!(".skills.evolve-old-{pid}"));
    let _ = std::fs::remove_dir_all(&staging);
    let _ = std::fs::remove_dir_all(&retired);

    let mut written = 0usize;
    for (relative, bytes) in entries {
        let target = staging.join(relative);
        if let Some(dir) = target.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(&target, bytes)?;
        written += 1;
    }

    if dest.exists() {
        std::fs::rename(dest, &retired)?;
    }
    match std::fs::rename(&staging, dest) {
        Ok(()) => {
            let _ = std::fs::remove_dir_all(&retired);
            Ok(written)
        }
        Err(e) => {
            if retired.exists() {
                let _ = std::fs::rename(&retired, dest);
            }
            let _ = std::fs::remove_dir_all(&staging);
            Err(e)
        }
    }
}
