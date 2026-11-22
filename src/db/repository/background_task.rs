//! Background Task Repository
//!
//! Persists detached commands so their run can be accounted for — while it is
//! in flight AND after it has ended (#964).
//!
//! A long command runs detached and the turn ends immediately, on the promise
//! that the session resumes when the command finishes. That promise used to
//! live only in an in-memory map, so killing the process dropped both the
//! child and the record of it: the session never continued and nothing
//! explained why (#763).
//!
//! A row is IN FLIGHT while its `status` is `NULL` or `running`. Startup treats
//! every such surviving row as interrupted, because the process that owned the
//! child is gone with it. At completion the row is no longer deleted: it is
//! stamped with its real outcome ([`BackgroundTaskOutcome`]) and kept, so the
//! one record that names the run can also say how the run ended (#964).

use crate::db::Pool;
use crate::db::database::interact_err;
use crate::db::retry::{write_retry_config, write_with_retry};
use anyhow::{Context, Result};
use rusqlite::params;
use uuid::Uuid;

/// A background command that was running when the row was written.
#[derive(Debug, Clone)]
pub struct BackgroundTaskRow {
    pub id: Uuid,
    pub session_id: Uuid,
    pub label: String,
    pub command: String,
    pub started_at: i64,
}

/// The outcome columns a completed row carries (#964).
///
/// Every field is `Option` because the columns are nullable and nothing was
/// backfilled: a row written before the migration keeps all five `NULL`.
/// [`Self::state_label`] is the only sanctioned way to turn that into text —
/// `NULL` renders `unknown`, never `success`, because the whole point of #938
/// was that a defaulted success is a lie.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BackgroundTaskOutcome {
    /// `running` / `success` / `failed` / `interrupted`, or `None` when the
    /// row predates the outcome columns.
    pub status: Option<String>,
    /// The COMMAND's own exit code. `None` when the number in `code` belonged
    /// to the harness instead (signalled, ceiling-killed, never awaited), and
    /// for every pre-migration row.
    pub exit_code: Option<i64>,
    /// Unix seconds of the terminal transition.
    pub completed_at: Option<i64>,
    /// The run's stdout capture, so the row alone locates the output.
    pub stdout_path: Option<String>,
    /// The run's stderr capture.
    pub stderr_path: Option<String>,
}

