//! Idle post-turn consolidation apply timing (#1023 fix 1).
//!
//! # The defect these tests pin
//!
//! The idle "eager apply" used to be gated on a CONTENT HASH of the first
//! `snapshot_len` messages (`compaction_prefix_fingerprint` /
//! `snapshot_prefix_hash`), computed on two structurally different objects:
//!
//! - at spawn, on the LIVE `AgentContext` — `Vec<ContentBlock>` with `ToolUse`,
//!   `ToolResult` and `Thinking` variants intact;
//! - at apply, on a context rebuilt from the DB by `load_compaction_window` +
//!   `clean_rows_for_llm` + `AgentContext::from_db_messages` — every row
//!   rehydrated to `Text` (plus an optional `Thinking` block).
//!
//! Any session with tool rows or reasoning therefore produced different bytes
//! on the two sides, so the eager apply could NEVER pass. The
//! `snapshot_len.min(len)` clamp hid a second divergence, since the reloaded
//! window is shorter and a different number of messages was hashed. Measured on
//! the live box: 10 `background_spawn` idle post-turns, **0** applies, 5
//! `eager apply skipped: prefix changed since spawn`.
//!
//! The guard now compares the compaction BOUNDARY's identity — the `rowid` of
//! the newest RESTART marker (`MessageRepository::latest_boundary_rowid`).
//! `messages` is append-only by design, so that single value is the complete
//! witness: rows APPENDED after spawn are legitimate (the apply re-appends them
//! as the delta) and must not invalidate the summary; only a compaction that
//! moved the boundary may.
//!
//! The old test in this file built `snapshot_prefix_hash` from the SAME
//! `context` it then resolved against, so it matched by construction and hid
//! the bug. The tests below build the two sides the way production does.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::{Arc, Mutex};

use crate::brain::agent::compaction_boundary::BoundarySpec;
use crate::brain::agent::context::{AgentContext, COMPACTION_MARKER_PREFIX, CompactionScope};
use crate::brain::agent::service::AgentService;
use crate::brain::agent::service::compaction::{
    BudgetPhase, CompactionOutcome, PendingState, compaction_boundary_moved,
    finished_idle_pending_for_tests,
};
use crate::brain::agent::service::context::CompactionDocument;
use crate::brain::agent::service::{ProgressCallback, ProgressEvent};
use crate::brain::provider::{ContentBlock, Message, Role};
use crate::channels::telegram::flow::{
    ToolOffloadNotice, idle_compacted_flow_line, tool_offload_notice,
};
use crate::channels::telegram::intermediates::{is_compaction_card, should_promote_intermediate};
use crate::db::models::Message as DbMessage;
use crate::db::{Database, MessageRepository, Pool};
use crate::services::ServiceContext;
use crate::tests::agent_service_mocks::MockProvider;
use uuid::Uuid;

async fn service() -> AgentService {
    let db = Database::connect_in_memory().await.unwrap();
    db.run_migrations().await.unwrap();
    AgentService::new_for_test(
        Arc::new(MockProvider),
        ServiceContext::new(db.pool().clone()),
    )
    .await
}

fn progress_log() -> (ProgressCallback, Arc<Mutex<Vec<ProgressEvent>>>) {
    let log = Arc::new(Mutex::new(Vec::new()));
    let sink = log.clone();
    let cb: ProgressCallback = Arc::new(move |_sid, event| {
        sink.lock().unwrap().push(event);
    });
    (cb, log)
}

fn consolidation_fixture() -> (Uuid, AgentContext) {
    let session_id = Uuid::new_v4();
    let mut c = AgentContext::new(session_id, 400_000);
    for i in 0..6 {
        c.add_message(Message::user(format!("q{i}")));
        AgentService::apply_scoped_compaction_summary(
            &mut c,
            CompactionScope::DeltaSinceMarker,
            &format!("MARKER_{i}"),
            None,
        );
    }
    c.add_message(Message::user("live tail"));
    assert!(c.frozen_segment_count() >= 5);
    (session_id, c)
}

fn idle_document() -> CompactionDocument {
    CompactionDocument {
        text: "## 1. Summary\n\nIdle merge doc.".to_string(),
        truncated: false,
    }
}

