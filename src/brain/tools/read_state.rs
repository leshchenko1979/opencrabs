//! Session-scoped fully-read path tracking (#1168) and document tracking (#831).
//!
//! `read_file` records paths it has shown in full during a session;
//! `write_file` consults the record before overwriting an existing file so a
//! windowed read cannot silently destroy lines the model never saw.
//!
//! State is keyed by `(session_id, canonical path)` and held process-wide:
//! tool instances are constructed independently, so per-instance maps would
//! never meet. Test isolation falls out of the session key — each test
//! context carries its own fresh UUID.
//!
//! The document registry (#831) is a SECOND, separate list, appended only from
//! the read site: project law that lives in a repo (`CLAUDE.md`, `AGENTS.md`,
//! a project's own `docs/*.md`) is read, obeyed, and would otherwise be
//! silently forgotten at the next compaction. It is deliberately not the
//! `#1168` set, because `write_file` also marks fully-read — reusing it would
//! classify every plan, report and evidence file the agent WROTE as a document
//! it READ.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use uuid::Uuid;

/// Upper bound on documents tracked per session. A session that reads a
/// hundred `.md` files keeps the most recent reads; the class exists to
/// survive compaction, not to become an index of the filesystem.
pub const MAX_TRACKED_DOCUMENTS: usize = 64;

fn registry() -> &'static std::sync::Mutex<HashSet<(Uuid, PathBuf)>> {
    static REGISTRY: OnceLock<std::sync::Mutex<HashSet<(Uuid, PathBuf)>>> = OnceLock::new();
    REGISTRY.get_or_init(|| std::sync::Mutex::new(HashSet::new()))
}

fn document_registry() -> &'static std::sync::Mutex<HashMap<Uuid, Vec<PathBuf>>> {
    static DOCS: OnceLock<std::sync::Mutex<HashMap<Uuid, Vec<PathBuf>>>> = OnceLock::new();
    DOCS.get_or_init(|| std::sync::Mutex::new(HashMap::new()))
}

/// Best-effort canonicalization so `./a.txt`, `a.txt`, and an absolute path
/// to the same file collapse onto one key.
///
/// `pub(crate)` so `retained_docs` keys the manifest's paths exactly the way
/// the registry keyed them — two spellings of one path must not become two
/// documents.
pub(crate) fn key_path(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

/// Record that `session_id` was shown the full contents of `path`.
pub fn mark_fully_read(session_id: Uuid, path: &Path) {
    registry()
        .lock()
        .expect("read_state registry poisoned")
        .insert((session_id, key_path(path)));
}

/// Whether `session_id` has been shown the full contents of `path` this run.
pub fn was_fully_read(session_id: Uuid, path: &Path) -> bool {
    registry()
        .lock()
        .expect("read_state registry poisoned")
        .contains(&(session_id, key_path(path)))
}

/// Whether `path` belongs in the document class at all (#831).
///
/// Eligible: a `.md` file the session may be expected to obey as project law.
/// Rejected by construction: skills (`skills/<slug>/<file>.md` — their own
/// class, tracked by `seen_skills`), anything under the agent's own home
/// (brain files are the prompt builder's job, and the profile tree holds
/// config and memory that must not be re-injected by a summariser), and
/// confidential paths (a document class must never become a route around the
/// OC-08 denylist).
pub fn is_document_candidate(path: &Path) -> bool {
    let canonical = key_path(path);
    let Some(name) = canonical.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    if !name.ends_with(".md") {
        return false;
    }
    if super::seen_skills::skill_slug_from_path(&canonical).is_some() {
        return false;
    }
    // `resolve_profile_home`, not `opencrabs_home`: a predicate must not mkdir.
    if canonical.starts_with(crate::config::profile::resolve_profile_home()) {
        return false;
    }
    super::confidential::is_confidential(&canonical).is_none()
}

/// Record that `session_id` read `path` whole (#831).
///
/// No-op for a path that is not a document candidate, and for one already
/// tracked: a re-read neither duplicates nor reorders, so the list stays in
/// first-read order and the cap is FIFO on that order.
pub fn mark_document_read(session_id: Uuid, path: &Path) {
    if !is_document_candidate(path) {
        return;
    }
    let canonical = key_path(path);
    let mut docs = document_registry()
        .lock()
        .expect("read_state document registry poisoned");
    let entries = docs.entry(session_id).or_default();
    if entries.contains(&canonical) {
        return;
    }
    entries.push(canonical);
    if entries.len() > MAX_TRACKED_DOCUMENTS {
        entries.remove(0);
    }
}

/// Whether `session_id` has read `path` whole in this process (#831).
pub fn was_document_read(session_id: Uuid, path: &Path) -> bool {
    let canonical = key_path(path);
    document_registry()
        .lock()
        .expect("read_state document registry poisoned")
        .get(&session_id)
        .is_some_and(|entries| entries.contains(&canonical))
}

/// Documents `session_id` has read whole, oldest first, insertion order (#831).
///
/// Process-scoped, like the fully-read registry: after a daemon restart the
/// list is empty even though the continuation document still names the paths.
pub fn documents_for_session(session_id: Uuid) -> Vec<PathBuf> {
    document_registry()
        .lock()
        .expect("read_state document registry poisoned")
        .get(&session_id)
        .cloned()
        .unwrap_or_default()
}
