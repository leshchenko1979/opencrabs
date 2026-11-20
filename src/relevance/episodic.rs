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
//! The shipped rule has ONE relevance signal: [`Score::hits`], the total
//! number of times the query's tokens occur in the row. Every tie then falls
//! back to the order SQL returned — recency — so a row that ties with a dozen
//! newer neighbours is unreachable at `k` however well it matches.
//!
//! Two cheap, query-derived signals now rank BELOW `hits`, so they can only
//! ever break a tie and a better lexical match is never outranked by a
//! positional one:
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
//! # What this deliberately does NOT do
//!
//! It does not widen `k` and it does not change WHICH rows match — only the
//! order of rows that match equally well, and only where the row text says
//! something beyond "contains both words". On a corpus where every matching
//! row is lexically identical to the query (the Track A fixture's clustered
//! vocabulary, #899), every signal here is constant across the pool and the
//! order is unchanged by construction — see `src/eval/results/track-a-episodic-v3.md`.

/// How many characters of a row count as its opening ("subject") zone.
pub const LEADING_ZONE: usize = 60;

/// How many distinct tokens a query is allowed to contribute.
pub const MAX_QUERY_TOKENS: usize = 8;

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

/// The relevance of one row to one query. Ordered: the higher, the more
/// relevant, with the fields compared in declaration order — so `hits` is the
/// primary signal, `leading` breaks its ties, and `adjacency` breaks theirs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Score {
    /// Total occurrences of every query token, summed. The signal the shipped
    /// ranker has always used, kept primary so the new terms can never
    /// override a stronger lexical match.
    pub hits: usize,
    /// Query tokens whose FIRST occurrence sits in the row's opening zone.
    pub leading: usize,
    /// Longest run of adjacent content words that are consecutive, in order,
    /// in the query's token list.
    pub adjacency: usize,
}

/// Score one row against `terms`, which may be in any case.
#[must_use]
pub fn score(content: &str, terms: &[String]) -> Score {
    let lower_terms: Vec<String> = terms.iter().map(|t| t.to_lowercase()).collect();
    score_lower(content, &lower_terms)
}

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
    // NEUTER EXPERIMENT (not for merge): rank on `hits` alone so the
    // tie-break term is removed from the comparison while the field is
    // still computed. Expect `opening_zone_breaks_a_hit_tie` and
    // `adjacency_breaks_a_tie_on_equal_hits_and_leading` to go RED.
    scored.sort_by_key(|(score, _)| std::cmp::Reverse(score.hits));
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
        hits,
        leading: leading_hits(&lower, terms),
        adjacency: adjacency_run(&lower, terms),
    }
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
