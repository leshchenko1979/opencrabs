//! `sessions.parent_session_id` — migration 68 plus its post-pass heal (#192).
//!
//! A sub-agent's child session must record the lane that spawned it. The column
//! is appended LAST to the migration list, above the declared
//! `MIGRATION_EFFECTS` window, so two boots have to be covered:
//!
//! - a database stamped one below the new entry gains the column when
//!   `to_latest` applies migration 68;
//! - a database stamped AT the new count with the column absent (the #1401
//!   shape: a pre-merge list skipped it and the stamp says done) gains it only
//!   from `heal_sessions_parent_session_id`, which is why that heal exists.
//!
//! The write/read leg proves the value survives the round trip through both
//! INSERT paths, which is what `oc-fanout` needs to resolve a child uuid back to
//! its lane.

use crate::db::Database;
use crate::db::database::{MIGRATION_SQL, build_migrations};
use crate::db::models::Session;
use crate::db::repository::SessionRepository;
use crate::services::{ServiceContext, SessionService};
use uuid::Uuid;

/// 1-based position of the migration that adds `parent_session_id`.
fn parent_migration_index() -> usize {
    MIGRATION_SQL
        .iter()
        .position(|sql| sql.contains("ADD COLUMN parent_session_id"))
        .expect("the parent_session_id migration must be in the list")
        + 1
}

async fn has_parent_column(db: &Database) -> bool {
    db.pool()
        .get()
        .await
        .unwrap()
        .interact(|conn| {
            crate::db::migration_heal::has_column(conn, "sessions", "parent_session_id")
        })
        .await
        .unwrap()
        .unwrap()
}

async fn user_version(db: &Database) -> i64 {
    db.pool()
        .get()
        .await
        .unwrap()
        .interact(|conn| conn.pragma_query_value(None, "user_version", |r| r.get(0)))
        .await
        .unwrap()
        .unwrap()
}

/// Migrate to `version` (1-based, exclusive of the new entry) and stamp `stamp`.
async fn db_at(version: usize, stamp: i64) -> Database {
    let db = Database::connect_in_memory().await.unwrap();
    db.pool()
        .get()
        .await
        .unwrap()
        .interact(move |conn| -> Result<(), String> {
            build_migrations()
                .to_version(conn, version)
                .map_err(|e| e.to_string())?;
            conn.pragma_update(None, "user_version", stamp)
                .map_err(|e| e.to_string())
        })
        .await
        .unwrap()
        .unwrap();
    db
}

/// A database one migration short of the new entry carries no parent column.
#[tokio::test]
async fn fixture_below_the_migration_lacks_the_column() {
    let below = parent_migration_index() - 1;
    let db = db_at(below, below as i64).await;
    assert!(
        !has_parent_column(&db).await,
        "fixture: the pre-migration schema carries no parent_session_id column"
    );
}

/// The migration itself: a database stamped one below the entry gains the
/// column when `to_latest` runs, and the stamp advances to the full count.
#[tokio::test]
async fn a_database_stamped_below_the_migration_gains_the_column() {
    let below = parent_migration_index() - 1;
    let db = db_at(below, below as i64).await;
    assert!(!has_parent_column(&db).await, "fixture: column absent");

    db.run_migrations()
        .await
        .expect("the migration must apply cleanly");

    assert!(
        has_parent_column(&db).await,
        "migration 68 must add parent_session_id"
    );
    assert_eq!(
        user_version(&db).await,
        MIGRATION_SQL.len() as i64,
        "the stamp must reach the full list length"
    );
}

/// The heal: a database stamped AT the full count but missing the column (the
/// pre-merge-list skip, #1401 class) gains it only from the post-pass heal.
#[tokio::test]
async fn a_stamp_at_latest_over_a_missing_column_is_healed() {
    let total = MIGRATION_SQL.len() as i64;
    // Schema built to the entry just below the new one, stamp claiming complete:
    // what a database stamped by a pre-merge list carries (#1401).
    let db = db_at(parent_migration_index() - 1, total).await;
    assert!(
        !has_parent_column(&db).await,
        "fixture: the stamp claims the list is complete while the column is absent"
    );
    assert_eq!(
        user_version(&db).await,
        total,
        "fixture: the stamp is already at the full count"
    );

    db.run_migrations()
        .await
        .expect("startup must survive a stamp that skipped the migration");

    assert!(
        has_parent_column(&db).await,
        "the post-pass heal must add the column the stamp skipped (#192)"
    );
    assert_eq!(user_version(&db).await, total);
}

