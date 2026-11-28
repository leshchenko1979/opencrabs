//! Python symbol and call-edge extraction (#1073).
//!
//! The code graph used to be Rust-only: `SymbolExtractor::new()` hardcoded the
//! Rust grammar, the indexing chokepoint gated on `.rs`, and `extract_callee`
//! collapsed every name at its last `.`. Python breaks the third one — `.` is
//! the *module* separator there, so `os.path.join` collapsed to `join` would
//! merge it with `str.join` and `Path.join` into one unanswerable name, and the
//! bare-name index strips `.` as well, so nothing downstream could recover it.
//! [`os_path_join_keeps_its_module_path`] is the regression test for that.
//!
//! Line conventions are inherited from the Rust arms, because both languages
//! write into one table: **symbols report 0-based tree-sitter rows, call edges
//! report the 1-based line holding the callee expression.** The asymmetry is
//! pre-existing (#489 moved only edges) and #1073 leaves it alone rather than
//! change Rust output.
//!
//! Expected line numbers are *computed from the fixture* ([`row_of`] /
//! [`line_of`]) rather than hand-counted, so an edit to a fixture cannot leave
//! a stale literal asserting the wrong thing.

#![cfg(feature = "code-graph")]

use crate::memory::COLLECTION_EXTERNAL;
use crate::memory::db::Store;
use crate::memory::index::index_file_sync_keyed;
use crate::memory::symbol_extractor::{
    CallEdge, GRAPH_SEMANTICS_VERSION, Symbol, SymbolExtractor, SymbolKind, is_parseable_source,
};
use std::path::PathBuf;
use tempfile::TempDir;

/// 0-based row of the first fixture line containing `needle`.
fn row_of(source: &str, needle: &str) -> usize {
    source
        .lines()
        .position(|line| line.contains(needle))
        .unwrap_or_else(|| panic!("fixture has no line containing {needle:?}"))
}

/// 1-based line of the first fixture line containing `needle` — the unit call
/// edges report in.
fn line_of(source: &str, needle: &str) -> usize {
    row_of(source, needle) + 1
}

/// Extract from a Python fixture body.
fn python(source: &str) -> (Vec<Symbol>, Vec<CallEdge>) {
    let mut extractor = SymbolExtractor::new().expect("extractor");
    extractor
        .extract(&PathBuf::from("fixture.py"), source)
        .expect("python fixture must parse")
}

/// `(name, kind)` pairs — assertions below are about *which* symbols exist,
/// not the order the walk happens to emit them.
fn named(symbols: &[Symbol]) -> Vec<(String, SymbolKind)> {
    symbols.iter().map(|s| (s.name.clone(), s.kind)).collect()
}

/// `(caller, callee, line)` triples, sorted for comparison. Named `edge_rows`
/// rather than `edges` so no test's `let (_, edges) = python(..)` binding can
/// shadow it out of scope.
fn edge_rows(edges: &[CallEdge]) -> Vec<(String, String, usize)> {
    let mut rows: Vec<(String, String, usize)> = edges
        .iter()
        .map(|e| (e.caller.clone(), e.callee.clone(), e.line))
        .collect();
    rows.sort();
    rows
}

/// (1) A plain `def` becomes a Function symbol with its own line range.
#[test]
fn function_definition_becomes_a_function_symbol() {
    const SOURCE: &str = "def add(a, b):\n    return a + b\n";

    let (symbols, edges) = python(SOURCE);

    assert_eq!(named(&symbols), vec![("add".into(), SymbolKind::Function)]);
    assert!(edges.is_empty(), "no call in this fixture: {edges:?}");

    // Symbols report tree-sitter's 0-based rows, matching the Rust arms: the
    // `def` line through the last line of its body.
    assert_eq!(
        (symbols[0].start_line, symbols[0].end_line),
        (row_of(SOURCE, "def add"), row_of(SOURCE, "return a + b")),
        "a `def` must span its own first line through the last line of its body"
    );
    assert_eq!(symbols[0].file_path, "fixture.py");
}

