//! The shared episodic relevance scorer (#911, #941).
//!
//! Three things are being pinned here, and they are different claims:
//!
//! 1. the ranking RULE does what the design says — a row whose query terms sit
//!    closer together outranks one that scatters them (#941), a row that OPENS
//!    with the query's words outranks one that does not at EQUAL proximity and
//!    hit count, a stronger lexical match breaks a proximity tie, and a row
//!    missing one of the query terms never outranks a row containing them all;
//! 2. the rule is actually SHARED — the shipped repository path and the Track
//!    A harness return the same order for the same input, which is the defect
//!    #911 names. Before the shared module they did not: the repository summed
//!    every occurrence of a token while the harness counted distinct tokens,
//!    so the leg was measuring a ranker the binary does not ship; and
//! 3. the label a leg publishes names the scorer it grades (#927).
//!
//! The proximity tests are the ones that fail if the primary term is deleted:
//! each asserts the expected order AND that the input order had the losing row
//! first, so a ranker that keeps the stable input order returns the opposite
//! result.

use crate::db::Database;
use crate::db::models::{Message, Session};
use crate::db::repository::SessionRepository;
use crate::db::repository::message::MessageRepository;
use crate::eval::episodic_track::search_slice;
use crate::relevance::episodic::{self, LEADING_ZONE, MAX_QUERY_TOKENS, NO_PROXIMITY};
use rusqlite::Connection;
use std::cmp::Reverse;
use uuid::Uuid;

/// A long, punctuation-free lead-in. Two properties matter and both are
/// deliberate: it is longer than [`LEADING_ZONE`] (so a token placed after it
/// sits OUTSIDE the opening zone) and it contains no `:`/`.`/`?`/`!`/newline
/// (so the zone is bounded by the character budget rather than a sentence
/// break, which is the harder of the two cases to get right).
const LONG_LEAD_IN: &str = "the service was rebuilt twice yesterday evening and the \
                            rollback finished before anyone noticed and the cache ";

// ─── the rule ───────────────────────────────────────────────────────────────

/// The load-bearing test for the new term. Both rows contain each query token
/// exactly once, so `hits` TIES at 2 and cannot decide anything; only the
/// opening-position signal separates them. The rows are supplied
/// newest-first, which is the order SQL returns and therefore the order a
/// hits-only ranker would keep — so a ranker without the term returns the
/// scattered row first and this test fails.
#[test]
fn opening_zone_breaks_a_hit_tie() {
    let terms = episodic::query_terms("deploy flaky");

    // Newest first, as `ORDER BY created_at DESC` delivers them.
    let scattered = format!("{LONG_LEAD_IN}deploy flaky");
    let opening = "deploy/flaky: rollback notes";
    let rows = vec![scattered.clone(), opening.to_string()];

    let scattered_score = episodic::score(&scattered, &terms);
    let opening_score = episodic::score(opening, &terms);
    assert_eq!(
        scattered_score.hits, opening_score.hits,
        "this test is only meaningful at EQUAL hit count: {scattered_score:?} vs {opening_score:?}"
    );
    assert_eq!(
        scattered_score.leading, 0,
        "the scattered row must have no token inside the opening zone: {scattered_score:?}"
    );
    assert_eq!(
        opening_score.leading, 2,
        "both query tokens open the other row: {opening_score:?}"
    );

    let ranked = episodic::rank(rows.clone(), &terms, String::as_str);
    assert_eq!(
        ranked.first().map(String::as_str),
        Some(opening),
        "the row that OPENS with the query words must rank first at equal hits; \
         a ranker without the opening-position term keeps the input order and \
         returns {scattered:?} first"
    );
    // The input order is what a hits-only ranker would return, so asserting
    // the output differs from it is what makes this test RED without the term.
    assert_ne!(
        ranked, rows,
        "the output equals the input order — the tie-break term did not fire"
    );
}

/// Adjacency is a separate signal from the opening zone AND from proximity:
/// the query's words appear in the SAME order in one row and reversed in the
/// other, both inside the opening zone, at equal hit count, and with the SAME
/// proximity span (the extra spaces in `phrase` pad it to the reversed row's
/// span, so only `adjacency` can separate them).
#[test]
fn adjacency_breaks_a_tie_on_equal_hits_and_leading() {
    let terms = episodic::query_terms("fix flaky deploy");
    // "fix" (3) + 4 spaces + "flaky" (5) + 1 + "deploy" (6) = span 13,
    // the same span "deploy flaky fix" has: 6 + 1 + 5 + 1 = 13.
    let phrase = "fix    flaky deploy";
    let reversed = "deploy flaky fix";

    let phrase_score = episodic::score(phrase, &terms);
    let reversed_score = episodic::score(reversed, &terms);
    assert_eq!(phrase_score.hits, reversed_score.hits, "hits must tie");
    assert_eq!(
        phrase_score.leading, reversed_score.leading,
        "the opening-zone signal must tie too, or adjacency is not what decided it"
    );
    assert_eq!(
        phrase_score.proximity, reversed_score.proximity,
        "the span must tie too, or proximity is what decided it: \
         {phrase_score:?} vs {reversed_score:?}"
    );
    assert_eq!(phrase_score.adjacency, 3, "{phrase_score:?}");
    assert_eq!(reversed_score.adjacency, 1, "{reversed_score:?}");

    let ranked = episodic::rank(
        vec![reversed.to_string(), phrase.to_string()],
        &terms,
        String::as_str,
    );
    assert_eq!(ranked.first().map(String::as_str), Some(phrase));
}

