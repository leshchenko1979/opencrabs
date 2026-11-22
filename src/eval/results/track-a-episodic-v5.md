# Track A episodic recall v5: length normalisation measured and rejected

Supersedes `track-a-episodic-v4.md` as the current card. v3 and v4 are left
exactly as published — this card corrects no figure in them, and where it
disagrees it says so and gives its own predicate.

v4 closed with a measured conclusion: the gold is retrievable in **40/40** cases
and sits at median rank 69, so the remaining gap is **ordering quality**, not
fetching. It also carried two inherited claims about proximity that had been
measured on the withdrawn probe — that length normalisation read *worse*
(`span/len` → 0.258, `span/sqrt(len)` → 0.290) and that "proximity as a tiebreak
changed nothing". Those numbers came from the probe with its own tokenizer, so
they measured a ranker the binary does not ship. This card re-measures the
length question through the shipped semantics and answers it properly.

**Everything in this card's tables is a REPLICA unless the row says otherwise.**
The box cannot build, so `opencrabs eval recall --track A` cannot be run against
a branch. The replica is `/tmp/live_track_a_941.py` + `/tmp/verify_v5_variants.py`
over the read-only `ops` store, same SQL arms, same tokenizer, same ranker. The
shipped leg has **not** been re-measured since #941 landed. No number below is a
claim about the binary.

Dataset: live `ops` store, 14-day slice
Corpus: **18,231 messages in the slice, 16,187 carrying a query token** (the
scored set)
Queries: 40 positives + 20 negatives
k = 10, pool = `TOKENIZED_FETCH_CAP` (200)
Scored by: `src/relevance/episodic.rs` — **one** implementation, called by the
shipped repository path and by the Track A harness

## The hypothesis

`Score::proximity` is `Reverse<span>`, where span is the **raw character
distance** between the first occurrence of the first query term and the first
occurrence of the last one. That is not length-normalised: a 4,000-character row
can put its terms 4,000 characters apart, a 200-character row only 200. So a
long row that discusses the query tightly can lose to a short row that mentions
it loosely — pure length bias, and a plausible reason the gold sits deep at the
14-day window where rows are long.

The hypothesis is reasonable. It is also **false on this corpus**, and the
diagnostic says exactly why.

## Result: every normalised form is worse

Same 40 cases, same fetched pools, one pass per variant. `raw` is what ships.

| proximity definition | recall@10 | MRR |
|---|---|---|
| **raw first-occurrence span (SHIPPED)** | **0.150 (6/40)** | **0.061** |
| `span / content length` | 0.075 (3/40) | 0.020 |
| `span / sqrt(content length)` | 0.125 (5/40) | 0.045 |
| `span / word count` | 0.075 (3/40) | 0.019 |
| inverse hits-per-word | 0.075 (3/40) | 0.010 |

Dividing by length halves recall. Dividing by `sqrt` costs one case. There is no
normalisation on this list that a ranker should adopt.

## Why: the bias is real, and this corpus has it pointed the other way

Length bias is not a myth — it is simply not the bias this data exhibits. Over
the 24 cases that reach the ranker:

| | mean | median |
|---|---|---|
| gold row length | **28,748 chars** | 16,376 |
| pool mean length per case | **115,595 chars** | — |

The gold rows are **four times shorter** than the rows they compete with. A
length divisor therefore penalises the gold twice: it already wins on absolute
span, and the divisor then shrinks that win because it is a large fraction of a
short row. The "correction" removes signal instead of bias.

This is a corpus property, not a law. On a corpus whose long rows were the
targets, normalisation would be right — and this table is the measurement to
re-run before assuming either direction.

## Span SHAPES: only one beat it, and it is not adoptable

Same pass, definitions that do not touch length at all:

| span shape | recall@10 | MRR |
|---|---|---|
| raw first occurrences (SHIPPED) | **0.150 (6/40)** | **0.061** |
| whole-word occurrences only | **0.175 (7/40)** | **0.078** |
| minimal window covering every term, in words | 0.050 (2/40) | 0.007 |
| minimal window covering every term, in characters | 0.025 (1/40) | 0.003 |
| rarest term as anchor, nearest other term | 0.050 (2/40) | 0.007 |
| closest pair of occurrences | 0.025 (1/40) | 0.003 |

The whole-word form is the only variant that beats the shipped span, and it wins
by **one case out of forty** — inside the noise of a 40-case sample. It is also
not adoptable as written: it would make the ranker's notion of "contains the
term" stricter than the `LIKE '%term%'` arms that fetched the row. A row matched
only as a substring (inside a longer word) would score `NO_PROXIMITY` and sort
last, behind rows that match worse. That is fetch/rank drift, the exact defect
#911 exists to prevent, and one case of recall does not buy it.

