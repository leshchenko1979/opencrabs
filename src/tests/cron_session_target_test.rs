//! The `deliver_to` session grammar resolves a target session (fork #144).
//!
//! Cron jobs can now deliver results into a session's notify queue via
//! `session:<uuid|prefix>` — the target is resolved through the shared
//! session-id resolver (`crate::cli::session_resolve`), so the prefix rules
//! live in ONE place and are pinned by that resolver's own suite
//! (`cli_session_id_prefix_test.rs`). What this file adds on top is the
//! cron-specific surface: the `session:` / `oc://session/` grammar, the
//! delivery-target bake step, and — against a real in-memory DB — the
//! job-scoped archived-aware tier (#332).

use crate::cli::session_resolve::resolve_session_id;
use uuid::Uuid;

fn session_with_id(id: Uuid) -> crate::db::models::Session {
    crate::db::models::Session {
        id,
        title: None,
        model: None,
        provider_name: None,
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
        archived_at: None,
        token_count: 0,
        total_cost: 0.0,
        working_directory: None,
        auto_title_attempted: false,
        project_id: None,
    }
}

/// The resolver is row-count-based: two rows sharing a prefix are ambiguous
/// even when the rows carry the SAME id — `Err`, matching the shared
/// resolver's contract verbatim (row count, not distinct-id count). This is
/// the one prefix rule the resolver's own suite does NOT pin (it proves
/// ambiguity with two DISTINCT ids), which is why it stays here rather than
/// moving to `cli_session_id_prefix_test.rs`.
#[test]
fn duplicate_rows_are_ambiguous() {
    let id = Uuid::new_v4();
    let sessions = vec![session_with_id(id), session_with_id(id)];
    let prefix = id.to_string()[..8].to_string();
    assert!(resolve_session_id(&sessions, &prefix).is_err());
}

#[test]
fn session_target_recognition_and_extraction() {
    use crate::channels::target_resolver::{extract_session_target, is_session_target};

    let full_uuid = "12345678-1234-1234-1234-123456789abc";
    let url_target = format!("oc://session/{full_uuid}");
    let legacy_target = format!("session:{full_uuid}");
    let channel_target = "telegram:123456:78";
    let url_channel = "oc://telegram/123456/78";

    assert!(is_session_target(&url_target));
    assert!(is_session_target(&legacy_target));
    assert!(!is_session_target(channel_target));
    assert!(!is_session_target(url_channel));

    assert_eq!(extract_session_target(&url_target), Some(full_uuid));
    assert_eq!(extract_session_target(&legacy_target), Some(full_uuid));
    assert_eq!(extract_session_target(channel_target), None);
    assert_eq!(extract_session_target(url_channel), None);
}

#[tokio::test]
async fn bake_delivery_target_bakes_session_target_to_session_uuid() {
    use crate::brain::tools::ToolExecutionContext;
    use crate::brain::tools::cron_manage::bake_delivery_target;

    let full_uuid = "12345678-1234-1234-1234-123456789abc";
    let url_target = format!("oc://session/{full_uuid}");
    let legacy_target = format!("session:{full_uuid}");
    let ctx = ToolExecutionContext::new(Uuid::new_v4());

    let baked_url = bake_delivery_target(&url_target, &ctx).await.unwrap();
    let baked_legacy = bake_delivery_target(&legacy_target, &ctx).await.unwrap();

    assert_eq!(baked_url, format!("session:{full_uuid}"));
    assert_eq!(baked_legacy, format!("session:{full_uuid}"));
}

#[test]
fn resolved_target_deliver_to_session_format() {
    use crate::channels::target_resolver::{ResolvedTarget, TargetDestination};

    let id = Uuid::new_v4();
    let rt = ResolvedTarget {
        session: Some(id),
        destination: TargetDestination::Session(id),
    };

    assert_eq!(rt.deliver_to(), format!("session:{id}"));
}

// ---------------------------------------------------------------------------
// #332 — the job-scoped DB tier (`resolve_job_session_target`).
//
// The pure core above is policy-free; the DB tier is where the ARCHIVED policy
// lives. Before #332 the three job-scoped call sites disagreed on it:
// `bake_delivery_target` and `resolve_or_create_cron_session` listed live rows
// only, so an archived target silently stopped resolving and delivery
// collapsed to nowhere with a misleading "no session matches" reason. These
// tests pin the shared helper's contract against a real in-memory DB with the
// real migrations.
// ---------------------------------------------------------------------------

use crate::cli::session_resolve::resolve_job_session_target;
use crate::db::Database;

async fn test_db() -> crate::db::Pool {
    let db = Database::connect_in_memory().await.unwrap();
    db.run_migrations().await.unwrap();
    db.pool().clone()
}

fn titled_session(id: Uuid, title: &str) -> crate::db::models::Session {
    let mut s = session_with_id(id);
    s.title = Some(title.to_string());
    s
}

/// A live session resolves by prefix.
#[tokio::test]
async fn job_target_resolves_live_session_by_prefix() {
    let pool = test_db().await;
    let repo = crate::db::repository::SessionRepository::new(pool.clone());
    let id = Uuid::new_v4();
    repo.create(&titled_session(id, "live")).await.unwrap();

    let target = format!("session:{}", &id.to_string()[..8]);
    assert_eq!(resolve_job_session_target(&pool, &target).await, Some(id));
}

