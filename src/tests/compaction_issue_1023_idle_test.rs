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
    BudgetPhase, CompactionOutcome, IDLE_CONSOLIDATION_MIN_SEGMENTS, PendingState,
    compaction_boundary_moved, finished_idle_pending_for_tests, idle_consolidation_min_gain_tokens,
    idle_consolidation_worth_the_cache, running_gate_pending_for_tests,
    running_idle_pending_for_tests,
};
use crate::brain::agent::service::context::CompactionDocument;
use crate::brain::agent::service::{ProgressCallback, ProgressEvent};
use crate::brain::provider::{
    ContentBlock, LLMRequest, LLMResponse, Message, Provider, ProviderStream, Role,
};
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

// ---------------------------------------------------------------------------
// Fixtures for the two steers below
// ---------------------------------------------------------------------------

/// Six frozen segments holding SUBSTANTIAL bodies, plus a live tail.
///
/// [`consolidation_fixture`] is the same shape with one-line markers, which is
/// the point of the pair: the two fixtures differ only in segment SIZE, so a
/// verdict that flips between them is reading the projection and not the
/// segment count.
fn substantial_segments_fixture() -> (Uuid, AgentContext) {
    let session_id = Uuid::new_v4();
    let mut c = AgentContext::new(session_id, 400_000);
    for i in 0..6 {
        c.add_message(Message::user(format!("q{i}")));
        AgentService::apply_scoped_compaction_summary(
            &mut c,
            CompactionScope::DeltaSinceMarker,
            &format!("MARKER_{i} {}", "detailed work log. ".repeat(900)),
            None,
        );
    }
    c.add_message(Message::user("live tail"));
    assert!(c.frozen_segment_count() >= 5);
    (session_id, c)
}

/// A summariser that never answers.
///
/// Used by the spawn test, which only asks whether a summariser was STARTED. A
/// provider that ANSWERED would write a daily memory note; one that FAILED
/// quickly would race the assertion, because `on_idle_compaction_ready` takes
/// the pending back out of the slot the moment a result lands, `Err` included.
/// Hanging keeps the slot exactly as the spawn left it, and the summariser's own
/// attempt deadline is what would eventually retire the task in production.
struct HangingSummariser;

#[async_trait::async_trait]
impl Provider for HangingSummariser {
    async fn complete(&self, _request: LLMRequest) -> crate::brain::provider::Result<LLMResponse> {
        std::future::pending().await
    }

    async fn stream(&self, _request: LLMRequest) -> crate::brain::provider::Result<ProviderStream> {
        std::future::pending().await
    }

    fn name(&self) -> &str {
        "hanging-summariser"
    }

    fn default_model(&self) -> &str {
        "mock-model"
    }

    fn supported_models(&self) -> Vec<String> {
        vec!["mock-model".to_string()]
    }

    fn context_window(&self, _model: &str) -> Option<u32> {
        Some(400_000)
    }

    fn calculate_cost(&self, _model: &str, _input: u32, _output: u32) -> f64 {
        0.0
    }
}

// ---------------------------------------------------------------------------
// Steer 1 (#1023): an idle merge must be worth the prompt cache it busts
// ---------------------------------------------------------------------------

/// The gate is a share of what SURVIVES the merge, so the same reclaim is a win
/// against a small prefix and a loss against a large one. Table-driven, and the
/// expectations are written as literal token counts rather than re-derived from
/// [`idle_consolidation_min_gain_tokens`] — a test that computes its answer from
/// the expression under test agrees with any threshold, including a wrong one.
#[test]
fn the_cache_gate_declines_a_small_win_and_accepts_a_large_one() {
    // (reclaim, retained, expected, why)
    let cases: &[(usize, usize, bool, &str)] = &[
        // 1% of a 70k prefix: ~1,150 calls to repay a full cache re-creation.
        (
            700,
            70_000,
            false,
            "a 1% reshuffle is the loss the owner named",
        ),
        // 12k of 48k: a quarter of what survives — the documented bar.
        (
            12_000,
            48_000,
            true,
            "a quarter reclaimed pays back in ~46 calls",
        ),
        (
            11_999,
            48_000,
            false,
            "one token under the bar is still under it",
        ),
        (
            0,
            0,
            false,
            "a merge that frees nothing cannot pay for the call",
        ),
        (0, 70_000, false, "nor can a no-op against a real prefix"),
        (5_000, 20_000, true, "exactly the bar is a pass"),
        (4_999, 20_000, false, "just below the bar is a decline"),
        (
            49_999,
            200_000,
            false,
            "20% of a 200k prefix is under the bar",
        ),
        (50_000, 200_000, true, "25% of 200k clears it exactly"),
        (80_000, 200_000, true, "40% clears it comfortably"),
    ];
    for (reclaim, retained, expected, why) in cases {
        assert_eq!(
            idle_consolidation_worth_the_cache(*reclaim, *retained),
            *expected,
            "reclaim {reclaim} against {retained} retained: {why}"
        );
    }
}

