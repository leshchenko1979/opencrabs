//! A scheduled job sends where it was configured to, or nowhere.
//!
//! A cron turn has no channel origin, so the proactive send path took its
//! destination from the tool input, and a job's turn picks that input up from
//! whatever it reads. On 2026-08-21 a memory note from two weeks earlier
//! carried a chat id and thread id under the heading "CONTINUE THIS TASK", and
//! a job posted its report into that group: one it was never configured for,
//! whose members had asked for nothing.

use crate::cron::send_scope::{
    NoTargetReason, PermittedTarget, SendPermission, SendScope, may_send_to, permission,
    refusal_for, with_send_scope, with_send_target,
};

const CONFIGURED: i64 = -1004252074515;
const SOMEWHERE_ELSE: i64 = -1004428873948;

#[tokio::test]
async fn outside_a_job_nothing_is_restricted() {
    // The rule exists to stop a job reaching chats it was never given, not to
    // police an ordinary reply in a chat the user is talking in.
    assert_eq!(permission(), SendPermission::Unscoped);
    assert!(may_send_to(SOMEWHERE_ELSE));
}

#[tokio::test]
async fn a_job_may_send_to_the_chat_it_was_given() {
    with_send_target(Some(CONFIGURED), async {
        assert_eq!(
            permission(),
            SendPermission::Permitted(vec![PermittedTarget {
                channel: "telegram",
                target_id: CONFIGURED.to_string(),
            }])
        );
        assert!(may_send_to(CONFIGURED));
    })
    .await;
}

#[tokio::test]
async fn a_job_may_not_send_to_any_other_chat() {
    // The exact leak: the destination came from a recalled memory, not from
    // the job's configuration.
    with_send_target(Some(CONFIGURED), async {
        assert!(
            !may_send_to(SOMEWHERE_ELSE),
            "a chat id found in memory is not permission to post there"
        );
    })
    .await;
}

#[tokio::test]
async fn a_job_without_a_target_sends_nowhere() {
    // Not the owner's DM, not a guess, nowhere. Its output stays in its session.
    with_send_target(None, async {
        assert_eq!(permission(), SendPermission::Nowhere);
        assert!(!may_send_to(CONFIGURED));
        assert!(!may_send_to(SOMEWHERE_ELSE));
    })
    .await;
}

#[tokio::test]
async fn the_scope_does_not_outlive_the_job() {
    // Task-local, so a sibling job on the scheduler is unaffected and the
    // restriction is gone once the turn ends.
    with_send_target(Some(CONFIGURED), async {
        assert!(!may_send_to(SOMEWHERE_ELSE));
    })
    .await;
    assert_eq!(permission(), SendPermission::Unscoped);
    assert!(may_send_to(SOMEWHERE_ELSE));
}

#[tokio::test]
async fn the_refusal_names_what_to_change() {
    with_send_target(Some(CONFIGURED), async {
        let msg = crate::cron::send_scope::refusal(SOMEWHERE_ELSE);
        assert!(msg.contains("deliver_to"), "got: {msg}");
        assert!(msg.contains(&CONFIGURED.to_string()), "got: {msg}");
    })
    .await;
}

#[test]
fn test_parse_permitted_targets() {
    use crate::cron::send_scope::parse_permitted_targets;

    assert_eq!(parse_permitted_targets(None), None);
    assert_eq!(parse_permitted_targets(Some("")), Some(vec![]));
    assert_eq!(
        parse_permitted_targets(Some("oc://session/12345678-1234-1234-1234-123456789abc")),
        Some(vec![])
    );
    assert_eq!(
        parse_permitted_targets(Some("telegram:-100123456, discord:987654321")),
        Some(vec![
            PermittedTarget {
                channel: "telegram",
                target_id: "-100123456".to_string(),
            },
            PermittedTarget {
                channel: "discord",
                target_id: "987654321".to_string(),
            }
        ])
    );
}

