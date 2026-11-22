-- #964: the `background_tasks` row carries its own outcome.
--
-- The row answered "what is this run" (label, command, cwd, started_at) and
-- never "how did it end". The terminal state lived only in the per-run status
-- JSON under `tmp/runs/<id>.json`, and the completion path DELETED the row, so
-- a reader holding the row — `tasks_list`, an operator query, a lane auditing
-- its own history — could not say whether the run finished, failed or never
-- started, and could not locate the output without knowing the file-naming
-- convention.
--
-- All five columns are nullable and deliberately NOT backfilled:
-- `background_tasks` rows are historical records, so a pre-migration row keeps
-- them all NULL and every reader must render that as `unknown`, never as
-- `success` (the #938 defect: a defaulted success is a lie).
--
-- `exit_code` is the COMMAND's own code. It stays NULL when the number in
-- `code` belongs to the harness instead — a signalled death, the lifetime
-- ceiling's SIGKILL, or a child that was never awaited (#786).
ALTER TABLE background_tasks ADD COLUMN status TEXT;
ALTER TABLE background_tasks ADD COLUMN exit_code INTEGER;
ALTER TABLE background_tasks ADD COLUMN completed_at INTEGER;
ALTER TABLE background_tasks ADD COLUMN stdout_path TEXT;
ALTER TABLE background_tasks ADD COLUMN stderr_path TEXT;