/// The threshold the wiring logs must be the same number the gate applies — a
/// decline line naming one bar while the code applies another is unreadable.
#[test]
fn the_logged_threshold_is_the_gates_own_bar() {
    // `retained == 0` is a special case, pinned separately: its bar is 0, so
    // "the bar itself passes" is vacuous there and the real floor is the
    // strict `reclaim > 0` (nothing freed, nothing pays for the call).
    for retained in [1usize, 2, 3, 999, 48_000, 70_001, 200_000] {
        let bar = idle_consolidation_min_gain_tokens(retained);
        assert!(
            idle_consolidation_worth_the_cache(bar, retained),
            "the bar itself must pass at {retained}"
        );
        assert!(
            !idle_consolidation_worth_the_cache(bar - 1, retained),
            "one token under the bar must decline at {retained}"
        );
    }
    assert_eq!(idle_consolidation_min_gain_tokens(0), 0);
    assert!(
        idle_consolidation_worth_the_cache(1, 0),
        "with nothing retained, any real reclaim passes the share test"
    );
    assert!(
        !idle_consolidation_worth_the_cache(0, 0),
        "but a merge that frees nothing never passes"
    );
}

/// A segment pile of tiny markers reclaims nothing: the merge replaces them with
/// a document of the same order of size. The projection must say so, and the gate
/// must then decline — otherwise the idle path would fire on every settle of a
/// heavily-compacted but small session.
#[test]
fn tiny_segments_project_no_reclaim_and_are_declined() {
    let (_session_id, ctx) = consolidation_fixture();
    let merged = ctx
        .incremental_consolidation_prefix()
        .unwrap_or_else(|| ctx.frozen_segment_count());
    let document = AgentContext::DEFAULT_SUMMARY_TOKENS;
    let (reclaim, retained) = ctx.projected_consolidation_reclaim(merged, document);
    assert_eq!(
        reclaim, 0,
        "six short markers cannot outweigh a {document}-token document"
    );
    assert!(retained > 0, "the tail and the later segments survive");
    assert!(!idle_consolidation_worth_the_cache(reclaim, retained));
}

/// The mirror image, through the production projection: a pile of SUBSTANTIAL
/// segments reclaims most of what it merges, so the gate passes. Same code path,
/// opposite verdict — the gate is reading the projection, not the segment count.
#[test]
fn substantial_segments_project_a_reclaim_worth_the_cache() {
    let (_session_id, ctx) = substantial_segments_fixture();
    assert!(ctx.frozen_segment_count() >= 5);

    let merged = ctx
        .incremental_consolidation_prefix()
        .expect("six segments merge a prefix of them");
    let (reclaim, retained) =
        ctx.projected_consolidation_reclaim(merged, AgentContext::DEFAULT_SUMMARY_TOKENS);
    assert!(
        reclaim > 0 && retained > 0,
        "fixture must both reclaim and retain something, got {reclaim}/{retained}"
    );
    assert!(
        idle_consolidation_worth_the_cache(reclaim, retained),
        "three multi-thousand-token segments against a 2k document must clear \
         the bar (reclaim {reclaim}, retained {retained})"
    );
}

/// The projection saturates: a document larger than the span it replaces makes
/// the merge GROW the window, and a signed subtraction would report that as a
/// reclaim the gate could be talked into accepting.
#[test]
fn a_document_bigger_than_the_segments_reclaims_nothing() {
    let (_session_id, ctx) = consolidation_fixture();
    let merged = ctx.frozen_segment_count();
    let (reclaim, _retained) = ctx.projected_consolidation_reclaim(merged, usize::MAX / 2);
    assert_eq!(reclaim, 0, "saturating, not wrapping");
}

