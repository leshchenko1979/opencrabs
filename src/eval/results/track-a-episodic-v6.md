# Track A episodic recall v6: the two arms are a union, not a fallback

Supersedes `track-a-episodic-v5.md` as the current card. v3, v4 and v5 are left
exactly as published: this card changes no figure in them and, where it
disagrees, it says so and gives its own measurement.

**Provenance, up front.**

| number | what it is |
|---|---|
| 0.150 (6/40) shipped shape, 0.200 (8/40) union | **REPLICA** — a Python sweep over the read-only `ops` store with the same SQL arms, the same tokenizer and the scoring rule copied field for field from `src/relevance/episodic.rs`. The box cannot build, so no branch was run. |
| 0.250 @ 14 d | **SHIPPED LEG** — the live binary's last 14-day Track A run, as reported on #941. It was not produced by this change and was not re-measured by it. |
| ~0.30 after this ships | **ARITHMETIC, not a measurement** — the replica's +2/40 gain added to the shipped-leg 0.250. |

The replica baseline and the shipped-leg baseline are not the same number
(0.150 vs 0.250) because the replica approximates the ranked top-10 with an
SQL span ordering over the same pool; treat the *delta* as the claim, not the
absolute.

Dataset: live `ops` store, 14-day slice, `since` = 1794093847
Corpus: 18,231 messages in the slice, 16,187 carrying a query token
Queries: 40 positives (+20 negatives, untouched by this change)
k = 10, pool = `TOKENIZED_FETCH_CAP` (200), unchanged
Scored by: `src/relevance/episodic.rs` — **one** implementation, called by the
shipped repository path and by the Track A harness

## The defect: the arms were a fallback chain

The shipped search has two SQL arms:

* **phrase arm** — `content LIKE '%<query>%'`: the query as ONE contiguous
  substring;
* **tokenized arm** — `content LIKE '%w1%' AND … AND '%wn%'`: every query word
  somewhere in the row.

They ran in sequence. `src/brain/tools/session_search.rs` did

```rust
if messages.is_empty() { … run the tokenized arm … }
```

and the harness mirrored it faithfully (`if !contiguous.is_empty() { return
Ok(contiguous); }`), so the eval leg measured the same rule the binary ran.
That is a correct #911 mirror of an incorrect rule: **one unrelated message
holding the query phrase verbatim discards the entire tokenized candidate
set**, including the row the query was actually about.

Measured on the 40 positives of the replica slice:

| question | result |
|---|---|
| positives that hit the short-circuit (phrase arm non-empty) | **16/40** |
| in those 16, gold present in the tokenized pool | **10/16** |
| in those 16, gold inside the tokenized arm's own top 10 | **2/16** |
| positives that reach the tokenized arm anyway (phrase arm empty) | 24/40, of which 6 hit @10 |

So the branch alone throws away two recoverable cases out of forty — and, more
importantly, it throws away the whole tokenized arm whenever an unrelated row
happens to contain the phrase.

## Result: the union recovers exactly those two cases

Both arms now run ALWAYS, are merged into one candidate pool by
`relevance::episodic::merge_arms` (dedupe by message id, phrase-arm rows marked
`contiguous: true`), and that pool is ordered by the same
`relevance::episodic::rank` — proximity span, then hits, then leading, then
adjacency, ties to SQL recency. Both legs call the same two functions.

| shape | recall@10 (replica) | MRR |
|---|---|---|
| short-circuit (shipped before this change) | 0.150 (6/40) | 0.061 |
| **union, contiguous arm fetched at the cap (200)** | **0.200 (8/40)** | **0.090** |
| union, contiguous arm fetched at k only | 0.200 (8/40) | 0.090 |

The two union rows are identical because the phrase arm's rows are rare enough
that the cap never binds for them in this sample. Both arms fetch
`tokenized_fetch_cap(limit)` rows newest-first — one pool rule for both arms,
not two — so the phrase arm's fetched set is the same function as the tokenized
arm's.

The eight union hits land at ranks 7, 1, 4, 2, 1, 3, 5 and 6, which is where
the 0.090 MRR comes from (the 0.061 shipped figure is v5's, re-used unchanged
because this change does not touch the 24 cases that already reached the
tokenized arm).

**The gain is 2/40 = +0.050 replica.** Applied to the shipped-leg 0.250 that is
~0.30, which is the number to expect after this ships. It is still below the
0.40 bar and **this change is not expected to reach it**.

## Measured and REJECTED: ordering the fetch by proximity

