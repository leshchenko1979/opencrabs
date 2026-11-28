//! Store — the memory database, owned by OpenCrabs.
//!
//! This replaced the `qmd` crate (qntx/qmd 0.3.2), which had not published a
//! release since Feb 2026 while every defect it ever shipped was found and
//! fixed on our side: byte-index chunking that panicked on multi-byte chars
//! (#1002), vector search that only ever saw chunk 0 (#998), doc-level-only
//! FTS (#1000). The audit (2026-08-12) mapped the entire surface we used —
//! 13 Store methods — and this file is that surface, nothing more: no rerank,
//! no generation, no collections admin, no llm_cache.
//!
//! The schema is qmd's verbatim on purpose. Every existing `memory.db` in the
//! wild was created by qmd, and `vector_search.rs` reads these tables
//! directly, so the DDL below IS the migration story: there isn't one.

use rusqlite::{Connection, OptionalExtension, params};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

/// One indexed document row plus its body when loaded.
#[derive(Debug, Clone)]
pub struct DocumentResult {
    pub collection_name: String,
    pub path: String,
    pub display_path: String,
    pub title: String,
    pub hash: String,
    pub modified_at: String,
    pub body: Option<String>,
}

/// A scored search hit.
#[derive(Debug, Clone)]
pub struct SearchResult {
    pub doc: DocumentResult,
    pub score: f64,
}

/// How much of the store is vectorised (#1067).
///
/// `documents_unembedded` and `last_embedded_at` are the two numbers that make
/// a stalled backfill visible: a store with 65 of 66 documents waiting and a
/// last-embedded date three months old is broken, and nothing said so before.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VectorStats {
    /// Active documents in the index.
    pub documents_active: usize,
    /// Active documents with no vector row at all.
    pub documents_unembedded: usize,
    /// Rows in `content_vectors`, one per embedded chunk.
    pub vector_rows: usize,
    /// UTC timestamp of the most recent embedding, if any.
    pub last_embedded_at: Option<String>,
}

/// Report of a memory database garbage collection sweep (#241).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MemoryGcReport {
    /// Number of orphaned vector metadata rows pruned from `content_vectors`.
    pub orphaned_vectors_pruned: usize,
    /// Number of orphaned vector embedding blobs pruned from `vectors_vec`.
    pub orphaned_embeddings_pruned: usize,
    /// Number of unreferenced content rows pruned from `content`.
    pub unreferenced_content_pruned: usize,
    /// Number of orphaned symbols pruned from `symbols`.
    pub orphaned_symbols_pruned: usize,
    /// Number of orphaned call edges pruned from `call_edges`.
    pub orphaned_call_edges_pruned: usize,
    /// Number of orphaned imports pruned from `imports`.
    pub orphaned_imports_pruned: usize,
}

/// The database store: one connection per database file, owned by the caller
/// behind a Mutex (see `super::store`).
#[derive(Debug)]
pub struct Store {
    conn: Connection,
    db_path: PathBuf,
}

