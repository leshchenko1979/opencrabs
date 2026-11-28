//! Tests for the `messages.content` FTS5 index (#855).
//!
//! Covers the four things that break silently if they are wrong: the triggers
//! sync on insert/update/delete, the unbuilt state cannot corrupt the index,
//! the rebuild is idempotent, and the `MATCH` expression builder cannot emit
//! FTS5 syntax errors from a user's query.
//!
//! House rule: no inline test modules in source files — everything lives here
//! under `src/tests/`.

use crate::db::Database;
use crate::db::message_fts::{
    FTS_STATE_TABLE, FTS_TABLE, FTS_TRIGGERS, INLINE_BUILD_MAX_ROWS,
    ensure_and_verify_messages_fts, ensure_messages_fts, fts_match_expression, may_build_inline,
    messages_fts_built, rebuild_messages_fts, verify_messages_fts,
};
use rusqlite::Connection;

/// A fresh in-memory database with the schema and the index objects, UNBUILT.
async fn setup() -> Database {
    let db = Database::connect_in_memory().await.unwrap();
    db.run_migrations().await.unwrap();
    db
}

/// Seed one session with the given bodies through the repository (so the
/// `messages` triggers fire exactly as they do in production).
async fn seed(db: &Database, bodies: &[&str]) -> uuid::Uuid {
    use crate::db::models::{Message, Session};
    use crate::db::repository::{MessageRepository, SessionRepository};

    let srepo = SessionRepository::new(db.pool().clone());
    let mrepo = MessageRepository::new(db.pool().clone());
    let session = Session::new(Some("fts".into()), Some("m".into()), None);
    srepo.create(&session).await.unwrap();
    for (i, body) in bodies.iter().enumerate() {
        mrepo
            .create(&Message::new(
                session.id,
                "user".into(),
                (*body).to_string(),
                i as i32 + 1,
            ))
            .await
            .unwrap();
    }
    session.id
}

/// Run `f` against the pool's single connection.
async fn on_conn<T, F>(db: &Database, f: F) -> T
where
    F: FnOnce(&Connection) -> T + Send + 'static,
    T: Send + 'static,
{
    let conn = db.pool().get().await.unwrap();
    // One `unwrap` only. `interact` returns `Result<T, InteractError>`, so the
    // outer `unwrap` covers the hop to the pool's blocking thread; the closure
    // already unwrapped its own `rusqlite::Result` at the call site. A second
    // `unwrap` here does not compile: `T` is generic and has no `unwrap`.
    conn.interact(move |c| f(c)).await.unwrap()
}

/// How many rows in the index match `expression`.
async fn match_count(db: &Database, expression: &str) -> i64 {
    let expression = expression.to_string();
    on_conn(db, move |c| {
        c.query_row(
            &format!("SELECT COUNT(*) FROM {FTS_TABLE} WHERE {FTS_TABLE} MATCH ?1"),
            [&expression],
            |r| r.get(0),
        )
        .unwrap()
    })
    .await
}

// ---------------------------------------------------------------- objects

#[tokio::test]
async fn heal_creates_the_index_state_and_all_three_triggers() {
    let db = setup().await;

    // `run_migrations` already ran the heal, so everything must be present.
    let names: Vec<String> = on_conn(&db, |c| {
        let mut stmt = c
            .prepare(
                "SELECT name FROM sqlite_master WHERE name IN \
                 ('messages_fts', 'messages_fts_state', 'messages_fts_ai', \
                  'messages_fts_ad', 'messages_fts_au') ORDER BY name",
            )
            .unwrap();
        let rows: Vec<String> = stmt
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        rows
    })
    .await;

    assert!(names.contains(&FTS_TABLE.to_string()), "got {names:?}");
    assert!(
        names.contains(&FTS_STATE_TABLE.to_string()),
        "got {names:?}"
    );
    for trigger in FTS_TRIGGERS {
        assert!(names.contains(&trigger.to_string()), "got {names:?}");
    }
}

/// The heal is idempotent: a second run creates nothing and reports `false`.
#[tokio::test]
async fn heal_is_idempotent() {
    let db = setup().await;
    let created_again = on_conn(&db, |c| ensure_messages_fts(c).unwrap()).await;
    assert!(
        !created_again,
        "a healthy database must not report a fresh create"
    );
}

/// An interrupted heal (two of three triggers) gets only the missing one, and
/// the pre-existing ones are not disturbed.
#[tokio::test]
async fn heal_restores_a_single_missing_trigger() {
    let db = setup().await;
    on_conn(&db, |c| {
        c.execute_batch("DROP TRIGGER messages_fts_au").unwrap();
    })
    .await;

    let created = on_conn(&db, |c| ensure_messages_fts(c).unwrap()).await;
    assert!(created, "the missing trigger must be recreated");

    let n: i64 = on_conn(&db, |c| {
        c.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='trigger' AND name='messages_fts_au'",
            [],
            |r| r.get(0),
        )
        .unwrap()
    })
    .await;
    assert_eq!(n, 1);
}

