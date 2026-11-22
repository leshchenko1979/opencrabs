-- #855: a full-text index over `messages.content`.
--
-- THE DEFECT: `session_search`'s content arms are unscoped `content LIKE '%x%'`
-- scans. On the ops store that is 43,756 rows / 607,204,493 content bytes (avg
-- 13,872 chars/row) with NO index on `content` and no FTS table — measured at
-- ~75 s per query (#855). This migration creates the index; the query arms gain
-- an FTS prefilter and the index is populated OFF the startup path (see
-- `crate::db::messages_fts`).
--
-- WHY THE `trigram` TOKENIZER, NOT `unicode61`
-- The shipped ranker and the #941 eval measure SUBSTRING semantics — `brown`
-- matches `rown`, mid-word included. `unicode61` matches whole tokens at word
-- boundaries, which would silently change the result set and invalidate the
-- #941 measurements. `trigram` indexes every 3-character sequence, so
-- `MATCH '"rown"'` finds `brown` (verified on this box, bundled SQLite 3.53.2).
-- The prefilter is therefore a SUPERSET of what `LIKE '%…%' COLLATE NOCASE`
-- matches: trigram folding is Unicode-aware while `LIKE … NOCASE` folds ASCII
-- only, so the index can never be the reason a LIKE match is dropped.
--
-- WHY `content=''` + `contentless_delete=1` RATHER THAN `content='messages'`
-- The external-content shape (`content='messages'`, `content_rowid='rowid'`)
-- was tried first and is WRONG for this table, for three measured reasons:
--
--   1. It BREAKS EVERY `DELETE FROM messages` UNTIL THE INDEX IS COMPLETE. On
--      an external-content FTS5 table, both `INSERT INTO fts(fts,rowid,content)
--      VALUES('delete',…)` and a plain `DELETE FROM fts WHERE rowid=?` raise
--      SQLITE_CORRUPT ("database disk image is malformed", code 11) when that
--      rowid is not in the index. The migration creates an EMPTY index over
--      43,756 existing rows, so between this migration and the end of the
--      background backfill every session delete, retention prune and
--      `delete_by_session` would fail on a user request. Reproduced in a
--      scratch DB on this box: one `DELETE FROM messages` against a freshly
--      migrated store => "Runtime error near line 5: database disk image is
--      malformed (11)".
--   2. Its completeness check is VACUOUS. `SELECT count(*) FROM messages_fts`
--      on an external-content table is answered by the CONTENT table, so it
--      always equals `count(*) FROM messages` — the backfill can never tell
--      whether it is needed, and no test can prove the index is in sync.
--   3. It cannot be backfilled in chunks. `rebuild` is all-or-nothing, so it
--      holds the write lock for the whole 607 MB pass (measured ~2–3 min on a
--      scratch corpus, see `src/eval/results/messages-fts-855.md`) and blocks
--      every user write meanwhile — the one thing the backfill must never do.
--
-- A contentless table with `contentless_delete=1` (SQLite >= 3.43; bundled is
-- 3.53.2) has none of those properties: `DELETE` on an absent rowid is a
-- harmless no-op, `count(*)` reports the real indexed row count, and rows are
-- added incrementally, so the backfill is chunked and resumable. What it does
-- NOT store is the message text — which is exactly right here, since nothing
-- ever reads `messages_fts.content`: the index is a PREFILTER and the LIKE arms
-- remain the authority on membership.
--
-- The index is keyed by `messages.rowid` (implicit — `messages.id` is TEXT, so
-- the FTS rowid is NOT `id`; verified with `pragma table_info`). Both index
-- maintenance and the query prefilter are gated on `messages_fts_state.complete`
-- so a partially built index can never narrow a search.
--
-- NO `rebuild` HERE, ON PURPOSE. Migrations run synchronously at startup; a
-- rebuild over 607 MB would block boot for minutes. This file only creates the
-- structure. `messages_fts_state` starts `complete = 0`, which makes every
-- search fall back to the plain-LIKE query (correct, merely slow) until the
-- background backfill flips it.
--
-- Appended LAST in `MIGRATION_SQL` (`src/db/database.rs`) so no existing
-- `user_version` stamp shifts — the #1401 class. No `MIGRATION_EFFECTS`
-- declaration is owed (that window is contiguous and ends far below this
-- entry).

CREATE VIRTUAL TABLE IF NOT EXISTS messages_fts USING fts5(
    content,
    content='',
    contentless_delete=1,
    tokenize='trigram'
);

-- One row, id = 1. `complete` is the gate the query path and the triggers read;
-- `cursor` is the highest `messages.rowid` already copied into the index, so a
-- backfill interrupted by a restart resumes instead of starting over.
CREATE TABLE IF NOT EXISTS messages_fts_state (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    complete INTEGER NOT NULL DEFAULT 0,
    cursor INTEGER NOT NULL DEFAULT 0
);

-- `OR IGNORE`: re-running this migration (or a store that already carries the
-- state row) must never reset a completed index back to `complete = 0`.
INSERT OR IGNORE INTO messages_fts_state (id, complete, cursor) VALUES (1, 0, 0);

-- The three sync triggers, the shape `documents_ai`/`_ad`/`_au` established in
-- `src/memory/db.rs`. They are deliberately UNGATED: on a contentless-delete
-- table every operation below is safe against a row that is not indexed yet
-- (insert-or-replace is idempotent, delete of an absent rowid is a no-op), so
-- the index is maintained from the moment the migration lands and the backfill
-- only has to cover the rows that predate it. `UPDATE OF content` is load-bearing:
-- without the column list every `cost`/`token_count`/`thinking` update would
-- re-tokenize a 14 KB body.
CREATE TRIGGER IF NOT EXISTS messages_ai AFTER INSERT ON messages BEGIN
    INSERT OR REPLACE INTO messages_fts(rowid, content) VALUES (new.rowid, new.content);
END;

CREATE TRIGGER IF NOT EXISTS messages_ad AFTER DELETE ON messages BEGIN
    DELETE FROM messages_fts WHERE rowid = old.rowid;
END;

CREATE TRIGGER IF NOT EXISTS messages_au AFTER UPDATE OF content ON messages BEGIN
    INSERT OR REPLACE INTO messages_fts(rowid, content) VALUES (new.rowid, new.content);
END;
