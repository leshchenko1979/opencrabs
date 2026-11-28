//! Bare-name lookup of path-qualified call edges (#1071).
//!
//! `call_edges.callee_symbol` stores BOTH bare (`clone`) and path-qualified
//! (`super::embedding::run_api_backfill`) forms, while structural queries
//! extract a bare identifier and used to look it up with exact equality —
//! which could never see the qualified 22% of a live index. These tests pin
//! the exact-first-then-normalized contract: the projection lookup finds what
//! exact match misses, exact match keeps working for bare rows, ambiguous
//! bare names group per stored source, and the driving predicate stays
//! pinned to the `idx_call_edges_callee_bare` expression index.
//!
//! Graph-tier throughout — the whole file is gated on `code-graph` like the
//! query layer it exercises.

#![cfg(feature = "code-graph")]

use crate::memory::db::Store;
use crate::memory::search::search_symbol_graph;
use tempfile::TempDir;

/// The bare-name projection SQLite expression, character-for-character the
/// one in `ensure_symbol_tables` and `query_callers_of_qualified` (#1071).
/// The EXPLAIN QUERY PLAN test below asserts the planner actually uses
/// `idx_call_edges_callee_bare` for this exact text.
const BARE_PROJECTION_SQL: &str = "substr(callee_symbol, length(rtrim(callee_symbol,'abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789_'))+1)";

fn store_with_edges(edges: &[(&str, &str, &str, usize)]) -> (TempDir, Store) {
    let temp = TempDir::new().unwrap();
    let store = Store::open(&temp.path().join("bare-lookup.db")).unwrap();
    store.ensure_symbol_tables().unwrap();
    for (caller, callee, file, line) in edges {
        store.insert_call_edge(caller, callee, file, *line).unwrap();
    }
    (temp, store)
}

/// (a) An edge inserted with a QUALIFIED callee is found by the normalized
/// lookup and NOT by the exact lookup — the negative half is the control
/// that proves the fallback adds reach rather than duplicating it.
#[test]
fn qualified_callee_found_by_normalized_not_by_exact() {
    let (_temp, store) = store_with_edges(&[(
        "run_backfill",
        "super::embedding::run_api_backfill",
        "src/ops.rs",
        42,
    )]);

    let exact = store.query_callers_of("run_api_backfill").unwrap();
    assert!(
        exact.is_empty(),
        "exact match must NOT see a qualified edge (control): got {exact:?}"
    );

    let normalized = store
        .query_callers_of_qualified("run_api_backfill")
        .unwrap();
    assert_eq!(
        normalized,
        vec![(
            "run_backfill".to_string(),
            "src/ops.rs".to_string(),
            42,
            "super::embedding::run_api_backfill".to_string(),
        )],
        "normalized lookup must find the qualified edge and return the STORED form"
    );
}

/// (b) An edge inserted with a BARE callee is still found by the exact
/// lookup — no regression on the 78% — and the normalized lookup sees the
/// same row, so the two layers agree on bare storage.
#[test]
fn bare_callee_still_found_by_exact_lookup() {
    let (_temp, store) = store_with_edges(&[("handler", "clone", "src/basic.rs", 7)]);

    let exact = store.query_callers_of("clone").unwrap();
    assert_eq!(
        exact,
        vec![("handler".to_string(), "src/basic.rs".to_string(), 7)],
        "exact lookup must keep finding bare-callee rows"
    );

    let normalized = store.query_callers_of_qualified("clone").unwrap();
    assert_eq!(
        normalized.len(),
        1,
        "normalized lookup must also see the bare row (projection of a bare value is itself)"
    );
    assert_eq!(
        normalized[0].3, "clone",
        "matched stored form for a bare row is the bare name itself"
    );
}

/// (c) Ambiguity: two qualified sources of the same bare name (`Vec::new`,
/// `Arc::new`) both come back, distinguishable by their stored forms — the
/// field the render layer groups on.
#[test]
fn ambiguous_bare_name_returns_distinct_stored_sources() {
    let (_temp, store) = store_with_edges(&[
        ("make_vec", "Vec::new", "src/a.rs", 1),
        ("make_arc", "Arc::new", "src/b.rs", 2),
    ]);

    let rows = store.query_callers_of_qualified("new").unwrap();
    assert_eq!(rows.len(), 2, "both qualified sources must come back");
    let sources: std::collections::BTreeSet<&str> =
        rows.iter().map(|(_, _, _, src)| src.as_str()).collect();
    assert_eq!(
        sources,
        ["Arc::new", "Vec::new"].into_iter().collect(),
        "the caller must be able to tell Vec::new and Arc::new apart"
    );

    // The grouped view carries each source as its own batch, sorted by the
    // stored form, with the exact 4-tuple collapsed to its caller triple.
    let grouped = store.query_callers_of_qualified_grouped("new").unwrap();
    assert_eq!(
        grouped,
        vec![
            (
                "Arc::new".to_string(),
                vec![("make_arc".to_string(), "src/b.rs".to_string(), 2)],
            ),
            (
                "Vec::new".to_string(),
                vec![("make_vec".to_string(), "src/a.rs".to_string(), 1)],
            ),
        ],
        "grouped lookup must yield one batch per stored source, sorted by source"
    );
}