/// Criterion 2 of #941, literally: two rows differing ONLY in term proximity
/// are ordered by it, and the row whose terms are 3,000 characters apart loses
/// to the one where they are adjacent.
///
/// The two rows are built so proximity is the ONLY field that differs — same
/// hit count, both terms outside the opening zone, and the same adjacency run
/// (whitespace collapses under the word split, so 3,000 spaces are one
/// separator, not 3,000 words). If proximity were dropped, the stable sort
/// would keep the input order and return the far row first.
#[test]
fn proximity_orders_rows_at_equal_hits_leading_and_adjacency() {
    let terms = episodic::query_terms("deploy flaky");

    // Newest first, as `ORDER BY created_at DESC` delivers them: the WRONG row
    // leads the input, so only the ranker can fix it.
    let wide_gap = " ".repeat(3_000);
    let far = format!("{LONG_LEAD_IN}deploy{wide_gap}flaky");
    let near = format!("{LONG_LEAD_IN}deploy flaky");
    let rows = vec![far.clone(), near.clone()];

    let far_score = episodic::score(&far, &terms);
    let near_score = episodic::score(&near, &terms);
    assert_eq!(
        far_score.hits, near_score.hits,
        "precondition: equal hit count, so `hits` cannot decide this: {far_score:?} vs {near_score:?}"
    );
    assert_eq!(
        (far_score.leading, far_score.adjacency),
        (near_score.leading, near_score.adjacency),
        "precondition: leading and adjacency must TIE, or proximity is not what decided it: \
         {far_score:?} vs {near_score:?}"
    );

    // The span is the character distance between the two first occurrences,
    // so the lead-in cancels out and only the gap between the terms remains.
    assert_eq!(
        near_score.proximity,
        Reverse("deploy".len() + 1),
        "adjacent row: one space between the terms: {near_score:?}"
    );
    assert_eq!(
        far_score.proximity,
        Reverse("deploy".len() + 3_000),
        "far row: 3,000 characters between the terms: {far_score:?}"
    );
    assert!(
        near_score.proximity > far_score.proximity,
        "a tighter span must compare GREATER — that is what makes it the primary \
         signal under the derived ascending Ord: {near_score:?} vs {far_score:?}"
    );

    let ranked = episodic::rank(rows.clone(), &terms, String::as_str);
    assert_eq!(
        ranked.first().map(String::as_str),
        Some(near.as_str()),
        "the row whose query terms sit together must rank first; a ranker \
         without the proximity term keeps the input order and returns {far:?} first"
    );
    assert_ne!(
        ranked, rows,
        "the output equals the input order — the proximity term did not fire"
    );
}

/// Criterion 3 of #941: a row MISSING one of the query terms must never
/// outrank a row that contains all of them, however many times the partial
/// match repeats the term it does have.
///
/// This is the guard that makes the span safe to rank on: a missing term has no
/// span, and if "no span" scored as 0 — the tightest possible cluster — every
/// half-match would lead the list. [`NO_PROXIMITY`] is the explicit worst value
/// instead.
#[test]
fn a_row_missing_a_query_term_never_outranks_a_full_match() {
    let terms = episodic::query_terms("deploy flaky");

    // Ten occurrences of "deploy", none of "flaky": a big hit count, no span.
    let ten_deploys = "deploy ".repeat(10);
    let partial = format!("{LONG_LEAD_IN}{ten_deploys}");
    // Both terms, once each, 3,000 characters apart: the worst FULL span.
    let wide_gap = " ".repeat(3_000);
    let full = format!("{LONG_LEAD_IN}deploy{wide_gap}flaky");

    let partial_score = episodic::score(&partial, &terms);
    let full_score = episodic::score(&full, &terms);
    assert!(
        partial_score.hits > full_score.hits,
        "precondition: the partial row must have the BIGGER hit count, or the \
         guard is untested: {partial_score:?} vs {full_score:?}"
    );
    assert_eq!(
        partial_score.proximity, NO_PROXIMITY,
        "a missing term has no span and must take the explicit worst value: {partial_score:?}"
    );

    let ranked = episodic::rank(vec![partial.clone(), full.clone()], &terms, String::as_str);
    assert_eq!(
        ranked.first().map(String::as_str),
        Some(full.as_str()),
        "a row containing ALL the query terms must outrank one that contains \
         only some of them, even a much more frequent some: {partial_score:?} vs {full_score:?}"
    );
}

