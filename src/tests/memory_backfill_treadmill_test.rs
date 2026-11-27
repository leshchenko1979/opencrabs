//! Regression tests for the embedding-backfill treadmill fixes (#1069).
//!
//! The defect: two backfill loops ran concurrently (the startup reindex's
//! detached backfill was unguarded), selection was path-ordered so the
//! largest documents sat dead last and were never reached, and `gc_orphans`
//! keyed "orphan" on `active = 0`, so the deactivate/re-insert window of a
//! re-index was a window in which the gc deleted every vector the document
//! owned and the backfill re-selected it forever.
//!
//! These tests exercise the three mechanisms that survived into the fix: the
//! atomic replace (a swap must not orphan the old hash's vectors), a gc whose
//! two legs key the same `active = 1` population (metadata and blobs move
//! together), and the resume cursor that treats "every seq 0..=max present"
//! as done. What is NOT exercised here is the embedding call itself — it is
//! llama.cpp- or network-bound and a test that drives it would prove the
//! model loads, not that the bookkeeping is right. That boundary is the same
//! one `memory_backfill_sweep_test.rs` draws, and it is stated rather than
//! hidden.
//!
//! Also stated: the "deactivated row's vectors are pruned by gc" behaviour is
//! deliberate, not the treadmill. The treadmill's engine was the re-index's
//! deactivate-then-reinsert pair running as two auto-commit statements, which
//! let a gc pass in another process see the intermediate `active = 0` and
//! delete vectors of content that was still correct. `replace_document` makes
//! the swap one transaction, so `active = 0` now means only "a prune leg ran"
//! — a file that genuinely left the index — and its vectors are exactly the
//! space gc exists to reclaim.
//!
//! Stated scope limit: the module has no raw-SQL surface to delete a
//! `documents` row outright, so "row deleted → vectors pruned" is not
//! re-proven here; `test_memory_gc_orphans` (memory_maintenance_test.rs)
//! already proves it with its `hash_orphan` setup — content and vector rows
//! for a hash no document references.

use crate::memory::db::{MemoryGcReport, Store};
use tempfile::tempdir;

const NOW: &str = "2026-02-14T00:00:00Z";

fn open_store() -> (tempfile::TempDir, Store) {
    let dir = tempdir().expect("tempdir");
    let store = Store::open(&dir.path().join("memory.db")).expect("open store");
    store.ensure_vector_table(4).expect("ensure vector table");
    (dir, store)
}

fn embed(store: &Store, hash: &str, seq: usize, chunk_hash: &str) {
    store
        .insert_embedding(
            hash,
            seq,
            0,
            &[1.0, 2.0, 3.0, 4.0],
            "test-model",
            NOW,
            Some(chunk_hash),
        )
        .expect("insert_embedding");
}

fn doc_row(store: &Store, collection: &str, path: &str, title: &str, hash: &str) {
    store
        .insert_document(collection, path, title, hash, NOW, NOW)
        .expect("insert_document");
}

