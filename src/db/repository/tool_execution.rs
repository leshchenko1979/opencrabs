//! Tool Execution Repository
//!
//! Tracks every tool call for usage analytics (Core Tools card in /usage dashboard).
//! Entries are append-only.

use crate::db::Pool;
use crate::db::database::interact_err;
use crate::db::retry::{write_retry_config, write_with_retry};
use anyhow::{Context, Result};
use rusqlite::params;

/// Aggregated tool usage stats
#[derive(Debug, Clone)]
pub struct ToolUsageStats {
    pub tool_name: String,
    pub call_count: i64,
}

/// Per-tool totals with failure counts, for the Mission Control analytics
/// panel (status `'error'` counts as a failure).
#[derive(Debug, Clone)]
pub struct ToolFailureStats {
    pub tool_name: String,
    pub total: i64,
    pub failures: i64,
}

/// Status of a row journaled BEFORE dispatch and not yet finished.
///
/// The row is opened by [`ToolExecutionRepository::record_started`] and closed
/// by [`ToolExecutionRepository::update_outcome`]. A `started` row surviving a
/// restart is work whose outcome nobody observed.
pub const STATUS_STARTED: &str = "started";

/// Status the boot reconciler writes over a `started` row that outlived its
/// turn: the call was dispatched, the process died before the outcome landed,
/// and whether the effect reached the world is unknown.
pub const STATUS_UNKNOWN: &str = "unknown";

/// A call journaled before dispatch that never reported an outcome.
///
/// `arguments` is the JSON the caller supplied when the row was opened; it is
/// `None` for rows written before the column existed. The resume digest names
/// the call from `tool_name` and shows the arguments when present.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownToolCall {
    pub id: String,
    pub tool_name: String,
    pub arguments: Option<String>,
}

/// Repository for tool execution tracking
#[derive(Clone)]
pub struct ToolExecutionRepository {
    pool: Pool,
}

impl ToolExecutionRepository {
    pub fn new(pool: Pool) -> Self {
        Self { pool }
    }

