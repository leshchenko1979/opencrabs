# Track B recall v2: the ranker and the metric

Supersedes `track-b-recall-v1.md`. v1 published `0.017` on all four metrics and
said honestly that it could not tell a weak ranker from a broken metric. It can
now: the `0.017` was three defects stacked, and two of them are fixed.

Dataset: `src/eval/fixtures/track_b_recall.json` (60 positives, 15 negatives)
Corpus: `src/eval/fixtures/track_b_corpus.md`, **293 sections**, synthetic
Scored by: `src/cli/eval_cmd.rs::score_recall_at_depth` at k=10
Configuration: `RECALL_MIN_SCORE = 0.38`, `section_rank::HEADING_BOOST = 4`

## Result

| metric | v1 (window, no boost) | v2 window (boost 4) | v2 depth leg (boost 4) |
|---|---|---|---|
| precision@10 | 0.017 | 0.083 | **0.083** |
| recall@10 | 0.017 | 0.083 | **0.100** |
| MRR | 0.017 | 0.083 | **0.092** |
| nDCG@10 | 0.017 | 0.083 | **0.094** |
| false-positive rate | 0.000 | 0.000 | **0.000** |

The window column is `score_recall` — the production injection model, two
sections inside a 1200-character budget. The depth column is
`score_recall_at_depth` — the same ranker and the same floor, retrieving the ten
sections the cutoff names. Both are published because they answer different
questions, and the depth leg is the one the scorecard reads.

Six of sixty positives now return a gold section at rank one, up from one. The
false-positive rate stays at zero: every gain here came without buying noise.

At the two cutoffs the plan asks for, the depth leg publishes:

| cutoff | recall | MRR | nDCG | FPR |
|---|---|---|---|---|
| k=2 | 0.100 | 0.092 | 0.094 | 0.000 |
| k=10 | 0.100 | 0.092 | 0.094 | 0.000 |

Identical, and not because `k` is inert any more — see below.

## The three defects, and which ones this fixes

1. **DEPTH — fixed.** `score_recall` asked for `RECALL_MAX_SECTIONS` (2)
   whatever `k` said, then scored that two-element list at k=10. `k` was inert,
   so the scorecard was a two-deep measurement wearing a ten label. The
   scorecard now reads its own leg, `score_recall_at_depth`, which retrieves the
   cutoff it names; the production window keeps its cap on purpose, because
   production really does inject two sections. Both halves are pinned by
   `eval_track_b_diagnosis_test::the_production_window_ignores_k_because_it_caps_at_two_sections`
   and `::the_scorecard_leg_reaches_deeper_than_the_production_window`.
2. **THRESHOLD — still binding.** On 293 sections the floor starves 46 of 60
   positives into an empty ranking (52 before the boost). Pinned by
   `the_shipped_threshold_starves_most_positives_into_silence`.
3. **RANKER — improved.** With the floor at 0 and full depth every gold is
   retrievable (60/60) at MRR 0.301, up from 0.240.

## Depth is fixed, and it did not buy much — read why

At the shipped floor the ranked list is at most two entries long for 74 of the
75 queries (61 empty, 10 single, 3 pairs, 1 of four). So recall@2 and recall@10
on the depth leg are **equal**: the floor, not the cap, is what is truncating.
The depth fix is real and it is worth the one extra hit, but it cannot be worth
much while defect 2 stands — a threshold that empties 61 of 75 rankings bounds
every depth metric no matter how deep the leg reaches.

That is the next lever, and it is a threshold question, not a depth one.

## Reading it honestly

This is a hard corpus: 293 sections, one gold each, and queries written from a
scenario rather than from the section they label. A recall@10 of 0.100 says the
lexical ranker is weak on this material — it does **not** say recall is broken,
and it is not comparable to the 13-section memory pair, where the same
configuration scores 1.000.
