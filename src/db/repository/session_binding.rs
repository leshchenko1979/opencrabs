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
    /// Timestamp (unix seconds) when a tap-initiated turn was opened (#200).
    /// Set when a button tap begins a turn, cleared when the turn completes.
    /// `None` indicates no turn is actively open from a button tap.
    pub turn_open_at: Option<i64>,
    /// What this session is waiting on, when it ended its turn on an EXTERNAL
    /// completion (#344) — a CI run, a peer lane, an owner reply. Free-form so
    /// a new await source never needs a migration. `None` means not awaiting.
    pub await_kind: Option<String>,
    /// The handle for [`Self::await_kind`] — a run id, a session uuid, an issue
    /// number. `None` is legal for a wait that has no identifier.
    pub await_ref: Option<String>,
    /// Timestamp (unix seconds) the wait began (#344). Doubles as the
    /// "is awaiting" predicate (`IS NOT NULL`) and as the ordering key.
    pub await_at: Option<i64>,
}

impl SessionBinding {
    /// Whether this binding carries a live await record (#344).
    pub fn is_awaiting(&self) -> bool {
        self.await_at.is_some()
    }
}

/// Reads and writes [`SessionBinding`] rows.
#[derive(Clone)]
pub struct SessionBindingRepository {
    pool: Pool,
}

/// Column list shared by every SELECT in this file, so the row shape cannot
/// drift between call sites (#344).
const BINDING_COLUMNS: &str = "b.session_id, b.channel, b.chat_id, b.thread_id, \
                               b.last_origin, b.turn_open_at, \
                               b.await_kind, b.await_ref, b.await_at";

