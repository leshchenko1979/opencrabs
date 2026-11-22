//! The `messages` full-text index (#855).
//!
//! `session_search`'s content arms are `content LIKE '%x%'` scans. On the ops
//! store that is 43,756 rows / 607,204,493 content bytes with no index on
//! `content`, measured at ~75 s per query (#855). [`super::database`]'s
//! migration list now carries a `trigram` FTS5 index over that column; this
//! module owns the three things the index needs to be USEFUL and SAFE:
//!
//! 1. [`FtsPrefilter`] — the `AND rowid IN (SELECT rowid FROM messages_fts WHERE
//!    messages_fts MATCH ?)` clause the query arms splice in. Built in ONE place
//!    so the repository and the Track A eval harness cannot grow two spellings
//!    of it (#911's law: a harness that drifts measures a ranker the binary does
//!    not ship).
//! 2. [`is_ready`] — the gate. The index is a PREFILTER, never the authority:
//!    the `LIKE` arms still decide membership, so the result set stays
//!    byte-identical to the pre-#855 query. Narrowing a scan with a partially
//!    built index, though, would silently DROP matches, so every prefilter is
//!    conditional on the backfill having finished.
//! 3. [`backfill`] / [`spawn_backfill`] — the 607 MB index build, off the
//!    startup path, in chunks, resumable across restarts.
//!
//! # Why the index is a prefilter and not the answer
//!
//! The shipped ranker and the #941 eval measure substring semantics
//! (`content LIKE '%term%'`, mid-word included). The `trigram` tokenizer
//! preserves exactly that, so the FTS arm could in principle BE the query — but
//! keeping `LIKE` as the authority means the fix cannot change a single result
//! row, only the work spent finding it. It also makes the index disposable: a
//! missing, partial or stale index degrades to today's scan instead of returning
//! wrong rows.
//!
//! # Tokens shorter than 3 characters
//!
//! The trigram tokenizer indexes 3-character sequences, so a 1- or 2-character
//! term cannot be expressed as a `MATCH` — it returns ZERO rows rather than
//! erroring, which is the dangerous shape (silent, and it looks like "no
//! matches"). Any such token makes [`FtsPrefilter`] yield `None` for the whole
//! call and the query falls back to the LIKE-only scan. The token is never
//! silently dropped.

use std::collections::HashSet;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use rusqlite::Connection;

use super::database::{Pool, interact_err};

/// The one prefilter clause in the tree. Spliced into the repository's
/// `contiguous_rows`/`tokenized_rows` and into the Track A harness's
/// `search_slice`, so the two legs cannot diverge (#911).
///
/// The single `?` binds the `MATCH` expression built by [`FtsPrefilter`]; the
/// caller must push that parameter in the position the clause occupies.
pub const PREFILTER_CLAUSE: &str =
    "AND rowid IN (SELECT rowid FROM messages_fts WHERE messages_fts MATCH ?)";

/// The shortest term the `trigram` tokenizer can answer.
pub const TRIGRAM_MIN_CHARS: usize = 3;

/// Rows copied per backfill chunk. Each chunk is one write transaction, so this
/// bounds how long a chunk holds the write lock — measured at ~0.5–0.8 s for a
/// 200-row chunk of the live store's size (13.9 KB/row). Bigger chunks build
/// faster (the per-commit cost amortizes) but stall user writes for longer.
pub const CHUNK_ROWS: usize = 200;

/// Pause between chunks, so a chunk cannot be immediately followed by the next
/// one and starve a user write out of the write lock.
const CHUNK_PAUSE: Duration = Duration::from_millis(25);

/// Passes over the table before the backfill gives up and leaves the index
/// marked incomplete. A second pass only happens when concurrent traffic made
/// the first one miss rows; more than two is a symptom, not a schedule.
const MAX_PASSES: u32 = 3;

/// Delay before the detached backfill starts, so boot's own work finishes first.
const SPAWN_DELAY: Duration = Duration::from_secs(5);

