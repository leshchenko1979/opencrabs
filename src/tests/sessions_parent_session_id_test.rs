//! `sessions.parent_session_id` — supplied by its post-pass heal, NOT by a
//! migration entry (#192).
//!
//! A sub-agent's child session must record the lane that spawned it, so a
//! commit trailered with a child session id can be resolved back to the lane
//! instead of notifying a black hole.
//!
//! **Why there is no migration.** A new entry appended to [`MIGRATION_SQL`]
//! bumps `Database::MIGRATION_COUNT`, and `needs_pre_migration_snapshot`
//! returns `true` for every profile whose `user_version` sits below the new
//! count. That re-arms the pre-migration `VACUUM INTO` — 155 s for default's
//! 637 MB and ~331 s for ops' 1.31 GB — behind `tg_worker_default.py`'s 90 s
//! CLI cap, which is the loop that filled the disk (#1034). The heal is a
//! `PRAGMA table_info` on a healthy boot and an `ALTER` only on the database
//! that lacks the column, so the column lands with no new index and no
//! snapshot obligation.
//!
//! The trade this makes is explicit: a fresh database relies on the heal rather
//! than on a migration, which is exactly how `heal_cron_deliver_mode` already
//! works in production. `no_migration_carries_the_column_and_the_heal_supplies_it`
//! pins both halves of that decision so a later merge cannot quietly re-add the
//! entry and re-arm the snapshot.

use crate::db::Database;
use crate::db::database::MIGRATION_SQL;
use crate::db::models::Session;
use crate::db::repository::SessionRepository;
use crate::services::{ServiceContext, SessionService};
use uuid::Uuid;

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

/// Drop the column so the next `run_migrations` sees the database the heal
/// exists for: a stamp at the full count over a schema that lacks the column.
async fn drop_parent_column(db: &Database) {
    db.pool()
        .get()
        .await
        .unwrap()
        .interact(|conn| conn.execute_batch("ALTER TABLE sessions DROP COLUMN parent_session_id;"))
        .await
        .unwrap()
        .unwrap();
}

/// #192 option 2: the column arrives from the heal, and NO migration carries it.
///
/// Both halves matter. The first assertion is what keeps `MIGRATION_COUNT` at
/// 67 — the decision this whole file exists to protect, since a new index is
/// what re-arms the pre-migration snapshot on every profile. The second proves
/// the heal genuinely covers the fresh-database path that the migration would
/// otherwise have covered.
#[tokio::test]
async fn no_migration_carries_the_column_and_the_heal_supplies_it() {
    assert!(
        !MIGRATION_SQL
            .iter()
            .any(|sql| sql.contains("ADD COLUMN parent_session_id")),
        "no migration may add parent_session_id: a new index raises \
         MIGRATION_COUNT and re-arms the pre-migration snapshot on every \
         profile (#192, #1034)"
    );

    let db = Database::connect_in_memory().await.unwrap();
    db.run_migrations().await.unwrap();
    assert!(
        has_parent_column(&db).await,
        "the post-pass heal must supply the column on a fresh database"
    );
}

/// The heal: a database stamped at the full count whose column is absent — the
/// #1401 shape, and now also every pre-existing database on this box — gains it
/// from `heal_sessions_parent_session_id` at startup.
#[tokio::test]
async fn a_stamp_at_latest_over_a_missing_column_is_healed() {
    let db = Database::connect_in_memory().await.unwrap();
    db.run_migrations().await.unwrap();
    drop_parent_column(&db).await;
    assert!(
        !has_parent_column(&db).await,
        "fixture: the column is gone while the stamp still claims the list is complete"
    );

    db.run_migrations()
        .await
        .expect("startup must survive a schema that is short one column");

    assert!(
        has_parent_column(&db).await,
        "the post-pass heal must add the column the stamp skipped (#192)"
    );
}

/// The heal is idempotent and a no-op on a database that already carries the
/// column: a second boot leaves it untouched.
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

    drop_parent_column(&db).await;

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
