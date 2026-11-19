# Track A — episodic recall over the live message store

Dataset: seeded in-memory store, `seeded_store(90)`, sampled over 14 days
Corpus: 40 positive + 20 negative queries
Scored by: `src/eval/episodic_track.rs`, asserted by `src/tests/episodic_track_test.rs`
Receipt: run `44531731211` (job `135586514752`), sha `79ac16960`, 2026-11-19T13:40:20Z

## Result

| metric | value |
|---|---|
| recall@10 | **1.000** |
| MRR | **1.000** |
| source-message | 40/40 |
| source-session | 40/40 |
| silence (negatives fired) | 0/20, FPR **0.000** |

Window: last 14 days = 90/90 messages. The unscoped `LIKE` costs ~75 s/query
(#855), which is why the leg is windowed.

## Reading it honestly

**This track is at its ceiling and therefore proves very little.** Recall 1.000
and MRR 1.000 on a seeded store of 90 messages means every positive query
retrieves its gold message in the first position: a leg that cannot fail is not
evidence that retrieval works, it is evidence that the fixture is easy.

The negative side is the only discriminating half — 0/20 fired — and even that
is weak, because the negatives were written by the same author as the positives.

The committed floor (`recall_at_k: 1.0`, `mrr: 0.99`) sits at the ceiling, so the
regression guard can only ever catch a full collapse. A drop from 1.000 to 0.999
would pass.

## What the numbers do not cover

- **Real store behaviour.** This leg runs in memory. The live-store variant
  (`opencrabs eval recall --track A`) reads the real message store and stays out
  of CI by design, so the published figure is a fixture, not production.
- **Scale.** 90 messages is three orders of magnitude below a real store.
- **Adversarial negatives.** None: every negative is a clean non-match.

## Next improvement

Make the track harder before raising the floor — recall 1.000 / MRR 1.000 cannot
discriminate, so add adversarial negatives and a larger seeded store until the
score moves off the ceiling, then pin the floor just below it.
