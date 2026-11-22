//! Message Repository
//!
//! Database operations for messages.

use crate::db::Pool;
use crate::db::database::interact_err;
use crate::db::messages_fts::{FtsPrefilter, PREFILTER_CLAUSE};
use crate::db::models::Message;
use crate::db::retry::{write_retry_config, write_with_retry};
use anyhow::{Context, Result};
use rusqlite::params;
use uuid::Uuid;

/// Repository for message operations
#[derive(Clone)]
pub struct MessageRepository {
    pool: Pool,
}

impl MessageRepository {
    /// Create a new message repository
    pub fn new(pool: Pool) -> Self {
        Self { pool }
    }

    /// Find message by ID
    pub async fn find_by_id(&self, id: Uuid) -> Result<Option<Message>> {
        let id_str = id.to_string();
        self.pool
            .get()
            .await
            .context("Failed to get connection")?
            .interact(move |conn| {
                conn.prepare_cached("SELECT * FROM messages WHERE id = ?1")?
                    .query_row(params![id_str], Message::from_row)
                    .optional()
            })
            .await
            .map_err(interact_err)?
            .context("Failed to find message")
    }

    /// Find all messages for a session
    pub async fn find_by_session(&self, session_id: Uuid) -> Result<Vec<Message>> {
        let sid = session_id.to_string();
        self.pool
            .get()
            .await
            .context("Failed to get connection")?
            .interact(move |conn| {
                let mut stmt = conn.prepare_cached(
                    "SELECT * FROM messages WHERE session_id = ?1 ORDER BY sequence ASC",
                )?;
                let rows = stmt.query_map(params![sid], Message::from_row)?;
                rows.collect::<std::result::Result<Vec<_>, _>>()
            })
            .await
            .map_err(interact_err)?
            .context("Failed to find messages by session")
    }

    /// Find the most recent `limit` messages for a session, oldest-first.
    ///
    /// Unlike [`find_by_session`], this pushes the LIMIT into SQL so reading
    /// the tail of a huge session costs `limit` rows instead of the whole
    /// history. The DESC scan is reversed in Rust to restore chronological
    /// order.
    pub async fn find_recent_by_session(
        &self,
        session_id: Uuid,
        limit: usize,
    ) -> Result<Vec<Message>> {
        let sid = session_id.to_string();
        let limit = limit as i64;
        let mut messages = self
            .pool
            .get()
            .await
            .context("Failed to get connection")?
            .interact(move |conn| {
                let mut stmt = conn.prepare_cached(
                    "SELECT * FROM messages WHERE session_id = ?1 \
                     ORDER BY sequence DESC LIMIT ?2",
                )?;
                let rows = stmt.query_map(params![sid, limit], Message::from_row)?;
                rows.collect::<std::result::Result<Vec<_>, _>>()
            })
            .await
            .map_err(interact_err)?
            .context("Failed to find recent messages by session")?;
        messages.reverse();
        Ok(messages)
    }

