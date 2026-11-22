//! Background Task Repository
//!
//! Persists detached commands so a restart can account for them, and keeps
//! each finished row as the record of how its run ended (#964).
//!
//! A long command runs detached and the turn ends immediately, on the promise
//! that the session resumes when the command finishes. That promise used to
//! live only in an in-memory map, so killing the process dropped both the
//! child and the record of it: the session never continued and nothing
//! explained why (#763).
//!
//! A row is inserted with `status = 'running'` before the child is awaited.
//! At the run's terminal write the row is UPDATED in place with `status`,
//! `exit_code`, `completed_at` and both capture paths; it is not deleted, so
//! the row alone answers "how did this end" and locates the output.
//!
//! A row whose `status` is NULL was written before the outcome columns
//! existed and never received a terminal write, so startup treats it as an
//! interrupted command alongside every `running` row. A row whose `status`
//! is `completed` or `killed` records a terminal transition and is NOT an
//! interruption candidate.

use crate::db::Pool;
use crate::db::database::interact_err;
use crate::db::retry::{write_retry_config, write_with_retry};
use anyhow::{Context, Result};
use rusqlite::params;
use uuid::Uuid;

/// One `background_tasks` row.
///
/// The first six fields are what the table has always carried. The outcome
/// fields are NULL until the run's terminal write lands, and stay NULL for a
/// row written before #964 added them.
#[derive(Debug, Clone)]
pub struct BackgroundTaskRow {
    pub id: Uuid,
    pub session_id: Uuid,
    pub label: String,
    pub command: String,
    pub started_at: i64,
    /// `running` while the child is believed live, `completed` when it exited
    /// on its own (any code, success or failure), `killed` when a signal or
    /// the operator's lifetime ceiling ended it. NULL for a row written before
    /// #964 and for a terminal write that measured nothing.
    pub status: Option<String>,
    /// The value the finish path recorded: the process's own code when it
    /// exited, and the shell encoding `128 + signal` when a signal ended it.
    /// NULL when no process was measured.
    pub exit_code: Option<i32>,
    /// Unix seconds of the terminal write, when one happened.
    pub completed_at: Option<i64>,
    /// The run's captured stdout file, as the spawn path laid it out.
    pub stdout_path: Option<String>,
    /// The run's captured stderr file.
    pub stderr_path: Option<String>,
}

#[derive(Clone)]
pub struct BackgroundTaskRepository {
    pool: Pool,
}

impl BackgroundTaskRepository {
    pub fn new(pool: Pool) -> Self {
        Self { pool }
    }

    /// Record a command as running. `id` is the caller's handle for it, so the
    /// matching [`Self::finish`] updates this exact row rather than guessing by
    /// label when two identical commands run at once.
    pub async fn record(
        &self,
        id: Uuid,
        session_id: Uuid,
        label: &str,
        command: &str,
        cwd: &str,
    ) -> Result<()> {
        let (id, session_id) = (id.to_string(), session_id.to_string());
        let (label, command, cwd) = (label.to_string(), command.to_string(), cwd.to_string());
        write_with_retry(&self.pool, &write_retry_config(), move |conn| {
            conn.execute(
                "INSERT INTO background_tasks \
                     (id, session_id, label, command, cwd, started_at, status) \
                     VALUES (?1, ?2, ?3, ?4, ?5, strftime('%s','now'), 'running')",
                params![id, session_id, label, command, cwd],
            )
        })
        .await
        .context("Failed to record background task")?;
        Ok(())
    }

    /// Record how a run ended, on the row [`Self::record`] inserted.
    ///
    /// The row is KEPT: it is now the record of the run, and deleting it would
    /// destroy the answer to "how did this end" that does not depend on the
    /// status file surviving its sweep. `status` is `completed`, `killed`, or
    /// NULL when nothing measured the run; `exit_code` is what the finish path
    /// measured; `completed_at` is this write's time in unix seconds.
    pub async fn finish(
        &self,
        id: Uuid,
        status: Option<&'static str>,
        exit_code: Option<i32>,
        stdout_path: &str,
        stderr_path: &str,
    ) -> Result<()> {
        let run = id.to_string();
        let id = run.clone();
        let stdout_path = stdout_path.to_string();
        let stderr_path = stderr_path.to_string();
        let updated = write_with_retry(&self.pool, &write_retry_config(), move |conn| {
            conn.execute(
                "UPDATE background_tasks \
                    SET status = ?2, exit_code = ?3, \
                        completed_at = strftime('%s','now'), \
                        stdout_path = ?4, stderr_path = ?5 \
                  WHERE id = ?1",
                params![id, status, exit_code, stdout_path, stderr_path],
            )
        })
        .await
        .context("Failed to record background task outcome")?;
        if updated == 0 {
            // The row was inserted before the child was awaited, so a miss is
            // not the ordinary case: say it rather than reporting a write that
            // landed nowhere.
            tracing::warn!(
                target: "background_task",
                "Run {run} finished but no background_tasks row accepted its outcome"
            );
        }
        Ok(())
    }

    /// Drop the row for an interrupted command once a boot has reported it.
    ///
    /// Only interrupted rows are cleared this way. A FINISHED row is never
    /// deleted — [`Self::finish`] updates it in place, and it is the outcome
    /// record (#964).
    pub async fn clear(&self, id: Uuid) -> Result<()> {
        let id = id.to_string();
        write_with_retry(&self.pool, &write_retry_config(), move |conn| {
            conn.execute("DELETE FROM background_tasks WHERE id = ?1", params![id])
        })
        .await
        .context("Failed to clear background task")?;
        Ok(())
    }

