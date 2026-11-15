//! #854 — the SQL compaction-boundary tail must equal the in-memory trim.
//!
//! Pushing the boundary into SQL means the predicate now exists twice:
//! `messages_from_last_compaction` (over an already-loaded `Vec`) and
//! `load_messages_since_last_compaction` (as a query). Two implementations of
//! one rule drift, so these tests pin them to the SAME answer over the same
//! fixture, and cover the two ways the obvious SQL is wrong.
//!
//! The fixture deliberately includes the three rows that the predicate's own
//! history turned on: a delta-segment banner (extends the window, not a
//! boundary), a summary whose BODY quotes the sentinel (still a boundary,
//! #1928), and an assistant row echoing the prefix (not a boundary, #175).

use crate::brain::agent::AgentService;
use crate::brain::agent::context::{COMPACTION_MARKER_PREFIX, SEGMENT_SENTINEL};
use crate::db::Database;
use crate::db::MessageRepository;
use crate::db::models::Message;
use crate::services::{ServiceContext, SessionService};
use chrono::Utc;
use uuid::Uuid;

/// Seed a session with rows whose sequence numbers the caller sets explicitly,
/// so tie behaviour is under the test's control rather than the writer's.
async fn seed(rows: &[(&str, &str, i32)]) -> (crate::db::Pool, Uuid) {
    let db = Database::connect_in_memory().await.unwrap();
    db.run_migrations().await.unwrap();
    let pool = db.pool().clone();

    let session = SessionService::new(ServiceContext::new(pool.clone()))
        .create_session(Some("boundary".to_string()))
        .await
        .unwrap();

    let repo = MessageRepository::new(pool.clone());
    for (role, content, sequence) in rows {
        repo.create(&Message {
            id: Uuid::new_v4(),
            session_id: session.id,
            role: (*role).to_string(),
            content: (*content).to_string(),
            sequence: *sequence,
            created_at: Utc::now(),
            token_count: None,
            cost: None,
            input_tokens: None,
            cache_creation_tokens: None,
            cache_read_tokens: None,
            thinking: None,
            duration_secs: None,
        })
        .await
        .unwrap();
    }

    (pool, session.id)
}

/// The whole point of the fix: the two forms of the boundary must agree.
async fn assert_forms_agree(pool: &crate::db::Pool, session_id: Uuid) {
    let repo = MessageRepository::new(pool.clone());
    let full = repo.find_by_session(session_id).await.unwrap();
    let in_memory = AgentService::messages_from_last_compaction(full);
    let from_sql = AgentService::load_messages_since_last_compaction(pool.clone(), session_id)
        .await
        .unwrap();

    let in_memory_ids: Vec<Uuid> = in_memory.iter().map(|m| m.id).collect();
    let from_sql_ids: Vec<Uuid> = from_sql.iter().map(|m| m.id).collect();
    assert_eq!(
        from_sql_ids, in_memory_ids,
        "SQL tail and in-memory trim disagree — the compaction predicate has drifted \
         between its two implementations"
    );
}

#[tokio::test]
async fn the_sql_tail_matches_the_in_memory_trim() {
    let marker = format!("{COMPACTION_MARKER_PREFIX}] summary of turns 1-2");
    let delta = format!("{COMPACTION_MARKER_PREFIX} — {SEGMENT_SENTINEL} continuation");
    // Body quotes the sentinel but does not START with the delta banner: still
    // a boundary (#1928).
    let quoting =
        format!("{COMPACTION_MARKER_PREFIX}] recap: the banner reads \"{SEGMENT_SENTINEL}\"");
    // An assistant row echoing the prefix is not a marker (#175).
    let echo = format!("{COMPACTION_MARKER_PREFIX}] appears in this transcript");

    let (pool, session_id) = seed(&[
        ("user", "hello", 1),
        ("assistant", "hi", 2),
        ("user", &marker, 3),
        ("user", &delta, 4),
        ("assistant", &echo, 5),
        ("user", &quoting, 6),
        ("assistant", "after the quoting boundary", 7),
    ])
    .await;

    assert_forms_agree(&pool, session_id).await;

    // The LAST boundary is the quoting summary at sequence 6, so the window is
    // rows 6-7 only: the earlier marker, the delta segment and the echo are all
    // correctly outside it.
    let tail = AgentService::load_messages_since_last_compaction(pool.clone(), session_id)
        .await
        .unwrap();
    assert_eq!(
        tail.iter().map(|m| m.sequence).collect::<Vec<_>>(),
        vec![6, 7],
        "expected the window to start at the quoting boundary"
    );
}

#[tokio::test]
async fn a_session_with_only_a_delta_banner_loads_its_whole_history() {
    // Delta banners EXTEND the window, so they are not boundaries: with no real
    // marker anywhere, the window is the whole session.
    let delta = format!("{COMPACTION_MARKER_PREFIX} — {SEGMENT_SENTINEL} segment two");
    let (pool, session_id) = seed(&[
        ("user", "one", 1),
        ("user", &delta, 2),
        ("assistant", "two", 3),
    ])
    .await;

    assert_forms_agree(&pool, session_id).await;

    let tail = AgentService::load_messages_since_last_compaction(pool.clone(), session_id)
        .await
        .unwrap();
    assert_eq!(tail.len(), 3, "a delta banner must not anchor the window");
}

#[tokio::test]
async fn a_reused_sequence_before_the_boundary_is_not_swept_into_the_tail() {
    // `MessageService::get_next_sequence` is `count(*) + 1`, so a delete in the
    // middle of a session lets a later insert reuse an earlier sequence. Here
    // row A holds the SAME sequence as the boundary row B and sorts before it.
    // A `sequence >= boundary` tail would drag A into the window; the keyset on
    // `(sequence, rowid)` must not.
    let marker = format!("{COMPACTION_MARKER_PREFIX}] summary");
    let (pool, session_id) = seed(&[
        ("user", "before the boundary, same sequence", 2),
        ("user", &marker, 2),
        ("assistant", "after the boundary", 3),
    ])
    .await;

    assert_forms_agree(&pool, session_id).await;

    let tail = AgentService::load_messages_since_last_compaction(pool.clone(), session_id)
        .await
        .unwrap();
    assert_eq!(
        tail.iter().map(|m| m.sequence).collect::<Vec<_>>(),
        vec![2, 3],
        "the window must start AT the boundary row, not at the start of its sequence"
    );
    assert!(
        !tail.iter().any(|m| m.content.contains("before the boundary")),
        "a row that sorts before the boundary was swept into the tail — the keyset \
         degraded to a bare `sequence >=`"
    );
}
