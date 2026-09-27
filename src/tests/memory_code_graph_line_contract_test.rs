//! The line contract of the external code index's call graph (#489).
//!
//! `memory_search` with `scope="external"` is the sanctioned answer path for
//! "who calls X", so a call edge's line is consumed as a citation by whatever
//! lane reads it. These tests pin that contract: an edge reports the **1-based**
//! line holding the **callee expression itself**, not the line where the call's
//! enclosing expression happens to start.
//!
//! Why the distinction is load-bearing for a method chain: tree-sitter starts a
//! chain's `call_expression` at the *receiver*, so every link of
//! `raw.trim().trim_matches(..).trim()` shares one start position. Reporting
//! that position puts every link on the receiver's line and collapses
//! same-name links into duplicate rows — the two symptoms #489 reports.

#![cfg(feature = "code-graph")]

use crate::memory::symbol_extractor::{CallEdge, SymbolExtractor};
use std::path::PathBuf;

/// A four-link method chain, mirroring the real defect at `context.rs:107`
/// (`trim`, `trim_matches`, `trim_matches`, `trim`). The first and last links
/// share a method name, so a chain-head line collapses three `trim` edges onto
/// a single line and the table gains two duplicate rows.
///
/// Line layout, 1-based — the assertions below depend on it:
/// ```text
/// 1  fn caller() {
/// 2      let raw = "  x  ";
/// 3      let out = raw
/// 4          .trim()
/// 5          .trim_matches('"')
/// 6          .trim_matches(' ')
/// 7          .trim();
/// 8      out
/// 9  }
/// ```
const CHAINED: &str = r#"fn caller() {
    let raw = "  x  ";
    let out = raw
        .trim()
        .trim_matches('"')
        .trim_matches(' ')
        .trim();
    out
}
"#;

/// Every call edge for one source body, as `(callee, line)` sorted for
/// comparison. Ordering is the extractor's walk order, which is not part of the
/// contract under test.
fn edges(source: &str) -> Vec<(String, usize)> {
    let mut extractor = SymbolExtractor::new().expect("extractor");
    let (_symbols, edges) = extractor
        .extract(&PathBuf::from("fixture.rs"), source)
        .expect("extract");
    let mut pairs: Vec<(String, usize)> = edges
        .into_iter()
        .map(|CallEdge { callee, line, .. }| (callee, line))
        .collect();
    pairs.sort();
    pairs
}

#[test]
fn chained_call_reports_the_line_holding_its_own_callee() {
    assert_eq!(
        edges(CHAINED),
        vec![
            ("trim".to_string(), 4),
            ("trim".to_string(), 7),
            ("trim_matches".to_string(), 5),
            ("trim_matches".to_string(), 6),
        ],
        "each link of the chain must report the 1-based line holding its own \
         callee expression: trim at 4 and 7, trim_matches at 5 and 6. A \
         chain-head report instead puts all four on line 3 and emits the \
         same-name links as duplicate rows"
    );
}

#[test]
fn plain_call_reports_the_line_holding_the_callee() {
    let source = "fn caller() {\n    callee();\n}\n\nfn callee() {}\n";
    assert_eq!(
        edges(source),
        vec![("callee".to_string(), 2)],
        "a plain call must report the 1-based line holding the callee"
    );
}

#[test]
fn sequential_calls_report_their_own_one_based_lines() {
    // The file's first line is the `fn caller()` signature, so these two
    // calls sit on lines 2 and 3.
    const SOURCE: &str = r#"fn caller() {
    first();
    second();
}

fn first() {}

fn second() {}
"#;
    assert_eq!(
        edges(SOURCE),
        vec![("first".to_string(), 2), ("second".to_string(), 3)],
        "the first line of the file is line 1, not line 0"
    );
}