    /// Find every message at or after the LAST row whose content starts with
    /// `marker_prefix` (and does not start with `excluded_prefix`), oldest-first.
    ///
    /// This is the SQL form of the boundary trim the agent service used to
    /// perform in memory over a fully loaded history: reading the tail of a
    /// 15,000-message session now costs the tail, not the history (#854).
    ///
    /// This layer deliberately knows nothing about compaction — it takes the
    /// marker vocabulary from the caller, so the predicate has exactly one home
    /// (the agent service) and this stays a generic "rows since the last
    /// anchored-prefix row" primitive.
    ///
    /// The boundary is the last matching row in `(sequence, rowid)` order —
    /// precisely the row `rposition` would pick over [`Self::find_by_session`]'s
    /// output — and the tail is a keyset on that pair rather than a bare
    /// `sequence >= boundary`. The keyset matters: sequence numbers come from
    /// `count(*) + 1` (`MessageService::get_next_sequence`), so a delete in the
    /// middle of a session lets a later insert reuse an earlier sequence, and
    /// `>=` would then pull in rows that sort BEFORE the boundary.
    ///
    /// `instr(content, prefix) = 1` is the anchored test, matching Rust's
    /// `starts_with`. It is used instead of `substr(content, 1, len)` because
    /// `substr` counts CHARACTERS while `str::len` counts BYTES — the delta
    /// banner contains an em dash, so a byte length would truncate the test
    /// mid-prefix and silently match the wrong rows.
    ///
    /// Returns `(tail, total_rows_in_session)`; the total is the whole session's
    /// count, reported so the caller can log how much was skipped.
    pub async fn find_since_last_marker(
        &self,
        session_id: Uuid,
        role: &str,
        marker_prefix: &str,
        excluded_prefix: &str,
    ) -> Result<(Vec<Message>, usize)> {
        let sid = session_id.to_string();
        let role = role.to_string();
        let marker_prefix = marker_prefix.to_string();
        let excluded_prefix = excluded_prefix.to_string();
        self.pool
            .get()
            .await
            .context("Failed to get connection")?
            // The closure's error type is pinned: two crates provide
            // `From<rusqlite::Error>` (the crate's own error and
            // `rusqlite_migration`), so an unannotated `Ok(..)` leaves `E`
            // ambiguous at the tail.
            .interact(move |conn| -> rusqlite::Result<(Vec<Message>, usize)> {
                let boundary: Option<(i64, i64)> = conn
                    .prepare_cached(
                        "SELECT sequence, rowid FROM messages \
                         WHERE session_id = ?1 AND role = ?2 \
                           AND instr(content, ?3) = 1 \
                           AND instr(content, ?4) <> 1 \
                         ORDER BY sequence DESC, rowid DESC LIMIT 1",
                    )?
                    .query_row(params![sid, role, marker_prefix, excluded_prefix], |r| {
                        Ok((r.get(0)?, r.get(1)?))
                    })
                    .optional()?;

                let total: i64 = conn
                    .prepare_cached("SELECT COUNT(*) FROM messages WHERE session_id = ?1")?
                    .query_row(params![sid], |r| r.get(0))?;

                let rows = match boundary {
                    // No boundary: the whole session is the window.
                    None => {
                        let mut stmt = conn.prepare_cached(
                            "SELECT * FROM messages WHERE session_id = ?1 ORDER BY sequence ASC",
                        )?;
                        let rows = stmt.query_map(params![sid], Message::from_row)?;
                        rows.collect::<std::result::Result<Vec<_>, _>>()?
                    }
                    Some((seq, rowid)) => {
                        let mut stmt = conn.prepare_cached(
                            "SELECT * FROM messages WHERE session_id = ?1 \
                               AND (sequence > ?2 OR (sequence = ?2 AND rowid >= ?3)) \
                             ORDER BY sequence ASC",
                        )?;
                        let rows = stmt.query_map(params![sid, seq, rowid], Message::from_row)?;
                        rows.collect::<std::result::Result<Vec<_>, _>>()?
                    }
                };

                Ok((rows, total as usize))
            })
            .await
            .map_err(interact_err)?
            .context("Failed to find messages since the last marker")
    }

    /// Create a new message
    pub async fn create(&self, message: &Message) -> Result<()> {
        let m = message.clone();
        write_with_retry(&self.pool, &write_retry_config(), move |conn| {
            conn.execute(
                "INSERT INTO messages (id, session_id, role, content, sequence,
                                         created_at, token_count, cost, input_tokens,
                                         cache_creation_tokens, cache_read_tokens, thinking,
                                         duration_secs)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
                params![
                    m.id.to_string(),
                    m.session_id.to_string(),
                    m.role,
                    m.content,
                    m.sequence,
                    m.created_at.timestamp(),
                    m.token_count,
                    m.cost,
                    m.input_tokens,
                    m.cache_creation_tokens,
                    m.cache_read_tokens,
                    m.thinking,
                    m.duration_secs,
                ],
            )?;

            // Touch session's updated_at so /sessions sorts by last interaction
            conn.execute(
                "UPDATE sessions SET updated_at = ?1 WHERE id = ?2",
                params![m.created_at.timestamp(), m.session_id.to_string()],
            )
        })
        .await
        .context("Failed to create message")?;

