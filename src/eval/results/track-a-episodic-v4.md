# Track A episodic recall v4: proximity replaces hit count as the primary signal

Supersedes `track-a-episodic-v3.md`. v3's figures are left exactly as published —
this card does not edit them. v3 closed by naming the ceiling precisely: *"the
gold is older than its neighbours AND lexically identical to them, so the pool
carries no information to rank on"*, and asked for a fixture whose neighbours
differ in a way a ranker could read.

#941 answers a different half of that. Before touching the fixture, it asked
whether the *ranker* had anything left to give on the live corpus, where rows do
differ. It did not: ranking by `(hits, leading, adjacency)` with an **unlimited**
budget scored exactly what the budgeted ranker scored. A ranker that cannot beat
itself given every row is not under-tuned — its feature set is exhausted. This
card publishes the replacement signal.

Dataset: seeded in-memory store, `seeded_store(1000)`, sampled over 14 days
Corpus: **1,000 messages drawn from 8 topical clusters**, synthetic
Queries: 40 positives + 20 negatives
Scored by: `src/eval/episodic_track.rs`, asserted by `src/tests/episodic_track_test.rs`
Scorer: `src/relevance/episodic.rs` — **one** implementation, called by the
shipped repository path and by this harness

## Result on the committed fixture

| metric | v3 | v4 | floor |
|---|---|---|---|
| recall@10 | 0.800 (32/40) | **0.800 (32/40)** | 0.700 |
| MRR | 0.227 | **0.227** | 0.180 |
| source-message | 32/40 | **32/40** | — |
| silence (negatives fired) | 10/20, FPR 0.500 | **10/20, FPR 0.500** | — |

**No regression, and no movement.** The design instrument
(`episodic_rank_model.py`) models the new rule as stage 2c and reports:

```
== stage 2c: SHIPPED #941 (proximity span, then hits, leading, adjacency) ==
recall@10=0.800 (32/40) MRR=0.227 FPR=0.500 (10/20)
pools whose proximity span is NON-constant: 0/40
```

The span is **constant across every pool of this fixture**, for the same
structural reason v3 proved: every row in a pool is generated from one template,
so the query's terms sit at identical offsets in all of them. The new primary
signal therefore cannot discriminate here — by construction, not by weakness.
v3's stage-3 ceiling proof is unchanged and still holds.

## Result on the live store

**The shipped leg was not run.** Box law forbids a local build, so
`opencrabs eval recall --track A` could not be executed against this branch. CI
is the compile authority and the gate receipt is the only build evidence this
card carries. What follows is a **Python replica**, not the shipped leg.

The replica (`/tmp/live_track_a_941.py`, same read-only `ops` store, same SQL
arms, same `sample`/`search_slice`/`score` logic) ran at `--slice-days 14`,
k=10:

| arm | recall@10 | MRR | oracle (unlimited budget) |
|---|---|---|---|
| old `(hits, leading, adjacency)` | 0.025 (1/40) | 0.004 | 0.025 (1/40) |
| new `(proximity, hits, leading, adjacency)` | **0.150 (6/40)** | **0.072** | 0.225 (9/40) |

The control arm reproduces the *shape* of the published baseline but not its
digit: the published v3-era figure was 0.053, this run's control read 0.025.
Both are 1–2 hits out of 40, and the sampler's 14-day window moves with the
clock, so the case set differs run to run. **Absolute numbers here are
sample-dependent and must not be compared across cards.** The ratio is the
claim: **0.025 → 0.150, a 6× improvement**, with the oracle confirming the
direction is not a budget artefact.

An independent measurement relayed from the lane that filed #941 put the same
comparison at 0.097 → 0.516 on its own 40 cases and a 500-row pool — a 5.3×
improvement. That figure was **not verified here** and is not reproduced by this
replica; it is recorded because it is the same ratio on a different sample, and
because it is the reason the signal was adopted.

### Pool truncation now binds

Under the old rule, truncating the recency pool made no difference (50 vs 5,000
fetched: 0.100 → 0.100) — which is why #941 falsified it as an explanation for
the misses. Under proximity it **does** bind: the budgeted arm reads 0.150 while
the same scorer with the whole pool reads 0.225. A signal that ranks by position
can promote a row from deep in the pool, so the pool's size becomes a term in
the result. `fetch_cap` is left at `5 * k` (clamped to 200) in this change; the
gap is reported, not closed, because widening the fetch is a separate cost
decision with its own measurement.

