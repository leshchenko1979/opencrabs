use crate::db::Database;
use crate::db::models::Message;
use crate::db::models::Session;
use crate::db::repository::SessionRepository;
use crate::db::repository::message::*;
use tokio;

#[tokio::test]
async fn test_message_crud() {
    let db = Database::connect_in_memory()
        .await
        .expect("Failed to create database");
    db.run_migrations().await.expect("Failed to run migrations");
    let session_repo = SessionRepository::new(db.pool().clone());
    let message_repo = MessageRepository::new(db.pool().clone());

    // Create session first
    let session = Session::new(Some("Test".to_string()), Some("model".to_string()), None);
    session_repo
        .create(&session)
        .await
        .expect("Failed to create session");

    // Create message
    let message = Message::new(session.id, "user".to_string(), "Hello!".to_string(), 1);
    message_repo
        .create(&message)
        .await
        .expect("Failed to create message");

    // Read
    let found = message_repo
        .find_by_id(message.id)
        .await
        .expect("Failed to find");
    assert!(found.is_some());
    assert_eq!(found.unwrap().content, "Hello!");

    // Update
    let mut updated = message.clone();
    updated.content = "Updated content".to_string();
    message_repo
        .update(&updated)
        .await
        .expect("Failed to update");

    let found = message_repo
        .find_by_id(message.id)
        .await
        .expect("Failed to find");
    assert_eq!(found.unwrap().content, "Updated content");

    // Delete
    message_repo
        .delete(message.id)
        .await
        .expect("Failed to delete");
    let found = message_repo
        .find_by_id(message.id)
        .await
        .expect("Failed to find");
    assert!(found.is_none());
}

#[tokio::test]
async fn test_message_list_by_session() {
    let db = Database::connect_in_memory()
        .await
        .expect("Failed to create database");
    db.run_migrations().await.expect("Failed to run migrations");
    let session_repo = SessionRepository::new(db.pool().clone());
    let message_repo = MessageRepository::new(db.pool().clone());

    let session = Session::new(Some("Test".to_string()), Some("model".to_string()), None);
    session_repo
        .create(&session)
        .await
        .expect("Failed to create session");

    // Create multiple messages
    for i in 0..3 {
        let msg = Message::new(
            session.id,
            "user".to_string(),
            format!("Message {}", i),
            i + 1,
        );
        message_repo
            .create(&msg)
            .await
            .expect("Failed to create message");
    }

    let messages = message_repo
        .list_by_session(session.id)
        .await
        .expect("Failed to list");
    assert_eq!(messages.len(), 3);

    let count = message_repo
        .count_by_session(session.id)
        .await
        .expect("Failed to count");
    assert_eq!(count, 3);
}

/// #730: a turn cancelled before any reply leaves a user query plus an empty
/// assistant placeholder. The pair must be removed together so the query does
/// not linger in the next turn's context and duplicate on resend.
#[tokio::test]
async fn delete_trailing_unanswered_pair_removes_query_and_empty_placeholder() {
    let db = Database::connect_in_memory().await.unwrap();
    db.run_migrations().await.unwrap();
    let session_repo = SessionRepository::new(db.pool().clone());
    let repo = MessageRepository::new(db.pool().clone());

    let session = Session::new(Some("t".to_string()), Some("m".to_string()), None);
    session_repo.create(&session).await.unwrap();

    // A prior answered pair that must survive untouched.
    repo.create(&Message::new(session.id, "user".into(), "first".into(), 1))
        .await
        .unwrap();
    repo.create(&Message::new(
        session.id,
        "assistant".into(),
        "a reply".into(),
        2,
    ))
    .await
    .unwrap();
    // The cancelled turn: a query and an empty assistant placeholder.
    repo.create(&Message::new(session.id, "user".into(), "go".into(), 3))
        .await
        .unwrap();
    repo.create(&Message::new(
        session.id,
        "assistant".into(),
        String::new(),
        4,
    ))
    .await
    .unwrap();

    let deleted = repo
        .delete_trailing_unanswered_pair(session.id)
        .await
        .unwrap();
    assert_eq!(
        deleted, 2,
        "the query and its empty placeholder are removed"
    );

    let remaining = repo.list_by_session(session.id).await.unwrap();
    assert_eq!(remaining.len(), 2, "only the answered pair survives");
    assert_eq!(remaining[0].content, "first");
    assert_eq!(remaining[1].content, "a reply");
}

