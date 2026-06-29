// SPDX-License-Identifier: Apache-2.0
//! Core parsing implementation.

use std::sync::{Arc, OnceLock};

use objects::object::ContentHash;
use tree_sitter::{Node, Tree as TSTree};

use super::{
    parser_language::Language,
    parser_pool::parse_fresh,
    parser_types::{FunctionDef, Import},
    syntax_index::{FunctionRef, ImportRef, SyntaxIndex},
};

/// A parsed file with its tree-sitter AST and Heddle-owned syntax index.
#[derive(Debug)]
pub struct ParsedFile {
    pub language: Language,
    pub source: Arc<str>,
    content_hash: ContentHash,
    tree: TSTree,
    index: OnceLock<SyntaxIndex>,
}

impl ParsedFile {
    /// Parse a file's contents.
    pub fn parse(source: impl AsRef<str>, language: Language) -> Option<Self> {
        let source = Arc::<str>::from(source.as_ref());
        let content_hash = ContentHash::compute(source.as_bytes());
        Self::parse_with_hash(source, language, content_hash)
    }

    /// Parse already-owned contents without copying the source string.
    pub fn parse_owned(source: String, language: Language) -> Option<Self> {
        let content_hash = ContentHash::compute(source.as_bytes());
        Self::parse_with_hash(Arc::<str>::from(source), language, content_hash)
    }

    pub(crate) fn parse_with_hash(
        source: Arc<str>,
        language: Language,
        content_hash: ContentHash,
    ) -> Option<Self> {
        let tree = parse_fresh(source.as_bytes(), language)?;
        if tree.root_node().has_error() {
            return None;
        }

        Some(Self {
            language,
            source,
            content_hash,
            tree,
            index: OnceLock::new(),
        })
    }

    /// Stable content identity for caches and sidecars.
    pub fn content_hash(&self) -> ContentHash {
        self.content_hash
    }

    /// Borrow the source text without cloning.
    pub fn source(&self) -> &str {
        &self.source
    }

    /// Get the root node of the AST.
    pub fn root_node(&self) -> Node<'_> {
        self.tree.root_node()
    }

    /// Compact Heddle-owned syntax data derived from the AST.
    pub fn syntax_index(&self) -> &SyntaxIndex {
        self.index.get_or_init(|| {
            SyntaxIndex::build(self.language, self.source.as_ref(), self.root_node())
        })
    }

    /// Borrow indexed function definitions.
    pub fn functions(&self) -> impl Iterator<Item = FunctionRef<'_>> + '_ {
        self.syntax_index().functions(self.source.as_ref())
    }

    /// Borrow indexed imports.
    pub fn imports(&self) -> impl Iterator<Item = ImportRef<'_>> + '_ {
        self.syntax_index().imports(self.source.as_ref())
    }

    /// Extract function definitions from the file.
    pub fn extract_functions(&self) -> Vec<FunctionDef> {
        self.functions().map(FunctionRef::to_owned).collect()
    }

    /// Extract imports from the file.
    pub fn extract_imports(&self) -> Vec<Import> {
        self.imports().map(ImportRef::to_owned).collect()
    }

    /// Check if a node kind string represents a function definition in the given language.
    pub fn is_function_kind(kind: &str, language: Language) -> bool {
        super::syntax_index::is_function_kind(kind, language)
    }
}