fn had_idle_receipt(events: &[ProgressEvent]) -> bool {
    events
        .iter()
        .any(|e| matches!(e, ProgressEvent::IdleCompactionSummary { .. }))
}

// ---------------------------------------------------------------------------
// The real divergence: a live spawn context vs the DB-rehydrated apply context
// ---------------------------------------------------------------------------

/// A restart marker's banner. Only the prefix decides the class; the real shape
/// just keeps the fixture honest.
fn restart_banner() -> String {
    format!("{COMPACTION_MARKER_PREFIX} — The conversation was compacted.")
}

/// The spawn-side context a live session holds: `ToolUse`, `ToolResult` and
/// `Thinking` blocks, exactly the variants the DB path cannot reproduce.
///
/// The conversation is the same one [`seed_tool_session`] writes to the DB —
/// the point is that the SAME conversation has two different in-memory shapes.
fn spawn_side_context(session_id: Uuid) -> AgentContext {
    let mut c = AgentContext::new(session_id, 200_000);
    // The window starts AT the boundary marker, exactly as the live context
    // does: everything above it was summarised away by an earlier compaction.
    c.add_message(Message::user(restart_banner()));
    c.add_message(Message {
        role: Role::Assistant,
        content: vec![
            ContentBlock::Thinking {
                thinking: "the user wants a file count".to_string(),
                signature: Some("sig-1".to_string()),
            },
            ContentBlock::ToolUse {
                id: "toolu_1".to_string(),
                name: "bash".to_string(),
                input: serde_json::json!({"command": "ls | wc -l"}),
            },
        ],
    });
    c.add_message(Message {
        role: Role::User,
        content: vec![ContentBlock::ToolResult {
            tool_use_id: "toolu_1".to_string(),
            content: "42".to_string(),
            is_error: None,
        }],
    });
    c
}

fn db_row(
    session_id: Uuid,
    role: &str,
    content: &str,
    thinking: Option<&str>,
    sequence: i32,
) -> DbMessage {
    DbMessage {
        id: Uuid::new_v4(),
        session_id,
        role: role.to_string(),
        content: content.to_string(),
        sequence,
        created_at: chrono::Utc::now(),
        token_count: None,
        cost: None,
        input_tokens: None,
        cache_creation_tokens: None,
        cache_read_tokens: None,
        thinking: thinking.map(str::to_string),
        duration_secs: None,
    }
}

/// Seed the SAME conversation as [`spawn_side_context`] into a real DB, with a
/// restart marker at the boundary. Returns the service and the session id.
///
/// The tool call is stored as its rendered text — that is what the DB holds and
/// what `from_db_messages` can rehydrate — which is precisely why the two sides
/// hash differently.
async fn seed_tool_session() -> (AgentService, Uuid, Pool) {
    let db = Database::connect_in_memory().await.unwrap();
    db.run_migrations().await.unwrap();
    let pool = db.pool().clone();
    let svc =
        AgentService::new_for_test(Arc::new(MockProvider), ServiceContext::new(pool.clone())).await;
    // `messages.session_id` carries a foreign key, so the session row must
    // exist before any row is written (PRAGMA foreign_keys = ON).
    let session = crate::services::SessionService::new(ServiceContext::new(pool.clone()))
        .create_session(Some("1023 anchor".to_string()))
        .await
        .unwrap();
    let session_id = session.id;
    let repo = MessageRepository::new(pool.clone());
    let repo_pool = pool;
    let rows = vec![
        // Above the boundary: summarised away, not part of the window.
        db_row(session_id, "user", "summarise this", None, 1),
        // The boundary itself.
        db_row(session_id, "user", &restart_banner(), None, 2),
        db_row(
            session_id,
            "assistant",
            "calling bash: ls | wc -l",
            Some("the user wants a file count"),
            3,
        ),
        db_row(session_id, "user", "42", None, 4),
        // Empty content and no reasoning: `from_db_messages` SKIPS this row
        // (Anthropic rejects empty text blocks), so the reloaded message vector
        // is shorter than the window's row count. That is the second divergence
        // the old `snapshot_len.min(len)` clamp papered over.
        db_row(session_id, "assistant", "", None, 5),
    ];
    for row in rows {
        repo.create(&row).await.unwrap();
    }
    (svc, session_id, repo_pool)
}