/// The guard: a trailing assistant row that actually holds a reply is a real
/// answered turn and must never be disturbed.
#[tokio::test]
async fn delete_trailing_unanswered_pair_leaves_answered_turn_alone() {
    let db = Database::connect_in_memory().await.unwrap();
    db.run_migrations().await.unwrap();
    let session_repo = SessionRepository::new(db.pool().clone());
    let repo = MessageRepository::new(db.pool().clone());

    let session = Session::new(Some("t".to_string()), Some("m".to_string()), None);
    session_repo.create(&session).await.unwrap();

    repo.create(&Message::new(session.id, "user".into(), "go".into(), 1))
        .await
        .unwrap();
    repo.create(&Message::new(
        session.id,
        "assistant".into(),
        "done".into(),
        2,
    ))
    .await
    .unwrap();

    let deleted = repo
        .delete_trailing_unanswered_pair(session.id)
        .await
        .unwrap();
    assert_eq!(deleted, 0, "an answered turn is untouched");
    assert_eq!(repo.list_by_session(session.id).await.unwrap().len(), 2);
}

/// #1166: find_recent_by_session returns the LAST n messages in
/// chronological order, pushing the LIMIT into SQL instead of loading the
/// whole history.
#[tokio::test]
async fn find_recent_by_session_returns_chronological_tail() {
    let db = Database::connect_in_memory().await.unwrap();
    db.run_migrations().await.unwrap();
    let session_repo = SessionRepository::new(db.pool().clone());
    let repo = MessageRepository::new(db.pool().clone());

    let session = Session::new(Some("t".to_string()), Some("m".to_string()), None);
    session_repo.create(&session).await.unwrap();

    for i in 1..=5 {
        repo.create(&Message::new(
            session.id,
            "user".into(),
            format!("msg{}", i),
            i,
        ))
        .await
        .unwrap();
    }

    // Tail of 2 = [msg4, msg5] — oldest-first within the window.
    let tail = repo.find_recent_by_session(session.id, 2).await.unwrap();
    assert_eq!(tail.len(), 2);
    assert_eq!(tail[0].content, "msg4");
    assert_eq!(tail[1].content, "msg5");

    // A limit larger than the history returns everything, still ordered.
    let all = repo.find_recent_by_session(session.id, 100).await.unwrap();
    assert_eq!(all.len(), 5);
    assert_eq!(all[0].content, "msg1");

    // Empty session yields an empty vec rather than an error.
    let empty_session = Session::new(Some("e".to_string()), Some("m".to_string()), None);
    session_repo.create(&empty_session).await.unwrap();
    let none = repo
        .find_recent_by_session(empty_session.id, 10)
        .await
        .unwrap();
    assert!(none.is_empty());
}

/// #278: pruning messages older than cutoff removes expired messages and child rows
#[tokio::test]
async fn test_prune_older_than_removes_expired_messages() {
    let db = Database::connect_in_memory().await.unwrap();
    db.run_migrations().await.unwrap();
    let session_repo = SessionRepository::new(db.pool().clone());
    let repo = MessageRepository::new(db.pool().clone());

    let session = Session::new(Some("t".to_string()), Some("m".to_string()), None);
    session_repo.create(&session).await.unwrap();

    let mut old_msg = Message::new(session.id, "user".into(), "old message".into(), 1);
    old_msg.created_at = chrono::Utc::now() - chrono::Duration::days(100);
    repo.create(&old_msg).await.unwrap();

    let mut recent_msg = Message::new(session.id, "user".into(), "recent message".into(), 2);
    recent_msg.created_at = chrono::Utc::now() - chrono::Duration::days(10);
    repo.create(&recent_msg).await.unwrap();

    let cutoff = (chrono::Utc::now() - chrono::Duration::days(90)).timestamp();
    let pruned = repo.prune_older_than(cutoff).await.unwrap();
    assert_eq!(pruned, 1);

    let remaining = repo.list_by_session(session.id).await.unwrap();
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].content, "recent message");
}

