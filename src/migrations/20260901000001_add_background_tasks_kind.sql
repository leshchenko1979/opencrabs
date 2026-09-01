-- #26 P2: sub-agents join the crash-recovery table.
--
-- The background_tasks table owned detached COMMANDS only (#763): a row was
-- written before the child ran and startup treated every survivor as
-- interrupted. Sub-agents kept living only in status files, so their boot
-- accounting ran through a separate file-scanning reconcile pass. P2 unifies:
-- one table, one kind column, one boot scan.
--
-- kind: 'command' (existing rows are all commands) or 'agent'. Agents store
-- their task prompt in `command` — it is "what the unit was doing" either way,
-- which is all the interrupted-report needs.

ALTER TABLE background_tasks ADD COLUMN kind TEXT NOT NULL DEFAULT 'command';