/// (2) A class is a Class symbol; a `def` in its body is a Method, not a
/// Function — the distinction Rust gets for free from `impl_item`.
#[test]
fn class_body_definitions_are_methods() {
    const SOURCE: &str = r#"class Store:
    def put(self, key):
        return key

    def get(self, key):
        return key
"#;

    let (symbols, _) = python(SOURCE);
    let kinds = named(&symbols);

    assert!(
        kinds.contains(&("Store".into(), SymbolKind::Class)),
        "the class must be a Class symbol, got: {kinds:?}"
    );
    assert!(
        kinds.contains(&("put".into(), SymbolKind::Method)),
        "a def in a class body is a Method, got: {kinds:?}"
    );
    assert!(
        kinds.contains(&("get".into(), SymbolKind::Method)),
        "a def in a class body is a Method, got: {kinds:?}"
    );
    assert!(
        !kinds.iter().any(|(_, kind)| *kind == SymbolKind::Function),
        "nothing here is a free function, got: {kinds:?}"
    );

    let put = symbols
        .iter()
        .find(|s| s.name == "put")
        .expect("put must be extracted");
    assert_eq!(
        (put.start_line, put.end_line),
        (row_of(SOURCE, "def put"), row_of(SOURCE, "return key")),
        "the method's range must cover its own definition only"
    );

    // A `def` nested inside a def is a closure, not a method.
    let (nested, _) = python(
        r#"class C:
    def outer(self):
        def inner():
            return 1
        return inner
"#,
    );
    assert!(
        named(&nested).contains(&("inner".into(), SymbolKind::Function)),
        "a def inside a def is a closure (Function), got: {:?}",
        named(&nested)
    );
}

/// (3) `import os` and `from x import y` become Import symbols, with the module
/// path preserved (`from collections import OrderedDict` → `collections.OrderedDict`).
#[test]
fn imports_become_import_symbols() {
    const SOURCE: &str = r#"import os
import os.path, sys
import numpy as np
from collections import OrderedDict
from . import sibling
from __future__ import annotations
"#;

    let (symbols, _) = python(SOURCE);
    let mut imports: Vec<String> = symbols
        .iter()
        .filter(|s| s.kind == SymbolKind::Import)
        .map(|s| s.name.clone())
        .collect();
    imports.sort();

    assert_eq!(
        imports,
        vec![
            ".sibling".to_string(),
            "__future__.annotations".to_string(),
            "collections.OrderedDict".to_string(),
            "numpy".to_string(),
            "os".to_string(),
            "os.path".to_string(),
            "sys".to_string(),
        ],
        "every import form must land, aliases resolved to the module they name"
    );
    assert!(
        !symbols.iter().any(|s| s.kind != SymbolKind::Import),
        "an import statement defines no other symbol: {symbols:?}"
    );
}

/// (4) A call edge is produced for `foo()`, attributed to the enclosing `def`.
#[test]
fn call_edge_names_the_enclosing_function_as_caller() {
    const SOURCE: &str = "def caller():\n    foo()\n";

    let (_, edges) = python(SOURCE);
    assert_eq!(
        edge_rows(&edges),
        vec![(
            "caller".to_string(),
            "foo".to_string(),
            line_of(SOURCE, "foo()")
        )],
        "the edge must name `caller` and cite the 1-based line holding the callee"
    );

    // Module-level calls have no caller to attribute, exactly as the Rust arm
    // emits nothing outside a function.
    let (_, module_level) = python("foo()\n");
    assert!(
        module_level.is_empty(),
        "a module-level call has no caller: {module_level:?}"
    );
}

/// (5) **The regression test for the whole design.** In Python `.` is the
/// module separator, so `os.path.join` must stay whole; collapsing it to `join`
/// merges it with `str.join` / `Path.join` into one unanswerable name, and the
/// bare-name index strips `.` too, so no downstream layer could recover it.
#[test]
fn os_path_join_keeps_its_module_path() {
    const SOURCE: &str = r#"import os


def build():
    return os.path.join("a", "b")
"#;

    let (_, edges) = python(SOURCE);
    let callees: Vec<String> = edges.iter().map(|e| e.callee.clone()).collect();

    assert_eq!(
        edge_rows(&edges),
        vec![(
            "build".to_string(),
            "os.path.join".to_string(),
            line_of(SOURCE, "os.path.join")
        )],
        "a dotted module path must be stored whole, got: {callees:?}"
    );
    assert!(
        !callees.iter().any(|c| c == "join"),
        "the bare tail is exactly the #1071 ambiguity this rule prevents: {callees:?}"
    );

    // Deeper paths and bare calls keep their own shapes.
    const DEEP: &str = "def f():\n    a.b.c.d()\n    plain()\n";
    let (_, deep) = python(DEEP);
    assert_eq!(
        edge_rows(&deep),
        vec![
            (
                "f".to_string(),
                "a.b.c.d".to_string(),
                line_of(DEEP, "a.b.c.d")
            ),
            (
                "f".to_string(),
                "plain".to_string(),
                line_of(DEEP, "plain()")
            ),
        ],
        "a four-segment path stays whole; a bare call stays bare"
    );
}