/// A ready-to-splice `MATCH` expression.
///
/// Constructing one is the only way to learn that the index can answer for a
/// given query: [`FtsPrefilter::for_phrase`] and [`FtsPrefilter::for_tokens`]
/// return `None` when the index is absent, not yet backfilled, or when any term
/// is too short for the trigram tokenizer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FtsPrefilter {
    /// The FTS5 `MATCH` expression: each term as a quoted phrase, `AND`-joined.
    /// Quoting is what makes `MATCH` mean SUBSTRING (a phrase is a contiguous
    /// run of characters) rather than a bag of tokens.
    pub match_expr: String,
}

impl FtsPrefilter {
    /// For the contiguous arm: the query as ONE substring.
    ///
    /// `content LIKE '%the quick brown%'` and `messages_fts MATCH '"the quick
    /// brown"'` select the same rows — the phrase form is a contiguous match.
    #[must_use]
    pub fn for_phrase(conn: &Connection, phrase: &str) -> Option<Self> {
        let trimmed = phrase.trim();
        if trimmed.is_empty() {
            return None;
        }
        let term = trimmed.to_string();
        Self::build(conn, std::slice::from_ref(&term))
    }

    /// For the tokenized arm: every token somewhere (AND of substrings).
    ///
    /// The tokens are the ones the caller already used to build the `LIKE`
    /// arms, so the prefilter cannot narrow to something the `LIKE` arms would
    /// not have accepted.
    #[must_use]
    pub fn for_tokens(conn: &Connection, tokens: &[String]) -> Option<Self> {
        if tokens.is_empty() {
            return None;
        }
        Self::build(conn, tokens)
    }

    /// `None` unless the index is complete AND every term is long enough.
    fn build(conn: &Connection, terms: &[String]) -> Option<Self> {
        // A term shorter than one trigram cannot be matched — and `MATCH` would
        // answer "no rows" rather than complain, so this check is the only
        // thing standing between a 2-character query and an empty result set.
        if terms.iter().any(|t| t.chars().count() < TRIGRAM_MIN_CHARS) {
            return None;
        }
        if !is_ready(conn) {
            return None;
        }
        let mut expr = String::new();
        for (i, term) in terms.iter().enumerate() {
            if i > 0 {
                expr.push_str(" AND ");
            }
            // Inside a quoted FTS5 phrase everything is literal except `"`,
            // which is escaped by doubling. That is what makes a term carrying
            // `%`, `_`, `-` or `:` safe to pass through — verified on this box.
            expr.push('"');
            expr.push_str(&term.replace('"', "\"\""));
            expr.push('"');
        }
        Some(Self { match_expr: expr })
    }
}

/// True when the FTS index exists and the backfill has finished.
///
/// One row read from a one-row table. A store that has not run the migration —
/// or any test fixture that builds its own `messages` table — answers `false`
/// and every caller falls back to the LIKE-only scan.
#[must_use]
pub fn is_ready(conn: &Connection) -> bool {
    conn.query_row(
        "SELECT complete FROM messages_fts_state WHERE id = 1",
        [],
        |row| row.get::<_, i64>(0),
    )
    .map(|complete| complete != 0)
    .unwrap_or(false)
}

/// True when the migration has run at all (the index and its state row exist).
#[must_use]
pub fn is_installed(conn: &Connection) -> bool {
    conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master \
         WHERE type = 'table' AND name = 'messages_fts_state'",
        [],
        |row| row.get::<_, i64>(0),
    )
    .map(|n| n > 0)
    .unwrap_or(false)
}

