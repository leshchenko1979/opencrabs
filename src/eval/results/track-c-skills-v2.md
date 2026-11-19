# Track C skills v2: C3 is either measured or NOT MEASURED

Supersedes `track-c-skills-v1.md`.

Dataset: `src/eval/fixtures/track_c_skills.json`, 35 entries / 12 negatives
Scored by: `src/eval/skills_track.rs`, asserted by `src/tests/eval_track_c_scorecard_test.rs`
Measured: 2026-11-19, against `/root/.opencrabs/profiles/ops/opencrabs.db` (read-only)

## The defect v1 shipped

v1 published a `C3 (skills ever employed)` row reading **0/35 (0.000)**, from a CI
runner. That row is the strongest possible claim about the index — *no skill has
ever been used* — and it was produced by the **absence of the instrument**: a CI
runner has no `session_seen_skills` table at all. An absent measurement printed
as a result.

C3 is now `Option<usize>`, so the two states cannot be confused:

| state | rendered | means |
|---|---|---|
| `None` | `NOT MEASURED here — needs session_seen_skills` | this leg cannot see employment |
| `Some(n)` | `n/35 skills ever employed (x.xxx)` | this leg read the record |

The CI fixture leg passes `None` and never prints a zero. Two tests hold the
line: the fixture card must say NOT MEASURED, name the missing table, and never
print `0/35`; and the metric must MOVE — `None`, `Some(all)` and `Some(partial)`
must produce three different coverages, so a value pinned at zero by
construction fails the gate.

## The live figure, with its predicate

The employment record is real. Every number below is a different predicate, and
they are not interchangeable:

| reading | value | predicate |
|---|---|---|
| rows in `session_seen_skills` | **471** | every activation, 2026-09-09 → 2026-11-19 |
| distinct strings in `slug` | **19** | raw `SELECT DISTINCT slug`, **includes 2 non-slugs** |
| genuine skill slugs | **17** | the 19 minus `[]` and `opencrabs-dev/editor.md` |
| installed skill directories | **36** | `~/.opencrabs/profiles/ops/skills/*/` |
| genuine slugs inside the scored index | **14** | intersection with the fixture's 35 gold slugs |
| **C3 as shipped** | **14/35 = 0.400** | `employed_slugs(index) / index size` |

**The shipped metric reports 0.400, not 0.528.** `employed_slugs()` intersects
the live record with the index it scored, and `c3_coverage` divides by that
index's size, so the denominator is the 35 scored skills — not the 36 installed
ones. Three genuinely-employed slugs are outside the fixture index
(`ai-antispam-factory`, `inferhub-watch-hq`, `miidas-factory`) and are invisible
to this metric by construction.

The figure `19/36 = 0.528` is the raw-string-over-installed-dirs reading. It is
not what the code computes and it counts two things that are not skills, so it
must not be quoted as C3.

## Two rows in the employment record are not skills

`SELECT DISTINCT slug` returns two values that no skill answers to:

| slug | rows | first seen |
|---|---|---|
| `[]` | 1 | 2026-11-14 |
| `opencrabs-dev/editor.md` | 1 | 2026-09-14 |

One is a serialised empty list, the other a **file path** — the editor role file
rather than its skill directory. Two rows out of 471 is 0.42 % of the record, but
it inflates the distinct-slug count by 2 of 19 (10.5 %), which is the numerator
of the raw reading. The writing path accepts a non-slug; this doc records the
contamination rather than silently filtering it.

## Concentration: one skill is most of the record

| slug | rows | share of 471 |
|---|---|---|
| `opencrabs-dev` | 264 | 56.1 % |
| `meta-factory` | 61 | 13.0 % |
| `inferhub-watch-hq` | 34 | 7.2 % |
| `ai-antispam-factory` | 31 | 6.6 % |
| `infra-factory` | 29 | 6.2 % |

The remaining 12 slugs carry 52 rows between them. A coverage figure of 0.400
says *how many* skills were used, never *how evenly* — 17 of 36 sounds broader
than the distribution behind it.

## What the numbers do not cover

- **CI C3.** Not measured, by design, and labelled so on the card. The CLI leg
  (`opencrabs eval skills`) reads the live store and stays out of CI.
- **Why a skill was or was not loaded.** The record proves a session touched a
  slug; it says nothing about whether the selector chose it or the prompt forced it.
- **Coverage over time.** 471 rows span 71 days with no daily breakdown here.
- **Skills installed but never in the index.** 1 of 36 installed directories is
  not among the 35 scored entries.

## Next improvement

`C1@5 = 0.371` is still the only real selection signal on this track. C3 is now
either measured or honestly absent, which makes it safe to quote but not yet a
target: moving it requires a runtime ranker that picks among the index, which is
the remaining leg of this plan.
