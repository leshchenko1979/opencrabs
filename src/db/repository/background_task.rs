//! Background Task Repository
//!
//! Persists in-flight detached commands so a restart can account for them.
//!
//! A long command runs detached and the turn ends immediately, on the promise
//! that the session resumes when the command finishes. That promise used to
//! live only in an in-memory map, so killing the process dropped both the
//! child and the record of it: the session never continued and nothing
//! explained why (#763).
//!
//! A row exists only while the command is believed to be running. Startup
//! treats every surviving row as interrupted, because the process that owned
//! the child is gone with it.

use crate::db::Pool;
use crate::db::database::interact_err;
use anyhow::{Context, Result};
use rusqlite::params;
use uuid::Uuid;

/// One unit of detached work that was running when the row was written.
///
/// `id` is the caller's own handle — a command's task id or a sub-agent's
/// 8-char agent id — so it is deliberately NOT forced through a `Uuid`: the
/// recovery path only needs it back to clear the exact row it reported.
/// `kind` separates the two reporters (`command` / `agent`); agents carry
/// their task prompt in `command`.
#[derive(Debug, Clone)]
pub struct BackgroundTaskRow {
    pub id: String,
    pub session_id: Uuid,
    pub label: String,
    pub command: String,
    pub started_at: i64,
    pub kind: String,
}

/// `kind` values. Kept here — next to the column — instead of a shared enum:
/// the recovery layer maps them onto its own report framing, and the DB layer
/// stays free of brain-service imports.
pub const KIND_COMMAND: &str = "command";
pub const KIND_AGENT: &str = "agent";

#[derive(Clone)]
pub struct BackgroundTaskRepository {
    pool: Pool,
}

impl BackgroundTaskRepository {
    pub fn new(pool: Pool) -> Self {
        Self { pool }
    }

    /// Record a unit of detached work as running. `id` is the caller's handle
    /// for it, so the matching [`Self::clear`] removes this exact row rather
    /// than guessing by label when two identical units run at once. `kind` is
    /// [`KIND_COMMAND`] or [`KIND_AGENT`].
    pub async fn record(
        &self,
        id: &str,
        session_id: Uuid,
        label: &str,
        command: &str,
        cwd: &str,
        kind: &str,
    ) -> Result<()> {
        let (id, session_id) = (id.to_string(), session_id.to_string());
        let (label, command, cwd) = (label.to_string(), command.to_string(), cwd.to_string());
        let kind = kind.to_string();
        self.pool
            .get()
            .await
            .context("Failed to get connection")?
            .interact(move |conn| {
                conn.execute(
                    "INSERT INTO background_tasks \
                     (id, session_id, label, command, cwd, started_at, kind) \
                     VALUES (?1, ?2, ?3, ?4, ?5, strftime('%s','now'), ?6)",
                    params![id, session_id, label, command, cwd, kind],
                )
            })
            .await
            .map_err(interact_err)?
            .context("Failed to record background task")?;
        Ok(())
    }

    /// Drop the row for a unit of work that finished normally.
    pub async fn clear(&self, id: impl ToString) -> Result<()> {
        let id = id.to_string();
        self.pool
            .get()
            .await
            .context("Failed to get connection")?
            .interact(move |conn| {
                conn.execute("DELETE FROM background_tasks WHERE id = ?1", params![id])
            })
            .await
            .map_err(interact_err)?
            .context("Failed to clear background task")?;
        Ok(())
    }

    /// Every surviving row, oldest first.
    ///
    /// Called once at startup: anything still here belonged to a process that
    /// no longer exists, so each row is an interrupted unit of detached work
    /// (command or agent, per `kind`).
    pub async fn all(&self) -> Result<Vec<BackgroundTaskRow>> {
        let rows = self
            .pool
            .get()
            .await
            .context("Failed to get connection")?
            .interact(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT id, session_id, label, command, started_at, kind \
                     FROM background_tasks ORDER BY started_at ASC",
                )?;
                let mapped = stmt
                    .query_map([], |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, String>(2)?,
                            row.get::<_, String>(3)?,
                            row.get::<_, i64>(4)?,
                            row.get::<_, String>(5)?,
                        ))
                    })?
                    .collect::<std::result::Result<Vec<_>, _>>()?;
                Ok::<_, rusqlite::Error>(mapped)
            })
            .await
            .map_err(interact_err)?
            .context("Failed to list background tasks")?;

        Ok(rows
            .into_iter()
            .filter_map(|(id, session_id, label, command, started_at, kind)| {
                // A row whose session id no longer parses is corrupt, not
                // fatal: skip it rather than failing startup over one unusable
                // record. `id` is opaque (commands use uuids, agents 8-char
                // handles) so it passes through unparsed.
                Some(BackgroundTaskRow {
                    id,
                    session_id: Uuid::parse_str(&session_id).ok()?,
                    label,
                    command,
                    started_at,
                    kind,
                })
            })
            .collect())
    }

    /// Remove every row. Used after startup has accounted for them.
    pub async fn clear_all(&self) -> Result<()> {
        self.pool
            .get()
            .await
            .context("Failed to get connection")?
            .interact(move |conn| conn.execute("DELETE FROM background_tasks", []))
            .await
            .map_err(interact_err)?
            .context("Failed to clear background tasks")?;
        Ok(())
    }
}
