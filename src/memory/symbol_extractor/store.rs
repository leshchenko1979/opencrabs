//! The indexing chokepoint: one file's source in, its graph rows out.
//!
//! Called from `memory::index::index_file_sync_keyed`, so every path that
//! indexes content — the cold external walk, the periodic sweep, the lazy
//! refresh — writes the graph from this one place.

use super::super::db::Store;
use super::{GRAPH_SEMANTICS_VERSION, SymbolExtractor, SymbolKind};

/// Extract symbols, call edges and imports from one source file and store them
/// in the symbol graph.
///
/// Creating a fresh [`SymbolExtractor`] per file is deliberate: the chokepoint
/// is a free function with no state to carry a parser across calls, and parser
/// construction is a cheap once-per-file allocation.
pub(crate) fn extract_and_store(store: &Store, key: &str, body: &str) {
    let mut extractor = match SymbolExtractor::new() {
        Ok(e) => e,
        Err(e) => {
            tracing::debug!("code-graph: extractor init failed: {e}");
            return;
        }
    };
    let path = std::path::Path::new(key);
    match extractor.extract(path, body) {
        Ok((symbols, call_edges)) => {
            // Replace, not append: re-indexing a changed file must not stack a
            // second copy of its graph (#522). On a replace failure we return
            // WITHOUT inserting: the transaction rolled back, so the file keeps
            // its previous graph (stale) rather than gaining a second copy
            // (doubled). For a row-doubling defect, stale is the safe direction.
            if let Err(e) = store.replace_file_graph(key) {
                tracing::warn!("code-graph: replace_file_graph {key}: {e}");
                return;
            }
            // Definitions (everything except imports)
            for sym in symbols.iter().filter(|s| s.kind != SymbolKind::Import) {
                if let Err(e) = store.insert_symbol(
                    &sym.name,
                    &sym.kind.to_string(),
                    key,
                    sym.start_line,
                    sym.end_line,
                ) {
                    tracing::debug!("code-graph: insert_symbol {key} {}: {e}", sym.name);
                }
            }
            // Caller -> callee edges
            for edge in call_edges {
                if let Err(e) = store.insert_call_edge(&edge.caller, &edge.callee, key, edge.line) {
                    tracing::debug!("code-graph: insert_call_edge {key} {}: {e}", edge.caller);
                }
            }
            // Imports tracked separately
            for sym in symbols.iter().filter(|s| s.kind == SymbolKind::Import) {
                if let Err(e) = store.insert_import(&sym.name, key, sym.start_line) {
                    tracing::debug!("code-graph: insert_import {key} {}: {e}", sym.name);
                }
            }
            // Stamp only after the rows landed. A failed replace returns early
            // above, and a stamp on a graph that was not written would hide the
            // file from every later pass (#489).
            if let Err(e) = store.stamp_graph_semantics(key, GRAPH_SEMANTICS_VERSION) {
                tracing::debug!("code-graph: stamp_graph_semantics {key}: {e}");
            }
        }
        Err(e) => tracing::debug!("code-graph: failed to extract symbols from {key}: {e}"),
    }
}
