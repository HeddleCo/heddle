// SPDX-License-Identifier: Apache-2.0
//! Semantic-index *extraction*: turning a source blob into the per-file symbol
//! list the merkle semantic index (heddle#1067) stores.
//!
//! The index node types and the canonical digest/hash byte layouts live in the
//! `objects` crate ([`objects::object::semantic_index`]); this module owns the
//! grammar-facing half — walking the AST to produce [`SymbolEntry`] values with
//! their normalization-stable `semantic_hash`. Assembly of the tree and the
//! store wiring live in the `repo` crate.
//!
//! A symbol's `semantic_hash` is a pure function of
//! `(source bytes, grammar, extractor_version)`: a DFS in document order over
//! the definition's node, comment subtrees skipped, each remaining leaf emitted
//! as `u32-LE(byte_len) ‖ exact source bytes`. Whitespace and comments are not
//! leaves, so reformatting and comment edits leave the hash untouched — while a
//! one-token change perturbs exactly the symbols that contain it.

use objects::object::{ContentHash, SymbolEntry, SymbolKindTag, compute_symbol_semantic_hash};

use crate::{
    parser::{Language, ParsedFile, walk_non_comment_leaves},
    symbol_resolver::{DefinitionKind, visit_definitions},
};

/// Version of the extraction logic itself. Bump when the taxonomy, container
/// resolution, or token-stream framing changes in a way that would alter a
/// `semantic_hash` for unchanged source — it participates in the file node's
/// identity so a bump forces a clean recompute via the supersedes chain.
pub const EXTRACTOR_VERSION: u32 = 1;

/// Stable lowercase language name recorded in file nodes and the root's
/// grammar map.
pub fn language_name(language: Language) -> &'static str {
    match language {
        Language::Rust => "rust",
        Language::Python => "python",
        Language::JavaScript => "javascript",
        Language::TypeScript => "typescript",
        Language::Go => "go",
        Language::C => "c",
        Language::Cpp => "cpp",
        Language::Java => "java",
        Language::Unknown => "unknown",
    }
}

/// Grammar version string for a language — the tree-sitter grammar crate
/// version. Participates in node identity so a grammar bump recomputes cleanly.
pub fn grammar_version(language: Language) -> &'static str {
    match language {
        Language::Rust => "tree-sitter-rust@0.24",
        Language::Python => "tree-sitter-python@0.25",
        Language::JavaScript => "tree-sitter-javascript@0.25",
        Language::TypeScript => "tree-sitter-typescript@0.23",
        Language::Go => "tree-sitter-go@0.25",
        Language::C => "tree-sitter-c@0.24",
        Language::Cpp => "tree-sitter-cpp@0.23",
        Language::Java => "tree-sitter-java@0.23",
        Language::Unknown => "none",
    }
}

fn map_kind(kind: DefinitionKind) -> SymbolKindTag {
    match kind {
        DefinitionKind::Function => SymbolKindTag::Function,
        DefinitionKind::Type => SymbolKindTag::Type,
        DefinitionKind::Trait => SymbolKindTag::Trait,
        DefinitionKind::Class => SymbolKindTag::Class,
        DefinitionKind::Interface => SymbolKindTag::Interface,
        DefinitionKind::TypeAlias => SymbolKindTag::TypeAlias,
        DefinitionKind::EnumDef => SymbolKindTag::Enum,
        DefinitionKind::ConstDecl => SymbolKindTag::Const,
        DefinitionKind::Module => SymbolKindTag::Module,
        DefinitionKind::Other => SymbolKindTag::Other,
    }
}

/// The symbols extracted from one source file, ready to be assembled into a
/// [`SemanticFileNode`](objects::object::SemanticFileNode).
pub struct ExtractedFile {
    pub language: Language,
    pub symbols: Vec<SymbolEntry>,
}