// ---------------------------------------------------------------------------
// Steer 1 at the wiring: a small win never reaches the summariser
// ---------------------------------------------------------------------------

/// The gate is consulted by `maybe_spawn_idle_consolidation`, not merely defined
/// next to it. Every precondition the old code checked is asserted HERE first, so
/// the empty slot cannot be explained by an earlier early-return: the segment
/// pile is over the minimum, the fill is under 65%, and the slot is free.
#[tokio::test]
async fn idle_consolidation_does_not_spawn_when_the_win_is_small() {
    let svc = service().await;
    let (session_id, mut ctx) = consolidation_fixture();

    // Preconditions the pre-#1023 gate would have spawned on.
    assert!(ctx.frozen_segment_count() >= IDLE_CONSOLIDATION_MIN_SEGMENTS);
    assert!(
        ctx.usage_percentage() < 65.0,
        "the idle path only runs below the gate, got {:.1}%",
        ctx.usage_percentage()
    );
    assert!(!svc.has_pending_compaction_for_tests(session_id));

    // The cached prefix IS the rendered context, so byte-identity of
    // `ctx.messages` across the visit is the DIRECT form of "did not bust the
    // prompt cache" — no-spawn alone is only a proxy, because the offload that
    // follows the gate also rewrites rows (it stubs stale tool results). The
    // gate is evaluated BEFORE the offload precisely so a decline cannot touch
    // either: every byte of the prefix, and the merge point it names, must be
    // exactly where they were.
    let prefix_before = format!("{:?}", ctx.messages);
    let merge_point_before = ctx.incremental_consolidation_prefix();

    svc.maybe_spawn_idle_consolidation_for_tests(session_id, &mut ctx, "mock-model", &None)
        .await;

    assert_eq!(
        format!("{:?}", ctx.messages),
        prefix_before,
        "a declined merge must leave the cached prompt prefix byte-identical"
    );
    assert_eq!(
        ctx.incremental_consolidation_prefix(),
        merge_point_before,
        "the merge point must not move either"
    );
    assert!(
        !svc.has_pending_compaction_for_tests(session_id),
        "a merge that reclaims nothing must not bust the prompt cache"
    );
    assert_eq!(
        svc.pending_snapshot_len_for_tests(session_id),
        None,
        "no summariser may be registered for a declined merge"
    );
}

/// The positive control for the test above: with substantial segments the same
/// wiring DOES spawn. Without this the decline test would pass just as happily
/// against a function that never spawns anything.
///
/// The provider never answers, on purpose: a summariser that answered would write
/// a daily memory note, and one that FAILED fast would be retired from the slot
/// by `on_idle_compaction_ready` before the assertion below runs. This test is
/// about the decision to spawn, not about the document that follows.
#[tokio::test]
async fn idle_consolidation_spawns_when_the_win_is_large() {
    let db = Database::connect_in_memory().await.unwrap();
    db.run_migrations().await.unwrap();
    let svc = AgentService::new_for_test(
        Arc::new(HangingSummariser),
        ServiceContext::new(db.pool().clone()),
    )
    .await;

    let (session_id, mut ctx) = substantial_segments_fixture();
    assert!(ctx.usage_percentage() < 65.0);
    let expected_snapshot = ctx.messages.len();

    svc.maybe_spawn_idle_consolidation_for_tests(session_id, &mut ctx, "mock-model", &None)
        .await;

    assert!(
        svc.has_pending_compaction_for_tests(session_id),
        "a large reclaim must still reach the summariser"
    );
    assert_eq!(
        svc.pending_snapshot_len_for_tests(session_id),
        Some(expected_snapshot),
        "the registered summariser must be anchored on the live window"
    );
}

// ---------------------------------------------------------------------------
// Steer 2 (#1023): over 65% with a compaction already under way is not an
// emergency — no second summariser, and no blocking below the ceiling
// ---------------------------------------------------------------------------