/// Mark the index stale so the next backfill rebuilds it from row 0.
///
/// Called after a `VACUUM` that can renumber `messages.rowid` (the one-time
/// `auto_vacuum` conversion in [`super::database`]). SQLite documents that
/// `VACUUM` MAY renumber the rowids of a table without an `INTEGER PRIMARY
/// KEY`, and `messages.id` is TEXT — so the index's key is not guaranteed
/// stable across one. Measured on this box the rowids were preserved, but a
/// silent key change would drop matches, which is the exact failure class this
/// issue exists to remove, so it is not left to chance.
///
/// Returns `true` when the index was present and is now marked stale.
pub fn invalidate(conn: &Connection) -> bool {
    if !is_installed(conn) {
        return false;
    }
    let cleared = conn
        .execute_batch(
            "DELETE FROM messages_fts;
             UPDATE messages_fts_state SET complete = 0, cursor = 0 WHERE id = 1;",
        )
        .is_ok();
    if cleared {
        tracing::info!(
            "messages_fts marked stale after a VACUUM that may have renumbered rowids (#855)"
        );
    }
    cleared
}

/// What a [`backfill`] call did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackfillOutcome {
    /// The migration has not run on this store — nothing to do.
    NotInstalled,
    /// The index was already complete; no rows were read.
    AlreadyComplete,
    /// Rows were copied and the index is now complete.
    Completed { rows: usize },
    /// Rows were copied but the index still does not cover the table. Searches
    /// keep using the LIKE-only scan; the next call retries from row 0.
    Incomplete { rows: usize },
}

/// What a [`backfill`] call measured. Logged by the detached spawn and asserted
/// by the tests.
#[derive(Debug, Clone, Copy)]
pub struct BackfillReport {
    pub outcome: BackfillOutcome,
    pub elapsed: Duration,
    /// `SELECT count(*) FROM messages_fts` — the real indexed row count, which
    /// a contentless-delete table answers from its own index (an
    /// external-content table would answer from the CONTENT table and always
    /// agree, which is why this design does not use one).
    pub indexed_rows: i64,
    /// `SELECT count(*) FROM messages`.
    pub message_rows: i64,
    /// Bytes the index occupies, from `dbstat` when the build carries it.
    pub index_bytes: Option<i64>,
}

impl BackfillReport {
    /// True when the index covers the table.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        matches!(
            self.outcome,
            BackfillOutcome::AlreadyComplete | BackfillOutcome::Completed { .. }
        )
    }
}

/// Paths with a backfill running in this process. Keyed by database path so a
/// multi-profile daemon can backfill each profile's store, while a second
/// request for the SAME store is dropped instead of racing the first. Entries
/// are removed when the run ends, so a later call (after a [`invalidate`]) runs
/// again.
static BACKFILLS_RUNNING: LazyLock<Mutex<HashSet<String>>> =
    LazyLock::new(|| Mutex::new(HashSet::new()));

/// Spawn the backfill as a detached task, off the startup critical path.
///
/// Returns immediately. Safe to call from more than one boot site: the same
/// store is only backfilled once at a time, and a store whose index is already
/// complete costs one state-row read.
pub fn spawn_backfill(pool: Pool, db_key: impl Into<String>) {
    let key = db_key.into();
    {
        let mut running = BACKFILLS_RUNNING
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !running.insert(key.clone()) {
            tracing::debug!(db = %key, "messages_fts backfill already running for this store");
            return;
        }
    }

    tokio::spawn(async move {
        // Boot's own work — provider creation, channel connects, the first
        // turn — must not compete with a 607 MB index build.
        tokio::time::sleep(SPAWN_DELAY).await;
        match backfill(&pool, CHUNK_ROWS).await {
            Ok(report) => tracing::info!(
                db = %key,
                outcome = ?report.outcome,
                indexed_rows = report.indexed_rows,
                message_rows = report.message_rows,
                index_bytes = ?report.index_bytes,
                elapsed_ms = report.elapsed.as_millis() as u64,
                "messages_fts backfill finished"
            ),
            Err(e) => tracing::warn!(
                db = %key,
                error = %format!("{e:#}"),
                "messages_fts backfill failed — searches keep using the LIKE-only scan"
            ),
        }
        BACKFILLS_RUNNING
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&key);
    });
}