/// A database migrated by this code starts with the index UNBUILT, because the
/// rebuild is the expensive part and belongs to the sweep.
#[tokio::test]
async fn index_starts_unbuilt() {
    let db = setup().await;
    let built = on_conn(&db, |c| messages_fts_built(c).unwrap()).await;
    assert!(
        !built,
        "boot must not build the index: the rebuild is O(all message text)"
    );
}

// ---------------------------------------------------------------- triggers

/// Insert, update and delete all keep the index in step with `messages`.
///
/// This is the whole contract of the three triggers, and each arm is asserted
/// against the word that must appear and the word that must be gone — a count
/// alone would pass if the update arm indexed the row twice.
#[tokio::test]
async fn triggers_sync_insert_update_and_delete() {
    let db = setup().await;
    // Build first: the triggers are gated on `built`, so an unbuilt index would
    // make this test assert the gate rather than the sync.
    on_conn(&db, |c| {
        ensure_messages_fts(c).unwrap();
        rebuild_messages_fts(c).unwrap();
    })
    .await;
    seed(&db, &["the quick brown fox"]).await;

    assert_eq!(
        match_count(&db, "\"quick\"").await,
        1,
        "INSERT trigger must index the new row"
    );

    // UPDATE: rewrite the content in place (this is `set_content`, the #894
    // leak-repair path).
    let id: String = on_conn(&db, |c| {
        c.query_row("SELECT id FROM messages LIMIT 1", [], |r| r.get(0))
            .unwrap()
    })
    .await;
    let id = uuid::Uuid::parse_str(&id).unwrap();
    crate::db::repository::MessageRepository::new(db.pool().clone())
        .set_content(id, "rewritten zebra text")
        .await
        .unwrap();

    assert_eq!(
        match_count(&db, "\"quick\"").await,
        0,
        "UPDATE trigger must drop the OLD document, not just add the new one"
    );
    assert_eq!(
        match_count(&db, "\"zebra\"").await,
        1,
        "UPDATE trigger must index the new content"
    );

    // DELETE.
    on_conn(&db, move |c| {
        c.execute("DELETE FROM messages WHERE id = ?1", [id.to_string()])
            .unwrap();
    })
    .await;
    assert_eq!(
        match_count(&db, "\"zebra\"").await,
        0,
        "DELETE trigger must un-index the row"
    );

    // And the index is still structurally sound after all three arms.
    let intact = on_conn(&db, |c| verify_messages_fts(c).unwrap()).await;
    assert!(
        intact,
        "index must survive a full insert/update/delete cycle"
    );
}

/// An `UPDATE` that does not touch `content` must not re-index the row.
///
/// The trigger is `AFTER UPDATE OF content` for this reason: the write path
/// updates cost and token columns constantly, and a table-wide `AFTER UPDATE`
/// would re-index the whole row (reading the big column) on each one.
#[tokio::test]
async fn update_of_other_columns_does_not_touch_the_index() {
    let db = setup().await;
    on_conn(&db, |c| {
        ensure_messages_fts(c).unwrap();
        rebuild_messages_fts(c).unwrap();
    })
    .await;
    seed(&db, &["cost update probe"]).await;

    let docsize_before: i64 = on_conn(&db, |c| {
        c.query_row(
            &format!("SELECT COALESCE(SUM(sz), 0) FROM {FTS_TABLE}_docsize"),
            [],
            |r| r.get(0),
        )
        .unwrap()
    })
    .await;

    on_conn(&db, |c| {
        c.execute("UPDATE messages SET cost = 1.25, token_count = 42", [])
            .unwrap();
    })
    .await;

    let docsize_after: i64 = on_conn(&db, |c| {
        c.query_row(
            &format!("SELECT COALESCE(SUM(sz), 0) FROM {FTS_TABLE}_docsize"),
            [],
            |r| r.get(0),
        )
        .unwrap()
    })
    .await;

    assert_eq!(
        docsize_before, docsize_after,
        "a non-content UPDATE must not re-index the row"
    );
}

