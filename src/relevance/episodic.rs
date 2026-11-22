//! Episodic message recall: how relevant one stored message is to one query.
//!
//! # Why this module exists
//!
//! The shipped search (`MessageRepository::search_by_content_tokenized`) and
//! the Track A harness (`eval::episodic_track::search_slice`) each carried
//! their OWN copy of the scoring rule, and the copies had drifted: the
//! repository summed every occurrence of every query token, the harness
//! counted how many DISTINCT tokens were present. Same query over the same
//! row, two different numbers — so the harness was measuring a ranker the
//! binary does not ship (#911). Both now call [`rank`], so the only way the
//! two can disagree is if one of them stops calling it.
//!
//! # The signals
//!
//! The rule used to have ONE relevance signal: [`Score::hits`], the total
//! number of times the query's tokens occur in the row. Every tie then fell
//! back to the order SQL returned — recency — so a row that tied with a dozen
//! newer neighbours was unreachable at `k` however well it matched.
//!
//! #941 measured what that costs, and the measurement is the reason
//! [`Score::proximity`] now ranks ABOVE `hits` rather than below it: on the
//! live store, ranking by `(hits, leading, adjacency)` with an UNLIMITED
//! budget scored the same recall@10 as the shipped, budgeted ranker (0.053
//! both). A ranker that cannot beat itself given every row is not
//! under-tuned — its feature set is exhausted. Median 10 rows tied with the
//! gold, worst case 1,532, and in 19 of 38 cases the ties alone buried it past
//! `k = 10`. No reweighting of those three signals can help; a new signal is
//! required.
//!
//! [`Score::proximity`] is that signal: the character span from the first to
//! the last query term's first occurrence. A row where the query words sit
//! together is ABOUT them; one where they are three thousand characters apart
//! merely contains both. Occurrence counts are identical in the two cases, so
//! `hits` cannot see the difference.
//!
//! Measured through the SHIPPED semantics — same tokenizer, same 14-day slice,
//! 18,231 messages, 40 positives / 20 negatives — proximity takes recall@10
//! from 0.000 to 0.075 at the old `5 * k` pool and to 0.150 at the
//! [`TOKENIZED_FETCH_CAP`] pool this module now sizes. So the signal is real
//! AND the pool was a second binding constraint, which is why both moved.
//!
//! Two figures that circulated in this doc and on #941 are WITHDRAWN: a
//! `0.097 → 0.516` proximity gain, and the pool falsification that rode with
//! it (`50 vs 5,000 fetched: 0.100 → 0.100`). Both came from a probe carrying
//! its own tokenizer (`len < 5` filter plus a hand-written stopword list), so
//! they measured a ranker the binary does not ship — and the pool conclusion
//! was flatly wrong, as the sweep above shows. Length-normalised forms were
//! measured on that same probe and read worse (0.258 for `span/len`, 0.290 for
//! `span/sqrt(len)`); the raw span is what ships, but treat those two numbers
//! as indicative only for the same reason.
//!
//! BM25 and dropping the query's low-idf terms were also tried and did not
//! beat the kept form. Unlike the pool, neither is a constraint worth moving.
//!
//! The two older signals still rank below `hits`, where they cost nothing and
//! still break its remaining ties:
//!
//! * [`Score::leading`] — how many query tokens have their FIRST occurrence
//!   inside the row's opening zone (the prefix before the first `:`, `.`,
//!   `?`, `!` or newline, capped at [`LEADING_ZONE`] characters). A message
//!   that OPENS with the query's words is about them; one that mentions them
//!   in passing is not.
//! * [`Score::adjacency`] — the longest run of adjacent content words that are
//!   also consecutive in the query's own order. Adjacent query words are a
//!   phrase; scattered ones are a bag of words.
//!
//! Proximity was tried as a TIEBREAK first and changed nothing (0.097 → 0.097),
//! because `hits` already separates the rows it would have reordered. It is
//! primary or it is dead weight.
//!
//! # What this deliberately does NOT do
//!
//! It does not widen `k` and it does not change WHICH rows match — only how
//! rows that already match are ordered. The pool that feeds the ranker is
//! sized separately by [`tokenized_fetch_cap`] and is NOT `k`. On a corpus
//! where every matching row is lexically identical to the query (the Track A
//! fixture's clustered vocabulary, #899), every signal here is constant across
//! the pool and the order is unchanged by construction — see
//! `src/eval/results/track-a-episodic-v4.md`.

use std::cmp::Reverse;

/// How many characters of a row count as its opening ("subject") zone.
pub const LEADING_ZONE: usize = 60;

/// How many distinct tokens a query is allowed to contribute.
pub const MAX_QUERY_TOKENS: usize = 8;