The obvious next lever was to fetch the phrase arm ordered by proximity instead
of `created_at DESC`. It is **circular**: proximity is already the ranker's
first sort key, so ordering the SQL by the same expression returns the same
top-10 the ranker would produce — measured at **6/24 either way** — while
costing **1.8× the SQL time**. Not implemented.

Nine scoring variants were measured in the v5 pass and all lose to the raw span
(`track-a-episodic-v5.md`, tables above); this change does not revisit scoring,
does not change the 200-row cap, and does not change the tokenizer.

## Why 0.40 is still not expected

v5 bracketed the gap: the path's ceiling is 0.425 (17 reachable hits, with the
16 short-circuited cases contributing zero), and the ranker scored 6 of those
17. The union moves the short-circuited cases from "unreachable" to "reachable
and mostly still lost": 10 of the 16 golds are in the tokenized pool, but only
2 survive into the top 10 under proximity ordering, because the phrase row that
triggered the short-circuit is usually a tighter span than the gold. So the
ceiling rises by at most 2 and the ranker still loses most of what it can see.

The remaining distance is **ordering inside the fetched pool**, not the fetch
and not the branch. That is a feature-set problem, not a tuning one.

## What changed in code

| file | change |
|---|---|
| `src/relevance/episodic.rs` | new `Candidate<T>` (row + `contiguous` flag) and `merge_arms` — the ONE place the arms are joined; both legs rank the joined pool with the existing `rank` |
| `src/db/repository/message.rs` | new `search_by_content_union` (the shipped search); one SQL string per arm, shared by the single-arm methods too; `search_by_content` and `search_by_content_tokenized` keep their signatures |
| `src/brain/tools/session_search.rs` | the `if messages.is_empty()` short-circuit is gone; the tool calls the union and its header now states how many returned rows hold the phrase **verbatim** and how many are token-only |
| `src/eval/episodic_track.rs` | `search_slice` runs both arms and merges them; no branch on emptiness |
| `src/tests/episodic_rank_test.rs` | three new tests (below) |

The user-visible label changed because the old one became false. It read
`~ token match — no contiguous match for '<query>'`; under the union a result
set can contain phrase rows *and* token-only rows, so the header now reports
the split (`~ ranked by term proximity: N of M rows contain '<query>' verbatim,
the other K contain all of its words but not the phrase`). When no returned row
holds the phrase it says so instead: `~ token match — no message contains
'<query>' verbatim; every row below contains all of its words, ranked by term
proximity.` — which keeps the `~ token match` prefix the existing
tool-level test looks for, while dropping the false "no contiguous match for"
claim. A source-level test pins that sentence out.

## Tests

| test | claim |
|---|---|
| `the_union_reaches_gold_when_another_row_holds_the_phrase` | the gold does NOT contain the phrase while a newer distractor does; the gold must still be a candidate in BOTH legs, flagged `contiguous: false`, and a one-token-only row must stay absent. Fails on the old short-circuit shape (gold absent). |
| `an_exact_phrase_row_leads_when_it_is_the_tightest_match` | the verbatim-phrase row is the OLDEST of two, so recency cannot put it first; the ranker must, and it must carry `contiguous: true` |
| `neither_leg_short_circuits_and_the_label_tells_the_truth` | neither leg contains the short-circuit branch; `session_search` calls the union and its label no longer claims "no contiguous match" |
| `runtime_and_eval_paths_return_the_same_order` | unchanged claim, now exercised through `search_by_content_union` — the path the binary actually ships |

The pre-existing ranker tests (`proximity_orders_rows_at_equal_hits_leading_and_adjacency`,
`hits_breaks_a_proximity_tie`, `tokenized_rows_rank_by_proximity_then_hit_count`,
`span_ignores_row_length`, `the_tokenized_pool_is_sized_by_one_shared_rule`) are
unchanged and still pass.

## What this card does NOT establish

- **No shipped-leg number for the union.** 0.200 is a replica. Confirming it
  needs a build and `opencrabs eval -p ops recall --track A --slice-days 14`.
- **No claim that 0.40 is met**, now or after this change. Expected ~0.30.
- **No claim that the union is the best use of the tokenized arm.** It only
  stops the arm being thrown away; ordering the two kinds of row against each
  other is untouched and is where the remaining gap lives.
- **No change to the cap, the tokenizer or the scoring rule**, and no
  relevance-ordered fetch — measured circular and rejected.

## Next

The measured shape of the next attempt is unchanged from v5: a signal that
separates a row *about* the query from one that merely contains it. This change
makes sure the ranker is at least *shown* the rows the phrase arm was hiding;
whether it can order them is the open question, and it now has the pool to
answer it.
