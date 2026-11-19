# Track C — skill selection against the live index

> **SUPERSEDED by `track-c-skills-v2.md` (#899).** The `C3 0/35 (0.000)` rows
> below are an absent instrument printed as a result — a CI runner has no
> `session_seen_skills` table — and must not be quoted as employment. v2 makes
> C3 either measured or explicitly NOT MEASURED, and publishes the live figure
> with its predicate.

Dataset: `src/eval/fixtures/track_c_skills.json`, 35 entries / 12 negatives
Scored by: `src/eval/skills_track.rs`, asserted by `src/tests/eval_track_c_scorecard_test.rs`
Receipt: run `44531731211` (job `135586514752`), sha `79ac16960`, 2026-11-19T13:40:20Z

## Result

Scored at both the budgeted window (k=5) and the full index (k=35):

| metric | k=5 | k=35 |
|---|---|---|
| C1 (gold inside the window) | **0.371** | 1.000 |
| C2 (prompt chars) | 1,140 | 1,140 |
| C3 (skills ever employed) | 0/35 (**0.000**) | 0/35 (**0.000**) |
| C4 (false selection on 12 negatives) | 0.000 | 0.000 |
| labels resolved against this index | 35/35 | 35/35 |

## Reading it honestly

**C1@35 = 1.000 is vacuous and must never be quoted as performance.** The index
holds exactly 35 entries, so "the gold is inside the first 35" is true by
construction for every query. It is the do-nothing baseline, published here
precisely so the number cannot be mistaken for a result.

**C1@5 = 0.371 is the only real selection signal**, and it means roughly 13 of 35
queries put their gold skill in the top five. Two thirds do not.

**C3 = 0/35 is a measurement gap wearing the costume of a result.** Employment is
read from `session_seen_skills`, which a CI run does not populate, so zero
employment here is the absence of the instrument — not evidence that skills are
never used. Any reader who quotes "0% of skills are ever employed" from this row
is quoting an artefact.

C4 = 0.000 on 12 negatives is the healthy half, and like Track B it is the half a
ranker that selects nothing wins for free. C1 and C4 must be read together: a
selector that never fires has perfect C4 and C1@5 = 0.

## What the numbers do not cover

- **Employment (C3).** Needs a live `session_seen_skills`; the CLI reads it live
  and stays out of CI by design.
- **Prompt cost in context.** C2 counts characters, not tokens, and not the
  position of those characters in the prompt.
- **Selection at k other than 5 and 35.** The shipped budget is 5; nothing
  between 5 and 35 is measured.

## Next improvement

Make C3 measurable before quoting it — either populate `session_seen_skills` in
the fixture or drop the metric — then treat C1@5 = 0.371 as the number to move,
since C1@35 = 1.000 and C3 = 0/35 carry no information at all.
