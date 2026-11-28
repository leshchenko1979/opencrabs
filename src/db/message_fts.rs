//! The `messages.content` full-text index (#855).
//!
//! # Why this exists
//!
//! `session_search` with `operation="search"` and no session scope used to read
//! every message row in the profile database. That is a leading-wildcard `LIKE`
//! over the largest column in the schema, so no index can serve it: on the `ops`
//! profile (144,058 rows, 3.8 GB) it was killed at 30 s and still running at
//! 120 s, and in production one call never returned at all. The fix has two
//! halves — default the scope to the calling session (see
//! `brain::tools::session_search`), and give the explicit `all` path an index.
//! This module is the second half.
//!
//! # External content, never a copy
//!
//! `content='messages'` makes this an EXTERNAL-CONTENT FTS5 table: the index
//! stores tokens and their positions, and reads the text back out of `messages`
//! on demand. It does NOT keep a second copy of `content`, which matters because
//! `messages.content` is the whole 1.26 GiB. A contentless-by-copy variant would
//! roughly double the profile's disk use for nothing.
//!
//! # `content_rowid='rowid'`, NOT `'id'`
//!
//! The issue text proposes `content_rowid='id'`, and it cannot work here:
//! `messages.id` is a TEXT uuid primary key, and FTS5 requires an INTEGER
//! content rowid. Measured on a copy of the `family` profile, an index built
//! with `content_rowid='id'` holds 1,981 entries for 3,520 rows — every uuid
//! casts to 0, so 1,435 rows collapse onto one rowid and the rest are dropped.
//! The implicit `rowid` is the correct key.
//!
//! `VACUUM` is documented as able to renumber rowids of tables without an
//! explicit INTEGER PRIMARY KEY, and the maintenance sweep vacuums this
//! database. Measured on SQLite 3.45.1 (the version `rusqlite 0.40` bundles is
//! 3.53.2, same behaviour) with 5k / 20k / 100k-row tables, deleting a
//! contiguous range and a scattered 3-in-4 range, `VACUUM` preserved every
//! rowid — but "may change" is not "will not change", so
//! [`verify_messages_fts`] runs FTS5's own `integrity-check` after a sweep that
//! vacuumed, and a failed check triggers a rebuild. That turns the documented
//! risk into a self-repair instead of silent mis-pointing.
//!
//! # The `built` flag, and why the triggers are gated on it
//!
//! Building the index over 144k rows / 1.26 GiB is a ONE-TIME cost that must not
//! land on the boot path (the issue asks for exactly this, and #1034 is the
//! standing lesson about expensive work behind a boot). So [`ensure_messages_fts`]
//! only creates the table, the state row and the triggers; the `rebuild` is run
//! by the maintenance sweep ([`rebuild_messages_fts`]).
//!
//! That split creates a trap, and it was measured rather than guessed: with an
//! external-content table, a DELETE or UPDATE trigger that fires against an
//! index that was never built corrupts it — `INSERT INTO f(f) VALUES('delete',
//! rowid, content)` on an empty index makes the next read fail with
//! `database disk image is malformed (11)`, and the corruption survives a later
//! `rebuild` (`integrity-check` still fails). So every trigger carries
//! `WHEN (SELECT built FROM messages_fts_state WHERE id = 1) = 1`, and
//! [`rebuild_messages_fts`] flips the flag to 1 in the SAME transaction as the
//! rebuild. Before that transaction the triggers are no-ops and the index is
//! legitimately empty; after it, they keep it in sync. Verified end to end:
//! delete + update + insert while unbuilt leave a clean empty index, the
//! rebuild produces the right rows, and live trigger sync then works.
//!
//! # What this costs the write path
//!
//! Two costs are inherent to external-content FTS5 and are stated rather than
//! hidden:
//!
//! - `append_content` (`UPDATE messages SET content = content || ?1`) re-indexes
//!   the WHOLE row on every append, because the index must drop the old
//!   document before inserting the new one. An assistant row that grows across a
//!   turn's iterations is re-indexed once per iteration.
//! - A bulk `DELETE FROM messages WHERE ...` (retention prune) fires the delete
//!   trigger per row and each one reads that row's `content` to un-index it, so
//!   a prune that used to be a pure rowid walk now touches the big column. It
//!   stays proportional to the rows removed, and it runs in the 24 h sweep.