The two structural variants — minimal window, closest pair — lose badly, which
is a useful negative: rewarding the tightest *local* cluster favours rows that
mention the query in passing over rows that are about it.

## What the cap costs, and where the 0.40 bar actually sits

Re-derived from the same sample, because the numbers decide whether the bar is
reachable through this path at all:

| question | result |
|---|---|
| positives short-circuited by the contiguous `LIKE` arm | 16/40 |
| …with the gold inside that arm's top 10 | **0/40** |
| ranker-reachable cases | 24/40 |
| gold present in the AND pool at any rank | 24/24 |
| …inside the newest 200 rows (the cap) | 17/24 |
| **ceiling through this path** | **(0 + 17)/40 = 0.425** |
| ceiling if the pool were unlimited | (0 + 24)/40 = 0.600 |

The 7 cases the cap excludes sit at pool rank 283, 356, 454, 551, 728, 874 and
1,248 — deep tail, unreachable at any `k` this path serves. The 16
short-circuited cases contribute **zero** either way: the contiguous arm fires
first and the gold is never in its top 10, so widening anything downstream
cannot recover them.

Note that the cap is not what is costing the hits: an **unlimited** pool reads
the same 0.150 as the 200-row cap (v4's pool table, re-measured here). The
ranker fails those 7 cases even when it can see the gold. The cap bounds the
ceiling at 0.425; the ranker's own ordering is what leaves 0.150 against it.

That leaves the bar in a specific place: **0.425 is the maximum this path can
score, and the acceptance bar is 0.40 @ 14d.** So the bar is reachable in
principle but with almost no margin — 17 hits are needed and 17 is the whole
reachable set, i.e. the ranker would have to put the gold in the top 10 in
*every* case it can see. Measured at 0.150, it currently does so in 6.

The gap is ordering. `track-a-episodic-v4.md` reached the same conclusion from
the retrieval side (median gold rank 69 over the union) and this card confirms
it from the pool side.

## What changed in this change

**Nothing in the ranking rule.** The shipped span is kept; the module doc is
corrected to say what is actually shipped and why, with measured figures in
place of the withdrawn probe's. A future lane reading "length normalisation read
worse (0.258/0.290)" would have trusted a number from a ranker the binary does
not ship; it now reads the sweep above, with the diagnostic that explains the
direction.

Three tests pin the choice so it is a decision rather than an accident:

| test | claim |
|---|---|
| `span_ignores_row_length` | two rows with the same absolute span and a 10× length difference score **identically**; a divisor anywhere in `proximity` would separate them and fail this |
| `length_bias_ranks_the_shorter_row_no_worse` | at *exactly equal relative* tightness (checked by integer cross-multiplication), the shorter row ranks **strictly better** — a normalised ranker would tie them |
| `no_proximity_sorts_last_against_any_finite_span` | `NO_PROXIMITY` is the floor of the type: maximal hits/leading/adjacency cannot lift a partial match above a full one |

The three pre-existing proximity tests are unchanged and still pass.

**Both call sites still share one function.** `search_by_content_tokenized` and
`search_slice` both call `relevance::episodic::rank`, and both size their pool
with `tokenized_fetch_cap`. This change touches neither.

## What this card does NOT establish

- **No shipped-leg number.** 0.150 is a replica. The shipped leg has not been
  re-measured since #941 landed; confirming or refuting 0.150 requires a build
  and `opencrabs eval -p ops recall --track A --slice-days 14`.
- **No claim that 0.40 is met.** It is not. Measured 0.150 against a 0.425
  ceiling; the remaining gap needs a ranking signal the current feature set does
  not have.
- **No claim that length normalisation is wrong in general.** It is wrong *on
  this corpus*, for the measured reason in the gold/pool length table. A corpus
  whose gold rows were long would point the other way.
- **The whole-word variant is not adopted**, despite 0.175 — one case, and it
  would break fetch/rank parity.

## Next

The ordering gap is a feature-set gap, not a tuning gap: v4's median-gold-rank
69 and this card's 0.425 ceiling bracket it from both sides. A signal that
separates a row *about* the query from one that merely contains it — semantic
similarity, or query-term position weighted by inverse document frequency over
the pool — is the shape of the next attempt. Re-tuning the four current fields
has now been measured twice and has nothing left to give.
