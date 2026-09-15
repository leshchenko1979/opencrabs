//! Boot-classifier recovery tests (#33, owner-approved design 2026-08-29).
//!
//! The classifier promotes the #1227 wake pass from log-only to recovery:
//! a recently-active session whose topic's LAST persisted message is from a
//! user had its turn interrupted by the restart → resume; bot-last → the
//! turn completed → log only; nothing persisted → unclassifiable → log only.
//!
//! These tests pin the classification contract on an in-memory DB, mirroring
//! the row shapes the classifier depends on: bindings via
//! `SessionBindingRepository::upsert` (INNER-JOINs `sessions`, so a session
//! row is required), bot rows with sender `bot:opencrabs`, user rows with an
//! arbitrary sender id.

use crate::channels::telegram::resume::classify_recently_active;
use crate::config::profile::{home_for_profile, with_profile_home_async};
use crate::db::models::{ChannelMessage, Session, BOT_SENDER_ID};
use crate::db::{ChannelMessageRepository, Database, SessionBindingRepository, SessionRepository};
use crate::tui::plan::{PlanDocument, PlanStatus, PlanTask, TaskStatus, TaskType};
use crate::utils::plan_files::save_plan;
use std::collections::HashSet;
use uuid::Uuid;

async fn in_temp_home<F, T>(f: F) -> T
where
    F: std::future::Future<Output = T>,
{
    let profile = format!("boot-classifier-test-{}", Uuid::new_v4());
    let out = with_profile_home_async(Some(&profile), f).await;
    let home = home_for_profile(Some(&profile));
    let _ = std::fs::remove_dir_all(&home);
    out
}

async fn test_db() -> Database {
    let db = Database::connect_in_memory().await.unwrap();
    db.run_migrations().await.unwrap();
    db
}

async fn bind_session(db: &Database, session: Uuid, chat: &str, thread: Option<i32>) {
    SessionRepository::new(db.pool().clone())
        .create(&Session {
            id: session,
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
        })
        .await
        .unwrap();
    SessionBindingRepository::new(db.pool().clone())
        .upsert(
            session.to_string(),
            "telegram",
            chat,
            thread,
            crate::db::BindingOrigin::Text,
        )
        .await
        .unwrap();
}

async fn store_msg(db: &Database, chat: &str, thread: Option<&str>, sender: &str, text: &str) {
    let mut cm = ChannelMessage::new(
        "telegram".to_string(),
        chat.to_string(),
        None,
        sender.to_string(),
        "sender".to_string(),
        text.to_string(),
        "text".to_string(),
        None,
    );
    cm.thread_id = thread.map(|t| t.to_string());
    ChannelMessageRepository::new(db.pool().clone())
        .insert(&cm)
        .await
        .unwrap();
}

#[tokio::test]
async fn user_last_topic_classifies_interrupted() {
    let db = test_db().await;
    let sid = Uuid::new_v4();
    bind_session(&db, sid, "-100123", Some(249)).await;
    // User spoke, bot never replied — the turn was killed mid-flight.
    store_msg(&db, "-100123", Some("249"), "user:alexey", "check the logs").await;

    let r = classify_recently_active(db.pool().clone(), &HashSet::new()).await;
    assert_eq!(
        r.interrupted.len(),
        1,
        "user-last must classify interrupted"
    );
    assert_eq!(r.interrupted[0].0, sid);
    assert_eq!(r.interrupted[0].1, -100123);
    assert_eq!(r.interrupted[0].2, Some(249));
    assert!(r.completed.is_empty() && r.unclassified.is_empty());
}

#[tokio::test]
async fn bot_last_topic_classifies_completed() {
    let db = test_db().await;
    let sid = Uuid::new_v4();
    bind_session(&db, sid, "-100456", Some(77)).await;
    store_msg(&db, "-100456", Some("77"), "user:alexey", "status?").await;
    // Bot's own outgoing row (record_outgoing shape) after the user — turn
    // completed before the kill.
    store_msg(&db, "-100456", Some("77"), BOT_SENDER_ID, "All green.").await;

    let r = classify_recently_active(db.pool().clone(), &HashSet::new()).await;
    assert!(
        r.interrupted.is_empty(),
        "bot-last must NOT be resumed — the turn already finished"
    );
    assert_eq!(r.completed.len(), 1);
    assert_eq!(r.completed[0], sid.simple().to_string()[..8].to_owned());
}

