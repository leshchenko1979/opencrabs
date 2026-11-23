//! Diagnosis fixtures for context-recovery failures around compaction boundaries.
//!
//! These encode the production report: boundary selection keyed on `sequence`
//! (not insertion order) plus `count + 1` sequence assignment after deletes can
//! anchor on a stale marker and drop every newer row whose sequence is lower.

use crate::brain::agent::AgentService;
use crate::brain::agent::context::{COMPACTION_MARKER_PREFIX, SEGMENT_SENTINEL};
use crate::db::Database;
use crate::db::MessageRepository;
use crate::db::models::Message;
use crate::services::{MessageService, ServiceContext, SessionService};
use chrono::Utc;
use uuid::Uuid;

async fn seed(rows: &[(&str, &str, i32)]) -> (crate::db::Pool, Uuid) {
    let db = Database::connect_in_memory().await.unwrap();
    db.run_migrations().await.unwrap();
    let pool = db.pool().clone();

    let session = SessionService::new(ServiceContext::new(pool.clone()))
        .create_session(Some("context-recovery-diagnosis".to_string()))
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

/// Failure mode 1: the loader must anchor on the newest FULL marker (largest
/// rowid), not an older marker whose `sequence` number is larger after reuse.
#[tokio::test]
async fn compaction_boundary_must_follow_newest_marker_not_highest_sequence() {
    let stale = format!("{COMPACTION_MARKER_PREFIX} — stale high-sequence marker");
    let fresh = format!(
        "{COMPACTION_MARKER_PREFIX} — The conversation was automatically compacted.\n\nFresh summary."
    );

    let (pool, session_id) = seed(&[
        ("user", "ancient history", 1),
        ("user", &stale, 500),
        ("user", &format!("post-stale tail {}", 1), 2),
        ("user", &format!("post-stale tail {}", 2), 3),
        ("user", &fresh, 4),
        ("assistant", "visible after fresh marker", 5),
    ])
    .await;

    let loaded = AgentService::load_messages_since_last_compaction(pool.clone(), session_id)
        .await
        .unwrap();

    assert_eq!(
        loaded.first().map(|m| m.content.as_str()),
        Some(fresh.as_str()),
        "boundary must be the newest full marker, not the stale seq=500 row"
    );
    assert!(
        loaded.iter().any(|m| m.content == "visible after fresh marker"),
        "tail after the fresh marker must survive the reload"
    );
}

/// Failure mode 2: rows after the boundary with reused `sequence` values must
/// not be dropped when the boundary row used a higher sequence number.
#[tokio::test]
async fn compaction_tail_must_not_drop_newer_rows_after_sequence_reuse() {
    let stale = format!("{COMPACTION_MARKER_PREFIX} — stale anchor");
    let (pool, session_id) = seed(&[("user", &stale, 500)]).await;

    let svc = MessageService::new(ServiceContext::new(pool.clone()));
    let excluded = 328usize;
    for i in 0..excluded {
        svc.create_message(
            session_id,
            "user".to_string(),
            format!("newer message {i}"),
        )
        .await
        .unwrap();
    }
    svc.create_message(
        session_id,
        "user".to_string(),
        format!(
            "{COMPACTION_MARKER_PREFIX} — The conversation was automatically compacted.\n\nAfter reuse."
        ),
    )
    .await
    .unwrap();
    svc.create_message(
        session_id,
        "assistant".to_string(),
        "must stay visible".to_string(),
    )
    .await
    .unwrap();

    let (loaded, total) = {
        let repo = MessageRepository::new(pool.clone());
        let delta_banner = format!("{COMPACTION_MARKER_PREFIX} — {SEGMENT_SENTINEL}");
        let (tail, total) = repo
            .find_since_last_marker(
                session_id,
                "user",
                COMPACTION_MARKER_PREFIX,
                &delta_banner,
            )
            .await
            .unwrap();
        (tail, total)
    };

    let skipped = total.saturating_sub(loaded.len());
    assert_eq!(
        skipped, 0,
        "expected zero rows skipped after the newest marker, but loader excluded {skipped} \
         (production sample excluded 328)"
    );
    assert!(
        loaded.iter().any(|m| m.content == "must stay visible"),
        "assistant tail after the newest marker must be in the loaded window"
    );
}

/// Failure mode 3 (historical): a full-window marker whose body quotes the
/// delta sentinel must still count as a boundary. Fixed by anchored
/// `starts_with` / `instr = 1` matching (#1928); this test is the regression lock.
#[test]
fn full_marker_quoting_sentinel_in_body_remains_the_boundary() {
    let rows = vec![
        Message {
            id: Uuid::new_v4(),
            session_id: Uuid::nil(),
            role: "user".to_string(),
            content: "pre-marker".to_string(),
            sequence: 1,
            created_at: Utc::now(),
            token_count: None,
            cost: None,
            input_tokens: None,
            cache_creation_tokens: None,
            cache_read_tokens: None,
            thinking: None,
            duration_secs: None,
        },
        Message {
            id: Uuid::new_v4(),
            session_id: Uuid::nil(),
            role: "user".to_string(),
            content: format!(
                "{COMPACTION_MARKER_PREFIX} — automatically compacted.\n\nBody mentions {SEGMENT_SENTINEL} in prose."
            ),
            sequence: 2,
            created_at: Utc::now(),
            token_count: None,
            cost: None,
            input_tokens: None,
            cache_creation_tokens: None,
            cache_read_tokens: None,
            thinking: None,
            duration_secs: None,
        },
        Message {
            id: Uuid::new_v4(),
            session_id: Uuid::nil(),
            role: "user".to_string(),
            content: format!("{COMPACTION_MARKER_PREFIX} — {SEGMENT_SENTINEL} delta segment"),
            sequence: 3,
            created_at: Utc::now(),
            token_count: None,
            cost: None,
            input_tokens: None,
            cache_creation_tokens: None,
            cache_read_tokens: None,
            thinking: None,
            duration_secs: None,
        },
        Message {
            id: Uuid::new_v4(),
            session_id: Uuid::nil(),
            role: "assistant".to_string(),
            content: "tail".to_string(),
            sequence: 4,
            created_at: Utc::now(),
            token_count: None,
            cost: None,
            input_tokens: None,
            cache_creation_tokens: None,
            cache_read_tokens: None,
            thinking: None,
            duration_secs: None,
        },
    ];

    let kept = AgentService::messages_from_last_compaction(rows);
    assert_eq!(kept.len(), 3);
    assert!(
        kept[0].content.contains("Body mentions"),
        "full marker with quoted sentinel must anchor reload"
    );
}