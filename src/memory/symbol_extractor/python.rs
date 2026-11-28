//! The Python node-kind walk (`.py`, #1073).
//!
//! Grammar names verified against `tree-sitter-python` v0.23.6's
//! `src/node-types.json` and `grammar.js` — not guessed:
//!
//! - `function_definition` (field `name`) → [`SymbolKind::Function`], or
//!   [`SymbolKind::Method`] when it sits inside a `class_definition` body.
//! - `class_definition` (field `name`) → [`SymbolKind::Class`].
//! - `import_statement` / `import_from_statement` / `future_import_statement`
//!   → [`SymbolKind::Import`].
//! - `call` (field `function`) → a caller → callee edge.
//!
//! Line conventions are the Rust arms' (see [`super::rust`]), because the two
//! languages write into one table and a consumer must not have to know which
//! grammar produced a row: **symbols report 0-based tree-sitter rows, call
//! edges report the 1-based line holding the callee expression**. The
//! asymmetry is inherited, not chosen — #1073 requires Rust output to stay
//! byte-identical, so aligning the two is a separate change.

use tree_sitter::Node;

use super::types::node_text;
use super::{CallEdge, Symbol, SymbolKind};

/// Walk one Python syntax tree, appending symbols and call edges.
pub(crate) fn extract(
    root: Node<'_>,
    source: &str,
    file_path: &str,
    symbols: &mut Vec<Symbol>,
    call_edges: &mut Vec<CallEdge>,
) {
    walk(
        root,
        source,
        file_path,
        // Python has no `impl`: a callable's owner is whatever encloses it.
        Scope {
            function: None,
            class: None,
        },
        symbols,
        call_edges,
    );
}

/// What encloses the node being walked.
#[derive(Clone, Copy)]
struct Scope<'a> {
    /// The nearest enclosing `def` — the caller an edge is attributed to.
    /// `None` at module level and in a class body, where a call has no caller
    /// to attribute (the same rule the Rust walk applies outside functions).
    function: Option<&'a str>,
    /// The nearest enclosing `class` — present makes a `def` a method.
    class: Option<&'a str>,
}

fn walk(
    node: Node<'_>,
    source: &str,
    file_path: &str,
    scope: Scope<'_>,
    symbols: &mut Vec<Symbol>,
    call_edges: &mut Vec<CallEdge>,
) {
    match node.kind() {
        "function_definition" => {
            let Some(name) = field_text(node, "name", source) else {
                return;
            };
            let kind = if scope.class.is_some() {
                SymbolKind::Method
            } else {
                SymbolKind::Function
            };
            push_symbol(node, &name, kind, file_path, symbols);

            let inner = Scope {
                function: Some(&name),
                // A `def` nested inside a `def` is a closure, not a method, so
                // the class context stops at the function boundary. (A class
                // defined *inside* a function re-opens it in the arm below.)
                class: None,
            };
            for child in node.children(&mut node.walk()) {
                walk(child, source, file_path, inner, symbols, call_edges);
            }
        }
        "class_definition" => {
            let Some(name) = field_text(node, "name", source) else {
                return;
            };
            push_symbol(node, &name, SymbolKind::Class, file_path, symbols);

            // A class body is not a callable: the enclosing function (if the
            // class is defined inside one) still owns any call written there,
            // which is the same attribution the Rust `impl_item` arm gives.
            let inner = Scope {
                function: scope.function,
                class: Some(&name),
            };
            for child in node.children(&mut node.walk()) {
                walk(child, source, file_path, inner, symbols, call_edges);
            }
        }
        "import_statement" => {
            // `import os`, `import os, sys`, `import numpy as np`.
            for child in named_children_with_field(node, "name") {
                let name = import_target(child, source);
                if !name.is_empty() {
                    push_symbol(node, &name, SymbolKind::Import, file_path, symbols);
                }
            }
        }
        "import_from_statement" => {
            // `from x import y` → `x.y`; `from . import y` → `.y`;
            // `from x import *` → `x`.
            let base = node
                .child_by_field_name("module_name")
                .map(|n| node_text(n, source).to_string())
                .unwrap_or_default();
            let mut emitted = false;
            for child in named_children_with_field(node, "name") {
                let target = import_target(child, source);
                if target.is_empty() {
                    continue;
                }
                let name = join_module_path(&base, &target);
                push_symbol(node, &name, SymbolKind::Import, file_path, symbols);
                emitted = true;
            }
            if !emitted && !base.is_empty() {
                push_symbol(node, &base, SymbolKind::Import, file_path, symbols);
            }
        }
        "future_import_statement" => {
            // `from __future__ import annotations` — a distinct node kind with
            // no `module_name` field, and still an import.
            let mut emitted = false;
            for child in named_children_with_field(node, "name") {
                let target = import_target(child, source);
                if target.is_empty() {
                    continue;
                }
                let name = join_module_path("__future__", &target);
                push_symbol(node, &name, SymbolKind::Import, file_path, symbols);
                emitted = true;
            }
            if !emitted {
                push_symbol(node, "__future__", SymbolKind::Import, file_path, symbols);
            }
        }
        "call" => {
            // Recurse after handling: a call nests inside another call's
            // arguments or receiver, and each nested call is its own edge
            // (the #1325 rule the Rust arm documents).
            if let (Some(caller), Some(callee)) = (scope.function, callee(node, source)) {
                call_edges.push(CallEdge {
                    caller: caller.to_string(),
                    callee,
                    file_path: file_path.to_string(),
                    line: callee_line(node),
                });
            }
            for child in node.children(&mut node.walk()) {
                walk(child, source, file_path, scope, symbols, call_edges);
            }
        }
        _ => {
            for child in node.children(&mut node.walk()) {
                walk(child, source, file_path, scope, symbols, call_edges);
            }
        }
    }
}