fn map_binding(row: &rusqlite::Row<'_>) -> rusqlite::Result<SessionBinding> {
    Ok(SessionBinding {
        session_id: row.get("session_id")?,
        channel: row.get("channel")?,
        chat_id: row.get("chat_id")?,
        thread_id: row.get("thread_id")?,
        last_origin: row.get("last_origin")?,
        turn_open_at: row.get("turn_open_at")?,
        await_kind: row.get("await_kind")?,
        await_ref: row.get("await_ref")?,
        await_at: row.get("await_at")?,
    })
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
    ///
    /// Both column lists are EXPLICIT and neither names the `await_*` columns
    /// (#344), so re-binding a session that is parked on an external
    /// completion never silently erases its wait.
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
        let origin_str = origin.as_str();
        let is_callback = origin == BindingOrigin::Callback;
        self.pool
            .get()
            .await
            .context("Failed to get connection")?
            .interact(move |conn| {
                if is_callback {
                    conn.execute(
                        "INSERT INTO session_bindings (session_id, channel, chat_id, thread_id, last_origin, turn_open_at) \
                         VALUES (?1, ?2, ?3, ?4, ?5, strftime('%s', 'now')) \
                         ON CONFLICT(session_id) DO UPDATE SET \
                           channel = excluded.channel, \
                           chat_id = excluded.chat_id, \
                           thread_id = excluded.thread_id, \
                           last_origin = excluded.last_origin, \
                           turn_open_at = excluded.turn_open_at, \
                           updated_at = strftime('%s', 'now')",
                        params![session_id, ch, cid, thread_id, origin_str],
                    )
                } else {
                    conn.execute(
                        "INSERT INTO session_bindings (session_id, channel, chat_id, thread_id, last_origin, turn_open_at) \
                         VALUES (?1, ?2, ?3, ?4, ?5, NULL) \
                         ON CONFLICT(session_id) DO UPDATE SET \
                           channel = excluded.channel, \
                           chat_id = excluded.chat_id, \
                           thread_id = excluded.thread_id, \
                           last_origin = excluded.last_origin, \
                           turn_open_at = NULL, \
                           updated_at = strftime('%s', 'now')",
                        params![session_id, ch, cid, thread_id, origin_str],
                    )
                }
            })
            .await
            .map_err(interact_err)?
            .context("Failed to upsert session binding")?;
        Ok(())
    }

    /// Clear `turn_open_at` for a session binding when a turn completes (#200).
    /// Safe no-op if no binding exists or turn_open_at is already NULL.
    pub async fn clear_turn_open_at(&self, session_id: &str) -> Result<()> {
        let sid = session_id.to_string();
        self.pool
            .get()
            .await
            .context("Failed to get connection")?
            .interact(move |conn| {
                conn.execute(
                    "UPDATE session_bindings SET turn_open_at = NULL WHERE session_id = ?1",
                    params![sid],
                )
            })
            .await
            .map_err(interact_err)?
            .context("Failed to clear turn_open_at")?;
        Ok(())
    }

    /// Declare that a session ended its turn waiting on an EXTERNAL
    /// completion (#344) — a CI run, a peer lane, an owner reply.
    ///
    /// Deliberately does NOT touch `updated_at`. That column is the freshness
    /// gate's input (`recent_for_channel`), and the whole point of the await
    /// record is to be readable for a binding the gate has already stopped
    /// considering. Bumping it here would keep the lane artificially fresh and
    /// hide the stall class this column exists to catch.
    ///
    /// Idempotent: re-declaring a wait overwrites the kind/ref and restamps
    /// `await_at`, which is what a lane switching from one CI run to another
    /// wants.
    pub async fn set_await(
        &self,
        session_id: &str,
        kind: &str,
        await_ref: Option<&str>,
    ) -> Result<()> {
        let sid = session_id.to_string();
        let kind = kind.to_string();
        let reference = await_ref.map(str::to_string);
        self.pool
            .get()
            .await
            .context("Failed to get connection")?
            .interact(move |conn| {
                conn.execute(
                    "UPDATE session_bindings \
                     SET await_kind = ?2, await_ref = ?3, await_at = strftime('%s', 'now') \
                     WHERE session_id = ?1",
                    params![sid, kind, reference],
                )
            })
            .await
            .map_err(interact_err)?
            .context("Failed to set await record")?;
        Ok(())
    }

    /// Clear a session's await record — the external completion arrived, or
    /// the wait was abandoned (#344). Safe no-op if no binding exists or the
    /// row is already not awaiting.
    pub async fn clear_await(&self, session_id: &str) -> Result<()> {
        let sid = session_id.to_string();
        self.pool
            .get()
            .await
            .context("Failed to get connection")?
            .interact(move |conn| {
                conn.execute(
                    "UPDATE session_bindings \
                     SET await_kind = NULL, await_ref = NULL, await_at = NULL \
                     WHERE session_id = ?1",
                    params![sid],
                )
            })
            .await
            .map_err(interact_err)?
            .context("Failed to clear await record")?;
        Ok(())
    }

    /// Every binding on one channel that carries an await record (#344),
    /// oldest wait first.
    ///
    /// **No time filter** — this is the OR-path beside `recent_for_channel`'s
    /// freshness predicate, not a widening of it. A lane that parked on an
    /// external completion hours ago is exactly the row this must return; the
    /// INNER JOIN against `sessions` still drops bindings whose session was
    /// deleted, so a dead route is never woken (#1224).
    pub async fn awaiting_for_channel(&self, channel: &str) -> Result<Vec<SessionBinding>> {
        let ch = channel.to_string();
        let sql = format!(
            "SELECT {BINDING_COLUMNS} \
             FROM session_bindings b \
             JOIN sessions s ON s.id = b.session_id \
             WHERE b.channel = ?1 AND b.await_at IS NOT NULL \
             ORDER BY b.await_at ASC"
        );
        let mapped = self
            .pool
            .get()
            .await
            .context("Failed to get connection")?
            .interact(move |conn| {
                conn.prepare(&sql)?
                    .query_map(params![ch], map_binding)?
                    .collect::<std::result::Result<Vec<_>, _>>()
            })
            .await
            .map_err(interact_err)?
            .context("Failed to list awaiting session bindings")?;
        Ok(mapped)
    }

    /// Every binding recorded for one channel, least-recently-changed first.
    /// INNER JOIN against sessions drops bindings whose session was deleted,
    /// so connect-time re-registration never revives dead routes (#1224).
    /// Called once per channel connect to re-register routes (#1224).
    pub async fn all_for_channel(&self, channel: &str) -> Result<Vec<SessionBinding>> {
        let ch = channel.to_string();
        let sql = format!(
            "SELECT {BINDING_COLUMNS} \
             FROM session_bindings b \
             JOIN sessions s ON s.id = b.session_id \
             WHERE b.channel = ?1 \
             ORDER BY b.updated_at ASC"
        );
        let mapped = self
            .pool
            .get()
            .await
            .context("Failed to get connection")?
            .interact(move |conn| {
                conn.prepare(&sql)?
                    .query_map(params![ch], map_binding)?
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
    /// Since #180 the rows also carry `last_origin`, and since #200 `turn_open_at`,
    /// which the classifier uses to tell an interrupted tap-turn from a completed one.
    ///
    /// This gate is UNCHANGED by #344: a session parked on an external
    /// completion is found by [`Self::awaiting_for_channel`] instead, so that
    /// this predicate keeps doing its job for the recently-active case.
    pub async fn recent_for_channel(
        &self,
        channel: &str,
        since_epoch: i64,
    ) -> Result<Vec<SessionBinding>> {
        let ch = channel.to_string();
        let sql = format!(
            "SELECT {BINDING_COLUMNS} \
             FROM session_bindings b \
             JOIN sessions s ON s.id = b.session_id \
             WHERE b.channel = ?1 AND b.updated_at >= ?2 \
             ORDER BY b.updated_at ASC"
        );
        let mapped = self
            .pool
            .get()
            .await
            .context("Failed to get connection")?
            .interact(move |conn| {
                conn.prepare(&sql)?
                    .query_map(params![ch, since_epoch], map_binding)?
                    .collect::<std::result::Result<Vec<_>, _>>()
            })
            .await
            .map_err(interact_err)?
            .context("Failed to list recent session bindings")?;
        Ok(mapped)
    }
}