/// `hits` still decides, one level down: at EQUAL proximity it orders the
/// rows, so a stronger lexical match wins the tie the span left.
#[test]
fn hits_breaks_a_proximity_tie() {
    let terms = episodic::query_terms("deploy flaky");
    // Same span (both start at "deploy" and put "flaky" one space later);
    // the second row repeats "deploy" and so has one more hit.
    let one = "deploy flaky: notes";
    let more = "deploy flaky deploy: notes";

    let one_score = episodic::score(one, &terms);
    let more_score = episodic::score(more, &terms);
    assert_eq!(
        one_score.proximity, more_score.proximity,
        "precondition: the spans must TIE: {one_score:?} vs {more_score:?}"
    );
    assert!(
        more_score.hits > one_score.hits,
        "precondition: the second row must have more hits: {one_score:?} vs {more_score:?}"
    );

    let ranked = episodic::rank(
        vec![one.to_string(), more.to_string()],
        &terms,
        String::as_str,
    );
    assert_eq!(
        ranked.first().map(String::as_str),
        Some(more),
        "at equal proximity the stronger lexical match must lead"
    );
}

/// The length-bias objection, pinned as a CHOICE rather than left implicit.
///
/// Two rows put their query terms the SAME absolute distance apart — one
/// space, so the same 7-character span — but one is more than ten times longer
/// than the other. The shipped span is absolute, so the two scores are
/// IDENTICAL and the ranker keeps the order it was given. That equality IS the
/// claim: a length divisor anywhere in `proximity` would separate these two
/// rows and this test would fail.
///
/// The measured reason the divisor is not there is in the module docs — every
/// normalised form scored worse (0.075 for `span/len`, 0.125 for
/// `span/sqrt(len)`, against 0.150 for the raw span), because on the tuned
/// corpus the gold rows are shorter than their neighbours, so dividing by
/// length removes signal rather than bias.
#[test]
fn span_ignores_row_length() {
    let terms = episodic::query_terms("deploy flaky");

    // Same terms, same one-space gap, both outside the opening zone.
    let short = format!("{LONG_LEAD_IN}deploy flaky");
    // The SAME text with neutral filler appended AFTER the last term: the
    // span, the hit count and the adjacency run are untouched; only length
    // changes. Filler must not contain a query term or it moves `hits`.
    let filler = "the rollback finished quietly and nothing else moved ".repeat(40);
    // The separator is load-bearing: without it `flaky` and the filler's first
    // word glue into `flakythe`, which breaks the adjacency run (2 → 1) while
    // leaving `hits` and the span untouched — the test then fails on a
    // fixture bug rather than on the claim it exists to pin.
    let long = format!("{short} {filler}");

    assert!(
        long.len() > short.len() * 10,
        "precondition: the second row must be an order of magnitude longer: {} vs {}",
        long.len(),
        short.len()
    );

    let short_score = episodic::score(&short, &terms);
    let long_score = episodic::score(&long, &terms);
    assert_eq!(
        short_score, long_score,
        "an absolute span must not see row length at all: {short_score:?} vs {long_score:?}"
    );

    // Non-vacuous, and it records WHICH WAY the declined correction would go:
    // dividing by length would prefer the long row here by an order of
    // magnitude, because the same absolute gap is a far smaller fraction of a
    // long row. That is a real reordering, not a rounding difference.
    let span = short_score.proximity.0;
    let short_ratio = span as f64 / short.len() as f64;
    let long_ratio = span as f64 / long.len() as f64;
    assert!(
        short_ratio > long_ratio * 10.0,
        "a length divisor would reorder these rows ({short_ratio} vs {long_ratio}) — \
         the shipped rule declines that correction deliberately"
    );

    // At a full tie the stable sort keeps the caller's recency order, which is
    // the documented fallback: length buys neither row a position.
    let rows = vec![long.clone(), short.clone()];
    assert_eq!(
        episodic::rank(rows.clone(), &terms, String::as_str),
        rows,
        "at equal absolute tightness neither row may be promoted by its length"
    );
}

/// The ordering the shipped rule actually implements, stated so that changing
/// it is a decision rather than an accident: at the SAME RELATIVE tightness —
/// the span as a fraction of the row — the shorter row ranks NO WORSE. Under
/// an absolute span it ranks strictly better, because the same fraction of a
/// smaller row is a smaller number of characters.
///
/// The two rows are built so the ratios are EXACTLY equal (checked by integer
/// cross-multiplication, no floats), so a length-normalised ranker would tie
/// them and could not produce the order asserted here. Both rows contain each
/// term once, neither term is in the opening zone, and the adjacency run is 2
/// in both, so `hits`, `leading` and `adjacency` all tie and proximity is the
/// only field that can order them.
#[test]
fn length_bias_ranks_the_shorter_row_no_worse() {
    let terms = episodic::query_terms("deploy flaky");
    let gap_short = 40usize;

    let short = format!("{LONG_LEAD_IN}deploy{}{}", " ".repeat(gap_short), "flaky");
    // span = len("deploy") + gap, so 20× the short span needs
    // 20 * (6 + gap_short) - 6 characters of gap.
    let span_short = "deploy".len() + gap_short;
    let gap_long = 20 * span_short - "deploy".len();
    let long_body = format!("{LONG_LEAD_IN}deploy{}{}", " ".repeat(gap_long), "flaky");
    // Pad the tail so the long row is EXACTLY 20× the short one — that is what
    // makes span/length identical for the two, so a divisor would tie them.
    let target = 20 * short.len();
    assert!(
        target > long_body.len(),
        "the long row must be padded, not truncated"
    );
    let long = format!("{long_body}{}", " ".repeat(target - long_body.len()));

    let short_score = episodic::score(&short, &terms);
    let long_score = episodic::score(&long, &terms);

    assert_eq!(
        (short_score.hits, short_score.leading, short_score.adjacency),
        (long_score.hits, long_score.leading, long_score.adjacency),
        "precondition: hits, leading and adjacency must TIE, or proximity is not \
         what decided it: {short_score:?} vs {long_score:?}"
    );

    let span_long = span_short * 20;
    assert_eq!(
        short_score.proximity,
        Reverse(span_short),
        "{short_score:?}"
    );
    assert_eq!(long_score.proximity, Reverse(span_long), "{long_score:?}");

    // Same relative tightness, exactly: span_short / len_short == span_long / len_long.
    assert_eq!(
        span_short * long.len(),
        span_long * short.len(),
        "precondition: the two rows must be equally TIGHT relative to their own \
         length, so only an absolute span can separate them"
    );

    assert!(
        short_score.proximity > long_score.proximity,
        "the shorter row must rank NO WORSE — here strictly better: \
         {short_score:?} vs {long_score:?}"
    );

    // Newest-first input, as SQL returns it: the wrong row leads, so a ranker
    // that kept the input order would fail this.
    let rows = vec![long.clone(), short.clone()];
    let ranked = episodic::rank(rows.clone(), &terms, String::as_str);
    assert_eq!(
        ranked.first().map(String::as_str),
        Some(short.as_str()),
        "at equal relative tightness the shorter row must lead; got {ranked:?}"
    );
    assert_ne!(
        ranked, rows,
        "the output equals the input order — nothing fired"
    );
}

