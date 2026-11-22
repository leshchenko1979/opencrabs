//! The `messages` full-text index (#855) — installation, trigger sync, and the
//! one property the whole design rests on: **the prefilter changes the query
//! plan, never the result set**.
//!
//! The index is a PREFILTER. `content LIKE '%x%' COLLATE NOCASE` stays the
//! authority on which rows match, and the `trigram` FTS arm only narrows the
//! scan. That is a claim about SQLite's semantics, not a hope, so it is checked
//! by running the SAME query with the index marked complete and marked stale and
//! asserting the two answer identically — on a fixture built to be hostile
//! (mixed case, non-ASCII, `LIKE` metacharacters, mid-word substrings, and terms
//! too short for one trigram).
//!
//! The last test pins the Track A harness to the shipped search. `search_slice`
//! carries its own copy of the two arms and the prefilter splice; if it drifted,
//! the eval leg would score a query the binary never runs, which is the #911
//! defect this file's sibling tests exist to prevent.

use rusqlite::params;
use uuid::Uuid;

use crate::db::Database;
use crate::db::messages_fts::{self, FtsPrefilter, PREFILTER_CLAUSE, TRIGRAM_MIN_CHARS};
use crate::db::models::Session;
use crate::db::repository::SessionRepository;
use crate::db::repository::message::MessageRepository;
use crate::eval::episodic_track::search_slice;

/// A migrated in-memory store: the real schema, so the `messages_fts` migration,
/// its state table and its three triggers are all present.
async fn migrated_db() -> Database {
    let db = Database::connect_in_memory()
        .await
        .expect("in-memory database");
    db.run_migrations().await.expect("migrations");
    db
}

/// Insert messages with EXPLICIT `created_at` and `sequence`, so the recency
/// ordering both search arms rely on is deterministic instead of a same-second
/// tie between rows written in the same test.
///
/// Returns the message ids in insertion order. The ids are real UUIDs because
/// `Message::from_row` parses them as such — a fixture with readable ids would
/// fail to deserialize rather than test the search.
async fn seed(db: &Database, rows: &[&str]) -> Vec<String> {
    let sessions = SessionRepository::new(db.pool().clone());
    let session = Session::new(Some("fts".to_string()), None, None);
    sessions.create(&session).await.expect("session");
    let sid = session.id.to_string();

    let base = 1_700_000_000i64;
    let owned: Vec<(String, String, i64, i64)> = rows
        .iter()
        .enumerate()
        .map(|(i, content)| {
            (
                Uuid::new_v4().to_string(),
                (*content).to_string(),
                i as i64,
                base + i as i64,
            )
        })
        .collect();
    let ids: Vec<String> = owned.iter().map(|(id, ..)| id.clone()).collect();

    {
        let conn = db.pool().get().await.expect("connection");
        conn.interact(move |c| -> rusqlite::Result<()> {
            for (id, content, sequence, created_at) in &owned {
                c.execute(
                    "INSERT INTO messages (id, session_id, role, content, sequence, created_at) \
                     VALUES (?1, ?2, 'user', ?3, ?4, ?5)",
                    params![id, sid, content, sequence, created_at],
                )?;
            }
            Ok(())
        })
        .await
        .expect("interact")
        .expect("insert messages");
    }

    ids
}

/// One scalar from the pool.
async fn scalar(db: &Database, sql: &'static str) -> i64 {
    let conn = db.pool().get().await.expect("connection");
    conn.interact(move |c| -> rusqlite::Result<i64> { c.query_row(sql, [], |r| r.get(0)) })
        .await
        .expect("interact")
        .expect("scalar")
}

/// Rows the FTS index itself answers for `match_expr`.
async fn fts_hits(db: &Database, match_expr: &str) -> i64 {
    let expr = match_expr.to_string();
    let conn = db.pool().get().await.expect("connection");
    conn.interact(move |c| -> rusqlite::Result<i64> {
        c.query_row(
            "SELECT count(*) FROM messages_fts WHERE messages_fts MATCH ?1",
            params![expr],
            |r| r.get(0),
        )
    })
    .await
    .expect("interact")
    .expect("fts count")
}

/// Flip the gate the query arms read, without touching the index itself — this
/// is how "prefiltered" and "plain LIKE" are compared on the SAME data.
async fn set_complete(db: &Database, complete: bool) {
    let flag = i64::from(complete);
    let conn = db.pool().get().await.expect("connection");
    conn.interact(move |c| -> rusqlite::Result<usize> {
        c.execute(
            "UPDATE messages_fts_state SET complete = ?1 WHERE id = 1",
            params![flag],
        )
    })
    .await
    .expect("interact")
    .expect("set complete");
}