/// A delete lets a later insert reuse an earlier `sequence` number, so every
/// tail reader must key on INSERTION order (`rowid`) and never on `sequence`.
///
/// The fixture is the shape `count(*) + 1` numbering produced in production:
/// five rows, the middle three deleted, then a newer row whose sequence (3)
/// is SMALLER than an older surviving row's (5). A `sequence DESC` reader
/// returns the stale row here — and `find_recent_by_session` carries a LIMIT,
/// so it drops the genuinely recent row outright rather than reordering it.
#[tokio::test]
async fn tail_readers_follow_insertion_order_not_reused_sequence() {
    let db = Database::connect_in_memory().await.unwrap();
    db.run_migrations().await.unwrap();
    let session_repo = SessionRepository::new(db.pool().clone());
    let repo = MessageRepository::new(db.pool().clone());

    let session = Session::new(Some("t".to_string()), Some("m".to_string()), None);
    session_repo.create(&session).await.unwrap();

    // Insertion order: first(user,1), b(assistant,2), c(assistant,3),
    // d(assistant,4), older_assistant(assistant,5).
    let first = Message::new(session.id, "user".into(), "first".into(), 1);
    repo.create(&first).await.unwrap();

    let mut doomed = Vec::new();
    for seq in 2..=4 {
        let msg = Message::new(
            session.id,
            "assistant".into(),
            format!("doomed{}", seq),
            seq,
        );
        repo.create(&msg).await.unwrap();
        doomed.push(msg);
    }

    let mut older_assistant = Message::new(session.id, "assistant".into(), "older".into(), 5);
    older_assistant.input_tokens = Some(500);
    repo.create(&older_assistant).await.unwrap();

    // Free sequences 2..=4 the way a turn rollback does, so `MAX(sequence) + 1`
    // (and, historically, `count(*) + 1`) hands the next insert a REUSED
    // number below the surviving row's.
    for msg in &doomed {
        repo.delete(msg.id).await.unwrap();
    }

    // Newest row by insertion order, carrying the recycled sequence 3.
    let mut newest = Message::new(session.id, "assistant".into(), "newest".into(), 3);
    newest.input_tokens = Some(900);
    repo.create(&newest).await.unwrap();

    // The window is the sharp one: LIMIT 1 over `sequence DESC` returns
    // `older_assistant` (5) and loses `newest` (3) entirely.
    let window = repo.find_recent_by_session(session.id, 1).await.unwrap();
    assert_eq!(window.len(), 1);
    assert_eq!(
        window[0].content, "newest",
        "find_recent_by_session must take the last INSERTED row, not the highest sequence"
    );

    // A window of two is [older_assistant, newest] in insertion order.
    let pair = repo.find_recent_by_session(session.id, 2).await.unwrap();
    assert_eq!(pair.len(), 2);
    assert_eq!(pair[0].content, "older");
    assert_eq!(pair[1].content, "newest");

    // Single-row tail readers.
    let last = repo.get_last_message(session.id).await.unwrap().unwrap();
    assert_eq!(last.content, "newest");

    let last_assistant = repo
        .get_last_assistant_message(session.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(last_assistant.content, "newest");

    let tokens = repo.last_assistant_input_tokens(session.id).await.unwrap();
    assert_eq!(
        tokens,
        Some(900),
        "the context-size restore must read the last INSERTED assistant row"
    );
}

/// #1046: `messages.resolved_model` round-trips, and a historical row (one
/// written before the column existed) reads back as `None` rather than
/// failing to load.
#[tokio::test]
async fn resolved_model_round_trips_and_defaults_to_null() {
    let db = Database::connect_in_memory().await.unwrap();
    db.run_migrations().await.unwrap();
    let session_repo = SessionRepository::new(db.pool().clone());
    let message_repo = MessageRepository::new(db.pool().clone());

    let session = Session::new(
        Some("Resolved".to_string()),
        Some("model".to_string()),
        None,
    );
    session_repo.create(&session).await.unwrap();

    // A row with no served id — the shape every historical row has.
    let plain = Message::new(session.id, "assistant".to_string(), "hi".to_string(), 1);
    assert_eq!(
        plain.resolved_model, None,
        "new rows start with no served id"
    );
    message_repo.create(&plain).await.unwrap();
    let loaded = message_repo.find_by_id(plain.id).await.unwrap().unwrap();
    assert_eq!(loaded.resolved_model, None, "NULL must read back as None");

    // A row that recorded the served id, written through the INSERT.
    let mut with_served = Message::new(session.id, "assistant".to_string(), "hi".to_string(), 2);
    with_served.resolved_model = Some("claude-sonnet-4-5".to_string());
    message_repo.create(&with_served).await.unwrap();
    let loaded = message_repo
        .find_by_id(with_served.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        loaded.resolved_model.as_deref(),
        Some("claude-sonnet-4-5"),
        "the served id must survive the INSERT and the row mapping"
    );

    // And the UPDATE path must carry it too: the assistant row is created
    // first and stamped with usage at turn end.
    let mut updated = loaded;
    updated.resolved_model = Some("claude-opus-4-6".to_string());
    message_repo.update(&updated).await.unwrap();
    let reloaded = message_repo.find_by_id(updated.id).await.unwrap().unwrap();
    assert_eq!(
        reloaded.resolved_model.as_deref(),
        Some("claude-opus-4-6"),
        "the served id must survive the UPDATE too"
    );
}