fn push_symbol(
    node: Node<'_>,
    name: &str,
    kind: SymbolKind,
    file_path: &str,
    symbols: &mut Vec<Symbol>,
) {
    symbols.push(Symbol {
        name: name.to_string(),
        kind,
        file_path: file_path.to_string(),
        start_line: node.start_position().row,
        end_line: node.end_position().row,
    });
}

fn field_text(node: Node<'_>, field: &str, source: &str) -> Option<String> {
    node.child_by_field_name(field)
        .map(|n| node_text(n, source).to_string())
}

/// Every named child carrying `field` — Python's import lists are repeated
/// fields (`commaSep1(field('name', …))`), which
/// `child_by_field_name` would collapse to the first one.
fn named_children_with_field<'tree>(node: Node<'tree>, field: &str) -> Vec<Node<'tree>> {
    let mut cursor = node.walk();
    node.children_by_field_name(field, &mut cursor).collect()
}

/// The module a `dotted_name` / `aliased_import` child names (`a.b as c` → `a.b`).
fn import_target(node: Node<'_>, source: &str) -> String {
    let target = if node.kind() == "aliased_import" {
        node.child_by_field_name("name").unwrap_or(node)
    } else {
        node
    };
    node_text(target, source).to_string()
}

/// `base` + `.` + `target`, with the separator dropped when `base` already
/// ends in one (`from . import x` → `.x`, `from ..pkg import y` → `..pkg.y`).
fn join_module_path(base: &str, target: &str) -> String {
    if base.is_empty() {
        return target.to_string();
    }
    if base.ends_with('.') {
        format!("{base}{target}")
    } else {
        format!("{base}.{target}")
    }
}

/// The 1-based line a call edge reports: the callee expression's own end.
///
/// Mirrors [`super::rust`]'s `callee_line` so a citation lands on the line that
/// holds the call. In Python the receiver and the callee can also be split
/// across lines by a parenthesised chain, and the callee's end is the line a
/// reader needs there too.
fn callee_line(node: Node<'_>) -> usize {
    node.child_by_field_name("function")
        .map(|callee| callee.end_position().row + 1)
        .unwrap_or_else(|| node.start_position().row + 1)
}

/// The callee name for a Python `call`, by the #1073 rule.
///
/// In Rust a `.` is always a method receiver, so the Rust walk keeps only the
/// last segment. In Python `.` is ALSO the module separator: collapsing
/// `os.path.join` to `join` merges it with `str.join` and `Path.join` into one
/// unanswerable name, and the bare-name index strips `.` too, so nothing
/// downstream can recover the path. So:
///
/// - `foo(...)` → `foo`
/// - `os.path.join(...)` → `os.path.join` (module path kept whole)
/// - `self.helper(...)` → `helper` (the one receiver that is a keyword-ish
///   self reference and carries no lookup value)
/// - `cls.build(...)` → `build`
///
/// There is no Python counterpart to the Rust arm's `Some`/`Ok`/`Err` noise
/// filter: `None` is not callable, so it never appears as a `call`'s function.
fn callee(node: Node<'_>, source: &str) -> Option<String> {
    let function = node.child_by_field_name("function")?;
    match function.kind() {
        "identifier" => Some(node_text(function, source).to_string()),
        "attribute" => {
            let attribute = function.child_by_field_name("attribute")?;
            let attribute_name = node_text(attribute, source).to_string();
            let object = function.child_by_field_name("object")?;
            let receiver = node_text(object, source);
            // `self.helper(..)` / `cls.build(..)`: the only receivers that are
            // self-references carrying no lookup value — strip them, exactly as
            // the Rust walk strips a variable receiver.
            if receiver == "self" || receiver == "cls" {
                return Some(attribute_name);
            }
            // Everything else keeps its qualifier, because in Python a `.` may
            // be the module separator (`os.path.join`) and the bare-name index
            // strips `.` too, so a collapsed name is unrecoverable.
            let raw = node_text(function, source);
            Some(if is_dotted_identifier(raw) {
                raw.to_string()
            } else {
                // `super().__init__(..)`, `factory()[0].build(..)`: the
                // receiver is an expression, not a name — attribute only.
                attribute_name
            })
        }
        // `d["f"]()`, `(lambda: 1)()`, `factory()()` — no name to attribute.
        // Keep the text only when it is still a plain dotted identifier.
        _ => {
            let raw = node_text(function, source);
            is_dotted_identifier(raw).then(|| raw.to_string())
        }
    }
}

/// `[A-Za-z_][A-Za-z0-9_.]*` — a name a caller query could ever match.
fn is_dotted_identifier(raw: &str) -> bool {
    let mut chars = raw.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.')
}
