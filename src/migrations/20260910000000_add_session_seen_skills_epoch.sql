-- Skill glob gate (issue #150): per-row compaction epoch. The in-memory
-- seen_skills registry becomes epoch-carrying — a skill is "seen in the
-- current session context" only if its stored epoch is >= the session's
-- current epoch. Rows written before this feature carry NULL epoch and are
-- treated as epoch 0 (always current, back-compat: pre-gate sessions pass).
ALTER TABLE session_seen_skills ADD COLUMN epoch INTEGER NULL;
