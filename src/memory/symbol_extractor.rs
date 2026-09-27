//! Tree-sitter based symbol extraction for Rust source files.
//!
//! Parses `.rs` files into ASTs and extracts:
//! - Function definitions (name, file, line range)
//! - Function calls (caller → callee edges)
//! - Struct/enum/trait definitions
//! - impl blocks (trait implementations)
//! - Use statements (imports)
//!
//! Feature-gated behind `code-graph`.

#[cfg(feature = "code-graph")]
use std::path::Path;
#[cfg(feature = "code-graph")]
use tree_sitter::{Node, Parser};

/// A symbol extracted from source code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Symbol {
    pub name: String,
    pub kind: SymbolKind,
    pub file_path: String,
    pub start_line: usize,
    pub end_line: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SymbolKind {
    Function,
    Struct,
    Enum,
    Trait,
    Impl,
    Import,
}

impl std::fmt::Display for SymbolKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SymbolKind::Function => write!(f, "function"),
            SymbolKind::Struct => write!(f, "struct"),
            SymbolKind::Enum => write!(f, "enum"),
            SymbolKind::Trait => write!(f, "trait"),
            SymbolKind::Impl => write!(f, "impl"),
            SymbolKind::Import => write!(f, "import"),
        }
    }
}

/// A call edge: caller function calls callee function.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallEdge {
    pub caller: String,
    pub callee: String,
    pub file_path: String,
    pub line: usize,
}

/// The extractor semantics a file's graph was written with (#489).
///
/// Bump this whenever a change makes already-stored graph rows wrong: a line
/// computation, a callee normalization, a symbol kind. The content hash cannot
/// observe such a change, so without the stamp a file whose bytes are unchanged
/// keeps rows written by the superseded semantics for as long as it exists.
///
/// 1 — everything before #489: a call site reported at the `call_expression`
///     start, i.e. the receiver's line, and 0-based.
/// 2 — #489: the callee expression's own line, 1-based.
#[cfg(feature = "code-graph")]
pub(crate) const GRAPH_SEMANTICS_VERSION: u32 = 2;

/// Extracts symbols and call graphs from Rust source files using tree-sitter.
#[cfg(feature = "code-graph")]
pub struct SymbolExtractor {
    parser: Parser,
}

#[cfg(feature = "code-graph")]
impl SymbolExtractor {
    pub fn new() -> Result<Self, anyhow::Error> {
        let mut parser = Parser::new();
        let language = tree_sitter_rust::LANGUAGE;
        parser.set_language(&language.into())?;
        Ok(Self { parser })
    }

    /// Parse a Rust file and extract all symbols and call edges.
    pub fn extract(
        &mut self,
        file_path: &Path,
        source: &str,
    ) -> Result<(Vec<Symbol>, Vec<CallEdge>), anyhow::Error> {
        let tree = self
            .parser
            .parse(source, None)
            .ok_or_else(|| anyhow::anyhow!("Failed to parse {}", file_path.display()))?;

        let file_path_str = file_path.to_string_lossy().to_string();
        let mut symbols = Vec::new();
        let mut call_edges = Vec::new();

        self.extract_from_node(
            tree.root_node(),
            source,
            &file_path_str,
            None, // no current function context at root
            &mut symbols,
            &mut call_edges,
        );

        Ok((symbols, call_edges))
    }

    fn extract_from_node(
        &self,
        node: Node,
        source: &str,
        file_path: &str,
        current_function: Option<&str>,
        symbols: &mut Vec<Symbol>,
        call_edges: &mut Vec<CallEdge>,
    ) {
        let kind = node.kind();

        match kind {
            "function_item" => {
                if let Some(name) = self.extract_function_name(node, source) {
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
                        self.extract_from_node(
                            child,
                            source,
                            file_path,
                            Some(&name),
                            symbols,
                            call_edges,
                        );
                    }
                }
            }
            "struct_item" => {
                if let Some(name) = self.extract_type_name(node, source) {
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
                if let Some(name) = self.extract_type_name(node, source) {
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
                if let Some(name) = self.extract_type_name(node, source) {
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
                let impl_name = self.extract_impl_name(node, source);
                symbols.push(Symbol {
                    name: impl_name,
                    kind: SymbolKind::Impl,
                    file_path: file_path.to_string(),
                    start_line: node.start_position().row,
                    end_line: node.end_position().row,
                });
                for child in node.children(&mut node.walk()) {
                    self.extract_from_node(
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
                // Extract import
                let import_path = self.extract_use_path(node, source);
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
                if let (Some(caller), Some(callee)) =
                    (current_function, self.extract_callee(node, source))
                {
                    call_edges.push(CallEdge {
                        caller: caller.to_string(),
                        callee,
                        file_path: file_path.to_string(),
                        line: Self::callee_line(node),
                    });
                }
                for child in node.children(&mut node.walk()) {
                    self.extract_from_node(
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
                    self.extract_from_node(
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

    fn extract_function_name(&self, node: Node, source: &str) -> Option<String> {
        node.child_by_field_name("name")
            .map(|n| n.utf8_text(source.as_bytes()).unwrap_or("").to_string())
    }

    fn extract_type_name(&self, node: Node, source: &str) -> Option<String> {
        node.child_by_field_name("name")
            .map(|n| n.utf8_text(source.as_bytes()).unwrap_or("").to_string())
    }

    fn extract_impl_name(&self, node: Node, source: &str) -> String {
        // Try to extract "impl Trait for Type" or "impl Type"
        let mut result = String::from("impl");
        if let Some(type_node) = node.child_by_field_name("type") {
            let type_name = type_node.utf8_text(source.as_bytes()).unwrap_or("");
            result.push(' ');
            result.push_str(type_name);
        }
        result
    }

    fn extract_use_path(&self, node: Node, source: &str) -> String {
        // Extract the full use path (e.g., "std::collections::HashMap")
        node.child_by_field_name("argument")
            .map(|n| n.utf8_text(source.as_bytes()).unwrap_or("").to_string())
            .unwrap_or_else(|| "use".to_string())
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
    fn callee_line(node: Node) -> usize {
        node.child_by_field_name("function")
            .map(|callee| callee.end_position().row + 1)
            .unwrap_or_else(|| node.start_position().row + 1)
    }

    fn extract_callee(&self, node: Node, source: &str) -> Option<String> {
        // Extract the function being called.
        // Receiver-qualified calls (`store.insert_symbol(..)`, `lines.push(..)`)
        // are normalized to the bare method name (`insert_symbol`, `push`):
        // the receiver is a local variable name that carries no lookup value,
        // and callers-of queries search by bare method name. Module paths
        // (`Arc::new`, `Vec::new`) keep their full path — no `.` receiver.
        let raw = node
            .child_by_field_name("function")
            .map(|n| n.utf8_text(source.as_bytes()).unwrap_or("").to_string())?;
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
}

/// Extract symbols, call edges and imports from one Rust source file and
/// store them in the symbol graph.
///
/// Called from the indexing chokepoint (`index::index_file_sync_keyed`) so
/// every path that indexes content — cold external walk, periodic sweep,
/// lazy refresh — populates the graph from one place. Creating a fresh
/// `SymbolExtractor` per file is deliberate: the chokepoint is a free
/// function with no state to carry a parser across calls, and parser
/// construction is a cheap once-per-file allocation.
#[cfg(feature = "code-graph")]
pub(crate) fn extract_and_store(store: &super::db::Store, key: &str, body: &str) {
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