## What changed

`Score` gains a primary field and `hits` demotes to its tiebreak:

```rust
pub struct Score {
    pub proximity: Reverse<usize>,  // primary: tightest span first
    pub hits: usize,                // proximity's tiebreak
    pub leading: usize,
    pub adjacency: usize,
}
```

The type derives `PartialOrd`/`Ord`, which compares fields in **declaration
order**, and `rank` sorts by `Reverse<Score>`. Field order is therefore the
ranking, and the doc comment on `Score` says so explicitly — reordering the
fields silently reorders every search result.

- `proximity` — the character span from the **first** query term's first
  occurrence to the last one's, inverted so a tighter cluster compares greater.
  First occurrence, not closest pair: a row that mentions a term twice, the
  second time next to another query word, is not more about the pair than one
  that mentions it once.
- Missing-term guard — a row where any query term is absent has no span, and
  takes [`NO_PROXIMITY`] = `Reverse(usize::MAX)`, the smallest possible value.
  Scoring that `0` would hand the tightest possible cluster to every half-match.
  This is what keeps "contains all the words, far apart" ahead of "contains one
  word, often".
- Length normalisation was measured and is **worse**: `span/len` → 0.258,
  `span/sqrt(len)` → 0.290, against 0.516 for the raw span. Dividing by length
  over-rewards long messages. Raw span is the right form.
- Proximity as a **tiebreak** changed nothing (0.097 → 0.097) because `hits`
  already separates the rows it would have reordered. It is primary or it is
  dead weight.

Three hypotheses are recorded so nobody re-runs them, all falsified by
measurement on the live store: pool truncation under the old rule (0.100 →
0.100), BM25 (→ 0.150), and dropping low-idf query terms (→ 0.125).

**Both call sites still share one function.** `search_by_content_tokenized` and
`search_slice` continue to call `relevance::episodic::rank`; the rule was not
forked, which was #911's whole point. `RANKER_LABEL` was updated to name the new
order so the eval leg's published label matches what it grades.

## The tests that fail without the change

- `proximity_orders_rows_at_equal_hits_leading_and_adjacency` — two rows built so
  proximity is the **only** differing field (equal hits, both terms outside the
  opening zone, same adjacency run), one with the terms adjacent and one 3,000
  characters apart. Asserts the near row leads *and* that the input order had
  the far row first, so a ranker without the term returns the opposite.
- `a_row_missing_a_query_term_never_outranks_a_full_match` — a row repeating one
  query term ten times against a row containing both terms 3,000 characters
  apart. Asserts the partial row takes `NO_PROXIMITY` and loses, despite having
  the bigger hit count.
- `hits_breaks_a_proximity_tie` — equal span, one extra occurrence: `hits` still
  decides one level down.
- `adjacency_breaks_a_tie_on_equal_hits_and_leading` — updated so the two rows
  also tie on span (the phrase is padded to the reversed row's span), or
  proximity would be what decided it rather than adjacency.
- `runtime_and_eval_paths_return_the_same_order` — updated to assert the shared
  ranker reorders on proximity and does not return the recency order, keeping
  the #911 drift guard non-vacuous.

## Reading it honestly

**The fixture does not move, and that is the correct outcome.** v3 proved this
corpus cannot grade any position-aware term. A v4 that showed a delta here would
be evidence of a bug. The signal's justification is the live measurement above
plus the unit tests, not a fixture delta.

**0.800 remains a hard-fixture number, not a production claim.** This leg runs in
memory against synthetic text.

**The live figures are replica figures.** The next card that publishes a live
number should come from the shipped leg on a build, not from a Python
reimplementation. The replica's job was to answer "is there a signal left?"
before a branch was built on the answer, and it did.

## Reproducing

```
python3 src/eval/fixtures/episodic_rank_model.py   # fixture, offline, no store
```

The live replica reads `~/.opencrabs/profiles/ops/opencrabs.db` read-only and is
not committed; it is a design instrument, not a gate.
