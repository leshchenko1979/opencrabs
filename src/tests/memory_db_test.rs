//! SQLite store schema and queries.
//!
//! Moved out of `src/memory/db.rs`: tests live under `src/tests/`,
//! never inline beside the logic they exercise (#1076).

use crate::db::WAL_JOURNAL_SIZE_LIMIT;
use crate::memory::db::*;

fn temp_store() -> (Store, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("memory.db")).unwrap();
    (store, dir)
}

#[test]
fn hash_is_sha256_hex() {
    // Same input qmd hashed for every existing memory.db row.
    let h = Store::hash_content("hello");
    assert_eq!(
        h,
        "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
    );
}

#[test]
fn roundtrip_document_and_fts() {
    let (store, _dir) = temp_store();
    let body = "# Title One\n\nthe quick brown fox jumps over the lazy dog";
    let hash = Store::hash_content(body);

    store.insert_content(&hash, body, "now").unwrap();
    store
        .insert_document("memory", "a.md", "Title One", &hash, "now", "now")
        .unwrap();

    let found = store.find_active_document("memory", "a.md").unwrap();
    assert_eq!(found.unwrap().1, hash);

    let hits = store.search_fts("\"fox\"", 10, None).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].doc.path, "a.md");
    assert!(hits[0].score > 0.0);

    // Collection filter excludes other collections.
    let brain_hits = store.search_fts("\"fox\"", 10, Some("brain")).unwrap();
    assert!(brain_hits.is_empty());

    let doc = store.get_document("memory", "a.md").unwrap().unwrap();
    assert_eq!(doc.body.as_deref(), Some(body));
}

#[test]
fn deactivate_removes_from_fts() {
    let (store, _dir) = temp_store();
    let body = "uniqueword for deactivation test";
    let hash = Store::hash_content(body);
    store.insert_content(&hash, body, "now").unwrap();
    store
        .insert_document("memory", "b.md", "", &hash, "now", "now")
        .unwrap();
    assert_eq!(
        store.search_fts("\"uniqueword\"", 10, None).unwrap().len(),
        1
    );

    store.deactivate_document("memory", "b.md").unwrap();
    assert_eq!(
        store.search_fts("\"uniqueword\"", 10, None).unwrap().len(),
        0
    );
    assert!(
        store
            .find_active_document("memory", "b.md")
            .unwrap()
            .is_none()
    );
}

#[test]
fn embedding_roundtrip_and_backfill_list() {
    let (store, _dir) = temp_store();
    let body = "embed me";
    let hash = Store::hash_content(body);
    store.insert_content(&hash, body, "now").unwrap();
    store
        .insert_document("memory", "c.md", "", &hash, "now", "now")
        .unwrap();

    // Needs embedding: no seq-0 row yet.
    let needing = store.get_hashes_needing_embedding().unwrap();
    assert_eq!(needing.len(), 1);
    assert_eq!(needing[0].0, hash);

    store.ensure_vector_table(4).unwrap();
    store
        .insert_embedding(
            &hash,
            0,
            0,
            &[0.1, 0.2, 0.3, 0.4],
            "test-model",
            "now",
            None,
        )
        .unwrap();

    assert!(store.get_hashes_needing_embedding().unwrap().is_empty());
}

#[test]
fn title_extraction() {
    assert_eq!(Store::extract_title("# Hello\nbody"), "Hello");
    assert_eq!(Store::extract_title("intro\n## Sub\nbody"), "Sub");
    assert_eq!(Store::extract_title("no heading here"), "");
}

// ─── #1062: memory.db's `-wal` carries the same cap as opencrabs.db (#354) ────