#[tokio::test]
async fn later_user_message_flips_completed_back_to_interrupted() {
    let db = test_db().await;
    let sid = Uuid::new_v4();
    bind_session(&db, sid, "-100789", Some(11)).await;
    store_msg(&db, "-100789", Some("11"), "user:alexey", "q1").await;
    store_msg(&db, "-100789", Some("11"), BOT_SENDER_ID, "a1").await;
    // A NEW user message after the bot reply = a fresh turn the restart
    // killed — this is the double-kill coma case (#729 resumed turns leave
    // no pending row), and it MUST land in interrupted, not completed.
    store_msg(&db, "-100789", Some("11"), "user:alexey", "q2").await;

    let r = classify_recently_active(db.pool().clone(), &HashSet::new()).await;
    assert_eq!(r.interrupted.len(), 1, "last word = user → interrupted");
    assert!(r.completed.is_empty());
}

#[tokio::test]
async fn no_topic_messages_classifies_unclassified() {
    let db = test_db().await;
    let sid = Uuid::new_v4();
    bind_session(&db, sid, "-100222", Some(5)).await;

    let r = classify_recently_active(db.pool().clone(), &HashSet::new()).await;
    assert!(
        r.interrupted.is_empty(),
        "resuming blind would replay noise"
    );
    assert_eq!(r.unclassified.len(), 1);
}

#[tokio::test]
async fn general_binding_null_thread_classifies() {
    let db = test_db().await;
    let sid = Uuid::new_v4();
    bind_session(&db, sid, "-100333", None).await;
    // General/DM rows are stored with a NULL thread — the classifier's
    // None arm must address exactly those, not fall into some topic.
    store_msg(&db, "-100333", None, "user:alexey", "general ping").await;

    let r = classify_recently_active(db.pool().clone(), &HashSet::new()).await;
    assert_eq!(r.interrupted.len(), 1);
    assert_eq!(r.interrupted[0].2, None);
}

#[tokio::test]
async fn already_resumed_sessions_are_skipped() {
    let db = test_db().await;
    let sid = Uuid::new_v4();
    bind_session(&db, sid, "-100444", Some(3)).await;
    store_msg(&db, "-100444", Some("3"), "user:alexey", "mid-turn text").await;

    let mut resumed = HashSet::new();
    resumed.insert(sid);
    let r = classify_recently_active(db.pool().clone(), &resumed).await;
    assert!(r.interrupted.is_empty(), "journal already roused this one");
    assert!(r.completed.is_empty() && r.unclassified.is_empty());
}

#[tokio::test]
async fn same_second_rows_resolve_to_last_inserted() {
    let db = test_db().await;
    let sid = Uuid::new_v4();
    bind_session(&db, sid, "-100555", Some(9)).await;
    // Both rows land in the same second (test speed) — the rowid tiebreak
    // must pick the LAST inserted, i.e. the bot's completion, not the user's.
    store_msg(&db, "-100555", Some("9"), "user:alexey", "go").await;
    store_msg(&db, "-100555", Some("9"), BOT_SENDER_ID, "done").await;

    let r = classify_recently_active(db.pool().clone(), &HashSet::new()).await;
    assert!(r.interrupted.is_empty(), "rowid tiebreak → bot row wins");
    assert_eq!(r.completed.len(), 1);
}

