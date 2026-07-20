// SPDX-License-Identifier: Apache-2.0
//! Content-addressed merkle semantic index (heddle#1067).
//!
//! A parallel merkle DAG over the source tree that stores *semantic* facts —
//! the symbols a file defines and a normalization-stable fingerprint of each —
//! rather than raw bytes. It mirrors the blob/tree/state DAG so semantic data
//! over all of history costs about as much to maintain as the source history
//! itself, and queries short-circuit on hash equality without re-parsing.
//!
//! ## The two-hash crux
//!
//! Every node carries two identities:
//!
//! - Its **storage hash** — the content-address of the encoded node blob. This
//!   changes whenever the node bytes change, including when a symbol's span
//!   moves under a reformat. It is the object-store key.
//! - Its **`semantic_digest`** — a fingerprint computed over the *meaning* of
//!   the node with spans deliberately excluded. Reformatting a file (which
//!   moves every span) leaves the `semantic_digest` untouched, so a top-down
//!   digest compare prunes reformatted-but-semantically-identical subtrees
//!   with zero re-parse.
//!
//! The digest byte layouts (`hd-sem-sym-v1`, `hd-sem-file-v1`, `hd-sem-dir-v1`)
//! are the canonical, cross-language-reproducible definitions. A verifier in
//! any language that reproduces these byte streams computes byte-identical
//! digests.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::ContentHash;

/// Coarse symbol classification carried by the index. Mirrors the
/// `semantic::symbol_resolver::DefinitionKind` taxonomy so types, traits,
/// enums, modules and the rest are first-class in the index — not just
/// functions.
///
/// The `snake_case` serde spelling is the durable wire form; the [`tag_byte`]
/// value is the durable *hashing* form and must never be renumbered (doing so
/// would silently change every `semantic_hash`/`semantic_digest`).
///
/// [`tag_byte`]: SymbolKindTag::tag_byte
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SymbolKindTag {
    /// Function / method / free function body.
    Function,
    /// Struct or record type definition.
    Type,
    /// Enum definition.
    Enum,
    /// Trait declaration (Rust).
    Trait,
    /// Class declaration (Python / JS / TS / Java / C++).
    Class,
    /// Interface declaration (TS / Java / Go).
    Interface,
    /// Type alias (`type Foo = ...`).
    TypeAlias,
    /// Constant or static at module scope.
    Const,
    /// Module / namespace.
    Module,
    /// Parseable but unclassified definition.
    Other,
}

impl SymbolKindTag {
    /// Stable single-byte tag used in the canonical digest byte streams.
    /// NEVER renumber — the values are baked into every stored digest.
    pub fn tag_byte(self) -> u8 {
        match self {
            SymbolKindTag::Function => 1,
            SymbolKindTag::Type => 2,
            SymbolKindTag::Enum => 3,
            SymbolKindTag::Trait => 4,
            SymbolKindTag::Class => 5,
            SymbolKindTag::Interface => 6,
            SymbolKindTag::TypeAlias => 7,
            SymbolKindTag::Const => 8,
            SymbolKindTag::Module => 9,
            SymbolKindTag::Other => 10,
        }
    }
}

/// The kind of a [`SemanticTreeEntry`]'s target.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SemanticEntryKind {
    /// A subdirectory — `node` is a [`SemanticTreeNode`].
    Dir,
    /// A parsed source file — `node` is a [`SemanticFileNode`].
    File,
    /// Unsupported language, parse failure, or over-budget file. Carries no
    /// semantic node: `node` and `semantic_digest` both equal the raw source
    /// blob hash, so a content change to an opaque file still perturbs the
    /// digest chain.
    Opaque,
}

impl SemanticEntryKind {
    /// Stable single-byte tag used in the canonical dir-digest byte stream.
    pub fn tag_byte(self) -> u8 {
        match self {
            SemanticEntryKind::Dir => 1,
            SemanticEntryKind::File => 2,
            SemanticEntryKind::Opaque => 3,
        }
    }
}

