-- Durable await record (#344): a lane waiting on an EXTERNAL completion
-- survives a restart without a human noticing.
--
-- The boot-time recovery path (`classify_recently_active`) opens with a
-- freshness gate: only bindings whose `updated_at` falls inside
-- WAKE_RECENT_SECS (3600 s) are candidates. A lane that ended its turn
-- waiting on a CI run, a peer lane or an owner reply is therefore NEVER
-- READ — not misclassified, simply never classified. Measured cost on the
-- 2026-09-14 restart: six lanes sat comatose until the owner poked them
-- 26.7-29.0 min later, with `lanes inside the window = 0`.
--
-- These three columns are an ORTHOGONAL second query path beside that gate,
-- never a widening of it. `await_at` IS NULL means "not awaiting", so every
-- pre-existing row keeps its current classification and no backfill is owed.
--
--   await_kind : what is being waited on (e.g. 'ci_run', 'peer_lane',
--                'owner_gate') — free-form TEXT, no CHECK constraint, so a
--                new await source never needs a migration.
--   await_ref  : the identifier the lane must quote (run id, session uuid,
--                issue number). NULL is legal for a wait with no handle.
--   await_at   : unix epoch seconds when the wait began. Doubles as the
--                "is awaiting" predicate and as the sweep's ordering key.
ALTER TABLE session_bindings ADD COLUMN await_kind TEXT;
ALTER TABLE session_bindings ADD COLUMN await_ref TEXT;
ALTER TABLE session_bindings ADD COLUMN await_at INTEGER;