/// The apply-side context, built exactly the way production builds it in
/// `on_idle_compaction_ready`.
async fn apply_side_context(pool: &Pool, session_id: Uuid) -> AgentContext {
    let mut rows = MessageRepository::new(pool.clone())
        .load_compaction_window(session_id, &BoundarySpec::canonical())
        .await
        .unwrap()
        .messages;
    crate::brain::agent::service::context_rows::clean_rows_for_llm(&mut rows, false);
    AgentContext::from_db_messages(session_id, rows, 200_000)
}

/// Reproduces the DELETED `compaction_prefix_fingerprint` byte for byte, so a
/// test can show what the old guard actually compared. Nothing in production
/// calls this; it exists only to prove the defect.
fn legacy_content_hash(context: &AgentContext, snapshot_len: usize) -> u64 {
    let mut hasher = DefaultHasher::new();
    let end = snapshot_len.min(context.messages.len());
    for msg in &context.messages[..end] {
        serde_json::to_string(&msg.role)
            .unwrap_or_default()
            .hash(&mut hasher);
        serde_json::to_string(&msg.content)
            .unwrap_or_default()
            .hash(&mut hasher);
    }
    hasher.finish()
}

async fn boundary_rowid(pool: &Pool, session_id: Uuid) -> Option<i64> {
    MessageRepository::new(pool.clone())
        .latest_boundary_rowid(session_id, &BoundarySpec::canonical())
        .await
        .unwrap()
}

/// The regression this fix exists for.
///
/// Same conversation, two shapes: the live context holds `ToolUse`,
/// `ToolResult` and `Thinking`; the reloaded context holds only `Text` (plus a
/// hoisted `Thinking`). The old content hash therefore DIFFERED and the eager
/// apply could never run. The boundary anchor — read from the DB, independent
/// of either shape — matches, so the apply proceeds.
#[tokio::test]
async fn anchor_matches_across_live_and_reloaded_shapes_where_the_old_hash_did_not() {
    let (_svc, session_id, pool) = seed_tool_session().await;
    let live = spawn_side_context(session_id);
    let reloaded = apply_side_context(&pool, session_id).await;

    // Precondition: the two contexts really are structurally different.
    let live_has_tool_blocks = live.messages.iter().any(|m| {
        m.content.iter().any(|b| {
            matches!(
                b,
                ContentBlock::ToolUse { .. } | ContentBlock::ToolResult { .. }
            )
        })
    });
    let reloaded_has_tool_blocks = reloaded.messages.iter().any(|m| {
        m.content.iter().any(|b| {
            matches!(
                b,
                ContentBlock::ToolUse { .. } | ContentBlock::ToolResult { .. }
            )
        })
    });
    assert!(
        live_has_tool_blocks,
        "the live fixture must carry tool blocks"
    );
    assert!(
        !reloaded_has_tool_blocks,
        "the DB path cannot rehydrate tool blocks — that is the divergence"
    );
    // The clamp's hidden divergence: the reloaded WINDOW has one more row than
    // the message vector `from_db_messages` builds from it, so hashing
    // `snapshot_len` elements of each compared a different number of messages.
    let window = MessageRepository::new(pool.clone())
        .load_compaction_window(session_id, &BoundarySpec::canonical())
        .await
        .unwrap();
    assert_eq!(window.messages.len(), 4, "rows at or after the boundary");
    assert_eq!(window.total_rows, 5, "rows in the session");
    assert_eq!(
        reloaded.messages.len(),
        3,
        "the empty assistant row is skipped on rehydration"
    );
    assert_eq!(
        live.messages.len(),
        3,
        "the live window holds the same three messages, in a different shape"
    );

    // What the OLD guard compared: different bytes for the same conversation.
    // This is the assertion that pins the defect — the eager apply could never
    // get past this comparison.
    assert_ne!(
        legacy_content_hash(&live, live.messages.len()),
        legacy_content_hash(&reloaded, reloaded.messages.len()),
        "the old content hash must differ across the two shapes — if it matched, \
         the old test would have been honest and #1023 would not exist"
    );
    // Even with the clamp pinned to the live length, the two sides differ: the
    // defect is the SHAPE, not the count.
    assert_ne!(
        legacy_content_hash(&live, live.messages.len()),
        legacy_content_hash(&reloaded, live.messages.len()),
        "the min-clamp does not rescue the old guard"
    );

    // What the NEW guard compares: one boundary, the same on both sides.
    let spawn_anchor = boundary_rowid(&pool, session_id).await;
    let apply_anchor = boundary_rowid(&pool, session_id).await;
    assert!(
        spawn_anchor.is_some(),
        "the fixture writes a restart marker, so there is a boundary"
    );
    assert_eq!(
        spawn_anchor, apply_anchor,
        "the boundary is a property of the DB, not of the in-memory shape"
    );
    assert!(
        !compaction_boundary_moved(spawn_anchor, apply_anchor),
        "the eager apply must proceed: the boundary did not move"
    );
}