impl Store {
    /// Open (or create) the store at `db_path` and initialize the schema.
    pub fn open(db_path: &Path) -> Result<Self, String> {
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("Failed to create store dir: {e}"))?;
        }

        let conn = Connection::open(db_path)
            .map_err(|e| format!("Failed to open {}: {e}", db_path.display()))?;

        // WAL lets `vector_search` read on its own read-only connection
        // while this one writes. The busy timeout covers the rare writer/
        // snapshot overlap without changing any observable behavior.
        conn.busy_timeout(std::time::Duration::from_secs(30))
            .map_err(|e| format!("Failed to set busy timeout: {e}"))?;

        // #1062: cap the `-wal` sidecar on THIS connection. `journal_size_limit`
        // is per-connection, so the pool's `post_create` cap (#354) cannot reach
        // a connection built here — without it this store's writes keep the WAL
        // ratchet alive and `memory.db-wal` is bounded only by the 64 MiB
        // TRUNCATE threshold in the maintenance sweep. Applied before
        // `initialize()`, so the cap is on the connection before its first write.
        crate::db::apply_wal_journal_size_limit(&conn)
            .map_err(|e| format!("Failed to set WAL journal size limit: {e}"))?;

        let mut store = Self {
            conn,
            db_path: db_path.to_path_buf(),
        };
        store.initialize()?;
        Ok(store)
    }

    /// Database file path.
    #[must_use]
    pub fn db_path(&self) -> &Path {
        &self.db_path
    }

    /// The store's own write connection.
    ///
    /// Test-only visibility, and deliberately so: `PRAGMA journal_size_limit`
    /// is PER-CONNECTION, so a test that opened a SECOND connection to the same
    /// file would read `-1` and prove nothing about the connection that does
    /// the writing (#1062 — the trap #354 documented). Production code uses the
    /// typed methods below.
    #[cfg(test)]
    pub(crate) fn conn(&self) -> &Connection {
        &self.conn
    }

    /// Schema DDL, byte-for-byte the one qmd 0.3.2 created. Do not "improve"
    /// it casually: existing memory.db files and vector_search.rs depend on
    /// exactly these tables, columns and triggers.
    fn initialize(&mut self) -> Result<(), String> {
        self.conn
            .execute_batch(
                r"
            PRAGMA journal_mode = WAL;
            PRAGMA foreign_keys = ON;

            -- Content-addressable storage
            CREATE TABLE IF NOT EXISTS content (
                hash TEXT PRIMARY KEY,
                doc TEXT NOT NULL,
                created_at TEXT NOT NULL
            );

            -- Documents table
            CREATE TABLE IF NOT EXISTS documents (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                collection TEXT NOT NULL,
                path TEXT NOT NULL,
                title TEXT NOT NULL,
                hash TEXT NOT NULL,
                created_at TEXT NOT NULL,
                modified_at TEXT NOT NULL,
                active INTEGER NOT NULL DEFAULT 1,
                FOREIGN KEY (hash) REFERENCES content(hash) ON DELETE CASCADE,
                UNIQUE(collection, path)
            );

            CREATE INDEX IF NOT EXISTS idx_documents_collection ON documents(collection, active);
            CREATE INDEX IF NOT EXISTS idx_documents_hash ON documents(hash);
            CREATE INDEX IF NOT EXISTS idx_documents_path ON documents(path, active);

            -- FTS index
            CREATE VIRTUAL TABLE IF NOT EXISTS documents_fts USING fts5(
                filepath, title, body,
                tokenize='porter unicode61'
            );

            -- Content vectors metadata
            CREATE TABLE IF NOT EXISTS content_vectors (
                hash TEXT NOT NULL,
                seq INTEGER NOT NULL DEFAULT 0,
                pos INTEGER NOT NULL DEFAULT 0,
                model TEXT NOT NULL,
                embedded_at TEXT NOT NULL,
                chunk_hash TEXT,
                PRIMARY KEY (hash, seq)
            );
            ",
            )
            .map_err(|e| format!("Failed to initialize schema: {e}"))?;

        // Column heal (#14): the DDL above is CREATE TABLE IF NOT EXISTS, a
        // no-op on tables created by older builds. A content_vectors table
        // that predates #1107 (2026-08-19) has no chunk_hash column, and
        // chunk_needs_embedding then fails on every chunk of every backfill
        // cycle. Probe and add the column when it is missing; idempotent.
        self.ensure_chunk_hash_column()?;

        self.create_fts_triggers()
    }

    /// Add `chunk_hash` to `content_vectors` when the table predates it (#14).
    ///
    /// SQLite has no `ADD COLUMN IF NOT EXISTS`, so probe `pragma_table_info`
    /// and ALTER only when the column is absent. On a fresh store the DDL
    /// above already created the column and this stays a single read.
    fn ensure_chunk_hash_column(&self) -> Result<(), String> {
        let has_column: bool = self
            .conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('content_vectors')
                 WHERE name = 'chunk_hash'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .map(|n| n > 0)
            .map_err(|e| format!("chunk_hash column probe: {e}"))?;

        if !has_column {
            self.conn
                .execute_batch("ALTER TABLE content_vectors ADD COLUMN chunk_hash TEXT;")
                .map_err(|e| format!("chunk_hash column heal: {e}"))?;
        }
        Ok(())
    }

    /// FTS5 external-content sync triggers. Copied verbatim from qmd: the
    /// FTS index is keyed by documents.id (rowid) and mirrors active rows.
    fn create_fts_triggers(&self) -> Result<(), String> {
        let trigger_exists: bool = self
            .conn
            .query_row(
                "SELECT 1 FROM sqlite_master WHERE type='trigger' AND name='documents_ai'",
                [],
                |_| Ok(true),
            )
            .unwrap_or(false);

        if !trigger_exists {
            self.conn
                .execute_batch(
                    r"
                CREATE TRIGGER IF NOT EXISTS documents_ai AFTER INSERT ON documents
                WHEN new.active = 1
                BEGIN
                    INSERT INTO documents_fts(rowid, filepath, title, body)
                    SELECT
                        new.id,
                        new.collection || '/' || new.path,
                        new.title,
                        (SELECT doc FROM content WHERE hash = new.hash)
                    WHERE new.active = 1;
                END;

                CREATE TRIGGER IF NOT EXISTS documents_ad AFTER DELETE ON documents BEGIN
                    DELETE FROM documents_fts WHERE rowid = old.id;
                END;

                CREATE TRIGGER IF NOT EXISTS documents_au AFTER UPDATE ON documents
                BEGIN
                    DELETE FROM documents_fts WHERE rowid = old.id AND new.active = 0;
                    INSERT OR REPLACE INTO documents_fts(rowid, filepath, title, body)
                    SELECT
                        new.id,
                        new.collection || '/' || new.path,
                        new.title,
                        (SELECT doc FROM content WHERE hash = new.hash)
                    WHERE new.active = 1;
                END;
                ",
                )
                .map_err(|e| format!("Failed to create FTS triggers: {e}"))?;
        }

        Ok(())
    }

    /// Hash content using SHA256. Must stay byte-compatible with hashes
    /// already stored (they are the foreign keys into `content`).
    #[must_use]
    pub fn hash_content(content: &str) -> String {
        let mut hasher = Sha256::new();
        hasher.update(content.as_bytes());
        format!("{:x}", hasher.finalize())
    }

    /// Extract title from markdown content (first `#` or `##` heading).
    #[must_use]
    pub fn extract_title(content: &str) -> String {
        for line in content.lines() {
            let trimmed = line.trim();
            if let Some(rest) = trimmed.strip_prefix("# ") {
                return rest.trim().to_string();
            }
            if let Some(rest) = trimmed.strip_prefix("## ") {
                return rest.trim().to_string();
            }
        }
        String::new()
    }

    /// Insert content into content-addressable storage.
    pub fn insert_content(
        &self,
        hash: &str,
        content: &str,
        created_at: &str,
    ) -> Result<(), String> {
        self.conn
            .execute(
                "INSERT OR IGNORE INTO content (hash, doc, created_at) VALUES (?1, ?2, ?3)",
                params![hash, content, created_at],
            )
            .map_err(|e| format!("insert_content: {e}"))?;
        Ok(())
    }

    /// Insert (or reactivate/update) a document record.
    pub fn insert_document(
        &self,
        collection: &str,
        path: &str,
        title: &str,
        hash: &str,
        created_at: &str,
        modified_at: &str,
    ) -> Result<(), String> {
        self.conn
            .execute(
                r"
            INSERT INTO documents (collection, path, title, hash, created_at, modified_at, active)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, 1)
            ON CONFLICT(collection, path) DO UPDATE SET
                title = excluded.title,
                hash = excluded.hash,
                modified_at = excluded.modified_at,
                active = 1
            ",
                params![collection, path, title, hash, created_at, modified_at],
            )
            .map_err(|e| format!("insert_document: {e}"))?;
        Ok(())
    }

    /// Find an active document by collection and path.
    pub fn find_active_document(
        &self,
        collection: &str,
        path: &str,
    ) -> Result<Option<(i64, String, String)>, String> {
        self.conn
            .query_row(
                "SELECT id, hash, title FROM documents WHERE collection = ?1 AND path = ?2 AND active = 1",
                params![collection, path],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()
            .map_err(|e| format!("find_active_document: {e}"))
    }

    /// Deactivate a document (soft delete; the FTS trigger drops the row).
    ///
    /// Stand-alone deactivation is for PRUNE legs only, where the document is
    /// genuinely leaving the index: the next `gc_orphans` pass prunes the
    /// hash's vector rows (the reclaim), and the `content` row follows once
    /// no document references it. Re-indexing an EXISTING file must never
    /// call this — use [`Store::replace_document`], which folds the
    /// deactivate + re-insert pair into one transaction so no reader, in any
    /// process, ever observes the document as inactive and prunes vectors
    /// that are still correct (#1069's treadmill).
    pub fn deactivate_document(&self, collection: &str, path: &str) -> Result<(), String> {
        self.conn
            .execute(
                "UPDATE documents SET active = 0 WHERE collection = ?1 AND path = ?2",
                params![collection, path],
            )
            .map_err(|e| format!("deactivate_document: {e}"))?;
        Ok(())
    }

    /// Atomically replace one document's row with the row for `new_hash`.
    ///
    /// The re-index path used to call [`Store::deactivate_document`] and then
    /// [`Store::insert_document`] as two auto-commit statements. Any
    /// `active`-sensitive query could run between them and see the document
    /// as absent: the FTS triggers drop its search row, the backfill selects
    /// it as needing embedding, and `gc_orphans` — which keys "orphan" on
    /// `active = 0` — deletes every vector row the old content owned. One
    /// transaction makes the swap invisible: readers either see the old row
    /// or the new one, never a hole.
    ///
    /// `old_hash` is the content hash of the row being replaced, taken from
    /// the `find_active_document` read that gated this call. The UPDATE is
    /// keyed on it so a concurrent re-index that already swapped the row is
    /// not deactivated a second time: if the row now belongs to another hash,
    /// the UPDATE matches nothing and only the caller's upsert runs. That
    /// upsert is deliberately last-write-wins (`ON CONFLICT ... DO UPDATE`),
    /// the same conflict semantics `insert_document` always had — the keyed
    /// UPDATE exists to keep the DEACTIVATE half honest, not to arbitrate the
    /// insert.
    ///
    /// Returns the INSERT's rowcount (1 on any successful insert or upsert);
    /// it says nothing about the deactivate leg, which matching 0 rows is a
    /// legitimate outcome of.
    pub fn replace_document(
        &self,
        collection: &str,
        path: &str,
        old_hash: &str,
        title: &str,
        new_hash: &str,
        now: &str,
    ) -> Result<usize, String> {
        // `unchecked_transaction` (not `transaction`): the Store mutex is
        // shared, so this method only ever holds `&self`, and
        // `Connection::transaction` requires `&mut self`. Same BEGIN..COMMIT
        // semantics, minus the `&mut` hand-back.
        let tx = self
            .conn
            .unchecked_transaction()
            .map_err(|e| format!("replace_document begin: {e}"))?;
        tx.execute(
            "UPDATE documents SET active = 0
             WHERE collection = ?1 AND path = ?2 AND hash = ?3 AND active = 1",
            params![collection, path, old_hash],
        )
        .map_err(|e| format!("replace_document deactivate: {e}"))?;
        let inserted = tx
            .execute(
                r"
            INSERT INTO documents (collection, path, title, hash, created_at, modified_at, active)
            VALUES (?1, ?2, ?3, ?4, ?5, ?5, 1)
            ON CONFLICT(collection, path) DO UPDATE SET
                title = excluded.title,
                hash = excluded.hash,
                modified_at = excluded.modified_at,
                active = 1
            ",
                params![collection, path, title, new_hash, now],
            )
            .map_err(|e| format!("replace_document insert: {e}"))?;
        tx.commit()
            .map_err(|e| format!("replace_document commit: {e}"))?;
        Ok(inserted)
    }

    /// All active document paths in a collection.
    pub fn get_active_document_paths(&self, collection: &str) -> Result<Vec<String>, String> {
        let mut stmt = self
            .conn
            .prepare("SELECT path FROM documents WHERE collection = ?1 AND active = 1")
            .map_err(|e| format!("get_active_document_paths: {e}"))?;
        let paths = stmt
            .query_map(params![collection], |row| row.get(0))
            .map_err(|e| format!("get_active_document_paths: {e}"))?
            .collect::<Result<Vec<String>, _>>()
            .map_err(|e| format!("get_active_document_paths: {e}"))?;
        Ok(paths)
    }

    /// Load one active document with its full body.
    pub fn get_document(
        &self,
        collection: &str,
        path: &str,
    ) -> Result<Option<DocumentResult>, String> {
        self.conn
            .query_row(
                r"
                SELECT
                    d.title,
                    d.hash,
                    d.modified_at,
                    c.doc,
                    LENGTH(c.doc) as body_length
                FROM documents d
                JOIN content c ON c.hash = d.hash
                WHERE d.collection = ?1 AND d.path = ?2 AND d.active = 1
                ",
                params![collection, path],
                |row| {
                    let title: String = row.get(0)?;
                    let hash: String = row.get(1)?;
                    let modified_at: String = row.get(2)?;
                    let body: String = row.get(3)?;

                    Ok(DocumentResult {
                        collection_name: collection.to_string(),
                        path: path.to_string(),
                        display_path: format!("{collection}/{path}"),
                        title,
                        hash,
                        modified_at,
                        body: Some(body),
                    })
                },
            )
            .optional()
            .map_err(|e| format!("get_document: {e}"))
    }

    /// Full-text search (BM25) over active documents, optionally restricted
    /// to one collection. Scores are negated bm25(): higher is better.
    pub fn search_fts(
        &self,
        query: &str,
        limit: usize,
        collection: Option<&str>,
    ) -> Result<Vec<SearchResult>, String> {
        Self::search_fts_on(&self.conn, query, limit, collection)
    }

    /// The shared body of [`Self::search_fts`] and [`Self::search_fts_readonly`].
    ///
    /// One implementation, so the eval harness's read-only reader cannot score a
    /// query differently from the store's own search (#981).
    fn search_fts_on(
        conn: &Connection,
        query: &str,
        limit: usize,
        collection: Option<&str>,
    ) -> Result<Vec<SearchResult>, String> {
        let sql = if collection.is_some() {
            r"
            SELECT
                d.collection,
                d.path,
                d.title,
                d.hash,
                d.modified_at,
                bm25(documents_fts) as score
            FROM documents_fts fts
            JOIN documents d ON d.id = fts.rowid
            JOIN content c ON c.hash = d.hash
            WHERE documents_fts MATCH ?1
              AND d.collection = ?2
              AND d.active = 1
            ORDER BY score
            LIMIT ?3
            "
        } else {
            r"
            SELECT
                d.collection,
                d.path,
                d.title,
                d.hash,
                d.modified_at,
                bm25(documents_fts) as score
            FROM documents_fts fts
            JOIN documents d ON d.id = fts.rowid
            JOIN content c ON c.hash = d.hash
            WHERE documents_fts MATCH ?1
              AND d.active = 1
            ORDER BY score
            LIMIT ?2
            "
        };

        let mut stmt = conn
            .prepare(sql)
            .map_err(|e| format!("search_fts prepare: {e}"))?;

        let map_row = |row: &rusqlite::Row| -> rusqlite::Result<SearchResult> {
            let collection_name: String = row.get(0)?;
            let path: String = row.get(1)?;
            let title: String = row.get(2)?;
            let hash: String = row.get(3)?;
            let modified_at: String = row.get(4)?;
            let score: f64 = row.get(5)?;

            Ok(SearchResult {
                doc: DocumentResult {
                    collection_name: collection_name.clone(),
                    display_path: format!("{collection_name}/{path}"),
                    path: path.clone(),
                    title,
                    hash,
                    modified_at,
                    body: None,
                },
                // BM25 returns negative scores; negate so higher is better.
                score: -score,
            })
        };

        let results: Vec<SearchResult> = if let Some(coll) = collection {
            stmt.query_map(params![query, coll, limit as i64], map_row)
        } else {
            stmt.query_map(params![query, limit as i64], map_row)
        }
        .map_err(|e| format!("search_fts: {e}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("search_fts: {e}"))?;

        Ok(results)
    }

    /// Read-only FTS search, for eval harnesses that must not touch the store
    /// (#981).
    ///
    /// The live retrieval leg reads a profile store it must never modify. The
    /// connection is opened read-only, so an accidental write becomes an error
    /// rather than a silent mutation, and the scoring body is shared with
    /// [`Self::search_fts`] so the two cannot disagree about a query.
    pub(crate) fn search_fts_readonly(
        db_path: &Path,
        query: &str,
        limit: usize,
        collection: Option<&str>,
    ) -> Result<Vec<SearchResult>, String> {
        let conn = Connection::open_with_flags(
            db_path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(|e| format!("Failed to open memory store read-only: {e}"))?;
        conn.busy_timeout(std::time::Duration::from_secs(30))
            .map_err(|e| format!("Failed to set busy timeout: {e}"))?;
        Self::search_fts_on(&conn, query, limit, collection)
    }

    /// The corpus fingerprint for the LIVE retrieval leg (#981), read-only.
    ///
    /// `vector_stats` answers the same question but needs an open `Store`, and
    /// `Store::open` runs schema DDL. The live leg must not write the store it
    /// measures — a run that changed the corpus would make its own numbers
    /// unreproducible — so the counts come off a read-only connection instead.
    ///
    /// Returns `(active documents, embedded vector rows, dominant model)`. The
    /// model is the most common one rather than an arbitrary row: a store that
    /// has been through a model change carries both, and the fingerprint should
    /// name the one most of the corpus was embedded with.
    pub(crate) fn fingerprint_readonly(db_path: &Path) -> Result<(usize, usize, String), String> {
        let conn = Connection::open_with_flags(
            db_path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(|e| format!("Failed to open memory store read-only: {e}"))?;
        conn.busy_timeout(std::time::Duration::from_secs(30))
            .map_err(|e| format!("Failed to set busy timeout: {e}"))?;

        let scalar = |sql: &str| -> Result<i64, String> {
            conn.query_row(sql, [], |r| r.get(0))
                .map_err(|e| format!("fingerprint_readonly: {e}"))
        };

        let documents = scalar("SELECT COUNT(*) FROM documents WHERE active = 1")? as usize;
        // `skipped-too-large` rows hold an EMPTY vector: they occupy a row but
        // cannot be scored, so counting them would overstate the corpus.
        let vectors =
            scalar("SELECT COUNT(*) FROM content_vectors WHERE model != 'skipped-too-large'")?
                as usize;
        let model = conn
            .query_row(
                r"
                SELECT model
                FROM content_vectors
                WHERE model != 'skipped-too-large'
                GROUP BY model
                ORDER BY COUNT(*) DESC
                LIMIT 1
                ",
                [],
                |r| r.get::<_, String>(0),
            )
            .unwrap_or_default();

        Ok((documents, vectors, model))
    }

    /// Create the vector blob table (only when embeddings are enabled).
    /// Dimensions are not enforced in the schema; rows carry whatever the
    /// configured backend produced.
    pub fn ensure_vector_table(&self, _dimensions: usize) -> Result<(), String> {
        self.conn
            .execute(
                r"
                CREATE TABLE IF NOT EXISTS vectors_vec (
                    hash_seq TEXT PRIMARY KEY,
                    embedding BLOB NOT NULL
                )
                ",
                [],
            )
            .map_err(|e| format!("ensure_vector_table: {e}"))?;
        Ok(())
    }

    /// Create symbol graph tables for code-graph feature.
    ///
    /// Three tables:
    /// - `symbols`: function/struct/enum/trait/impl/import definitions
    /// - `call_edges`: caller → callee relationships
    /// - `imports`: module import paths
    #[cfg(feature = "code-graph")]
    pub fn ensure_symbol_tables(&self) -> Result<(), String> {
        self.conn
            .execute_batch(
                r"
                CREATE TABLE IF NOT EXISTS symbols (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    symbol_name TEXT NOT NULL,
                    kind TEXT NOT NULL,
                    file_path TEXT NOT NULL,
                    start_line INTEGER NOT NULL,
                    end_line INTEGER NOT NULL,
                    indexed_at TEXT NOT NULL
                );

                CREATE INDEX IF NOT EXISTS idx_symbols_name ON symbols(symbol_name);
                CREATE INDEX IF NOT EXISTS idx_symbols_file ON symbols(file_path);
                CREATE INDEX IF NOT EXISTS idx_symbols_kind ON symbols(kind);

                CREATE TABLE IF NOT EXISTS call_edges (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    caller_symbol TEXT NOT NULL,
                    callee_symbol TEXT NOT NULL,
                    file_path TEXT NOT NULL,
                    call_line INTEGER NOT NULL,
                    indexed_at TEXT NOT NULL
                );

                CREATE INDEX IF NOT EXISTS idx_call_edges_caller ON call_edges(caller_symbol);
                CREATE INDEX IF NOT EXISTS idx_call_edges_callee ON call_edges(callee_symbol);

                -- #1071: bare-name projection of `callee_symbol` — the stored
                -- value is EITHER bare (`clone`) or path-qualified
                -- (`super::embedding::run_api_backfill`); `rtrim(X,Y)` treats
                -- Y as a character SET, so it strips the trailing identifier
                -- characters and leaves `super::embedding::`, and
                -- substr(..., len+1) then yields the bare tail. For an
                -- already-bare value rtrim strips everything, so the
                -- projection is the value itself: bare edges stay queryable
                -- by the same expression.
                -- PIN — the WHERE predicate that drives this index is
                -- `substr(callee_symbol, length(rtrim(callee_symbol,
                -- 'abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789_'))+1) = ?`
                -- and it MUST stay character-for-character identical to this
                -- expression, or SQLite silently stops using the index and
                -- every bare-lookup query becomes a full scan. The plan is
                -- pinned by
                -- memory_caller_bare_lookup_test::bare_name_predicate_uses_the_expression_index
                -- (EXPLAIN QUERY PLAN must contain `idx_call_edges_callee_bare`).
                -- Measured on the live ops store (140,230 edges): build 1.1 s,
                -- 2.3 MiB, predicate lookup 0.01 s vs 0.3 s full scan.
                CREATE INDEX IF NOT EXISTS idx_call_edges_callee_bare ON call_edges(
                  substr(callee_symbol, length(rtrim(callee_symbol,'abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789_'))+1)
                );

                CREATE TABLE IF NOT EXISTS imports (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    module_path TEXT NOT NULL,
                    file_path TEXT NOT NULL,
                    import_line INTEGER NOT NULL,
                    indexed_at TEXT NOT NULL
                );

                CREATE INDEX IF NOT EXISTS idx_imports_module ON imports(module_path);
                CREATE INDEX IF NOT EXISTS idx_imports_file ON imports(file_path);

                -- #489: which extractor semantics wrote a file's graph. The
                -- content hash cannot answer that: bytes unchanged since the
                -- last index say nothing about whether the rows on disk were
                -- written by the semantics in force today, and the hash-skip in
                -- index_file_sync_keyed never revisits such a file. An absent
                -- stamp reads as stale, which is what makes rows written before
                -- this table existed reachable again.
                CREATE TABLE IF NOT EXISTS graph_semantics (
                    file_path TEXT PRIMARY KEY,
                    version INTEGER NOT NULL,
                    stamped_at TEXT NOT NULL
                );

                -- #522: replace_file_graph deletes a file's graph rows by
                -- file_path; without this index every such DELETE is a full
                -- scan of the table that doubled to GiB.
                CREATE INDEX IF NOT EXISTS idx_call_edges_file ON call_edges(file_path);
                ",
            )
            .map_err(|e| format!("ensure_symbol_tables: {e}"))?;
        Ok(())
    }

    /// Row counts for the symbol graph tables: (symbols, call_edges, imports).
    /// Used by logging and benchmark scaffolding.
    #[cfg(feature = "code-graph")]
    pub fn symbol_graph_counts(&self) -> Result<(i64, i64, i64), String> {
        let symbols: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM symbols", [], |r| r.get(0))
            .map_err(|e| format!("symbol_graph_counts symbols: {e}"))?;
        let edges: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM call_edges", [], |r| r.get(0))
            .map_err(|e| format!("symbol_graph_counts call_edges: {e}"))?;
        let imports: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM imports", [], |r| r.get(0))
            .map_err(|e| format!("symbol_graph_counts imports: {e}"))?;
        Ok((symbols, edges, imports))
    }

    /// Drop every graph row owned by one file so the next extract REPLACES it
    /// instead of appending (#522). One transaction: a crash mid-replace must
    /// never leave the file with a partial or empty graph.
    #[cfg(feature = "code-graph")]
    pub fn replace_file_graph(&self, file_path: &str) -> Result<(), String> {
        let tx = self
            .conn
            .unchecked_transaction()
            .map_err(|e| format!("replace_file_graph begin: {e}"))?;
        tx.execute(
            "DELETE FROM symbols WHERE file_path = ?1",
            params![file_path],
        )
        .map_err(|e| format!("replace_file_graph symbols: {e}"))?;
        tx.execute(
            "DELETE FROM call_edges WHERE file_path = ?1",
            params![file_path],
        )
        .map_err(|e| format!("replace_file_graph call_edges: {e}"))?;
        tx.execute(
            "DELETE FROM imports WHERE file_path = ?1",
            params![file_path],
        )
        .map_err(|e| format!("replace_file_graph imports: {e}"))?;
        tx.commit()
            .map_err(|e| format!("replace_file_graph commit: {e}"))?;
        Ok(())
    }

    /// The extractor semantics that last wrote `file_path`'s graph (#489).
    ///
    /// `None` means no stamp — which covers every row written before the stamp
    /// existed, and reads as stale deliberately: an unstamped graph is one
    /// whose semantics are unknown, so it is re-extracted rather than trusted.
    #[cfg(feature = "code-graph")]
    pub fn graph_semantics_version(&self, file_path: &str) -> Option<u32> {
        self.conn
            .query_row(
                "SELECT version FROM graph_semantics WHERE file_path = ?1",
                params![file_path],
                |row| row.get::<_, i64>(0),
            )
            .ok()
            .map(|version| version as u32)
    }

    /// Record that `file_path`'s graph was written by `version` semantics.
    #[cfg(feature = "code-graph")]
    pub fn stamp_graph_semantics(&self, file_path: &str, version: u32) -> Result<(), String> {
        self.conn
            .execute(
                r"
                INSERT INTO graph_semantics (file_path, version, stamped_at)
                VALUES (?1, ?2, ?3)
                ON CONFLICT(file_path) DO UPDATE SET
                    version = excluded.version,
                    stamped_at = excluded.stamped_at
                ",
                params![
                    file_path,
                    version as i64,
                    crate::utils::string::utc_timestamp()
                ],
            )
            .map_err(|e| format!("stamp_graph_semantics: {e}"))?;
        Ok(())
    }

    /// Free pages currently on the SQLite freelist; `0` when the pragma fails.
    ///
    /// Not cfg-gated: the maintenance sweep reads it on every build, and a
    /// failing pragma must read as "no holes" rather than error a sweep.
    pub fn freelist_count(&self) -> i64 {
        self.conn
            .query_row("PRAGMA freelist_count;", [], |row| row.get(0))
            .unwrap_or(0)
    }

    /// Insert a symbol into the symbol graph.
    #[cfg(feature = "code-graph")]
    pub fn insert_symbol(
        &self,
        symbol_name: &str,
        kind: &str,
        file_path: &str,
        start_line: usize,
        end_line: usize,
    ) -> Result<(), String> {
        let indexed_at = chrono::Utc::now().to_rfc3339();
        self.conn
            .execute(
                r"
                INSERT INTO symbols (symbol_name, kind, file_path, start_line, end_line, indexed_at)
                VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                ",
                params![
                    symbol_name,
                    kind,
                    file_path,
                    start_line as i64,
                    end_line as i64,
                    indexed_at
                ],
            )
            .map_err(|e| format!("insert_symbol: {e}"))?;
        Ok(())
    }

    /// Insert a call edge (caller → callee).
    #[cfg(feature = "code-graph")]
    pub fn insert_call_edge(
        &self,
        caller_symbol: &str,
        callee_symbol: &str,
        file_path: &str,
        call_line: usize,
    ) -> Result<(), String> {
        let indexed_at = chrono::Utc::now().to_rfc3339();
        self.conn
            .execute(
                r"
                INSERT INTO call_edges (caller_symbol, callee_symbol, file_path, call_line, indexed_at)
                VALUES (?1, ?2, ?3, ?4, ?5)
                ",
                params![
                    caller_symbol,
                    callee_symbol,
                    file_path,
                    call_line as i64,
                    indexed_at
                ],
            )
            .map_err(|e| format!("insert_call_edge: {e}"))?;
        Ok(())
    }

    /// Insert an import.
    #[cfg(feature = "code-graph")]
    pub fn insert_import(
        &self,
        module_path: &str,
        file_path: &str,
        import_line: usize,
    ) -> Result<(), String> {
        let indexed_at = chrono::Utc::now().to_rfc3339();
        self.conn
            .execute(
                r"
                INSERT INTO imports (module_path, file_path, import_line, indexed_at)
                VALUES (?1, ?2, ?3, ?4)
                ",
                params![module_path, file_path, import_line as i64, indexed_at],
            )
            .map_err(|e| format!("insert_import: {e}"))?;
        Ok(())
    }

    /// Query symbols by name (exact match).
    #[cfg(feature = "code-graph")]
    pub fn query_symbols_by_name(
        &self,
        name: &str,
    ) -> Result<Vec<(String, String, usize, usize)>, String> {
        let mut stmt = self
            .conn
            .prepare(
                r"
                SELECT kind, file_path, start_line, end_line
                FROM symbols
                WHERE symbol_name = ?1
                ORDER BY (file_path LIKE '%/tests/%'), kind, file_path, start_line
                ",
            )
            .map_err(|e| format!("query_symbols_by_name prepare: {e}"))?;

        let rows = stmt
            .query_map(params![name], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)? as usize,
                    row.get::<_, i64>(3)? as usize,
                ))
            })
            .map_err(|e| format!("query_symbols_by_name: {e}"))?;

        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("query_symbols_by_name collect: {e}"))
    }

    /// Query call edges where the given symbol is the callee (who calls this function?).
    #[cfg(feature = "code-graph")]
    pub fn query_callers_of(&self, callee: &str) -> Result<Vec<(String, String, usize)>, String> {
        let mut stmt = self
            .conn
            .prepare(
                r"
                SELECT caller_symbol, file_path, call_line
                FROM call_edges
                WHERE callee_symbol = ?1
                ORDER BY caller_symbol, file_path, call_line
                ",
            )
            .map_err(|e| format!("query_callers_of prepare: {e}"))?;

        let rows = stmt
            .query_map(params![callee], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)? as usize,
                ))
            })
            .map_err(|e| format!("query_callers_of: {e}"))?;

        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("query_callers_of collect: {e}"))
    }

    /// Query call edges where the given symbol is the caller (what does this function call?).
    #[cfg(feature = "code-graph")]
    pub fn query_callees_of(&self, caller: &str) -> Result<Vec<(String, String, usize)>, String> {
        let mut stmt = self
            .conn
            .prepare(
                r"
                SELECT callee_symbol, file_path, call_line
                FROM call_edges
                WHERE caller_symbol = ?1
                ORDER BY callee_symbol, file_path, call_line
                ",
            )
            .map_err(|e| format!("query_callees_of prepare: {e}"))?;

        let rows = stmt
            .query_map(params![caller], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)? as usize,
                ))
            })
            .map_err(|e| format!("query_callees_of: {e}"))?;

        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("query_callees_of collect: {e}"))
    }

    /// Query callers of `callee` by bare-name projection (#1071). Matches the
    /// call edges whose stored `callee_symbol` is a PATH-QUALIFIED form
    /// (`super::embedding::run_api_backfill`) whose bare tail equals `callee`,
    /// plus — trivially — edges already stored bare under the same name. The
    /// expression must stay character-for-character identical to the
    /// `idx_call_edges_callee_bare` index expression in `ensure_symbol_tables`
    /// or SQLite will silently fall back to a full scan; the plan is pinned by
    /// memory_caller_bare_lookup_test.
    ///
    /// Returns (caller_symbol, file_path, call_line, matched_callee_symbol):
    /// the last field is the STORED form that matched, which is what lets the
    /// render layer tell `Vec::new` from `Arc::new` apart when a bare name
    /// like `new` maps to many qualified sources (measured: 558 sources,
    /// 8,497 edges, for `new` alone in the live ops index).
    ///
    /// Call `query_callers_of` FIRST — the exact match is the cheap, common
    /// case (78% of live edges are stored bare) and keeps existing behaviour
    /// untouched; this method is the fallback for the qualified remainder.
    #[cfg(feature = "code-graph")]
    pub fn query_callers_of_qualified(
        &self,
        callee: &str,
    ) -> Result<Vec<(String, String, usize, String)>, String> {
        let mut stmt = self
            .conn
            .prepare(
                r"
                SELECT caller_symbol, file_path, call_line, callee_symbol
                FROM call_edges
                WHERE substr(callee_symbol, length(rtrim(callee_symbol,'abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789_'))+1) = ?1
                ORDER BY caller_symbol, file_path, call_line
                ",
            )
            .map_err(|e| format!("query_callers_of_qualified prepare: {e}"))?;

        let rows = stmt
            .query_map(params![callee], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)? as usize,
                    row.get::<_, String>(3)?,
                ))
            })
            .map_err(|e| format!("query_callers_of_qualified: {e}"))?;

        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("query_callers_of_qualified collect: {e}"))
    }

    /// `query_callers_of_qualified` grouped by the stored callee form that
    /// matched (#1071). A bare name is frequently ambiguous — `new` maps to
    /// `Vec::new`, `Arc::new`, `String::new`, … — so the render layer needs
    /// per-source batches to head them separately instead of printing a flat
    /// multi-thousand-row list.
    ///
    /// Returns one (matched_callee_symbol, rows) pair per distinct stored
    /// form, sorted by the stored form, each `rows` batch sorted by
    /// (caller_symbol, file_path, call_line) — the underlying query's order,
    /// preserved per group. Empty input yields an empty Vec.
    #[cfg(feature = "code-graph")]
    pub fn query_callers_of_qualified_grouped(
        &self,
        callee: &str,
    ) -> Result<Vec<(String, Vec<(String, String, usize)>)>, String> {
        use std::collections::BTreeMap;

        let mut groups: BTreeMap<String, Vec<(String, String, usize)>> = BTreeMap::new();
        for (caller, file, line, matched) in self.query_callers_of_qualified(callee)? {
            groups
                .entry(matched)
                .or_default()
                .push((caller, file, line));
        }
        Ok(groups.into_iter().collect())
    }

    /// Transitive callers of a symbol up to `max_depth` hops, via a recursive
    /// walk of `call_edges` (#89). Returns (caller, file, call_line, depth)
    /// with depth 1 = direct caller; each caller appears once at its SHALLOWEST
    /// depth, deterministically ordered by (depth, caller, file, line).
    ///
    /// #1071: the seed matches BOTH stored forms of the callee — exact
    /// (`callee_symbol = ?1`) and bare-name projection — because a callee is
    /// stored either bare or path-qualified and the recursive step joins on
    /// `callee_symbol = caller_symbol`, a bare-vs-qualified name collision:
    /// `caller_symbol` is always bare, so every qualified edge becomes
    /// invisible to the recursion's join (measured on the live ops index:
    /// 21,501 depth-2 rows were reachable ONLY through qualified edges). The
    /// projection member is guarded by `callee_symbol <> ?1` so an
    /// already-bare edge matched by the exact member is not re-seeded; UNION
    /// still deduplicates the overlap.
    ///
    /// Cycle guard: the recursive member increments `depth` and is bounded by
    /// `imp.depth < ?2`, so an A→B→A cycle in corrupt data terminates at the
    /// depth cap instead of walking forever; UNION (not UNION ALL) keeps the
    /// intermediate row set from fanning out on repeated identical edges.
    #[cfg(feature = "code-graph")]
    pub fn query_transitive_callers(
        &self,
        callee: &str,
        max_depth: usize,
    ) -> Result<Vec<(String, String, usize, usize)>, String> {
        let mut stmt = self
            .conn
            .prepare(
                r"
                SELECT caller_symbol, file_path, call_line, MIN(depth) AS depth
                FROM (
                    WITH RECURSIVE impact(caller_symbol, file_path, call_line, depth) AS (
                        SELECT caller_symbol, file_path, call_line, 1
                        FROM call_edges
                        WHERE callee_symbol = ?1
                        UNION
                        SELECT caller_symbol, file_path, call_line, 1
                        FROM call_edges
                        WHERE callee_symbol <> ?1
                          AND substr(callee_symbol, length(rtrim(callee_symbol,'abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789_'))+1) = ?1
                        UNION
                        SELECT ce.caller_symbol, ce.file_path, ce.call_line, imp.depth + 1
                        FROM call_edges ce
                        JOIN impact imp ON ce.callee_symbol = imp.caller_symbol
                        WHERE imp.depth < ?2
                    )
                    SELECT caller_symbol, file_path, call_line, depth FROM impact
                )
                GROUP BY caller_symbol
                ORDER BY depth, caller_symbol, file_path, call_line
                ",
            )
            .map_err(|e| format!("query_transitive_callers prepare: {e}"))?;

        let rows = stmt
            .query_map(params![callee, max_depth as i64], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)? as usize,
                    row.get::<_, i64>(3)? as usize,
                ))
            })
            .map_err(|e| format!("query_transitive_callers: {e}"))?;

        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("query_transitive_callers collect: {e}"))
    }

    /// Insert (or replace) an embedding for one chunk of a content hash.
    /// `hash_seq` is `{hash}_{seq}` — the key format `vector_search.rs`
    /// reads, so it must not change.
    /// Insert an embedding vector with per-chunk hash for caching (#1107).
    ///
    /// `hash` is the hash of the WHOLE document (foreign key to `content` table).
    /// `seq` is the chunk sequence number within the document.
    /// `chunk_hash` is the hash of THIS chunk's content (for cache invalidation).
    #[allow(clippy::too_many_arguments)]
    pub fn insert_embedding(
        &self,
        hash: &str,
        seq: usize,
        pos: usize,
        embedding: &[f32],
        model: &str,
        embedded_at: &str,
        chunk_hash: Option<&str>,
    ) -> Result<(), String> {
        self.conn
            .execute(
                r"
            INSERT OR REPLACE INTO content_vectors (hash, seq, pos, model, embedded_at, chunk_hash)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6)
            ",
                params![hash, seq as i64, pos as i64, model, embedded_at, chunk_hash],
            )
            .map_err(|e| format!("insert_embedding metadata: {e}"))?;

        let hash_seq = format!("{hash}_{seq}");
        let embedding_bytes: Vec<u8> = embedding.iter().flat_map(|f| f.to_le_bytes()).collect();

        self.conn
            .execute(
                "INSERT OR REPLACE INTO vectors_vec (hash_seq, embedding) VALUES (?1, ?2)",
                params![hash_seq, embedding_bytes],
            )
            .map_err(|e| format!("insert_embedding blob: {e}"))?;

        Ok(())
    }

    /// Check if a chunk needs re-embedding based on its content hash (#1107).
    ///
    /// Returns `true` if no embedding exists for this (hash, seq) pair, or if
    /// the stored chunk_hash differs from the new chunk_hash. Returns `false`
    /// if the chunk is unchanged and can be skipped.
    pub fn chunk_needs_embedding(
        &self,
        hash: &str,
        seq: usize,
        chunk_hash: &str,
    ) -> Result<bool, String> {
        // `optional()` turns "no such row" into None, so the closure itself
        // must read the column as Option<String>: a healed legacy row has a
        // NULL chunk_hash, and reading it as plain String would raise
        // InvalidColumnType(Null) instead of falling through to re-embed (#14).
        let stored_hash: Option<String> = self
            .conn
            .query_row(
                "SELECT chunk_hash FROM content_vectors WHERE hash = ?1 AND seq = ?2",
                params![hash, seq as i64],
                |r| r.get::<_, Option<String>>(0),
            )
            .optional()
            .map_err(|e| format!("chunk_needs_embedding: {e}"))?
            .flatten();

        match stored_hash {
            None => Ok(true),                         // No embedding exists yet
            Some(stored) => Ok(stored != chunk_hash), // Re-embed if hash changed
        }
    }

    /// Does this document already hold a contiguous, complete vector set?
    ///
    /// The backfill's resume cursor. The pass used to re-walk every selected
    /// document from the top each time it ran, so under churn the same early
    /// documents were re-verified forever while the tail of the list never got
    /// serviced. A document is "done" when every seq from 0..=max(seq) is
    /// present: chunked embedding writes strictly ascending seqs, so a hole
    /// (a failed chunk) keeps the document eligible without re-embedding the
    /// chunks that DID land.
    ///
    /// A single chunk count is deliberately not enough: a 12-chunk document
    /// whose pass died after chunk 3 has 4 rows and is not done. That is the
    /// arithmetic behind the deficit measured on the ops profile (10,081 rows
    /// in `content_vectors` against a 12,169-chunk lower bound): partial rows
    /// are work in progress, not completion.
    pub fn document_fully_embedded(&self, hash: &str) -> Result<bool, String> {
        // Guarded by COUNT(*) > 0, and that guard is load-bearing: over an
        // empty set MAX(seq) is NULL, so `COALESCE(NULL + 1, 0) = COUNT(*)`
        // would evaluate as `0 = 0` and read TRUE for a document with no
        // vector rows at all — the cursor would then tell the backfill that
        // every unembedded document was done. With the guard, an empty set
        // short-circuits to false; a non-empty set is done iff MAX(seq) + 1
        // == COUNT(*), which is "every seq 0..=max present" because (hash,
        // seq) is the PRIMARY KEY and duplicates cannot inflate the count.
        // A hole ({0,1,3}: 4 != 3) keeps the document eligible.
        self.conn
            .query_row(
                "SELECT COUNT(*) > 0 AND MAX(seq) + 1 = COUNT(*)
                 FROM content_vectors WHERE hash = ?1",
                params![hash],
                |r| r.get::<_, bool>(0),
            )
            .map_err(|e| format!("document_fully_embedded: {e}"))
    }

    /// Counts describing how much of the store is actually vectorised (#1067).
    ///
    /// Counts only, never document bodies: `/doctor` runs on a channel and must
    /// not pull memory content into a group chat, and the queries stay cheap on
    /// a large store.
    pub fn vector_stats(&self) -> Result<VectorStats, String> {
        let scalar = |sql: &str| -> Result<i64, String> {
            self.conn
                .query_row(sql, [], |r| r.get(0))
                .map_err(|e| format!("vector_stats: {e}"))
        };

        Ok(VectorStats {
            documents_active: scalar("SELECT COUNT(*) FROM documents WHERE active = 1")? as usize,
            // Same shape as `get_hashes_needing_embedding`, counted rather than
            // materialised: doctor wants the number, not 65 document bodies.
            documents_unembedded: scalar(
                r"
                SELECT COUNT(DISTINCT d.hash)
                FROM documents d
                LEFT JOIN content_vectors v ON d.hash = v.hash AND v.seq = 0
                WHERE d.active = 1 AND v.hash IS NULL
                ",
            )? as usize,
            vector_rows: scalar("SELECT COUNT(*) FROM content_vectors")? as usize,
            last_embedded_at: self
                .conn
                .query_row("SELECT MAX(embedded_at) FROM content_vectors", [], |r| {
                    r.get::<_, Option<String>>(0)
                })
                .map_err(|e| format!("vector_stats: {e}"))?,
        })
    }

    /// Active documents with no `seq = 0` vector row yet: (hash, path, body).
    pub fn get_hashes_needing_embedding(&self) -> Result<Vec<(String, String, String)>, String> {
        let mut stmt = self
            .conn
            .prepare(
                r"
            SELECT DISTINCT d.hash, d.path, c.doc
            FROM documents d
            JOIN content c ON c.hash = d.hash
            LEFT JOIN content_vectors v ON d.hash = v.hash AND v.seq = 0
            WHERE d.active = 1 AND v.hash IS NULL
            ",
            )
            .map_err(|e| format!("get_hashes_needing_embedding: {e}"))?;

        let results = stmt
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
            .map_err(|e| format!("get_hashes_needing_embedding: {e}"))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("get_hashes_needing_embedding: {e}"))?;

        Ok(results)
    }

    /// Garbage collect orphaned vectors, unreferenced content chunks, and stale symbols (#241).
    ///
    /// Purges:
    /// - `content_vectors` rows whose `hash` has no ACTIVE document in `documents`.
    /// - `vectors_vec` blobs whose `hash_seq` has no corresponding `content_vectors` or active document.
    /// - `content` rows that are unreferenced by any document.
    /// - Code-graph tables (`symbols`, `call_edges`, `imports`) for files not existing in active external indexing.
    ///
    /// ## Why "no active document" is safe again
    ///
    /// The race that made this keying dangerous (#1069) was the re-index's
    /// deactivate-then-reinsert pair: two auto-commit statements, so another
    /// process running this gc against the same WAL database could observe the
    /// row as `active = 0` mid-swap and delete the vectors of content that was
    /// still correct — the document then re-embedded from scratch, pass after
    /// pass. [`Store::replace_document`] makes the swap a single transaction,
    /// so no reader in ANY process observes the intermediate state, and
    /// `active = 0` again means only one thing: a PRUNE leg ran (the file
    /// genuinely left the index). Its vectors are the space this gc exists to
    /// reclaim — keying on "any document row" instead would leak it forever,
    /// because nothing in the application ever hard-deletes a `documents` row.
    pub fn gc_orphans(&self) -> Result<MemoryGcReport, String> {
        let mut report = MemoryGcReport::default();

        // 1. Delete vector blobs from vectors_vec for hash_seq pairs whose
        // metadata row is gone, or whose hash no ACTIVE document references
        // (the same population leg 2 prunes below — see the 1b comment).
        let has_vectors_vec: bool = self
            .conn
            .query_row(
                "SELECT 1 FROM sqlite_master WHERE type='table' AND name='vectors_vec'",
                [],
                |_| Ok(true),
            )
            .unwrap_or(false);

        if has_vectors_vec {
            let pruned_emb = self
                .conn
                .execute(
                    r"
                DELETE FROM vectors_vec WHERE hash_seq NOT IN (
                    SELECT v.hash || '_' || v.seq
                    FROM content_vectors v
                    JOIN documents d ON d.hash = v.hash
                    WHERE d.active = 1
                )
                ",
                    [],
                )
                .map_err(|e| format!("gc_orphans vectors_vec: {e}"))?;
            report.orphaned_embeddings_pruned = pruned_emb;
        }

        // 1b. Both prune legs key on the SAME population - active = 1 - and
        // must stay in sync. Leg 1 briefly keyed on active = 1 while leg 2
        // keyed on "any document row" during this fix's drafting; had that
        // shipped, a deactivated document kept its content_vectors metadata
        // (chunk_hash intact, so chunk_needs_embedding says "nothing to do")
        // while its vectors_vec blobs were pruned: silently unsearchable, and
        // invisible to every retry heuristic because no chunk ever looks
        // stale. Same population on both legs, always.
        //
        // Why active = 1 and not "any document row": the deactivate/re-insert
        // window an "any row" keying would protect against no longer exists —
        // the re-index swap is one transaction (replace_document), so no
        // reader, in any process, ever observes the row as active = 0
        // mid-swap. The only lasting producers of active = 0 rows are the
        // PRUNE legs (a file that genuinely left the index), and those rows'
        // vectors are exactly the space this gc exists to reclaim. Keying on
        // "any document row" instead would leak that space forever, because
        // nothing in the application ever hard-deletes a documents row.
        let pruned_vec = self
            .conn
            .execute(
                r"
            DELETE FROM content_vectors WHERE hash NOT IN (
                SELECT hash FROM documents WHERE active = 1
            )
            ",
                [],
            )
            .map_err(|e| format!("gc_orphans content_vectors: {e}"))?;
        report.orphaned_vectors_pruned = pruned_vec;

        // 3. Delete unreferenced content rows
        let pruned_content = self
            .conn
            .execute(
                r"
            DELETE FROM content WHERE hash NOT IN (
                SELECT hash FROM documents
            )
            ",
                [],
            )
            .map_err(|e| format!("gc_orphans content: {e}"))?;
        report.unreferenced_content_pruned = pruned_content;

        // 4. Prune code-graph tables if they exist
        #[cfg(feature = "code-graph")]
        {
            let has_symbols: bool = self
                .conn
                .query_row(
                    "SELECT 1 FROM sqlite_master WHERE type='table' AND name='symbols'",
                    [],
                    |_| Ok(true),
                )
                .unwrap_or(false);

            if has_symbols {
                // Delete symbols, call_edges, imports for files that are not referenced in documents (collection='external')
                let pruned_sym = self
                    .conn
                    .execute(
                        r"
                    DELETE FROM symbols WHERE file_path NOT IN (
                        SELECT path FROM documents WHERE collection = 'external' AND active = 1
                    )
                    ",
                        [],
                    )
                    .map_err(|e| format!("gc_orphans symbols: {e}"))?;
                report.orphaned_symbols_pruned = pruned_sym;

                let pruned_edges = self
                    .conn
                    .execute(
                        r"
                    DELETE FROM call_edges WHERE file_path NOT IN (
                        SELECT path FROM documents WHERE collection = 'external' AND active = 1
                    )
                    ",
                        [],
                    )
                    .map_err(|e| format!("gc_orphans call_edges: {e}"))?;
                report.orphaned_call_edges_pruned = pruned_edges;

                let pruned_imp = self
                    .conn
                    .execute(
                        r"
                    DELETE FROM imports WHERE file_path NOT IN (
                        SELECT path FROM documents WHERE collection = 'external' AND active = 1
                    )
                    ",
                        [],
                    )
                    .map_err(|e| format!("gc_orphans imports: {e}"))?;
                report.orphaned_imports_pruned = pruned_imp;
            }
        }

        if report.orphaned_vectors_pruned > 0
            || report.orphaned_embeddings_pruned > 0
            || report.unreferenced_content_pruned > 0
            || report.orphaned_symbols_pruned > 0
        {
            tracing::info!(
                orphaned_vectors = report.orphaned_vectors_pruned,
                orphaned_embeddings = report.orphaned_embeddings_pruned,
                unreferenced_content = report.unreferenced_content_pruned,
                orphaned_symbols = report.orphaned_symbols_pruned,
                "Memory GC sweep pruned orphaned entries"
            );
        }

        Ok(report)
    }

    /// Run safe memory store maintenance (optimize, passive checkpoint, and conditional vacuum) (#273).
    ///
    /// Default knobs. The maintenance sweep passes its own through
    /// [`Store::vacuum_memory_with`] so the freelist gate and the reclaim
    /// budget are always the same numbers (#522).
    pub fn vacuum_memory(&self) -> Result<bool, String> {
        self.vacuum_memory_with(crate::db::MaintenanceKnobs::default())
    }

    /// [`Store::vacuum_memory`] under caller-supplied knobs (#522).
    pub fn vacuum_memory_with(&self, knobs: crate::db::MaintenanceKnobs) -> Result<bool, String> {
        crate::db::execute_safe_maintenance(&self.conn, "memory.db", knobs)
            .map_err(|e| format!("vacuum_memory: {e}"))
    }
}
