//! #572 — boot reconciliation of topic bindings.
//!
//! Two passes and a budget, and each is asserted for a reason that is not
//! cosmetic:
//!
//! * the recorded-facts pass runs FIRST, and the topics it decides are never
//!   probed — asserted by ORDER (a probe-log of what was actually asked) rather
//!   than by comment;
//! * the sweep issues at most `budget` probes;
//! * only a `Gone` verdict acts. `Live` and `Inconclusive` leave the binding
//!   standing and write no fact — the property that keeps a network blip or a
//!   rate limit from silently retiring a live lane.
//!
//! `reconcile` takes the probe as a parameter, so all of this is driven with a
//! scripted closure and no network. The production closure is a single
//! `send_chat_action` whose failure text goes through the shared
//! `plan_card::is_message_gone_error` vocabulary.

use crate::channels::telegram::handler::{TopicEvent, apply_topic_teardown};
use crate::channels::telegram::topic_reconcile::{ProbeOutcome, classify_probe_error, reconcile};
use crate::db::models::{ChannelMessage as DbChannelMessage, Session};
use crate::db::{
    BindingOrigin, ChannelMessageRepository, Database, SessionBindingRepository, SessionRepository,
};
use crate::services::{ServiceContext, SessionService};
use std::sync::{Arc, Mutex};
use uuid::Uuid;

const CHAT: i64 = -1_005_720_002;
const CHAT_STR: &str = "-1005720002";

async fn test_db() -> Database {
    let db = Database::connect_in_memory().await.expect("in-memory db");
    db.run_migrations().await.expect("migrations");
    db
}

async fn seed(db: &Database, thread: i32) -> Uuid {
    let id = Uuid::new_v4();
    SessionRepository::new(db.pool().clone())
        .create(&Session {
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
        })
        .await
        .expect("create session");
    SessionBindingRepository::new(db.pool().clone())
        .upsert(
            id.to_string(),
            "telegram",
            CHAT_STR,
            Some(thread),
            BindingOrigin::Text,
        )
        .await
        .expect("bind");
    id
}

/// A lifecycle fact as the inbound path would have written it.
async fn record_fact(messages: &ChannelMessageRepository, thread: i32, message_type: &str) {
    let row = DbChannelMessage::new(
        "telegram".into(),
        CHAT_STR.into(),
        None,
        "1".into(),
        "admin".into(),
        format!("topic {message_type}"),
        message_type.into(),
        None,
    )
    .with_thread(Some(thread.to_string()), None);
    messages.insert(&row).await.expect("insert fact");
}

fn svc_for(db: &Database) -> SessionService {
    SessionService::new(ServiceContext::new(db.pool().clone()))
}

async fn still_bound(bindings: &SessionBindingRepository, thread: i32) -> bool {
    bindings
        .find_by_channel_chat_thread("telegram", CHAT_STR, Some(thread))
        .await
        .unwrap()
        .is_some()
}

async fn fact_count(messages: &ChannelMessageRepository, thread: i32, t: &str) -> usize {
    messages
        .recent(
            Some("telegram"),
            CHAT_STR,
            10,
            Some(&thread.to_string()),
            Some(t),
        )
        .await
        .unwrap()
        .len()
}

/// The recorded-facts pass runs first, so a topic the store already knows about
/// is torn down WITHOUT a probe being spent on it.
#[tokio::test]
async fn recorded_facts_are_applied_before_any_probe() {
    let db = test_db().await;
    let bindings = SessionBindingRepository::new(db.pool().clone());
    let messages = ChannelMessageRepository::new(db.pool().clone());
    let svc = svc_for(&db);

    let known = 601;
    let other = 602;
    seed(&db, known).await;
    seed(&db, other).await;
    record_fact(&messages, known, "topic_closed").await;

    let probed: Arc<Mutex<Vec<(i64, i32)>>> = Arc::new(Mutex::new(Vec::new()));
    let log = probed.clone();
    let report = reconcile(&bindings, &messages, &svc, 8, move |chat, thread| {
        let log = log.clone();
        async move {
            log.lock().unwrap().push((chat, thread));
            ProbeOutcome::Live
        }
    })
    .await;

    // The fact decided one topic — and the probe log proves the decision came
    // first: that topic was never asked about, while the undecided one was.
    assert_eq!(report.facts_applied, 1, "the recorded closure was applied");
    assert!(
        !still_bound(&bindings, known).await,
        "a topic with a recorded closure fact must be unbound"
    );
    let asked = probed.lock().unwrap().clone();
    assert!(
        asked.contains(&(CHAT, other)),
        "an undecided bound topic must be probed — otherwise this test could not fail"
    );
    assert!(
        !asked.contains(&(CHAT, known)),
        "a topic decided by a recorded fact must not also be probed"
    );
}