/// The largest recency pool the tokenized search will fetch to re-rank.
///
/// The pool is what the ranker can even SEE: `search_by_content_tokenized`
/// fetches the newest `N` matching rows and orders those, so a target outside
/// the pool is unreachable however well it scores. Measured on the live store
/// (#941), recall@10 by pool size: 50 → 0.075, 100 → 0.125, **200 → 0.150**,
/// unlimited → 0.150. It saturates at 200, and the median AND-arm pool is 171,
/// so 200 covers the typical case outright rather than truncating it.
///
/// This lives here, not in either caller, because the shipped search and the
/// Track A harness must fetch the SAME pool — a harness that sizes its own
/// pool measures a ranker the binary does not ship, which is the #911 defect
/// this module exists to prevent.
pub const TOKENIZED_FETCH_CAP: usize = 200;

/// The recency pool to fetch for a caller that wants `limit` ranked rows.
///
/// Wide on purpose: [`TOKENIZED_FETCH_CAP`] is where recall stops improving,
/// so a `k`-row request fetches that many candidates and re-ranks them down.
/// A caller asking for more than the cap gets the cap (a 200-row re-rank is
/// the documented bound, not an accident).
#[must_use]
pub fn tokenized_fetch_cap(limit: usize) -> i64 {
    limit.saturating_mul(20).clamp(1, TOKENIZED_FETCH_CAP) as i64
}

/// The tokens a query contributes to the SQL `LIKE` arms: lowercased, sorted,
/// deduped, capped at [`MAX_QUERY_TOKENS`].
///
/// Sorted because a `WHERE` clause has no order — the sort exists so two
/// spellings of the same query build byte-identical SQL, and the cap stops a
/// pasted paragraph from becoming a fifty-arm conjunction.
#[must_use]
pub fn query_tokens(query: &str) -> Vec<String> {
    let mut tokens: Vec<String> = query.split_whitespace().map(|t| t.to_lowercase()).collect();
    tokens.sort();
    tokens.dedup();
    tokens.truncate(MAX_QUERY_TOKENS);
    tokens
}

/// The same query's tokens in the QUERY'S OWN ORDER (deduped, capped) — what
/// the positional signals read.
///
/// This is a separate list from [`query_tokens`] because adjacency is defined
/// against word order and `sort()` destroys it: on a sorted list "fix the
/// flaky deploy" becomes `[deploy, fix, flaky, the]` and no two adjacent
/// content words can ever be consecutive in it, so the signal would be a
/// constant and the term would be dead code wearing a metric.
#[must_use]
pub fn query_terms(query: &str) -> Vec<String> {
    let mut terms: Vec<String> = Vec::new();
    for token in query.split_whitespace() {
        let token = token.to_lowercase();
        if token.is_empty() || terms.contains(&token) {
            continue;
        }
        terms.push(token);
        if terms.len() == MAX_QUERY_TOKENS {
            break;
        }
    }
    terms
}

/// The relevance of one row to one query.
///
/// **Field order IS the ranking.** The type derives `PartialOrd`/`Ord`, which
/// compares fields in declaration order, and [`rank`] sorts by `Reverse<Score>`
/// — so the FIRST field below is the primary signal and each later field only
/// breaks the tie the ones before it left. Reordering these fields silently
/// reorders every search result; that is why the order is stated here rather
/// than left to be inferred from the call site.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Score {
    /// How tightly the query's terms cluster: the character span from the
    /// first term's first occurrence to the last one's, INVERTED (via
    /// [`Reverse`]) so that a tighter cluster compares GREATER and the derived
    /// ascending `Ord` puts it first. [`NO_PROXIMITY`] when a term is missing.
    ///
    /// Primary since #941: `hits` cannot tell "both words are here, next to
    /// each other" from "both words are here, three thousand characters
    /// apart", and on a corpus with shared vocabulary that distinction is the
    /// only one that survives.
    pub proximity: Reverse<usize>,
    /// Total occurrences of every query token, summed. Demoted from primary to
    /// proximity's tiebreak in #941, where it was measured to be incapable of
    /// discriminating: 1,532 rows could tie on it.
    pub hits: usize,
    /// Query tokens whose FIRST occurrence sits in the row's opening zone.
    pub leading: usize,
    /// Longest run of adjacent content words that are consecutive, in order,
    /// in the query's token list.
    pub adjacency: usize,
}

/// The value of [`Score::proximity`] when the span is UNDEFINED — because a
/// query term is missing from the row (or there are no terms at all).
///
/// `Reverse(usize::MAX)` is the smallest possible `proximity`, so it sorts
/// last. That is the explicit guard behind the rule "a row missing a term must
/// never outrank one that contains all of them": a partial match has no span,
/// and calling that `0` — the tightest possible cluster — would put every
/// half-match at the top of the list.
pub const NO_PROXIMITY: Reverse<usize> = Reverse(usize::MAX);

/// Score one row against `terms`, which may be in any case.
#[must_use]
pub fn score(content: &str, terms: &[String]) -> Score {
    let lower_terms: Vec<String> = terms.iter().map(|t| t.to_lowercase()).collect();
    score_lower(content, &lower_terms)
}