/// (a) An atomic replace must not orphan the old content's vectors.
///
/// The pre-fix path called `deactivate_document` before re-inserting, so a
/// gc_orphans pass landing inside that window saw `active = 0` and deleted
/// the old hash's vector rows, while the backfill saw the same flip and
/// re-selected the document forever. `replace_document` does the swap in one
/// transaction, so a concurrent pass can only observe the row before-or-
/// after, never a hole — and even observing the old hash is harmless once gc
/// keys on "referenced by ANY document row". Assert both halves: the swap
/// leaves all vector rows in place, and they are pruned only once genuinely
/// unreferenced (after the swap, nothing references the old hash).
#[test]
fn a_replace_keeps_the_old_hash_vectors_until_genuinely_unreferenced() {
    let (_dir, store) = open_store();

    store
        .insert_content("hash_old", "old body", NOW)
        .expect("insert old content");
    doc_row(&store, "brain", "SOUL.md", "Soul", "hash_old");
    embed(&store, "hash_old", 0, "ch0");
    embed(&store, "hash_old", 1, "ch1");
    let before = store.vector_stats().expect("stats before");
    assert_eq!(before.vector_rows, 2, "setup: two chunks embedded");
    assert_eq!(before.documents_active, 1);

    // Re-index with new bytes: the content row for the new hash must exist
    // BEFORE the swap — `documents.hash` carries a foreign key into
    // `content(hash)` (PRAGMA foreign_keys = ON) and the swap inserts the new
    // hash into `documents`. This ordering is exactly the one
    // `index_file_sync_keyed` uses after the fix.
    store
        .insert_content("hash_new", "new body", NOW)
        .expect("insert new content");
    let swapped = store
        .replace_document("brain", "SOUL.md", "hash_old", "Soul", "hash_new", NOW)
        .expect("replace_document");
    assert_eq!(swapped, 1, "the row swap must update exactly one row");

    // The swap itself loses no vector rows: content is content-addressed and
    // only the document row moved.
    let after = store.vector_stats().expect("stats after");
    assert_eq!(after.vector_rows, 2, "no vector rows lost by the swap");
    assert_eq!(after.documents_active, 1);
    assert_eq!(
        after.documents_unembedded, 1,
        "the new hash has no seq-0 vector yet, so it is backfill-eligible"
    );

    // And gc prunes the old hash ONLY once no document references it — which
    // is now true, so pruning it here is correct, not racy: the swap moved
    // the row first, atomically. (Inside the swap's transaction window no
    // outside reader can see active=0 at all; test (b) proves the related
    // standalone-deactivation case.)
    let report: MemoryGcReport = store.gc_orphans().expect("gc after swap");
    assert_eq!(
        report.orphaned_vectors_pruned, 2,
        "hash_old is now genuinely unreferenced: pruned, and only it"
    );
    assert_eq!(report.orphaned_embeddings_pruned, 2);
    assert_eq!(
        report.unreferenced_content_pruned, 1,
        "only hash_old's content row is unreferenced; hash_new's is live"
    );
    let gc_stats = store.vector_stats().expect("stats post-gc");
    assert_eq!(gc_stats.vector_rows, 0);
    assert_eq!(
        gc_stats.documents_active, 1,
        "the new document row survives"
    );
}

/// (b) Deactivation is a prune leg, not a mid-replace state, and gc reclaims
/// accordingly: once a document is deactivated, its vector rows are garbage
/// (the space this gc exists to reclaim) — and the deactivated row's own
/// vectors, not those of live neighbours.
///
/// The state this test guards was the treadmill's engine BEFORE the atomic
/// swap: `index_file_sync_keyed` deactivated the row and re-inserted it as
/// two auto-commit statements, so a gc pass in another process could observe
/// the intermediate `active = 0` and delete vectors of content that was still
/// correct, and the document then re-embedded from scratch forever. That
/// window is closed structurally (replace_document is one transaction), so
/// the invariant the tests CAN check in-process is the gc's population
/// contract: active=0 rows' vectors are prune candidates; active=1 rows' are
/// not, whatever their neighbours' state.
#[test]
fn b_deactivation_prunes_vectors_and_spares_active_rows() {
    let (_dir, store) = open_store();

    store
        .insert_content("hash_d", "body", NOW)
        .expect("insert content");
    doc_row(&store, "memory", "2026-02-14.md", "Note", "hash_d");
    embed(&store, "hash_d", 0, "ch0");
    // A second, still-active document whose vectors must NOT be touched.
    store
        .insert_content("hash_live", "live body", NOW)
        .expect("insert live content");
    doc_row(&store, "memory", "2026-02-15.md", "Note 2", "hash_live");
    embed(&store, "hash_live", 0, "cl0");
    let before = store.vector_stats().expect("stats before");
    assert_eq!(before.vector_rows, 2, "setup: one chunk per document");

    // Deactivate one document — the prune-leg call. `active = 0`, row intact.
    store
        .deactivate_document("memory", "2026-02-14.md")
        .expect("deactivate_document");

    // GC prunes the deactivated document's vectors and leaves the active
    // one alone. Both prune legs (metadata and blobs) key the SAME active=1
    // population, so the blob for hash_d goes with its metadata row and the
    // blob for hash_live survives with its own.
    let report = store.gc_orphans().expect("gc with deactivated doc");
    assert_eq!(
        report.orphaned_vectors_pruned, 1,
        "exactly the deactivated document's vector row is garbage"
    );
    assert_eq!(
        report.orphaned_embeddings_pruned, 1,
        "the blob goes with its metadata: the two legs key the same population"
    );
    assert_eq!(report.unreferenced_content_pruned, 0);
    let after = store.vector_stats().expect("stats post-gc");
    assert_eq!(
        after.vector_rows, 1,
        "the active document's vector survives the gc pass"
    );
    assert_eq!(after.documents_active, 1);
    assert_eq!(after.documents_unembedded, 0);
}

