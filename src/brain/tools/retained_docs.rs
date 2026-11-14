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

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
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

/// The registered set, process-wide.
///
/// It cannot be a `Vec` threaded between call sites: `register` runs while a
/// compaction is being applied and `render` runs at the next turn's brain
/// assembly, two different calls with no shared stack. Keyed by session, so
/// test isolation falls out of the fresh UUID each test context carries.
fn registered_registry() -> &'static std::sync::Mutex<HashMap<Uuid, Vec<DocEntry>>> {
    static REGISTERED: OnceLock<std::sync::Mutex<HashMap<Uuid, Vec<DocEntry>>>> = OnceLock::new();
    REGISTERED.get_or_init(|| std::sync::Mutex::new(HashMap::new()))
}

/// Resolve the summariser's `retained_documents` list into the set this session
/// will carry, and say which requests were refused.
///
/// REPLACES the session's set: a later compaction's manifest is the model's
/// current answer, and merging with an earlier one would keep documents the
/// model has since dropped.
///
/// A path is accepted only if it is a document candidate AND this session read
/// it whole in this process. The second leg is what makes the field safe to
/// hand a model: naming a path is not evidence of having read it, and a
/// document the process never opened cannot be re-injected from a stale copy.
/// Order is newest-read first — independent of the manifest's own order, so the
/// result is stable and a re-registration of the same list is idempotent.
pub fn register(session_id: Uuid, requested: &[String]) -> (Vec<DocEntry>, Vec<String>) {
    use crate::brain::tools::read_state;

    // Oldest-first in the registry; the model cares about the most recent.
    let mut read_order = read_state::documents_for_session(session_id);
    read_order.reverse();

    let mut accepted: Vec<(usize, PathBuf)> = Vec::new();
    let mut rejected: Vec<String> = Vec::new();

    for raw in requested {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            continue;
        }
        let path = PathBuf::from(trimmed);
        if !read_state::is_document_candidate(&path) {
            rejected.push(format!(
                "not registered: {trimmed} (not a project document)"
            ));
            continue;
        }
        if !read_state::was_document_read(session_id, &path) {
            rejected.push(format!(
                "not registered: {trimmed} (not read in this process)"
            ));
            continue;
        }
        let canonical = read_state::key_path(&path);
        let Some(position) = read_order.iter().position(|p| p == &canonical) else {
            rejected.push(format!(
                "not registered: {trimmed} (not read in this process)"
            ));
            continue;
        };
        if accepted.iter().any(|(_, p)| p == &canonical) {
            continue;
        }
        accepted.push((position, canonical));
    }

    // Newest-read first, then the cap: the documents the model most recently
    // obeyed are the ones most likely to govern the pending work, so the cap
    // must never be decided by the order the manifest happened to list.
    accepted.sort_by_key(|(position, _)| *position);
    if accepted.len() > MAX_RETAINED_DOCUMENTS {
        for (_, path) in accepted.drain(MAX_RETAINED_DOCUMENTS..) {
            rejected.push(format!(
                "not registered: {} (over the {MAX_RETAINED_DOCUMENTS}-document cap)",
                path.display()
            ));
        }
    }

    let entries: Vec<DocEntry> = accepted.iter().map(|(_, p)| probe(p)).collect();
    registered_registry()
        .lock()
        .expect("retained_docs registry poisoned")
        .insert(session_id, entries.clone());
    (entries, rejected)
}

/// What this session is carrying right now: the last successfully registered
/// set, empty when it has registered nothing.
pub fn registered_for_session(session_id: Uuid) -> Vec<DocEntry> {
    registered_registry()
        .lock()
        .expect("retained_docs registry poisoned")
        .get(&session_id)
        .cloned()
        .unwrap_or_default()
}

/// Re-inject the registered bodies, inside whatever budget the skills left.
///
/// `budget_tokens` is the LEFTOVER of the retained-set budget, not a second
/// allowance: skills and documents share one 5 % envelope, so a session that
/// carries heavy skills gets proportionally less room for documents and no
/// constant in this module can drift from that.
///
/// Newest-read first, and the first document that does not fit STOPS the walk
/// rather than being skipped over: the order is by recency, so a body that
/// overflows is evidence the older ones — smaller only by accident — are not
/// the ones to spend the remaining room on. A body over
/// [`MAX_DOC_BODY_BYTES`] is skipped whole; half a document is worse than a
/// path to it.
///
/// `None` when nothing was included, so a session with no registered documents
/// gets a system brain byte-identical to before #831.
pub fn render(session_id: Uuid, budget_tokens: usize) -> Option<String> {
    let entries = registered_for_session(session_id);
    if entries.is_empty() || budget_tokens == 0 {
        return None;
    }

    let mut section = String::from("\n\n## Retained documents\n");
    let mut used = crate::brain::tokenizer::count_tokens(&section);
    let mut included = 0usize;

    for entry in &entries {
        if entry.status != DocStatus::Ok {
            continue;
        }
        match std::fs::metadata(&entry.path) {
            Ok(meta) if meta.len() > MAX_DOC_BODY_BYTES as u64 => continue,
            Ok(_) => {}
            Err(_) => continue,
        }
        let Ok(raw) = std::fs::read(&entry.path) else {
            continue;
        };
        let body = String::from_utf8_lossy(&raw);
        let chunk = format!(
            "\n\n### {}\n```\n{}\n```",
            entry.path.display(),
            body.trim_end()
        );
        let cost = crate::brain::tokenizer::count_tokens(&chunk);
        if used + cost > budget_tokens {
            break;
        }
        used += cost;
        section.push_str(&chunk);
        included += 1;
    }

    if included == 0 { None } else { Some(section) }
}
