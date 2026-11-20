//! The shared episodic relevance scorer (#911).
//!
//! Two things are being pinned here, and they are different claims:
//!
//! 1. the ranking RULE does what the design says — a row that OPENS with the
//!    query's words outranks one that scatters them at EQUAL hit count, and a
//!    stronger lexical match is never overridden by a positional one; and
//! 2. the rule is actually SHARED — the shipped repository path and the Track
//!    A harness return the same order for the same input, which is the defect
//!    #911 names. Before the shared module they did not: the repository summed
//!    every occurrence of a token while the harness counted distinct tokens,
//!    so the leg was measuring a ranker the binary does not ship.
//!
//! Test 1 is the one that fails if the tie-break term is deleted: it asserts
//! the opening-zone row comes first *and* that the input order had the
//! scattered row first, so a hits-only ranker (which keeps the stable input
//! order) returns the opposite result.

use crate::db::Database;
use crate::db::models::{Message, Session};
use crate::db::repository::SessionRepository;
use crate::db::repository::message::MessageRepository;
use crate::eval::episodic_track::search_slice;
use crate::relevance::episodic::{self, LEADING_ZONE, MAX_QUERY_TOKENS};
use rusqlite::Connection;
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

/// Adjacency is a separate signal from the opening zone: the query's words
/// appear in the SAME order in one row and reversed in the other, both inside
/// the opening zone, both at equal hit count.
#[test]
fn adjacency_breaks_a_tie_on_equal_hits_and_leading() {
    let terms = episodic::query_terms("fix flaky deploy");
    let phrase = "fix flaky deploy";
    let reversed = "deploy flaky fix";

    let phrase_score = episodic::score(phrase, &terms);
    let reversed_score = episodic::score(reversed, &terms);
    assert_eq!(phrase_score.hits, reversed_score.hits, "hits must tie");
    assert_eq!(
        phrase_score.leading, reversed_score.leading,
        "the opening-zone signal must tie too, or adjacency is not what decided it"
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

/// The new signals rank BELOW `hits`, so a row that simply mentions the query
/// more often still wins. This is the guard against the positional terms
/// turning into a heuristic that beats the lexical match.
#[test]
fn hits_stays_the_primary_signal() {
    let terms = episodic::query_terms("deploy flaky");
    // Many mentions, none of them leading.
    let many = format!("{LONG_LEAD_IN}deploy and deploy and deploy with flaky");
    // One mention of each, both leading — the positional maximum.
    let leading = "deploy flaky: notes";

    let many_score = episodic::score(&many, &terms);
    let leading_score = episodic::score(leading, &terms);
    assert!(
        many_score.hits > leading_score.hits,
        "precondition: the scattered row must have more hits ({many_score:?} vs {leading_score:?})"
    );
    assert_eq!(
        leading_score.leading, 2,
        "precondition: the leading row must max the positional signal"
    );

    let ranked = episodic::rank(
        vec![leading.to_string(), many.to_string()],
        &terms,
        String::as_str,
    );
    assert_eq!(
        ranked.first().map(String::as_str),
        Some(many.as_str()),
        "a stronger lexical match must not be outranked by a positional one"
    );
}

/// Edge cases that would otherwise panic or silently return a constant.
#[test]
fn edge_cases_are_total() {
    let empty = episodic::score("anything at all", &[]);
    assert_eq!(
        empty,
        episodic::Score {
            hits: 0,
            leading: 0,
            adjacency: 0
        }
    );

    let terms = episodic::query_terms("deploy");
    assert_eq!(terms, vec!["deploy".to_string()]);

    // A single token: adjacency can be at most 1, and only if it leads.
    let single = episodic::score("deploy now", &terms);
    assert_eq!(single.hits, 1);
    assert_eq!(single.leading, 1);
    assert_eq!(single.adjacency, 1);

    // No break character anywhere: the zone is the character budget, not the
    // end of the string. A token past the budget must NOT count as leading.
    let past_zone = format!("{LONG_LEAD_IN}deploy");
    assert!(past_zone.len() > LEADING_ZONE);
    assert_eq!(episodic::score(&past_zone, &terms).leading, 0);

    // A row shorter than the budget is entirely opening zone.
    assert_eq!(episodic::score("deploy", &terms).leading, 1);

    // Empty content and a query that matches nothing are both zero, not a panic.
    assert_eq!(
        episodic::score("", &terms),
        episodic::Score {
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
/// query never appears contiguously in any row, which keeps `search_slice` on
/// its tokenized arm (its contiguous arm is un-ranked recency).
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
        .search_by_content_tokenized(None, QUERY, K)
        .await
        .expect("tokenized search")
        .into_iter()
        .map(|m| m.id.to_string())
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
    // test above is not vacuous: assert the discriminating row really is
    // ordered by the summed count.
    assert_eq!(
        expected.first().map(String::as_str),
        Some(seeded[1].id.as_str()),
        "the row with the most token occurrences must lead — a distinct-count \
         ranker would put it last"
    );
}