/// An idle merge in flight, fill between the 65% trigger and the 80% ceiling:
/// the visit must hand the turn back with the slot untouched. Wrapped in a
/// timeout because "does not block" is only meaningful if the test can tell the
/// difference — see [`an_idle_merge_at_the_ceiling_does_block`].
#[tokio::test]
async fn an_idle_merge_in_flight_above_65_does_not_spawn_or_block() {
    let svc = service().await;
    let (session_id, mut ctx) = consolidation_fixture();
    // A snapshot length the live window cannot have: if a second summariser
    // were spawned, the slot would be rewritten with `ctx.messages.len()`.
    let sentinel = ctx.messages.len() + 41;
    svc.insert_pending_compaction_for_tests(
        session_id,
        running_idle_pending_for_tests(&ctx, sentinel, 0, None),
    );

    let (cb, log) = progress_log();
    let state = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        svc.resolve_pending_compaction_for_tests(
            session_id,
            &mut ctx,
            BudgetPhase::MidLoop,
            72.0,
            &Some(cb),
        ),
    )
    .await
    .expect("an in-flight merge at 72% must not block the visit");

    assert!(
        matches!(state, PendingState::StillRunning),
        "the summariser is still working, so the visit reports it and moves on"
    );
    assert_eq!(
        svc.pending_snapshot_len_for_tests(session_id),
        Some(sentinel),
        "the slot must be the SAME pending — no second summariser"
    );
    let events = log.lock().unwrap();
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, ProgressEvent::Compacting { .. })),
        "nothing is queued for replacement here, so nothing may announce one: {events:?}"
    );
    assert!(!had_idle_receipt(&events));
}

/// The same band, on the GATE-spawned path: a background summary started at 65%
/// keeps running at 72% without a second spawn, a second notice, or a wait.
#[tokio::test]
async fn a_gate_spawned_summary_in_flight_above_65_does_not_spawn_or_block() {
    let svc = service().await;
    let (session_id, mut ctx) = consolidation_fixture();
    let sentinel = ctx.messages.len() + 7;
    svc.insert_pending_compaction_for_tests(
        session_id,
        running_gate_pending_for_tests(&ctx, sentinel, None),
    );

    let (cb, log) = progress_log();
    let state = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        svc.resolve_pending_compaction_for_tests(
            session_id,
            &mut ctx,
            BudgetPhase::MidLoop,
            79.9,
            &Some(cb),
        ),
    )
    .await
    .expect("79.9% is below the back-pressure ceiling, so the visit must not block");

    assert!(matches!(state, PendingState::StillRunning));
    assert_eq!(
        svc.pending_snapshot_len_for_tests(session_id),
        Some(sentinel),
        "no second summariser may be spawned while one is in flight"
    );
    assert!(
        !log.lock()
            .unwrap()
            .iter()
            .any(|e| matches!(e, ProgressEvent::Compacting { .. })),
        "the user is not waiting, so no compaction notice is owed"
    );
}

// A known gap, and why the two tests below are written the way they are.
//
// `must_wait_for_compaction` returns true at `TurnStart` and at/over the 80%
// ceiling, and `resolve_pending_compaction` acts on it only for an IDLE pending
// (`pending.handle.is_none()`), which is awaited on `idle_done`. A GATE-spawned
// pending owns a join handle, so the `must_wait` branch announces the wait
// ("Waiting on background compaction" + a `Compacting` event) and then falls
// through `take_pending_document_result` -> `None if pending.is_still_running()`
// -> reinsert -> `StillRunning`, i.e. it does not await the handle the way the
// predicate's doc comment says it must.
//
// So these tests pin the ANNOUNCEMENT, which is real either way, and tolerate
// both outcomes for the block: demanding one would either bless the gap or fail
// the day it is closed. The contract itself is pinned where it belongs, as a
// pure predicate, in `compaction_background_test.rs`.

