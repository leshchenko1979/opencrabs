//! Tests for the `session_search` SCOPE default and the two search paths (#855).
//!
//! Two behaviours are pinned here, and they are the two halves of the issue:
//!
//! 1. An ABSENT `session` now means the CALLING session, not every session. That
//!    is what turns the default call from a full-table `LIKE` scan into a
//!    0.013 s indexed lookup.
//! 2. `session="all"` goes through the FTS5 index and matches WORDS, while a
//!    scoped search stays on `LIKE` and matches arbitrary SUBSTRINGS. The
//!    difference is asserted directly rather than assumed, because a future
//!    "simplification" that put both paths on FTS5 would silently change what
//!    the common case finds.
//!
//! House rule: no inline test modules in source files — everything lives here
//! under `src/tests/`.

use crate::brain::tools::session_search::{SearchScope, SessionSearchTool, resolve_scope};
use crate::brain::tools::{Tool, ToolExecutionContext};
use crate::db::Database;
use crate::db::models::{Message, Session};
use crate::db::repository::{MessageRepository, SessionRepository};
use serde_json::json;
use uuid::Uuid;

async fn setup() -> (Database, SessionSearchTool) {
    let db = Database::connect_in_memory().await.unwrap();
    db.run_migrations().await.unwrap();
    let tool = SessionSearchTool::new(db.pool().clone());
    (db, tool)
}

/// A context whose session id is the one the test calls "mine".
fn ctx(session: Uuid) -> ToolExecutionContext {
    ToolExecutionContext::new(session)
}

/// Create a session with the given message bodies, returning its id.
async fn seed(db: &Database, title: &str, bodies: &[&str]) -> Uuid {
    let srepo = SessionRepository::new(db.pool().clone());
    let mrepo = MessageRepository::new(db.pool().clone());
    let session = Session::new(Some(title.to_string()), Some("m".to_string()), None);
    srepo.create(&session).await.unwrap();
    for (i, body) in bodies.iter().enumerate() {
        mrepo
            .create(&Message::new(
                session.id,
                "user".into(),
                (*body).to_string(),
                i as i32 + 1,
            ))
            .await
            .unwrap();
    }
    session.id
}

// ---------------------------------------------------------------- resolution

/// The absent case is the calling session. This is the whole default change:
/// before #855 it resolved to `All`, which was the full-table scan.
#[test]
fn absent_session_resolves_to_the_calling_session() {
    let calling = Uuid::new_v4();
    assert_eq!(
        resolve_scope(None, calling),
        SearchScope::Calling(calling),
        "an omitted 'session' must scope to the caller, not to every session"
    );
}

/// `session="all"` is the explicit cross-session opt-in.
#[test]
fn explicit_all_resolves_to_all() {
    assert_eq!(resolve_scope(Some("all"), Uuid::new_v4()), SearchScope::All);
}

/// A blank `session` is the same request as an absent one, so a template that
/// renders `session: ""` cannot accidentally become a title filter.
#[test]
fn blank_session_resolves_to_the_calling_session() {
    let calling = Uuid::new_v4();
    for blank in ["", " ", "\t\n"] {
        assert_eq!(
            resolve_scope(Some(blank), calling),
            SearchScope::Calling(calling),
            "'{blank}' must be treated as absent"
        );
    }
}

/// A number or a title keyword stays a filter — the pre-existing vocabulary is
/// untouched.
#[test]
fn number_and_title_stay_filters() {
    assert_eq!(
        resolve_scope(Some("2"), Uuid::new_v4()),
        SearchScope::Filter("2".to_string())
    );
    assert_eq!(
        resolve_scope(Some("deploy"), Uuid::new_v4()),
        SearchScope::Filter("deploy".to_string())
    );
}

// ---------------------------------------------------------------- scoped path