#[test]
fn test_cron_job_scope_is_fail_closed_for_a_targetless_job() {
    use crate::cron::send_scope::{cron_job_scope, parse_permitted_targets};

    // The parser keeps the two meanings apart — absent field vs. present but
    // channel-less. #317 is the reason the cron entry point may not consume it
    // directly: the parser's `None` reads downstream as "not a cron turn".
    assert_eq!(parse_permitted_targets(None), None);

    // Every way a job can name no channel collapses onto the SAME scope:
    // empty, and never `None`.
    assert_eq!(cron_job_scope(None), vec![]);
    assert_eq!(cron_job_scope(Some("")), vec![]);
    assert_eq!(
        cron_job_scope(Some("oc://session/12345678-1234-1234-1234-123456789abc")),
        vec![]
    );

    // A configured channel target still comes through untouched.
    assert_eq!(
        cron_job_scope(Some("telegram:-100123456")),
        vec![PermittedTarget {
            channel: "telegram",
            target_id: "-100123456".to_string(),
        }]
    );
}

#[test]
fn test_extract_cron_job_id_from_session_title() {
    use crate::cron::send_scope::extract_cron_job_id_from_session_title;
    use uuid::Uuid;

    let id = Uuid::new_v4();
    let title = format!("Cron: hourly report [cron-job:{id}]");
    assert_eq!(extract_cron_job_id_from_session_title(&title), Some(id));

    assert_eq!(
        extract_cron_job_id_from_session_title("General Chat [chat:12345]"),
        None
    );
    assert_eq!(
        extract_cron_job_id_from_session_title("plain session"),
        None
    );
}

#[tokio::test]
async fn test_resolve_cron_session_scope() {
    use crate::cron::send_scope::{PermittedTarget, resolve_cron_session_scope};
    use crate::db::Database;
    use crate::db::models::Session;
    use uuid::Uuid;

    let db = Database::connect_in_memory().await.unwrap();
    db.run_migrations().await.unwrap();
    let pool = db.pool().clone();
    let job_id = Uuid::new_v4();

    // 1. Non-cron session returns None (Unscoped)
    let plain_session = Session {
        id: Uuid::new_v4(),
        title: Some("User chat".to_string()),
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
    };
    assert_eq!(
        resolve_cron_session_scope(&pool, Some(&plain_session), "telegram").await,
        None
    );

    // 2. Cron session title with existing job in DB
    let cron_repo = crate::db::CronJobRepository::new(pool.clone());
    let job = crate::db::models::CronJob {
        id: job_id,
        name: "Test Job".to_string(),
        cron_expr: "0 * * * *".to_string(),
        timezone: "UTC".to_string(),
        prompt: "echo".to_string(),
        provider: None,
        model: None,
        thinking: "low".to_string(),
        auto_approve: true,
        deliver_to: Some("telegram:-100555444".to_string()),
        deliver_api_key: None,
        enabled: true,
        last_run_at: None,
        next_run_at: None,
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
        profile_name: None,
        trigger_cmd: None,
        trigger_on: None,
        set_goal: false,
        goal_template: None,
    };
    cron_repo.insert(&job).await.unwrap();

    let cron_session = Session {
        id: Uuid::new_v4(),
        title: Some(format!("Cron: Test Job [cron-job:{job_id}]")),
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
    };
    assert_eq!(
        resolve_cron_session_scope(&pool, Some(&cron_session), "cron").await,
        Some(SendScope::Permitted(vec![PermittedTarget {
            channel: "telegram",
            target_id: "-100555444".to_string(),
        }]))
    );

    // 3. Channel == 'cron' but unknown job fails closed to Nowhere, and says
    //    WHY it cannot name a target rather than inventing one.
    let unknown_cron_session = Session {
        id: Uuid::new_v4(),
        title: Some("Old cron session [cron-job:00000000-0000-0000-0000-000000000000]".to_string()),
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
    };
    // The job row is gone, so WHAT it declared is unknowable — not "it
    // declared nothing". Telling those apart is the point of #332's reason.
    assert_eq!(
        resolve_cron_session_scope(&pool, Some(&unknown_cron_session), "cron").await,
        Some(SendScope::Nowhere(NoTargetReason::JobUnknown))
    );
}

