//! #854/#987 — the SQL compaction window must equal the in-memory trim.
//!
//! Pushing the boundary into SQL means the predicate has two READERS:
//! `messages_from_last_compaction` (over an already-loaded `Vec`) and
//! `MessageRepository::load_compaction_window` (as a query). Two readers of one
//! rule still drift, so these tests pin them to the SAME answer over the same
//! fixture — and since #987 both go through
//! `compaction_boundary::boundary_start_index`, so a drift here means the SQL
//! pre-filter and the Rust classifier have parted company.
//!
//! The fixture deliberately includes every row the predicate's history turned
//! on: a delta-segment banner (extends the window, not a boundary), a summary
//! whose BODY quotes the sentinel (still a boundary, #1928), a SUSPENDED row
//! (nothing was summarised — not a boundary, #987) and an assistant row echoing
//! the prefix (not a marker, #175).

use crate::brain::agent::AgentService;
use crate::brain::agent::compaction_boundary::{
    BoundarySpec, MarkerKind, SUSPENDED_MARKER_PREFIX, classify_user_marker,
};
use crate::brain::agent::context::{COMPACTION_MARKER_PREFIX, SEGMENT_SENTINEL};
use crate::db::Database;
use crate::db::models::Message;
use crate::db::{CompactionWindow, MessageRepository};
use crate::services::{ServiceContext, SessionService};
use chrono::Utc;
use uuid::Uuid;

/// Seed a session with rows whose sequence numbers the caller sets explicitly,
/// so tie behaviour is under the test's control rather than the writer's.
/// Returns the ids in insertion order, so a test can name the boundary row.
async fn seed(rows: &[(&str, &str, i32)]) -> (crate::db::Pool, Uuid, Vec<Uuid>) {
    let db = Database::connect_in_memory().await.unwrap();
    db.run_migrations().await.unwrap();
    let pool = db.pool().clone();

    let session = SessionService::new(ServiceContext::new(pool.clone()))
        .create_session(Some("boundary".to_string()))
        .await
        .unwrap();

    let repo = MessageRepository::new(pool.clone());
    let mut ids = Vec::with_capacity(rows.len());
    for (role, content, sequence) in rows {
        let id = Uuid::new_v4();
        repo.create(&Message {
            id,
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
            resolved_model: None,
        })
        .await
        .unwrap();
        ids.push(id);
    }

    (pool, session.id, ids)
}

/// The SQL window for a session.
async fn window(pool: &crate::db::Pool, session_id: Uuid) -> CompactionWindow {
    MessageRepository::new(pool.clone())
        .load_compaction_window(session_id, &BoundarySpec::canonical())
        .await
        .unwrap()
}

