//! Session Binding Repository
//!
//! Persists which channel (and chat / forum topic) owns each session so a
//! restart can re-register delivery routes at channel-connect time instead
//! of waiting for the next inbound message (#1224). A session idle at boot
//! has no way to reach ingress: without this record every background-task
//! completion and sub-agent result parks until a human pokes the topic.

use crate::db::{Pool, database::interact_err};
use anyhow::{Context, Result};
use rusqlite::params;

/// What kind of interaction last refreshed a binding (#180).
///
/// The boot classifier consults this to decide whether the topic's last
/// message still carries a completion signal. A bot reply that follows a TEXT
/// message means the turn finished; a bot reply that follows a BUTTON TAP is
/// just the card the button rode in on, so it says nothing about whether the
/// turn completed. Without the distinction a tap-initiated turn killed before
/// its PROCESSING row existed was invisible to both recovery nets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BindingOrigin {
    /// An inbound text message refreshed the binding.
    Text,
    /// An inline button tap refreshed the binding.
    Callback,
}

impl BindingOrigin {
    /// The value persisted in `session_bindings.last_origin`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Callback => "callback",
        }
    }

    /// Parse a stored `last_origin`.
    ///
    /// `NULL` — a row written before this column existed — reads as
    /// [`BindingOrigin::Text`], preserving the pre-#180 classification for
    /// every legacy row. An unrecognised value is treated the same way rather
    /// than guessed at: the conservative reading is the one that keeps the
    /// existing sender heuristic in charge.
    pub fn from_stored(raw: Option<&str>) -> Self {
        match raw {
            Some("callback") => Self::Callback,
            _ => Self::Text,
        }
    }
}

/// One persisted binding: "session S lives on channel C, chat X, topic T".
#[derive(Debug, Clone)]
pub struct SessionBinding {
    pub session_id: String,
    pub channel: String,
    pub chat_id: String,
    pub thread_id: Option<i32>,
    /// Interaction kind that last refreshed this row (#180). `None` for rows
    /// written before the column existed; read it through
    /// [`BindingOrigin::from_stored`] rather than matching on the raw string.
    pub last_origin: Option<String>,
}

/// Reads and writes [`SessionBinding`] rows.
#[derive(Clone)]
pub struct SessionBindingRepository {
    pool: Pool,
}

impl SessionBindingRepository {
    pub fn new(pool: Pool) -> Self {
        Self { pool }
    }

    /// Record or refresh where a session lives. Idempotent per session:
    /// re-binding replaces the previous channel/chat/thread, which is what a
    /// session moving between surfaces needs.
    ///
    /// `origin` records WHAT refreshed it (#180). Both a text message and a
    /// button tap call this; only the text path used to, which is why a tap
    /// never made its session a candidate for boot recovery.
    pub async fn upsert(
        &self,
        session_id: String,
        channel: &str,
        chat_id: &str,
        thread_id: Option<i32>,
        origin: BindingOrigin,
    ) -> Result<()> {
        let ch = channel.to_string();
        let cid = chat_id.to_string();
        let origin = origin.as_str();
        self.pool
            .get()
            .await
            .context("Failed to get connection")?
            .interact(move |conn| {
                conn.execute(
                    "INSERT INTO session_bindings (session_id, channel, chat_id, thread_id, last_origin) \
                     VALUES (?1, ?2, ?3, ?4, ?5) \
                     ON CONFLICT(session_id) DO UPDATE SET \
                       channel = excluded.channel, \
                       chat_id = excluded.chat_id, \
                       thread_id = excluded.thread_id, \
                       last_origin = excluded.last_origin, \
                       updated_at = strftime('%s', 'now')",
                    params![session_id, ch, cid, thread_id, origin],
                )
            })
            .await
            .map_err(interact_err)?
            .context("Failed to upsert session binding")?;
        Ok(())
    }

    /// Every binding recorded for one channel, least-recently-changed first.
    /// INNER JOIN against sessions drops bindings whose session was deleted,
    /// so connect-time re-registration never revives dead routes (#1224).
    /// Called once per channel connect to re-register routes (#1224).
    pub async fn all_for_channel(&self, channel: &str) -> Result<Vec<SessionBinding>> {
        let ch = channel.to_string();
        let mapped = self
            .pool
            .get()
            .await
            .context("Failed to get connection")?
            .interact(move |conn| {
                conn.prepare(
                    "SELECT b.session_id, b.channel, b.chat_id, b.thread_id, b.last_origin \
                     FROM session_bindings b \
                     JOIN sessions s ON s.id = b.session_id \
                     WHERE b.channel = ?1 \
                     ORDER BY b.updated_at ASC",
                )?
                .query_map(params![ch], |row| {
                    Ok(SessionBinding {
                        session_id: row.get("session_id")?,
                        channel: row.get("channel")?,
                        chat_id: row.get("chat_id")?,
                        thread_id: row.get("thread_id")?,
                        last_origin: row.get("last_origin")?,
                    })
                })?
                .collect::<std::result::Result<Vec<_>, _>>()
            })
            .await
            .map_err(interact_err)?
            .context("Failed to list session bindings")?;
        Ok(mapped)
    }

    /// Bindings for one channel whose `updated_at` is at least `since_epoch`
    /// (unix seconds), least-recently-changed first.
    ///
    /// The boot-time wake pass (#1227) uses this to find sessions that were
    /// active around a restart, so it can ping their topics that the platform
    /// survived — without waking every historical session. The INNER JOIN
    /// against `sessions` drops bindings whose session was deleted, matching
    /// [`Self::all_for_channel`] (#1224).
    ///
    /// Since #180 the rows also carry `last_origin`, which the classifier uses
    /// to tell a completed turn from one whose only evidence is a button card.
    pub async fn recent_for_channel(
        &self,
        channel: &str,
        since_epoch: i64,
    ) -> Result<Vec<SessionBinding>> {
        let ch = channel.to_string();
        let mapped = self
            .pool
            .get()
            .await
            .context("Failed to get connection")?
            .interact(move |conn| {
                conn.prepare(
                    "SELECT b.session_id, b.channel, b.chat_id, b.thread_id, b.last_origin \
                     FROM session_bindings b \
                     JOIN sessions s ON s.id = b.session_id \
                     WHERE b.channel = ?1 AND b.updated_at >= ?2 \
                     ORDER BY b.updated_at ASC",
                )?
                .query_map(params![ch, since_epoch], |row| {
                    Ok(SessionBinding {
                        session_id: row.get("session_id")?,
                        channel: row.get("channel")?,
                        chat_id: row.get("chat_id")?,
                        thread_id: row.get("thread_id")?,
                        last_origin: row.get("last_origin")?,
                    })
                })?
                .collect::<std::result::Result<Vec<_>, _>>()
            })
            .await
            .map_err(interact_err)?
            .context("Failed to list recent session bindings")?;
        Ok(mapped)
    }
}