/// (6) `self.helper()` strips the receiver — `self` is the one receiver that
/// carries no lookup value — and `cls.build()` does the same.
#[test]
fn self_and_cls_receivers_are_stripped() {
    const SOURCE: &str = r#"class Worker:
    def helper(self):
        return 1

    @classmethod
    def make(cls):
        return cls.build()

    def run(self):
        return self.helper()
"#;

    let (_, edges) = python(SOURCE);
    let callees: Vec<String> = edges.iter().map(|e| e.callee.clone()).collect();

    assert_eq!(
        edge_rows(&edges),
        vec![
            (
                "make".to_string(),
                "build".to_string(),
                line_of(SOURCE, "cls.build()")
            ),
            (
                "run".to_string(),
                "helper".to_string(),
                line_of(SOURCE, "self.helper()")
            ),
        ],
        "self./cls. must reduce to the bare method name, got: {callees:?}"
    );
    assert!(
        !callees
            .iter()
            .any(|c| c.starts_with("self.") || c.starts_with("cls.")),
        "no receiver-qualified self reference may be stored: {callees:?}"
    );

    // A non-self receiver keeps its qualifier: Python's `a.b` is
    // indistinguishable from a module path, and the #1071 bare-name projection
    // recovers the tail for lookup anyway.
    const OTHER: &str = "def f(store):\n    store.insert(1)\n";
    let (_, other) = python(OTHER);
    assert_eq!(
        edge_rows(&other),
        vec![(
            "f".to_string(),
            "store.insert".to_string(),
            line_of(OTHER, "store.insert")
        )],
        "only self/cls are stripped; any other qualifier is information"
    );
}

/// (7) Rust output is unchanged by the module split: the same fixture yields
/// the same symbols and the same edges as before #1073 — one arm per node kind,
/// byte-identical names, kinds and 1-based lines.
#[test]
fn rust_extraction_is_unchanged() {
    const SOURCE: &str = r#"use std::sync::Arc;

struct Point {
    x: f64,
}

trait Drawable {
    fn draw(&self);
}

impl Drawable for Point {
    fn draw(&self) {
        helper();
    }
}

fn helper() -> Arc<u32> {
    Arc::new(1u32)
}
"#;

    let mut extractor = SymbolExtractor::new().expect("extractor");
    let (symbols, edges) = extractor
        .extract(&PathBuf::from("fixture.rs"), SOURCE)
        .expect("rust fixture must parse");

    let mut shape: Vec<(String, SymbolKind)> = named(&symbols);
    // Sort by name only: `SymbolKind` is deliberately not `Ord`, and the
    // fixture holds two same-name symbols (`draw`) whose relative order is
    // walk order, so a kind-inclusive sort key would be both over-coupled
    // and impossible.
    shape.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(
        shape,
        vec![
            ("Drawable".to_string(), SymbolKind::Trait),
            ("Point".to_string(), SymbolKind::Struct),
            // ONE `draw`: the trait's bodyless `fn draw(&self);` parses as
            // `function_signature_item`, which the walk does not claim —
            // pre-#1073 behaviour, pinned as-is.
            ("draw".to_string(), SymbolKind::Function),
            ("helper".to_string(), SymbolKind::Function),
            // `impl_name` reads only the `type` field: "impl " + "Point".
            ("impl Point".to_string(), SymbolKind::Impl),
            ("std::sync::Arc".to_string(), SymbolKind::Import),
        ],
        "the Rust walk must emit exactly these symbols: the trait, the struct, the impl body's \
         `draw` and `helper` as Functions, the impl row named 'impl <type>', and the full use path"
    );

    let mut edge_shape = edge_rows(&edges);
    edge_shape.sort();
    assert_eq!(
        edge_shape,
        vec![
            (
                "draw".to_string(),
                "helper".to_string(),
                line_of(SOURCE, "helper();")
            ),
            (
                "helper".to_string(),
                "Arc::new".to_string(),
                line_of(SOURCE, "Arc::new(1u32)")
            ),
        ],
        "impl-body calls keep the Rust callee rule: `helper` bare (a `.` receiver is stripped), \
         `Arc::new` whole (a `::` path is kept), on the 1-based callee line"
    );
}