/// `messages_fts::is_ready` through the pool.
async fn ready(db: &Database) -> bool {
    let conn = db.pool().get().await.expect("connection");
    conn.interact(|c| messages_fts::is_ready(c))
        .await
        .expect("interact")
}

/// Ids the SHIPPED search returns, in order.
async fn shipped_ids(repo: &MessageRepository, query: &str, k: usize) -> Vec<String> {
    repo.search_by_content_union(None, query, k)
        .await
        .expect("union search")
        .into_iter()
        .map(|c| c.row.id.to_string())
        .collect()
}

/// Ids the Track A harness returns, in order — the same store, the same query.
async fn harness_ids(db: &Database, query: &str, k: usize) -> Vec<String> {
    let q = query.to_string();
    let conn = db.pool().get().await.expect("connection");
    conn.interact(move |c| search_slice(c, 0, &q, k))
        .await
        .expect("interact")
        .expect("search_slice")
        .into_iter()
        .map(|r| r.message_id)
        .collect()
}

// ─── 1. Installation ────────────────────────────────────────────────────────

/// The migration creates the index, its state row and the three triggers, and
/// running it again must not reset a completed index.
#[tokio::test]
async fn migration_installs_the_index_state_and_triggers() {
    let db = migrated_db().await;

    let names: Vec<String> = {
        let conn = db.pool().get().await.expect("connection");
        conn.interact(|c| -> rusqlite::Result<Vec<String>> {
            let mut stmt = c.prepare(
                "SELECT name FROM sqlite_master \
                 WHERE name LIKE 'messages_fts%' OR name IN \
                 ('messages_ai', 'messages_ad', 'messages_au') \
                 ORDER BY name",
            )?;
            let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
            rows.collect()
        })
        .await
        .expect("interact")
        .expect("schema names")
    };

    for expected in [
        "messages_fts",
        "messages_fts_state",
        "messages_ai",
        "messages_ad",
        "messages_au",
    ] {
        assert!(
            names.iter().any(|n| n == expected),
            "migration did not create `{expected}`; found {names:?}"
        );
    }

    let installed = {
        let conn = db.pool().get().await.expect("connection");
        conn.interact(|c| messages_fts::is_installed(c))
            .await
            .expect("interact")
    };
    assert!(installed, "index should be installed after the migration");

    // The migration creates the STRUCTURE only — the 607 MB copy is the
    // background backfill's job, so the gate starts CLOSED and every search runs
    // the plain-LIKE scan until it opens.
    assert!(
        !ready(&db).await,
        "the index must not claim to be ready before a backfill"
    );

    // Idempotence: replaying the migration SQL must not reset `complete` back
    // to 0, which would silently degrade a live store to the slow scan on every
    // boot. Replaying it directly (rather than only re-running
    // `run_migrations`, which skips an applied migration) is what actually
    // exercises the `IF NOT EXISTS` / `INSERT OR IGNORE` guards in the file.
    set_complete(&db, true).await;
    {
        let conn = db.pool().get().await.expect("connection");
        conn.interact(|c| -> rusqlite::Result<()> {
            c.execute_batch(include_str!(
                "../migrations/20261122000001_add_messages_fts.sql"
            ))
        })
        .await
        .expect("interact")
        .expect("replay migration");
    }
    db.run_migrations().await.expect("second migration run");
    assert_eq!(
        scalar(&db, "SELECT complete FROM messages_fts_state WHERE id = 1").await,
        1,
        "replaying the migration must not clear a completed index"
    );
    assert_eq!(
        scalar(&db, "SELECT count(*) FROM messages_fts_state").await,
        1,
        "the state table holds exactly one row"
    );
}

// ─── 2. Trigger sync ────────────────────────────────────────────────────────