use rusqlite::OptionalExtension;

/// Name of the FTS5 index over `messages.content`.
pub(crate) const FTS_TABLE: &str = "messages_fts";

/// One-row table holding the "index has been built" flag.
///
/// A table rather than `PRAGMA user_version` or a marker row in `messages`:
/// the flag has to be readable from inside a trigger's `WHEN` clause, and this
/// is the cheapest object that can be.
pub(crate) const FTS_STATE_TABLE: &str = "messages_fts_state";

/// The three triggers that keep the index in sync.
pub(crate) const FTS_TRIGGERS: [&str; 3] =
    ["messages_fts_ai", "messages_fts_ad", "messages_fts_au"];

/// Create the index, its state row and its triggers when absent. Idempotent.
///
/// Returns `true` when it created anything, which is the [`super::migration_heal`]
/// contract. A healthy database pays three `sqlite_master` probes and one
/// `SELECT` on the state row; nothing is rebuilt here (see the module docs).
pub(crate) fn ensure_messages_fts(conn: &rusqlite::Connection) -> rusqlite::Result<bool> {
    if !super::migration_heal::has_table(conn, "messages")? {
        // No messages table, no index to build. The table itself is older than
        // this heal, so a missing one is not this function's job.
        return Ok(false);
    }

    let mut created = false;

    if !super::migration_heal::has_table(conn, FTS_TABLE)? {
        conn.execute_batch(&format!(
            "CREATE VIRTUAL TABLE {FTS_TABLE} USING fts5(
                 content,
                 content='messages',
                 content_rowid='rowid',
                 tokenize='unicode61'
             );"
        ))?;
        created = true;
    }

    if !super::migration_heal::has_table(conn, FTS_STATE_TABLE)? {
        // Seeded UNBUILT. A database that already has the index but loses this
        // table would be re-marked unbuilt and its triggers would go quiet until
        // the next sweep rebuilds — which is the safe direction: an index that
        // stops being maintained is worse than one that gets rebuilt.
        conn.execute_batch(&format!(
            "CREATE TABLE IF NOT EXISTS {FTS_STATE_TABLE} (
                 id    INTEGER PRIMARY KEY CHECK (id = 1),
                 built INTEGER NOT NULL
             );
             INSERT OR IGNORE INTO {FTS_STATE_TABLE}(id, built) VALUES (1, 0);"
        ))?;
        created = true;
    }

    // Each trigger is probed separately, so a database that has two of the three
    // (an interrupted boot) gets the missing one instead of being skipped whole.
    for trigger in FTS_TRIGGERS {
        if super::migration_heal::has_trigger(conn, trigger)? {
            continue;
        }
        conn.execute_batch(&format!(
            "CREATE TRIGGER IF NOT EXISTS {trigger} {}",
            trigger_body(trigger)
        ))?;
        created = true;
    }

    Ok(created)
}

/// The body of one sync trigger, gated on the `built` flag.
///
/// Built with the consts rather than written out, so a rename of the index or
/// the state table cannot leave a trigger pointing at a name that no longer
/// exists — a trigger that silently matches nothing would stop the index being
/// maintained with no error anywhere.
fn trigger_body(trigger: &str) -> String {
    let gate = format!("WHEN (SELECT built FROM {FTS_STATE_TABLE} WHERE id = 1) = 1");
    match trigger {
        "messages_fts_ai" => format!(
            "AFTER INSERT ON messages
             {gate}
             BEGIN
                 INSERT INTO {FTS_TABLE}(rowid, content) VALUES (new.rowid, new.content);
             END;"
        ),
        "messages_fts_ad" => format!(
            "AFTER DELETE ON messages
             {gate}
             BEGIN
                 INSERT INTO {FTS_TABLE}({FTS_TABLE}, rowid, content)
                 VALUES ('delete', old.rowid, old.content);
             END;"
        ),
        // `UPDATE OF content` so the other columns (cost, tokens, thinking) do
        // not each pay a full re-index of the row.
        "messages_fts_au" => format!(
            "AFTER UPDATE OF content ON messages
             {gate}
             BEGIN
                 INSERT INTO {FTS_TABLE}({FTS_TABLE}, rowid, content)
                 VALUES ('delete', old.rowid, old.content);
                 INSERT INTO {FTS_TABLE}(rowid, content) VALUES (new.rowid, new.content);
             END;"
        ),
        _ => unreachable!("FTS_TRIGGERS lists exactly the three bodies above"),
    }
}