#[tokio::test]
async fn test_resolve_cron_session_scope_targetless_job_is_nowhere() {
    // #317: the resume path and the scheduler path must agree. Both now derive
    // the scope through `cron_job_scope`, so a job with no `deliver_to` resumes
    // Nowhere rather than Unscoped.
    use crate::cron::send_scope::resolve_cron_session_scope;
    use crate::db::Database;
    use crate::db::models::Session;
    use uuid::Uuid;

    let db = Database::connect_in_memory().await.unwrap();
    db.run_migrations().await.unwrap();
    let pool = db.pool().clone();
    let job_id = Uuid::new_v4();

    let job = crate::db::models::CronJob {
        id: job_id,
        name: "Targetless Job".to_string(),
        cron_expr: "0 * * * *".to_string(),
        timezone: "UTC".to_string(),
        prompt: "echo".to_string(),
        provider: None,
        model: None,
        thinking: "low".to_string(),
        auto_approve: true,
        deliver_to: None,
        deliver_api_key: None,
        enabled: true,
        last_run_at: None,
        next_run_at: None,
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
        profile_name: None,
        trigger_cmd: None,
        trigger_on: None,
        set_goal: false,
        goal_template: None,
    };
    crate::db::CronJobRepository::new(pool.clone())
        .insert(&job)
        .await
        .unwrap();

    let cron_session = Session {
        id: Uuid::new_v4(),
        title: Some(format!("Cron: Targetless Job [cron-job:{job_id}]")),
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
    };

    assert_eq!(
        resolve_cron_session_scope(&pool, Some(&cron_session), "cron").await,
        Some(SendScope::Nowhere(NoTargetReason::Undeclared)),
        "a job with no deliver_to must resume Nowhere, not Unscoped"
    );
}

// ---------------------------------------------------------------------------
// #332 / D1 — a `session:` target scopes to the channel its session is BOUND to.
//
// `parse_permitted_targets` recognises only the concrete channel prefixes, so a
// job whose `deliver_to` is `session:<uuid>` produced an EMPTY permitted set.
// The turn was scoped to Nowhere while the job had a real destination — the
// session's own bound chat — and every sibling send into that chat was refused
// with a reason claiming the job declared no `deliver_to` at all.
//
// These tests drive the binding-aware expansion against a real DB: the helper
// in isolation, and the full `resolve_cron_session_scope` resume path.
// ---------------------------------------------------------------------------

/// Seed one session row plus (optionally) a channel binding for it.
async fn seed_bound_session(
    pool: &crate::db::Pool,
    channel: Option<&str>,
    chat_id: &str,
) -> uuid::Uuid {
    use crate::db::models::Session;
    use crate::db::repository::SessionBindingRepository;

    let id = uuid::Uuid::new_v4();
    let session = Session {
        id,
        title: Some("bound target".to_string()),
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
    };
    crate::db::repository::SessionRepository::new(pool.clone())
        .create(&session)
        .await
        .unwrap();

    if let Some(channel) = channel {
        SessionBindingRepository::new(pool.clone())
            .upsert(
                id.to_string(),
                channel,
                chat_id,
                None,
                crate::db::repository::session_binding::BindingOrigin::Text,
            )
            .await
            .unwrap();
    }
    id
}

async fn scope_test_pool() -> crate::db::Pool {
    let db = crate::db::Database::connect_in_memory().await.unwrap();
    db.run_migrations().await.unwrap();
    db.pool().clone()
}