/// The three triggers keep the index correct for rows written after the
/// migration — which is what lets the backfill be a one-off and lets it run
/// concurrently with user traffic.
#[tokio::test]
async fn triggers_keep_the_index_in_sync_on_insert_update_and_delete() {
    let db = migrated_db().await;
    let ids = seed(&db, &["the quick brown fox"]).await;
    let first = ids[0].clone();

    // INSERT
    assert_eq!(scalar(&db, "SELECT count(*) FROM messages_fts").await, 1);
    assert_eq!(
        fts_hits(&db, "\"brown\"").await,
        1,
        "insert must be indexed"
    );

    // UPDATE OF content — the trigger is column-scoped so a cost/token update
    // does not re-tokenize a 14 KB body; the content update must still land.
    {
        let id = first.clone();
        let conn = db.pool().get().await.expect("connection");
        conn.interact(move |c| -> rusqlite::Result<usize> {
            c.execute(
                "UPDATE messages SET content = 'a lazy dog sleeps' WHERE id = ?1",
                params![id],
            )
        })
        .await
        .expect("interact")
        .expect("update content");
    }

    assert_eq!(
        fts_hits(&db, "\"brown\"").await,
        0,
        "the old text must leave the index"
    );
    assert_eq!(
        fts_hits(&db, "\"lazy\"").await,
        1,
        "the new text must enter the index"
    );
    assert_eq!(
        scalar(&db, "SELECT count(*) FROM messages_fts").await,
        1,
        "an update must replace the entry, not duplicate it"
    );

    // DELETE — on a contentless-delete table this is a no-op for a rowid that
    // was never indexed, which is why the index can be live while the backfill
    // is still running.
    {
        let id = first.clone();
        let conn = db.pool().get().await.expect("connection");
        conn.interact(move |c| -> rusqlite::Result<usize> {
            c.execute("DELETE FROM messages WHERE id = ?1", params![id])
        })
        .await
        .expect("interact")
        .expect("delete");
    }

    assert_eq!(
        scalar(&db, "SELECT count(*) FROM messages_fts").await,
        0,
        "a delete must remove the entry"
    );
    assert_eq!(fts_hits(&db, "\"lazy\"").await, 0);
}

// ─── 3. Mid-word substring parity ───────────────────────────────────────────

/// `brown` matches `rown` — mid-word, exactly as `LIKE '%rown%'` does. This is
/// why the tokenizer is `trigram` and not `unicode61`, which would match whole
/// tokens only and silently change the result set the #941 eval measures.
#[tokio::test]
async fn mid_word_substring_parity_between_like_and_the_prefilter() {
    let db = migrated_db().await;
    seed(
        &db,
        &["the quick brown fox", "a browned pan", "nothing here"],
    )
    .await;
    let repo = MessageRepository::new(db.pool().clone());

    let report = messages_fts::backfill(db.pool(), 100)
        .await
        .expect("backfill");
    assert!(report.is_complete(), "backfill: {report:?}");
    assert_eq!(report.indexed_rows, report.message_rows);

    // With the gate OPEN the prefilter is live.
    let prefiltered = shipped_ids(&repo, "rown", 10).await;
    assert_eq!(prefiltered.len(), 2, "`rown` must find both brown rows");

    // With the gate CLOSED the same query runs the pre-#855 scan.
    set_complete(&db, false).await;
    let plain = shipped_ids(&repo, "rown", 10).await;

    assert_eq!(
        prefiltered, plain,
        "the prefilter must not change which rows a mid-word substring finds"
    );
    assert!(
        !plain.is_empty(),
        "the LIKE-only leg must still find the rows — otherwise this test compares two empty sets"
    );
}

// ─── 4. Terms shorter than one trigram ──────────────────────────────────────

/// A 1- or 2-character term cannot be a trigram, and `MATCH` answers ZERO rows
/// for one instead of erroring — the dangerous, silent shape. The whole query
/// must therefore fall back to the LIKE scan rather than narrow to nothing.
#[tokio::test]
async fn a_term_shorter_than_one_trigram_falls_back_to_like() {
    let db = migrated_db().await;
    let ids = seed(&db, &["ab is here", "cd is not"]).await;
    let repo = MessageRepository::new(db.pool().clone());
    messages_fts::backfill(db.pool(), 100)
        .await
        .expect("backfill");

    let short = "ab".to_string();
    assert!(
        short.chars().count() < TRIGRAM_MIN_CHARS,
        "this test only means something for a sub-trigram term"
    );

    // The gate is OPEN, so this is a genuine "the index cannot answer" case and
    // not a "the index is absent" one.
    assert!(
        ready(&db).await,
        "the index must be ready for this test to be meaningful"
    );

    let (phrase, tokens) = {
        let conn = db.pool().get().await.expect("connection");
        let q = short.clone();
        conn.interact(move |c| {
            (
                FtsPrefilter::for_phrase(c, &q),
                FtsPrefilter::for_tokens(c, std::slice::from_ref(&q)),
            )
        })
        .await
        .expect("interact")
    };
    assert!(
        phrase.is_none(),
        "a sub-trigram term must not produce a MATCH expression"
    );
    assert!(
        tokens.is_none(),
        "a sub-trigram token must not produce a MATCH expression"
    );

    let found = shipped_ids(&repo, "ab", 10).await;
    assert_eq!(
        found,
        vec![ids[0].clone()],
        "the LIKE fallback must still find the 2-character substring"
    );
}

// ─── 5. Plan, not result ────────────────────────────────────────────────────