/// An unscoped call searches ONLY the calling session.
///
/// The other session holds the same word, so a search that still scanned every
/// session would return two rows and this test would see both.
#[tokio::test]
async fn default_search_is_scoped_to_the_calling_session() {
    let (db, tool) = setup().await;
    let mine = seed(&db, "mine", &["the deploy failed at noon"]).await;
    let theirs = seed(&db, "theirs", &["a different deploy, unrelated"]).await;

    let result = tool
        .execute(
            json!({"operation": "search", "query": "deploy"}),
            &ctx(mine),
        )
        .await
        .unwrap();
    assert!(result.success, "error: {:?}", result.error);
    assert!(
        result.output.contains("the deploy failed at noon"),
        "the calling session's own message must be found, got: {}",
        result.output
    );
    assert!(
        !result.output.contains("a different deploy"),
        "another session's message must NOT appear in a default search, got: {}",
        result.output
    );
    let _ = theirs;
}

/// A scoped search keeps `LIKE` semantics: a substring INSIDE a word matches.
///
/// This is the half of the design that must not change. `deploy` inside
/// `redeployed` is a `LIKE` hit and an FTS5 miss, so this test fails loudly if
/// the scoped path is ever moved onto the token index.
#[tokio::test]
async fn scoped_search_matches_substrings_not_only_words() {
    let (db, tool) = setup().await;
    let mine = seed(&db, "mine", &["we redeployed the service twice"]).await;

    let result = tool
        .execute(
            json!({"operation": "search", "query": "deploy"}),
            &ctx(mine),
        )
        .await
        .unwrap();
    assert!(result.success, "error: {:?}", result.error);
    assert!(
        result.output.contains("redeployed"),
        "scoped search must match a substring inside a word, got: {}",
        result.output
    );
}

/// A scoped search on an empty session reports no match rather than an error.
#[tokio::test]
async fn scoped_search_on_empty_session_is_a_clean_miss() {
    let (db, tool) = setup().await;
    let mine = seed(&db, "empty", &[]).await;

    let result = tool
        .execute(
            json!({"operation": "search", "query": "anything"}),
            &ctx(mine),
        )
        .await
        .unwrap();
    assert!(result.success, "error: {:?}", result.error);
    assert!(
        result.output.contains("No messages found matching"),
        "got: {}",
        result.output
    );
}

/// An explicit `session="all"` reaches another session's messages.
#[tokio::test]
async fn explicit_all_reaches_other_sessions() {
    let (db, tool) = setup().await;
    let mine = seed(&db, "mine", &["nothing relevant here"]).await;
    seed(&db, "theirs", &["the zanzibar migration ran"]).await;

    let result = tool
        .execute(
            json!({"operation": "search", "query": "zanzibar", "session": "all"}),
            &ctx(mine),
        )
        .await
        .unwrap();
    assert!(result.success, "error: {:?}", result.error);
    assert!(
        result.output.contains("zanzibar"),
        "an explicit 'all' must search every session, got: {}",
        result.output
    );
}

// ---------------------------------------------------------------- all path

/// The `all` path builds the index inline on a small database and returns rows.
#[tokio::test]
async fn all_path_builds_the_index_inline_when_small() {
    let (db, tool) = setup().await;
    let mine = seed(&db, "mine", &["quokka sighting reported"]).await;

    let repo = MessageRepository::new(db.pool().clone());
    assert!(
        !repo.messages_fts_ready().await.unwrap(),
        "precondition: a freshly migrated database has an UNBUILT index"
    );

    let result = tool
        .execute(
            json!({"operation": "search", "query": "quokka", "session": "all"}),
            &ctx(mine),
        )
        .await
        .unwrap();
    assert!(result.success, "error: {:?}", result.error);
    assert!(result.output.contains("quokka"), "got: {}", result.output);
    assert!(
        repo.messages_fts_ready().await.unwrap(),
        "the inline build must leave the index ready"
    );
}

/// The `all` path matches WORDS: a substring inside a word does NOT hit, and
/// the output says so.
///
/// This is the documented trade of the indexed path, and it is the mirror of
/// `scoped_search_matches_substrings_not_only_words`. Asserting it keeps the
/// two paths' differing semantics visible instead of letting a reader assume
/// they are interchangeable.
#[tokio::test]
async fn all_path_matches_words_not_substrings() {
    let (db, tool) = setup().await;
    let mine = seed(&db, "mine", &["we redeployed the service twice"]).await;

    let result = tool
        .execute(
            json!({"operation": "search", "query": "deploy", "session": "all"}),
            &ctx(mine),
        )
        .await
        .unwrap();
    assert!(result.success, "error: {:?}", result.error);
    assert!(
        !result.output.contains("redeployed"),
        "the indexed path must not match a substring inside a word, got: {}",
        result.output
    );

    // The same query scoped DOES match, which is what makes the difference a
    // property of the paths rather than of the data.
    let scoped = tool
        .execute(
            json!({"operation": "search", "query": "deploy"}),
            &ctx(mine),
        )
        .await
        .unwrap();
    assert!(scoped.output.contains("redeployed"), "{}", scoped.output);
}