/// Build the index, in chunks, until it covers `messages`.
///
/// Cheap no-op when the index is already complete — one state read plus two
/// `count(*)`s. Safe to call concurrently with user traffic: each chunk is its
/// own write transaction, and the triggers keep the index correct for rows
/// written while the backfill runs (a row inserted at the tail is picked up by
/// the loop; one updated after being copied is replaced by its trigger).
pub async fn backfill(pool: &Pool, chunk_rows: usize) -> Result<BackfillReport> {
    let started = Instant::now();
    let chunk_rows = chunk_rows.max(1);

    let (installed, complete) = read_state(pool).await?;
    if !installed {
        return Ok(BackfillReport {
            outcome: BackfillOutcome::NotInstalled,
            elapsed: started.elapsed(),
            indexed_rows: 0,
            message_rows: 0,
            index_bytes: None,
        });
    }

    let (mut indexed, mut messages) = counts(pool).await?;
    if complete && indexed >= messages {
        return Ok(BackfillReport {
            outcome: BackfillOutcome::AlreadyComplete,
            elapsed: started.elapsed(),
            indexed_rows: indexed,
            message_rows: messages,
            index_bytes: index_bytes(pool).await,
        });
    }
    if complete {
        // The index claims to be complete but does not cover the table — a row
        // was written around the triggers. Rebuild from row 0 rather than
        // trusting the flag.
        tracing::warn!(
            indexed_rows = indexed,
            message_rows = messages,
            "messages_fts claims completeness but is short — rebuilding from row 0 (#855)"
        );
        reset_cursor(pool).await?;
    }

    tracing::info!(
        message_rows = messages,
        indexed_rows = indexed,
        chunk_rows,
        "messages_fts backfill starting"
    );

    let mut copied = 0usize;
    for pass in 1..=MAX_PASSES {
        loop {
            let n = copy_chunk(pool, chunk_rows).await?;
            if n == 0 {
                break;
            }
            copied += n;
            // Hand the write lock back between chunks: a user write queued
            // behind this loop must not wait for the whole 607 MB pass.
            tokio::time::sleep(CHUNK_PAUSE).await;
        }

        let (idx, msgs) = counts(pool).await?;
        indexed = idx;
        messages = msgs;
        if indexed >= messages {
            mark_complete(pool).await?;
            let report = BackfillReport {
                outcome: BackfillOutcome::Completed { rows: copied },
                elapsed: started.elapsed(),
                indexed_rows: indexed,
                message_rows: messages,
                index_bytes: index_bytes(pool).await,
            };
            tracing::info!(
                rows = copied,
                indexed_rows = report.indexed_rows,
                message_rows = report.message_rows,
                index_bytes = ?report.index_bytes,
                elapsed_ms = report.elapsed.as_millis() as u64,
                "messages_fts backfill complete"
            );
            return Ok(report);
        }

        tracing::warn!(
            pass,
            indexed_rows = indexed,
            message_rows = messages,
            "messages_fts backfill pass ended short of the table — restarting from row 0"
        );
        reset_cursor(pool).await?;
    }

    let report = BackfillReport {
        outcome: BackfillOutcome::Incomplete { rows: copied },
        elapsed: started.elapsed(),
        indexed_rows: indexed,
        message_rows: messages,
        index_bytes: index_bytes(pool).await,
    };
    tracing::warn!(
        rows = copied,
        indexed_rows = indexed,
        message_rows = messages,
        elapsed_ms = report.elapsed.as_millis() as u64,
        "messages_fts backfill gave up after {MAX_PASSES} passes — searches stay on the LIKE-only scan"
    );
    Ok(report)
}

/// `(installed, complete)` from the state row.
async fn read_state(pool: &Pool) -> Result<(bool, bool)> {
    let conn = pool.get().await.context("Failed to get connection")?;
    conn.interact(|conn| -> rusqlite::Result<(bool, bool)> {
        let installed = is_installed(conn);
        Ok((installed, installed && is_ready(conn)))
    })
    .await
    .map_err(interact_err)?
    .context("messages_fts state probe")
}