/// [`NO_PROXIMITY`] is the SMALLEST value, so a row missing a term sorts last
/// even when every tiebreak field is maximal. This is the pure-ordering form of
/// the guard `a_row_missing_a_query_term_never_outranks_a_full_match` asserts
/// at the rank level: no hit count, however large, can lift a partial match
/// above a full one, because proximity is compared first and
/// `Reverse(usize::MAX)` is the floor of the type.
#[test]
fn no_proximity_sorts_last_against_any_finite_span() {
    assert_eq!(
        NO_PROXIMITY,
        Reverse(usize::MAX),
        "the guard must be the extreme value, not a sentinel guess that a long \
         enough row could reach"
    );

    let maximal_tiebreaks = episodic::Score {
        proximity: NO_PROXIMITY,
        hits: usize::MAX,
        leading: usize::MAX,
        adjacency: usize::MAX,
    };
    let worst_full_match = episodic::Score {
        // One short of the extreme: the widest span a real row could produce
        // without colliding with the sentinel.
        proximity: Reverse(usize::MAX - 1),
        hits: 0,
        leading: 0,
        adjacency: 0,
    };
    assert!(
        maximal_tiebreaks < worst_full_match,
        "a partial match must sort last even with maximal hits/leading/adjacency: \
         {maximal_tiebreaks:?} vs {worst_full_match:?}"
    );

    // At the rank level, with a wider gap than the sibling test uses: fifty
    // occurrences of one term still lose to a single full match whose terms sit
    // 5,000 characters apart.
    let terms = episodic::query_terms("deploy flaky");
    let partial = format!("{LONG_LEAD_IN}{}", "deploy ".repeat(50));
    let full = format!("{LONG_LEAD_IN}deploy{}flaky", " ".repeat(5_000));
    let partial_score = episodic::score(&partial, &terms);
    let full_score = episodic::score(&full, &terms);
    assert_eq!(
        partial_score.proximity, NO_PROXIMITY,
        "a missing term has no span: {partial_score:?}"
    );
    assert!(
        partial_score.hits > full_score.hits,
        "precondition: the partial row must out-hit the full one: \
         {partial_score:?} vs {full_score:?}"
    );
    assert_eq!(
        episodic::rank(vec![partial, full.clone()], &terms, String::as_str)
            .first()
            .map(String::as_str),
        Some(full.as_str()),
        "the full match must lead despite a 5,000-character span"
    );
}

/// Edge cases that would otherwise panic or silently return a constant.
#[test]
fn edge_cases_are_total() {
    let empty = episodic::score("anything at all", &[]);
    assert_eq!(
        empty,
        episodic::Score {
            proximity: NO_PROXIMITY,
            hits: 0,
            leading: 0,
            adjacency: 0
        }
    );

    let terms = episodic::query_terms("deploy");
    assert_eq!(terms, vec!["deploy".to_string()]);

    // A single token: adjacency can be at most 1, and only if it leads. Its
    // span is 0 — one term has nothing to be far from — so the proximity term
    // is constant across the pool and `hits` decides, exactly as before #941.
    let single = episodic::score("deploy now", &terms);
    assert_eq!(single.hits, 1);
    assert_eq!(single.leading, 1);
    assert_eq!(single.adjacency, 1);
    assert_eq!(single.proximity, Reverse(0));

    // No break character anywhere: the zone is the character budget, not the
    // end of the string. A token past the budget must NOT count as leading.
    let past_zone = format!("{LONG_LEAD_IN}deploy");
    assert!(past_zone.len() > LEADING_ZONE);
    assert_eq!(episodic::score(&past_zone, &terms).leading, 0);

    // A row shorter than the budget is entirely opening zone.
    assert_eq!(episodic::score("deploy", &terms).leading, 1);

    // Empty content and a query that matches nothing are both zero, not a panic
    // — and the missing term takes the explicit no-span value rather than a
    // tight `0`.
    assert_eq!(
        episodic::score("", &terms),
        episodic::Score {
            proximity: NO_PROXIMITY,
            hits: 0,
            leading: 0,
            adjacency: 0
        }
    );
}

