//! Startup must survive a database whose `user_version` disagrees with its
//! schema, in EITHER direction.
//!
//! `rusqlite_migration` applies migrations by list INDEX, so `user_version` is a
//! position in a filename-sorted list and not a migration identity. Inserting a
//! migration with an earlier filename shifts every later index, and a stamp
//! written by the old list then maps onto a DIFFERENT migration. Two symptoms,
//! each measured before it was fixed:
//!
//! - the stamp lands **below** a migration that already ran, so `to_latest`
//!   replays a non-idempotent `ALTER TABLE ... ADD COLUMN` and SQLite rejects the
//!   duplicate column — startup dies, not one migration. #1401
//!   (`pending_requests.channel_thread_id`), #209 / #212
//!   (`session_seen_skills.active`).
//! - the stamp lands **above** a migration that never ran, so `to_latest` passes
//!   it by and the schema stays short, silently — #724, the v0.5.3 shape.
//!
//! `reconcile_before_migrations` runs before `to_latest` and settles both from
//! the schema. These tests drive it through the real `Database::run_migrations`,
//! and they read migration positions from the live list rather than hard-coding
//! them, so a future re-sort cannot make a test agree with itself.

use crate::db::Database;
use crate::db::database::{MIGRATION_SQL, build_migrations};

/// Arm A as MEASURED — the live client shape, `user_version` 45, schema objects
/// and stamp only (no rows). Committed rather than reconstructed: see the
/// fixture's own header for why a rebuild from the current list cannot stand in
/// for it.
const ARM_A_SQL: &str = include_str!("fixtures/724-arm-a.sql");

/// 1-based position of the migration containing `marker`, read from the live
/// list.
fn index_1based(marker: &str) -> usize {
    MIGRATION_SQL
        .iter()
        .position(|sql| sql.contains(marker))
        .unwrap_or_else(|| panic!("no migration contains {marker:?}"))
        + 1
}

fn thread_id_index() -> usize {
    index_1based("ADD COLUMN channel_thread_id")
}

fn repo_remote_index() -> usize {
    index_1based("ADD COLUMN repo_remote")
}

fn cron_trigger_index() -> usize {
    index_1based("ADD COLUMN trigger_cmd")
}

fn exit_code_index() -> usize {
    index_1based("ADD COLUMN exit_code")
}

async fn has_column(db: &Database, table: &str, column: &str) -> bool {
    let (table, column) = (table.to_owned(), column.to_owned());
    db.pool()
        .get()
        .await
        .unwrap()
        .interact(move |conn| crate::db::migration_heal::has_column(conn, &table, &column))
        .await
        .unwrap()
        .unwrap()
}