/// Parse `source` (as `language`) and extract its symbols with per-symbol
/// normalization-stable hashes.
///
/// Returns `None` when the language is unsupported or the file fails to parse —
/// the caller records those as `Opaque` in the index (fingerprint = raw source
/// blob hash). `extractor_version` is threaded through for callers that want to
/// pin a specific version (capture uses [`EXTRACTOR_VERSION`]).
pub fn extract_semantic_file(source: &[u8], language: Language) -> Option<ExtractedFile> {
    // Unsupported language → no grammar → Opaque.
    language.parser_handle()?;
    let source_text = std::str::from_utf8(source).ok()?;
    let parsed = ParsedFile::parse(source_text, language)?;

    let mut symbols = Vec::new();
    visit_definitions(parsed.root_node(), source, &mut |site| {
        let kind = map_kind(site.kind);
        let semantic_hash = symbol_semantic_hash(site.node, source, kind);
        let container_path = site
            .parent_name
            .map(|p| vec![p])
            .unwrap_or_default();
        symbols.push(SymbolEntry {
            name: site.name,
            kind,
            container_path,
            semantic_hash,
            span: (site.start_line, site.end_line),
        });
    });

    Some(ExtractedFile { language, symbols })
}

/// Build the canonical `hd-sem-sym-v1` token stream for a definition node and
/// hash it. Length-prefixed leaves in document order, comment subtrees skipped.
fn symbol_semantic_hash(
    node: tree_sitter::Node<'_>,
    source: &[u8],
    kind: SymbolKindTag,
) -> ContentHash {
    let mut token_stream: Vec<u8> = Vec::new();
    walk_non_comment_leaves(node, |leaf| {
        let bytes = &source[leaf.byte_range()];
        token_stream.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
        token_stream.extend_from_slice(bytes);
    });
    compute_symbol_semantic_hash(kind, &token_stream)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn extract(src: &str) -> Vec<SymbolEntry> {
        extract_semantic_file(src.as_bytes(), Language::Rust)
            .expect("rust parse")
            .symbols
    }

    #[test]
    fn reformat_leaves_symbol_hash_stable() {
        let a = "fn add(a: i32, b: i32) -> i32 { a + b }\n";
        let b = "fn add(a: i32,   b: i32) -> i32 {\n    a + b\n}\n";
        let sa = extract(a);
        let sb = extract(b);
        assert_eq!(sa.len(), 1);
        assert_eq!(sb.len(), 1);
        assert_eq!(
            sa[0].semantic_hash, sb[0].semantic_hash,
            "reformatting must not change the symbol semantic_hash"
        );
    }

    #[test]
    fn comment_edit_leaves_symbol_hash_stable() {
        let a = "fn f() {\n    // old comment\n    g();\n}\n";
        let b = "fn f() {\n    // a completely different comment\n    g();\n}\n";
        assert_eq!(extract(a)[0].semantic_hash, extract(b)[0].semantic_hash);
    }

    #[test]
    fn one_token_change_perturbs_only_that_symbol() {
        let a = "fn f() -> i32 { 1 }\nfn g() -> i32 { 2 }\n";
        let b = "fn f() -> i32 { 1 }\nfn g() -> i32 { 3 }\n";
        let sa = extract(a);
        let sb = extract(b);
        let f_a = sa.iter().find(|s| s.name == "f").unwrap();
        let f_b = sb.iter().find(|s| s.name == "f").unwrap();
        let g_a = sa.iter().find(|s| s.name == "g").unwrap();
        let g_b = sb.iter().find(|s| s.name == "g").unwrap();
        assert_eq!(f_a.semantic_hash, f_b.semantic_hash, "untouched symbol stable");
        assert_ne!(g_a.semantic_hash, g_b.semantic_hash, "edited symbol changes");
    }

    #[test]
    fn string_literal_contents_included() {
        let a = "fn f() { let s = \"hello\"; }\n";
        let b = "fn f() { let s = \"world\"; }\n";
        assert_ne!(
            extract(a)[0].semantic_hash,
            extract(b)[0].semantic_hash,
            "string literal contents are part of the fingerprint"
        );
    }

    #[test]
    fn types_are_first_class() {
        let src = "struct S { x: u32 }\nenum E { A, B }\ntrait T { fn m(&self); }\n";
        let names: Vec<_> = extract(src).into_iter().map(|s| (s.name, s.kind)).collect();
        assert!(names.contains(&("S".to_string(), SymbolKindTag::Type)));
        assert!(names.contains(&("E".to_string(), SymbolKindTag::Enum)));
        assert!(names.contains(&("T".to_string(), SymbolKindTag::Trait)));
    }

    #[test]
    fn unsupported_language_is_none() {
        assert!(extract_semantic_file(b"whatever", Language::Unknown).is_none());
    }
}