/// The heal is idempotent and a no-op on a database that already carries the
/// column: a second boot leaves it byte-identical.
#[tokio::test]
async fn the_heal_is_a_no_op_on_a_correct_database() {
    let db = Database::connect_in_memory().await.unwrap();
    db.run_migrations().await.unwrap();
    assert!(has_parent_column(&db).await, "fresh DB carries the column");

    let healed = db
        .pool()
        .get()
        .await
        .unwrap()
        .interact(|conn| crate::db::migration_heal::heal_sessions_parent_session_id(conn))
        .await
        .unwrap()
        .unwrap();
    assert!(!healed, "the heal must not rewrite a complete schema");
}

/// The write side: a session created with a parent persists it and reads it
/// back through `SessionService` — the round trip `oc-fanout` relies on.
#[tokio::test]
async fn a_session_created_with_a_parent_reads_it_back() {
    let db = Database::connect_in_memory().await.unwrap();
    db.run_migrations().await.unwrap();
    let svc = SessionService::new(ServiceContext::new(db.pool().clone()));

    let parent = svc
        .create_session(Some("owning lane".into()))
        .await
        .unwrap();
    assert!(
        parent.parent_session_id.is_none(),
        "a top-level session carries no parent"
    );

    let child = svc
        .create_session_with_provider(
            Some("subagent: worker".into()),
            None,
            None,
            None,
            Some(parent.id),
        )
        .await
        .unwrap();
    assert_eq!(child.parent_session_id, Some(parent.id));

    let read_back = svc.get_session_required(child.id).await.unwrap();
    assert_eq!(
        read_back.parent_session_id,
        Some(parent.id),
        "the parent must survive the write and read"
    );
}

/// The repository `create` path persists the parent too, and the channel INSERT
/// path always writes NULL (a channel session is never a spawned child).
#[tokio::test]
async fn repository_inserts_persist_and_default_the_parent() {
    let db = Database::connect_in_memory().await.unwrap();
    db.run_migrations().await.unwrap();
    let repo = SessionRepository::new(db.pool().clone());

    let parent = Uuid::new_v4();
    let mut child = Session::new(Some("subagent: repo".into()), None, None);
    child.parent_session_id = Some(parent);
    repo.create(&child).await.expect("create child");

    let read_back = repo.find_by_id(child.id).await.unwrap().unwrap();
    assert_eq!(read_back.parent_session_id, Some(parent));

    // The channel path passes NULL for the parent.
    let channel = Session::new(Some("chat [chat:-1]".into()), None, None);
    let stored = repo
        .insert_or_resolve_channel(&channel, "[chat:-1]")
        .await
        .expect("insert channel session");
    assert!(
        stored.parent_session_id.is_none(),
        "a channel session must not carry a parent"
    );
    let read_channel = repo.find_by_id(stored.id).await.unwrap().unwrap();
    assert!(read_channel.parent_session_id.is_none());
}

/// The snapshot probe must know about this heal. A stamp at the full count with
/// the column absent is the ONE database `heal_sessions_parent_session_id`
/// exists for, so `heals_would_write` has to report a write and the
/// pre-migration snapshot has to run BEFORE the heal alters it.
///
/// Discriminating both ways on purpose: the fresh-DB leg proves no OTHER heal
/// fires on a complete schema (without it, the second assertion could pass on
/// someone else's precondition and prove nothing about this line), and the
/// dropped-column leg proves this one does.
#[tokio::test]
async fn the_snapshot_probe_sees_the_missing_column() {
    let db = Database::connect_in_memory().await.unwrap();
    db.run_migrations().await.unwrap();
    let total = MIGRATION_SQL.len() as i64;

    let quiet = db
        .pool()
        .get()
        .await
        .unwrap()
        .interact(move |conn| crate::db::migration_heal::heals_would_write(conn, total))
        .await
        .unwrap()
        .unwrap();
    assert!(
        !quiet,
        "a complete schema must not request a snapshot: no heal would write"
    );

    db.pool()
        .get()
        .await
        .unwrap()
        .interact(|conn| conn.execute_batch("ALTER TABLE sessions DROP COLUMN parent_session_id;"))
        .await
        .unwrap()
        .unwrap();

    let loud = db
        .pool()
        .get()
        .await
        .unwrap()
        .interact(move |conn| crate::db::migration_heal::heals_would_write(conn, total))
        .await
        .unwrap()
        .unwrap();
    assert!(
        loud,
        "a stamp at the full count over a missing parent_session_id column must \
         request a snapshot before the heal writes (#192)"
    );
}