/// The #332 regression itself: an ARCHIVED session still resolves. This is the
/// whole point of the helper — a job's target outlives the session's lifecycle
/// state, and the old `include_archived: false` listings lost it.
#[tokio::test]
async fn job_target_resolves_archived_session_by_prefix() {
    let pool = test_db().await;
    let repo = crate::db::repository::SessionRepository::new(pool.clone());
    let id = Uuid::new_v4();
    repo.create(&titled_session(id, "archived")).await.unwrap();
    repo.archive(id).await.unwrap();

    // Preconditions — without these the test would also pass against the OLD
    // live-only listing and prove nothing.
    assert!(
        repo.list_archived().await.unwrap().iter().any(|s| s.id == id),
        "precondition: the row must really be archived"
    );
    assert!(
        !repo
            .list(crate::db::repository::SessionListOptions {
                include_archived: false,
                ..Default::default()
            })
            .await
            .unwrap()
            .iter()
            .any(|s| s.id == id),
        "precondition: an archived row must be absent from a live-only listing"
    );

    let target = format!("session:{}", &id.to_string()[..8]);
    assert_eq!(resolve_job_session_target(&pool, &target).await, Some(id));
}

/// Full UUIDs pass through with no rows in the table — fast-path parity with
/// the pure core.
#[tokio::test]
async fn job_target_full_uuid_passthrough_without_rows() {
    let pool = test_db().await;
    let id = Uuid::new_v4();
    assert_eq!(
        resolve_job_session_target(&pool, &format!("session:{id}")).await,
        Some(id)
    );
}

/// The `oc://session/<id>` URL form is accepted too — one grammar for the whole
/// `deliver_to` surface, extracted inside the helper.
#[tokio::test]
async fn job_target_accepts_oc_url_form() {
    let pool = test_db().await;
    let id = Uuid::new_v4();
    assert_eq!(
        resolve_job_session_target(&pool, &format!("oc://session/{id}")).await,
        Some(id)
    );
}

/// A channel target is not a session target: `None`, no panic, and no
/// accidental prefix match against the session table.
#[tokio::test]
async fn job_target_rejects_non_session_grammar() {
    let pool = test_db().await;
    assert_eq!(
        resolve_job_session_target(&pool, "telegram:123456:78").await,
        None
    );
    assert_eq!(resolve_job_session_target(&pool, "oc://telegram/123/78").await, None);
}

/// A prefix matching no session is `None` (the caller owns the loud failure).
#[tokio::test]
async fn job_target_unknown_prefix_rejects() {
    let pool = test_db().await;
    let repo = crate::db::repository::SessionRepository::new(pool.clone());
    repo.create(&titled_session(Uuid::new_v4(), "someone"))
        .await
        .unwrap();
    assert_eq!(
        resolve_job_session_target(&pool, "session:zzzzzzzz").await,
        None
    );
}

// ---------------------------------------------------------------------------
// #434 — the delivery path hands the resolver an ALREADY-SPLIT bare id.
//
// `cron::scheduler` splits `deliver_to` on ':' and passes `target_id` on, so
// demanding the `session:` prefix here rejected every legacy `session:<uuid>`
// job at fire time ("no session matches") while the identical target resolved
// fine through every other caller. These pin the bare grammar; both fail
// against d32600eda.
// ---------------------------------------------------------------------------

/// A bare full uuid — the exact shape the scheduler hands over — resolves with
/// no rows in the table (same fast path the prefixed form gets).
#[tokio::test]
async fn job_target_resolves_bare_full_uuid() {
    let pool = test_db().await;
    let id = Uuid::new_v4();
    assert_eq!(
        resolve_job_session_target(&pool, &id.to_string()).await,
        Some(id)
    );
}

/// A bare 8-char prefix — `session:` already stripped by the caller — resolves
/// through the DB tier.
#[tokio::test]
async fn job_target_resolves_bare_prefix() {
    let pool = test_db().await;
    let repo = crate::db::repository::SessionRepository::new(pool.clone());
    let id = Uuid::new_v4();
    repo.create(&titled_session(id, "bare-prefix"))
        .await
        .unwrap();

    assert_eq!(
        resolve_job_session_target(&pool, &id.to_string()[..8]).await,
        Some(id)
    );
}

/// An empty id is not a wildcard: `resolve_one_by_prefix` matches EVERY row
/// against the empty prefix, so a blank target must stop before the listing
/// rather than resolving to whichever session happens to be alone in the table.
#[tokio::test]
async fn job_target_empty_id_is_not_a_wildcard() {
    let pool = test_db().await;
    let repo = crate::db::repository::SessionRepository::new(pool.clone());
    repo.create(&titled_session(Uuid::new_v4(), "only-row"))
        .await
        .unwrap();

    assert_eq!(resolve_job_session_target(&pool, "session:").await, None);
    assert_eq!(resolve_job_session_target(&pool, "").await, None);
}