/// One symbol defined in a source file.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SymbolEntry {
    /// Bare symbol name as it appears in the AST.
    pub name: String,
    /// Coarse classification.
    pub kind: SymbolKindTag,
    /// Enclosing scope path (impl block, class, module, ...), outermost first.
    pub container_path: Vec<String>,
    /// Normalization-stable fingerprint of the symbol's definition — a pure
    /// function of `(bytes, grammar, extractor_version)` that is invariant
    /// under reformatting and comment edits. See [`compute_symbol_semantic_hash`].
    pub semantic_hash: ContentHash,
    /// `(start_line, end_line)`, 1-indexed inclusive. PROVENANCE ONLY — the
    /// span is deliberately excluded from every digest so a reformat that moves
    /// the symbol leaves the fingerprint stable.
    pub span: (u32, u32),
}

impl SymbolEntry {
    /// Canonical address spelling: `container::path::name`, or just `name`
    /// when the symbol is at file scope.
    pub fn address(&self) -> String {
        if self.container_path.is_empty() {
            self.name.clone()
        } else {
            format!("{}::{}", self.container_path.join("::"), self.name)
        }
    }

    /// Sort key: `(container_path, name, kind, span.0)`.
    fn sort_key(&self) -> (&[String], &str, u8, u32) {
        (
            &self.container_path,
            self.name.as_str(),
            self.kind.tag_byte(),
            self.span.0,
        )
    }
}

/// Compute a symbol's normalization-stable `semantic_hash`.
///
/// Canonical layout `hd-sem-sym-v1`:
/// `kind_tag ‖ 0x00 ‖ token_stream`.
///
/// `token_stream` is produced by a DFS in document order over the symbol's
/// definition node, skipping comment-kind subtrees, emitting for each remaining
/// leaf `u32-LE(byte_len) ‖ exact source bytes`. Length-prefixed rather than
/// space-joined so token boundaries are unambiguous. Callers assemble the
/// stream (they hold the tree); this owns the framing.
pub fn compute_symbol_semantic_hash(kind: SymbolKindTag, token_stream: &[u8]) -> ContentHash {
    let mut buf = Vec::with_capacity(2 + token_stream.len());
    buf.push(kind.tag_byte());
    buf.push(0x00);
    buf.extend_from_slice(token_stream);
    ContentHash::compute_typed("hd-sem-sym-v1", &buf)
}

/// Compute a file node's `semantic_digest` over its (already sorted) symbols.
///
/// Canonical layout `hd-sem-file-v1`, per symbol:
/// `u32-LE-len-prefixed(container_path joined by "::") ‖ name ‖ kind_tag ‖ semantic_hash`.
/// Spans are EXCLUDED — that is what makes the digest reformat-stable.
pub fn compute_file_semantic_digest(symbols: &[SymbolEntry]) -> ContentHash {
    let mut buf = Vec::new();
    for symbol in symbols {
        let joined = symbol.container_path.join("::");
        buf.extend_from_slice(&(joined.len() as u32).to_le_bytes());
        buf.extend_from_slice(joined.as_bytes());
        buf.extend_from_slice(symbol.name.as_bytes());
        buf.push(symbol.kind.tag_byte());
        buf.extend_from_slice(symbol.semantic_hash.as_bytes());
    }
    ContentHash::compute_typed("hd-sem-file-v1", &buf)
}

/// Compute a directory node's `semantic_digest` over its entries.
///
/// Canonical layout `hd-sem-dir-v1`, per entry:
/// `name ‖ kind_tag ‖ child semantic_digest`.
pub fn compute_dir_semantic_digest(entries: &[SemanticTreeEntry]) -> ContentHash {
    let mut buf = Vec::new();
    for entry in entries {
        buf.extend_from_slice(entry.name.as_bytes());
        buf.push(entry.kind.tag_byte());
        buf.extend_from_slice(entry.semantic_digest.as_bytes());
    }
    ContentHash::compute_typed("hd-sem-dir-v1", &buf)
}

/// The per-file semantic node: the symbols a source blob defines plus the
/// reformat-stable digest over them.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SemanticFileNode {
    pub format_version: u8,
    pub language: String,
    pub grammar_version: String,
    pub extractor_version: u32,
    /// Content hash of the raw source blob this node was extracted from.
    pub source_blob: ContentHash,
    /// Symbols sorted by `(container_path, name, kind, span.0)`.
    pub symbols: Vec<SymbolEntry>,
    /// Reformat-stable digest — see [`compute_file_semantic_digest`].
    pub semantic_digest: ContentHash,
}

impl SemanticFileNode {
    pub const FORMAT_VERSION: u8 = 1;

