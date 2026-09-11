//! Session-scoped seen-skill tracking (issue #131, #150).
//!
//! Records which skill bodies a session has CONSUMED — by any surface — so
//! the post-compaction advisory stamp (#125) can list skills the agent
//! actually read, not only those invoked via slash command.
//!
//! Two hooks feed this registry:
//! - `load_brain_file` with a bare skill slug (the #131 canonical form)
//! - `read_file` on a `skills/<slug>/SKILL.md` path (whole-file reads)
//! - `load_brain_file` with the filename form `<slug>.md` (issue #138)
//! - slash invocation (`register_active_skill`) also marks seen (#138) so
//!   the stamp's union survives restarts
//!
//! Persistence (#138): every `mark_seen` best-effort writes a row to the
//! `session_seen_skills` table (`SessionSkillsRepository`); daemon boot
//! hydrates the registry from that table, so a restart/rebuild no longer
//! makes a skill-consuming session look "skill-less" to the stamp. DB is
//! the durability layer only — the in-memory set stays the hot path, and
//! any DB failure degrades to a WARN (no panic, stamp never fails).
//!
//! In-memory registry stores `(session_id, slug) -> epoch` (issue #150).
//! When context compaction occurs, `note_compaction` bumps the session's epoch.
//! `seen_since_compaction` checks whether a skill was seen at or after the current epoch.
//!
//! This is deliberately SEPARATE from `AgentService::active_skills` (the
//! #219 slash-invocation registry): that set also drives per-turn body
//! re-injection into the system prompt, and read-counted skills must not be
//! re-injected on top of the read already present in conversation history.
//! The compaction stamp is the UNION of both registries.

use std::collections::{BTreeSet, HashMap};
use std::path::Path;
use std::sync::OnceLock;
use uuid::Uuid;

/// In-memory registry: (session, slug) → the compaction epoch at which the
/// skill body was last loaded into that session's context (issue #150).
/// Epoch 0 == "loaded before any compaction".
fn registry() -> &'static std::sync::Mutex<HashMap<(Uuid, String), u64>> {
    static REGISTRY: OnceLock<std::sync::Mutex<HashMap<(Uuid, String), u64>>> = OnceLock::new();
    REGISTRY.get_or_init(|| std::sync::Mutex::new(HashMap::new()))
}

/// Per-session compaction epoch counter (issue #150). `note_compaction`
/// bumps it; skills seen at an older epoch are no longer "in context".
fn epochs() -> &'static std::sync::Mutex<HashMap<Uuid, u64>> {
    static EPOCHS: OnceLock<std::sync::Mutex<HashMap<Uuid, u64>>> = OnceLock::new();
    EPOCHS.get_or_init(|| std::sync::Mutex::new(HashMap::new()))
}

/// Extract the skill slug from a path that points at a skill definition
/// file: any path whose second-to-last component is `skills` and whose
/// file name is `SKILL.md` yields `Some(slug)`. Returns `None` for
/// everything else (brain files, regular files, skill assets).
pub fn skill_slug_from_path(path: &Path) -> Option<String> {
    let file_name = path.file_name()?.to_str()?;
    if file_name != "SKILL.md" {
        return None;
    }
    let mut comps = path.components().rev();
    comps.next()?; // SKILL.md
    let slug = comps.next()?;
    if comps.next()?.as_os_str() != "skills" {
        return None;
    }
    slug.as_os_str().to_str().map(|s| s.to_string())
}

/// Record that `session_id` consumed skill `slug` (via read or slug-form
/// load) at the session's CURRENT compaction epoch.
///
/// Always upserts (issue #150: updating the epoch ensures that re-reading
/// a skill after compaction unblocks the skill glob gate).
///
/// Also best-effort persists the row (#138) on first sight in this process:
/// a DB write failure logs WARN and is swallowed — the in-memory registry
/// stays authoritative for the current process, persistence only buys
/// restart durability. Spawned on a detached task so the hot read/load
/// path never awaits the DB.
pub fn mark_seen(session_id: Uuid, slug: &str) {
    let epoch = current_epoch(session_id);
    let newly = registry()
        .lock()
        .expect("seen_skills registry poisoned")
        .insert((session_id, slug.to_string()), epoch)
        .is_none();
    if newly {
        let slug = slug.to_string();
        // Persist only inside a live tokio runtime — plain #[test] fns and
        // other non-async contexts have no reactor; the in-memory registry
        // already did its job there, and DB durability is best-effort.
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                match persist_seen(session_id, &slug).await {
                    Ok(()) => {}
                    Err(e) => tracing::warn!(
                        "seen_skills: DB persist of ({session_id}, {slug}) failed (in-memory \
                         registry unaffected): {e:#}"
                    ),
                }
            });
        }
    }
}

