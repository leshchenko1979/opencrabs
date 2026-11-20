#!/usr/bin/env python3
"""Track A ranking model — the DESIGN instrument for #911 (NOT the shipped gate).

Why this file exists: the lane that fixes the retriever cannot run `cargo test`
(the box has no compiler; CI is the only compile-and-test gate). So the ranking
change is designed against a standalone reproduction of the SAME deterministic
fixture the committed leg scores, and the numbers it produces are the design's
receipt — the authoritative figures are the CI run of the Rust tests.

The fixture is not transcribed here. `CLUSTERS` and `STOPWORDS` are PARSED out of
`src/eval/episodic_track.rs` itself, so a fixture change cannot silently leave
this model measuring an older corpus.

Usage:
    python3 src/eval/fixtures/episodic_rank_model.py            # baseline + proposed
    python3 src/eval/fixtures/episodic_rank_model.py --baseline # baseline only

Stages, in order:
  1. CONTROL — reproduce the CURRENT scoring (token-hit count, recency tie) and
     assert it yields the published 0.800 / 0.227 / 0.500. If it does not, the
     model is wrong and no proposed term may be trusted until it does.
  2. PROPOSED — score the same cases through the new two-signal scorer and print
     the deltas, the rank distribution and the honest ceiling.
"""

import argparse
import ast
import os
import re
import sys
from collections import Counter

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__)))))
TRACK = os.path.join(ROOT, "src", "eval", "episodic_track.rs")

POSITIVES = 40
NEGATIVES = 20
K = 10
SLICE_DAYS = 14


# ---------------------------------------------------------------- fixture parse


def _strip_comments(src: str) -> str:
    return re.sub(r"//[^\n]*", "", src)


def _rust_str_list(block: str) -> list:
    return re.findall(r'"([^"]*)"', block)


def parse_clusters(src: str) -> list:
    """Parse `const CLUSTERS: &[(&str, &[&str], &[&str])] = &[ ... ];`."""
    m = re.search(r"const CLUSTERS[^=]*=\s*&\[(.*?)\n\];", src, re.S)
    if not m:
        raise SystemExit("could not locate CLUSTERS in %s" % TRACK)
    body = _strip_comments(m.group(1))
    clusters = []
    # Each entry is: ( "topic", &[ ...subjects... ], &[ ...actions... ] )
    for entry in re.finditer(
        r'\(\s*"([^"]+)"\s*,\s*&\[(.*?)\]\s*,\s*&\[(.*?)\]\s*,?\s*\)', body, re.S
    ):
        topic = entry.group(1)
        subjects = _rust_str_list(entry.group(2))
        actions = _rust_str_list(entry.group(3))
        clusters.append((topic, subjects, actions))
    if not clusters:
        raise SystemExit("CLUSTERS parsed empty — the pattern no longer matches")
    return clusters


def parse_stopwords(src: str) -> set:
    m = re.search(r"const STOPWORDS[^=]*=\s*&\[(.*?)\];", src, re.S)
    if not m:
        raise SystemExit("could not locate STOPWORDS in %s" % TRACK)
    return set(_rust_str_list(_strip_comments(m.group(1))))


def content_tokens(text: str, stopwords: set) -> list:
    """Mirror of `content_tokens`: lowercase alnum tokens len>=5, non-stopword,
    first-appearance order, deduplicated."""
    seen = set()
    out = []
    for raw in re.split(r"[^0-9A-Za-z]+", text):
        if len(raw) < 5:
            continue
        tok = raw.lower()
        if tok in stopwords:
            continue
        if tok not in seen:
            seen.add(tok)
            out.append(tok)
    return out


# ---------------------------------------------------------------- seeded store