/// The D1 fix itself: a job pointed at a session bound to a chat permits THAT
/// chat — not Nowhere.
#[tokio::test]
async fn a_session_target_expands_to_the_bound_channel() {
    use crate::cron::send_scope::{cron_job_scope_async, expand_session_targets};

    let pool = scope_test_pool().await;
    let id = seed_bound_session(&pool, Some("telegram"), "-100777888").await;
    let deliver_to = format!("session:{id}");

    assert_eq!(
        expand_session_targets(&pool, Some(&deliver_to)).await,
        vec![PermittedTarget {
            channel: "telegram",
            target_id: "-100777888".to_string(),
        }]
    );
    assert_eq!(
        cron_job_scope_async(&pool, Some(&deliver_to)).await,
        SendScope::Permitted(vec![PermittedTarget {
            channel: "telegram",
            target_id: "-100777888".to_string(),
        }]),
        "the full job scope must permit the bound chat, not collapse to Nowhere"
    );
}

/// The `oc://session/<uuid>` URL form expands identically — one grammar.
#[tokio::test]
async fn a_session_url_target_expands_too() {
    use crate::cron::send_scope::expand_session_targets;

    let pool = scope_test_pool().await;
    let id = seed_bound_session(&pool, Some("discord"), "987654321").await;

    assert_eq!(
        expand_session_targets(&pool, Some(&format!("oc://session/{id}"))).await,
        vec![PermittedTarget {
            channel: "discord",
            target_id: "987654321".to_string(),
        }]
    );
}

/// A prefix works the same as a full id — the helper owns the matching rules.
#[tokio::test]
async fn a_session_prefix_expands_to_the_bound_channel() {
    use crate::cron::send_scope::expand_session_targets;

    let pool = scope_test_pool().await;
    let id = seed_bound_session(&pool, Some("telegram"), "-100123456").await;
    let short = format!("session:{}", &id.to_string()[..8]);

    assert_eq!(
        expand_session_targets(&pool, Some(&short)).await,
        vec![PermittedTarget {
            channel: "telegram",
            target_id: "-100123456".to_string(),
        }]
    );
}

/// A session with NO binding row fails closed: nothing is permitted, so the
/// turn stays at Nowhere rather than inheriting a destination it cannot prove.
#[tokio::test]
async fn an_unbound_session_target_permits_nothing() {
    use crate::cron::send_scope::{cron_job_scope_async, expand_session_targets};

    let pool = scope_test_pool().await;
    let id = seed_bound_session(&pool, None, "").await;
    let deliver_to = format!("session:{id}");

    assert!(expand_session_targets(&pool, Some(&deliver_to)).await.is_empty());
    assert_eq!(
        cron_job_scope_async(&pool, Some(&deliver_to)).await,
        SendScope::Nowhere(NoTargetReason::Unresolvable { declared: deliver_to.clone() }),
        "the job DID declare a destination — it just cannot be resolved"
    );
}

/// An ARCHIVED session yields Nowhere — the acceptance-criterion case. Archived
/// rows carry no binding in the live DB (0 of 116 measured), so the expansion
/// contributes nothing. Pinned as expected behaviour: an archived session is
/// not a route, and the reason reads Unresolvable — the job declared a target
/// that no channel can be resolved from, which is NOT the same fact as a job
/// that declared nothing.
#[tokio::test]
async fn an_archived_session_target_permits_nothing() {
    use crate::cron::send_scope::{cron_job_scope_async, expand_session_targets};

    let pool = scope_test_pool().await;
    let id = seed_bound_session(&pool, None, "").await;
    crate::db::repository::SessionRepository::new(pool.clone())
        .archive(id)
        .await
        .unwrap();

    // Precondition: the row really is archived.
    assert!(
        crate::db::repository::SessionRepository::new(pool.clone())
            .list_archived()
            .await
            .unwrap()
            .iter()
            .any(|s| s.id == id),
        "precondition: the session must really be archived"
    );

    let deliver_to = format!("session:{id}");
    assert!(expand_session_targets(&pool, Some(&deliver_to)).await.is_empty());
    assert_eq!(
        cron_job_scope_async(&pool, Some(&deliver_to)).await,
        SendScope::Nowhere(NoTargetReason::Unresolvable { declared: deliver_to.clone() })
    );
}