/// The tokenizer: the SQL-arm list is sorted and deduped, the scoring list
/// keeps the query's own order, and both are capped.
#[test]
fn the_two_token_lists_differ_in_the_way_adjacency_needs() {
    let arms = episodic::query_tokens("deploy flaky deploy");
    assert_eq!(arms, vec!["deploy".to_string(), "flaky".to_string()]);

    let terms = episodic::query_terms("fix the flaky deploy");
    assert_eq!(
        terms,
        vec![
            "fix".to_string(),
            "the".to_string(),
            "flaky".to_string(),
            "deploy".to_string()
        ],
        "the scoring list must preserve the query's word order"
    );

    let many = (0..MAX_QUERY_TOKENS + 4)
        .map(|i| format!("w{i}"))
        .collect::<Vec<_>>()
        .join(" ");
    assert_eq!(episodic::query_terms(&many).len(), MAX_QUERY_TOKENS);
    assert_eq!(episodic::query_tokens(&many).len(), MAX_QUERY_TOKENS);
}

// ─── the shared rule ────────────────────────────────────────────────────────

/// Rows seeded into BOTH stores. The order they are returned in is
/// `created_at DESC`, so `newest` comes first and `oldest` last.
#[derive(Clone)]
struct Row {
    id: String,
    content: String,
    created_at: i64,
}

/// The rows are shaped so the OLD drift would show: the repository summed
/// every occurrence of every token, the harness counted DISTINCT tokens. `many`
/// contains "deploy" twice, so the two rules disagree on it — sum ranks it
/// first, distinct ranks it last.
fn rows() -> Vec<Row> {
    let now = chrono::Utc::now().timestamp();
    vec![
        // newest — scattered, one mention of each token
        Row {
            id: Uuid::new_v4().to_string(),
            content: format!("{LONG_LEAD_IN}deploy came back clean while a flaky retry ran"),
            created_at: now - 10,
        },
        // middle — scattered, "deploy" twice: sum hits 3, distinct 2
        Row {
            id: Uuid::new_v4().to_string(),
            content: format!(
                "{LONG_LEAD_IN}deploy came back and the deploy was clean while a flaky retry ran"
            ),
            created_at: now - 20,
        },
        // oldest — both tokens OPEN the row: the positional maximum
        Row {
            id: Uuid::new_v4().to_string(),
            content: "deploy/flaky: rollback notes".to_string(),
            created_at: now - 30,
        },
    ]
}

/// Criterion 2 of #911, literally: the shipped runtime path and the eval
/// harness return the SAME ordering for the same input.
///
/// Two independent stores are seeded with byte-identical rows — a real
/// `Database` behind `MessageRepository`, and a bare `rusqlite` connection
/// behind `search_slice` — so neither path can borrow the other's state. The
/// query never appears contiguously in any row, so both legs reach the answer
/// through the tokenized arm of the union; the union path itself (both arms
/// populated) is pinned by the two tests below.
#[tokio::test]
async fn runtime_and_eval_paths_return_the_same_order() {
    const QUERY: &str = "deploy flaky";
    const K: usize = 10;
    let seeded = rows();

    // ── the runtime path ──
    let db = Database::connect_in_memory()
        .await
        .expect("in-memory database");
    db.run_migrations().await.expect("migrations");
    let session_repo = SessionRepository::new(db.pool().clone());
    let message_repo = MessageRepository::new(db.pool().clone());

    let session = Session::new(Some("relevance".to_string()), None, None);
    session_repo.create(&session).await.expect("session");

    for (i, row) in seeded.iter().enumerate() {
        let mut message = Message::new(
            session.id,
            "user".to_string(),
            row.content.clone(),
            i as i32,
        );
        message.id = Uuid::parse_str(&row.id).expect("uuid");
        message.created_at =
            chrono::DateTime::from_timestamp(row.created_at, 0).expect("timestamp");
        message_repo.create(&message).await.expect("message");
    }

    let shipped: Vec<String> = message_repo
        .search_by_content_union(None, QUERY, K)
        .await
        .expect("union search")
        .into_iter()
        .map(|candidate| candidate.row.id.to_string())
        .collect();

    // ── the eval harness, on its own store ──
    let conn = Connection::open_in_memory().expect("harness store");
    conn.execute_batch(
        "CREATE TABLE messages (
            id TEXT PRIMARY KEY NOT NULL,
            session_id TEXT NOT NULL,
            content TEXT NOT NULL,
            created_at INTEGER NOT NULL
        );",
    )
    .expect("schema");
    for row in &seeded {
        conn.execute(
            "INSERT INTO messages (id, session_id, content, created_at) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![row.id, "session-1", row.content, row.created_at],
        )
        .expect("harness insert");
    }
    let since = chrono::Utc::now().timestamp() - 3_600;
    let harness: Vec<String> = search_slice(&conn, since, QUERY, K)
        .expect("search_slice")
        .into_iter()
        .map(|r| r.message_id)
        .collect();

    assert_eq!(
        shipped, harness,
        "the shipped path and the eval harness disagree on the same input — \
         that is exactly the #911 defect (two rankers under one name)"
    );

    // And both must be the order the shared rule produces, so a change that
    // makes the two agree on the WRONG order still fails.
    let pairs: Vec<(String, String)> = seeded
        .iter()
        .map(|r| (r.id.clone(), r.content.clone()))
        .collect();
    let expected: Vec<String> = episodic::rank(
        pairs,
        &episodic::query_terms(QUERY),
        |pair: &(String, String)| pair.1.as_str(),
    )
    .into_iter()
    .map(|pair| pair.0)
    .collect();
    assert_eq!(
        shipped, expected,
        "the shared ranker must produce the shipped order (sum hits first)"
    );

    // The drift that #911 names would have produced a different order, so the
    // test above is not vacuous: assert the shared ranker really reorders, and
    // that it reorders on the signal that now leads (#941 — proximity).
    //
    // `deploy/flaky: rollback notes` puts both terms seven characters apart;
    // the two lead-in rows separate them by a whole clause, and the longer of
    // the two by more still. So the tight row leads, and the order is NOT the
    // recency order the input arrived in.
    assert_eq!(
        expected.first().map(String::as_str),
        Some(seeded[2].id.as_str()),
        "the row whose query terms sit closest together must lead — that is \
         the #941 primary signal, and it is what makes this test non-vacuous"
    );
    assert_eq!(
        expected,
        vec![
            seeded[2].id.clone(),
            seeded[0].id.clone(),
            seeded[1].id.clone()
        ],
        "tightest span first, then the nearer of the two scattered rows"
    );
    assert_ne!(
        expected,
        seeded.iter().map(|r| r.id.clone()).collect::<Vec<_>>(),
        "the ranker returned the input (recency) order unchanged"
    );
}