/// The `built` gate is what stops an unbuilt index from being corrupted.
///
/// Measured, not assumed: with an external-content FTS5 table, a `'delete'`
/// command against an index that was never built makes every later read fail
/// with `database disk image is malformed`, and the damage survives a rebuild.
/// This test fires a delete, an update and an insert while the index is
/// unbuilt, then builds it, and requires both a clean integrity check and the
/// correct row set.
#[tokio::test]
async fn unbuilt_index_is_not_corrupted_by_writes() {
    let db = setup().await;
    seed(&db, &["alpha beta", "gamma delta", "epsilon zeta"]).await;

    // Unbuilt: the triggers must be no-ops.
    assert!(
        !on_conn(&db, |c| messages_fts_built(c).unwrap()).await,
        "precondition: the index is unbuilt"
    );

    on_conn(&db, |c| {
        c.execute_batch(
            "DELETE FROM messages WHERE content = 'gamma delta';
             UPDATE messages SET content = 'omega zeta' WHERE content = 'alpha beta';
             INSERT INTO messages (id, session_id, role, content, sequence, created_at)
               SELECT lower(hex(randomblob(16))), session_id, 'user', 'brand new subagent', 9, 1
               FROM messages LIMIT 1;",
        )
        .unwrap();
    })
    .await;

    // Build over the post-write table.
    on_conn(&db, |c| rebuild_messages_fts(c).unwrap()).await;

    let intact = on_conn(&db, |c| verify_messages_fts(c).unwrap()).await;
    assert!(
        intact,
        "the index must be intact after writes made it unbuilt"
    );

    assert_eq!(
        match_count(&db, "\"gamma\"").await,
        0,
        "a row deleted before the build must not be in the index"
    );
    assert_eq!(
        match_count(&db, "\"omega\"").await,
        1,
        "a row rewritten before the build must carry its NEW content"
    );
    assert_eq!(
        match_count(&db, "\"beta\"").await,
        0,
        "the pre-update content must not survive"
    );
    assert_eq!(
        match_count(&db, "\"subagent\"").await,
        1,
        "a row inserted before the build must be indexed by the rebuild"
    );
}

// ---------------------------------------------------------------- rebuild

/// Rebuilding twice is safe and idempotent.
#[tokio::test]
async fn rebuild_is_idempotent() {
    let db = setup().await;
    seed(&db, &["one two", "three four"]).await;
    on_conn(&db, |c| {
        ensure_messages_fts(c).unwrap();
        rebuild_messages_fts(c).unwrap();
        rebuild_messages_fts(c).unwrap();
    })
    .await;

    let n: i64 = on_conn(&db, |c| {
        c.query_row(&format!("SELECT COUNT(*) FROM {FTS_TABLE}"), [], |r| {
            r.get(0)
        })
        .unwrap()
    })
    .await;
    assert_eq!(n, 2, "a second rebuild must not duplicate documents");
    assert!(on_conn(&db, |c| verify_messages_fts(c).unwrap()).await);
}

/// The rebuild marks the index built in the SAME transaction, so the flag can
/// never claim a build that did not happen.
#[tokio::test]
async fn rebuild_flips_the_built_flag() {
    let db = setup().await;
    seed(&db, &["flag probe"]).await;
    assert!(!on_conn(&db, |c| messages_fts_built(c).unwrap()).await);
    on_conn(&db, |c| {
        ensure_messages_fts(c).unwrap();
        rebuild_messages_fts(c).unwrap();
    })
    .await;
    assert!(on_conn(&db, |c| messages_fts_built(c).unwrap()).await);
}

/// The sweep leg builds when unbuilt and reports that it did real work; a
/// second call finds a healthy index and reports none.
#[tokio::test]
async fn sweep_leg_builds_once_then_reports_no_work() {
    let db = setup().await;
    seed(&db, &["sweep probe one", "sweep probe two"]).await;

    let first = on_conn(&db, |c| ensure_and_verify_messages_fts(c).unwrap()).await;
    assert!(first, "an unbuilt index must be built by the sweep leg");

    let second = on_conn(&db, |c| ensure_and_verify_messages_fts(c).unwrap()).await;
    assert!(
        !second,
        "a healthy index must cost the sweep a verify, not a rebuild"
    );
}

/// A disagreement between the index and the content table is repaired, not
/// ignored.
///
/// The realistic cause is a `VACUUM` that renumbered rowids. This test produces
/// the same end state directly — the index points at the wrong row — and
/// requires the sweep leg to notice through FTS5's own `integrity-check` and
/// rebuild.
#[tokio::test]
async fn sweep_leg_repairs_a_disagreeing_index() {
    let db = setup().await;
    seed(&db, &["genuine content here"]).await;
    on_conn(&db, |c| {
        ensure_messages_fts(c).unwrap();
        rebuild_messages_fts(c).unwrap();
    })
    .await;

    // Produce exactly the state a rowid-renumbering VACUUM would: the index
    // holds a document for a row that no longer exists. The shadow tables are
    // read-only to SQL ("table f_docsize may not be modified"), so the row goes
    // from `messages` with its delete trigger dropped — which is what a VACUUM
    // does to the index's view without telling the index.
    on_conn(&db, |c| {
        c.execute_batch(
            "DROP TRIGGER messages_fts_ad;
             DELETE FROM messages;",
        )
        .unwrap();
    })
    .await;

    assert!(
        !on_conn(&db, |c| verify_messages_fts(c).unwrap()).await,
        "precondition: the index must now disagree with the content table"
    );

    let repaired = on_conn(&db, |c| ensure_and_verify_messages_fts(c).unwrap()).await;
    assert!(repaired, "the sweep leg must rebuild a disagreeing index");
    assert!(
        on_conn(&db, |c| verify_messages_fts(c).unwrap()).await,
        "the index must agree with the content table after the repair"
    );
}

