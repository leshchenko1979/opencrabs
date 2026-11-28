//! Tree-sitter based symbol and call-edge extraction for the code graph.
//!
//! Parses source files into ASTs and extracts definitions (functions, types,
//! imports, and — since #1073 — Python classes and methods) plus caller →
//! callee edges, which `memory::db` stores as the symbol graph that
//! `memory_search` with `scope="external"` answers "who calls X" from.
//!
//! Layout, one language per file so a new grammar is a new module and never an
//! edit to an existing language's arms:
//!
//! - [`types`] — the shared vocabulary: [`Symbol`], [`SymbolKind`],
//!   [`CallEdge`], the [`Language`] dispatch table, [`GRAPH_SEMANTICS_VERSION`]
//!   and [`is_parseable_source`] (the one predicate the indexing gates read).
//! - [`extractor`] — [`SymbolExtractor`]: parser ownership and the
//!   extension → grammar dispatch.
//! - [`rust`] — the Rust node-kind walk (`.rs`).
//! - [`python`] — the Python node-kind walk (`.py`, #1073).
//! - [`store`] — [`extract_and_store`], the chokepoint that writes one file's
//!   graph rows.
//!
//! Feature-gated behind `code-graph`. This file is declarations and re-exports
//! only — functions never live in `mod.rs` (CONTRIBUTING.md).

pub(crate) mod extractor;
pub(crate) mod python;
pub(crate) mod rust;
pub(crate) mod store;
pub(crate) mod types;

// One import path per name for every caller inside this module tree
// (`super::SymbolKind` from the language walks, `symbol_extractor::Symbol`
// from `memory::index`). Every name below has a non-test consumer, so the
// non-test lib target never carries an unused re-export.
pub(crate) use extractor::SymbolExtractor;
pub(crate) use store::extract_and_store;
pub(crate) use types::{
    CallEdge, GRAPH_SEMANTICS_VERSION, Language, Symbol, SymbolKind, is_parseable_source,
};
