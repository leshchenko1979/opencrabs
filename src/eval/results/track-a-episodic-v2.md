# Track A episodic recall v2: a fixture that can fail

Supersedes `track-a-episodic-v1.md`. v1 published **recall 1.000 / MRR 1.000 /
FPR 0.000** and said plainly that the track "proves very little". It proved
nothing at all: both sides were tautologies, and this card publishes the
measurement that replaced them.

Dataset: seeded in-memory store, `seeded_store(1000)`, sampled over 14 days
Corpus: **1,000 messages drawn from 8 topical clusters**, synthetic
Queries: 40 positives + 20 negatives
Scored by: `src/eval/episodic_track.rs`, asserted by `src/tests/episodic_track_test.rs`
Receipt: run `44585938866` (job `135778745627`), sha `0ab68e5e6`, 2026-11-19T18:15:51Z

## Result

| metric | v1 | v2 | floor |
|---|---|---|---|
| recall@10 | 1.000 | **0.800** (32/40) | 0.700 |
| MRR | 1.000 | **0.227** | 0.180 |
| source-message | 40/40 | **32/40** | — |
| silence (negatives fired) | 0/20, FPR 0.000 | **10/20, FPR 0.500** | — |

Rank distribution over the 32 hits:

| rank | 1 | 2 | 3 | 4 | 5 | 6 | 7 | 8 | 9 | 10 |
|---|---|---|---|---|---|---|---|---|---|---|
| hits | 3 | 3 | 3 | 3 | 4 | 3 | 3 | 3 | 3 | 4 |

MRR is the rank-weighted sum over all 40 positives, with a miss contributing
zero: `9.086905 / 40 = 0.227`.

## Why v1 could not fail

1. **POSITIVES were a lookup, not a retrieval.** `seeded_store` gave every
   message its own 18 private nonsense tokens (`zq5alpha zq5bravo …`), and
   `case_from` lifted 3 of them as the query. The tokenized AND arm then matched
   exactly one row **by construction**. Recall 1.000 measured the constructor.
2. **NEGATIVES were admitted only after proving silence.** A pair was added only
   once a `COUNT` confirmed zero co-occurrence, so 0/20 firing was decided
   before the case existed. FPR 0.000 measured the filter.

A leg whose two halves are both guaranteed cannot detect a regression, and its
floor has to sit at the ceiling — v1's floors were `1.0` / `0.99`, so a drop to
0.999 passed.

## What changed

**Shared vocabulary.** The corpus is 8 topical clusters (rollout, billing,
telegram, memory, workflow, server, browser, mailbox), each with a topic word,
10 subjects and 8 actions. Messages inside a cluster share most of their words,
so a query selects a **neighbourhood of a dozen rows** instead of one — which
is what makes ranking a measurement rather than a dictionary lookup.

**Queries a person would ask.** The positive query is the message's *narrowest
word that appears in at least two messages* plus its *widest* word. It is
answerable without being unique: `"rollback rollout"` names a topic and a
direction, not a row id.

**Two classes of negative.**

- **10 near-misses** — pairs that genuinely co-occur in a non-gold message. A
  ranker that does nothing fires on these, so the metric can move.
- **10 genuine absences** — pairs with zero co-occurrence. Silence is a result
  here, not a premise.

FPR 0.500 is the near-miss class firing and the absence class staying quiet.
The test asserts both directions: `false_positives > 0` (the fixture can fail)
**and** `false_positives < negatives` (it can pass too).

## The floors, and why they are where they are

| metric | measured | floor | headroom |
|---|---|---|---|
| recall@10 | 0.800 | 0.700 | 0.100 (12.5 % of score) |
| MRR | 0.227 | 0.180 | 0.047 (20.8 % of score) |

The fixture is deterministic (fixed seed, no clock in the corpus), so there is
no run-to-run noise to absorb: the floors sit below the score to catch a real
regression, not to tolerate jitter. A leg that goes blind — ranker inverted,
query built wrong, negatives silently re-filtered — lands far under both.

## Reading it honestly

**0.800 recall is a hard-fixture number, not a production claim.** This leg
runs in memory against synthetic text. The live variant
(`opencrabs eval recall --track A`) reads the real message store and stays out
of CI by design, so nothing here measures production retrieval.

**The 8 misses are one per cluster, and the cause is recency, not relevance.**
Every miss is a gold whose query matches 12–13 rows, of which 10–12 are *newer*
than the gold: it ranks 11th–13th by `created_at`, so `LIMIT 10` cuts it. The
ranker has exactly one relevance signal — how many query tokens a row contains
— and every match here contains both, so the tie is broken by SQL recency order
and nothing else. Recall 0.800 is what a token-hit counter with a recency
tie-break scores on shared vocabulary.

**FPR 0.500 is high, and correctly so.** Half the negatives were built to be
fireable. A near-miss that a lexical ranker ignores is not a near-miss. The
figure is a property of the fixture, so it is pinned by a test rather than
tuned toward zero.

**Source-session is not published.** With shared vocabulary a positive query
matches rows from several sessions by construction, so session-level recall
would read high without discriminating anything. Source-message is the binding
figure.

## Next improvement

All 8 misses are the same defect: the ranker's only signal is token-hit count,
so any tie resolves to the newest row, and a gold that is older than ten of its
own neighbours is unreachable no matter how well it matches. That is a
**retriever** limitation, not a fixture one — a subject-position or phrase-
proximity term inside `search_slice` would separate a gold from its
neighbours. This card measures the fixture as it stands; publish a third card
when the ranker changes, not before.