// ─── the published label ────────────────────────────────────────────────────

/// The CLI source, embedded at compile time so the guard below cannot be
/// skipped by a missing file and costs nothing at runtime.
const EVAL_CMD_SRC: &str = include_str!("../cli/eval_cmd.rs");

/// #927: the label a leg publishes must name the ranker the leg actually runs.
///
/// This is a DIFFERENT claim from "the ranker is shared" (the test above). A
/// leg can call [`episodic::rank`] and still tell every reader of its JSON
/// that no ranker exists — which is exactly what Track A did between #911 and
/// #927, because the label lived in the command and the ranker lived in the
/// module. Both halves are pinned here: the text names the shared scorer, and
/// the command QUOTES it rather than carrying a copy that can drift again.
#[test]
fn the_published_ranker_label_names_the_shared_scorer() {
    let label = episodic::RANKER_LABEL;

    assert!(
        label.contains("relevance::episodic::rank"),
        "the label must name the shared scorer the leg calls; got {label:?}"
    );
    for signal in ["proximity", "hits", "leading", "adjacency"] {
        assert!(
            label.contains(signal),
            "the label must name the `{signal}` signal it grades; got {label:?}"
        );
    }
    assert!(
        !label.contains("no message ranker exists"),
        "the label must not claim no ranker exists — #911 added one (#927); got {label:?}"
    );

    // The command must not own a second copy of the sentence. Without this,
    // re-inlining a literal in the CLI would leave the const above correct and
    // the emitted label false — the exact split-brain #927 was filed for.
    assert!(
        EVAL_CMD_SRC.contains("episodic::RANKER_LABEL"),
        "src/cli/eval_cmd.rs must quote episodic::RANKER_LABEL instead of its own copy"
    );
    assert!(
        !EVAL_CMD_SRC.contains("no message ranker exists"),
        "src/cli/eval_cmd.rs must not carry the ranker-less claim (#927)"
    );
}

/// The recency pool is what the ranker can SEE, so it is a ranking input even
/// though it is not a signal: a target outside the pool is unreachable however
/// well it scores. #941 measured it as a binding constraint (recall@10 0.075 at
/// the old `5 * k` pool, 0.150 at the cap) and it is pinned here for the same
/// reason the label is — one constant, both legs, or the harness measures a
/// ranker the binary does not ship (#911).
#[test]
fn the_tokenized_pool_is_sized_by_one_shared_rule() {
    // `k = 10` is the shipped limit. At the old `5 * k` it fetched 50 rows;
    // the sweep says the gain saturates at the cap, so the shipped request
    // must actually reach it.
    assert_eq!(
        episodic::tokenized_fetch_cap(10),
        episodic::TOKENIZED_FETCH_CAP as i64,
        "the shipped k=10 request must fetch the full cap, not 5 * k"
    );

    // Saturating, never overflowing: a caller asking for a million rows still
    // gets the documented bound, and no input returns 0 (which would make
    // every tokenized search return nothing).
    assert_eq!(
        episodic::tokenized_fetch_cap(usize::MAX),
        episodic::TOKENIZED_FETCH_CAP as i64,
        "the pool must clamp at TOKENIZED_FETCH_CAP for any limit"
    );
    assert_eq!(
        episodic::tokenized_fetch_cap(0),
        1,
        "a zero limit must not fetch zero rows"
    );

    // Neither caller may own a copy of the rule. A hardcoded multiplier in one
    // leg and not the other is precisely the split-brain that made Track A
    // measure the wrong ranker before #911, and it reads as a green gate
    // because both halves compile.
    const MESSAGE_REPO_SRC: &str = include_str!("../db/repository/message.rs");
    const TRACK_SRC: &str = include_str!("../eval/episodic_track.rs");
    for (name, src) in [
        ("src/db/repository/message.rs", MESSAGE_REPO_SRC),
        ("src/eval/episodic_track.rs", TRACK_SRC),
    ] {
        assert!(
            src.contains("episodic::tokenized_fetch_cap"),
            "{name} must size its pool with episodic::tokenized_fetch_cap"
        );
        assert!(
            !src.contains("saturating_mul(5)"),
            "{name} must not carry its own pool multiplier — that is the drift this pins"
        );
    }
}