    /// Build a node, sorting the symbols canonically and computing the digest.
    pub fn new(
        language: impl Into<String>,
        grammar_version: impl Into<String>,
        extractor_version: u32,
        source_blob: ContentHash,
        mut symbols: Vec<SymbolEntry>,
    ) -> Self {
        symbols.sort_by(|a, b| a.sort_key().cmp(&b.sort_key()));
        let semantic_digest = compute_file_semantic_digest(&symbols);
        Self {
            format_version: Self::FORMAT_VERSION,
            language: language.into(),
            grammar_version: grammar_version.into(),
            extractor_version,
            source_blob,
            symbols,
            semantic_digest,
        }
    }

    pub fn encode(&self) -> Result<Vec<u8>, SemanticIndexError> {
        rmp_serde::to_vec_named(self).map_err(|err| SemanticIndexError::Encoding(err.to_string()))
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, SemanticIndexError> {
        let node: Self =
            rmp_serde::from_slice(bytes).map_err(|err| SemanticIndexError::Encoding(err.to_string()))?;
        if node.format_version != Self::FORMAT_VERSION {
            return Err(SemanticIndexError::UnsupportedVersion(node.format_version));
        }
        Ok(node)
    }

    /// Find a symbol by its canonical address (`container::name`).
    pub fn symbol_by_address(&self, address: &str) -> Option<&SymbolEntry> {
        self.symbols.iter().find(|s| s.address() == address)
    }
}

/// One child edge of a [`SemanticTreeNode`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SemanticTreeEntry {
    pub name: String,
    pub kind: SemanticEntryKind,
    /// Storage hash of the child node (a [`SemanticFileNode`] or
    /// [`SemanticTreeNode`] blob), or — for [`SemanticEntryKind::Opaque`] — the
    /// raw source blob hash.
    pub node: ContentHash,
    /// The child's `semantic_digest` (its reformat-stable identity).
    pub semantic_digest: ContentHash,
}

/// A semantic directory node mirroring a source [`Tree`](super::Tree).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SemanticTreeNode {
    pub format_version: u8,
    /// Entries sorted by `name` (mirrors the source tree's ordering).
    pub entries: Vec<SemanticTreeEntry>,
}

impl SemanticTreeNode {
    pub const FORMAT_VERSION: u8 = 1;

    /// Build a node, sorting entries by name and computing the dir digest,
    /// which is returned alongside the node.
    pub fn new(mut entries: Vec<SemanticTreeEntry>) -> (Self, ContentHash) {
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        let digest = compute_dir_semantic_digest(&entries);
        (
            Self {
                format_version: Self::FORMAT_VERSION,
                entries,
            },
            digest,
        )
    }

    /// The node's reformat-stable digest.
    pub fn semantic_digest(&self) -> ContentHash {
        compute_dir_semantic_digest(&self.entries)
    }

    pub fn encode(&self) -> Result<Vec<u8>, SemanticIndexError> {
        rmp_serde::to_vec_named(self).map_err(|err| SemanticIndexError::Encoding(err.to_string()))
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, SemanticIndexError> {
        let node: Self =
            rmp_serde::from_slice(bytes).map_err(|err| SemanticIndexError::Encoding(err.to_string()))?;
        if node.format_version != Self::FORMAT_VERSION {
            return Err(SemanticIndexError::UnsupportedVersion(node.format_version));
        }
        Ok(node)
    }

    pub fn get(&self, name: &str) -> Option<&SemanticTreeEntry> {
        self.entries
            .binary_search_by(|e| e.name.as_str().cmp(name))
            .ok()
            .map(|i| &self.entries[i])
    }
}

/// Root of a state's semantic index. Attached to a state via
/// `StateAttachmentBody::SemanticIndex`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SemanticIndexRoot {
    pub format_version: u8,
    pub extractor_version: u32,
    /// Language → grammar version, for every language present in the tree.
    pub grammars: BTreeMap<String, String>,
    /// Storage hash of the top [`SemanticTreeNode`].
    pub tree: ContentHash,
    /// The top tree node's `semantic_digest` — the whole-tree fingerprint.
    pub semantic_digest: ContentHash,
}

impl SemanticIndexRoot {
    pub const FORMAT_VERSION: u8 = 1;

