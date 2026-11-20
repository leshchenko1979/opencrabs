# Track A episodic recall v3: a relevance term the fixture cannot measure

Supersedes `track-a-episodic-v2.md`. v2 closed with the exact defect #911 was
filed against — *"all 8 misses are the same defect: the ranker's only signal is
token-hit count, so any tie resolves to the newest row"* — and asked for a third
card *when the ranker changes, not before*. The ranker changed. This card
publishes the change **and** the measurement, which is the awkward part: on this
fixture the measurement is **+0.000**, and the reason is not a weak term.

Dataset: seeded in-memory store, `seeded_store(1000)`, sampled over 14 days
Corpus: **1,000 messages drawn from 8 topical clusters**, synthetic
Queries: 40 positives + 20 negatives
Scored by: `src/eval/episodic_track.rs`, asserted by `src/tests/episodic_track_test.rs`
Scorer: `src/relevance/episodic.rs` — **one** implementation, called by the
shipped repository path and by this harness
Receipt: CI run on `fix/911-episodic-relevance-term` (the harness is the
authority; the figures below are reproduced by the design instrument, see
"Two instruments" at the end)

## Result

| metric | v2 | v3 | floor |
|---|---|---|---|
| recall@10 | 0.800 (32/40) | **0.800 (32/40)** | 0.700 |
| MRR | 0.227 | **0.227** | 0.180 |
| source-message | 32/40 | **32/40** | — |
| silence (negatives fired) | 10/20, FPR 0.500 | **10/20, FPR 0.500** | — |

Delta from the new relevance signals: **recall +0.000, MRR +0.000.**

That is not a null result about the term. It is a property of the corpus, and
the next section is why it is stated as a proof rather than a shrug.

## Why the fixture cannot show this term working

Every message the sampler produces has the same template, with the same words
in the same positions:

```
{topic} {subject}: {a0} and {a1} after {a2} was not on the list,
then {subject} {a0} {a1} {a2} again with the {topic} work,
and the {a0} {a1} {a2} {subject} note is fine
```

The query for a positive is *the message's narrowest shared word* plus *its
widest word*, and the template repeats its subject and all three actions
verbatim. So **every row in a match pool is lexically indistinguishable from
every other row in it**: same token counts, same opening position, same
adjacency. `episodic_rank_model.py` stage 3 enumerates this directly:

```
misses whose whole pool is feature-identical: 8/8
CEILING PROVEN: every pool row of every miss is identical on every
query-derived feature, so 0.800 is the ceiling for ANY ranker that
reads only (content, query tokens) on this fixture.
```

For all 8 misses the pool holds 12–13 rows and **every row in a given pool
scores the identical tuple** — `(hits 5, leading 2, adjacency 1)` or
`(hits 5, leading 2, adjacency 2)`, with `adjacency` differing between pools
but never within one:

| query | pool | features (hits, leading, adjacency) |
|---|---|---|
| `rollback rollout` | 13 | (5, 2, 1) |
| `subscription billing` | 13 | (5, 2, 2) |
| `subscriber telegram` | 12 | (5, 2, 1) |
| `relevance memory` | 12 | (5, 2, 2) |
| `compile workflow` | 13 | (5, 2, 1) |
| `cronjob server` | 12 | (5, 2, 1) |
| `navigate browser` | 12 | (5, 2, 2) |
| `greylist mailbox` | 13 | (5, 2, 1) |

The `leading` signal is constant at its maximum of 2 for a structural reason
worth naming: the query *is* the opening of the template — `{topic} {subject}:`
— so every row in every pool opens with it. The gold is simply the oldest. No
function of `(content, query tokens)` can order that pool, so no ranker of this
shape can lift recall off 0.800 here. A term that closed the gap on this fixture
would be evidence of a bug, not of a better retriever.

The v2 fixture is left exactly as it is. It is doing its job — it can fail, it
pins a real number, and it proves the retriever has no positional signal. What
it cannot do is grade one.

## What changed