// ─── the union of the two arms (#941) ──────────────────────────────────────

/// Seed a runtime `Database` and an independent harness `Connection` with the
/// same rows, so a test can compare the two legs on identical input.
///
/// Returns the `Database` (the caller must keep it alive for the pool), the
/// repository, the bare harness connection and the harness `since` bound.
async fn seeded_pair(seeded: &[Row]) -> (Database, MessageRepository, Connection, i64) {
    let db = Database::connect_in_memory()
        .await
        .expect("in-memory database");
    db.run_migrations().await.expect("migrations");
    let session_repo = SessionRepository::new(db.pool().clone());
    let message_repo = MessageRepository::new(db.pool().clone());

    let session = Session::new(Some("union".to_string()), None, None);
    session_repo.create(&session).await.expect("session");

    for (i, row) in seeded.iter().enumerate() {
        let mut message = Message::new(
            session.id,
            "user".to_string(),
            row.content.clone(),
            i as i32,
        );
        message.id = Uuid::parse_str(&row.id).expect("uuid");
        message.created_at =
            chrono::DateTime::from_timestamp(row.created_at, 0).expect("timestamp");
        message_repo.create(&message).await.expect("message");
    }

    let conn = Connection::open_in_memory().expect("harness store");
    conn.execute_batch(
        "CREATE TABLE messages (
            id TEXT PRIMARY KEY NOT NULL,
            session_id TEXT NOT NULL,
            content TEXT NOT NULL,
            created_at INTEGER NOT NULL
        );",
    )
    .expect("schema");
    for row in seeded {
        conn.execute(
            "INSERT INTO messages (id, session_id, content, created_at) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![row.id, "session-1", row.content, row.created_at],
        )
        .expect("harness insert");
    }
    let since = chrono::Utc::now().timestamp() - 3_600;
    (db, message_repo, conn, since)
}

/// The defect #941 names, pinned: a gold row that does NOT contain the query
/// phrase must still reach the ranker when some OTHER row does.
///
/// The phrase row is the NEWEST and the gold is older, so the old
/// short-circuit — "the phrase arm returned rows, stop" — returned the phrase
/// row and dropped the gold from the candidate set entirely. On that code
/// shape this test fails with the gold absent from both legs; under the union
/// the gold is a token-only candidate and the phrase row still leads.
#[tokio::test]
async fn the_union_reaches_gold_when_another_row_holds_the_phrase() {
    const QUERY: &str = "deploy flaky";
    const K: usize = 10;
    let now = chrono::Utc::now().timestamp();

    let gold = Row {
        id: Uuid::new_v4().to_string(),
        // Both tokens, NOT adjacent: a tokenized-only match.
        content: "deploy came back clean while a flaky retry ran".to_string(),
        created_at: now - 20,
    };
    let phrase_row = Row {
        id: Uuid::new_v4().to_string(),
        // The query verbatim, and NEWER than the gold.
        content: "notes: the deploy flaky dance happened again in staging".to_string(),
        created_at: now - 10,
    };
    let partial = Row {
        id: Uuid::new_v4().to_string(),
        // Only ONE of the two tokens: neither arm may return it.
        content: "deploy only, no second word here".to_string(),
        created_at: now - 5,
    };
    let seeded = vec![gold.clone(), phrase_row.clone(), partial.clone()];

    let (_db, message_repo, conn, since) = seeded_pair(&seeded).await;

    let shipped = message_repo
        .search_by_content_union(None, QUERY, K)
        .await
        .expect("union search");
    let shipped_ids: Vec<String> = shipped
        .iter()
        .map(|candidate| candidate.row.id.to_string())
        .collect();

    assert!(
        shipped_ids.contains(&gold.id),
        "the gold contains both query words but not the phrase and must still be a \
         candidate — dropping it is exactly the short-circuit #941 removes; got {shipped_ids:?}"
    );
    assert_eq!(
        shipped_ids.first().map(String::as_str),
        Some(phrase_row.id.as_str()),
        "the verbatim-phrase row is the tightest match and must lead; got {shipped_ids:?}"
    );
    assert!(
        !shipped_ids.contains(&partial.id),
        "a row holding only ONE query token matches neither arm and must not appear: \
         {shipped_ids:?}"
    );

    // The arm that found each row is carried through, so the caller can label a
    // token-only row honestly instead of claiming a contiguous match.
    let contiguous_of = |id: &str| {
        shipped
            .iter()
            .find(|candidate| candidate.row.id.to_string() == id)
            .map(|candidate| candidate.contiguous)
    };
    assert_eq!(
        contiguous_of(&phrase_row.id),
        Some(true),
        "the phrase row must be flagged as a contiguous hit"
    );
    assert_eq!(
        contiguous_of(&gold.id),
        Some(false),
        "the gold row must be flagged as a token-only hit"
    );

    // The harness must reach the same answer through the same functions (#911).
    let harness: Vec<String> = search_slice(&conn, since, QUERY, K)
        .expect("search_slice")
        .into_iter()
        .map(|ranked| ranked.message_id)
        .collect();
    assert_eq!(
        shipped_ids, harness,
        "the shipped union and the harness union must agree on the same input"
    );
}

