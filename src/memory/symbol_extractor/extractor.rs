//! The extractor: one tree-sitter parser, dispatched per file extension.
//!
//! Before #1073 this was a single file that hardcoded `tree_sitter_rust::LANGUAGE`
//! and one Rust node-kind match. The walk now lives in [`super::rust`] and the
//! grammar is chosen per file by [`Language::for_path`], so adding a language
//! (Python here, markdown in #1074) never touches an existing language's arms.

use std::path::Path;
use tree_sitter::Parser;

use super::{CallEdge, Language, Symbol, python, rust};

/// Extracts symbols and call graphs from source files using tree-sitter.
pub struct SymbolExtractor {
    parser: Parser,
    /// The grammar currently loaded in `parser`, tracked so a run of files in
    /// one language pays `set_language` once rather than per file.
    language: Language,
}

impl SymbolExtractor {
    /// A fresh extractor, primed with the Rust grammar.
    ///
    /// Priming keeps `new()`'s historical contract — it fails only when a
    /// grammar cannot be loaded — and [`Self::extract`] switches grammars for
    /// any file whose extension asks for a different one.
    pub fn new() -> Result<Self, anyhow::Error> {
        let mut parser = Parser::new();
        parser.set_language(&Language::Rust.parser_language())?;
        Ok(Self {
            parser,
            language: Language::Rust,
        })
    }

    /// Parse a source file and extract all symbols and call edges.
    ///
    /// The grammar comes from the path's extension ([`Language::for_path`]). A
    /// path with no registered grammar is an explicit error, logged here and by
    /// the caller: returning an empty `Ok` would let the chokepoint store an
    /// empty graph for a file it never parsed, which is indistinguishable from
    /// "this file defines nothing".
    pub fn extract(
        &mut self,
        file_path: &Path,
        source: &str,
    ) -> Result<(Vec<Symbol>, Vec<CallEdge>), anyhow::Error> {
        let Some(language) = Language::for_path(file_path) else {
            tracing::debug!(
                "code-graph: no grammar registered for {}",
                file_path.display()
            );
            anyhow::bail!(
                "code-graph: no grammar registered for {}",
                file_path.display()
            );
        };

        if language != self.language {
            self.parser.set_language(&language.parser_language())?;
            self.language = language;
        }

        let tree = self
            .parser
            .parse(source, None)
            .ok_or_else(|| anyhow::anyhow!("Failed to parse {}", file_path.display()))?;

        let file_path_str = file_path.to_string_lossy().to_string();
        let mut symbols = Vec::new();
        let mut call_edges = Vec::new();

        match language {
            Language::Rust => rust::extract(
                tree.root_node(),
                source,
                &file_path_str,
                &mut symbols,
                &mut call_edges,
            ),
            Language::Python => python::extract(
                tree.root_node(),
                source,
                &file_path_str,
                &mut symbols,
                &mut call_edges,
            ),
        }

        Ok((symbols, call_edges))
    }
}
