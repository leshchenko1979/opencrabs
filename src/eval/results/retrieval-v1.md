# Retrieval ablation v1 — does vector search earn its keep over BM25?

**Issue:** [#981](https://github.com/leshchenko1979/opencrabs/issues/981)
**Binary:** `opencrabs 0.5.4+fork.492`, mtime 2026-11-24 01:03:04 UTC
**Profile:** `ops` (the `default` profile has `vector_enabled = false`, so an
unprofiled run reports "no embedder" — that is a config fact, not a defect)
**Code under test:** `6dee0271d` (the eval id-namespace fix)
**Measured:** 2026-11-24, k = 10

The binary's build metadata does not name a commit, so the code it carries is
established two ways rather than assumed. `6dee0271d` is an ancestor of
`origin/main` at `03d5f02bb` (committed 00:27:56 UTC), and the binary was built
at 01:03:04 UTC, after that. Behaviourally, the namespace fix is present: with
it the fixture vector arm scores recall 1.000, without it every vector and
hybrid case scores 0.000 against bare-id gold labels.

> Every number below comes from a command run in this session. No figure is
> carried over from an earlier report. Where a number is not trustworthy, it is
> marked and excluded from the verdict.

## Corpus

| | |
|---|---|
| dataset | `src/eval/fixtures/retrieval_v1_recall.json` |
| cases | 72 |
| positives | 60 (one gold document each) |
| negatives | 12 |
| split | 30 lexical / 30 paraphrase / 12 negative |
| fixture corpus | 61 sections, 60 documents |

The split is **derived at load time** from the shipped tokenizer and never
stored, so a mislabelled case fails a test rather than quietly flattering an arm.

## Leg 1 — fixture (hermetic, scored)

Command:
```
opencrabs -p ops eval retrieval --dataset src/eval/fixtures/retrieval_v1_recall.json --json
```
Exit 0.

| metric | BM25 | vector | hybrid |
|---|---|---|---|
| recall@10 | 0.5333 | **1.0000** | **1.0000** |
| MRR | 0.5250 | 0.9389 | 0.9389 |
| nDCG@10 | 0.5272 | 0.9544 | 0.9544 |
| FPR (12 negatives) | 1.00 | 1.00 | 1.00 |
| precision@10 | 0.2488 ⚠ | 0.1000 | 0.1000 |

⚠ **precision is not usable in this table.** `precision_at_k` divides by the
number of results actually returned (`top.len()`), not by `k`
(`src/eval/recall.rs:13-24`). BM25 returns short lists, so each hit is divided by
a small denominator and its precision is inflated; the vector arms always fill
k = 10. The three precision figures are therefore not comparable across arms and
are excluded from the verdict. recall, MRR and nDCG are unaffected.

### Paired comparison (BM25 baseline vs vector)

| | |
|---|---|
| recall delta | **+0.4667** |
| bootstrap CI (2000 resamples) | [0.3500, 0.6000] |
| CI excludes zero | **true** |
| exact McNemar p | 7.45e-09 |
| determinism | `bit-stable` — the `determinism` field of the `--json` output, from the double-embed probe |

### Unique-hit accounting (60 positives)

| | |
|---|---|
| found by both | 32 |
| found by vector only | **28** |
| found by BM25 only | **0** |
| found by neither | 0 |
| unique-hit ratio | 28 / 0 — unbounded (requirement was ≥ 2×) |

The 28 vector-only wins are exactly the paraphrase class: BM25 finds 32 of 60
(its 30 lexical cases plus 2 paraphrases), the vector arm finds all 60, and
BM25 contributes **no** case the vector arm misses.

## Latency — per query, on the live leg

The whole-process wall clock is NOT the per-query cost. One process pays a fixed
cost (startup, embedder model load, and on the fixture leg the whole corpus is
re-embedded), and that fixed cost is what the earlier 26 s figure measured.

Per-query cost is measured here by **slope**: the same corpus is scored at 1, 24
and 72 cases, five runs each, and the slope of wall clock against case count is
the per-query cost — the fixed cost cancels. Measured on the **live** leg, which
reads the profile's existing store: no reindex, no corpus embedding, so the only
fixed cost is startup plus model load.

| mode | 1 case | 24 cases | 72 cases | slope | fixed |
|---|---|---|---|---|---|
| BM25 (FTS5) | 0.219 s | 0.274 s | 0.349 s | **1.8 ms/query** | 0.22 s |
| vector | 0.488 s | 3.563 s | 11.142 s | **151.3 ms/query** | 0.17 s |

Vector costs **84× more per query** than BM25 — a delta of **+149.5 ms**, and the
slope is stable across the two estimators (least squares 151.3 ms, two-point
150.1 ms).

Read in context, 151 ms per query is not a user-visible cost: one retrieval sits
inside an agent turn whose LLM round trip is measured in seconds. The 84× is a
ratio, not a delay. What it does mean is that a pre-registered "latency ≤ 2×
BM25" rule is violated on a strict per-query reading.

The 151 ms is query embedding plus vector search combined; the split is not
separately instrumented, so it is not claimed here. By construction the embedding
dominates: a `embeddinggemma-300M` forward pass on CPU against a scan over 2,747
vectors.

Fixture-leg caveat: its vector arm re-embeds the entire corpus inside each
process, which is why its slope is unusable (it came out negative — fixed cost
varies more than the per-query signal). The fixture leg's latency is therefore
not reported at all.

Produced by the committed script, which regenerates its own case subsets so it
does not depend on anything left in `/tmp`:

```
python3 src/eval/fixtures/measure_retrieval_latency.py --profile ops
```

It runs 5 repetitions at each of 1, 24 and 72 cases per mode and reports the
slope. The individual medians it printed were:

| mode | 1 case | 24 cases | 72 cases |
|---|---|---|---|
| BM25 | 0.219 s | 0.274 s | 0.349 s |
| vector | 0.488 s | 3.563 s | 11.142 s |

No `benches/` directory exists in the repository, so the 3.49 ms / 2.57 ms
figures from an earlier sub-agent report have **no artifact** and are not cited
here.

## Leg 2 — live store (read-only, unscored)

Command:
```
opencrabs -p ops eval retrieval --live --json
```
Exit 0.

| | |
|---|---|
| documents | 448 |
| vectors | 2747 |
| model | `embeddinggemma-300M` |
| all three arms, recall@10 | 0.0000 |
| unique-hit accounting | neither = 60/60 |

**This leg does not measure retrieval quality.** It reports 0.0000 for every arm
because the fixture's gold labels name fixture documents (`doc-01 …`), which do
not exist in the live store — every gold id is unresolved and counted as a miss
by design. What it *does* establish is that the live leg runs read-only against
the real profile store and stamps a real corpus fingerprint; it never reindexes
and never embeds.

To score the live leg, a labelled set naming **real live document paths** is
required. That set does not exist yet.

## Findings that are not the verdict

1. **precision@k has a wrong denominator** — see above. Not in the pre-registered
   rule, so it does not block the verdict, but it must not be cited until fixed.
2. **hybrid is indistinguishable from vector** on every metric. With BM25 finding
   the gold in only 32 of 60 cases, fusion can only alter those 32, and it
   evidently never reorders them. Plausible under RRF when the vector arm already
   ranks the gold first — but a fusion arm that never differs from one of its
   inputs provides no evidence *about fusion* on this corpus.

## Limitations, stated plainly

- The fixture corpus is **synthetic and domain-diverse**: one document per
  unrelated topic, so adjacent documents share almost no vocabulary. That makes
  IDF near-uniform and gives BM25 very little to discriminate on. This is BM25's
  worst case and vector search's best case, and it is why the result is read
  **asymmetrically**:
  - vector **losing or tying** here would have been a strong negative result —
    it would not win anywhere;
  - vector **winning** here is necessary but not sufficient — it may not transfer
    to a real corpus whose documents share vocabulary.
- Both legs use different lexical engines (fixture ranks Markdown with `Ranked`;
  live ranks the FTS5 index). Absolute numbers must never be compared across
  legs; each leg computes its delta inside itself.
- Latency is a slope estimate from CLI wall clock, not a bench. It separates
  fixed per-process cost from per-query cost, but it does not split query
  embedding from vector search inside a query. A `benches/` profile would be
  needed for that split and does not exist in the repository.

## Reproduce every figure

All commands are run from the repository root with the `ops` profile.

| figure | command |
|---|---|
| Leg 1 metrics, split, paired delta, unique hits, determinism | `opencrabs -p ops eval retrieval --dataset src/eval/fixtures/retrieval_v1_recall.json --json` |
| Leg 2 fingerprint and per-arm scores | `opencrabs -p ops eval retrieval --live --json` |
| per-query latency (both modes, slope) | `python3 src/eval/fixtures/measure_retrieval_latency.py --profile ops` |
| corpus shape (61 sections, 60 documents) | `grep -c '^#' src/eval/fixtures/retrieval_v1_corpus.md` and `grep -o 'doc-[0-9]*' src/eval/fixtures/retrieval_v1_corpus.md \| sort -u \| wc -l` |
| code under test | `git merge-base --is-ancestor 6dee0271d origin/main` |
| binary build time | `stat -c '%y' /usr/local/bin/opencrabs` |
| binary version | `opencrabs --version` |

The two `eval retrieval` legs are also covered by `src/tests/retrieval_track_test.rs`
(15 tests), so a change that moves the fixture scorecard fails CI rather than
silently invalidating this document.