/// The negative case: a boundary that MOVED still rejects the document. Without
/// this, the fix would be indistinguishable from deleting the guard.
#[test]
fn a_moved_boundary_is_still_rejected() {
    // Same shapes, boundary moved: an earlier marker was superseded.
    assert!(compaction_boundary_moved(Some(41), Some(97)));
    // No boundary at spawn, one now — the window is not the one summarised.
    assert!(compaction_boundary_moved(None, Some(97)));
    // A boundary at spawn that has since vanished.
    assert!(compaction_boundary_moved(Some(41), None));
    // And the pairs that must NOT be treated as a move.
    assert!(!compaction_boundary_moved(Some(41), Some(41)));
    assert!(!compaction_boundary_moved(None, None));
}

/// End to end through the production resolve path, with the two sides shaped
/// the way they really are: the pending was spawned against the LIVE context
/// (tool blocks, reasoning) and the visit resolves against the DB-REHYDRATED
/// one. Under the old content hash this visit discarded the summary; with the
/// boundary anchor it applies.
#[tokio::test]
async fn eager_apply_uses_the_anchor_so_a_reloaded_context_still_applies() {
    let (svc, session_id, pool) = seed_tool_session().await;
    let spawn_anchor = boundary_rowid(&pool, session_id).await;
    let live = spawn_side_context(session_id);
    svc.insert_pending_compaction_for_tests(
        session_id,
        finished_idle_pending_for_tests(&live, idle_document(), 0, spawn_anchor),
    );

    // The apply side is the DB rehydration, exactly as `on_idle_compaction_ready`
    // builds it — text-only rows, no ToolUse/ToolResult blocks.
    let mut reloaded = apply_side_context(&pool, session_id).await;
    let state = svc
        .resolve_pending_compaction_for_tests(
            session_id,
            &mut reloaded,
            BudgetPhase::PostTurn,
            40.0,
            &None,
        )
        .await;
    assert!(
        matches!(state, PendingState::Applied(_)),
        "the boundary is unchanged, so a reloaded apply context must still land"
    );
}

/// The negative case: a boundary that MOVED still rejects the document, through
/// the same production path. Without this the fix would be indistinguishable
/// from deleting the guard.
#[tokio::test]
async fn a_moved_boundary_still_discards_through_the_resolve_path() {
    let (svc, session_id, pool) = seed_tool_session().await;
    let spawn_anchor = boundary_rowid(&pool, session_id).await;
    // A spawn that recorded a different boundary — a compaction moved it since.
    let moved = Some(spawn_anchor.unwrap_or(0) + 1);

    let mut ctx = apply_side_context(&pool, session_id).await;
    svc.insert_pending_compaction_for_tests(
        session_id,
        finished_idle_pending_for_tests(&ctx, idle_document(), 0, moved),
    );
    let state = svc
        .resolve_pending_compaction_for_tests(
            session_id,
            &mut ctx,
            BudgetPhase::PostTurn,
            40.0,
            &None,
        )
        .await;
    assert!(
        matches!(state, PendingState::Empty),
        "a moved boundary must discard without apply"
    );
    assert!(!svc.has_pending_compaction_for_tests(session_id));
}