// ---------------------------------------------------------------- MATCH expr

/// A multi-word query becomes an AND of quoted tokens.
#[test]
fn match_expression_is_an_and_of_quoted_tokens() {
    assert_eq!(
        fts_match_expression("quick brown fox").as_deref(),
        Some("\"brown\" AND \"fox\" AND \"quick\""),
        "tokens are sorted (query_tokens does that) and ANDed"
    );
}

/// Duplicate words collapse, so a repeated word cannot make the expression
/// require the same token twice.
#[test]
fn match_expression_dedupes_tokens() {
    assert_eq!(
        fts_match_expression("deploy deploy deploy").as_deref(),
        Some("\"deploy\"")
    );
}

/// An empty or whitespace-only query has no expression, so the caller returns
/// no rows instead of emitting `MATCH ''` — which is a hard syntax error.
#[test]
fn match_expression_rejects_empty_queries() {
    assert_eq!(fts_match_expression(""), None);
    assert_eq!(fts_match_expression("   \t\n "), None);
}

/// Punctuation is neutralised by quoting.
///
/// Unquoted, `foo-bar` is `foo NOT bar`, `*` is a syntax error and `NEAR(` is an
/// unterminated group. Every one of these is reachable from a user's query.
#[test]
fn match_expression_quotes_punctuation_into_safety() {
    for query in [
        "foo-bar",
        "a*b",
        "NEAR(",
        "\"unterminated",
        "col:val",
        "x OR y",
    ] {
        let expr = fts_match_expression(query)
            .unwrap_or_else(|| panic!("{query} must produce an expression"));
        // Every token is quoted, so no bare operator can survive.
        assert!(
            expr.split(" AND ")
                .all(|arm| arm.starts_with('"') && arm.ends_with('"')),
            "{query} -> {expr} must quote every arm"
        );
    }
}

/// A quote inside a token is doubled — FTS5's own escape inside a quoted string.
#[test]
fn match_expression_doubles_embedded_quotes() {
    let expr = fts_match_expression("say\"hi").expect("expression");
    assert_eq!(
        expr, "\"say\"\"hi\"",
        "an embedded quote must be doubled, not left to close the string"
    );
}

/// The expression the builder produces actually runs, for every shape that used
/// to be a syntax error. This is the assertion the unit tests above cannot
/// make: quoting is only correct if FTS5 accepts the result.
#[tokio::test]
async fn built_expressions_are_accepted_by_fts5() {
    let db = setup().await;
    seed(&db, &["the quick brown fox", "a b c"]).await;
    on_conn(&db, |c| {
        ensure_messages_fts(c).unwrap();
        rebuild_messages_fts(c).unwrap();
    })
    .await;

    for query in [
        "quick",
        "quick brown",
        "foo-bar",
        "a*b",
        "NEAR(",
        "col:val",
        "x OR y",
        "!!!",
        "say\"hi",
    ] {
        let expr = fts_match_expression(query)
            .unwrap_or_else(|| panic!("{query} must produce an expression"));
        let n: i64 = on_conn(&db, {
            let expr = expr.clone();
            move |c| {
                c.query_row(
                    &format!("SELECT COUNT(*) FROM {FTS_TABLE} WHERE {FTS_TABLE} MATCH ?1"),
                    [&expr],
                    |r| r.get(0),
                )
                .unwrap_or_else(|e| panic!("query {query:?} -> {expr} must not error: {e}"))
            }
        })
        .await;
        assert!(n >= 0);
    }
}

// ---------------------------------------------------------------- inline bound

/// The inline-build bound is inclusive at the constant and excludes one row
/// past it — the `CONST - 1` / `CONST` / `CONST + 1` boundary the testing guide
/// asks for.
#[test]
fn inline_build_bound_is_inclusive_at_the_constant() {
    assert!(may_build_inline(INLINE_BUILD_MAX_ROWS - 1));
    assert!(may_build_inline(INLINE_BUILD_MAX_ROWS));
    assert!(
        !may_build_inline(INLINE_BUILD_MAX_ROWS + 1),
        "one row past the bound must defer to the sweep"
    );
    assert!(may_build_inline(0), "an empty table is trivially cheap");
}