/// #180: a BUTTON TAP started the turn, so the bot card sitting last in the
/// topic is not a completion signal — it is the card the button rode in on.
/// Before this, a tap-initiated turn killed in the dispatch→PROCESSING window
/// (where NET 1 has no row) was classified `completed` and lost.
#[tokio::test]
async fn callback_origin_with_bot_last_classifies_interrupted() {
    let db = test_db().await;
    let sid = Uuid::new_v4();
    bind_session(&db, sid, "-100666", Some(21)).await;
    // Leg A re-binds on the tap, recording what refreshed the row.
    SessionBindingRepository::new(db.pool().clone())
        .upsert(
            sid.to_string(),
            "telegram",
            "-100666",
            Some(21),
            crate::db::BindingOrigin::Callback,
        )
        .await
        .unwrap();
    store_msg(&db, "-100666", Some("21"), "user:alexey", "ping").await;
    // The bot's own card — last word in the topic, but not a completion.
    store_msg(&db, "-100666", Some("21"), BOT_SENDER_ID, "card").await;

    let r = classify_recently_active(db.pool().clone(), &HashSet::new()).await;
    assert_eq!(
        r.interrupted.len(),
        1,
        "a tap-initiated turn must be resumable even when the bot card is last"
    );
    assert_eq!(r.interrupted[0].0, sid);
    assert_eq!(r.interrupted[0].1, -100666);
    assert_eq!(r.interrupted[0].2, Some(21));
    assert!(
        r.completed.is_empty(),
        "the card the button rode in on is not a completion signal"
    );
    assert!(r.unclassified.is_empty());
}

/// #200: once a tap-initiated turn completes, `turn_open_at` is cleared to NULL.
/// When the daemon restarts, the completed turn must NOT be spuriously classified as
/// interrupted; it falls through to check `last_topic_sender`, sees BOT_SENDER_ID,
/// and classifies `completed`.
#[tokio::test]
async fn completed_callback_turn_classifies_completed() {
    let db = test_db().await;
    let sid = Uuid::new_v4();
    bind_session(&db, sid, "-100777", Some(42)).await;
    let binding_repo = SessionBindingRepository::new(db.pool().clone());
    // Tap starts the turn: origin is callback and turn_open_at is set.
    binding_repo
        .upsert(
            sid.to_string(),
            "telegram",
            "-100777",
            Some(42),
            crate::db::BindingOrigin::Callback,
        )
        .await
        .unwrap();
    store_msg(&db, "-100777", Some("42"), "user:alexey", "request").await;
    // The bot finishes and posts its reply.
    store_msg(&db, "-100777", Some("42"), BOT_SENDER_ID, "done").await;

    // Turn completes normally: turn_open_at is cleared.
    binding_repo
        .clear_turn_open_at(&sid.to_string())
        .await
        .unwrap();

    let r = classify_recently_active(db.pool().clone(), &HashSet::new()).await;
    assert!(
        r.interrupted.is_empty(),
        "a completed tap turn must not be classified as interrupted"
    );
    assert_eq!(
        r.completed.len(),
        1,
        "a completed tap turn with bot last must classify as completed"
    );
    assert_eq!(r.completed[0], &sid.to_string()[..8]);
    assert!(r.unclassified.is_empty());
}

/// #180 back-compat: a row written before `last_origin` existed carries NULL
/// and must keep the pre-fix text semantics — the sender heuristic stays in
/// charge. An unrecognised value is read the same conservative way.
#[test]
fn null_and_unknown_origin_read_as_text() {
    use crate::db::BindingOrigin;
    assert_eq!(BindingOrigin::from_stored(None), BindingOrigin::Text);
    assert_eq!(
        BindingOrigin::from_stored(Some("text")),
        BindingOrigin::Text
    );
    assert_eq!(
        BindingOrigin::from_stored(Some("something-else")),
        BindingOrigin::Text
    );
    assert_eq!(
        BindingOrigin::from_stored(Some("callback")),
        BindingOrigin::Callback
    );
    assert_eq!(BindingOrigin::Callback.as_str(), "callback");
    assert_eq!(BindingOrigin::Text.as_str(), "text");
}

