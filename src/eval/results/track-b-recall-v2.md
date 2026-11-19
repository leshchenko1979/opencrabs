# Track B recall v2: the ranker, not the metric

Supersedes `track-b-recall-v1.md`. v1 published `0.017` on all four metrics and
said honestly that it could not tell a weak ranker from a broken metric. It
can now: the `0.017` was three defects stacked, and one of them is fixed.

Dataset: `src/eval/fixtures/track_b_recall.json` (60 positives, 15 negatives)
Corpus: `src/eval/fixtures/track_b_corpus.md`, **293 sections**, synthetic
Scored by: `src/cli/eval_cmd.rs::score_recall` at k=10
Configuration: `RECALL_MIN_SCORE = 0.38`, `section_rank::HEADING_BOOST = 4`

## Result

| metric | v1 (no boost, >= 0.35) | v2 (boost 4, >= 0.38) |
|---|---|---|
| precision@10 | 0.017 | **0.083** |
| recall@10 | 0.017 | **0.083** |
| MRR | 0.017 | **0.083** |
| nDCG@10 | 0.017 | **0.083** |
| false-positive rate | 0.000 | 0.000 |

All four carry the same value because five of sixty positives now retrieve their
single gold section at rank one — one finding, not four.

## The three defects, and which one this fixes

1. **DEPTH — open.** `score_recall` asks for `RECALL_MAX_SECTIONS` (2) whatever
   `k` says, then scores that two-element list at k=10. `k` is inert, so this is
   a two-deep measurement wearing a ten label. Pinned by
   `eval_track_b_diagnosis_test::the_k10_score_is_identical_to_the_k2_score_because_k_is_inert`.
2. **THRESHOLD — improved, still binding.** On 293 sections the floor starves 46
   of 60 positives into an empty ranking (52 before the boost). Pinned by
   `the_shipped_threshold_starves_most_positives_into_silence`.
3. **RANKER — improved.** With the floor at 0 and full depth every gold is
   retrievable (60/60) at MRR 0.301, up from 0.240.

The boost is the fix that needed no metric change at all: same corpus, same
depth, same window, `1/60 -> 5/60`.

## What the ranker actually scores

Lifting the injection window (depth 10, no character budget) at the shipped
floor, the same ranker reaches **recall@10 0.100 / MRR 0.092**. That is the
honest measure of ranking quality; `0.083` is that quality seen through a
two-section keyhole.

## Reading it honestly

This is a hard corpus: 293 sections, one gold each, and queries written from a
scenario rather than from the section they label. A recall@10 near 0.1 says the
lexical ranker is weak on this material — it does **not** say recall is broken,
and it is not comparable to the 13-section memory pair, where the same
configuration scores 1.000.

The next lever is depth: until `k` reaches the retriever, every number here
under-reports by construction.