/// A binding on a channel outside the scope grammar (`cli`, `cron`, …) has no
/// channel destination to permit — it contributes nothing rather than widening
/// the scope with a channel the send path cannot address.
#[tokio::test]
async fn a_non_channel_binding_permits_nothing() {
    use crate::cron::send_scope::expand_session_targets;

    let pool = scope_test_pool().await;
    let id = seed_bound_session(&pool, Some("cli"), "local").await;

    assert!(
        expand_session_targets(&pool, Some(&format!("session:{id}")))
            .await
            .is_empty()
    );
}

/// Concrete and expanded targets COMPOSE, and a duplicate is collapsed: a job
/// naming both a chat and a session bound to that same chat permits the chat
/// once, so the refusal text reads cleanly.
#[tokio::test]
async fn concrete_and_expanded_targets_compose_without_duplicates() {
    use crate::cron::send_scope::cron_job_scope_async;

    let pool = scope_test_pool().await;
    let id = seed_bound_session(&pool, Some("telegram"), "-100555444").await;
    let deliver_to = format!("telegram:-100555444, session:{id}");

    assert_eq!(
        cron_job_scope_async(&pool, Some(&deliver_to)).await,
        SendScope::Permitted(vec![PermittedTarget {
            channel: "telegram",
            target_id: "-100555444".to_string(),
        }])
    );
}

/// Non-session segments are left to the concrete parser — the expansion must
/// not double-count them, and an HTTP webhook target stays out of the scope.
#[tokio::test]
async fn non_session_segments_are_untouched_by_the_expansion() {
    use crate::cron::send_scope::{cron_job_scope_async, expand_session_targets};

    let pool = scope_test_pool().await;
    let deliver_to = "telegram:-100999111, https://example.test/hook";

    assert!(expand_session_targets(&pool, Some(deliver_to)).await.is_empty());
    assert_eq!(
        cron_job_scope_async(&pool, Some(deliver_to)).await,
        SendScope::Permitted(vec![PermittedTarget {
            channel: "telegram",
            target_id: "-100999111".to_string(),
        }])
    );
}

/// The resume path agrees with the fire path: a cron session whose job targets
/// a bound session resumes Permitted, not Nowhere.
#[tokio::test]
async fn resolve_cron_session_scope_is_binding_aware() {
    use crate::cron::send_scope::resolve_cron_session_scope;
    use crate::db::models::{CronJob, Session};

    let pool = scope_test_pool().await;
    let bound = seed_bound_session(&pool, Some("telegram"), "-100321654").await;
    let job_id = uuid::Uuid::new_v4();

    let job = CronJob {
        id: job_id,
        name: "Session Target Job".to_string(),
        cron_expr: "0 * * * *".to_string(),
        timezone: "UTC".to_string(),
        prompt: "echo".to_string(),
        provider: None,
        model: None,
        thinking: "low".to_string(),
        auto_approve: true,
        deliver_to: Some(format!("session:{bound}")),
        deliver_api_key: None,
        enabled: true,
        last_run_at: None,
        next_run_at: None,
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
        profile_name: None,
        trigger_cmd: None,
        trigger_on: None,
        set_goal: false,
        goal_template: None,
    };
    crate::db::CronJobRepository::new(pool.clone())
        .insert(&job)
        .await
        .unwrap();

    let cron_session = Session {
        id: uuid::Uuid::new_v4(),
        title: Some(format!("Cron: Session Target Job [cron-job:{job_id}]")),
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
    };

    assert_eq!(
        resolve_cron_session_scope(&pool, Some(&cron_session), "cron").await,
        Some(SendScope::Permitted(vec![PermittedTarget {
            channel: "telegram",
            target_id: "-100321654".to_string(),
        }])),
        "a resumed cron turn must be scoped like the firing turn that made it"
    );
}

// ---------------------------------------------------------------------------
// #332 · Step 3 — the Nowhere refusals say WHICH Nowhere
//
// A scope that may reach nothing is not one fact but three, and the old text
// asserted the first in all of them: "this scheduled job has no deliver_to".
// For a job whose `deliver_to` names an unbound session that is FALSE — the
// field is set — and it sent the model looking for a configuration error that
// was not there, while the real cause (a session target reaches a channel only
// through that session's own binding) went unstated.
// ---------------------------------------------------------------------------