    /// Record a tool execution.
    ///
    /// Refuses to write rows with an empty `tool_name`. Historically a model
    /// occasionally emitted `tool_use` blocks with no name field — dispatch
    /// errored, but the failure still got recorded with the empty name,
    /// producing a blank row in the usage dashboard. There's nothing
    /// meaningful to record for an unnamed tool; logging the refusal at warn
    /// level is enough to surface upstream model misbehaviour without
    /// polluting the stats.
    ///
    /// `status` states the TOOL-LEVEL outcome and `exit_code` carries what the
    /// process the tool ran returned (#763, #938). The two are separate on
    /// purpose, and the status is derived from the code by
    /// [`tool_outcome_status`]: a clean process (`exit_code == Some(0)`) is a
    /// `success`, a non-zero one is `nonzero` — a run that did its job but did
    /// not report clean — and `error` is reserved for a tool that reported
    /// failure without ever spawning a process (NULL code). Before #763 the code
    /// was folded into `status` and both classes read `error`; between #763 and
    /// #938 a carried code pinned `success` without consulting it.
    #[allow(clippy::too_many_arguments)]
    pub async fn record(
        &self,
        id: &str,
        message_id: &str,
        session_id: &str,
        tool_name: &str,
        status: &str,
        provider: Option<&str>,
        model: Option<&str>,
        duration_ms: Option<i64>,
        exit_code: Option<i32>,
    ) -> Result<()> {
        if is_blank_tool_name(tool_name) {
            tracing::warn!(
                "ToolRepo::record skipped: empty tool_name (id={}, message_id={}, status={})",
                id,
                message_id,
                status
            );
            return Ok(());
        }
        // Defense-in-depth: reject garbage tool names that leak from
        // phantom tool calls (reasoning text + stray XML/JSON fragments).
        // Valid tool names are lowercase alphanumeric + underscores, ≤64 chars.
        if is_malformed_tool_name(tool_name) {
            tracing::warn!(
                "ToolRepo::record skipped: invalid tool_name {:?} (id={}, message_id={}, status={})",
                &tool_name[..tool_name.len().min(80)],
                id,
                message_id,
                status
            );
            return Ok(());
        }
        let id = id.to_string();
        let message_id = message_id.to_string();
        let session_id = session_id.to_string();
        let tool_name = tool_name.to_string();
        let status = status.to_string();
        let provider = provider.map(|s| s.to_string());
        let model = model.map(|s| s.to_string());
        write_with_retry(&self.pool, &write_retry_config(), move |conn| {
            conn.execute(
                "INSERT OR IGNORE INTO tool_executions \
                     (id, message_id, session_id, tool_name, status, provider, model, duration_ms, \
                      exit_code) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![
                    id,
                    message_id,
                    session_id,
                    tool_name,
                    status,
                    provider,
                    model,
                    duration_ms,
                    exit_code
                ],
            )
        })
        .await
        .context("Failed to record tool execution")?;
        Ok(())
    }

    /// Open the journal row for a side-effecting call BEFORE it is dispatched.
    ///
    /// This is the method the whole safe-auto-resume card hangs on. [`record`]
    /// is written in a spawned task AFTER the tool returns, so a turn killed
    /// mid-call leaves no row at all — the one case a journal exists for. This
    /// row is written first, `status = 'started'`, and the outcome is written
    /// over it by [`update_outcome`] when the call returns. A `started` row
    /// that survives the process is therefore a call nobody saw finish.
    ///
    /// `arguments` is the call's JSON as the model emitted it; the resume
    /// digest shows it so the resumed turn can tell a `git push` from a
    /// `git status` without guessing. `turn_id` groups the calls of one turn
    /// and is also persisted on the `pending_requests` row, which is how boot
    /// joins an interrupted turn to its journal.
    ///
    /// `INSERT OR IGNORE` on purpose: journaling the same call id twice (a
    /// retried dispatch, a replayed turn) must not clobber the row or double
    /// it. The row is finished by an `UPDATE`, never by a second insert.
    ///
    /// Callers journal only `Mutating`/`Destructive` calls; read-only work is
    /// deliberately not written (a DB write per read buys no safety).
    pub async fn record_started(
        &self,
        id: &str,
        message_id: &str,
        session_id: &str,
        turn_id: &str,
        tool_name: &str,
        arguments: &str,
    ) -> Result<()> {
        if is_blank_tool_name(tool_name) {
            tracing::warn!(
                "ToolRepo::record_started skipped: empty tool_name (id={}, message_id={}, turn_id={})",
                id,
                message_id,
                turn_id
            );
            return Ok(());
        }
        if is_malformed_tool_name(tool_name) {
            tracing::warn!(
                "ToolRepo::record_started skipped: invalid tool_name {:?} (id={}, message_id={}, turn_id={})",
                &tool_name[..tool_name.len().min(80)],
                id,
                message_id,
                turn_id
            );
            return Ok(());
        }
        let id = id.to_string();
        let message_id = message_id.to_string();
        let session_id = session_id.to_string();
        let turn_id = turn_id.to_string();
        let tool_name = tool_name.to_string();
        let arguments = arguments.to_string();
        write_with_retry(&self.pool, &write_retry_config(), move |conn| {
            conn.execute(
                "INSERT OR IGNORE INTO tool_executions \
                     (id, message_id, session_id, turn_id, tool_name, arguments, status) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    id,
                    message_id,
                    session_id,
                    turn_id,
                    tool_name,
                    arguments,
                    STATUS_STARTED
                ],
            )
        })
        .await
        .context("Failed to journal a started tool execution")?;
        Ok(())
    }

    /// The calls of `turn_id` that were started and never reported an outcome.
    ///
    /// Both statuses count as unknown: `started` is a row the boot reconciler
    /// has not reached yet, `unknown` is one it has already stamped. A resumed
    /// turn must treat both the same — the call reached dispatch and its
    /// effect is unobserved — so the read does not depend on whether the
    /// reconciler ran first.
    ///
    /// Ordered by `created_at` so the digest lists the calls in the order the
    /// interrupted turn attempted them. Rows finished through [`update_outcome`]
    /// (`success`, `nonzero`, `error`) are excluded: those have an answer.
    pub async fn unknown_for_turn(&self, turn_id: &str) -> Result<Vec<UnknownToolCall>> {
        let turn_id = turn_id.to_string();
        self.pool
            .get()
            .await
            .context("Failed to get connection")?
            .interact(move |conn| {
                let mut stmt = conn.prepare_cached(
                    "SELECT id, tool_name, arguments FROM tool_executions \
                     WHERE turn_id = ?1 AND status IN (?2, ?3) \
                     ORDER BY created_at ASC, id ASC",
                )?;
                let rows =
                    stmt.query_map(params![turn_id, STATUS_STARTED, STATUS_UNKNOWN], |r| {
                        Ok(UnknownToolCall {
                            id: r.get(0)?,
                            tool_name: r.get(1)?,
                            arguments: r.get(2)?,
                        })
                    })?;
                rows.collect::<std::result::Result<Vec<_>, _>>()
            })
            .await
            .map_err(interact_err)?
            .context("Failed to read a turn's unknown calls")
    }

    /// Delete unresolved journal rows older than `cutoff_epoch`.
    ///
    /// Only `started`/`unknown` rows are eligible. Completed rows are analytics
    /// data and age out with their messages through the existing retention
    /// prune ([`MessageRepository::prune_older_than`], which already deletes
    /// `tool_executions` by `message_id`); deleting them here would be a second
    /// retention policy on the same table. This sweep is the floor for the
    /// journal's OWN debris: a turn whose message is kept forever must not keep
    /// an unresolvable `unknown` row forever.
    ///
    /// Returns the number of rows deleted.
    ///
    /// [`MessageRepository::prune_older_than`]: crate::db::repository::MessageRepository::prune_older_than
    pub async fn sweep_older_than(&self, cutoff_epoch: i64) -> Result<usize> {
        write_with_retry(&self.pool, &write_retry_config(), move |conn| {
            conn.execute(
                "DELETE FROM tool_executions \
                 WHERE created_at < ?1 AND status IN (?2, ?3)",
                params![cutoff_epoch, STATUS_STARTED, STATUS_UNKNOWN],
            )
        })
        .await
        .context("Failed to sweep stale turn-journal rows")
    }

    /// Close a journal row opened by [`record_started`] with the call's outcome.
    ///
    /// This is the completion half of the pre-dispatch journal (#974). It is
    /// an UPDATE for the same reason [`update_outcome`] is: the row already
    /// exists, and a second INSERT would either be ignored (leaving the row
    /// reading `started` forever, which the boot reconciler would then report
    /// as an unknown side effect) or double the call.
    ///
    /// Unlike [`update_outcome`] it also writes the columns `record` fills in
    /// for its analytics rows — `provider`, `model`, `duration_ms` — because
    /// the journal row IS this call's analytics row. Without them the Core
    /// Tools card would lose every mutating call the moment journaling shipped.
    ///
    /// Returns the number of rows touched. `0` means the row never landed
    /// (the name guard refused it), and there is nothing to correct.
    pub async fn complete_started(
        &self,
        id: &str,
        status: &str,
        exit_code: Option<i32>,
        provider: Option<&str>,
        model: Option<&str>,
        duration_ms: Option<i64>,
    ) -> Result<usize> {
        let id = id.to_string();
        let status = status.to_string();
        let provider = provider.map(|s| s.to_string());
        let model = model.map(|s| s.to_string());
        write_with_retry(&self.pool, &write_retry_config(), move |conn| {
            conn.execute(
                "UPDATE tool_executions \
                 SET status = ?2, exit_code = ?3, provider = ?4, model = ?5, duration_ms = ?6 \
                 WHERE id = ?1",
                params![id, status, exit_code, provider, model, duration_ms],
            )
        })
        .await
        .context("Failed to close a journaled tool execution")
    }

    /// Drop a journal row whose call never reached a tool (#974).
    ///
    /// A pre-execution miss — an unknown tool name, arguments that fail
    /// validation — is a model error, not a call, and the analytics path
    /// deliberately records nothing for it (#687). A journal row opened
    /// before dispatch must therefore be removed, or it would read `unknown`
    /// at boot and tell a resumed turn that a side effect of unknown state was
    /// attempted. Restricted to `started` so a row that did report an outcome
    /// can never be erased by this path.
    pub async fn discard_started(&self, id: &str) -> Result<usize> {
        let id = id.to_string();
        write_with_retry(&self.pool, &write_retry_config(), move |conn| {
            conn.execute(
                "DELETE FROM tool_executions WHERE id = ?1 AND status = ?2",
                params![id, STATUS_STARTED],
            )
        })
        .await
        .context("Failed to discard a journaled tool execution that never dispatched")
    }

    /// Flip every row still reading `started` to `unknown` (#974).
    ///
    /// Run once at boot, before any interrupted turn is considered for
    /// replay. A row only reads `started` between [`record_started`] and its
    /// completion, and both happen inside one process — so at boot any such
    /// row belongs to the process that just died, and whether its effect
    /// reached the world is unknowable. The row is kept, not deleted: the
    /// whole point is that the resumed turn can see what it started.
    ///
    /// Idempotent: a second boot finds nothing to flip. Returns the number of
    /// rows reconciled.
    pub async fn reconcile_started_to_unknown(&self) -> Result<usize> {
        write_with_retry(&self.pool, &write_retry_config(), move |conn| {
            conn.execute(
                "UPDATE tool_executions SET status = ?2 WHERE status = ?1",
                params![STATUS_STARTED, STATUS_UNKNOWN],
            )
        })
        .await
        .context("Failed to reconcile interrupted journal rows")
    }

    /// Write a detached run's terminal outcome back onto its row (#787).
    ///
    /// Returns the number of rows touched, and `0` is a MEANINGFUL answer: the
    /// row is inserted by `tool_loop` in a spawned task *after* the tool call
    /// returned, so a fast run can finish before its own row exists. The caller
    /// retries on `0`.
    ///
    /// It is also how a journal row opened by [`record_started`] is CLOSED:
    /// the `started` status is replaced by the call's real outcome, which is
    /// what keeps an interrupted call distinguishable from a completed one.
    ///
    /// It is deliberately an UPDATE and not an upsert. `tool_executions`
    /// requires `message_id` and `created_at` NOT NULL, and the completion
    /// site knows neither — an INSERT there would have to invent values, which
    /// is a worse defect than the one this fixes. The status file remains the
    /// record of truth if the row never lands.
    pub async fn update_outcome(
        &self,
        id: &str,
        exit_code: Option<i32>,
        status: &str,
    ) -> Result<usize> {
        let id = id.to_string();
        let status = status.to_string();
        write_with_retry(&self.pool, &write_retry_config(), move |conn| {
            conn.execute(
                "UPDATE tool_executions SET exit_code = ?2, status = ?3 WHERE id = ?1",
                params![id, exit_code, status],
            )
        })
        .await
        .context("Failed to record a detached run's outcome")
    }

    /// Get tool usage stats grouped by tool_name, optionally filtered by time period
    pub async fn stats_by_tool(&self, since_epoch: Option<i64>) -> Result<Vec<ToolUsageStats>> {
        self.pool
            .get()
            .await
            .context("Failed to get connection")?
            .interact(move |conn| {
                // Skip rows with empty tool_name (see ToolRepo::record).
                let (query, param): (String, Vec<Box<dyn rusqlite::types::ToSql>>) =
                    if let Some(since) = since_epoch {
                        (
                            "SELECT tool_name, COUNT(*) as cnt \
                             FROM tool_executions \
                             WHERE created_at >= ?1 AND tool_name <> '' \
                             GROUP BY tool_name \
                             ORDER BY cnt DESC"
                                .to_string(),
                            vec![Box::new(since)],
                        )
                    } else {
                        (
                            "SELECT tool_name, COUNT(*) as cnt \
                             FROM tool_executions \
                             WHERE tool_name <> '' \
                             GROUP BY tool_name \
                             ORDER BY cnt DESC"
                                .to_string(),
                            vec![],
                        )
                    };
                let mut stmt = conn.prepare_cached(&query)?;
                let param_refs: Vec<&dyn rusqlite::types::ToSql> =
                    param.iter().map(|p| p.as_ref()).collect();
                let rows = stmt.query_map(param_refs.as_slice(), |row| {
                    Ok(ToolUsageStats {
                        tool_name: row.get(0)?,
                        call_count: row.get(1)?,
                    })
                })?;
                rows.collect::<std::result::Result<Vec<_>, _>>()
            })
            .await
            .map_err(interact_err)?
            .context("Failed to query tool usage stats")
    }

    /// Per-tool totals and failure counts (`status = 'error'`), optionally
    /// since an epoch. Powers the Mission Control analytics panel's fail-rate
    /// view. Ordered by total calls descending.
    /// Last RSI activity (self_improve / feedback_analyze / rsi_propose
    /// executions) and how many tool events recorded AFTER it (#469): a
    /// stale RSI with a busy ledger is the signal Mission Control surfaces.
    /// `None` last-ts means RSI never ran; events_since then counts all
    /// recorded tool events.
    /// Recent (session_id, tool_name) rows ordered so consecutive rows in a
    /// session are adjacent and in execution order (#504). The caller groups
    /// them into per-session tool sequences for the skill-pattern detector.
    /// Empty tool names are skipped (see `record`).
    pub async fn recent_session_tool_sequences(&self, limit: i64) -> Result<Vec<(String, String)>> {
        self.pool
            .get()
            .await
            .context("Failed to get connection")?
            .interact(move |conn| {
                let mut stmt = conn.prepare_cached(
                    "SELECT session_id, tool_name FROM tool_executions \
                     WHERE tool_name <> '' AND session_id <> '' \
                     ORDER BY session_id, created_at, id LIMIT ?1",
                )?;
                let rows = stmt.query_map(params![limit], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
                })?;
                rows.collect::<std::result::Result<Vec<_>, _>>()
            })
            .await
            .map_err(interact_err)?
            .context("Failed to fetch session tool sequences")
    }

    pub async fn rsi_staleness(&self) -> Result<(Option<i64>, i64)> {
        self.pool
            .get()
            .await
            .context("Failed to get connection")?
            .interact(move |conn| -> rusqlite::Result<(Option<i64>, i64)> {
                let last: Option<i64> = conn.query_row(
                    "SELECT MAX(created_at) FROM tool_executions \
                     WHERE tool_name IN ('self_improve','feedback_analyze','rsi_propose')",
                    [],
                    |r| r.get(0),
                )?;
                let since: i64 = match last {
                    Some(ts) => conn.query_row(
                        "SELECT COUNT(*) FROM tool_executions WHERE created_at > ?1",
                        rusqlite::params![ts],
                        |r| r.get(0),
                    )?,
                    None => {
                        conn.query_row("SELECT COUNT(*) FROM tool_executions", [], |r| r.get(0))?
                    }
                };
                Ok((last, since))
            })
            .await
            .map_err(interact_err)?
            .context("Failed to query RSI staleness")
    }

    pub async fn stats_with_failures(
        &self,
        since_epoch: Option<i64>,
    ) -> Result<Vec<ToolFailureStats>> {
        self.pool
            .get()
            .await
            .context("Failed to get connection")?
            .interact(move |conn| {
                let (query, param): (String, Vec<Box<dyn rusqlite::types::ToSql>>) =
                    if let Some(since) = since_epoch {
                        (
                            "SELECT tool_name, COUNT(*) AS total, \
                                    SUM(CASE WHEN status = 'error' THEN 1 ELSE 0 END) AS fails \
                             FROM tool_executions \
                             WHERE created_at >= ?1 AND tool_name <> '' \
                             GROUP BY tool_name ORDER BY total DESC"
                                .to_string(),
                            vec![Box::new(since)],
                        )
                    } else {
                        (
                            "SELECT tool_name, COUNT(*) AS total, \
                                    SUM(CASE WHEN status = 'error' THEN 1 ELSE 0 END) AS fails \
                             FROM tool_executions \
                             WHERE tool_name <> '' \
                             GROUP BY tool_name ORDER BY total DESC"
                                .to_string(),
                            vec![],
                        )
                    };
                let mut stmt = conn.prepare_cached(&query)?;
                let param_refs: Vec<&dyn rusqlite::types::ToSql> =
                    param.iter().map(|p| p.as_ref()).collect();
                let rows = stmt.query_map(param_refs.as_slice(), |row| {
                    Ok(ToolFailureStats {
                        tool_name: row.get(0)?,
                        total: row.get(1)?,
                        failures: row.get::<_, Option<i64>>(2)?.unwrap_or(0),
                    })
                })?;
                rows.collect::<std::result::Result<Vec<_>, _>>()
            })
            .await
            .map_err(interact_err)?
            .context("Failed to query tool failure stats")
    }
}