    pub fn new(
        extractor_version: u32,
        grammars: BTreeMap<String, String>,
        tree: ContentHash,
        semantic_digest: ContentHash,
    ) -> Self {
        Self {
            format_version: Self::FORMAT_VERSION,
            extractor_version,
            grammars,
            tree,
            semantic_digest,
        }
    }

    pub fn encode(&self) -> Result<Vec<u8>, SemanticIndexError> {
        rmp_serde::to_vec_named(self).map_err(|err| SemanticIndexError::Encoding(err.to_string()))
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, SemanticIndexError> {
        let root: Self =
            rmp_serde::from_slice(bytes).map_err(|err| SemanticIndexError::Encoding(err.to_string()))?;
        if root.format_version != Self::FORMAT_VERSION {
            return Err(SemanticIndexError::UnsupportedVersion(root.format_version));
        }
        Ok(root)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SemanticIndexError {
    #[error("unsupported semantic index node version {0}")]
    UnsupportedVersion(u8),
    #[error("semantic index node encoding error: {0}")]
    Encoding(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(seed: u8) -> ContentHash {
        ContentHash::from_bytes([seed; 32])
    }

    fn sym(name: &str, container: &[&str], kind: SymbolKindTag, span: (u32, u32)) -> SymbolEntry {
        SymbolEntry {
            name: name.to_string(),
            kind,
            container_path: container.iter().map(|s| s.to_string()).collect(),
            semantic_hash: ContentHash::compute(name.as_bytes()),
            span,
        }
    }

    #[test]
    fn file_digest_excludes_span() {
        let a = SemanticFileNode::new(
            "rust",
            "0.24",
            1,
            h(1),
            vec![sym("foo", &[], SymbolKindTag::Function, (10, 20))],
        );
        // Same symbol, moved by a reformat (span shifted).
        let b = SemanticFileNode::new(
            "rust",
            "0.24",
            1,
            h(1),
            vec![sym("foo", &[], SymbolKindTag::Function, (99, 120))],
        );
        assert_eq!(
            a.semantic_digest, b.semantic_digest,
            "span must not affect the file semantic_digest"
        );
    }

    #[test]
    fn file_digest_changes_on_symbol_hash_change() {
        let mut s = sym("foo", &[], SymbolKindTag::Function, (1, 2));
        let d1 = compute_file_semantic_digest(std::slice::from_ref(&s));
        s.semantic_hash = ContentHash::compute(b"different-body");
        let d2 = compute_file_semantic_digest(std::slice::from_ref(&s));
        assert_ne!(d1, d2);
    }

    #[test]
    fn symbol_hash_stable_and_kind_sensitive() {
        let ts = b"some token stream";
        let a = compute_symbol_semantic_hash(SymbolKindTag::Function, ts);
        let b = compute_symbol_semantic_hash(SymbolKindTag::Function, ts);
        assert_eq!(a, b);
        let c = compute_symbol_semantic_hash(SymbolKindTag::Type, ts);
        assert_ne!(a, c, "kind participates in the symbol hash");
    }

    #[test]
    fn symbols_sorted_canonically() {
        let node = SemanticFileNode::new(
            "rust",
            "0.24",
            1,
            h(1),
            vec![
                sym("zed", &[], SymbolKindTag::Function, (1, 1)),
                sym("abe", &["Impl"], SymbolKindTag::Function, (2, 2)),
                sym("abe", &[], SymbolKindTag::Function, (3, 3)),
            ],
        );
        let names: Vec<_> = node.symbols.iter().map(|s| s.address()).collect();
        assert_eq!(names, vec!["abe", "zed", "Impl::abe"]);
    }

    #[test]
    fn dir_digest_stable_and_roundtrip() {
        let e = SemanticTreeEntry {
            name: "a.rs".to_string(),
            kind: SemanticEntryKind::File,
            node: h(5),
            semantic_digest: h(6),
        };
        let (node, digest) = SemanticTreeNode::new(vec![e.clone()]);
        assert_eq!(node.semantic_digest(), digest);
        let bytes = node.encode().unwrap();
        assert_eq!(SemanticTreeNode::decode(&bytes).unwrap(), node);
    }

    #[test]
    fn address_spelling() {
        assert_eq!(sym("foo", &[], SymbolKindTag::Function, (0, 0)).address(), "foo");
        assert_eq!(
            sym("open", &["Repository"], SymbolKindTag::Function, (0, 0)).address(),
            "Repository::open"
        );
    }
}
