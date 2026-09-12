//! Session Seen-Skills Repository (issue #138)
//!
//! Durable backing store for the in-memory `seen_skills` registry
//! (`src/brain/tools/seen_skills.rs`). The registry is the hot path — every
//! `mark_seen` writes a row here, and daemon boot hydrates the registry from
//! this table so the post-compaction skill inventory stamp (#125/#131)
//! survives restarts and rebuilds.
//!
//! Failure mode is deliberately soft (acceptance 5, #138): callers treat any
//! DB error as a WARN + continue — the in-memory registry keeps functioning,
//! a stamp just loses restart durability. No panic paths.

use crate::db::database::interact_err;
use crate::db::Pool;
use anyhow::{Context, Result};
use rusqlite::params;
use uuid::Uuid;

#[derive(Clone)]
pub struct SessionSkillsRepository {
    pool: Pool,
}

impl SessionSkillsRepository {
    pub fn new(pool: Pool) -> Self {
        Self { pool }
    }

    /// Record that `session_id` consumed skill `slug` at the given
    /// compaction `epoch`. Upsert — the (session_id, slug) pair is the
    /// primary key, so repeats refresh `seen_at` AND `epoch` (issue #150:
    /// the gate compares stored epoch against the session's current epoch,
    /// so a stale row must always be updatable to the new epoch).
    pub async fn record(&self, session_id: Uuid, slug: &str, epoch: u64) -> Result<()> {
        let sid = session_id.to_string();
        let slug = slug.to_string();
        let epoch = epoch as i64;
        self.pool
            .get()
            .await
            .context("Failed to get connection")?
            .interact(move |conn| {
                conn.execute(
                    "INSERT INTO session_seen_skills (session_id, slug, epoch) VALUES (?1, ?2, ?3) \
                     ON CONFLICT(session_id, slug) DO UPDATE \
                     SET seen_at = strftime('%s', 'now'), epoch = excluded.epoch",
                    params![sid, slug, epoch],
                )
            })
            .await
            .map_err(interact_err)?
            .context("Failed to record seen skill")?;
        Ok(())
    }

    /// All (session_id, slug, epoch, active) rows — the boot-hydrate feed for
    /// BOTH registries (#138 part 2: the seen set and the active set hydrate
    /// from this one query). Epoch is `None` for pre-feature rows (treated as
    /// 0 by the hydrate); `active` is false for every row that predates the
    /// active flag. Bounded by the stamp's own cleanup: rows for sessions
    /// deleted by normal session pruning are removed by
    /// [`Self::prune_missing_sessions`].
    pub async fn all(&self) -> Result<Vec<(Uuid, String, Option<i64>, bool)>> {
        let rows = self
            .pool
            .get()
            .await
            .context("Failed to get connection")?
            .interact(move |conn| {
                let mut stmt = conn
                    .prepare("SELECT session_id, slug, epoch, active FROM session_seen_skills")?;
                let mapped = stmt.query_map([], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, Option<i64>>(2)?,
                        r.get::<_, i64>(3)? != 0,
                    ))
                })?;
                mapped.collect::<std::result::Result<Vec<(String, String, Option<i64>, bool)>, _>>()
            })
            .await
            .map_err(interact_err)?
            .context("Failed to read seen skills")?;
        Ok(rows
            .into_iter()
            .filter_map(|(sid, slug, epoch, active)| {
                Uuid::parse_str(&sid)
                    .ok()
                    .map(|id| (id, slug, epoch, active))
            })
            .collect())
    }

    /// Flip the ACTIVE flag for one (session, slug) row (#138 part 2).
    ///
    /// This is the durability layer for the two consumers that used to read an
    /// in-memory-only map — the per-turn body re-injection and the compaction
    /// inventory stamp — so both survive a daemon restart.
    ///
    /// Activation UPSERTS: the row may not exist yet when the active path runs
    /// before the seen path ever did. The insert deliberately carries only
    /// `active` — `seen_at` takes its column default and `epoch` stays NULL
    /// (epoch 0 semantics) — and on conflict ONLY `active` is written, so a
    /// later `record()` (which writes epoch/seen_at) is never clobbered by a
    /// re-activation.
    ///
    /// Deactivation is a plain UPDATE, deliberately NOT an upsert: the discard
    /// path deletes the row outright, and an upserting deactivate would
    /// resurrect it as a phantom seen-only row. It matches the legacy
    /// sigil-prefixed spelling too, exactly like [`Self::delete_skill`].
    pub async fn set_active(&self, session_id: Uuid, slug: &str, active: bool) -> Result<()> {
        let sid = session_id.to_string();
        let slug = slug.to_string();
        self.pool
            .get()
            .await
            .context("Failed to get connection")?
            .interact(move |conn| {
                if active {
                    conn.execute(
                        "INSERT INTO session_seen_skills (session_id, slug, active) VALUES (?1, ?2, 1) \
                         ON CONFLICT(session_id, slug) DO UPDATE SET active = 1",
                        params![sid, slug],
                    )
                } else {
                    conn.execute(
                        "UPDATE session_seen_skills SET active = 0 \
                         WHERE session_id = ?1 AND (slug = ?2 OR slug = '/' || ?2)",
                        params![sid, slug],
                    )
                }
            })
            .await
            .map_err(interact_err)?
            .context("Failed to set active flag")?;
        Ok(())
    }

    /// Drop rows whose session no longer exists (normal session pruning).
    /// Called at boot alongside the hydrate; a failure is soft (WARN at the
    /// call site) — pruning is hygiene, not correctness.
    pub async fn prune_missing_sessions(&self) -> Result<u64> {
        let n = self
            .pool
            .get()
            .await
            .context("Failed to get connection")?
            .interact(move |conn| {
                conn.execute(
                    "DELETE FROM session_seen_skills \
                     WHERE session_id NOT IN (SELECT id FROM sessions)",
                    [],
                )
            })
            .await
            .map_err(interact_err)?
            .context("Failed to prune seen skills")?;
        Ok(n as u64)
    }

    /// Delete a single seen-skill record for a session.
    ///
    /// Matches the bare slug **and** a legacy sigil-prefixed row (#179): before
    /// slugs were canonicalised the slash-command path persisted `/foo` while
    /// every other path persisted `foo`, so a stale row can outlive the fix.
    /// Clearing both forms on the first discard retires those without a
    /// migration — `'/' || ?2` is a no-op match when the row is already bare.
    pub async fn delete_skill(&self, session_id: Uuid, slug: &str) -> Result<()> {
        let sid = session_id.to_string();
        let slug = slug.to_string();
        self.pool
            .get()
            .await
            .context("Failed to get connection")?
            .interact(move |conn| {
                conn.execute(
                    "DELETE FROM session_seen_skills \
                     WHERE session_id = ?1 AND (slug = ?2 OR slug = '/' || ?2)",
                    rusqlite::params![sid, slug],
                )
            })
            .await
            .map_err(interact_err)?
            .context("Failed to delete seen skill")?;
        Ok(())
    }
}
