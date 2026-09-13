//! Tests for healing `projects.repo_remote` when skipped on upstream/fork boundary (#209).

use crate::db::database::{MIGRATION_SQL, build_migrations};
use crate::db::Database;

async fn db_with_repo_remote_skipped() -> Database {
    let db = Database::connect_in_memory().await.unwrap();
    db.pool()
        .get()
        .await
        .unwrap()
        .interact(|conn| -> Result<(), String> {
            // Apply migrations up to 47 (all except repo_remote at 48)
            build_migrations()
                .to_version(conn, 47)
                .map_err(|e| e.to_string())?;
            // Stamp to latest (48) without having applied 48
            conn.pragma_update(None, "user_version", MIGRATION_SQL.len() as i64)
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
    assert!(!has_repo_remote(&db).await, "fixture must lack repo_remote column");
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
        MIGRATION_SQL[47].contains("projects"),
        "projects.repo_remote must be appended last per invariant (#1510, #209)"
    );
}