/// A turn about to answer, with a gate-spawned summary still running: the visit
/// announces that the context is queued for replacement.
///
/// The notice is emitted before any await, so it is asserted unconditionally; the
/// block is not asserted, for the reason above.
#[tokio::test]
async fn a_turn_start_visit_announces_a_running_gate_summary() {
    let svc = service().await;
    let (session_id, mut ctx) = consolidation_fixture();
    let sentinel = ctx.messages.len() + 3;
    svc.insert_pending_compaction_for_tests(
        session_id,
        running_gate_pending_for_tests(&ctx, sentinel, None),
    );

    let (cb, log) = progress_log();
    // The notice is emitted synchronously, before any await, so this budget only
    // has to cover the visit's own bookkeeping — and it doubles as the guard that
    // stops the test hanging if the wait is ever implemented.
    let attempt = tokio::time::timeout(
        std::time::Duration::from_millis(500),
        svc.resolve_pending_compaction_for_tests(
            session_id,
            &mut ctx,
            BudgetPhase::TurnStart,
            72.0,
            &Some(cb),
        ),
    )
    .await;

    assert!(
        log.lock()
            .unwrap()
            .iter()
            .any(|e| matches!(e, ProgressEvent::Compacting { .. })),
        "a turn about to answer against a context queued for replacement must say so"
    );
    if let Ok(state) = attempt {
        assert!(
            matches!(state, PendingState::StillRunning),
            "the summariser has not landed, so the visit reports it and moves on"
        );
        assert_eq!(
            svc.pending_snapshot_len_for_tests(session_id),
            Some(sentinel),
            "the running summariser keeps its slot — no second one is started"
        );
    }
}

/// The ceiling, same shape and same caveat: at 80% the mid-loop visit announces
/// the in-flight summary. Past the back-pressure ceiling this notice is the only
/// signal the user gets, so it is worth pinning on its own.
#[tokio::test]
async fn a_ceiling_visit_announces_a_running_gate_summary() {
    let svc = service().await;
    let (session_id, mut ctx) = consolidation_fixture();
    let sentinel = ctx.messages.len() + 5;
    svc.insert_pending_compaction_for_tests(
        session_id,
        running_gate_pending_for_tests(&ctx, sentinel, None),
    );

    let (cb, log) = progress_log();
    // The notice is emitted synchronously, before any await, so this budget only
    // has to cover the visit's own bookkeeping — and it doubles as the guard that
    // stops the test hanging if the wait is ever implemented.
    let attempt = tokio::time::timeout(
        std::time::Duration::from_millis(500),
        svc.resolve_pending_compaction_for_tests(
            session_id,
            &mut ctx,
            BudgetPhase::MidLoop,
            80.0,
            &Some(cb),
        ),
    )
    .await;

    assert!(
        log.lock()
            .unwrap()
            .iter()
            .any(|e| matches!(e, ProgressEvent::Compacting { .. })),
        "past the back-pressure ceiling the notice is the only signal the user gets"
    );
    if let Ok(state) = attempt {
        assert!(matches!(state, PendingState::StillRunning));
        assert_eq!(
            svc.pending_snapshot_len_for_tests(session_id),
            Some(sentinel),
            "the running summariser keeps its slot"
        );
    }
}

/// The negative control for both tests above: the same fixture and the same
/// helper, differing only in fill level, DOES block once the context reaches the
/// back-pressure ceiling. Without it "does not block" would be unfalsifiable —
/// a visit that never waits anywhere would pass the two tests above.
///
/// Pinned on the IDLE path on purpose: that is the path that actually awaits
/// (`idle_done`), and its pending has no join handle. A gate-spawned pending is
/// NOT awaited here — see the note on `must_wait` in `resolve_pending_compaction`
/// for the known gap, which this file reports rather than silently blesses.
#[tokio::test]
async fn an_idle_merge_at_the_ceiling_does_block() {
    let svc = service().await;
    let (session_id, mut ctx) = consolidation_fixture();
    svc.insert_pending_compaction_for_tests(
        session_id,
        running_idle_pending_for_tests(&ctx, ctx.messages.len(), 0, None),
    );

    let blocked = tokio::time::timeout(
        std::time::Duration::from_millis(150),
        svc.resolve_pending_compaction_for_tests(
            session_id,
            &mut ctx,
            BudgetPhase::MidLoop,
            80.0,
            &None,
        ),
    )
    .await;
    assert!(
        blocked.is_err(),
        "at 80% the mid-loop visit waits for the summariser rather than growing a \
         window already past the point where the next call fits"
    );
}

