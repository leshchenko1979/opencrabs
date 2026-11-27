use crate::config::profile::with_home_override;
use crate::memory::db::Store;
use crate::memory::store::*;

#[test]
fn test_memory_dir() {
    let dir = memory_dir();
    assert!(dir.to_string_lossy().contains("memory"));
}

#[test]
fn test_index_and_search_integration() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("test.db");
    let store = Store::open(&db_path).unwrap();

    let body = "# Session\nFixed the authentication bug in login flow";
    let hash = Store::hash_content(body);
    let now = "2024-01-01T00:00:00";
    let title = Store::extract_title(body);

    store.insert_content(&hash, body, now).unwrap();
    store
        .insert_document("test", "2024-01-01.md", &title, &hash, now, now)
        .unwrap();

    let results = store
        .search_fts("\"authentication\"", 5, Some("test"))
        .unwrap();
    assert!(!results.is_empty());

    let found = store.find_active_document("test", "2024-01-01.md").unwrap();
    assert!(found.is_some());
    let (_id, found_hash, _title) = found.unwrap();
    assert_eq!(found_hash, hash);
}

#[test]
fn test_vector_search_returns_results() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("vec.db");
    let store = Store::open(&db_path).unwrap();
    store.ensure_vector_table(768).unwrap();

    let body = "# Debug\nTracked down the memory leak in the render loop";
    let hash = Store::hash_content(body);
    let title = Store::extract_title(body);
    let now = "2024-01-01T00:00:00";

    store.insert_content(&hash, body, now).unwrap();
    store
        .insert_document("memory", "2024-01-01.md", &title, &hash, now, now)
        .unwrap();

    // Insert a fake 768-dim embedding
    let mut embedding = vec![0.0f32; 768];
    embedding[0] = 1.0;
    embedding[1] = 0.5;
    store
        .insert_embedding(&hash, 0, 0, &embedding, "test-model", now, None)
        .unwrap();

    // Query with a similar vector — should find the document
    let mut query_emb = vec![0.0f32; 768];
    query_emb[0] = 0.9;
    query_emb[1] = 0.6;

    let results =
        crate::memory::vector_search::search_chunks(&db_path, &query_emb, 5, None).unwrap();
    assert!(!results.is_empty());
    assert_eq!(results[0].title, "Debug");
}

#[test]
fn test_vector_search_empty_when_no_embeddings() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("novec.db");
    let store = Store::open(&db_path).unwrap();
    store.ensure_vector_table(768).unwrap();

    let body = "# Session\nSome content without embeddings";
    let hash = Store::hash_content(body);
    let title = Store::extract_title(body);
    let now = "2024-01-01T00:00:00";

    store.insert_content(&hash, body, now).unwrap();
    store
        .insert_document("memory", "2024-01-01.md", &title, &hash, now, now)
        .unwrap();

    // Vector search with no embeddings stored — should return empty
    let query_emb = vec![0.1f32; 768];
    let results =
        crate::memory::vector_search::search_chunks(&db_path, &query_emb, 5, None).unwrap();
    assert!(results.is_empty());
}

#[test]
fn test_hashes_needing_embedding() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("need.db");
    let store = Store::open(&db_path).unwrap();
    store.ensure_vector_table(768).unwrap();

    let body1 = "# First\nDocument without embedding";
    let hash1 = Store::hash_content(body1);
    let now = "2024-01-01T00:00:00";

    store.insert_content(&hash1, body1, now).unwrap();
    store
        .insert_document(
            "memory",
            "first.md",
            &Store::extract_title(body1),
            &hash1,
            now,
            now,
        )
        .unwrap();

    let body2 = "# Second\nDocument with embedding";
    let hash2 = Store::hash_content(body2);

    store.insert_content(&hash2, body2, now).unwrap();
    store
        .insert_document(
            "memory",
            "second.md",
            &Store::extract_title(body2),
            &hash2,
            now,
            now,
        )
        .unwrap();

    // Embed only the second document
    let emb = vec![0.1f32; 768];
    store
        .insert_embedding(&hash2, 0, 0, &emb, "test", now, None)
        .unwrap();

    // Only the first should need embedding
    let needing = store.get_hashes_needing_embedding().unwrap();
    assert_eq!(needing.len(), 1);
    assert_eq!(needing[0].0, hash1);
}

