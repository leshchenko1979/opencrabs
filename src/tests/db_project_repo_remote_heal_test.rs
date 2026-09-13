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

#[tokio::test]
async fn migration_sql_order_invariants() {
    assert!(
        MIGRATION_SQL[36].contains("ADD COLUMN origin"),
        "origin column must remain at index 36"
    );
    assert!(
        MIGRATION_SQL[45].contains("projects"),
        "projects.repo_remote must be at index 45 per chronological filename order (#1510, #209)"
    );
    assert!(
        MIGRATION_SQL[46].contains("last_origin"),
        "session_bindings_last_origin must remain at index 46 per chronological filename order"
    );
    assert!(
        MIGRATION_SQL[47].contains("active"),
        "session_seen_skills_active must remain at index 47 per chronological filename order"
    );
    assert!(
        MIGRATION_SQL[48].contains("turn_open_at"),
        "session_bindings_turn_open_at must remain at index 48 per chronological filename order (#200)"
    );
}

#[tokio::test]
async fn skip_applied_active_migration_prevents_duplicate_column_crash() {
    let db = Database::connect_in_memory().await.unwrap();
    // Simulate prod DB shape at user_version 47 with column active present and projects.repo_remote missing
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

    // Verify user_version is now 48, active exists, and repo_remote was healed
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
                "repo_remote column must be healed"
            );
        })
        .await
        .unwrap();
}
