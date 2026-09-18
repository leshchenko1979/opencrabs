-- FORK (#299): evidence-backed goal judge.
--
-- `criteria` — JSON array of declared, checkable criteria for the goal
--   (nullable). An empty/NULL value means the goal carries no declared
--   criteria, and the aggregation caps such a goal at `Uncertain`: with
--   nothing to prove, "done" can never be `Verified`.
-- `consecutive_uncertain` — how many consecutive turns the judge returned
--   `Uncertain`. At MAX_CONSECUTIVE_UNCERTAIN the goal parks as `paused`
--   with a reason naming the exhausted evidence budget.
ALTER TABLE goal_state ADD COLUMN criteria TEXT;
ALTER TABLE goal_state ADD COLUMN consecutive_uncertain INTEGER NOT NULL DEFAULT 0;