/// `(indexed_rows, message_rows)`.
async fn counts(pool: &Pool) -> Result<(i64, i64)> {
    let conn = pool.get().await.context("Failed to get connection")?;
    conn.interact(|conn| -> rusqlite::Result<(i64, i64)> {
        conn.query_row(
            "SELECT (SELECT count(*) FROM messages_fts), (SELECT count(*) FROM messages)",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
    })
    .await
    .map_err(interact_err)?
    .context("messages_fts row counts")
}

/// Bytes the index occupies, when `dbstat` is compiled in (it is optional).
async fn index_bytes(pool: &Pool) -> Option<i64> {
    let conn = pool.get().await.ok()?;
    conn.interact(|conn| -> rusqlite::Result<Option<i64>> {
        conn.query_row(
            "SELECT sum(pgsize) FROM dbstat WHERE name LIKE 'messages_fts%'",
            [],
            |row| row.get::<_, Option<i64>>(0),
        )
    })
    .await
    .ok()?
    .ok()
    .flatten()
}

/// Copy one chunk, advancing the cursor in the same transaction.
async fn copy_chunk(pool: &Pool, limit: usize) -> Result<usize> {
    let conn = pool.get().await.context("Failed to get connection")?;
    conn.interact(move |conn| -> rusqlite::Result<usize> {
        let cursor: i64 = conn.query_row(
            "SELECT cursor FROM messages_fts_state WHERE id = 1",
            [],
            |row| row.get(0),
        )?;
        let rows: Vec<(i64, String)> = {
            let mut stmt = conn.prepare(
                "SELECT rowid, content FROM messages \
                 WHERE rowid > ?1 ORDER BY rowid LIMIT ?2",
            )?;
            let mapped = stmt.query_map(rusqlite::params![cursor, limit as i64], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })?;
            mapped.collect::<rusqlite::Result<Vec<_>>>()?
        };
        let Some(last) = rows.last().map(|(rowid, _)| *rowid) else {
            return Ok(0);
        };

        let tx = conn.unchecked_transaction()?;
        {
            // `OR REPLACE` makes the copy idempotent: a row the insert trigger
            // already indexed while this pass was running is overwritten with
            // the same text instead of being duplicated (a duplicate insert on
            // a contentless-delete table leaves stale entries behind — measured).
            let mut insert =
                tx.prepare("INSERT OR REPLACE INTO messages_fts(rowid, content) VALUES (?1, ?2)")?;
            for (rowid, content) in &rows {
                insert.execute(rusqlite::params![rowid, content])?;
            }
        }
        tx.execute(
            "UPDATE messages_fts_state SET cursor = ?1 WHERE id = 1",
            rusqlite::params![last],
        )?;
        tx.commit()?;
        Ok(rows.len())
    })
    .await
    .map_err(interact_err)?
    .context("messages_fts backfill chunk")
}

/// Set the flag the query path and the tests read.
async fn mark_complete(pool: &Pool) -> Result<()> {
    let conn = pool.get().await.context("Failed to get connection")?;
    conn.interact(|conn| -> rusqlite::Result<usize> {
        conn.execute(
            "UPDATE messages_fts_state SET complete = 1 WHERE id = 1",
            [],
        )
    })
    .await
    .map_err(interact_err)?
    .context("messages_fts mark complete")?;
    Ok(())
}

/// Drop the flag and rewind the cursor.
async fn reset_cursor(pool: &Pool) -> Result<()> {
    let conn = pool.get().await.context("Failed to get connection")?;
    conn.interact(|conn| -> rusqlite::Result<usize> {
        conn.execute(
            "UPDATE messages_fts_state SET complete = 0, cursor = 0 WHERE id = 1",
            [],
        )
    })
    .await
    .map_err(interact_err)?
    .context("messages_fts reset cursor")?;
    Ok(())
}