**One scorer, two call sites.** `MessageRepository::search_by_content_tokenized`
and `eval::episodic_track::search_slice` each carried their own copy of the
rule, and the copies had **drifted**: the repository summed every occurrence of
every query token, the harness counted how many *distinct* tokens were present.
Same query, same row, two different numbers — so Track A was grading a ranker
the binary does not ship. Both now call `relevance::episodic::rank`, and a test
(`runtime_and_eval_paths_return_the_same_order`) seeds two independent stores
with byte-identical rows and asserts the two paths return the same order. That
test fails on the pre-#911 code.

**Two signals, both below `hits`.** The score is the tuple
`(hits, leading, adjacency)`, compared in that order:

- `hits` — total occurrences of every query term, summed. Unchanged, and kept
  **primary** so a positional signal can never outrank a stronger lexical match.
- `leading` — how many query terms have their *first* occurrence inside the
  row's opening zone: the prefix before the first `:`, `.`, `?`, `!` or newline,
  capped at 60 characters. A message that opens with the query's words is about
  them; one that mentions them in passing is not.
- `adjacency` — the longest run of adjacent content words that are also
  consecutive, in the query's own order. A phrase, not a bag of words.

Ties fall through to the input order, which is SQL recency — unchanged from v2.
The sort is stable, so the existing fallback survives untouched. `k` is not
widened and the matching predicate is not changed: this only reorders rows that
match equally well.

## The floors, re-derived

The floors are re-derived from this measurement rather than inherited, and they
land in the same place for the same reason — which is the honest outcome when
the number did not move:

| metric | measured | floor | headroom |
|---|---|---|---|
| recall@10 | 0.800 | 0.700 | 0.100 (12.5 % of score) |
| MRR | 0.227 | 0.180 | 0.047 (20.8 % of score) |

A change that provably cannot move the number must not move the floor: lowering
it would excuse a regression the fixture *can* see, and raising it would fail
the build on a delta the fixture cannot produce. The floors stay where the
measurement puts them, and `episodic_track_test.rs` still pins 32/40, 10/20 and
MRR 0.227 — none of which moved, because none of them could.

## Reading it honestly

**The new term is unmeasured on Track A, and measured elsewhere.** Its
justification is not a recall delta on this card; it is (a) the argument that a
hit-count tie over shared vocabulary has no other discriminator, (b) the unit
tests that pin the ordering at equal `hits`, and (c) the drift fix, which is a
correctness defect independent of the term. Anyone reading this card for
"relevance improved recall" should read it as "relevance cannot be graded on
this corpus".

**`opening_zone_breaks_a_hit_tie` is the test that fails without the term.** Two
rows, each containing each query term exactly once — so `hits` ties at 2 — one
opening with the terms and one burying them past the 60-character zone. It
asserts the opening row ranks first *and* that the input order had the buried
row first, so a hits-only ranker (which keeps the stable input order) returns
the opposite. Deleting `leading` turns it red.

**0.800 remains a hard-fixture number, not a production claim.** This leg runs
in memory against synthetic text. The live variant
(`opencrabs eval recall --track A`) reads the real store and stays out of CI by
design.

**The 8 misses are unchanged, and their cause is now named precisely.** v2 said
"recency, not relevance". v3 can say more: the gold is older than its
neighbours *and* lexically identical to them, so the pool carries no
information to rank on. Closing this gap needs a fixture whose neighbours
differ in a way a ranker could read — a real corpus, or a synthetic one with
per-row position variation. That is a fixture change, and it belongs in its own
issue rather than being smuggled in behind a ranker change.

## Two instruments

`src/eval/fixtures/episodic_rank_model.py` is the **design instrument**. It
reimplements the fixture and both scoring rules in Python, and its first job was
to prove it understands the shipped algorithm: run against the *unchanged*
rule it must reproduce v2's published `0.800 / 0.227 / 0.500` to the digit, and
it does. Only then is its stage-2 and stage-3 output worth reading. It is not
shipped, not compiled, and not a gate — it exists so a design can be rejected
before a branch is built on it.

**CI is the authority for every number in the table above.** The instrument
explains why the delta is zero; the Rust harness measures it.

Run it with:

```
python3 src/eval/fixtures/episodic_rank_model.py
```
