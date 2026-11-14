//! Documents a session read whole, retained across compaction (#831).
//!
//! Project law that lives in a repo — `CLAUDE.md`, `AGENTS.md`, a project's
//! own `docs/*.md` — is read, obeyed, and then forgotten at the next
//! compaction, because the retention machinery knows only `skills/<slug>/`.
//! This module is the retention half of the fix: it lists the candidates for
//! the summariser and, once the summariser names them, renders their bodies
//! back into the injected brain inside the same 5% retained-set budget that
//! governs skills.
//!
//! The tracking half lives in `read_state` (`mark_document_read` and friends);
//! nothing here discovers paths on its own.

use std::path::{Path, PathBuf};
use uuid::Uuid;

/// Registration cap: how many documents one manifest may name.
pub const MAX_RETAINED_DOCUMENTS: usize = 8;

/// Row cap for the inventory table and the continuation-document block. One
/// constant, so the two surfaces can never disagree about how many they show.
pub const MAX_DOC_ROWS: usize = 12;

/// Files larger than this are listed but not weighed: tokenising a multi-MB
/// file on every compaction would cost more than the row is worth.
pub const MAX_DOC_INVENTORY_BYTES: u64 = 256 * 1024;

/// Per-body injection cap. A larger body is skipped whole rather than cut
/// mid-sentence: half a document is worse than a path to it.
pub const MAX_DOC_BODY_BYTES: usize = 64 * 1024;

/// How much of a document the probe could actually measure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocStatus {
    /// Read and weighed.
    Ok,
    /// Over [`MAX_DOC_INVENTORY_BYTES`]: size known, tokens and lines unknown.
    TooLarge,
    /// Could not be read at all.
    Unreadable,
}

/// One document candidate, measured.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocEntry {
    pub path: PathBuf,
    pub tokens: usize,
    pub lines: Option<usize>,
    pub bytes: Option<u64>,
    pub status: DocStatus,
}

/// Measure one path. Never panics on IO error — an unreadable document is a
/// row with a status, not a failed compaction.
pub fn probe(path: &Path) -> DocEntry {
    let bytes = std::fs::metadata(path).ok().map(|m| m.len());
    if let Some(len) = bytes
        && len > MAX_DOC_INVENTORY_BYTES
    {
        return DocEntry {
            path: path.to_path_buf(),
            tokens: 0,
            lines: None,
            bytes,
            status: DocStatus::TooLarge,
        };
    }
    match std::fs::read(path) {
        Ok(raw) => {
            let text = String::from_utf8_lossy(&raw);
            DocEntry {
                path: path.to_path_buf(),
                tokens: crate::brain::agent::context::AgentContext::estimate_tokens(&text),
                lines: Some(text.lines().count()),
                bytes: Some(raw.len() as u64),
                status: DocStatus::Ok,
            }
        }
        Err(_) => DocEntry {
            path: path.to_path_buf(),
            tokens: 0,
            lines: None,
            bytes,
            status: DocStatus::Unreadable,
        },
    }
}

/// The documents to show the summariser: newest-read first, capped at
/// [`MAX_DOC_ROWS`]. The `usize` is how many older documents were omitted, so
/// an omission is visible rather than silent.
///
/// Single source of truth for both the inventory table and the
/// continuation-document block.
pub fn inventory_documents(session_id: Uuid) -> (Vec<DocEntry>, usize) {
    let mut paths = crate::brain::tools::read_state::documents_for_session(session_id);
    // The registry is oldest-first; the model cares about the most recent.
    paths.reverse();
    let total = paths.len();
    paths.truncate(MAX_DOC_ROWS);
    let omitted = total - paths.len();
    (paths.iter().map(|p| probe(p)).collect(), omitted)
}