/// The load-bearing property: on a hostile fixture the prefiltered query and the
/// plain-LIKE query return the SAME rows, in the same order. Mixed case,
/// non-ASCII, `LIKE` metacharacters and a sub-trigram term all included.
#[tokio::test]
async fn the_prefilter_changes_the_plan_not_the_result_set() {
    let db = migrated_db().await;
    seed(
        &db,
        &[
            "The Quick BROWN fox",
            "the quick brown fox, again",
            "CAFÉ crème brûlée",
            "café noir",
            "100% sure about the 50% guess",
            "under_score and more_underscores",
            "a-b hyphenated thing",
            "ab",
            "quote \" inside the text",
            "brown",
        ],
    )
    .await;
    let repo = MessageRepository::new(db.pool().clone());
    messages_fts::backfill(db.pool(), 100)
        .await
        .expect("backfill");

    // Every query the fixture was built to attack: substring, mid-word, case
    // (ASCII and not), LIKE metacharacters, a doubled quote, and one token that
    // is too short for the index.
    let queries = [
        "brown",
        "rown",
        "BROWN",
        "quick brown",
        "café",
        "CAFÉ",
        "100%",
        "50%",
        "under_score",
        "more_underscores",
        "a-b",
        "ab",
        "quote \"",
    ];

    for query in queries {
        let prefiltered = shipped_ids(&repo, query, 10).await;
        set_complete(&db, false).await;
        let plain = shipped_ids(&repo, query, 10).await;
        set_complete(&db, true).await;

        assert_eq!(
            prefiltered, plain,
            "the FTS prefilter changed the result set for query {query:?} — \
             the index must never be the authority on membership"
        );
    }

    // A fixture where every query returns nothing would pass the loop above
    // while testing nothing, so pin at least one non-empty answer.
    let brown = shipped_ids(&repo, "rown", 10).await;
    assert!(
        brown.len() >= 3,
        "the hostile fixture must actually produce matches, got {brown:?}"
    );
}

// ─── 6. Harness parity ──────────────────────────────────────────────────────

/// The Track A harness must return what the shipped search returns — same rows,
/// same order — on a store whose index is complete. `search_slice` duplicates
/// the two arms and the prefilter splice, so this is the pin that stops the eval
/// leg from measuring a query the binary does not run (#911).
#[tokio::test]
async fn the_track_a_harness_and_the_shipped_search_agree() {
    let db = migrated_db().await;
    seed(
        &db,
        &[
            "the quick brown fox jumps",
            "brown fox in the yard",
            "a quick note about foxes",
            "the fox and the hound",
            "quick brown",
            "brown",
            "nothing to see",
            "the yard is brown",
            "quick thinking",
            "foxes are quick",
        ],
    )
    .await;
    let repo = MessageRepository::new(db.pool().clone());
    messages_fts::backfill(db.pool(), 100)
        .await
        .expect("backfill");

    // Multi-token (both arms run), single-token (the tokenized arm is skipped in
    // both legs), mid-word, and a term the index cannot answer.
    for query in ["quick brown", "brown", "rown", "fox", "ab"] {
        let shipped = shipped_ids(&repo, query, 10).await;
        let harness = harness_ids(&db, query, 10).await;
        assert_eq!(
            shipped, harness,
            "harness and shipped search disagree for {query:?} — the eval leg is \
             measuring a different query (#911)"
        );
    }

    // The comparison is only worth something if at least one query returns a
    // non-trivial set from BOTH legs.
    let shipped = shipped_ids(&repo, "quick brown", 10).await;
    assert!(
        shipped.len() >= 2,
        "fixture must produce a real candidate set, got {shipped:?}"
    );
}

// ─── The splice itself ──────────────────────────────────────────────────────

/// The clause is spliced into four SQL strings (two repository arms, two harness
/// arms). It binds exactly one parameter, and it must stay a `rowid IN`
/// sub-select over the index — a bare `MATCH` in the `WHERE` of a query that also
/// reads `messages` is not valid FTS5.
#[test]
fn the_prefilter_clause_binds_one_parameter_and_scopes_by_rowid() {
    assert_eq!(
        PREFILTER_CLAUSE.matches('?').count(),
        1,
        "the clause must bind exactly one parameter: {PREFILTER_CLAUSE}"
    );
    assert!(
        PREFILTER_CLAUSE.contains("rowid IN (SELECT rowid FROM messages_fts"),
        "the clause must narrow by rowid: {PREFILTER_CLAUSE}"
    );
    assert!(
        PREFILTER_CLAUSE.contains("messages_fts MATCH"),
        "the clause must drive the index with MATCH: {PREFILTER_CLAUSE}"
    );
    // The callers splice it between existing terms with their own spacing, so it
    // must not carry a trailing `AND` or a leading `WHERE`.
    let trimmed = PREFILTER_CLAUSE.trim();
    assert!(trimmed.starts_with("AND "), "clause: {PREFILTER_CLAUSE}");
    assert!(!trimmed.ends_with("AND"), "clause: {PREFILTER_CLAUSE}");
}