    /// Every row a restart would have to report as interrupted: `status` is
    /// `running` (the process that owned the child is gone) or NULL (a row
    /// written before the outcome columns existed, which never received a
    /// terminal write either).
    ///
    /// Finished rows are deliberately absent: they record their own outcome,
    /// and reporting them as interrupted would describe a completed run as
    /// one the restart killed.
    pub async fn interrupted_candidates(&self) -> Result<Vec<BackgroundTaskRow>> {
        let rows = self
            .pool
            .get()
            .await
            .context("Failed to get connection")?
            .interact(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT id, session_id, label, command, started_at, \
                            status, exit_code, completed_at, stdout_path, stderr_path \
                       FROM background_tasks \
                      WHERE status IS NULL OR status = 'running' \
                      ORDER BY started_at ASC",
                )?;
                let mapped = stmt
                    .query_map([], raw_row)?
                    .collect::<std::result::Result<Vec<_>, _>>()?;
                Ok::<_, rusqlite::Error>(mapped)
            })
            .await
            .map_err(interact_err)?
            .context("Failed to list background tasks")?;

        Ok(rows
            .into_iter()
            .filter_map(BackgroundTaskRow::from_raw)
            .collect())
    }

    /// One row by id, whether it is running or finished.
    ///
    /// This is the read a surface uses when a run's status FILE cannot be
    /// read: the row still says what its writer measured (#964).
    pub async fn get(&self, id: Uuid) -> Result<Option<BackgroundTaskRow>> {
        let id = id.to_string();
        let row = self
            .pool
            .get()
            .await
            .context("Failed to get connection")?
            .interact(move |conn| {
                conn.query_row(
                    "SELECT id, session_id, label, command, started_at, \
                            status, exit_code, completed_at, stdout_path, stderr_path \
                       FROM background_tasks WHERE id = ?1",
                    params![id],
                    raw_row,
                )
                .map(Some)
                .or_else(|e| match e {
                    rusqlite::Error::QueryReturnedNoRows => Ok(None),
                    other => Err(other),
                })
            })
            .await
            .map_err(interact_err)?
            .context("Failed to read background task")?;
        Ok(row.and_then(BackgroundTaskRow::from_raw))
    }

    /// The newest `limit` rows for one session, finished or not.
    ///
    /// Feeds the `/tasks` roster's row arm: a run whose status file has been
    /// swept is still listed from its row (#964).
    pub async fn recent_for_session(
        &self,
        session_id: Uuid,
        limit: usize,
    ) -> Result<Vec<BackgroundTaskRow>> {
        let session_id = session_id.to_string();
        let limit = limit as i64;
        let rows = self
            .pool
            .get()
            .await
            .context("Failed to get connection")?
            .interact(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT id, session_id, label, command, started_at, \
                            status, exit_code, completed_at, stdout_path, stderr_path \
                       FROM background_tasks \
                      WHERE session_id = ?1 \
                      ORDER BY started_at DESC LIMIT ?2",
                )?;
                let mapped = stmt
                    .query_map(params![session_id, limit], raw_row)?
                    .collect::<std::result::Result<Vec<_>, _>>()?;
                Ok::<_, rusqlite::Error>(mapped)
            })
            .await
            .map_err(interact_err)?
            .context("Failed to list background tasks for session")?;

        Ok(rows
            .into_iter()
            .filter_map(BackgroundTaskRow::from_raw)
            .collect())
    }

    /// Remove every row a boot would treat as interrupted (`running` or NULL).
    ///
    /// Narrowed from a blanket `DELETE FROM background_tasks` (#964): finished
    /// rows now carry the run's outcome, and this is the one deletion that may
    /// run after accounting without erasing one. The boot path does not call it
    /// — [`crate::brain::agent::service::restart_recovery::report_interrupted`]
    /// clears each row after reporting it — so this is the bulk form of the
    /// same, interrupted-only, disposal.
    pub async fn clear_all(&self) -> Result<()> {
        self.pool
            .get()
            .await
            .context("Failed to get connection")?
            .interact(move |conn| {
                conn.execute(
                    "DELETE FROM background_tasks WHERE status IS NULL OR status = 'running'",
                    [],
                )
            })
            .await
            .map_err(interact_err)?
            .context("Failed to clear background tasks")?;
        Ok(())
    }
}

/// The ten columns every query above selects, in order.
type RawRow = (
    String,
    String,
    String,
    String,
    i64,
    Option<String>,
    Option<i32>,
    Option<i64>,
    Option<String>,
    Option<String>,
);

fn raw_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<RawRow> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
        row.get(6)?,
        row.get(7)?,
        row.get(8)?,
        row.get(9)?,
    ))
}

impl BackgroundTaskRow {
    /// A row whose ids no longer parse is corrupt, not fatal: skip it rather
    /// than failing a boot or a roster read over one unusable record.
    fn from_raw(raw: RawRow) -> Option<Self> {
        let (
            id,
            session_id,
            label,
            command,
            started_at,
            status,
            exit_code,
            completed_at,
            stdout_path,
            stderr_path,
        ) = raw;
        Some(Self {
            id: Uuid::parse_str(&id).ok()?,
            session_id: Uuid::parse_str(&session_id).ok()?,
            label,
            command,
            started_at,
            status,
            exit_code,
            completed_at,
            stdout_path,
            stderr_path,
        })
    }
}
