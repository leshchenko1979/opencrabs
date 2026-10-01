//! Tests for healing `projects.repo_remote` when skipped on upstream/fork boundary (#209).

use crate::db::Database;
use crate::db::database::{MIGRATION_SQL, build_migrations};

async fn db_with_repo_remote_skipped() -> Database {
    let db = Database::connect_in_memory().await.unwrap();
    db.pool()
        .get()
        .await
        .unwrap()
        .interact(|conn| -> Result<(), String> {
            // Apply migrations up to 45 (before repo_remote at 46 / index 45)
            build_migrations()
                .to_version(conn, 45)
                .map_err(|e| e.to_string())?;
            // Stamp to 47 (simulating a DB already migrated past index 45 on fork)
            conn.pragma_update(None, "user_version", 47)
                .map_err(|e| e.to_string())
        })
        .await
        .unwrap()
        .unwrap();
    db
}

async fn has_repo_remote(db: &Database) -> bool {
    db.pool()
        .get()
        .await
        .unwrap()
        .interact(|conn| crate::db::migration_heal::has_column(conn, "projects", "repo_remote"))
        .await
        .unwrap()
        .unwrap()
}

#[tokio::test]
async fn fixture_reproduces_skipped_repo_remote() {
    let db = db_with_repo_remote_skipped().await;
    assert!(
        !has_repo_remote(&db).await,
        "fixture must lack repo_remote column"
    );
}

#[tokio::test]
async fn heal_adds_missing_repo_remote_and_is_idempotent() {
    let db = db_with_repo_remote_skipped().await;

    // Run heal
    let healed = db
        .pool()
        .get()
        .await
        .unwrap()
        .interact(|conn| crate::db::migration_heal::heal_project_repo_remote(conn))
        .await
        .unwrap()
        .unwrap();

    assert!(healed, "heal must report true when column was missing");
    assert!(has_repo_remote(&db).await, "column must exist after heal");

    // Second run is idempotent
    let second = db
        .pool()
        .get()
        .await
        .unwrap()
        .interact(|conn| crate::db::migration_heal::heal_project_repo_remote(conn))
        .await
        .unwrap()
        .unwrap();

    assert!(!second, "second heal run must be a no-op");
}

/// The declared effects are keyed by SQL MARKER, not by index, so this is not
/// what keeps them correct — `declared_effects_resolve_to_the_measured_indices`
/// in `migration_heal` is. What this pins is the ORDER the markers sit in, so a
/// migration inserted with an earlier filename fails HERE, loudly, instead of
/// silently shifting every position the #724 measurement was taken at.
#[test]
fn migration_sql_order_invariants() {
    let cases: [(usize, &str, &str); 8] = [
        (36, "ADD COLUMN origin", "pending_requests.origin"),
        (
            43,
            "ADD COLUMN channel_thread_id",
            "pending_requests.channel_thread_id (#1401)",
        ),
        (44, "ADD COLUMN epoch", "session_seen_skills.epoch"),
        (
            45,
            "ADD COLUMN repo_remote",
            "projects.repo_remote (#1510, #209)",
        ),
        (
            46,
            "ADD COLUMN last_origin",
            "session_bindings.last_origin",
        ),
        (
            47,
            "ADD COLUMN active",
            "session_seen_skills.active (#209, #212)",
        ),
        (
            48,
            "ADD COLUMN turn_open_at",
            "session_bindings.turn_open_at (#200)",
        ),
        (50, "ADD COLUMN trigger_cmd", "cron_jobs.trigger_cmd"),
    ];
    for (index, needle, what) in cases {
        assert!(
            MIGRATION_SQL[index].contains(needle),
            "{what} must stay at index {index} of MIGRATION_SQL: the list is \
             filename-sorted, so inserting an earlier migration shifts it"
        );
    }
}

/// A prod DB stamped 47 with `session_seen_skills.active` already present and
/// `projects.repo_remote` missing: the stamp sits ON a migration that partly
/// ran, and BELOW one that did not — both directions of the #724 fault at once.
#[tokio::test]
async fn a_stamp_on_a_partly_applied_migration_boots() {
    let db = Database::connect_in_memory().await.unwrap();
    // The prod shape at stamp 47: `active` present, `repo_remote` missing.
    db.pool()
        .get()
        .await
        .unwrap()
        .interact(|conn| -> Result<(), String> {
            // Apply migrations up to 45 (before repo_remote and last_origin)
            build_migrations()
                .to_version(conn, 45)
                .map_err(|e| e.to_string())?;
            // Add session_bindings.last_origin and session_seen_skills.active manually
            conn.execute_batch(
                "ALTER TABLE session_bindings ADD COLUMN last_origin TEXT; \
                 ALTER TABLE session_seen_skills ADD COLUMN active INTEGER NOT NULL DEFAULT 0;",
            )
            .map_err(|e| e.to_string())?;
            // Stamp user_version to 47 (exact state of prod DB before 6fb2657e)
            conn.pragma_update(None, "user_version", 47)
                .map_err(|e| e.to_string())
        })
        .await
        .unwrap()
        .unwrap();

    // run_migrations() must succeed without crashing on duplicate column active
    db.run_migrations()
        .await
        .expect("run_migrations must succeed on prod DB at stamp 47");

    // Verify user_version is now stamped to the latest migration, active exists, and repo_remote was healed
    db.pool()
        .get()
        .await
        .unwrap()
        .interact(|conn| {
            let version: i64 = conn
                .pragma_query_value(None, "user_version", |r| r.get(0))
                .unwrap();
            assert_eq!(
                version,
                crate::db::Database::MIGRATION_COUNT as i64,
                "user_version must be stamped to latest migration"
            );
            assert!(
                crate::db::migration_heal::has_column(conn, "session_seen_skills", "active")
                    .unwrap(),
                "active column must exist"
            );
            assert!(
                crate::db::migration_heal::has_column(conn, "projects", "repo_remote").unwrap(),
                "the migration below the stamp must be applied"
            );
            assert!(
                crate::db::migration_heal::has_column(conn, "session_seen_skills", "loaded_mtime")
                    .unwrap(),
                "the other half of the partly applied migration must be filled in"
            );
            assert!(
                crate::db::migration_heal::has_column(conn, "session_bindings", "last_origin")
                    .unwrap(),
                "an applied migration's object must not be replayed"
            );
        })
        .await
        .unwrap();
}
