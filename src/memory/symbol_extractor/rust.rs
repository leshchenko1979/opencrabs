//! The Rust node-kind walk (`.rs`).
//!
//! Lifted verbatim out of the single-file extractor in #1073 — the arms, their
//! order and their line conventions are unchanged, so every `.rs` graph row
//! this walk produces is byte-identical to the pre-#1073 output. Any change
//! here is a `GRAPH_SEMANTICS_VERSION` bump.

use tree_sitter::Node;

use super::types::node_text;
use super::{CallEdge, Symbol, SymbolKind};

/// Walk one Rust syntax tree, appending symbols and call edges.
pub(crate) fn extract(
    root: Node<'_>,
    source: &str,
    file_path: &str,
    symbols: &mut Vec<Symbol>,
    call_edges: &mut Vec<CallEdge>,
) {
    walk(
        root, source, file_path, None, // no current function context at root
        symbols, call_edges,
    );
}

fn walk(
    node: Node<'_>,
    source: &str,
    file_path: &str,
    current_function: Option<&str>,
    symbols: &mut Vec<Symbol>,
    call_edges: &mut Vec<CallEdge>,
) {
    let kind = node.kind();

    match kind {
        "function_item" => {
            if let Some(name) = child_text(node, "name", source) {
                let start_line = node.start_position().row;
                let end_line = node.end_position().row;
                symbols.push(Symbol {
                    name: name.clone(),
                    kind: SymbolKind::Function,
                    file_path: file_path.to_string(),
                    start_line,
                    end_line,
                });

                // Recurse into function body with this function as context
                for child in node.children(&mut node.walk()) {
                    walk(child, source, file_path, Some(&name), symbols, call_edges);
                }
            }
        }
        "struct_item" => {
            if let Some(name) = child_text(node, "name", source) {
                symbols.push(Symbol {
                    name,
                    kind: SymbolKind::Struct,
                    file_path: file_path.to_string(),
                    start_line: node.start_position().row,
                    end_line: node.end_position().row,
                });
            }
        }
        "enum_item" => {
            if let Some(name) = child_text(node, "name", source) {
                symbols.push(Symbol {
                    name,
                    kind: SymbolKind::Enum,
                    file_path: file_path.to_string(),
                    start_line: node.start_position().row,
                    end_line: node.end_position().row,
                });
            }
        }
        "trait_item" => {
            if let Some(name) = child_text(node, "name", source) {
                symbols.push(Symbol {
                    name,
                    kind: SymbolKind::Trait,
                    file_path: file_path.to_string(),
                    start_line: node.start_position().row,
                    end_line: node.end_position().row,
                });
            }
        }
        "impl_item" => {
            // Extract impl block (trait implementation), then recurse:
            // impl bodies contain `function_item` methods, and stopping
            // here made every impl method in the codebase invisible to the
            // symbols table (#1325 — `insert_symbol`, `query_callers_of`
            // had edges but no symbol rows).
            symbols.push(Symbol {
                name: impl_name(node, source),
                kind: SymbolKind::Impl,
                file_path: file_path.to_string(),
                start_line: node.start_position().row,
                end_line: node.end_position().row,
            });
            for child in node.children(&mut node.walk()) {
                walk(
                    child,
                    source,
                    file_path,
                    current_function,
                    symbols,
                    call_edges,
                );
            }
        }
        "use_declaration" => {
            // Extract the full use path (e.g., "std::collections::HashMap")
            let import_path = node
                .child_by_field_name("argument")
                .map(|n| node_text(n, source).to_string())
                .unwrap_or_else(|| "use".to_string());
            symbols.push(Symbol {
                name: import_path,
                kind: SymbolKind::Import,
                file_path: file_path.to_string(),
                start_line: node.start_position().row,
                end_line: node.end_position().row,
            });
        }
        "call_expression" => {
            // Extract function call, then recurse: calls nest inside other
            // calls' subtrees (receiver `.await.context()` chains, call
            // arguments, method chains) and each nested call is its own
            // edge (#1325 — `retry_db_operation(..).await.context(..)`
            // dropped the inner edge when this arm stopped after handling).
            if let (Some(caller), Some(callee)) = (current_function, callee(node, source)) {
                call_edges.push(CallEdge {
                    caller: caller.to_string(),
                    callee,
                    file_path: file_path.to_string(),
                    line: callee_line(node),
                });
            }
            for child in node.children(&mut node.walk()) {
                walk(
                    child,
                    source,
                    file_path,
                    current_function,
                    symbols,
                    call_edges,
                );
            }
        }
        _ => {
            // Recurse into children
            for child in node.children(&mut node.walk()) {
                walk(
                    child,
                    source,
                    file_path,
                    current_function,
                    symbols,
                    call_edges,
                );
            }
        }
    }
}

fn child_text(node: Node<'_>, field: &str, source: &str) -> Option<String> {
    node.child_by_field_name(field)
        .map(|n| node_text(n, source).to_string())
}

/// "impl Trait for Type" or "impl Type".
fn impl_name(node: Node<'_>, source: &str) -> String {
    let mut result = String::from("impl");
    if let Some(type_node) = node.child_by_field_name("type") {
        result.push(' ');
        result.push_str(node_text(type_node, source));
    }
    result
}

/// The 1-based line a call edge reports (#489).
///
/// This is the **callee expression's own** end, not the `call_expression`'s
/// start. A method chain's `call_expression` begins at the *receiver*, so
/// every link of `a.trim().trim_matches(x).trim()` shares that start: they
/// all report the receiver's line, which puts a citation on a line that
/// does not hold the call, and collapses same-name links onto one line so
/// they store as duplicate rows. The callee expression ends at its own
/// method name, which is the line a reader looking for the call needs.
///
/// tree-sitter rows are 0-based; citations are 1-based.
fn callee_line(node: Node<'_>) -> usize {
    node.child_by_field_name("function")
        .map(|callee| callee.end_position().row + 1)
        .unwrap_or_else(|| node.start_position().row + 1)
}

fn callee(node: Node<'_>, source: &str) -> Option<String> {
    // Extract the function being called.
    // Receiver-qualified calls (`store.insert_symbol(..)`, `lines.push(..)`)
    // are normalized to the bare method name (`insert_symbol`, `push`):
    // the receiver is a local variable name that carries no lookup value,
    // and callers-of queries search by bare method name. Module paths
    // (`Arc::new`, `Vec::new`) keep their full path — no `.` receiver.
    let raw = node
        .child_by_field_name("function")
        .map(|n| node_text(n, source).to_string())?;
    let callee = match raw.rsplit_once('.') {
        Some((_, method)) => method.to_string(),
        None => raw,
    };
    // Skip enum-variant constructor noise: `Some(x)`, `Ok(x)`, `Err(e)`.
    // tree-sitter cannot distinguish these from function calls
    // syntactically, and they otherwise dominate the edge table
    // (1278 + 698 edges of garbage in the 1383-file repo benchmark).
    if matches!(callee.as_str(), "Some" | "Ok" | "Err" | "None") {
        return None;
    }
    Some(callee)
}