#[test]
fn test_rrf_merges_fts_and_vector() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("rrf.db");
    let store = Store::open(&db_path).unwrap();
    store.ensure_vector_table(768).unwrap();

    let now = "2024-01-01T00:00:00";

    // Doc A: matches FTS for "authentication", has embedding
    let body_a = "# Auth Fix\nFixed the authentication bug in the login flow";
    let hash_a = Store::hash_content(body_a);
    store.insert_content(&hash_a, body_a, now).unwrap();
    store
        .insert_document(
            "memory",
            "auth.md",
            &Store::extract_title(body_a),
            &hash_a,
            now,
            now,
        )
        .unwrap();
    let mut emb_a = vec![0.0f32; 768];
    emb_a[0] = 1.0;
    store
        .insert_embedding(&hash_a, 0, 0, &emb_a, "test", now, None)
        .unwrap();

    // Doc B: different content, different embedding direction
    let body_b = "# Refactor\nRefactored the database connection pooling layer";
    let hash_b = Store::hash_content(body_b);
    store.insert_content(&hash_b, body_b, now).unwrap();
    store
        .insert_document(
            "memory",
            "refactor.md",
            &Store::extract_title(body_b),
            &hash_b,
            now,
            now,
        )
        .unwrap();
    let mut emb_b = vec![0.0f32; 768];
    emb_b[1] = 1.0;
    store
        .insert_embedding(&hash_b, 0, 0, &emb_b, "test", now, None)
        .unwrap();

    // FTS finds doc A for "authentication"
    let fts = store
        .search_fts("\"authentication\"", 5, Some("memory"))
        .unwrap();
    assert!(!fts.is_empty());
    assert_eq!(fts[0].doc.title, "Auth Fix");

    // Vector search close to emb_a finds doc A first
    let mut q = vec![0.0f32; 768];
    q[0] = 0.9;
    let vec_results =
        crate::memory::vector_search::search_chunks(&db_path, &q, 5, Some("memory")).unwrap();
    assert!(!vec_results.is_empty());
    assert_eq!(vec_results[0].title, "Auth Fix");

    // RRF combines both — doc A should rank highest (appears in both lists)

    use crate::memory::search::hybrid_search_rrf;
    let fts_tuples: Vec<_> = fts
        .iter()
        .map(|r| {
            (
                r.doc.path.clone(),
                r.doc.path.clone(),
                r.doc.title.clone(),
                body_a.to_string(),
            )
        })
        .collect();
    let vec_tuples: Vec<_> = vec_results
        .iter()
        .map(|r| {
            let body = if r.title == "Auth Fix" {
                body_a
            } else {
                body_b
            };
            (
                r.path.clone(),
                r.path.clone(),
                r.title.clone(),
                body.to_string(),
            )
        })
        .collect();

    let rrf = hybrid_search_rrf(fts_tuples, vec_tuples, 60);
    assert!(!rrf.is_empty());
    assert_eq!(rrf[0].title, "Auth Fix");
    assert!(rrf[0].score > 0.0);
}

// ─── #1062: the placeholder sweep's own write connection carries the -wal cap ──

/// `clear_skipped_placeholders` opens its OWN read-write connection and issues
/// two DELETEs, so it is a writer that never passes through `apply_pragmas`
/// and never touches the `Store` handle. A fix that capped only `Store::open`
/// would leave this one keeping the WAL ratchet alive (#1062).
///
/// Driven directly through [`placeholder_sweep_conn`] rather than through
/// `clear_skipped_placeholders`: the pragma has to be read off the connection
/// that writes, and a second connection to the same file reads `-1` no matter
/// what this one carries (`journal_size_limit` is per-connection).
#[test]
fn placeholder_sweep_connection_carries_the_wal_journal_size_limit() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("memory.db");
    // A real schema first, so the connection below opens a store, not a
    // zero-byte file.
    {
        let _store = Store::open(&db_path).unwrap();
    }

    let conn = placeholder_sweep_conn(&db_path).expect("sweep connection");
    let limit: i64 = conn
        .query_row("PRAGMA journal_size_limit;", [], |row| row.get(0))
        .expect("read journal_size_limit off the sweep connection");
    assert_eq!(
        limit,
        crate::db::WAL_JOURNAL_SIZE_LIMIT as i64,
        "the placeholder sweep writes to memory.db on its own connection, so it \
         must carry the same 64 MiB cap the store does"
    );
}

/// End to end through the public path: the sweep still clears the placeholders,
/// now on a capped connection. Proves the helper is actually wired into
/// `clear_skipped_placeholders` rather than sitting unused beside it.
///
/// `clear_skipped_placeholders` resolves `<profile home>/memory/memory.db`, so
/// the home is scoped with `with_home_override` — task-local, therefore
/// invisible to a parallel test, unlike pointing `$HOME` at a tempdir (#912).
#[test]
fn clear_skipped_placeholders_still_clears_on_a_capped_connection() {
    let home = tempfile::tempdir().unwrap();
    with_home_override(home.path().to_path_buf(), || {
        let db_path = memory_dir().join("memory.db");
        let store = Store::open(&db_path).unwrap();
        store.ensure_vector_table(4).unwrap();

        let body = "a document the pre-chunking embedder refused";
        let hash = Store::hash_content(body);
        store.insert_content(&hash, body, "now").unwrap();
        store
            .insert_document("memory", "big.md", "big", &hash, "now", "now")
            .unwrap();
        // The placeholder shape the old embedder wrote: a seq-0 row whose model
        // is the marker, plus the empty vector blob the sweep must drop first.
        store
            .insert_embedding(&hash, 0, 0, &[], "skipped-too-large", "now", None)
            .unwrap();

        assert_eq!(
            clear_skipped_placeholders().expect("sweep"),
            1,
            "the sweep must remove the placeholder row it was pointed at"
        );
        assert_eq!(
            clear_skipped_placeholders().expect("second sweep"),
            0,
            "the sweep is idempotent: nothing left to clear"
        );
    });
}