/// (d) The plan pin: the normalized predicate must be resolved through the
/// expression index, not a full scan. If someone later edits the projection
/// expression in either the index or `query_callers_of_qualified` so the two
/// stop matching character-for-character, SQLite silently degrades to SCAN
/// over every call edge — this test fails instead of the live index.
#[test]
fn bare_name_predicate_uses_the_expression_index() {
    let (_temp, store) = store_with_edges(&[
        (
            "run_backfill",
            "super::embedding::run_api_backfill",
            "src/ops.rs",
            42,
        ),
        ("handler", "clone", "src/basic.rs", 7),
    ]);

    let sql = format!(
        "EXPLAIN QUERY PLAN SELECT caller_symbol FROM call_edges WHERE {BARE_PROJECTION_SQL} = 'run_api_backfill';"
    );
    let mut stmt = store.conn().prepare(&sql).unwrap();
    let mut plan_lines: Vec<String> = Vec::new();
    let mut rows = stmt.query([]).unwrap();
    while let Some(row) = rows.next().unwrap() {
        plan_lines.push(row.get::<_, String>(3).unwrap());
    }
    let plan = plan_lines.join(" | ");
    assert!(
        plan.contains("idx_call_edges_callee_bare"),
        "normalized predicate must resolve via idx_call_edges_callee_bare; plan was: {plan}"
    );
    assert!(
        plan.contains("SEARCH"),
        "plan must SEARCH the index, not SCAN the table; plan was: {plan}"
    );
    assert!(
        !plan.contains("SCAN call_edges"),
        "normalized predicate must not fall back to a full table scan; plan was: {plan}"
    );
}

/// The render layer: exact hit stays the flat single-source listing (78%
/// path, unchanged behaviour).
#[test]
fn search_calls_exact_hit_renders_flat_as_before() {
    let (_temp, store) = store_with_edges(&[
        ("handler", "clone", "src/basic.rs", 7),
        ("other", "clone", "src/basic.rs", 9),
    ]);

    let results = search_symbol_graph(&store, "calls", "clone", 25, 0).unwrap();
    assert_eq!(results.len(), 2, "exact hit renders without group headers");
    assert!(
        results
            .iter()
            .all(|r| r.snippet.contains("calls clone at line")),
        "flat rendering unchanged, got {results:?}"
    );
}

/// The render layer: a bare name with zero bare-callee rows but TWO
/// qualified sources groups under one header per source — this is the only
/// way `Vec::new` stays distinguishable from `Arc::new` in the answer.
#[test]
fn search_calls_groups_ambiguous_qualified_sources_by_header() {
    let (_temp, store) = store_with_edges(&[
        ("make_vec", "Vec::new", "src/a.rs", 1),
        ("make_arc", "Arc::new", "src/b.rs", 2),
    ]);

    let results = search_symbol_graph(&store, "calls", "new", 25, 0).unwrap();
    // One header per source, in stored-form order, each followed by its rows.
    assert_eq!(
        results.len(),
        4,
        "2 headers + 2 caller lines, got {results:?}"
    );
    assert_eq!(
        results[0].snippet, "Arc::new (1 callers via this name)",
        "first header is the first stored source with its count"
    );
    assert!(results[0].path.is_empty(), "header lines render bare");
    assert_eq!(
        results[1].snippet, "make_arc calls Arc::new at line 2",
        "caller line names the STORED source, not the bare name"
    );
    assert_eq!(
        results[2].snippet, "Vec::new (1 callers via this name)",
        "second header is the second stored source"
    );
    assert_eq!(
        results[3].snippet, "make_vec calls Vec::new at line 1",
        "each caller line follows its own header"
    );
}