/// True for an empty or whitespace-only tool name.
///
/// The name guard has one home because [`ToolExecutionRepository::record`] and
/// [`ToolExecutionRepository::record_started`] must refuse the same names: a
/// call journaled before dispatch for a name the analytics path rejects would
/// create a row that can never be completed meaningfully.
fn is_blank_tool_name(tool_name: &str) -> bool {
    tool_name.trim().is_empty()
}

/// True for a name that is not a real tool name.
///
/// Defense-in-depth against garbage that leaks from phantom tool calls
/// (reasoning text + stray XML/JSON fragments). Valid tool names are lowercase
/// alphanumeric plus underscores, at most 64 bytes.
fn is_malformed_tool_name(tool_name: &str) -> bool {
    tool_name.len() > 64
        || !tool_name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

/// The tool-level outcome for the execution record (#763, #938).
///
/// Single home for the predicate. It used to be restated at five call sites —
/// four in `tool_loop.rs`, one in `parallel_tools.rs` — each writing the same
/// three-way `if`, which is how a rule drifts.
///
/// Three values, and the reason each exists:
///
/// * `success` — a process ran and reported clean (`Some(0)`), or no process
///   ran and the tool reported success. `Some(0)` wins over a disagreeing tool
///   flag: the code is the more specific evidence.
/// * `nonzero` — a process ran and returned non-zero. Not `error` (#763: a
///   deliberate `grep` with no match, a probe's `exit 1`), and not `success`
///   (#938: a killed or failed run is not clean). Its own value, so
///   `status = 'error'` keeps meaning "the tool itself failed" and every
///   aggregate binned by it keeps its meaning.
/// * `error` — no process outcome (`exit_code` NULL) and the tool reported
///   failure. Non-shell behaviour, unchanged since before #763.
///
/// Cutover: **2026-11-21 (UTC)**, the date this predicate first ships. New
/// rows read `nonzero` from that point where they previously read `success`
/// beside a non-zero code. Rows written before it keep their old value —
/// `tool_executions` is append-only and no backfill is performed (rewriting an
/// append-only record would be a worse defect than the mis-bin being fixed).
/// A series counting `success` steps down once, at that date, and is honest
/// after it; a consumer that wants the corrected classification for history
/// reads `exit_code <> 0` rather than the stored status.
///
/// The date lives here rather than in a migration on purpose: a comment-only
/// migration would be an empty statement list (rusqlite_migration builds one
/// statement set per entry), and it would move `MIGRATION_COUNT` for a change
/// that alters no schema. This doc ships WITH the behaviour it describes, so
/// the two cannot drift apart.
pub fn tool_outcome_status(success: bool, exit_code: Option<i32>) -> &'static str {
    match exit_code {
        Some(0) => "success",
        Some(_) => "nonzero",
        None if success => "success",
        None => "error",
    }
}