/// (c) The resume cursor. "Done" is every seq 0..=max present — not merely a
/// row count and not merely "any row exists" — because a pass that died
/// mid-document must re-enter it, while a completed one must not be re-walked
/// chunk by chunk on every sweep. The ops defect behind this: MEMORY.md
/// (~250 chunks) sat at position 348 of 348 and every pass restarted from the
/// top, so it was re-verified forever and never banked.
#[test]
fn c_document_fully_embedded_cursor() {
    let (_dir, store) = open_store();

    // No rows at all: unembedded, the plain case a backfill starts from.
    // Over an empty set MAX(seq) is NULL, and the first draft of the cursor
    // (`COALESCE(MAX(seq)+1, 0) = COUNT(*)`) evaluated that as `0 = 0` —
    // TRUE for a document with no vectors, which would have made the
    // backfill skip everything. The guard `COUNT(*) > 0 AND …` is what makes
    // this read false.
    assert!(
        !store
            .document_fully_embedded("hash_c")
            .expect("cursor on empty"),
        "no rows means not embedded"
    );

    // A hole: seqs {0, 1, 3} — chunk 2 never landed (a failed embed, a
    // panic, a process restart). MAX(seq)+1 = 4 ≠ COUNT(*) = 3: NOT done, so
    // the backfill re-enters and fills the hole; chunk_needs_embedding keeps
    // chunks 0/1 from being re-embedded.
    embed(&store, "hash_c", 0, "c0");
    embed(&store, "hash_c", 1, "c1");
    embed(&store, "hash_c", 3, "c3");
    assert!(
        !store
            .document_fully_embedded("hash_c")
            .expect("cursor with hole"),
        "a hole in the seqs means not done"
    );

    // Contiguous 0..=2: done. MAX(seq)+1 = 3 = COUNT(*) = 3. This is what
    // lets a later pass skip a document an earlier pass completed, instead of
    // re-verifying all its chunks pass after pass — the treadmill's resume
    // cursor.
    embed(&store, "hash_c", 2, "c2");
    assert!(
        store
            .document_fully_embedded("hash_c")
            .expect("cursor complete"),
        "contiguous seqs 0..=2 means done"
    );
}

/// (e) End to end without the engine: the bookkeeping a backfill pass relies
/// on, through the same public Store methods the embed loop calls.
/// get_hashes_needing_embedding selects the document, chunk_needs_embedding
/// gates each chunk, insert_embedding records it, the document drops out of
/// selection, and the cursor then reports it done — so the next pass, which
/// previously re-walked it from the top, can skip it entirely.
#[test]
fn e_bookkeeping_selects_then_completes() {
    let (_dir, store) = open_store();

    store
        .insert_content("hash_e", "body", NOW)
        .expect("insert content");
    doc_row(&store, "brain", "AGENTS.md", "Agents", "hash_e");

    // Selected for backfill.
    let needing = store.get_hashes_needing_embedding().expect("needing");
    assert_eq!(needing.len(), 1, "the unembedded document is selected");
    assert_eq!(needing[0].0, "hash_e", "selected by hash");
    assert!(
        store
            .chunk_needs_embedding("hash_e", 0, "ch0")
            .expect("chunk 0 before"),
        "no embedding for (hash, seq) yet: needs embedding"
    );

    // A pass embeds seq 0 with the chunk's content hash (#1107).
    let ch0 = Store::hash_content("chunk zero");
    embed(&store, "hash_e", 0, ch0.as_str());

    // The chunk no longer needs work — this is what keeps a re-entered
    // document from re-embedding chunks that already landed.
    assert!(
        !store
            .chunk_needs_embedding("hash_e", 0, ch0.as_str())
            .expect("chunk 0 after"),
        "an embedded, unchanged chunk needs no re-embedding"
    );
    // A changed chunk does.
    assert!(
        store
            .chunk_needs_embedding("hash_e", 0, "different-chunk")
            .expect("chunk 0 changed"),
        "a chunk whose content hash changed needs re-embedding"
    );

    // One chunk is all this tiny body has, so the document drops out of
    // selection and the cursor reports it done: the state the NEXT pass
    // reads when deciding to skip it.
    let needing2 = store.get_hashes_needing_embedding().expect("needing after");
    assert_eq!(
        needing2.len(),
        0,
        "a fully-embedded document is no longer selected"
    );
    assert!(
        store
            .document_fully_embedded("hash_e")
            .expect("cursor after pass"),
        "the pass's work is banked: the cursor reports done"
    );
}