/// `memory.db` never got `PRAGMA journal_size_limit`, so #354's 64 MiB cap
/// covered only the pooled `opencrabs.db`. The store builds its own
/// `rusqlite::Connection` and never passes through `apply_pragmas`, so the cap
/// could not reach it by construction: its sidecar was bounded only by the
/// 64 MiB TRUNCATE threshold leg of the maintenance sweep.
///
/// This asserts the pragma on the store's OWN connection. The second
/// connection below is a NEGATIVE CONTROL, not evidence: it reads `-1`
/// regardless of what the store carries, because the pragma is per-connection
/// — the trap #354 documented, and the reason this test needs `store.conn()`.
#[test]
fn store_connection_carries_the_wal_journal_size_limit() {
    let (store, _dir) = temp_store();

    let limit: i64 = store
        .conn()
        .query_row("PRAGMA journal_size_limit;", [], |row| row.get(0))
        .expect("read journal_size_limit off the store's own connection");
    assert_eq!(
        limit, WAL_JOURNAL_SIZE_LIMIT as i64,
        "the memory store's write connection must carry the 64 MiB -wal cap: \
         without it memory.db's sidecar is bounded only by the sweep's \
         TRUNCATE threshold, which is the #1062 defect"
    );

    let fresh = rusqlite::Connection::open(store.db_path()).expect("second connection");
    let fresh_limit: i64 = fresh
        .query_row("PRAGMA journal_size_limit;", [], |row| row.get(0))
        .expect("read journal_size_limit off the second connection");
    assert_eq!(
        fresh_limit, -1,
        "negative control: a fresh connection defaults to -1 (unlimited), so \
         reading one would pass even against an uncapped store"
    );
}

/// The cap — not the checkpoint, and not a `VACUUM` — is what bounds
/// `memory.db-wal`, proven on the store's own connection (#1062).
///
/// Mirrors #354's `test_passive_checkpoint_caps_wal_at_journal_size_limit`.
/// The production 64 MiB is pinned by the test above; this one sets a 4 MiB
/// limit on the SAME connection, because the mechanism is size-independent and
/// a 64 MiB fixture is ~16 000 inserts, i.e. a CI timeout rather than a test.
///
/// Both halves are asserted, because the end state alone would also pass
/// against a `VACUUM`-based trim: a PASSIVE checkpoint must leave the file
/// unchanged, and the next WRITE must bring it back under the cap.
#[test]
fn store_connection_caps_wal_at_journal_size_limit() {
    const TEST_LIMIT: u64 = 4 * 1024 * 1024;

    let (store, _dir) = temp_store();
    let wal_path = wal_path_of(store.db_path());

    // Auto-checkpoint off, or SQLite folds the frames back into the main file
    // at its 1000-page watermark and the sidecar never reaches the cap. The
    // small limit goes on the same connection the store writes through.
    store
        .conn()
        .execute_batch(&format!(
            "PRAGMA wal_autocheckpoint = 0;
             PRAGMA journal_size_limit = {TEST_LIMIT};"
        ))
        .expect("configure the store connection");

    let payload = "x".repeat(4096);
    let mut written = 0usize;
    while wal_len(&wal_path) <= TEST_LIMIT && written < 8000 {
        store
            .insert_content(&format!("cap-probe-{written}"), &payload, "now")
            .expect("insert");
        written += 1;
    }
    let grown = wal_len(&wal_path);
    assert!(
        grown > TEST_LIMIT,
        "fixture must grow the sidecar past the cap: {grown} <= {TEST_LIMIT} \
         after {written} inserts"
    );

    store
        .conn()
        .execute_batch("PRAGMA wal_checkpoint(PASSIVE);")
        .expect("passive checkpoint");
    let after_checkpoint = wal_len(&wal_path);
    assert_eq!(
        after_checkpoint, grown,
        "a PASSIVE checkpoint alone must not shrink the file — the trim lands \
         on the next WRITE, not on the checkpoint ({grown} -> {after_checkpoint})"
    );

    store
        .insert_content("cap-probe-after", &payload, "now")
        .expect("write after the checkpoint");
    let after_write = wal_len(&wal_path);
    assert!(
        after_write <= TEST_LIMIT,
        "the next write on the store's connection must truncate to the cap: \
         {after_write} > {TEST_LIMIT}"
    );
}

/// `<db>` → `<db>-wal`, the sidecar SQLite keeps beside a WAL database.
fn wal_path_of(db_path: &std::path::Path) -> std::path::PathBuf {
    let mut os = db_path.as_os_str().to_os_string();
    os.push("-wal");
    std::path::PathBuf::from(os)
}

/// Bytes in the `-wal` sidecar, or 0 when it does not exist yet.
fn wal_len(wal_path: &std::path::Path) -> u64 {
    std::fs::metadata(wal_path).map(|m| m.len()).unwrap_or(0)
}