/// Best-effort DB persist of one seen-skill row (#138). Soft-fails when no
/// global pool exists yet (unit tests, pre-connect startup).
async fn persist_seen(session_id: Uuid, slug: &str) -> anyhow::Result<()> {
    let pool = crate::db::global_pool()
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("no global DB pool (not connected yet)"))?;
    crate::db::repository::SessionSkillsRepository::new(pool)
        .record(session_id, slug)
        .await
}

/// Hydrate the in-memory registry from the DB at daemon boot (#138).
///
/// Loads every persisted (session, slug) row into the registry so a
/// restart does not erase skills a session consumed before it. Also prunes
/// rows whose session no longer exists (hygiene, soft-fail). Called from
/// `AgentService::new` — the chokepoint every surface constructs through —
/// via a detached task so service construction never blocks on the DB.
/// Only fires once per process (the registry static is process-wide; a
/// second hydrate is a no-op by construction but still skipped for clarity).
pub fn hydrate_from_db() {
    use std::sync::atomic::{AtomicBool, Ordering};
    static HYDRATED: AtomicBool = AtomicBool::new(false);
    if HYDRATED.swap(true, Ordering::Relaxed) {
        return;
    }
    let Some(pool) = crate::db::global_pool().cloned() else {
        tracing::debug!("seen_skills: no global DB pool at hydrate time (tests?) — skipping");
        return;
    };
    tokio::spawn(async move {
        let repo = crate::db::repository::SessionSkillsRepository::new(pool);
        match repo.all().await {
            Ok(rows) => {
                let n = {
                    // Guard strictly scoped: a MutexGuard is not Send, so it
                    // must not be alive across the prune await below.
                    let mut reg = registry().lock().expect("seen_skills registry poisoned");
                    for (sid, slug) in rows {
                        // Hydrated rows carry epoch 0 — "loaded before any
                        // compaction" (issue #150): they count for the #125
                        // stamp inventory without claiming the body is still
                        // in context for the skill glob gate.
                        reg.insert((sid, slug), 0);
                    }
                    reg.len()
                };
                tracing::info!("seen_skills: hydrated registry from DB ({n} total rows)");
                match repo.prune_missing_sessions().await {
                    Ok(0) => {}
                    Ok(k) => tracing::debug!("seen_skills: pruned {k} row(s) for dead sessions"),
                    Err(e) => tracing::warn!("seen_skills: prune failed (soft): {e:#}"),
                }
            }
            Err(e) => tracing::warn!(
                "seen_skills: DB hydrate failed (in-memory registry starts empty, \
                 stamps may undercount until next mark_seen): {e:#}"
            ),
        }
    });
}

/// The session's current compaction epoch (0 before any compaction).
fn current_epoch(session_id: Uuid) -> u64 {
    *epochs()
        .lock()
        .expect("seen_skills epochs poisoned")
        .get(&session_id)
        .unwrap_or(&0)
}

/// Bump the session's compaction epoch (issue #150): called from the
/// compaction path. AFTER a compaction, the registry entries keep their
/// old epoch, so `seen_since_compaction` flips false for every skill
/// until the body is re-read. Clears NOTHING — the #125 stamp's
/// seen-inventory (`seen_for_session`) remains intact.
pub fn note_compaction(session_id: Uuid) {
    let mut epochs = epochs().lock().expect("seen_skills epochs poisoned");
    let next = epochs.get(&session_id).copied().unwrap_or(0) + 1;
    epochs.insert(session_id, next);
}

/// Whether `session_id`'s context currently holds skill `slug`'s body:
/// the stored epoch is >= the session's current epoch. Fresh sessions
/// (no rows) report false — the gate fires on the first matching call.
pub fn seen_since_compaction(session_id: Uuid, slug: &str) -> bool {
    let stored = registry()
        .lock()
        .expect("seen_skills registry poisoned")
        .get(&(session_id, slug.to_string()))
        .copied();
    if let Some(epoch) = stored {
        epoch >= current_epoch(session_id)
    } else {
        false
    }
}

