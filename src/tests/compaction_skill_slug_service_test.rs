//! Service-level regression tests for #179 — one key space for active skills.
//!
//! The unit tests in `compaction_skill_slug_test.rs` pin the matcher and the
//! normaliser. These drive the real `AgentService` ingestion paths, because
//! the defect lived in the seam between them: the slash-command path passed
//! `Skill::slash_name` while the compaction manifest passed the bare slug, so
//! the same skill could be keyed two ways — and a discard of one form left
//! the other behind.

use crate::brain::agent::service::AgentService;
use crate::brain::provider::Provider;
use crate::db::Database;
use crate::db::repository::session_skills::SessionSkillsRepository;
use crate::services::ServiceContext;
use crate::tests::agent_service_mocks::MockProvider;
use std::sync::Arc;
use uuid::Uuid;

/// Helper: create an in-memory AgentService for testing.
async fn make_service() -> AgentService {
    let db = Database::connect_in_memory().await.unwrap();
    db.run_migrations().await.unwrap();
    let pool = db.pool().clone();
    let context = ServiceContext::new(pool);
    let provider: Arc<dyn Provider> = Arc::new(MockProvider);
    AgentService::new_for_test(provider, context).await
}

/// Registering via the slash-command spelling and the manifest spelling
/// yields ONE entry, not two. Pre-fix the set held `canarya` and `/canarya`
/// side by side, and only one of them was ever matched for re-injection.
#[tokio::test]
async fn bare_and_slashed_registration_share_one_key() {
    let svc = make_service().await;
    let sid = Uuid::new_v4();

    svc.register_active_skill(sid, "/canarya"); // slash-command path
    svc.register_active_skill(sid, "canarya"); // manifest path

    let skills = svc.active_skills_for_session(sid);
    assert_eq!(
        skills.len(),
        1,
        "both ingestion paths must key the same entry, got: {skills:?}"
    );
    assert!(skills.contains("canarya"));
    assert!(!skills.contains("/canarya"));
}

/// A discard written in either spelling removes the single bare-keyed entry.
#[tokio::test]
async fn discard_in_either_spelling_removes_the_entry() {
    let svc = make_service().await;

    let sid_a = Uuid::new_v4();
    svc.register_active_skill(sid_a, "canarya");
    svc.unregister_active_skill(sid_a, "/canarya");
    assert!(
        svc.active_skills_for_session(sid_a).is_empty(),
        "slash-spelled discard must clear the bare-keyed entry"
    );

    let sid_b = Uuid::new_v4();
    svc.register_active_skill(sid_b, "/canarya");
    svc.unregister_active_skill(sid_b, "canarya");
    assert!(
        svc.active_skills_for_session(sid_b).is_empty(),
        "bare-spelled discard must clear the slash-registered entry"
    );
}

/// The compaction manifest's own sequence — discard first, then register —
/// leaves exactly the skills the manifest kept, regardless of spelling.
#[tokio::test]
async fn manifest_discard_then_register_leaves_only_the_kept_skill() {
    let svc = make_service().await;
    let sid = Uuid::new_v4();

    // Pre-existing state, as a slash invocation would have produced it.
    svc.register_active_skill(sid, "/canarya");
    svc.register_active_skill(sid, "miidas");

    // The manifest: drop canarya (documented bare form), keep miidas.
    svc.unregister_active_skill(sid, "canarya");
    svc.register_active_skill(sid, "miidas");

    let skills = svc.active_skills_for_session(sid);
    assert_eq!(
        skills.len(),
        1,
        "only the kept skill should remain: {skills:?}"
    );
    assert!(skills.contains("miidas"));
    assert!(!skills.contains("canarya"));
}

/// A stale row persisted before slugs were canonicalised (the slash-command
/// path wrote `/foo`) is cleared by a discard of the bare slug — so the
/// compaction stamp's union cannot keep counting a skill the model just
/// discarded, and no migration is needed.
#[tokio::test]
async fn legacy_slashed_db_row_is_cleared_on_discard() {
    let db = Database::connect_in_memory().await.unwrap();
    db.run_migrations().await.unwrap();
    let repo = SessionSkillsRepository::new(db.pool().clone());

    let sid = Uuid::new_v4();
    let other = Uuid::new_v4();

    // Legacy row (slashed) + an unrelated session's row that must survive.
    repo.record(sid, "/canarya", 0).await.unwrap();
    repo.record(other, "/canarya", 0).await.unwrap();

    repo.delete_skill(sid, "canarya").await.unwrap();

    let rows = repo.all().await.unwrap();
    assert!(
        !rows.iter().any(|(s, _, _, _)| *s == sid),
        "the legacy slashed row must be cleared, got: {rows:?}"
    );
    assert!(
        rows.iter().any(|(s, _, _, _)| *s == other),
        "another session's row must be untouched, got: {rows:?}"
    );

    // Idempotent: a second discard on an already-clean session is a no-op.
    repo.delete_skill(sid, "canarya").await.unwrap();
}
