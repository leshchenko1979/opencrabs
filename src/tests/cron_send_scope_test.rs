//! A scheduled job sends where it was configured to, or nowhere.
//!
//! A cron turn has no channel origin, so the proactive send path took its
//! destination from the tool input, and a job's turn picks that input up from
//! whatever it reads. On 2026-08-21 a memory note from two weeks earlier
//! carried a chat id and thread id under the heading "CONTINUE THIS TASK", and
//! a job posted its report into that group: one it was never configured for,
//! whose members had asked for nothing.

use crate::cron::send_scope::{
    may_send_to, permission, with_send_target, PermittedTarget, SendPermission,
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
    use crate::cron::send_scope::{resolve_cron_session_scope, PermittedTarget};
    use crate::db::models::Session;
    use crate::db::Database;
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
        Some(vec![PermittedTarget {
            channel: "telegram",
            target_id: "-100555444".to_string(),
        }])
    );

    // 3. Channel == 'cron' but unknown job fails closed to Some([])
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
    assert_eq!(
        resolve_cron_session_scope(&pool, Some(&unknown_cron_session), "cron").await,
        Some(vec![])
    );
}