/// An exact-phrase row must still LEAD when it genuinely is the tightest
/// match — even though it is the OLDEST row, so only the ranker can put it
/// there. Merging the arms must not cost the phrase arm its identity.
#[tokio::test]
async fn an_exact_phrase_row_leads_when_it_is_the_tightest_match() {
    const QUERY: &str = "deploy flaky";
    const K: usize = 10;
    let now = chrono::Utc::now().timestamp();

    let loose = Row {
        id: Uuid::new_v4().to_string(),
        content: format!("{LONG_LEAD_IN}deploy came back clean while a flaky retry ran"),
        created_at: now - 10, // newest: recency alone would put it first
    };
    let phrase = Row {
        id: Uuid::new_v4().to_string(),
        content: "deploy flaky: rollback notes".to_string(),
        created_at: now - 30, // oldest: only the ranker can promote it
    };
    let seeded = vec![loose.clone(), phrase.clone()];

    let (_db, message_repo, conn, since) = seeded_pair(&seeded).await;

    let shipped = message_repo
        .search_by_content_union(None, QUERY, K)
        .await
        .expect("union search");
    assert_eq!(
        shipped
            .first()
            .map(|candidate| candidate.row.id.to_string()),
        Some(phrase.id.clone()),
        "the tightest (verbatim-phrase) row must lead even though it is the oldest"
    );
    assert!(
        shipped
            .first()
            .is_some_and(|candidate| candidate.contiguous),
        "the row that leads must be the one the phrase arm found"
    );

    let harness: Vec<String> = search_slice(&conn, since, QUERY, K)
        .expect("search_slice")
        .into_iter()
        .map(|ranked| ranked.message_id)
        .collect();
    assert_eq!(
        harness.first(),
        Some(&phrase.id),
        "the harness must promote the phrase row too"
    );
}

/// Neither leg may short-circuit on "the phrase arm returned rows", and the
/// user-facing label must not claim what the union made false (#941).
///
/// The label used to say "~ token match — no contiguous match for '<query>'",
/// which was true only while the two arms were exclusive. Once both arms run,
/// a result set can hold BOTH kinds of row, so that sentence is a lie the
/// moment one phrase row exists — and a user reading it would be told the
/// search found no verbatim match while it was showing one.
#[test]
fn neither_leg_short_circuits_and_the_label_tells_the_truth() {
    const SESSION_SEARCH_SRC: &str = include_str!("../brain/tools/session_search.rs");
    const TRACK_SRC: &str = include_str!("../eval/episodic_track.rs");
    const MESSAGE_REPO_SRC: &str = include_str!("../db/repository/message.rs");

    // The shipped tool goes through the union, not the phrase arm alone.
    assert!(
        SESSION_SEARCH_SRC.contains("search_by_content_union"),
        "session_search must call the union path"
    );
    // The single-arm calls are what the short-circuit was built on: the tool
    // fetched the phrase arm, then conditionally fetched the tokenized one.
    // Neither call may survive in the tool.
    assert!(
        !SESSION_SEARCH_SRC.contains(".search_by_content("),
        "session_search must not call the phrase-only arm"
    );
    assert!(
        !SESSION_SEARCH_SRC.contains(".search_by_content_tokenized("),
        "session_search must not call the tokenized arm as a fallback"
    );
    assert!(
        !SESSION_SEARCH_SRC.contains("no contiguous match for"),
        "the label must not claim there is no contiguous match — the union can return one"
    );
    assert!(
        SESSION_SEARCH_SRC.contains("verbatim"),
        "the label must say which rows hold the phrase verbatim"
    );

    // Both legs join the arms in ONE place and rank the joined pool.
    for (name, src) in [
        ("src/eval/episodic_track.rs", TRACK_SRC),
        ("src/db/repository/message.rs", MESSAGE_REPO_SRC),
    ] {
        assert!(
            src.contains("merge_arms"),
            "{name} must merge the two arms through episodic::merge_arms"
        );
    }
    assert!(
        !TRACK_SRC.contains("if !contiguous.is_empty()"),
        "the harness must not short-circuit on a non-empty phrase arm"
    );
    assert!(
        MESSAGE_REPO_SRC.contains("pub async fn search_by_content_union"),
        "the repository must expose the union path the tool ships"
    );
}