def seeded_store(clusters: list, n: int) -> list:
    """Mirror of `seeded_store`: message i takes its cluster from i % 8 and its
    words from pure index arithmetic. `created_at` increases with i."""
    rows = []
    nclusters = len(clusters)
    for i in range(n):
        topic, subjects, actions = clusters[i % nclusters]
        j = i // nclusters
        subject = subjects[j % len(subjects)]
        slot = (j // len(subjects)) % len(actions)
        a0 = actions[slot]
        a1 = actions[(slot + 3) % len(actions)]
        a2 = actions[(slot + 6) % len(actions)]
        content = (
            f"{topic} {subject}: {a0} and {a1} after {a2} was not on the list, "
            f"then {subject} {a0} {a1} {a2} again with the {topic} work, "
            f"and the {a0} {a1} {a2} {subject} note is fine"
        )
        rows.append(
            {
                "id": f"m{i}",
                "session_id": f"session-{i % 5}",
                "role": "user",
                "content": content,
                "created_at": i,  # relative order is all the ranker reads
            }
        )
    return rows


# ---------------------------------------------------------------- sampling


def case_from(content, mid, session_id, counts):
    tokens = content_tokens(content, STOPWORDS)
    if len(tokens) < 2:
        return None
    specific = None
    for tok in tokens:
        n = counts.get(tok, 0)
        if n < 2:
            continue
        if specific is None or n < counts.get(specific, 0):
            specific = tok
    broad = None
    for tok in tokens:
        n = counts.get(tok, 0)
        if broad is None or n > counts.get(broad, 0):
            broad = tok
    if specific is None or broad is None or specific == broad:
        return None
    return {"query": f"{specific} {broad}", "message_id": mid, "session_id": session_id}


def cooccur(rows, a, b):
    return sum(1 for r in rows if a in r["content"].lower() and b in r["content"].lower())


def sample(rows, clusters):
    candidates = [
        r for r in rows if len(r["content"]) >= 160 and r["role"] in ("user", "assistant")
    ]
    candidates.sort(key=lambda r: (r["created_at"], r["id"]))
    counts = Counter()
    for r in candidates[:2000]:
        for tok in content_tokens(r["content"], STOPWORDS):
            counts[tok] += 1

    stride = max(len(candidates) // POSITIVES, 1)
    order = list(range(0, len(candidates), stride))
    order.extend(range(len(candidates)))
    positives = []
    for idx in order:
        if len(positives) == POSITIVES:
            break
        r = candidates[idx]
        if any(c["message_id"] == r["id"] for c in positives):
            continue
        case = case_from(r["content"], r["id"], r["session_id"], counts)
        if case:
            positives.append(case)

    pool = []
    for r in candidates:
        pool.extend(content_tokens(r["content"], STOPWORDS))
        if len(pool) >= 400:
            break
    pool = pool[:400]

    half = NEGATIVES // 2
    negatives = []
    i = 0
    while len(negatives) < half and i + 1 < len(pool):
        a, b = pool[i], pool[i + 1]
        if a != b and f"{a} {b}" not in negatives and cooccur(rows, a, b) > 0:
            negatives.append(f"{a} {b}")
        i += 1
    j = 0
    while len(negatives) < NEGATIVES and j + 7 < len(pool):
        a, b = pool[j], pool[j + 7]
        if a != b and f"{a} {b}" not in negatives and cooccur(rows, a, b) == 0:
            negatives.append(f"{a} {b}")
        j += 1
    return {"positives": positives, "negatives": negatives}


# ---------------------------------------------------------------- retrieval


def like(rows, tokens):
    """The AND-of-LIKE arms, recency ordered — the SQL pool both paths fetch."""
    hits = [r for r in rows if all(t in r["content"].lower() for t in tokens)]
    hits.sort(key=lambda r: -r["created_at"])
    return hits


def contiguous(rows, query, k):
    q = query.lower()
    hits = [r for r in rows if q in r["content"].lower()]
    hits.sort(key=lambda r: -r["created_at"])
    return hits[:k]


def tokenize(query):
    toks = sorted(set(query.split()))
    return toks[:8]


def score_baseline(rows, query, k):
    """CURRENT shipped score: total token occurrences, recency breaks ties."""
    toks = tokenize(query)
    if len(toks) < 2:
        return []
    pool = like(rows, toks)[: min(k * 5, 200)]
    scored = []
    for r in pool:
        lower = r["content"].lower()
        hits = sum(lower.count(t) for t in toks)
        scored.append((hits, r))
    scored.sort(key=lambda x: -x[0])  # stable -> recency preserved in ties
    return [r for _, r in scored[:k]]


# --------------------------------------------------- proposed second signal
#
# The shipped score has ONE relevance signal: how many times the query tokens
# occur in the row. Every tie then falls back to SQL recency. The proposal adds
# two cheap, query-derived signals that rank BELOW the hit count, so they can
# only ever break a tie — never override a better lexical match:
#
#   * `leading`   — how many query tokens have their FIRST occurrence inside the
#                   row's opening zone (the prefix before the first `:`/`.`/`?`/
#                   `!`/newline, capped at LEADING_ZONE characters). A message
#                   that OPENS with the query's words is about them; one that
#                   mentions them in passing is not.
#   * `adjacency` — the longest run of content tokens that appear adjacently in
#                   the row AND consecutively in the query's own order. Adjacent
#                   query words are a phrase; scattered ones are a bag of words.
#
# Both are deterministic, allocation-light and independent of the corpus, so
# they behave the same on a live store as on the fixture.

LEADING_ZONE = 60


def _first_index(content, tok):
    return content.find(tok)


def leading_hits(content, tokens):
    """Query tokens whose first occurrence sits in the row's opening zone."""
    end = len(content)
    for brk in (":", ".", "?", "!", "\n"):
        pos = content.find(brk)
        if pos != -1:
            end = min(end, pos)
    end = min(end, LEADING_ZONE)
    return sum(1 for t in tokens if 0 <= _first_index(content, t) < end)


def adjacency_run(content, tokens):
    """Longest run of consecutive content words that are consecutive, in order,
    in the query's token list."""
    words = [w for w in re.split(r"[^0-9A-Za-z]+", content.lower()) if w]
    order = {t: i for i, t in enumerate(tokens)}
    best = 0
    for start in range(len(words)):
        run = 0
        pos = start
        want = order.get(words[start])
        if want is None:
            continue
        while pos < len(words) and want is not None and want < len(tokens) and words[pos] == tokens[want]:
            run += 1
            pos += 1
            want += 1
        best = max(best, run)
    return best


def score_proposed(content, tokens):
    lower = content.lower()
    hits = sum(lower.count(t) for t in tokens)
    return (hits, leading_hits(lower, tokens), adjacency_run(lower, tokens))


def score_proposed_primary_only(content, tokens):
    """The proposal with the SECOND signal disabled — the 'goes RED' arm."""
    lower = content.lower()
    return (sum(lower.count(t) for t in tokens),)


def rank_with(rows, tokens, scorer):
    pool = like(rows, tokens)[: min(K * 5, 200)]
    scored = [(scorer(r["content"], tokens), r) for r in pool]
    scored.sort(key=lambda x: tuple(-v for v in x[0]))  # stable -> recency in full ties
    return [r for _, r in scored[:K]]


def search_proposed(rows, query, k):
    toks = tokenize(query)
    if len(toks) < 2:
        return []
    return rank_with(rows, toks, score_proposed)


def search_proposed_primary_only(rows, query, k):
    toks = tokenize(query)
    if len(toks) < 2:
        return []
    return rank_with(rows, toks, score_proposed_primary_only)


def prove_ceiling(rows, dataset):
    """Prove that the Track A fixture cannot discriminate the gold from its
    neighbours: for every MISSING case, print the feature tuple of every pool
    row and report whether the tuple is constant across the whole pool.

    If it is constant, no function of (content, query tokens) can separate the
    gold, and the recall figure is a fixture constant rather than a retriever
    measurement."""
    const = 0
    total = 0
    for case in dataset["positives"]:
        toks = tokenize(case["query"])
        pool = like(rows, toks)[: min(K * 5, 200)]
        ids = [r["id"] for r in pool]
        if case["message_id"] in ids[:K]:
            continue  # a hit — not a case the ceiling question is about
        total += 1
        feats = {score_proposed(r["content"], toks) for r in pool}
        if len(feats) == 1:
            const += 1
            only = next(iter(feats))
            print(f"  constant: {case['query']!r} pool={len(pool)} features={only}")
        else:
            print(f"  DISCRIMINABLE: {case['query']} pool={len(pool)} features={sorted(feats)}")
    print(f"\nmisses whose whole pool is feature-identical: {const}/{total}")
    return const == total


def evaluate(rows, dataset, ranker, k=K):
    found = 0
    rr = 0.0
    ranks = []
    for case in dataset["positives"]:
        ranked = ranker(rows, case["query"], k)
        ids = [r["id"] for r in ranked]
        if case["message_id"] in ids:
            pos = ids.index(case["message_id"])
            found += 1
            rr += 1.0 / (pos + 1)
            ranks.append(pos + 1)
    fp = sum(1 for q in dataset["negatives"] if ranker(rows, q, k))
    return {
        "recall": found / len(dataset["positives"]),
        "mrr": rr / len(dataset["positives"]),
        "found": found,
        "fp": fp,
        "fpr": fp / len(dataset["negatives"]),
        "ranks": ranks,
    }


def search_baseline(rows, query, k):
    """Mirror of `search_slice`: contiguous arm first, then tokenized."""
    first = contiguous(rows, query, k)
    if first:
        return first
    return score_baseline(rows, query, k)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--baseline", action="store_true", help="only reproduce the published baseline")
    args = ap.parse_args()

    src = open(TRACK, encoding="utf-8").read()
    global CLUSTERS, STOPWORDS
    CLUSTERS = parse_clusters(src)
    STOPWORDS = parse_stopwords(src)
    print(f"fixture: {len(CLUSTERS)} clusters, {len(STOPWORDS)} stopwords (parsed from the Rust source)")

    rows = seeded_store(CLUSTERS, 1000)
    dataset = sample(rows, CLUSTERS)
    print(f"dataset: {len(dataset['positives'])} positives, {len(dataset['negatives'])} negatives")

    base = evaluate(rows, dataset, search_baseline)
    print("\n== stage 1: CONTROL (current shipped score) ==")
    print(
        f"recall@{K}={base['recall']:.3f} ({base['found']}/{len(dataset['positives'])}) "
        f"MRR={base['mrr']:.3f} FPR={base['fpr']:.3f} ({base['fp']}/{len(dataset['negatives'])})"
    )
    print("ranks:", sorted(base["ranks"]))

    published = (0.8, 0.227, 0.5)
    got = (round(base["recall"], 3), round(base["mrr"], 3), round(base["fpr"], 3))
    if got != published:
        print(
            f"\nCONTROL FAILED: model yields {got}, published is {published}.\n"
            "The model of the algorithm is wrong — fix it before trusting any proposed term.",
            file=sys.stderr,
        )
        return 1
    print("CONTROL OK: reproduces the published v2 figures exactly.")

    if args.baseline:
        return 0

    proposed = evaluate(rows, dataset, search_proposed)
    primary_only = evaluate(rows, dataset, search_proposed_primary_only)
    print("\n== stage 2: PROPOSED (hits, then opening-zone, then adjacency) ==")
    print(
        f"recall@{K}={proposed['recall']:.3f} ({proposed['found']}/{len(dataset['positives'])}) "
        f"MRR={proposed['mrr']:.3f} FPR={proposed['fpr']:.3f} ({proposed['fp']}/{len(dataset['negatives'])})"
    )
    print("ranks:", sorted(proposed["ranks"]))
    print("\n== stage 2b: the same proposal with the SECOND signal DISABLED ==")
    print(
        f"recall@{K}={primary_only['recall']:.3f} ({primary_only['found']}/{len(dataset['positives'])}) "
        f"MRR={primary_only['mrr']:.3f}"
    )
    print(f"\ndelta: recall {proposed['recall'] - base['recall']:+.3f}, MRR {proposed['mrr'] - base['mrr']:+.3f}")

    print("\n== stage 3: can this fixture measure a relevance term at all? ==")
    constant = prove_ceiling(rows, dataset)
    if constant:
        print(
            "CEILING PROVEN: every pool row of every miss is identical on every\n"
            "query-derived feature, so 0.800 is the ceiling for ANY ranker that\n"
            "reads only (content, query tokens) on this fixture. The new term is\n"
            "therefore unmeasurable here by construction — not by weakness in the term."
        )
    return 0


if __name__ == "__main__":
    sys.exit(main())