/// The whole point of the fix: the two forms of the boundary must agree, row
/// for row.
async fn assert_forms_agree(pool: &crate::db::Pool, session_id: Uuid) {
    let repo = MessageRepository::new(pool.clone());
    let full = repo.find_by_session(session_id).await.unwrap();
    let in_memory = AgentService::messages_from_last_compaction(full);
    let from_sql = window(pool, session_id).await;

    let in_memory_ids: Vec<Uuid> = in_memory.iter().map(|m| m.id).collect();
    let from_sql_ids: Vec<Uuid> = from_sql.messages.iter().map(|m| m.id).collect();
    assert_eq!(
        from_sql_ids, in_memory_ids,
        "SQL tail and in-memory trim disagree — the compaction predicate has drifted \
         between its two implementations"
    );
    assert_eq!(
        from_sql.excluded_rows(),
        from_sql.total_rows - from_sql.messages.len(),
        "the reported skip count must equal total minus tail"
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

    let (pool, session_id, ids) = seed(&[
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
    let loaded = window(&pool, session_id).await;
    assert_eq!(
        loaded
            .messages
            .iter()
            .map(|m| m.sequence)
            .collect::<Vec<_>>(),
        vec![6, 7],
        "expected the window to start at the quoting boundary"
    );

    // #987: the window now carries its own provenance instead of making the
    // caller re-derive it from a row count.
    let boundary = loaded.boundary.expect("a boundary must be reported");
    assert_eq!(
        boundary.message_id, ids[5],
        "the reported boundary must be the quoting row"
    );
    assert_eq!(boundary.kind, MarkerKind::Restart);
    assert_eq!(loaded.total_rows, 7);
    assert_eq!(loaded.excluded_rows(), 5);
}

#[tokio::test]
async fn a_session_with_only_a_delta_banner_loads_its_whole_history() {
    // Delta banners EXTEND the window, so they are not boundaries: with no real
    // marker anywhere, the window is the whole session.
    let delta = format!("{COMPACTION_MARKER_PREFIX} — {SEGMENT_SENTINEL} segment two");
    let (pool, session_id, _) = seed(&[
        ("user", "one", 1),
        ("user", &delta, 2),
        ("assistant", "two", 3),
    ])
    .await;

    assert_forms_agree(&pool, session_id).await;

    let loaded = window(&pool, session_id).await;
    assert_eq!(
        loaded.messages.len(),
        3,
        "a delta banner must not anchor the window"
    );
    assert!(loaded.boundary.is_none());
    assert_eq!(loaded.excluded_rows(), 0);
}

/// #987 behaviour change: the loop guard's SUSPENDED row is not a fresh start.
///
/// `CompactionOutcome::LoopGuard` declined to compact, so NOTHING was
/// summarised and the history above it is still live context. Anchoring a
/// reload there would drop that history — the exact loss the row's own text
/// says did not happen. Before #987 the loader excluded only the delta banner,
/// so it treated this row as a boundary.
#[tokio::test]
async fn a_suspended_row_does_not_anchor_the_window() {
    let suspended = format!(
        "{SUSPENDED_MARKER_PREFIX} — The conversation crossed the compaction threshold, \
         but the last 3 compactions ran with no completed tool call between them. Nothing \
         was summarised: this is the harness breaking a loop, not a fresh start."
    );
    let (pool, session_id, _) = seed(&[
        ("user", "still live history", 1),
        ("assistant", "still live answer", 2),
        ("user", &suspended, 3),
        ("assistant", "after the guard", 4),
    ])
    .await;

    assert_forms_agree(&pool, session_id).await;

    let loaded = window(&pool, session_id).await;
    assert!(
        loaded.boundary.is_none(),
        "a suspended row summarised nothing, so it cannot be a reload boundary"
    );
    assert_eq!(
        loaded.messages.len(),
        4,
        "the history the guard declined to summarise must stay in the window"
    );
}

/// A real restart BEFORE a suspended row still anchors: the guard does not
/// erase the boundary an earlier compaction established.
#[tokio::test]
async fn a_suspended_row_after_a_restart_keeps_the_restart_boundary() {
    let marker =
        format!("{COMPACTION_MARKER_PREFIX} — The conversation was automatically compacted.");
    let suspended = format!("{SUSPENDED_MARKER_PREFIX} — Nothing was summarised.");
    let (pool, session_id, ids) = seed(&[
        ("user", "summarised away", 1),
        ("user", &marker, 2),
        ("assistant", "after the restart", 3),
        ("user", &suspended, 4),
        ("assistant", "after the guard", 5),
    ])
    .await;

    assert_forms_agree(&pool, session_id).await;

    let loaded = window(&pool, session_id).await;
    let boundary = loaded.boundary.expect("the restart must anchor the window");
    assert_eq!(boundary.message_id, ids[1]);
    assert_eq!(
        loaded.messages.len(),
        4,
        "the window runs from the restart through everything the guard left in place"
    );
}

#[tokio::test]
async fn a_reused_sequence_before_the_boundary_is_not_swept_into_the_tail() {
    // A delete can leave two rows sharing a `sequence` but different `rowid`s.
    // The tail is `rowid >= boundary`, so the older same-sequence row must not
    // be swept in when it sorts before the boundary by insertion order.
    let marker = format!("{COMPACTION_MARKER_PREFIX}] summary");
    let (pool, session_id, _) = seed(&[
        ("user", "before the boundary, same sequence", 2),
        ("user", &marker, 2),
        ("assistant", "after the boundary", 3),
    ])
    .await;

    assert_forms_agree(&pool, session_id).await;

    let loaded = window(&pool, session_id).await;
    assert_eq!(
        loaded
            .messages
            .iter()
            .map(|m| m.sequence)
            .collect::<Vec<_>>(),
        vec![2, 3],
        "the window must start AT the boundary row, not at the start of its sequence"
    );
    assert!(
        !loaded
            .messages
            .iter()
            .any(|m| m.content.contains("before the boundary")),
        "a row that sorts before the boundary was swept into the tail — the keyset \
         degraded to a bare `sequence >=`"
    );
}

/// The SQL pre-filter (`instr(content, prefix) = 1`) and the Rust classifier
/// must accept the same rows: a coarse `WHERE` NARROWER than the fine rule
/// would hide a real boundary from the loader, and the two halves would be
/// back to disagreeing.
#[test]
fn every_marker_kind_agrees_between_the_two_halves_of_the_rule() {
    let restart =
        format!("{COMPACTION_MARKER_PREFIX} — The conversation was automatically compacted.");
    let delta = format!("{COMPACTION_MARKER_PREFIX} — {SEGMENT_SENTINEL} only the tail");
    let suspended = format!("{SUSPENDED_MARKER_PREFIX} — Nothing was summarised.");
    let echo = format!("{COMPACTION_MARKER_PREFIX}] quoted by an assistant");
    let leading = format!(" see {COMPACTION_MARKER_PREFIX}] indented");

    // The classifier's verdict, which is what the loader trusts.
    assert_eq!(classify_user_marker(&restart), Some(MarkerKind::Restart));
    assert_eq!(classify_user_marker(&delta), Some(MarkerKind::DeltaSegment));
    assert_eq!(
        classify_user_marker(&suspended),
        Some(MarkerKind::Suspended)
    );
    assert_eq!(classify_user_marker(&echo), Some(MarkerKind::Restart));
    assert_eq!(classify_user_marker(&leading), None);

    // Every classified row must survive the SQL pre-filter; the reverse is not
    // required, because the classifier is the fine filter over a superset.
    for content in [&restart, &delta, &suspended, &echo] {
        assert!(
            content.starts_with(COMPACTION_MARKER_PREFIX),
            "a classified marker that the SQL pre-filter would drop: {content}"
        );
    }
}