/// Has the index been built?
///
/// `false` for a database that has no index at all, so a caller never has to
/// distinguish "missing" from "present but empty" before deciding to build.
pub(crate) fn messages_fts_built(conn: &rusqlite::Connection) -> rusqlite::Result<bool> {
    if !super::migration_heal::has_table(conn, FTS_TABLE)? {
        return Ok(false);
    }
    if !super::migration_heal::has_table(conn, FTS_STATE_TABLE)? {
        // The index exists but its flag table does not: nothing can be trusted
        // about what is in it, so it is reported unbuilt and the sweep rebuilds.
        return Ok(false);
    }
    let built: Option<i64> = conn
        .query_row(
            &format!("SELECT built FROM {FTS_STATE_TABLE} WHERE id = 1"),
            [],
            |r| r.get(0),
        )
        .optional()?;
    Ok(built == Some(1))
}

/// Rebuild the whole index from `messages` and mark it built, atomically.
///
/// ONE transaction covers the rebuild and the flag: a crash between them would
/// otherwise leave a built index that the triggers believe is unbuilt (or the
/// reverse), and either state is a silent wrong answer.
///
/// The cost is O(all message text) and it is the reason this is not on the boot
/// path: measured on the `family` profile (3,520 rows, 49 MB of content) 3.4 s,
/// and on a 45,000-row / 106 MB synthetic table 6.1 s. The `ops` profile
/// (144k rows, 1.26 GiB) is minutes, which is why it belongs in the sweep.
pub(crate) fn rebuild_messages_fts(conn: &rusqlite::Connection) -> rusqlite::Result<()> {
    conn.execute_batch(&format!(
        "BEGIN IMMEDIATE;
         INSERT INTO {FTS_TABLE}({FTS_TABLE}) VALUES('rebuild');
         UPDATE {FTS_STATE_TABLE} SET built = 1 WHERE id = 1;
         COMMIT;"
    ))
    .inspect_err(|e| {
        // The batch is a transaction, so a failure rolls it back — but say so,
        // because the caller sees only the error and the next boot depends on
        // the flag having stayed at 0.
        tracing::warn!("messages_fts rebuild failed, index stays unbuilt: {e}");
    })
}

/// Verify the index against `messages` with FTS5's own check.
///
/// `true` when the index agrees with the content table. This is what catches a
/// `VACUUM` that renumbered rowids (see the module docs): the check re-reads
/// each document through `content_rowid`, so a rowid pointing at the wrong row
/// fails it instead of quietly returning the wrong message.
///
/// **The `rank` argument must be 1.** `integrity-check` at its default rank 0
/// only validates the index's internal structures — measured: a table whose
/// index was deliberately left pointing at a deleted row passes rank 0 and
/// fails rank 1 with `database disk image is malformed`. Rank 1 is the variant
/// that also compares against the content table, and it is the only one that
/// can see the disagreement this function exists to find.
pub(crate) fn verify_messages_fts(conn: &rusqlite::Connection) -> rusqlite::Result<bool> {
    if !messages_fts_built(conn)? {
        return Ok(false);
    }
    match conn.execute_batch(&format!(
        "INSERT INTO {FTS_TABLE}({FTS_TABLE}, rank) VALUES('integrity-check', 1);"
    )) {
        Ok(()) => Ok(true),
        Err(e) => {
            tracing::warn!("messages_fts integrity check failed: {e}");
            Ok(false)
        }
    }
}

