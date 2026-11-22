//! In-memory extraction of a named member from a release archive (#948).
//!
//! One implementation shared by the three call sites that each carried their
//! own copy: the pre-built-binary evolve strategy, crash-recovery rollback,
//! and the RTK auto-download. All three fetch a `.tar.gz` (Linux/macOS) or
//! `.zip` (Windows) release asset into memory and pull one named binary out
//! of it, so the walk lives here once.

use std::io::Read;
use std::path::Path;

/// Extract a named member from an in-memory `.tar.gz` archive.
///
/// Matches on the entry's exact file name, so a leading directory inside the
/// archive is fine. A missing member yields `ErrorKind::NotFound`.
pub(crate) fn extract_member_tar_gz(data: &[u8], file_name: &str) -> std::io::Result<Vec<u8>> {
    collect_tar_gz(data, 1, |path| basename_is(path, file_name))?
        .into_iter()
        .next()
        .map(|(_, bytes)| bytes)
        .ok_or_else(|| missing(file_name))
}

/// Extract a named member from an in-memory `.zip` archive.
///
/// A missing member yields `ErrorKind::NotFound`.
pub(crate) fn extract_member_zip(data: &[u8], file_name: &str) -> std::io::Result<Vec<u8>> {
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(data))?;

    for i in 0..archive.len() {
        let mut file = archive.by_index(i)?;
        if file.name().ends_with(file_name) {
            let mut buf = Vec::new();
            file.read_to_end(&mut buf)?;
            return Ok(buf);
        }
    }

    Err(missing(file_name))
}

/// Walk an in-memory `.tar.gz`, collecting the entries `want` accepts as
/// `(in-archive path, contents)`.
///
/// Stops as soon as `limit` entries have been collected, so a single-member
/// lookup never reads past its target — the same early exit the three
/// hand-rolled copies had.
fn collect_tar_gz(
    data: &[u8],
    limit: usize,
    want: impl Fn(&Path) -> bool,
) -> std::io::Result<Vec<(String, Vec<u8>)>> {
    let decoder = flate2::read::GzDecoder::new(data);
    let mut archive = tar::Archive::new(decoder);
    let mut members = Vec::new();

    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.to_path_buf();
        if !want(&path) {
            continue;
        }
        let mut buf = Vec::new();
        entry.read_to_end(&mut buf)?;
        members.push((path.to_string_lossy().into_owned(), buf));
        if members.len() >= limit {
            break;
        }
    }

    Ok(members)
}

/// Whether `path`'s final component is exactly `file_name`.
fn basename_is(path: &Path, file_name: &str) -> bool {
    path.file_name().and_then(|n| n.to_str()) == Some(file_name)
}

/// The `NotFound` error every extractor reports for an absent member.
fn missing(file_name: &str) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::NotFound,
        format!("'{}' not found in archive", file_name),
    )
}