/// Whether `session_id` has consumed skill `slug` this run (any epoch —
/// legacy stamp inventory semantics; the gate uses
/// [`seen_since_compaction`]).
pub fn was_seen(session_id: Uuid, slug: &str) -> bool {
    registry()
        .lock()
        .expect("seen_skills registry poisoned")
        .contains_key(&(session_id, slug.to_string()))
}

/// All skills `session_id` has consumed, sorted (deterministic stamp order).
pub fn seen_for_session(session_id: Uuid) -> Vec<String> {
    let all: BTreeSet<String> = registry()
        .lock()
        .expect("seen_skills registry poisoned")
        .iter()
        .filter(|((sid, _), _)| *sid == session_id)
        .map(|((_, slug), _)| slug.clone())
        .collect();
    all.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slug_extraction_from_skill_paths() {
        assert_eq!(
            skill_slug_from_path(Path::new(
                "/root/.opencrabs/profiles/ops/skills/opencrabs-dev/SKILL.md"
            )),
            Some("opencrabs-dev".to_string())
        );
        assert_eq!(
            skill_slug_from_path(Path::new("skills/foo/SKILL.md")),
            Some("foo".to_string())
        );
    }

    #[test]
    fn non_skill_paths_yield_none() {
        assert_eq!(
            skill_slug_from_path(Path::new("/home/user/MEMORY.md")),
            None
        );
        assert_eq!(skill_slug_from_path(Path::new("skills/foo/other.md")), None);
        assert_eq!(
            skill_slug_from_path(Path::new("not-skills/foo/SKILL.md")),
            None
        );
        assert_eq!(skill_slug_from_path(Path::new("skills/foo/")), None);
    }

    #[test]
    fn mark_seen_is_idempotent_and_session_scoped() {
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        mark_seen(a, "opencrabs-dev");
        mark_seen(a, "opencrabs-dev");
        assert!(was_seen(a, "opencrabs-dev"));
        assert_eq!(seen_for_session(a), vec!["opencrabs-dev".to_string()]);
        assert!(!was_seen(b, "opencrabs-dev"));
        assert!(seen_for_session(b).is_empty());
    }

    #[test]
    fn seen_list_is_sorted_and_multi() {
        let a = Uuid::new_v4();
        mark_seen(a, "zeta");
        mark_seen(a, "alpha");
        assert_eq!(
            seen_for_session(a),
            vec!["alpha".to_string(), "zeta".to_string()]
        );
    }

    // --- issue #150: epoch-carrying registry + skill-gate semantics ---

    #[test]
    fn fresh_session_reports_not_seen_since_compaction() {
        let a = Uuid::new_v4();
        assert!(!seen_since_compaction(a, "anything"));
    }

    #[test]
    fn seen_passes_and_compaction_rearms_gate() {
        let a = Uuid::new_v4();
        mark_seen(a, "my-skill");
        assert!(seen_since_compaction(a, "my-skill"));
        // A compaction bumps the epoch; the stored row keeps the old one.
        note_compaction(a);
        assert!(!seen_since_compaction(a, "my-skill"));
        // Re-reading re-arms at the new epoch.
        mark_seen(a, "my-skill");
        assert!(seen_since_compaction(a, "my-skill"));
    }

    #[test]
    fn compaction_clears_nothing_stamp_inventory_intact() {
        let a = Uuid::new_v4();
        mark_seen(a, "one");
        mark_seen(a, "two");
        note_compaction(a);
        // The #125 stamp's seen-inventory survives.
        assert_eq!(
            seen_for_session(a),
            vec!["one".to_string(), "two".to_string()]
        );
        // ...but neither body is "in context" for gate purposes.
        assert!(!seen_since_compaction(a, "one"));
        assert!(!seen_since_compaction(a, "two"));
    }

    #[test]
    fn sessions_are_independent() {
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        mark_seen(a, "sk");
        note_compaction(a);
        // b never compacted: its row stays current.
        mark_seen(b, "sk");
        assert!(seen_since_compaction(b, "sk"));
        assert!(!seen_since_compaction(a, "sk"));
    }
}