impl BackgroundTaskOutcome {
    /// The state a reader may report for this row.
    ///
    /// Only a status the writer actually recorded is reported. Anything else —
    /// `NULL`, an unrecognised string — is `unknown`: a reader must never
    /// upgrade a missing outcome into a claim about how the run ended.
    pub fn state_label(&self) -> &'static str {
        match self.status.as_deref() {
            Some("running") => "running",
            Some("success") => "success",
            Some("failed") => "failed",
            Some("interrupted") => "interrupted",
            _ => "unknown",
        }
    }

    /// The row's status when it is TERMINAL (`success`/`failed`/`interrupted`).
    ///
    /// `running` and `NULL` are not terminal: a reader holding one of those
    /// still has to look elsewhere for how the run ended.
    pub fn terminal_label(&self) -> Option<&'static str> {
        match self.state_label() {
            "success" => Some("success"),
            "failed" => Some("failed"),
            "interrupted" => Some("interrupted"),
            _ => None,
        }
    }

    /// True when this row predates the outcome columns (nothing recorded).
    pub fn is_pre_migration(&self) -> bool {
        self.status.is_none()
    }
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
    /// matching outcome write addresses this exact row rather than guessing by
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

    /// Stamp a finished run's real outcome onto its row, keeping the row.
    ///
    /// This replaces the DELETE the completion path used to do (#964). The row
    /// is the record that names the run; deleting it at the moment it acquired
    /// an answer left only the status JSON, whose name a reader had to already
    /// know. `completed_at` defaults to now when the caller does not state it,
    /// so every writer agrees on when the terminal transition happened.
    ///
    /// Returns the number of rows updated. `0` means the row was already gone
    /// (a second completion, or a startup clear that raced it) — logged by the
    /// caller, never treated as an error.
    pub async fn finish(&self, id: Uuid, outcome: BackgroundTaskOutcome) -> Result<usize> {
        let id = id.to_string();
        // Destructured because the retry closure is an `Fn`: it borrows these
        // per attempt and must not move them out on the first one.
        let BackgroundTaskOutcome {
            status,
            exit_code,
            completed_at,
            stdout_path,
            stderr_path,
        } = outcome;
        write_with_retry(&self.pool, &write_retry_config(), move |conn| {
            conn.execute(
                "UPDATE background_tasks SET \
                     status = ?2, exit_code = ?3, \
                     completed_at = COALESCE(?4, strftime('%s','now')), \
                     stdout_path = ?5, stderr_path = ?6 \
                 WHERE id = ?1",
                params![
                    id,
                    status,
                    exit_code,
                    completed_at,
                    stdout_path,
                    stderr_path
                ],
            )
        })
        .await
        .context("Failed to record background task outcome")
    }

    /// The outcome columns of one row, when it exists.
    pub async fn outcome(&self, id: &str) -> Result<Option<BackgroundTaskOutcome>> {
        let id = id.to_string();
        self.pool
            .get()
            .await
            .context("Failed to get connection")?
            .interact(
                move |conn| -> rusqlite::Result<Option<BackgroundTaskOutcome>> {
                    let mut stmt = conn.prepare(
                        "SELECT status, exit_code, completed_at, stdout_path, stderr_path \
                     FROM background_tasks WHERE id = ?1",
                    )?;
                    let mut rows = stmt.query(params![id])?;
                    match rows.next()? {
                        Some(row) => Ok(Some(BackgroundTaskOutcome {
                            status: row.get(0)?,
                            exit_code: row.get(1)?,
                            completed_at: row.get(2)?,
                            stdout_path: row.get(3)?,
                            stderr_path: row.get(4)?,
                        })),
                        None => Ok(None),
                    }
                },
            )
            .await
            .map_err(interact_err)?
            .context("Failed to read background task outcome")
    }

    /// Drop the row for a command whose interruption has been accounted for.
    ///
    /// Not used on completion any more (#964): a completed row keeps its
    /// outcome. This is the startup path, which has already delivered the
    /// interruption into the session and must not re-report it next boot.
    pub async fn clear(&self, id: Uuid) -> Result<()> {
        let id = id.to_string();
        write_with_retry(&self.pool, &write_retry_config(), move |conn| {
            conn.execute("DELETE FROM background_tasks WHERE id = ?1", params![id])
        })
        .await
        .context("Failed to clear background task")?;
        Ok(())
    }

    /// Every IN-FLIGHT row, oldest first.
    ///
    /// Called once at startup: anything still here belonged to a process that
    /// no longer exists, so each row is an interrupted command. Completed rows
    /// are excluded — they carry a terminal `status` and are history, not
    /// orphans; reporting them as interrupted would be the phantom
    /// interruption this table exists to prevent (#763, #964).
    ///
    /// A `NULL` status is treated as in flight: it is a row written before the
    /// outcome columns existed, and its process is as gone as any other.
    pub async fn all(&self) -> Result<Vec<BackgroundTaskRow>> {
        let rows = self
            .pool
            .get()
            .await
            .context("Failed to get connection")?
            .interact(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT id, session_id, label, command, started_at \
                     FROM background_tasks \
                     WHERE status IS NULL OR status = 'running' \
                     ORDER BY started_at ASC",
                )?;
                let mapped = stmt
                    .query_map([], |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, String>(2)?,
                            row.get::<_, String>(3)?,
                            row.get::<_, i64>(4)?,
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
            .filter_map(|(id, session_id, label, command, started_at)| {
                // A row whose ids no longer parse is corrupt, not fatal: skip it
                // rather than failing startup over one unusable record.
                Some(BackgroundTaskRow {
                    id: Uuid::parse_str(&id).ok()?,
                    session_id: Uuid::parse_str(&session_id).ok()?,
                    label,
                    command,
                    started_at,
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