/// #218: A session pursuing an autonomous goal whose bot sent the last message
/// before daemon kill was interrupted in its autonomous loop.
/// If `goal_state` has `state = 'active'` and `turns_used < max_turns`, it must
/// classify as `interrupted` so it resumes and keeps working.
#[tokio::test]
async fn active_goal_with_bot_last_classifies_interrupted() {
    let db = test_db().await;
    let sid = Uuid::new_v4();
    bind_session(&db, sid, "-100888", Some(55)).await;
    store_msg(&db, "-100888", Some("55"), "user:alexey", "run goal").await;
    store_msg(
        &db,
        "-100888",
        Some("55"),
        BOT_SENDER_ID,
        "working on step 1",
    )
    .await;

    // Seed active goal with turns remaining
    let pool = db.pool().clone();
    let s_id = sid.to_string();
    {
        let conn = pool.get().await.unwrap();
        conn.interact(move |conn| {
            conn.execute(
                "INSERT INTO goal_state (id, session_id, goal_text, state, turns_used, max_turns, created_at, updated_at) \
                 VALUES (?1, ?2, 'make tea', 'active', 2, 10, '2026-09-13T00:00:00Z', '2026-09-13T00:00:00Z')",
                rusqlite::params!["goal-1", s_id],
            )
        })
        .await
        .unwrap()
        .unwrap();
    }

    let r = classify_recently_active(db.pool().clone(), &HashSet::new()).await;
    assert_eq!(
        r.interrupted.len(),
        1,
        "active unexhausted goal must classify as interrupted even if bot spoke last"
    );
    assert_eq!(r.interrupted[0].0, sid);
    assert_eq!(r.interrupted[0].1, -100888);
    assert_eq!(r.interrupted[0].2, Some(55));
    assert!(r.completed.is_empty());
}

/// #218: If an active goal has reached `max_turns`, it is exhausted and should
/// not classify as interrupted when bot spoke last.
#[tokio::test]
async fn exhausted_goal_with_bot_last_classifies_completed() {
    let db = test_db().await;
    let sid = Uuid::new_v4();
    bind_session(&db, sid, "-100889", Some(56)).await;
    store_msg(&db, "-100889", Some("56"), "user:alexey", "run goal").await;
    store_msg(
        &db,
        "-100889",
        Some("56"),
        BOT_SENDER_ID,
        "done with max turns",
    )
    .await;

    // Seed active goal but exhausted turns
    let pool = db.pool().clone();
    let s_id = sid.to_string();
    {
        let conn = pool.get().await.unwrap();
        conn.interact(move |conn| {
            conn.execute(
                "INSERT INTO goal_state (id, session_id, goal_text, state, turns_used, max_turns, created_at, updated_at) \
                 VALUES (?1, ?2, 'make tea', 'active', 10, 10, '2026-09-13T00:00:00Z', '2026-09-13T00:00:00Z')",
                rusqlite::params!["goal-2", s_id],
            )
        })
        .await
        .unwrap()
        .unwrap();
    }

    let r = classify_recently_active(db.pool().clone(), &HashSet::new()).await;
    assert!(
        r.interrupted.is_empty(),
        "exhausted goal must not classify as interrupted"
    );
    assert_eq!(r.completed.len(), 1);
    assert_eq!(r.completed[0], &sid.to_string()[..8]);
}

/// #218: If a goal is paused, it should not classify as interrupted when bot spoke last.
#[tokio::test]
async fn paused_goal_with_bot_last_classifies_completed() {
    let db = test_db().await;
    let sid = Uuid::new_v4();
    bind_session(&db, sid, "-100890", Some(57)).await;
    store_msg(&db, "-100890", Some("57"), "user:alexey", "pause goal").await;
    store_msg(&db, "-100890", Some("57"), BOT_SENDER_ID, "paused").await;

    // Seed paused goal
    let pool = db.pool().clone();
    let s_id = sid.to_string();
    {
        let conn = pool.get().await.unwrap();
        conn.interact(move |conn| {
            conn.execute(
                "INSERT INTO goal_state (id, session_id, goal_text, state, turns_used, max_turns, created_at, updated_at) \
                 VALUES (?1, ?2, 'make tea', 'paused', 2, 10, '2026-09-13T00:00:00Z', '2026-09-13T00:00:00Z')",
                rusqlite::params!["goal-3", s_id],
            )
        })
        .await
        .unwrap()
        .unwrap();
    }

    let r = classify_recently_active(db.pool().clone(), &HashSet::new()).await;
    assert!(
        r.interrupted.is_empty(),
        "paused goal must not classify as interrupted"
    );
    assert_eq!(r.completed.len(), 1);
    assert_eq!(r.completed[0], &sid.to_string()[..8]);
}

