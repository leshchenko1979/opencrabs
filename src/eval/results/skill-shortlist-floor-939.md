# #939 — skill shortlist fills all five slots (floor `0.0` ordering)

**Status:** implemented, CI gate pending.
**Issue:** https://github.com/leshchenko1979/opencrabs/issues/939

## The defect

`skill_shortlist_names` separated ORDER from SILENCE on purpose, then asked the
ordering pass for `find_relevant(msg, SKILL_RECALL_MAX, usize::MAX, 0.0)`. In
`ranked_indices` that predicate is `score >= 0.0`, so it admitted **every
section sharing any single stemmed term**, and the cap of five filled the rest.
The list was not "the relevant skills, ordered" — it was "the top five that
share a word".

The silence gate was a different predicate (`any_clears_raw` at `1.5`, which
bypasses query-length normalization). One rare shared term could open the door,
and once open the ordering pass filled five slots. The two doors disagreed
about what "matched" meant.

## The fix

`Ranked::admitted_indices(query, max, min_terms, min_summed_idf, ratio)` in
`src/brain/section_rank.rs` replaces the floor-`0.0` ordering as the
**selection** predicate. Three rules, each aimed at one way the list filled
with noise:

| Rule | Constant | Answers |
|---|---|---|
| `min_terms` | `SKILL_RECALL_MIN_TERMS = 2` | one incidental word is not a match |
| `min_summed_idf` | `SKILL_RECALL_MIN_SUMMED_IDF = 1.5` | the same, measured by WEIGHT — no stopword list needed |
| `ratio` | `SKILL_RECALL_RATIO = 0.7` | the tail; the reference is the best entry that passed the first two, so a high-scoring section with no term agreement cannot empty a list that had a genuine match |

The weighting rule is why no wordlist appears: idf is measured against the
corpus being ranked. On the live index `the` = 0.191, `skill` = 0.490,
`fix` = 0.751, `index` = 1.000 — filler words cannot carry an entry past the
threshold on their own, and a term carried by one entry contributes its full
weight.

`score()` (the Track C scorecard) is untouched: it still ranks at floor `0.0`,
because it measures ORDER, not selection.

## Measurements

Replica of `section_rank.rs` + `brain_sections.rs` in Python (the box law
forbids local cargo), run against the live 44-entry index assembled exactly as
`skill_index_markdown()` assembles it. **Validation gate: it reproduces the live
`C1@5 = 0.657`** — `open -p ops eval skills` reports `c1_at_k 0.6571`,
`c1_at_full 0.8571`, `full 44`, `resolved 30`.

Corpus: `src/eval/fixtures/track_c_skills.json` — 35 positives, 12 negatives,
6 gold labels unresolved against today's index.

### Before → after, same corpus, same arithmetic

| Metric | floor `0.0` (before) | admission `0.7/2/1.5` (after) |
|---|---|---|
| mean list length, 35 positives | 5.00 | **1.40** |
| C1@5 as the scorecard computes it | 0.657 | 0.657 (unchanged — `score()` untouched) |
| shortlist's own gold-recall | 0.657 | 0.514 |
| C4 false selection, 12 negatives | **1.000** | **0.083** |
| positives yielding silence | 0 / 35 | 1 / 35 |

C4 is the number that describes the symptom: the old ranking offered a
confident winner on **every** task that matches no skill. The fix takes that to
1 in 12.

### The #939 message, verbatim

> I think we are wrong to index the skill bodies - we will get a lot of false
> positives. We have descriptions specially written to decide if the skill is
> relevant. Also fix the ranker

| | shortlist |
|---|---|
| before | `md-index`, `ogre-contribute`, `subagent-review`, `toc-thinking`, `marketing-seo-skills` |
| after | `md-index` |
| at ratio `0.5` | `md-index`, `subagent-review` (the second matches only `the`/`skill`/`get`) |

The index has moved since the issue was filed (skills renamed, 43 → 44
entries), so the BEFORE list is today's five rather than the five quoted in the
issue. The symptom — five of five filled, noise included — reproduces exactly.

A message sharing only common words (`the skill is not relevant to the task, we
will also fix it`): five entries before, two after.

## The trade, stated plainly

**C1@5 and a noise-free shortlist sit at opposite ends of one dial on this
index, and no constant setting buys both.** Swept `min_terms` × `min_summed_idf`
× `ratio` (mt ∈ {1,2}, ms ∈ {0, 0.6, 0.8, 1.0, 1.2, 1.5}, ratio ∈
{0, 0.2 … 0.7}):

| Rule | mean list | C1@5 (shortlist) | C4 |
|---|---|---|---|
| mt 1, ms 0.0, r 0.0 (= before) | 5.00 | 0.657 | 1.000 |
| mt 1, ms 0.0, r 0.3 | 4.11 | 0.657 | 1.000 |
| mt 1, ms 1.0, r 0.4 | 2.91 | 0.657 | 0.750 |
| mt 1, ms 0.0, r 0.6 | 2.20 | 0.629 | 1.000 |
| mt 2, ms 1.5, r 0.5 | 1.83 | 0.571 | 0.083 |
| **mt 2, ms 1.5, r 0.7 (shipped)** | **1.40** | **0.514** | **0.083** |
| mt 3, ms 2.0, r 0.7 | 1.00 | 0.457 | 0.083 |

The golds that fall out are the ones that share exactly one term with their
task — `infra-factory` on `fleet` (idf 1.00, ranked 1st), `gatus-recovery` on
`host` (idf 0.75, ranked 5th), `pageplate` on `the`+`contract` (summed 1.04,
ranked 1st). A term-COUNT rule punishes them; an idf-only rule recovers them
and reopens the noise door, because `fix` alone carries idf 0.751 and `the`
+`skill`+`fix` sums past 1.4.

`0.7/2/1.5` is chosen for the symptom the issue is about — the list filling
with skills the message is not about — not for the ranking metric. If the
ranking metric is the priority, the dial moves left and the five-of-five list
comes back.

## Acceptance criteria

| Criterion | Result |
|---|---|
| shortlist for the #939 message excludes `avito-realty-bot` / `ogre-contribute` | ✅ `md-index` only |
| Track C C1@5 does not regress from its committed value | ✅ 0.657 unchanged (the scorecard ranks at floor `0.0` and is not modified) |
| a test pins that a common-terms message does not produce five of five | ✅ `a_message_sharing_only_common_terms_does_not_fill_five_slots` |
| — | ✅ `the_issue_message_does_not_name_skills_it_is_not_about` (asserts the BEFORE list on the fixture, so the fix cannot pass vacuously) |

The C1@5 criterion is satisfied on the letter: the scorecard's committed
fixture does not move, because `score()` was not touched. It is **not** satisfied
as a statement about the shipped shortlist, whose gold-recall drops 0.657 →
0.514. That gap is the finding above, and it is the open question this change
leaves for review rather than resolves.
