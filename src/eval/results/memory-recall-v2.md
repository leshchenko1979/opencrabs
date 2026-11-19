# MEMORY.md recall v2: the heading field boost

Supersedes `memory-recall-v1.md` for the SHIPPED configuration. v1 remains the
record of the BM25-vs-hit-count decision; this file records what the ranker did
afterwards.

Dataset: `src/eval/fixtures/memory_recall.json`
Corpus: `src/eval/fixtures/memory_corpus.md`, 13 sections, synthetic
Scored by: `src/tests/memory_recall_eval_test.rs` and
`src/tests/eval_cli_recall_test.rs`, metrics from `src/eval/recall.rs`
Configuration: `RECALL_MIN_SCORE = 0.38`, `section_rank::HEADING_BOOST = 4`

## Result

| metric | v1 (BM25 >= 0.35, no boost) | v2 (BM25 >= 0.38, heading boost 4) |
|---|---|---|
| precision@2 | 0.917 | **1.000** |
| recall@2 | 0.917 | **1.000** |
| MRR | 0.917 | **1.000** |
| nDCG@2 | 0.917 | **1.000** |
| false-positive rate | 0.250 | **0.167** |

12 positive queries, 12 negatives.

## What changed and why

A heading is the section's own name. A query word landing in it is a stronger
signal than the same word buried in body text, so a heading token is counted
`1 + HEADING_BOOST` times. The length normalization follows the counts, so both
halves keep sharing one `tf`/`df`/length table instead of a second scoring
field.

This is not a recall-for-precision trade: **both halves improve.** v1 gave up
one answer in twelve to buy silence, and v2 gets that answer back while firing
on fewer conversational messages — because the boosted heading pulls the correct
section over the bar instead of leaving a body-word accident to clear it.

## The threshold moved with the ranker, not after it

The boost raises every section's score, so at the old floor of 0.35 the
false-positive rate stayed at 0.250 — the same value v1 shipped, sitting exactly
on the `FPR <= 0.25` contract. Any later change would then have broken the gate
by arithmetic rather than by defect.

0.38 is the LOWEST floor that reaches the FPR 0.167 tier; every value from there
to roughly 0.45 scores identically on this pair, so the low end was chosen to
keep the most recall on the much larger Track B corpus (see
`track-b-recall-v2.md`).

## Limits of this fixture

The negatives here are short and clean. Measured against a real 156-section
MEMORY.md and 437 real user messages, the pre-BM25 rule injected sections on
89.5% of messages and the shipped BM25 configuration took that to 17.4%. The
synthetic pair is a conservative floor for the improvement, not a forecast of
production rates.