/// #244: A session with an active plan containing pending/incomplete tasks
/// whose bot sent the last message before daemon kill was interrupted while
/// executing a multi-step plan. It must classify as `interrupted` so it resumes.
#[tokio::test]
async fn active_plan_with_bot_last_classifies_interrupted() {
    in_temp_home(async {
        let db = test_db().await;
        let sid = Uuid::new_v4();
        bind_session(&db, sid, "-100901", Some(101)).await;
        store_msg(&db, "-100901", Some("101"), "user:alexey", "execute plan").await;
        store_msg(
            &db,
            "-100901",
            Some("101"),
            BOT_SENDER_ID,
            "Finished task 1, starting task 2",
        )
        .await;

        let mut plan = PlanDocument::new(sid, "Multi-step build".to_string());
        plan.status = PlanStatus::Active;
        let mut t1 = PlanTask::new(1, "Task 1".to_string(), "d1".to_string(), TaskType::Edit);
        t1.status = TaskStatus::Completed;
        let t2 = PlanTask::new(2, "Task 2".to_string(), "d2".to_string(), TaskType::Edit);
        plan.add_task(t1);
        plan.add_task(t2);
        save_plan(&plan).await.unwrap();

        let r = classify_recently_active(db.pool().clone(), &HashSet::new()).await;
        assert_eq!(
            r.interrupted.len(),
            1,
            "active incomplete plan must classify as interrupted when bot spoke last (#244)"
        );
        assert_eq!(r.interrupted[0].0, sid);
        assert_eq!(r.interrupted[0].1, -100901);
        assert_eq!(r.interrupted[0].2, Some(101));
        assert!(r.completed.is_empty());
    })
    .await;
}

/// #244: If all tasks in an active plan are Completed or Skipped, the plan is
/// finished; when bot spoke last, it must classify as `completed`.
#[tokio::test]
async fn active_plan_all_completed_tasks_with_bot_last_classifies_completed() {
    in_temp_home(async {
        let db = test_db().await;
        let sid = Uuid::new_v4();
        bind_session(&db, sid, "-100902", Some(102)).await;
        store_msg(&db, "-100902", Some("102"), "user:alexey", "execute plan").await;
        store_msg(
            &db,
            "-100902",
            Some("102"),
            BOT_SENDER_ID,
            "All tasks complete!",
        )
        .await;

        let mut plan = PlanDocument::new(sid, "Finished plan".to_string());
        plan.status = PlanStatus::Active;
        let mut t1 = PlanTask::new(1, "Task 1".to_string(), "d1".to_string(), TaskType::Edit);
        t1.status = TaskStatus::Completed;
        let mut t2 = PlanTask::new(2, "Task 2".to_string(), "d2".to_string(), TaskType::Edit);
        t2.status = TaskStatus::Skipped;
        plan.add_task(t1);
        plan.add_task(t2);
        save_plan(&plan).await.unwrap();

        let r = classify_recently_active(db.pool().clone(), &HashSet::new()).await;
        assert!(
            r.interrupted.is_empty(),
            "fully completed active plan must not classify as interrupted"
        );
        assert_eq!(r.completed.len(), 1);
        assert_eq!(r.completed[0], &sid.to_string()[..8]);
    })
    .await;
}

/// #244: An unapproved draft plan in `PlanStatus::Editing` is waiting for user
/// approval, not actively executing tasks; when bot spoke last, it classifies as `completed`.
#[tokio::test]
async fn editing_plan_with_bot_last_classifies_completed() {
    in_temp_home(async {
        let db = test_db().await;
        let sid = Uuid::new_v4();
        bind_session(&db, sid, "-100903", Some(103)).await;
        store_msg(&db, "-100903", Some("103"), "user:alexey", "plan this").await;
        store_msg(
            &db,
            "-100903",
            Some("103"),
            BOT_SENDER_ID,
            "Here is the plan draft, awaiting approval",
        )
        .await;

        let mut plan = PlanDocument::new(sid, "Draft plan".to_string());
        plan.status = PlanStatus::Editing;
        plan.pending_approval = true;
        let t1 = PlanTask::new(1, "Task 1".to_string(), "d1".to_string(), TaskType::Edit);
        plan.add_task(t1);
        save_plan(&plan).await.unwrap();

        let r = classify_recently_active(db.pool().clone(), &HashSet::new()).await;
        assert!(
            r.interrupted.is_empty(),
            "editing draft plan must not classify as interrupted"
        );
        assert_eq!(r.completed.len(), 1);
        assert_eq!(r.completed[0], &sid.to_string()[..8]);
    })
    .await;
}