/// What a leg publishes about itself when it grades THIS ranker.
///
/// A provenance string is how a reader decides what a number measures, so it
/// lives beside the code it names rather than in whichever command happens to
/// print it. #911 replaced the ranker-less baseline with [`rank`] and left the
/// caller's label asserting "no message ranker exists — #855": the same
/// two-ranker-under-one-name defect, one surface over (#927). One constant,
/// one reader-facing home — a leg that calls this module quotes this string.
pub const RANKER_LABEL: &str = "shipped: contiguous LIKE (SQL recency order); a multi-word \
     miss falls back to tokenized AND re-ranked by relevance::episodic::rank — \
     (proximity span, hits, leading, adjacency), ties to SQL recency";

/// Order `rows` (already in the caller's fallback order — SQL recency) by
/// relevance, best first. The sort is STABLE, so rows that tie on every signal
/// keep the order they arrived in, which is exactly the shipped fallback.
///
/// `content` extracts a row's searchable text, so a caller that already holds
/// the rows does not have to clone their bodies into this module.
pub fn rank<T, F>(rows: Vec<T>, terms: &[String], content: F) -> Vec<T>
where
    F: Fn(&T) -> &str,
{
    let lower_terms: Vec<String> = terms.iter().map(|t| t.to_lowercase()).collect();
    let mut scored: Vec<(Score, T)> = rows
        .into_iter()
        .map(|row| (score_lower(content(&row), &lower_terms), row))
        .collect();
    scored.sort_by_key(|(score, _)| std::cmp::Reverse(*score));
    scored.into_iter().map(|(_, row)| row).collect()
}

fn score_lower(content: &str, terms: &[String]) -> Score {
    let lower = content.to_lowercase();
    let hits = terms
        .iter()
        .filter(|t| !t.is_empty())
        .map(|t| lower.matches(t.as_str()).count())
        .sum();
    Score {
        proximity: proximity_span(&lower, terms),
        hits,
        leading: leading_hits(&lower, terms),
        adjacency: adjacency_run(&lower, terms),
    }
}

/// How tightly the query's terms cluster: the character span from the first
/// term's first occurrence to the last one's, INVERTED so a tighter cluster
/// compares GREATER (#941).
///
/// "First occurrence" is deliberate. A row that mentions `deploy` twice, the
/// second time next to `flaky`, is not about the pair any more than one that
/// mentions it once — the span is where the query's words FIRST come together
/// or it is nothing. Using the closest pair of occurrences instead was not
/// measured and is not what this returns.
///
/// [`NO_PROXIMITY`] when a non-empty term is missing: a partial match has no
/// span, and scoring it `0` would hand the tightest possible value to every
/// row that matches only half the query. That guard is what keeps "contains
/// all the words, far apart" ahead of "contains one word, often".
///
/// A single term (or no terms) has no span to measure and ties at
/// `Reverse(0)` / [`NO_PROXIMITY`] respectively, so single-word queries fall
/// through to `hits` exactly as they always did.
fn proximity_span(lower: &str, terms: &[String]) -> Reverse<usize> {
    let mut lo = usize::MAX;
    let mut hi = 0usize;
    let mut seen = false;
    for term in terms.iter().filter(|t| !t.is_empty()) {
        let Some(pos) = lower.find(term.as_str()) else {
            return NO_PROXIMITY;
        };
        seen = true;
        lo = lo.min(pos);
        hi = hi.max(pos);
    }
    if seen { Reverse(hi - lo) } else { NO_PROXIMITY }
}

/// Byte offset at which a row's opening zone ends: the first sentence break,
/// or [`LEADING_ZONE`] characters in, whichever comes first.
fn leading_end(lower: &str) -> usize {
    let mut end = lower.len();
    for brk in [':', '.', '?', '!', '\n'] {
        if let Some(pos) = lower.find(brk) {
            end = end.min(pos);
        }
    }
    // Char-safe: a byte slice at an arbitrary offset could split a multi-byte
    // character, so the length budget is resolved through `char_indices`.
    let zone = lower
        .char_indices()
        .nth(LEADING_ZONE)
        .map_or(lower.len(), |(idx, _)| idx);
    end.min(zone)
}

fn leading_hits(lower: &str, terms: &[String]) -> usize {
    let end = leading_end(lower);
    terms
        .iter()
        .filter(|t| !t.is_empty() && lower.find(t.as_str()).is_some_and(|pos| pos < end))
        .count()
}

fn adjacency_run(lower: &str, terms: &[String]) -> usize {
    let words: Vec<&str> = lower
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .collect();
    let mut best = 0;
    for (start, word) in words.iter().enumerate() {
        let Some(mut want) = terms.iter().position(|t| t.as_str() == *word) else {
            continue;
        };
        let mut pos = start;
        let mut run = 0;
        while let Some(cur) = words.get(pos) {
            match terms.get(want) {
                Some(token) if token.as_str() == *cur => {
                    run += 1;
                    pos += 1;
                    want += 1;
                }
                _ => break,
            }
        }
        best = best.max(run);
    }
    best
}
