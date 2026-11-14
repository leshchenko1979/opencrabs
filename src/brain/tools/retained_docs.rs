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

/// The registered set, process-wide, paired with the reasons any requested
/// path was refused.
///
/// It cannot be a `Vec` threaded between call sites: `register` runs while a
/// compaction is being applied and `render` runs at the next turn's brain
/// assembly, two different calls with no shared stack. Keyed by session, so
/// test isolation falls out of the fresh UUID each test context carries.
///
/// The refusals ride here rather than only reaching a log because a refusal
/// the MODEL never sees is a silent drop: the manifest is model output, so a
/// hallucinated path would otherwise leave the model believing the document
/// it invented is in its context.
type RegisteredRegistry = std::sync::Mutex<HashMap<Uuid, (Vec<DocEntry>, Vec<String>)>>;

fn registered_registry() -> &'static RegisteredRegistry {
    static REGISTERED: OnceLock<RegisteredRegistry> = OnceLock::new();
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
        .insert(session_id, (entries.clone(), rejected.clone()));
    (entries, rejected)
}

/// What this session is carrying right now: the last successfully registered
/// set, empty when it has registered nothing.
pub fn registered_for_session(session_id: Uuid) -> Vec<DocEntry> {
    registered_registry()
        .lock()
        .expect("retained_docs registry poisoned")
        .get(&session_id)
        .map(|(entries, _)| entries.clone())
        .unwrap_or_default()
}

/// Whether the turn-start assembly has anything to say for this session: a
/// retained body to inject, or a refusal the model has not been told about.
pub fn has_retained_set(session_id: Uuid) -> bool {
    registered_registry()
        .lock()
        .expect("retained_docs registry poisoned")
        .get(&session_id)
        .is_some_and(|(entries, refusals)| !entries.is_empty() || !refusals.is_empty())
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
/// `None` only when the session registered nothing at all, so a session that
/// read no documents gets a system brain byte-identical to before #831. A
/// session that DID register documents always gets a block — if none fitted the
/// budget, the block names them instead of returning silence.
pub fn render(session_id: Uuid, budget_tokens: usize) -> Option<String> {
    let (entries, refusals) = registered_registry()
        .lock()
        .expect("retained_docs registry poisoned")
        .get(&session_id)
        .cloned()
        .unwrap_or_default();
    if entries.is_empty() && refusals.is_empty() {
        return None;
    }

    let header = "\n\n## Retained documents\n";
    let mut section = String::from(header);
    let mut used = crate::brain::tokenizer::count_tokens(header);
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

    let mut out = String::new();
    if included > 0 {
        out.push_str(&section);
    } else if !entries.is_empty() {
        // Nothing fitted the leftover budget — the common shape for a session
        // that carries a heavy skill, since documents share the skills' 5 %.
        // Returning `None` here would be the same lie as dropping a refused
        // path: the model asked for these, the harness did not deliver them,
        // and silence is the one outcome it cannot notice.
        out.push_str("\n\n## Documents NOT retained\n");
        out.push_str(
            "These paths were registered for retention and are NOT in your context — read \
             them again if the pending work depends on them.\n",
        );
        for entry in &entries {
            let why = match entry.status {
                DocStatus::Ok => "no room left in the retained-set budget",
                DocStatus::TooLarge => "file too large to retain",
                DocStatus::Unreadable => "unreadable",
            };
            out.push_str(&format!("- {} ({why})\n", entry.path.display()));
        }
    }
    // The manifest is model output, so a refused path is a claim the model made
    // and the harness did not honour. Saying so is what keeps a hallucinated
    // path from reading as a retained one; silence is the only outcome that
    // cannot be recovered from, because the model has no way to notice.
    if !refusals.is_empty() {
        out.push_str("\n\n## Documents NOT retained\n");
        out.push_str(
            "These paths were named in `retained_documents` and refused. They are NOT in your \
             context — read them again if the pending work depends on them.\n",
        );
        for reason in &refusals {
            out.push_str(&format!("- {reason}\n"));
        }
    }
    if out.is_empty() { None } else { Some(out) }
}