/// The OTHER half of the announcement sits below the guard too (#1023).
///
/// `compaction_background_test.rs::the_announcement_sits_below_the_guard`
/// already pins the WARN half. That test is why this looked done: #1686 moved
/// `gate_warn_message` below the guard and left the second emit — the structured
/// `gate_fire` record — above it, so a session at 70% with a summariser in
/// flight still wrote a `gate_fire` for every tool iteration it re-entered the
/// gate on (session `a58b8714`: 128 announcements for one summariser). A
/// duplicate in the event stream is as wrong as one in the log, and no existing
/// test looked at it.
///
/// So: the event is below the guard, the WARN is below the guard, and the
/// announcement text lives in exactly one place — `gate_warn_message` — rather
/// than being re-typed into a second WARN.
#[test]
fn the_gate_fire_event_sits_below_the_guard_too() {
    const COMPACTION: &str = include_str!("../brain/agent/service/compaction.rs");

    let guard = COMPACTION
        .find("if !gate_announces_compaction(&pending_state) {")
        .expect("the in-flight guard must exist");
    let warn = COMPACTION
        .find("tracing::warn!(\"{}\", gate_warn_message(usage_pct));")
        .expect("the gate WARN must be emitted through `gate_warn_message`");
    let event = COMPACTION
        .find("CompactionEventStage::GateFire")
        .expect("the structured gate event must exist");

    assert!(
        guard < warn,
        "the gate WARN must come after the guard, or every re-entry re-announces"
    );
    assert!(
        guard < event,
        "the gate_fire event must come after the guard, for the same reason"
    );
    // The duplicate also needed a WARN carrying the text inline, so: no WARN
    // call site may contain it. `gate_warn_message` legitimately holds those
    // words, which is why this walks the WARNs rather than asking whether the
    // phrase appears in the file at all.
    //
    // The tail is taken with `chars().take(..)`, never a byte slice: this file
    // is full of em dashes, so an offset landing inside a multi-byte character
    // PANICS instead of asserting — a test that dies on an unrelated edit is
    // worse than no test.
    const PHRASE: &str = "triggering LLM compaction";
    for (at, _) in COMPACTION.match_indices("tracing::warn!(") {
        let call: String = COMPACTION[at..].chars().take(300).collect();
        assert!(
            !call.contains(PHRASE),
            "the WARN at byte {at} inlines the announcement text — route it through \
             `gate_warn_message` below the guard instead"
        );
    }
    assert_eq!(
        COMPACTION.matches("gate_warn_message(usage_pct)").count(),
        1,
        "exactly one emit site may announce the gate"
    );
}

// ---------------------------------------------------------------------------
// #1029: the ready path must not consume the document before it knows it can
// apply it
// ---------------------------------------------------------------------------

/// A failed window load must leave the finished document IN the completion slot.
///
/// Before #1029 the ready path took the document out of `idle_completion`
/// first and only then loaded the window. When the load failed it reinserted a
/// pending whose slot was already empty — and `is_still_running` reads a
/// slot-less, handle-less pending as `true` while `idle_result_ready` reads
/// `false`. The session then reported "summariser still running" forever, with
/// no result that could ever arrive, and `maybe_spawn_idle_consolidation` bailed
/// on `contains_key` — so no further consolidation for that session, ever.
///
/// The failure is injected deterministically: dropping the `messages` table
/// makes `load_compaction_window` return `Err`, while `current_boundary_rowid`
/// degrades to `None` on the same error, which matches a spawn anchor of `None`
/// and lets the visit reach the load.
#[tokio::test]
async fn a_failed_window_load_does_not_consume_the_idle_document() {
    let db = Database::connect_in_memory().await.unwrap();
    db.run_migrations().await.unwrap();
    let svc = AgentService::new_for_test(
        Arc::new(MockProvider),
        ServiceContext::new(db.pool().clone()),
    )
    .await;

    let (session_id, ctx) = consolidation_fixture();
    svc.insert_pending_compaction_for_tests(
        session_id,
        finished_idle_pending_for_tests(&ctx, idle_document(), 0, None),
    );

    db.pool()
        .get()
        .await
        .unwrap()
        .interact(|conn| conn.execute_batch("DROP TABLE messages"))
        .await
        .unwrap()
        .unwrap();

    svc.on_idle_compaction_ready_for_tests(session_id).await;

    let (running, ready) = svc
        .pending_readiness_for_tests(session_id)
        .expect("the failed visit must reinsert the pending, not drop it");

    assert!(
        !running,
        "a reinserted pending whose document is still in the slot is NOT running — \
         the pre-#1029 order made this report `true` for the process lifetime"
    );
    assert!(
        ready,
        "a failed window load must not consume the document: it has to stay in the \
         completion slot so the next visit can apply it"
    );
}
