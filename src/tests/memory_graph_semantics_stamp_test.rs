//! The graph-semantics stamp (#489).
//!
//! A file's content hash cannot say which extractor semantics wrote its graph.
//! Without a separate stamp, a file whose bytes are unchanged keeps rows written
//! by superseded semantics for as long as it exists: `index_file_sync_keyed`
//! returns on the hash match *before* it reaches the extractor, and every other
//! path — the cold walk, the periodic sweep, the lazy refresh — funnels through
//! it, so nothing else revisits the file either.

#![cfg(feature = "code-graph")]

use crate::memory::COLLECTION_EXTERNAL;
use crate::memory::db::Store;
use crate::memory::index::index_file_sync_keyed;
use crate::memory::symbol_extractor::GRAPH_SEMANTICS_VERSION;
use tempfile::TempDir;

/// A four-link method chain — the shape that produced duplicate rows under the
/// pre-#489 line computation, because every link shared the receiver's start.
const CHAIN: &str = r#"fn caller() {
    let raw = "  x  ";
    let out = raw
        .trim()
        .trim_matches('"')
        .trim_matches(' ')
        .trim();
    out
}
"#;

/// Distinct rows in a `query_callees_of` result, i.e. the count a correct graph
/// should hold.
fn distinct(rows: &[(String, String, usize)]) -> usize {
    let mut sorted = rows.to_vec();
    sorted.sort();
    sorted.dedup();
    sorted.len()
}

#[test]
fn a_stale_stamp_re_extracts_a_file_whose_content_is_unchanged() {
    let temp = TempDir::new().unwrap();
    let store = Store::open(&temp.path().join("graph.db")).unwrap();
    store.ensure_symbol_tables().unwrap();

    let key = "/fixture/chain.rs";

    // First index: writes the graph and stamps the semantics that wrote it.
    assert!(index_file_sync_keyed(&store, COLLECTION_EXTERNAL, key, CHAIN).unwrap());
    assert_eq!(
        store.graph_semantics_version(key),
        Some(GRAPH_SEMANTICS_VERSION),
        "a successful extract must stamp the semantics that wrote it"
    );

    // Residue exactly as an older extractor would leave it: an extra edge
    // sharing every grouping column, and a stamp behind the current semantics.
    store.insert_call_edge("caller", "trim", key, 4).unwrap();
    store.stamp_graph_semantics(key, 1).unwrap();
    let before = store.query_callees_of("caller").unwrap();
    assert!(
        before.len() > distinct(&before),
        "the fixture must start with duplicate rows or this test proves nothing: {before:?}"
    );

    // Re-index the SAME bytes: the content hash matches, so only the stamp can
    // force the re-extract.
    let wrote = index_file_sync_keyed(&store, COLLECTION_EXTERNAL, key, CHAIN).unwrap();
    assert!(wrote, "a stale stamp must report the re-extract as work");

    let after = store.query_callees_of("caller").unwrap();
    assert_eq!(
        after.len(),
        distinct(&after),
        "after the stamp-driven re-extract the file must hold no duplicate rows: {after:?}"
    );
    assert_eq!(
        store.graph_semantics_version(key),
        Some(GRAPH_SEMANTICS_VERSION),
        "the re-extract must restamp, or the file is re-extracted on every pass"
    );
}

#[test]
fn a_current_stamp_leaves_an_unchanged_file_alone() {
    let temp = TempDir::new().unwrap();
    let store = Store::open(&temp.path().join("graph.db")).unwrap();
    store.ensure_symbol_tables().unwrap();

    let key = "/fixture/stable.rs";
    assert!(index_file_sync_keyed(&store, COLLECTION_EXTERNAL, key, CHAIN).unwrap());

    let second = index_file_sync_keyed(&store, COLLECTION_EXTERNAL, key, CHAIN).unwrap();
    assert!(
        !second,
        "with a current stamp and an unchanged hash the file must be skipped"
    );
}
