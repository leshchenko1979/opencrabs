//! Regression tests for #1626 — tokenized AND-match fallback on
//! `session_search`. Verbatim substring search stays the primary path; when
//! a multi-word query has zero contiguous hits, the tool retries with every
//! word matched independently and labels the rows `~ token match` so the
//! weaker semantics are explicit. Single-token queries never double-search.

use crate::brain::tools::session_search::SessionSearchTool;
use crate::brain::tools::{Tool, ToolExecutionContext};
use crate::db::Database;
use crate::db::models::{Message, Session};
use crate::db::repository::{MessageRepository, SessionRepository};
use serde_json::json;
use uuid::Uuid;

async fn setup() -> (Database, SessionSearchTool) {
    let db = Database::connect_in_memory().await.unwrap();
    db.run_migrations().await.unwrap();
    let tool = SessionSearchTool::new(db.pool().clone());
    (db, tool)
}

fn ctx() -> ToolExecutionContext {
    ToolExecutionContext::new(Uuid::new_v4())
}

/// Seed one session holding the given message bodies.
async fn seed(db: &Database, title: &str, bodies: &[&str]) {
    let srepo = SessionRepository::new(db.pool().clone());
    let mrepo = MessageRepository::new(db.pool().clone());
    let session = Session::new(Some(title.to_string()), Some("m".to_string()), None);
    srepo.create(&session).await.unwrap();
    for (i, body) in bodies.iter().enumerate() {
        mrepo
            .create(&Message::new(
                session.id,
                "user".into(),
                body.to_string(),
                i as i32 + 1,
            ))
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn verbatim_wins_when_present() {
    let (db, tool) = setup().await;
    seed(
        &db,
        "contiguous",
        &["the quick brown fox jumps over the lazy dog"],
    )
    .await;

    let result = tool
        .execute(
            json!({"operation": "search", "query": "quick brown fox"}),
            &ctx(),
        )
        .await
        .unwrap();
    assert!(result.success, "error: {:?}", result.error);
    assert!(
        result.output.contains("contiguous"),
        "verbatim hit expected, got: {}",
        result.output
    );
    assert!(
        !result.output.contains("~ token match"),
        "verbatim path must not carry the fallback banner: {}",
        result.output
    );
}

#[tokio::test]
async fn fallback_fires_on_zero_verbatim_multiword() {
    let (db, tool) = setup().await;
    // Same words, scattered: no contiguous "quick brown fox" anywhere.
    seed(&db, "scattered", &["the fox was quick and the sky brown"]).await;

    let result = tool
        .execute(
            json!({"operation": "search", "query": "quick brown fox"}),
            &ctx(),
        )
        .await
        .unwrap();
    assert!(result.success, "error: {:?}", result.error);
    assert!(
        result.output.contains("~ token match"),
        "tokenized fallback banner expected, got: {}",
        result.output
    );
    assert!(
        result.output.contains("scattered"),
        "tokenized row expected, got: {}",
        result.output
    );
}

#[tokio::test]
async fn single_token_miss_returns_plain_no_match() {
    let (db, tool) = setup().await;
    seed(&db, "other", &["nothing relevant here"]).await;

    let result = tool
        .execute(json!({"operation": "search", "query": "zebra"}), &ctx())
        .await
        .unwrap();
    assert!(result.success);
    assert!(
        result.output.contains("No messages found matching"),
        "plain no-match expected (single token must not double-search), got: {}",
        result.output
    );
    assert!(!result.output.contains("~ token match"));
}

#[tokio::test]
async fn zero_zero_returns_empty() {
    let (db, tool) = setup().await;
    seed(&db, "unrelated", &["some unrelated content"]).await;

    let result = tool
        .execute(
            json!({"operation": "search", "query": "totally absent words"}),
            &ctx(),
        )
        .await
        .unwrap();
    assert!(result.success);
    assert!(
        result.output.contains("No messages found matching"),
        "both passes empty -> plain no-match, got: {}",
        result.output
    );
    assert!(!result.output.contains("~ token match"));
}

#[tokio::test]
async fn and_semantics_requires_every_token() {
    let (db, tool) = setup().await;
    seed(&db, "partial", &["alpha appears, beta too, gamma nowhere"]).await;

    let result = tool
        .execute(
            json!({"operation": "search", "query": "zzz alpha beta"}),
            &ctx(),
        )
        .await
        .unwrap();
    assert!(result.success, "error: {:?}", result.error);
    // "zzz" matches nothing, so verbatim AND tokenized both miss entirely —
    // AND-semantics must not degrade to OR.
    assert!(
        result.output.contains("No messages found matching"),
        "AND-match must require every token, got: {}",
        result.output
    );
}

/// Ranking is proximity FIRST, then hit count — the #941 order. One output
/// carries BOTH halves, and neither half is vacuous:
///
/// * `sparse` and `dense` are the pair that fails without the #941 change.
///   `dense` repeats every term (7 hits against 3) but scatters them over 42
///   characters while `sparse` keeps them inside 19, so proximity puts
///   `sparse` first and a hit-count-first ranker returns the opposite.
/// * `tied` and `sparse` have the SAME proximity span (19), the same
///   opening-zone count and the same adjacency run; `tied` simply repeats
///   `beta` once more, so it must lead on hit count alone.
///
/// Recency is pinned so the SQL order is the WRONG order — `dense` is the
/// newest row — which is what makes this an assertion about ranking rather
/// than about insertion order.
#[tokio::test]
async fn tokenized_rows_rank_by_proximity_then_hit_count() {
    let (db, tool) = setup().await;
    let srepo = SessionRepository::new(db.pool().clone());
    let mrepo = MessageRepository::new(db.pool().clone());
    let now = chrono::Utc::now();

    // (title, body, created_at). `dense` is NEWEST, so recency alone would
    // lead with it — the ranker must move it last.
    let rows = [
        (
            "dense",
            "alpha alpha beta beta and more alpha beta words",
            now,
        ),
        (
            "tied",
            "words: alpha once, beta once, beta again",
            now - chrono::Duration::minutes(30),
        ),
        (
            "sparse",
            "words: alpha once, beta once",
            now - chrono::Duration::minutes(60),
        ),
    ];

    for (title, body, created_at) in rows {
        let session = Session::new(Some(title.to_string()), Some("m".to_string()), None);
        srepo.create(&session).await.unwrap();
        let mut message = Message::new(session.id, "user".into(), body.to_string(), 1);
        message.created_at = created_at;
        mrepo.create(&message).await.unwrap();
    }

    let result = tool
        .execute(
            json!({"operation": "search", "query": "words alpha beta"}),
            &ctx(),
        )
        .await
        .unwrap();
    assert!(result.success, "error: {:?}", result.error);
    assert!(
        result.output.contains("~ token match"),
        "fallback expected, got: {}",
        result.output
    );

    let pos = |name: &str| {
        result
            .output
            .find(name)
            .unwrap_or_else(|| panic!("{name} row must be present: {}", result.output))
    };
    let (dense, tied, sparse) = (pos("dense"), pos("tied"), pos("sparse"));

    // Half 1 — proximity outranks hit count: `sparse` has 3 hits to `dense`'s
    // 7, but its terms span 19 characters against `dense`'s 42.
    assert!(
        sparse < dense,
        "proximity must outrank hit count: `sparse` (span 19, 3 hits) must \
         precede `dense` (span 42, 7 hits); got:\n{}",
        result.output
    );

    // Half 2 — at EQUAL span, hit count decides. `tied` and `sparse` differ
    // only in that `tied` repeats `beta`.
    assert!(
        tied < sparse,
        "at equal proximity the stronger lexical match must lead: `tied` \
         (span 19, 4 hits) must precede `sparse` (span 19, 3 hits); got:\n{}",
        result.output
    );

    // Together those two give `tied < sparse < dense`, which is NOT the
    // recency order the store returned — `dense` was the newest row.
}