async fn has_table(db: &Database, table: &str) -> bool {
    let table = table.to_owned();
    db.pool()
        .get()
        .await
        .unwrap()
        .interact(move |conn| {
            conn.query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?1",
                [table],
                |r| r.get::<_, i64>(0),
            )
            .map(|n| n > 0)
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

/// Run `reconcile_before_migrations` at the connection's current stamp, the way
/// `Database::run_migrations` does.
async fn reconcile(db: &Database) -> bool {
    db.pool()
        .get()
        .await
        .unwrap()
        .interact(|conn| {
            let version: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
            crate::db::migration_heal::reconcile_before_migrations(conn, version)
        })
        .await
        .unwrap()
        .unwrap()
}

async fn execute(db: &Database, sql: &str) {
    let sql = sql.to_owned();
    db.pool()
        .get()
        .await
        .unwrap()
        .interact(move |conn| conn.execute_batch(&sql))
        .await
        .unwrap()
        .unwrap()
}

/// A database migrated to `version`, then stamped back to `stamp` — the shape a
/// re-sorted migration list leaves behind.
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

/// (a) One behind, effect PRESENT: the stamp must clear the migration, or
/// `to_latest` replays the `ALTER` and startup dies (#1401).
#[tokio::test]
async fn a_stamp_one_behind_a_present_effect_is_stamped_past() {
    let index = thread_id_index();
    let db = db_at(index, index as i64 - 1).await;
    assert!(
        has_column(&db, "pending_requests", "channel_thread_id").await,
        "fixture: the migration already ran"
    );

    assert!(
        reconcile(&db).await,
        "a stamp below an applied migration is a reconciliation write"
    );
    assert_eq!(
        user_version(&db).await,
        index as i64,
        "the stamp must clear the migration that already ran"
    );

    db.run_migrations()
        .await
        .expect("startup must survive a migration that already ran");
    assert!(has_column(&db, "pending_requests", "channel_thread_id").await);
    assert_eq!(user_version(&db).await, MIGRATION_SQL.len() as i64);
}

/// (b) One behind, effect ABSENT: the stamp must NOT move. Stamping past here
/// would silently lose the column the migration exists to add.
#[tokio::test]
async fn a_stamp_one_behind_an_absent_effect_is_refused_not_skipped() {
    let index = thread_id_index();
    let db = db_at(index - 1, index as i64 - 1).await;
    assert!(
        !has_column(&db, "pending_requests", "channel_thread_id").await,
        "fixture: the migration has not run"
    );

    assert!(
        !reconcile(&db).await,
        "a stamp that is honest about what ran is not a reconciliation write"
    );
    assert_eq!(
        user_version(&db).await,
        index as i64 - 1,
        "the stamp must be left where it is"
    );

    db.run_migrations()
        .await
        .expect("startup must apply the migration it still owes");
    assert!(
        has_column(&db, "pending_requests", "channel_thread_id").await,
        "the migration must have been applied, not skipped"
    );
}

/// (c) The v0.5.3 shape: the stamp says 45 while two LATER migrations already
/// ran, four and six positions up the list, with unapplied migrations between
/// them. Every hazard must be cleared and every gap filled (#724).
#[tokio::test]
async fn the_v053_shape_is_stamped_past_every_hazard() {
    let stamp = 45i64;
    let db = db_at(stamp as usize, stamp).await;

    let repo_remote = repo_remote_index();
    let cron_trigger = cron_trigger_index();
    assert!(
        repo_remote > stamp as usize && cron_trigger > stamp as usize,
        "fixture: both hazards sit above the stamp"
    );
    for index in [repo_remote, cron_trigger] {
        execute(&db, MIGRATION_SQL[index - 1]).await;
    }

    // The interleaving that defeated the old guard: hazards above the stamp,
    // with unapplied migrations BETWEEN them.
    assert!(has_column(&db, "projects", "repo_remote").await);
    assert!(has_column(&db, "cron_jobs", "trigger_cmd").await);
    assert!(
        !has_column(&db, "session_bindings", "last_origin").await,
        "fixture: the gap between the two hazards is unapplied"
    );

    db.run_migrations()
        .await
        .expect("startup must survive a stamp below two applied migrations");

    assert_eq!(user_version(&db).await, MIGRATION_SQL.len() as i64);
    assert!(
        has_column(&db, "projects", "repo_remote").await,
        "the hazard must be stepped over, not replayed"
    );
    assert!(
        has_column(&db, "session_bindings", "last_origin").await,
        "a migration the too-high stamp used to skip must be applied"
    );
    assert!(has_column(&db, "session_bindings", "turn_open_at").await);
    assert!(has_column(&db, "session_seen_skills", "active").await);
    assert!(has_column(&db, "channel_messages", "ship_plane").await);
    assert!(has_table(&db, "pending_tombstones").await);
}

/// (d) A current database is not a reconciliation target, across repeat boots.
#[tokio::test]
async fn a_healthy_database_is_left_alone_across_repeat_boots() {
    let db = Database::connect_in_memory().await.unwrap();
    db.run_migrations().await.unwrap();
    let version = user_version(&db).await;
    assert_eq!(version, MIGRATION_SQL.len() as i64);

    assert!(
        !reconcile(&db).await,
        "a current database has nothing to reconcile"
    );

    db.run_migrations().await.unwrap();
    assert_eq!(user_version(&db).await, version);
    assert!(has_column(&db, "pending_requests", "channel_thread_id").await);
    assert!(has_table(&db, "notify_queue").await);
}

/// Every schema object a database carries: tables, indexes, and (table, column)
/// pairs, each sorted. `sqlite_%` internals are the engine's, not the schema's.
#[derive(Debug, PartialEq, Eq)]
struct SchemaObjects {
    tables: Vec<String>,
    indexes: Vec<String>,
    columns: Vec<(String, String)>,
}

async fn schema_objects(db: &Database) -> SchemaObjects {
    db.pool()
        .get()
        .await
        .unwrap()
        .interact(|conn| -> rusqlite::Result<SchemaObjects> {
            let names = |kind: &str| -> rusqlite::Result<Vec<String>> {
                conn.prepare(&format!(
                    "SELECT name FROM sqlite_master WHERE type = '{kind}' \
                     AND name NOT LIKE 'sqlite_%' ORDER BY name"
                ))?
                .query_map([], |r| r.get(0))?
                .collect()
            };
            let tables = names("table")?;
            let indexes = names("index")?;
            let mut columns: Vec<(String, String)> = Vec::new();
            for table in &tables {
                let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
                let mut rows = stmt.query([])?;
                while let Some(row) = rows.next()? {
                    columns.push((table.clone(), row.get(1)?));
                }
            }
            columns.sort();
            Ok(SchemaObjects {
                tables,
                indexes,
                columns,
            })
        })
        .await
        .unwrap()
        .unwrap()
}

/// The fill-below pass, on the shape that actually shipped.
///
/// Arm A is the live client database: `user_version` 45 while two LATER
/// migrations already ran (so `to_latest` replays an `ALTER` and startup dies),
/// and the migrations the v0.5.3 list never reached are absent (so a stamp-only
/// fix would leave the schema short, in silence). Booting it must land on the
/// SAME schema as a database migrated from zero — object for object, tables,
/// indexes and columns — because "it started" is not the claim being made here.
#[tokio::test]
async fn the_arm_a_shape_heals_to_the_schema_of_a_fresh_database() {
    let db = Database::connect_in_memory().await.unwrap();
    execute(&db, ARM_A_SQL).await;
    assert_eq!(
        user_version(&db).await,
        45,
        "fixture: Arm A is stamped 45 with later migrations already applied"
    );

    db.run_migrations()
        .await
        .expect("startup must survive the Arm A shape");

    let fresh = Database::connect_in_memory().await.unwrap();
    fresh.run_migrations().await.unwrap();

    let healed = schema_objects(&db).await;
    let reference = schema_objects(&fresh).await;
    assert!(
        !reference.tables.is_empty() && !reference.columns.is_empty(),
        "the reference database must have a schema for this comparison to mean anything"
    );
    assert_eq!(
        healed.tables, reference.tables,
        "healed Arm A must carry the same TABLES as a fresh database"
    );
    assert_eq!(
        healed.indexes, reference.indexes,
        "healed Arm A must carry the same INDEXES as a fresh database"
    );
    assert_eq!(
        healed.columns, reference.columns,
        "healed Arm A must carry the same COLUMNS as a fresh database"
    );

    // The named assertion target, stated directly so a failure says WHICH object
    // the v0.5.3 list never created rather than just "the sets differ".
    for table in ["pending_tombstones", "whatsapp_newsletter_cursors"] {
        assert!(
            healed.tables.iter().any(|t| t == table),
            "{table} must be present"
        );
    }
    for index in [
        "idx_pending_tombstones_session",
        "idx_session_seen_skills_session",
    ] {
        assert!(
            healed.indexes.iter().any(|i| i == index),
            "{index} must be present"
        );
    }
    for (table, column) in [
        ("channel_messages", "ship_plane"),
        ("session_bindings", "last_origin"),
        ("session_bindings", "turn_open_at"),
        ("session_seen_skills", "active"),
    ] {
        assert!(
            healed
                .columns
                .iter()
                .any(|(t, c)| t == table && c == column),
            "{table}.{column} must be present"
        );
    }
}

/// The pre-migration snapshot predicate must fire for exactly the boots that
/// rewrite the schema (#714).
///
/// `needs_pre_migration_snapshot` short-circuits on `user_version <
/// migration_count` and otherwise asks `heals_would_write`. A reconciliation
/// boot at the CURRENT stamp is therefore invisible to the snapshot unless the
/// predicate knows about the pass — which is the whole point of extending it
/// here.
#[tokio::test]
async fn a_reconciliation_boot_is_worth_a_pre_migration_snapshot() {
    // Arm A: stamped 45, so the count test alone would already snapshot it.
    let arm_a = Database::connect_in_memory().await.unwrap();
    execute(&arm_a, ARM_A_SQL).await;
    assert!(
        heals_would_write(&arm_a).await,
        "the Arm A shape rewrites the schema and must be snapshotted"
    );

    // The case the count test misses: a database whose stamp says LATEST while
    // its schema is short — the silent half of the fault. It reaches the
    // predicate with user_version == migration_count.
    //
    // Arm A already carries the two migrations that shifted furthest up (it
    // applied them under the v0.5.3 list), so raising its stamp to the list
    // length is enough to build the shape: the stamp says done while
    // `channel_messages.ship_plane`, `pending_tombstones` and `last_origin` are
    // still missing.
    let silent = Database::connect_in_memory().await.unwrap();
    execute(&silent, ARM_A_SQL).await;
    execute(
        &silent,
        &format!("PRAGMA user_version = {}", MIGRATION_SQL.len()),
    )
    .await;
    assert_eq!(
        user_version(&silent).await,
        MIGRATION_SQL.len() as i64,
        "fixture: the stamp claims the list is complete"
    );
    assert!(
        !has_column(&silent, "session_bindings", "last_origin").await,
        "fixture: the schema is short despite the stamp"
    );
    assert!(
        heals_would_write(&silent).await,
        "a stamp at latest over a short schema still rewrites, so it is still a snapshot"
    );

    // And the control: a genuinely current database is not a snapshot.
    let healthy = Database::connect_in_memory().await.unwrap();
    healthy.run_migrations().await.unwrap();
    assert!(
        !heals_would_write(&healthy).await,
        "a database at the current version with a complete schema is not a reconciliation target"
    );
}

/// The #763 column must reach a database whose stamp already claims the list is
/// complete.
///
/// `20261002000001_add_tool_executions_exit_code.sql` sits near the list's
/// tail, so an upstream merge that inserts an earlier filename below it moves
/// its index and a database stamped against the pre-merge list skips it in
/// silence — the #1401 class in the direction `to_latest` cannot see. The
/// post-pass heal is the column's only cover there.
#[tokio::test]
async fn a_stamp_at_latest_over_a_missing_exit_code_column_is_healed() {
    let total = MIGRATION_SQL.len();
    // Migrated to the entry just below #763's (so the column is absent) but
    // stamped at the full length — what the pre-merge list leaves behind.
    // Located by name, not `total - 1`: #763 was the last entry when this test
    // was written, and a later append silently moves the cut point past it —
    // the same drift this file exists to guard.
    let db = db_at(exit_code_index() - 1, total as i64).await;
    assert!(
        !has_column(&db, "tool_executions", "exit_code").await,
        "fixture: the pre-#763 schema carries no exit_code column"
    );
    assert!(
        heals_would_write(&db).await,
        "a stamp at latest over a short tool_executions schema still rewrites, so it is still \
         worth a pre-migration snapshot"
    );

    db.run_migrations()
        .await
        .expect("startup must survive a stamp that skipped the #763 migration");
    assert!(
        has_column(&db, "tool_executions", "exit_code").await,
        "the post-pass heal must add the column the stamp skipped (#763)"
    );
    assert_eq!(user_version(&db).await, total as i64);
    assert!(
        !heals_would_write(&db).await,
        "a healed database is not a rewrite target on the next boot"
    );
}

/// The fresh-DB leg of the #763 acceptance: a database built from the live list
/// carries the column with no heal involved.
#[tokio::test]
async fn a_fresh_database_carries_the_exit_code_column() {
    let db = Database::connect_in_memory().await.unwrap();
    db.run_migrations().await.unwrap();
    assert!(
        has_column(&db, "tool_executions", "exit_code").await,
        "the #763 migration must run on a fresh database"
    );
}

/// The predicate is read through the connection, the way
/// `needs_pre_migration_snapshot` reads it.
async fn heals_would_write(db: &Database) -> bool {
    db.pool()
        .get()
        .await
        .unwrap()
        .interact(|conn| {
            let version: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
            crate::db::migration_heal::heals_would_write(conn, version)
        })
        .await
        .unwrap()
        .unwrap()
}