// ---------------------------------------------------------------------------
// Hold / settle timing (unchanged behaviour)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn held_idle_applies_at_settle_not_midloop_under_threshold() {
    let svc = service().await;
    let (session_id, mut ctx) = consolidation_fixture();
    let turn_gen = svc.session_turn_generation_for_tests(session_id);
    // Nothing persisted for this session, so the boundary is `None` on both
    // sides and the guard is satisfied by construction.
    let pending = finished_idle_pending_for_tests(&ctx, idle_document(), turn_gen, None);
    svc.insert_pending_compaction_for_tests(session_id, pending);

    let (cb, log) = progress_log();
    let mid = svc
        .resolve_pending_compaction_for_tests(
            session_id,
            &mut ctx,
            BudgetPhase::MidLoop,
            40.0,
            &Some(cb.clone()),
        )
        .await;
    assert!(
        matches!(mid, PendingState::StillRunning),
        "under 65% mid-loop must hold the idle result"
    );
    assert!(
        svc.has_pending_compaction_for_tests(session_id),
        "pending slot must stay occupied"
    );

    let settle = svc
        .apply_held_idle_compaction_at_settle(session_id, &mut ctx, &Some(cb))
        .await
        .expect("settle must apply");
    assert!(matches!(settle, CompactionOutcome::Summarised(_)));
    assert!(had_idle_receipt(&log.lock().unwrap()));
}

#[tokio::test]
async fn held_idle_applies_at_midloop_threshold_without_new_summariser() {
    let svc = service().await;
    let (session_id, mut ctx) = consolidation_fixture();
    let turn_gen = svc.session_turn_generation_for_tests(session_id);
    svc.insert_pending_compaction_for_tests(
        session_id,
        finished_idle_pending_for_tests(&ctx, idle_document(), turn_gen, None),
    );

    let (cb, log) = progress_log();
    let state = svc
        .resolve_pending_compaction_for_tests(
            session_id,
            &mut ctx,
            BudgetPhase::MidLoop,
            66.5,
            &Some(cb),
        )
        .await;
    assert!(
        matches!(state, PendingState::Applied(_)),
        "above 65% mid-loop must apply the held idle summary"
    );
    assert!(had_idle_receipt(&log.lock().unwrap()));
}

/// Held results are discarded at settle when the boundary moved under them
/// (#1023): the old "prefix changed" test, now driven by the boundary rowid
/// instead of by mutating the in-memory prefix.
#[tokio::test]
async fn held_idle_discarded_at_settle_when_the_boundary_moved() {
    let svc = service().await;
    let (session_id, mut ctx) = consolidation_fixture();
    let turn_gen = svc.session_turn_generation_for_tests(session_id);
    // The DB has no rows for this session, so the current boundary is `None`;
    // a spawn that recorded `Some(97)` therefore describes a window that no
    // longer is.
    svc.insert_pending_compaction_for_tests(
        session_id,
        finished_idle_pending_for_tests(&ctx, idle_document(), turn_gen, Some(97)),
    );

    let settle = svc
        .apply_held_idle_compaction_at_settle(session_id, &mut ctx, &None)
        .await;
    assert!(
        settle.is_none(),
        "a moved boundary must discard the held result at settle"
    );
}

#[test]
fn offload_and_idle_merge_notices_promote_as_compaction_receipts() {
    let offload = tool_offload_notice(&ToolOffloadNotice {
        blocks_offloaded: 2,
        tokens_freed: 4_000,
        before_pct: 70.0,
        after_pct: 58.0,
        before_tokens: 70_000,
        after_tokens: 58_000,
        ctx_max: 100_000,
    });
    assert!(is_compaction_card(&offload));
    assert!(should_promote_intermediate(&offload, 0));

    let idle = format!(
        "{}\n   **idle consolidation**",
        idle_compacted_flow_line(42.0, 38.0, 5, 3, std::time::Duration::from_secs(3))
    );
    assert!(is_compaction_card(&idle));
    assert!(should_promote_intermediate(&idle, 0));
}