        tracing::debug!(
            "Created message: {} in session: {}",
            message.id,
            message.session_id
        );
        Ok(())
    }

    /// Update an existing message
    pub async fn update(&self, message: &Message) -> Result<()> {
        let m = message.clone();
        write_with_retry(&self.pool, &write_retry_config(), move |conn| {
            conn.execute(
                "UPDATE messages
                     SET content = ?1, token_count = ?2, cost = ?3, input_tokens = ?4,
                         cache_creation_tokens = ?5, cache_read_tokens = ?6, thinking = ?7,
                         duration_secs = ?8
                     WHERE id = ?9",
                params![
                    m.content,
                    m.token_count,
                    m.cost,
                    m.input_tokens,
                    m.cache_creation_tokens,
                    m.cache_read_tokens,
                    m.thinking,
                    m.duration_secs,
                    m.id.to_string()
                ],
            )
        })
        .await
        .context("Failed to update message")?;

        tracing::debug!("Updated message: {}", message.id);
        Ok(())
    }

    /// Fetch the server-reported `input_tokens` from the most recent
    /// assistant message in a session that has one set. Returns `None`
    /// when the session has no assistant responses yet (first turn, or
    /// a fresh session where every assistant row is still streaming).
    /// Used on session load as the authoritative "last known context
    /// size" — no need to tokenize raw content to guess.
    pub async fn last_assistant_input_tokens(&self, session_id: Uuid) -> Result<Option<i32>> {
        let sid = session_id.to_string();
        let tokens = self
            .pool
            .get()
            .await
            .context("Failed to get connection")?
            .interact(move |conn| {
                conn.query_row(
                    "SELECT input_tokens FROM messages \
                     WHERE session_id = ?1 AND role = 'assistant' \
                     AND input_tokens IS NOT NULL \
                     ORDER BY sequence DESC LIMIT 1",
                    params![sid],
                    |r| r.get::<_, Option<i32>>(0),
                )
                .optional()
            })
            .await
            .map_err(interact_err)?
            .context("Failed to query last assistant input_tokens")?;
        Ok(tokens.flatten())
    }

    /// Append content to an existing message (for real-time history persistence)
    pub async fn append_content(&self, id: Uuid, content_to_append: &str) -> Result<()> {
        let id_str = id.to_string();
        let content = content_to_append.to_string();
        write_with_retry(&self.pool, &write_retry_config(), move |conn| {
            conn.execute(
                "UPDATE messages SET content = content || ?1 WHERE id = ?2",
                params![content, id_str],
            )
        })
        .await
        .context("Failed to append to message")?;

        tracing::debug!("Appended content to message: {}", id);
        Ok(())
    }

    /// Set the thinking/reasoning content on an existing message.
    /// Called once per iteration for non-CLI providers to persist reasoning
    /// that was collected during streaming into the `thinking` column.
    pub async fn set_thinking(&self, id: Uuid, thinking: &str) -> Result<()> {
        let id_str = id.to_string();
        let thinking_val = thinking.to_string();
        write_with_retry(&self.pool, &write_retry_config(), move |conn| {
            conn.execute(
                "UPDATE messages SET thinking = ?1 WHERE id = ?2",
                params![thinking_val, id_str],
            )
        })
        .await
        .context("Failed to set thinking on message")?;

        tracing::debug!("Set thinking ({} chars) on message: {}", thinking.len(), id);
        Ok(())
    }

    /// Append to existing thinking content (accumulates reasoning across
    /// multiple tool-loop iterations for non-CLI providers).
    pub async fn append_thinking(&self, id: Uuid, thinking_to_append: &str) -> Result<()> {
        let id_str = id.to_string();
        let thinking = thinking_to_append.to_string();
        write_with_retry(&self.pool, &write_retry_config(), move |conn| {
            // If thinking is NULL, just set it; otherwise concatenate with separator
            conn.execute(
                "UPDATE messages SET thinking = CASE \
                     WHEN thinking IS NULL THEN ?1 \
                     ELSE thinking || char(10) || char(10) || ?1 \
                     END \
                     WHERE id = ?2",
                params![thinking, id_str],
            )
        })
        .await
        .context("Failed to append thinking to message")?;

        tracing::debug!(
            "Appended thinking ({} chars) to message: {}",
            thinking_to_append.len(),
            id
        );
        Ok(())
    }

    /// Delete a message
    pub async fn delete(&self, id: Uuid) -> Result<()> {
        let id_str = id.to_string();
        self.pool
            .get()
            .await
            .context("Failed to get connection")?
            .interact(move |conn| {
                conn.execute("DELETE FROM messages WHERE id = ?1", params![id_str])
            })
            .await
            .map_err(interact_err)?
            .context("Failed to delete message")?;

        tracing::debug!("Deleted message: {}", id);
        Ok(())
    }

    /// List all messages for a session
    pub async fn list_by_session(&self, session_id: Uuid) -> Result<Vec<Message>> {
        self.find_by_session(session_id).await
    }

    /// Remove a trailing unanswered user query and its empty assistant
    /// placeholder as a pair (#730).
    ///
    /// A turn persists the user row and an empty assistant placeholder up front,
    /// then streams content into the placeholder. If the turn is cancelled
    /// before the agent produces anything, the placeholder stays empty and the
    /// user query is left unanswered — and because the empty placeholder is now
    /// the trailing row, a naive "delete the last user row" cleanup misses it,
    /// so the query survives into the next turn's LLM context and duplicates on
    /// resend. This drops the pair from the tail so nothing lingers.
    ///
    /// Only the freshly-created trailing rows are touched: the last row is
    /// deleted iff it is an empty/whitespace-only assistant placeholder, and the
    /// row before it iff it is a user message. A trailing assistant row that
    /// actually holds a reply is left untouched, so a real answered turn is
    /// never disturbed. Returns the number of rows deleted (0, 1, or 2).
    pub async fn delete_trailing_unanswered_pair(&self, session_id: Uuid) -> Result<usize> {
        let mut deleted = 0;

        // Drop the empty assistant placeholder if it is the trailing row.
        if let Some(last) = self.get_last_message(session_id).await?
            && last.role == "assistant"
            && last.content.trim().is_empty()
        {
            self.delete(last.id).await?;
            deleted += 1;
        }

        // Now drop the unanswered user query if it is the (new) trailing row.
        if let Some(last) = self.get_last_message(session_id).await?
            && last.role == "user"
        {
            self.delete(last.id).await?;
            deleted += 1;
        }

        Ok(deleted)
    }

    /// Search messages by text content using SQL LIKE.
    /// `session_ids = None` searches all sessions; `Some(&[..])` restricts to the listed ids.
    /// Returns up to `limit` matches, most recent first. Case-insensitive.
    pub async fn search_by_content(
        &self,
        session_ids: Option<&[Uuid]>,
        query: &str,
        limit: usize,
    ) -> Result<Vec<Message>> {
        let phrase = format!("%{}%", escape_like(query));
        let raw_query = query.to_string();
        let cap = limit as i64;
        let id_strs: Option<Vec<String>> =
            session_ids.map(|ids| ids.iter().map(|i| i.to_string()).collect());

        self.pool
            .get()
            .await
            .context("Failed to get connection")?
            .interact(move |conn| {
                contiguous_rows(conn, id_strs.as_deref(), phrase, &raw_query, cap)
            })
            .await
            .map_err(interact_err)?
            .context("Failed to search messages by content")
    }

    /// Tokenized companion to `search_by_content` (#1626): every
    /// whitespace-separated token must appear somewhere in the content (AND
    /// of substring matches) instead of one contiguous substring. Fetches a
    /// recency-ordered pool sized by
    /// [`crate::relevance::episodic::tokenized_fetch_cap`] — wide enough that
    /// the target is usually IN the pool (#941 measured the pool as a binding
    /// constraint) — then re-ranks it through
    /// [`crate::relevance::episodic::rank`] — query-term proximity span first
    /// (#941), then total token hits, then a token's opening-position signal,
    /// then query-order adjacency, with SQL recency kept inside a full tie
    /// (#911).
    ///
    /// **This arm is no longer a fallback.** The shipped path is
    /// [`Self::search_by_content_union`], which runs this arm ALWAYS, merges
    /// it with the phrase arm, and ranks the union with the same function. A
    /// caller that stops at this method measures half of what ships, which is
    /// the #941 defect; it is kept because the single-arm shape is still the
    /// clearest way to exercise the tokenized SQL and its ranking in
    /// isolation.
    pub async fn search_by_content_tokenized(
        &self,
        session_ids: Option<&[Uuid]>,
        query: &str,
        limit: usize,
    ) -> Result<Vec<Message>> {
        // Dedup exact repeats; cap the arms so a pasted paragraph of a
        // query cannot build a 50-LIKE WHERE clause — 8 tokens already
        // pins hard. `query_tokens` is the shared tokenizer (#911); the
        // scorer below reads `terms`, the same tokens in the query's own
        // order, because adjacency is meaningless on a sorted list.
        let tokens = crate::relevance::episodic::query_tokens(query);
        let terms = crate::relevance::episodic::query_terms(query);
        if tokens.is_empty() {
            return Ok(Vec::new());
        }

        let fetch_cap = crate::relevance::episodic::tokenized_fetch_cap(limit);
        let patterns: Vec<String> = tokens
            .iter()
            .map(|t| format!("%{}%", escape_like(t)))
            .collect();
        let id_strs: Option<Vec<String>> =
            session_ids.map(|ids| ids.iter().map(|i| i.to_string()).collect());

        let rows = self
            .pool
            .get()
            .await
            .context("Failed to get connection")?
            .interact(move |conn| {
                tokenized_rows(conn, id_strs.as_deref(), &tokens, &patterns, fetch_cap)
            })
            .await
            .map_err(interact_err)?
            .context("Failed to search messages by content (tokenized)")?;

        // ONE ranking rule, shared with the Track A harness (#911):
        // `relevance::episodic::rank` scores by query-term proximity span,
        // then by total token hits, then by the row's opening position, then
        // by query-order adjacency, and keeps the SQL recency order inside a
        // full tie. The harness calls the same function, so the eval leg
        // measures the ranker this path ships.
        Ok(
            crate::relevance::episodic::rank(rows, &terms, |m| m.content.as_str())
                .into_iter()
                .take(limit)
                .collect(),
        )
    }

    /// The shipped search: BOTH arms, ONE candidate pool, ONE ranker (#941).
    ///
    /// [`Self::search_by_content`] matches the query as a single contiguous
    /// phrase; [`Self::search_by_content_tokenized`] matches every query word
    /// somewhere. They used to run in sequence — the tokenized arm only when
    /// the phrase arm came back EMPTY — and that short-circuit was the #941
    /// defect: one unrelated message holding the phrase verbatim discarded the
    /// whole tokenized candidate set, including the row the query was actually
    /// about. On the replica slice 16 of 40 positives hit that short-circuit,
    /// and in those the tokenized arm holds the gold in its pool for 10 and in
    /// its own top 10 for 2 — two cases out of forty thrown away by the branch
    /// alone.
    ///
    /// So this method runs BOTH arms always, merges them into one pool
    /// ([`crate::relevance::episodic::merge_arms`]) and ranks that pool with
    /// [`crate::relevance::episodic::rank`] — the same single scorer the Track
    /// A harness calls, so the two cannot diverge (#911). Both arms fetch
    /// `tokenized_fetch_cap(limit)` rows newest-first: one pool rule, not two.
    ///
    /// The returned [`crate::relevance::episodic::Candidate`] keeps the arm
    /// that found each row, so a caller can label a token-only row honestly
    /// instead of claiming a contiguous match that does not exist.
    ///
    /// A single-token query is the one case where the two arms are the SAME
    /// query — an AND of one token is the same substring as the phrase — so
    /// the tokenized scan is skipped (it could only return rows the first scan
    /// already has) and the phrase arm's rows are ranked. Still one ranker;
    /// only the redundant scan is gone.
    pub async fn search_by_content_union(
        &self,
        session_ids: Option<&[Uuid]>,
        query: &str,
        limit: usize,
    ) -> Result<Vec<crate::relevance::episodic::Candidate<Message>>> {
        use crate::relevance::episodic::{
            merge_arms, query_terms, query_tokens, tokenized_fetch_cap,
        };

        let tokens = query_tokens(query);
        if tokens.is_empty() || limit == 0 {
            return Ok(Vec::new());
        }
        let terms = query_terms(query);
        let cap = tokenized_fetch_cap(limit);
        let phrase = format!("%{}%", escape_like(query));
        let raw_query = query.to_string();
        let patterns: Vec<String> = tokens
            .iter()
            .map(|t| format!("%{}%", escape_like(t)))
            .collect();
        let id_strs: Option<Vec<String>> =
            session_ids.map(|ids| ids.iter().map(|i| i.to_string()).collect());

        let (contiguous, tokenized) = self
            .pool
            .get()
            .await
            .context("Failed to get connection")?
            .interact(move |conn| {
                let contiguous =
                    contiguous_rows(conn, id_strs.as_deref(), phrase, &raw_query, cap)?;
                // One token: the AND clause IS the phrase arm's substring, so a
                // second scan can only return rows the first one already has.
                let tokenized = if patterns.len() < 2 {
                    Vec::new()
                } else {
                    tokenized_rows(conn, id_strs.as_deref(), &tokens, &patterns, cap)?
                };
                Ok::<(Vec<Message>, Vec<Message>), rusqlite::Error>((contiguous, tokenized))
            })
            .await
            .map_err(interact_err)?
            .context("Failed to search messages (phrase and tokenized union)")?;

        Ok(merge_arms(
            contiguous,
            tokenized,
            &terms,
            |m: &Message| m.id.to_string(),
            |m: &Message| m.content.as_str(),
        )
        .into_iter()
        .take(limit)
        .collect())
    }

    /// Count messages in a session
    pub async fn count_by_session(&self, session_id: Uuid) -> Result<i64> {
        let sid = session_id.to_string();
        self.pool
            .get()
            .await
            .context("Failed to get connection")?
            .interact(move |conn| {
                conn.query_row(
                    "SELECT COUNT(*) FROM messages WHERE session_id = ?1",
                    params![sid],
                    |row| row.get(0),
                )
            })
            .await
            .map_err(interact_err)?
            .context("Failed to count messages")
    }

    /// Count all messages across all sessions
    /// Get the last message in a session
    pub async fn get_last_message(&self, session_id: Uuid) -> Result<Option<Message>> {
        let sid = session_id.to_string();
        self.pool
            .get()
            .await
            .context("Failed to get connection")?
            .interact(move |conn| {
                conn.prepare_cached(
                    "SELECT * FROM messages WHERE session_id = ?1 ORDER BY sequence DESC LIMIT 1",
                )?
                .query_row(params![sid], Message::from_row)
                .optional()
            })
            .await
            .map_err(interact_err)?
            .context("Failed to get last message")
    }

    /// Get the most recent assistant message in a session.
    ///
    /// Used to recover reply context in Telegram DMs: bot messages live in
    /// this `messages` table (not `channel_messages`), and rich messages
    /// (Bot API 10.1) arrive with empty `text()`/`caption()`, so the
    /// replied-to text must be recovered from here. The incoming user
    /// message is not yet persisted at recovery time, so the last assistant
    /// row is the message the user is replying to in the common case.
    pub async fn get_last_assistant_message(&self, session_id: Uuid) -> Result<Option<Message>> {
        let sid = session_id.to_string();
        self.pool
            .get()
            .await
            .context("Failed to get connection")?
            .interact(move |conn| {
                conn.prepare_cached(
                    "SELECT * FROM messages WHERE session_id = ?1 AND role = 'assistant' \
                     ORDER BY sequence DESC LIMIT 1",
                )?
                .query_row(params![sid], Message::from_row)
                .optional()
            })
            .await
            .map_err(interact_err)?
            .context("Failed to get last assistant message")
    }

    /// Delete all messages in a session
    pub async fn delete_by_session(&self, session_id: Uuid) -> Result<()> {
        let sid = session_id.to_string();
        self.pool
            .get()
            .await
            .context("Failed to get connection")?
            .interact(move |conn| {
                conn.execute("DELETE FROM messages WHERE session_id = ?1", params![sid])
            })
            .await
            .map_err(interact_err)?
            .context("Failed to delete session messages")?;

        tracing::debug!("Deleted all messages for session: {}", session_id);
        Ok(())
    }

    /// Prune messages older than `cutoff_unix` timestamp (#278).
    ///
    /// Also cleans up any associated child records (such as `attachments` or `tool_executions`
    /// referencing the deleted messages) to maintain database hygiene.
    /// Returns the number of pruned message rows.
    ///
    /// Goes through [`write_with_retry`] like every other write in this
    /// repository: a `DELETE` that meets a busy database is retried instead of
    /// failing the sweep outright, and the three statements share one
    /// transaction so a child cleanup can never outlive the rows it orphaned.
    /// `unchecked_transaction` takes `&Connection` — `write_with_retry`'s
    /// closure cannot take `&mut` without rebuilding the pool future on every
    /// attempt, which is the whole reason that helper exists.
    pub async fn prune_older_than(&self, cutoff_unix: i64) -> Result<usize> {
        let pruned = write_with_retry(
            &self.pool,
            &write_retry_config(),
            move |conn| -> rusqlite::Result<usize> {
                let tx = conn.unchecked_transaction()?;
                // Clean up any child rows referencing messages to be deleted (in case FK cascades aren't active)
                let _ = tx.execute(
                    "DELETE FROM attachments WHERE message_id IN (SELECT id FROM messages WHERE created_at < ?1)",
                    params![cutoff_unix],
                );
                let _ = tx.execute(
                    "DELETE FROM tool_executions WHERE message_id IN (SELECT id FROM messages WHERE created_at < ?1)",
                    params![cutoff_unix],
                );
                let count = tx.execute(
                    "DELETE FROM messages WHERE created_at < ?1",
                    params![cutoff_unix],
                )?;
                tx.commit()?;
                Ok(count)
            },
        )
        .await
        .context("Failed to prune expired messages")?;

        if pruned > 0 {
            tracing::info!("Pruned {pruned} message(s) older than timestamp {cutoff_unix}");
        }
        Ok(pruned)
    }
}