/// The absent-field case keeps its original, true text.
#[tokio::test]
async fn a_targetless_job_refusal_says_the_field_is_absent() {
    let scope = Some(SendScope::Nowhere(NoTargetReason::Undeclared));
    with_send_scope(scope, async {
        let msg = refusal_for("telegram", "-100999");
        assert!(msg.contains("has no deliver_to"), "got: {msg}");
        assert!(msg.contains("telegram:-100999"), "got: {msg}");
    })
    .await;
}

/// The declared-but-unresolvable case must NOT claim the field is absent: the
/// job HAS one, and the refusal names it and the real cause.
#[tokio::test]
async fn an_unresolvable_target_refusal_does_not_claim_a_missing_field() {
    let declared = "session:2f0d6a1e-0000-0000-0000-000000000000".to_string();
    let scope = Some(SendScope::Nowhere(NoTargetReason::Unresolvable {
        declared: declared.clone(),
    }));
    with_send_scope(scope, async {
        let msg = refusal_for("telegram", "-100999");
        assert!(
            !msg.contains("has no deliver_to"),
            "the job DOES have a deliver_to; got: {msg}"
        );
        assert!(
            msg.contains(&declared),
            "the refusal must name the declared target; got: {msg}"
        );
        assert!(
            msg.contains("binding"),
            "and explain the real cause; got: {msg}"
        );
    })
    .await;
}

/// An unreadable job row is a third fact again — never "the job declared
/// nothing".
#[tokio::test]
async fn an_unreadable_job_refusal_does_not_claim_a_missing_field() {
    let scope = Some(SendScope::Nowhere(NoTargetReason::JobUnknown));
    with_send_scope(scope, async {
        let msg = refusal_for("telegram", "-100999");
        assert!(!msg.contains("has no deliver_to"), "got: {msg}");
        assert!(msg.contains("could not be read"), "got: {msg}");
    })
    .await;
}

/// The three Nowhere refusals are pairwise distinct, so a reader can tell
/// which case it is holding from the text alone.
#[tokio::test]
async fn the_nowhere_refusals_are_pairwise_distinct() {
    let undeclared = with_send_scope(
        Some(SendScope::Nowhere(NoTargetReason::Undeclared)),
        async { refusal_for("telegram", "-1") },
    )
    .await;
    let unresolvable = with_send_scope(
        Some(SendScope::Nowhere(NoTargetReason::Unresolvable {
            declared: "session:abc".to_string(),
        })),
        async { refusal_for("telegram", "-1") },
    )
    .await;
    let unknown = with_send_scope(Some(SendScope::Nowhere(NoTargetReason::JobUnknown)), async {
        refusal_for("telegram", "-1")
    })
    .await;

    assert_ne!(undeclared, unresolvable);
    assert_ne!(undeclared, unknown);
    assert_ne!(unresolvable, unknown);
}

/// End to end on the real path: a job whose `deliver_to` names an UNBOUND
/// session resolves to the unresolvable reason — not the undeclared one — and
/// the text the model reads says so.
#[tokio::test]
async fn an_unbound_session_job_refuses_with_the_unresolvable_reason() {
    use crate::cron::send_scope::cron_job_scope_async;

    let pool = scope_test_pool().await;
    let id = seed_bound_session(&pool, None, "").await;
    let deliver_to = format!("session:{id}");

    let scope = cron_job_scope_async(&pool, Some(&deliver_to)).await;
    assert_eq!(
        scope,
        SendScope::Nowhere(NoTargetReason::Unresolvable { declared: deliver_to.clone() }),
        "a job that DID declare a destination must not be read as undeclared"
    );

    with_send_scope(Some(scope), async {
        let msg = refusal_for("telegram", "-100999");
        assert!(!msg.contains("has no deliver_to"), "got: {msg}");
        assert!(msg.contains(&deliver_to), "got: {msg}");
    })
    .await;
}