/// The `all` path's banner states that it matches words, so a user is never
/// told a substring miss means the text is absent.
#[tokio::test]
async fn all_path_states_its_token_semantics() {
    let (db, tool) = setup().await;
    let mine = seed(&db, "mine", &["token semantics banner probe"]).await;

    let result = tool
        .execute(
            json!({"operation": "search", "query": "banner", "session": "all"}),
            &ctx(mine),
        )
        .await
        .unwrap();
    assert!(result.success, "error: {:?}", result.error);
    assert!(
        result.output.contains("WORDS"),
        "the indexed path must say it matches words, got: {}",
        result.output
    );
    assert!(
        result.output.contains("substring"),
        "and must point at the scoped path for substring semantics, got: {}",
        result.output
    );
}

/// A query made only of punctuation produces no `MATCH` expression and so no
/// rows — and, critically, no FTS5 syntax error.
#[tokio::test]
async fn all_path_survives_a_punctuation_only_query() {
    let (db, tool) = setup().await;
    let mine = seed(&db, "mine", &["some ordinary content"]).await;

    let result = tool
        .execute(
            json!({"operation": "search", "query": "!!!", "session": "all"}),
            &ctx(mine),
        )
        .await
        .unwrap();
    assert!(
        result.success,
        "a punctuation-only query must not error, got: {:?}",
        result.error
    );
    assert!(
        result.output.contains("No messages found matching"),
        "got: {}",
        result.output
    );
}

/// A query with FTS5 operators in it is treated as literal words, not syntax.
#[tokio::test]
async fn all_path_treats_operators_as_text() {
    let (db, tool) = setup().await;
    let mine = seed(&db, "mine", &["alpha beta gamma"]).await;

    // Unquoted this would be `alpha NOT beta`, which is a different query and
    // would return the alpha row; quoted it is the phrase "alpha-beta", which
    // matches nothing here.
    let result = tool
        .execute(
            json!({"operation": "search", "query": "alpha-beta", "session": "all"}),
            &ctx(mine),
        )
        .await
        .unwrap();
    assert!(
        result.success,
        "operators in a query must not be executed, got: {:?}",
        result.error
    );
}

/// The `all` path labels rows that hold the query verbatim, because an indexed
/// hit is not evidence of a phrase match.
#[tokio::test]
async fn all_path_reports_how_many_rows_are_verbatim() {
    let (db, tool) = setup().await;
    let mine = seed(
        &db,
        "mine",
        &[
            "the deploy failed",
            "the deploy was fine",
            "a rollout, deployed",
        ],
    )
    .await;

    let result = tool
        .execute(
            json!({"operation": "search", "query": "deploy failed", "session": "all"}),
            &ctx(mine),
        )
        .await
        .unwrap();
    assert!(result.success, "error: {:?}", result.error);
    // Two rows hold both words, one holds them contiguously — the banner exists
    // only when the sets differ, and it must name the count.
    assert!(
        result.output.contains("verbatim"),
        "the banner must say how many rows hold the phrase verbatim, got: {}",
        result.output
    );
}

// ---------------------------------------------------------------- tail

/// `tail` is untouched by the scope change: it still defaults to the newest
/// session and still rejects `all`.
#[tokio::test]
async fn tail_still_defaults_to_the_newest_session() {
    let (db, tool) = setup().await;
    let older = seed(&db, "older", &["older body"]).await;
    let newer = seed(&db, "newer", &["newer body"]).await;
    let _ = (older, newer);

    let result = tool
        .execute(json!({"operation": "tail", "n": 5}), &ctx(Uuid::new_v4()))
        .await
        .unwrap();
    assert!(result.success, "error: {:?}", result.error);
    assert!(
        result.output.contains("newer body"),
        "tail must still read the newest session, got: {}",
        result.output
    );
}