/// The sweep is bounded, acts on `Gone` alone, and leaves everything else —
/// binding and store — untouched.
#[tokio::test]
async fn bounded_sweep_acts_only_on_a_gone_verdict() {
    let db = test_db().await;
    let bindings = SessionBindingRepository::new(db.pool().clone());
    let messages = ChannelMessageRepository::new(db.pool().clone());
    let svc = svc_for(&db);

    let gone = 701;
    let alive = 702;
    let broken = 703; // probe fails with something unrecognised
    seed(&db, gone).await;
    seed(&db, alive).await;
    seed(&db, broken).await;

    let probed: Arc<Mutex<Vec<(i64, i32)>>> = Arc::new(Mutex::new(Vec::new()));
    let log = probed.clone();
    let budget = 8usize;
    let report = reconcile(&bindings, &messages, &svc, budget, move |_chat, thread| {
        let log = log.clone();
        async move {
            log.lock().unwrap().push((_chat, thread));
            match thread {
                t if t == gone => ProbeOutcome::Gone,
                t if t == alive => ProbeOutcome::Live,
                _ => ProbeOutcome::Inconclusive("Bad Gateway".into()),
            }
        }
    })
    .await;

    assert!(
        report.probed <= budget,
        "the sweep must issue at most the budget ({budget}) of probes, issued {}",
        report.probed
    );

    // Gone: unbound, retired, and the fact written — nobody else can write it.
    assert!(!still_bound(&bindings, gone).await);
    assert_eq!(fact_count(&messages, gone, "topic_deleted").await, 1);

    // Live: untouched, and nothing recorded about it.
    assert!(still_bound(&bindings, alive).await);
    assert_eq!(fact_count(&messages, alive, "topic_deleted").await, 0);

    // Inconclusive: the load-bearing negative — no teardown, no fact.
    assert!(
        still_bound(&bindings, broken).await,
        "an inconclusive probe must not retire a binding"
    );
    assert_eq!(
        fact_count(&messages, broken, "topic_deleted").await,
        0,
        "an inconclusive probe must not write a closure fact"
    );
    assert_eq!(report.inconclusive, 2, "Live and Inconclusive both decide nothing");
}

/// The classification the production probe relies on: the shared gone-vocabulary
/// retires, anything else does not.
#[test]
fn probe_errors_are_classified_by_the_shared_gone_vocabulary() {
    assert_eq!(
        classify_probe_error("Bad Request: TOPIC_CLOSED"),
        ProbeOutcome::Gone
    );
    assert_eq!(
        classify_probe_error("Bad Request: message thread not found"),
        ProbeOutcome::Gone
    );
    assert!(matches!(
        classify_probe_error("Request failed after retries: timed out"),
        ProbeOutcome::Inconclusive(_)
    ));
    assert!(matches!(
        classify_probe_error("Too Many Requests: retry after 30"),
        ProbeOutcome::Inconclusive(_)
    ));
}

/// The `Deleted` arm is reachable through the same core the inbound path uses,
/// so the probe writes its fact in the one vocabulary the store reads.
#[tokio::test]
async fn probe_teardown_uses_the_inbound_core_and_its_own_fact_type() {
    let db = test_db().await;
    let bindings = SessionBindingRepository::new(db.pool().clone());
    let messages = ChannelMessageRepository::new(db.pool().clone());
    let svc = svc_for(&db);

    let thread = 801;
    seed(&db, thread).await;

    apply_topic_teardown(
        TopicEvent::Deleted,
        CHAT,
        thread,
        None,
        None,
        None,
        &messages,
        &bindings,
        &svc,
    )
    .await;

    assert!(!still_bound(&bindings, thread).await);
    assert_eq!(fact_count(&messages, thread, "topic_deleted").await, 1);
    assert_eq!(fact_count(&messages, thread, "topic_closed").await, 0);
}