/// Extension trait for rusqlite to add `.optional()` to query results
trait OptionalExt<T> {
    fn optional(self) -> rusqlite::Result<Option<T>>;
}

impl<T> OptionalExt<T> for rusqlite::Result<T> {
    fn optional(self) -> rusqlite::Result<Option<T>> {
        match self {
            Ok(v) => Ok(Some(v)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e),
        }
    }
}

/// Escape the `LIKE` metacharacters in `raw` so a query matches literally.
fn escape_like(raw: &str) -> String {
    raw.replace('%', "\\%").replace('_', "\\_")
}

/// The contiguous-phrase arm: rows whose content contains `phrase` (already
/// `%`-wrapped and escaped) verbatim, newest first, at most `cap` rows.
///
/// ONE SQL string for every caller in this file. The shipped search and the
/// union path must not grow two spellings of "contains the query" — a fetch
/// that drifts from the other is the #911 defect one layer down.
///
/// `raw_query` is the query BEFORE `LIKE` escaping: it is what the FTS
/// prefilter is built from ([`FtsPrefilter::for_phrase`]). The prefilter is
/// spliced in as [`PREFILTER_CLAUSE`] — an extra `rowid IN (SELECT rowid FROM
/// messages_fts WHERE messages_fts MATCH ?)` term — and the `LIKE` arm stays
/// the authority on which rows match. The prefilter can only ever be a
/// SUPERSET of the `LIKE` arm (the trigram tokenizer folds more case than
/// `COLLATE NOCASE` does, and a quoted FTS5 phrase is a contiguous character
/// match), so splicing it changes the query PLAN and never the result set.
/// When the index is absent, incomplete, or a term is shorter than one
/// trigram, `fts_arm` is empty and this is the pre-#855 scan unchanged.
fn contiguous_rows(
    conn: &rusqlite::Connection,
    id_strs: Option<&[String]>,
    phrase: String,
    raw_query: &str,
    cap: i64,
) -> rusqlite::Result<Vec<Message>> {
    let prefilter = FtsPrefilter::for_phrase(conn, raw_query);
    let fts_arm = fts_clause(&prefilter);
    match id_strs {
        // An explicit but empty session filter selects nothing — this is NOT
        // the same as `None`, which means "no filter" and scans every session.
        Some([]) => Ok(Vec::new()),
        Some(ids) => {
            let placeholders = vec!["?"; ids.len()].join(",");
            let sql = format!(
                "SELECT * FROM messages \
                 WHERE session_id IN ({placeholders}) {fts_arm}AND content LIKE ? ESCAPE '\\' \
                 COLLATE NOCASE \
                 ORDER BY created_at DESC LIMIT ?"
            );
            let mut stmt = conn.prepare(&sql)?;
            let mut params_vec: Vec<&dyn rusqlite::ToSql> = Vec::with_capacity(ids.len() + 3);
            for id in ids {
                params_vec.push(id);
            }
            push_prefilter(&mut params_vec, &prefilter);
            params_vec.push(&phrase);
            params_vec.push(&cap);
            let rows = stmt.query_map(params_vec.as_slice(), Message::from_row)?;
            rows.collect()
        }
        None => {
            let sql = format!(
                "SELECT * FROM messages \
                 WHERE 1 = 1 {fts_arm}AND content LIKE ? ESCAPE '\\' COLLATE NOCASE \
                 ORDER BY created_at DESC LIMIT ?"
            );
            let mut stmt = conn.prepare_cached(&sql)?;
            let mut params_vec: Vec<&dyn rusqlite::ToSql> = Vec::with_capacity(3);
            push_prefilter(&mut params_vec, &prefilter);
            params_vec.push(&phrase);
            params_vec.push(&cap);
            let rows = stmt.query_map(params_vec.as_slice(), Message::from_row)?;
            rows.collect()
        }
    }
}

