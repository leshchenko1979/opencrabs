# Track B recall v3: the floor is corpus calibration

Supersedes `track-b-recall-v2.md`. v2 named the threshold as the next lever and
left the number at 0.38 with no evidence about what that number costs on a large
corpus. This publishes the sweep that answers it, and the decision it forced.

Dataset: `src/eval/fixtures/track_b_recall.json` (60 positives, 15 negatives)
Corpus: `src/eval/fixtures/track_b_corpus.md`, **293 sections**, synthetic
Comparison corpus: `src/eval/fixtures/memory_recall.json` over
`memory_corpus.md` (**13 sections**) — the pair the `<= 0.25` FPR contract names
Scored by: `opencrabs eval recall --track b --k 10`, live artifact 2026-11-20
Configuration: `RECALL_MIN_SCORE = 0.38`, `section_rank::HEADING_BOOST = 4`

## The finding: one constant, two different products

The floor is **corpus calibration, not a product truth**. The same number admits
a different fraction of a 13-section brain file and a 293-section corpus, because
a lexical score falls off with the size of the haystack it competes in. A floor
tuned on the memory pair therefore says nothing on its own about Track B — and
v2's "starves 46 of 60" was exactly that gap, stated as a defect without a curve
behind it.

Two columns carry the difference. **fired** counts positives that returned a
NON-EMPTY ranking; **correct** counts those whose gold section made it through.
The gap between them is the ranker's, not the floor's.

### Track B — 293 sections, 60 positives, 15 negatives

| floor | fired (of 60) | correct | recall@10 | FP (of 15) | FPR |
|---|---|---|---|---|---|
| 0.05 | 60 | 11 | 0.183 | 15 | 1.000 |
| 0.10 | 60 | 11 | 0.183 | 15 | 1.000 |
| 0.15 | 60 | 11 | 0.183 | 15 | 1.000 |
| 0.20 | 60 | 11 | 0.183 | 8 | 0.533 |
| 0.25 | 47 | 10 | 0.167 | 1 | 0.067 |
| 0.30 | 26 | 9 | 0.150 | 0 | 0.000 |
| 0.36 | 17 | 7 | 0.117 | 0 | 0.000 |
| **0.38 (shipped)** | **14** | **5** | **0.083** | **0** | **0.000** |
| 0.45 | 6 | 2 | 0.033 | 0 | 0.000 |

### Memory pair — 13 sections, 12 positives, 12 negatives

| floor | fired (of 12) | correct | recall@10 | FP (of 12) | FPR |
|---|---|---|---|---|---|
| 0.05 | 12 | 12 | 1.000 | 12 | 1.000 |
| 0.10 | 12 | 12 | 1.000 | 12 | 1.000 |
| 0.15 | 12 | 12 | 1.000 | 12 | 1.000 |
| 0.20 | 12 | 12 | 1.000 | 7 | 0.583 |
| 0.25 | 12 | 12 | 1.000 | 6 | 0.500 |
| 0.30 | 12 | 12 | 1.000 | 3 | 0.250 |
| 0.36 | 12 | 12 | 1.000 | 3 | 0.250 |
| **0.38 (shipped)** | **12** | **12** | **1.000** | **2** | **0.167** |
| 0.45 | 9 | 9 | 0.750 | 1 | 0.083 |

## The starvation count at the shipped floor

At `0.38`, **14 of 60 Track B positives return a non-empty ranking** — 46 are
starved into silence. That is the number the constant costs, and it is now
pinned by `the_shipped_floor_does_not_starve_track_b`, which asserts the exact
count of 14 and fails if the floor or the ranker moves without a re-measure. A
second assertion in the same test reads `0.45` (6 of 60) to prove the pin can
actually see starvation rather than passing on any input.

## Why 0.38 and not the cheaper 0.36

On the large corpus the floor is a **volume knob, not a quality knob**: precision
among fired sections is flat across the plateau — 0.346 at 0.30, 0.412 at 0.36,
0.357 at 0.38, 0.333 at 0.45. Lowering it buys more answers at the same hit rate,
which is why 0.36 looks attractive: it fires three more positives (17 vs 14) and
one more correct (7 vs 5) at an unchanged Track B FPR of 0.000.

It was declined anyway. At 0.36 the memory pair lands on **FPR 0.250 — exactly
the `<= 0.25` contract**, with zero headroom. #911 is about to change the
ranker; a gate sitting on its own boundary would then break by arithmetic rather
than by defect, and the failure would be indistinguishable from a real
regression. 0.38 keeps 0.167, the headroom that move bought.

## What limits Track B is the ranking, not the floor

Recall is capped at **11 of 60 even at floor 0.05, where all 60 fire**. With
everything admitted, 49 positives still never surface their gold — so no floor
value recovers them. That ceiling is `section_rank`'s, and it is #911's subject.
This document therefore records a floor that is defensible on its own terms
without claiming it fixed Track B.

## Reading it honestly

The 13-section and 293-section columns are not comparable and are not meant to
be: the same configuration scores 1.000 on one and 0.083 on the other. That is
the point of publishing both. A floor is a per-corpus compromise, and the number
in the binary is the one that keeps the small pair inside its contract while
admitting as much of the large corpus as the contract allows.
