//! The code graph's shared vocabulary: symbol rows, call edges, the language
//! dispatch table, and the extractor-semantics stamp.
//!
//! These types are per-graph, not per-language — the node-kind walks that fill
//! them live in [`super::rust`] and [`super::python`] (#1073 split the single
//! file once a second language arrived).

use std::path::Path;
use tree_sitter::Node;

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
    Class,
    Method,
    /// A whole-file/module construct. No extractor constructs this yet: the
    /// variant exists so the kind vocabulary is complete for the languages
    /// that will claim it (a Python package's `__init__.py`, a markdown
    /// document's headings in #1074) without a second schema change (#1073).
    #[allow(dead_code)]
    Module,
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
            SymbolKind::Class => write!(f, "class"),
            SymbolKind::Method => write!(f, "method"),
            SymbolKind::Module => write!(f, "module"),
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
/// 3 — #1073: Python files enter the graph (class/method/import kinds, and a
///     Python callee rule that keeps dotted module paths whole). Files that
///     were indexed as "not a source file" before carry no stamp at all, and
///     nothing but this bump revisits an unchanged file.
pub(crate) const GRAPH_SEMANTICS_VERSION: u32 = 3;

/// The grammar a file is parsed with, chosen by extension.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Language {
    Rust,
    Python,
}

impl Language {
    /// The grammar for `path`, or `None` when no grammar is registered.
    ///
    /// This is the single source of truth for "can the code graph parse this
    /// file": the indexing gates ([`super::is_parseable_source`]) and the
    /// extractor's own dispatch both read it, so the two can never disagree
    /// about which suffixes are in scope.
    pub(crate) fn for_path(path: &Path) -> Option<Self> {
        match path.extension().and_then(|ext| ext.to_str()) {
            Some("rs") => Some(Self::Rust),
            Some("py") => Some(Self::Python),
            _ => None,
        }
    }

    /// The tree-sitter language for this grammar.
    pub(crate) fn parser_language(self) -> tree_sitter::Language {
        match self {
            Self::Rust => tree_sitter_rust::LANGUAGE.into(),
            Self::Python => tree_sitter_python::LANGUAGE.into(),
        }
    }
}

/// Is `key` a file the code graph can parse?
///
/// The one predicate the indexing chokepoint gates on, called from both the
/// cold/changed path and the unchanged-content re-extract path in
/// `memory::index` (#1073). A suffix list kept in two places drifts; this one
/// cannot, because it is [`Language::for_path`].
pub(crate) fn is_parseable_source(key: &str) -> bool {
    Language::for_path(Path::new(key)).is_some()
}

/// The UTF-8 text of a node, or `""` for a node spanning invalid UTF-8.
pub(crate) fn node_text<'a>(node: Node<'_>, source: &'a str) -> &'a str {
    node.utf8_text(source.as_bytes()).unwrap_or("")
}