/// The clause to splice, plus the trailing space its own `AND` needs. Empty
/// when the index cannot answer this query.
///
/// Every caller writes `... {fts_arm}AND <rest>`, so an empty arm leaves one
/// space between the preceding term and `AND` — which is why this returns no
/// leading space: `{placeholders} AND ...` must not become `{placeholders}AND`
/// (SQLite reads that as an unrecognized token).
fn fts_clause(prefilter: &Option<FtsPrefilter>) -> String {
    if prefilter.is_some() {
        format!("{PREFILTER_CLAUSE} ")
    } else {
        String::new()
    }
}

/// Bind the `MATCH` expression in the position the spliced clause occupies —
/// which is always immediately before the `LIKE` arms, in every caller.
fn push_prefilter<'a>(
    params_vec: &mut Vec<&'a dyn rusqlite::ToSql>,
    prefilter: &'a Option<FtsPrefilter>,
) {
    if let Some(p) = prefilter {
        params_vec.push(&p.match_expr);
    }
}

/// The tokenized arm: rows containing EVERY `patterns` entry (AND of substring
/// matches), newest first, at most `cap` rows — the recency pool the ranker
/// then reorders. Same one-SQL-string rule as [`contiguous_rows`], and the same
/// #855 prefilter: `tokens` are the UNESCAPED terms `patterns` were built from,
/// and [`FtsPrefilter::for_tokens`] turns them into an AND of quoted phrases
/// that is a superset of the AND of `LIKE` arms below it.
fn tokenized_rows(
    conn: &rusqlite::Connection,
    id_strs: Option<&[String]>,
    tokens: &[String],
    patterns: &[String],
    cap: i64,
) -> rusqlite::Result<Vec<Message>> {
    if patterns.is_empty() {
        return Ok(Vec::new());
    }
    let like_arms = vec!["content LIKE ? ESCAPE '\\' COLLATE NOCASE"; patterns.len()].join(" AND ");
    let prefilter = FtsPrefilter::for_tokens(conn, tokens);
    let fts_arm = fts_clause(&prefilter);
    match id_strs {
        // An explicit but empty session filter selects nothing — this is NOT
        // the same as `None`, which means "no filter" and scans every session.
        Some([]) => Ok(Vec::new()),
        Some(ids) => {
            let placeholders = vec!["?"; ids.len()].join(",");
            let sql = format!(
                "SELECT * FROM messages \
                 WHERE session_id IN ({placeholders}) {fts_arm}AND {like_arms} \
                 ORDER BY created_at DESC LIMIT ?"
            );
            let mut stmt = conn.prepare(&sql)?;
            let mut params_vec: Vec<&dyn rusqlite::ToSql> =
                Vec::with_capacity(ids.len() + patterns.len() + 2);
            for id in ids {
                params_vec.push(id);
            }
            push_prefilter(&mut params_vec, &prefilter);
            for p in patterns {
                params_vec.push(p);
            }
            params_vec.push(&cap);
            let rows = stmt.query_map(params_vec.as_slice(), Message::from_row)?;
            rows.collect()
        }
        None => {
            let sql = format!(
                "SELECT * FROM messages \
                 WHERE 1 = 1 {fts_arm}AND {like_arms} \
                 ORDER BY created_at DESC LIMIT ?"
            );
            let mut stmt = conn.prepare(&sql)?;
            let mut params_vec: Vec<&dyn rusqlite::ToSql> = Vec::with_capacity(patterns.len() + 2);
            push_prefilter(&mut params_vec, &prefilter);
            for p in patterns {
                params_vec.push(p);
            }
            params_vec.push(&cap);
            let rows = stmt.query_map(params_vec.as_slice(), Message::from_row)?;
            rows.collect()
        }
    }
}
