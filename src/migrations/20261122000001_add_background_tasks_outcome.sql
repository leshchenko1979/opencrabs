-- #964: the row records how its run ended.
--
-- `background_tasks` answered "what was this run" and nothing else. The
-- terminal state lived only in the per-run status JSON under the runs dir, so
-- a reader holding the row could not say whether the command finished,
-- failed, or was still running, and could not locate its output without
-- knowing the file-naming convention.
--
-- The row now survives completion: the terminal write UPDATES it in place
-- instead of deleting it. All five columns are NULLABLE because a row written
-- before this migration was inserted while its run was running and never
-- received a terminal write — NULL is what "this row records no outcome"
-- looks like. No backfill: rewriting historical rows is a worse defect than
-- the one being fixed, and every reader must render NULL as unknown rather
-- than as success.
ALTER TABLE background_tasks ADD COLUMN status TEXT;
ALTER TABLE background_tasks ADD COLUMN exit_code INTEGER;
ALTER TABLE background_tasks ADD COLUMN completed_at INTEGER;
ALTER TABLE background_tasks ADD COLUMN stdout_path TEXT;
ALTER TABLE background_tasks ADD COLUMN stderr_path TEXT;