/// Build when absent, and re-verify when present. Returns `true` when it
/// rebuilt (i.e. when the sweep did real work).
///
/// This is the maintenance leg's whole body: unbuilt → rebuild; built →
/// `integrity-check` and rebuild on failure. Both branches are O(content) but
/// the sweep is the one place in the process that is allowed to be.
pub(crate) fn ensure_and_verify_messages_fts(
    conn: &rusqlite::Connection,
) -> rusqlite::Result<bool> {
    ensure_messages_fts(conn)?;
    if !super::migration_heal::has_table(conn, FTS_TABLE)? {
        return Ok(false);
    }
    if !messages_fts_built(conn)? {
        rebuild_messages_fts(conn)?;
        return Ok(true);
    }
    if !verify_messages_fts(conn)? {
        tracing::warn!(
            "messages_fts disagreed with the messages table (a VACUUM that renumbered rowids \
             would do this); rebuilding it"
        );
        rebuild_messages_fts(conn)?;
        return Ok(true);
    }
    Ok(false)
}

/// The FTS5 `MATCH` expression for a user query, or `None` when the query has
/// no usable token.
///
/// Every token is double-quoted, which is what keeps a user's punctuation from
/// being read as FTS5 syntax: `foo-bar` becomes the phrase `"foo-bar"` (two
/// tokens, adjacent) instead of `foo NOT bar`, and a stray `*` or `NEAR(` cannot
/// turn a search into a syntax error. Quoting is not a nicety here — a bare
/// `MATCH ''` or `MATCH 'AND'` is a hard `fts5: syntax error`, measured on
/// SQLite 3.45.1, and both are reachable from a user's query string.
///
/// Tokens are joined with `AND`, matching the tokenized arm's semantics on the
/// `LIKE` path: a multi-word query means "every word appears", not "any".
pub(crate) fn fts_match_expression(query: &str) -> Option<String> {
    let tokens = crate::relevance::episodic::query_tokens(query);
    if tokens.is_empty() {
        return None;
    }
    let arms: Vec<String> = tokens
        .iter()
        // A quote inside a token is doubled, which is FTS5's own escape inside
        // a quoted string.
        .map(|t| format!("\"{}\"", t.replace('"', "\"\"")))
        .collect();
    Some(arms.join(" AND "))
}

/// Largest `messages` table the tool will index INLINE, on the `session='all'`
/// path, when the index has not been built yet.
///
/// The index is normally built by the maintenance sweep, which is the one place
/// in the process allowed to spend O(all message text) on housekeeping. A
/// profile that only ever runs the CLI never reaches that sweep, so without this
/// the explicit `all` scope would be permanently unavailable there — the tool
/// would refuse a request it can cheaply satisfy.
///
/// 5,000 rows is the bound that keeps an inline build inside one tool call:
/// measured on a copy of the `family` profile, 3,520 rows / 49 MB of content
/// rebuild in 3.4 s, and a 45,000-row / 106 MB synthetic table in 6.1 s. The
/// bound is on COUNT rather than bytes because the count is one indexed read
/// and the row sizes on this table vary by two orders of magnitude.
pub(crate) const INLINE_BUILD_MAX_ROWS: i64 = 5_000;

/// May a caller with `rows` messages in the table build the index inline?
///
/// Pure, so the boundary is testable without seeding 5,001 rows.
#[must_use]
pub(crate) fn may_build_inline(rows: i64) -> bool {
    rows <= INLINE_BUILD_MAX_ROWS
}

/// `count(*)` on `messages`, or 0 when the table is absent.
pub(crate) fn message_row_count(conn: &rusqlite::Connection) -> rusqlite::Result<i64> {
    if !super::migration_heal::has_table(conn, "messages")? {
        return Ok(0);
    }
    conn.query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0))
}
