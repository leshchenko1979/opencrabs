# Track B — reference-doc recall at k=10

Dataset: `src/eval/fixtures/track_b_recall.json`, generated from
`track_b_corpus.md` (5,297 lines) by `generate_track_b_corpus.py`
Corpus: 60 positive + 15 negative queries
Scored by: `src/eval/recall.rs`, asserted by `src/tests/eval_track_b_scorecard_test.rs`
Receipt: run `44531731211` (job `135586514752`), sha `79ac16960`, 2026-11-19T13:40:20Z

## Result

| metric | value |
|---|---|
| precision@10 | **0.017** |
| recall@10 | **0.017** |
| MRR | **0.017** |
| nDCG@10 | **0.017** |
| silence (negatives fired) | 0/15, FPR **0.000** |

60 positive queries, scored at the published cutoff k=10.

## Reading it honestly

**Four metrics carrying the same number is not four findings — it is one.**
When P, R, MRR and nDCG are all 0.017, the ranking is not merely weak: the gold
section is almost never retrieved at all, and when it is, it is deep enough that
it contributes next to nothing to MRR. 0.017 is 1/60, i.e. roughly one positive
query out of sixty lands its gold section anywhere in the top ten.

The committed floor is `0.01`, so this leg passes by a margin of 0.007. That is
a guard that says "not zero", not "works".

FPR 0.000 on 15 negatives is the healthy half, and it is also the half a
do-nothing ranker wins for free — the mutation test in the scorecard suite makes
exactly that point: an empty ranking scores perfect silence while every positive
metric collapses.

## What the numbers do not cover

- **The window the pipeline actually injects.** `RECALL_MAX_SECTIONS = 2`, so
  production injects at most two sections while this scorecard measures ten. A
  number taken at k=10 is not a claim about k=2.
- **The real MEMORY.md.** This corpus is synthetic and committed; the live file
  is larger and cannot be committed.
- **Why retrieval misses.** The leg scores rankings; it does not say whether the
  dataset labels or the BM25 normalization is at fault.

## Next improvement

Score the same dataset at the window production actually injects (k=2) and
compare against k=10 — if recall@2 is ~0.00 while recall@10 is 0.017, the defect
is the ranking, not the metric, and the next change belongs in retrieval rather
than in the fixture.