/// #244: A plan with `pre_init_editing = true` is in pre-init planning mode,
/// not actively executing tasks; when bot spoke last, it classifies as `completed`.
#[tokio::test]
async fn pre_init_editing_plan_with_bot_last_classifies_completed() {
    in_temp_home(async {
        let db = test_db().await;
        let sid = Uuid::new_v4();
        bind_session(&db, sid, "-100904", Some(104)).await;
        store_msg(&db, "-100904", Some("104"), "user:alexey", "make a plan").await;
        store_msg(
            &db,
            "-100904",
            Some("104"),
            BOT_SENDER_ID,
            "I will create a plan for you",
        )
        .await;

        let mut plan = PlanDocument::new(sid, "Pre-init plan".to_string());
        plan.status = PlanStatus::Active;
        plan.pre_init_editing = true;
        let t1 = PlanTask::new(1, "Task 1".to_string(), "d1".to_string(), TaskType::Edit);
        plan.add_task(t1);
        save_plan(&plan).await.unwrap();

        let r = classify_recently_active(db.pool().clone(), &HashSet::new()).await;
        assert!(
            r.interrupted.is_empty(),
            "pre-init editing plan must not classify as interrupted"
        );
        assert_eq!(r.completed.len(), 1);
        assert_eq!(r.completed[0], &sid.to_string()[..8]);
    })
    .await;
}

/// #244: An active plan with 0 tasks has no work to execute; when bot spoke
/// last, it classifies as `completed`.
#[tokio::test]
async fn active_plan_empty_tasks_with_bot_last_classifies_completed() {
    in_temp_home(async {
        let db = test_db().await;
        let sid = Uuid::new_v4();
        bind_session(&db, sid, "-100905", Some(105)).await;
        store_msg(
            &db,
            "-100905",
            Some("105"),
            "user:alexey",
            "init empty plan",
        )
        .await;
        store_msg(
            &db,
            "-100905",
            Some("105"),
            BOT_SENDER_ID,
            "Empty plan initialized",
        )
        .await;

        let mut plan = PlanDocument::new(sid, "Empty plan".to_string());
        plan.status = PlanStatus::Active;
        save_plan(&plan).await.unwrap();

        let r = classify_recently_active(db.pool().clone(), &HashSet::new()).await;
        assert!(
            r.interrupted.is_empty(),
            "empty active plan must not classify as interrupted"
        );
        assert_eq!(r.completed.len(), 1);
        assert_eq!(r.completed[0], &sid.to_string()[..8]);
    })
    .await;
}

/// #244: Verify WAKE_RECENT_SECS is set to 3600 (60 minutes) and queries
/// bindings updated within 3600 seconds.
#[tokio::test]
async fn wake_recent_secs_constant_value_is_3600() {
    assert_eq!(
        crate::channels::telegram::resume::WAKE_RECENT_SECS,
        3600,
        "WAKE_RECENT_SECS must be 3600 seconds (60 minutes, #244)"
    );
    let db = test_db().await;
    let sid = Uuid::new_v4();
    bind_session(&db, sid, "-100906", Some(106)).await;
    let repo = SessionBindingRepository::new(db.pool().clone());
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    // 30 minutes ago (1800s): inside 3600s window
    let since = now - crate::channels::telegram::resume::WAKE_RECENT_SECS;
    let recent = repo.recent_for_channel("telegram", since).await.unwrap();
    assert_eq!(recent.len(), 1);
    assert_eq!(recent[0].session_id, sid.to_string());
}