/// The render layer: a bare name with exactly ONE qualified source renders
/// flat — grouping is reserved for genuinely ambiguous names.
#[test]
fn search_calls_single_qualified_source_renders_flat() {
    let (_temp, store) = store_with_edges(&[(
        "run_backfill",
        "super::embedding::run_api_backfill",
        "src/ops.rs",
        42,
    )]);

    let results = search_symbol_graph(&store, "calls", "run_api_backfill", 25, 0).unwrap();
    assert_eq!(results.len(), 1, "single source renders without headers");
    assert_eq!(
        results[0].snippet, "run_backfill calls run_api_backfill at line 42",
        "flat snippet names the query symbol as before"
    );
}

/// Pagination inside the grouped rendering: per-source take(n) plus the
/// ordinary truncation marker keeps the #89 contract intact. The per-source
/// marker only fires on a batch larger than the effective cap — the
/// structural floor (MIN_STRUCTURAL_RESULTS, 25) — so the batch is built at
/// that size.
#[test]
fn search_calls_grouped_rendering_respects_n_and_truncation() {
    let mut edges: Vec<(&str, &str, &str, usize)> = vec![("make_arc", "Arc::new", "src/b.rs", 2)];
    // 26 Vec::new callers: one over the floor, so the Vec::new batch must
    // carry a truncation marker while Arc::new's stays complete.
    for i in 0..26 {
        let name: &'static str = Box::leak(format!("vec_caller_{:02}", i).into_boxed_str());
        edges.push((name, "Vec::new", "src/a.rs", 10 + i as usize));
    }
    let (_temp, store) = store_with_edges(&edges);

    // n=1 sits below the floor and is clamped up to MIN_STRUCTURAL_RESULTS
    // (25): the Vec::new batch truncates, the Arc::new batch does not.
    let results = search_symbol_graph(&store, "calls", "new", 1, 0).unwrap();
    // Arc::new: 1 header + 1 caller line. Vec::new: 1 header + 25 caller
    // lines + 1 marker.
    assert_eq!(
        results.len(),
        1 + 1 + 1 + 25 + 1,
        "per-source marker must fire on the 26-row batch, got {results:?}"
    );
    let markers: Vec<&str> = results
        .iter()
        .map(|r| r.snippet.as_str())
        .filter(|s| s.contains("more callers of"))
        .collect();
    assert_eq!(
        markers.len(),
        1,
        "exactly one source must truncate, got {markers:?}"
    );
    assert!(
        markers[0].contains("1 more callers of Vec::new"),
        "the truncated marker must name Vec::new, got {}",
        markers[0]
    );
    assert!(
        results
            .iter()
            .any(|r| r.snippet == "Arc::new (1 callers via this name)"),
        "the complete source still renders its header, got {results:?}"
    );
}

/// Transitive walkers must see through qualified callees at depth >= 2: the
/// recursive member joins `callee_symbol = caller_symbol`, and
/// `caller_symbol` is bare — so before #1071 every qualified edge was
/// invisible to the recursion.
#[test]
fn transitive_callers_find_qualified_edges_at_depth_two() {
    // handler -> Vec::new (qualified); vec_hub -> handler (bare). Asking for
    // the transitive impact of `new` must reach vec_hub through BOTH hops.
    let (_temp, store) = store_with_edges(&[
        ("handler", "Vec::new", "src/a.rs", 1),
        ("vec_hub", "handler", "src/b.rs", 2),
    ]);

    let rows = store.query_transitive_callers("new", 3).unwrap();
    assert!(
        rows.iter()
            .any(|(caller, _, _, depth)| caller == "handler" && *depth == 1),
        "depth-1 caller via the qualified edge must be found, got {rows:?}"
    );
    assert!(
        rows.iter()
            .any(|(caller, _, _, depth)| caller == "vec_hub" && *depth == 2),
        "depth-2 caller reached THROUGH the qualified edge must be found, got {rows:?}"
    );
}

/// The transitive seed must not double-count a bare callee: the projection
/// member is guarded by `callee_symbol <> ?1`, so a bare row matched by the
/// exact member stays one row after UNION.
#[test]
fn transitive_callers_bare_seed_is_not_double_seeded() {
    let (_temp, store) = store_with_edges(&[
        ("handler", "clone", "src/basic.rs", 7),
        ("clone_hub", "handler", "src/basic.rs", 8),
    ]);

    let rows = store.query_transitive_callers("clone", 3).unwrap();
    let handler_rows: Vec<_> = rows
        .iter()
        .filter(|(caller, _, _, _)| caller == "handler")
        .collect();
    assert_eq!(
        handler_rows.len(),
        1,
        "bare callee must appear exactly once at its shallowest depth, got {rows:?}"
    );
    assert!(
        rows.iter()
            .any(|(caller, _, _, depth)| caller == "clone_hub" && *depth == 2),
        "depth-2 walker must still run over bare storage, got {rows:?}"
    );
}