/// (8) A non-source extension produces no graph rows at all.
///
/// The indexing gate never calls the extractor for such a file; this pins the
/// extractor's own fallback too, so a future caller that bypasses the gate gets
/// an explicit error rather than a silently empty graph.
#[test]
fn non_source_extension_produces_no_graph_rows() {
    let temp = TempDir::new().unwrap();
    let store = Store::open(&temp.path().join("graph.db")).unwrap();
    store.ensure_symbol_tables().unwrap();

    let key = "/fixture/notes.md";
    const BODY: &str = "# Notes\n\n```python\ndef not_python():\n    pass\n```\n";

    assert!(
        index_file_sync_keyed(&store, COLLECTION_EXTERNAL, key, BODY).unwrap(),
        "the document itself must still be indexed (returns true = new content)"
    );

    // No symbol, edge or import rows may exist for the markdown file. The
    // schema is empty in a fresh store, so any row at all would have to have
    // come from extraction.
    let mut stmt = store
        .conn()
        .prepare("SELECT COUNT(*) FROM symbols UNION ALL SELECT COUNT(*) FROM call_edges UNION ALL SELECT COUNT(*) FROM imports")
        .unwrap();
    let mut counts: Vec<i64> = Vec::new();
    let mut rows = stmt.query([]).unwrap();
    while let Some(row) = rows.next().unwrap() {
        counts.push(row.get::<_, i64>(0).unwrap());
    }
    assert_eq!(
        counts,
        vec![0, 0, 0],
        "a .md file must produce no graph rows, got {counts:?}"
    );
    assert_eq!(
        store.graph_semantics_version(key),
        None,
        "no extraction means no semantics stamp either"
    );

    // The extractor's own fallback for an unregistered extension is an error,
    // not an empty result: an empty Ok would be indistinguishable from "this
    // file defines nothing" for a caller that bypassed the gate.
    let mut extractor = SymbolExtractor::new().expect("extractor");
    let result = extractor.extract(&PathBuf::from("notes.md"), BODY);
    assert!(
        result.is_err(),
        "extract on an unregistered extension must be an explicit error, got {result:?}"
    );

    // And the shared predicate the indexing gates read agrees.
    assert!(!is_parseable_source(key), ".md is not a parseable source");
    assert!(is_parseable_source("/fixture/mod.rs"));
    assert!(is_parseable_source("/fixture/mod.py"));
}

/// (9) The stamp contract, Python side: a `.py` file gets the current
/// `GRAPH_SEMANTICS_VERSION`, and re-indexing the SAME bytes is then skipped —
/// the mirror of `memory_graph_semantics_stamp_test`'s Rust controls, which
/// proves the bump to 3 did not break the unchanged-content fast path for the
/// language that motivated it.
#[test]
fn python_file_stamps_and_skips_on_unchanged_reindex() {
    let temp = TempDir::new().unwrap();
    let store = Store::open(&temp.path().join("graph.db")).unwrap();
    store.ensure_symbol_tables().unwrap();

    let key = "/fixture/store.py";
    const BODY: &str = "def put(key):\n    return key\n";

    assert!(
        index_file_sync_keyed(&store, COLLECTION_EXTERNAL, key, BODY).unwrap(),
        "the first index must write"
    );
    assert_eq!(
        store.graph_semantics_version(key),
        Some(GRAPH_SEMANTICS_VERSION),
        "the Python extract must stamp the current semantics"
    );

    // The graph actually landed: `put` is a Function symbol.
    let kinds: Vec<String> = store
        .query_symbols_by_name("put")
        .unwrap()
        .into_iter()
        .map(|(kind, _, _, _)| kind)
        .collect();
    assert_eq!(
        kinds,
        vec!["function".to_string()],
        "the extracted Function must be stored under its lowercase Display kind"
    );

    let second = index_file_sync_keyed(&store, COLLECTION_EXTERNAL, key, BODY).unwrap();
    assert!(
        !second,
        "with a current stamp and an unchanged hash the Python file must be skipped, \
         exactly like a Rust one"
    );
}
