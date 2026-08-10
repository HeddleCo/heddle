// SPDX-License-Identifier: Apache-2.0
//! Content-addressed merkle semantic index (heddle#1067).
//!
//! A parallel merkle DAG over the source tree that stores *semantic* facts —
//! definitions, scopes, imports, and symbol occurrences —
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
//! The digest byte layouts (`hd-sem-sym-v1`, `hd-sem-file-v3`, `hd-sem-dir-v2`)
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

    /// Span-free sort key: `(container_path, name, kind, semantic_hash)`.
    fn sort_key(&self) -> (&[String], &str, u8, ContentHash) {
        (
            &self.container_path,
            self.name.as_str(),
            self.kind.tag_byte(),
            self.semantic_hash,
        )
    }
}

/// Half-open byte range in the source blob. Spans are provenance metadata:
/// they are encoded for navigation, but excluded from semantic digests.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ByteSpan {
    pub start: u32,
    pub end: u32,
}

impl ByteSpan {
    pub fn new(start: u32, end: u32) -> Self {
        Self { start, end }
    }
}

/// Source-local lexical scope classification.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScopeKind {
    Module,
    Type,
    Function,
    Block,
}

impl ScopeKind {
    fn tag_byte(self) -> u8 {
        match self {
            Self::Module => 1,
            Self::Type => 2,
            Self::Function => 3,
            Self::Block => 4,
        }
    }
}

/// A deterministic source-local scope. `local_id` is assigned in preorder.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScopeEntry {
    pub local_id: u32,
    pub parent: Option<u32>,
    pub kind: ScopeKind,
    pub span: ByteSpan,
}

/// Source-level import form. Resolution to repository paths happens later.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImportKindTag {
    Use,
    Import,
    Reexport,
    Dynamic,
}

impl ImportKindTag {
    fn tag_byte(self) -> u8 {
        match self {
            Self::Use => 1,
            Self::Import => 2,
            Self::Reexport => 3,
            Self::Dynamic => 4,
        }
    }
}

/// Namespace in which a binding or occurrence participates.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SymbolNamespace {
    Value,
    Type,
    Both,
}

impl SymbolNamespace {
    fn tag_byte(self) -> u8 {
        match self {
            Self::Value => 1,
            Self::Type => 2,
            Self::Both => 3,
        }
    }
}

/// One name introduced by an import, in canonical source order.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ImportBinding {
    pub imported: String,
    pub local: String,
    pub namespace: SymbolNamespace,
}

/// One unresolved, source-local import record.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportEntry {
    pub kind: ImportKindTag,
    pub module_specifier: String,
    pub bindings: Vec<ImportBinding>,
    pub scope: u32,
    pub span: ByteSpan,
}

/// Role played by a source-level symbol occurrence.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OccurrenceRole {
    Definition,
    Reference,
    Call,
    TypeReference,
}

impl OccurrenceRole {
    fn tag_byte(self) -> u8 {
        match self {
            Self::Definition => 1,
            Self::Reference => 2,
            Self::Call => 3,
            Self::TypeReference => 4,
        }
    }
}

/// One unresolved symbol occurrence, numbered in source order.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OccurrenceEntry {
    pub local_id: u32,
    pub role: OccurrenceRole,
    pub name: String,
    pub qualifier: Vec<String>,
    pub namespace: SymbolNamespace,
    pub scope: u32,
    pub span: ByteSpan,
}

/// Canonical source-local facts assembled into a [`SemanticFileNode`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SemanticFileFacts {
    pub symbols: Vec<SymbolEntry>,
    pub scopes: Vec<ScopeEntry>,
    pub imports: Vec<ImportEntry>,
    pub occurrences: Vec<OccurrenceEntry>,
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

/// Hash the file's *scaffold*: the residual non-definition top-level token
/// stream (every leaf under the file root not covered by an extracted symbol's
/// span). This is what binds `use`-decl swaps, `impl Trait` headers, attribute
/// edits, `macro_rules!` bodies and definition-free files (re-export-only libs,
/// top-level statements) into the file digest — semantic content that lives
/// *outside* any extracted symbol.
///
/// Canonical layout `hd-sem-scaffold-v1`: the length-prefixed leaf token stream
/// (same framing as a symbol hash's token stream), comments excluded.
pub fn compute_file_scaffold_hash(token_stream: &[u8]) -> ContentHash {
    ContentHash::compute_typed("hd-sem-scaffold-v1", token_stream)
}

/// Compute a file node's `semantic_digest` over its scaffold and canonical
/// source-local facts. Spans are deliberately excluded from this identity.
///
/// Canonical layout `hd-sem-file-v3`: `scaffold_hash`, then per symbol
/// `u32-LE(container element count) ‖ (u32-LE-len ‖ bytes)* ‖ u32-LE(name len) ‖
/// name ‖ kind_tag ‖ semantic_hash`. Every variable-length field is
/// length-framed (no record-boundary ambiguity; `["a::b"]` no longer aliases
/// `["a","b"]`). Scope, import, and occurrence records use count-framed
/// fields and stable one-byte enum tags. All spans are EXCLUDED.
pub fn compute_file_semantic_digest(
    scaffold_hash: ContentHash,
    symbols: &[SymbolEntry],
    scopes: &[ScopeEntry],
    imports: &[ImportEntry],
    occurrences: &[OccurrenceEntry],
) -> ContentHash {
    let mut buf = Vec::new();
    buf.extend_from_slice(scaffold_hash.as_bytes());
    buf.extend_from_slice(&(symbols.len() as u32).to_le_bytes());
    for symbol in symbols {
        buf.extend_from_slice(&(symbol.container_path.len() as u32).to_le_bytes());
        for segment in &symbol.container_path {
            buf.extend_from_slice(&(segment.len() as u32).to_le_bytes());
            buf.extend_from_slice(segment.as_bytes());
        }
        buf.extend_from_slice(&(symbol.name.len() as u32).to_le_bytes());
        buf.extend_from_slice(symbol.name.as_bytes());
        buf.push(symbol.kind.tag_byte());
        buf.extend_from_slice(symbol.semantic_hash.as_bytes());
    }
    buf.extend_from_slice(&(scopes.len() as u32).to_le_bytes());
    for scope in scopes {
        buf.extend_from_slice(&scope.local_id.to_le_bytes());
        match scope.parent {
            Some(parent) => {
                buf.push(1);
                buf.extend_from_slice(&parent.to_le_bytes());
            }
            None => buf.push(0),
        }
        buf.push(scope.kind.tag_byte());
    }
    buf.extend_from_slice(&(imports.len() as u32).to_le_bytes());
    for import in imports {
        buf.push(import.kind.tag_byte());
        push_str(&mut buf, &import.module_specifier);
        buf.extend_from_slice(&(import.bindings.len() as u32).to_le_bytes());
        for binding in &import.bindings {
            push_str(&mut buf, &binding.imported);
            push_str(&mut buf, &binding.local);
            buf.push(binding.namespace.tag_byte());
        }
        buf.extend_from_slice(&import.scope.to_le_bytes());
    }
    buf.extend_from_slice(&(occurrences.len() as u32).to_le_bytes());
    for occurrence in occurrences {
        buf.extend_from_slice(&occurrence.local_id.to_le_bytes());
        buf.push(occurrence.role.tag_byte());
        push_str(&mut buf, &occurrence.name);
        buf.extend_from_slice(&(occurrence.qualifier.len() as u32).to_le_bytes());
        for segment in &occurrence.qualifier {
            push_str(&mut buf, segment);
        }
        buf.push(occurrence.namespace.tag_byte());
        buf.extend_from_slice(&occurrence.scope.to_le_bytes());
    }
    ContentHash::compute_typed("hd-sem-file-v3", &buf)
}

fn push_str(buf: &mut Vec<u8>, value: &str) {
    buf.extend_from_slice(&(value.len() as u32).to_le_bytes());
    buf.extend_from_slice(value.as_bytes());
}

/// Compute a directory node's `semantic_digest` over its entries.
///
/// Canonical layout `hd-sem-dir-v2`, per entry:
/// `u32-LE(name len) ‖ name ‖ kind_tag ‖ child semantic_digest`. The name is
/// length-framed so entry boundaries are unambiguous.
pub fn compute_dir_semantic_digest(entries: &[SemanticTreeEntry]) -> ContentHash {
    let mut buf = Vec::new();
    for entry in entries {
        buf.extend_from_slice(&(entry.name.len() as u32).to_le_bytes());
        buf.extend_from_slice(entry.name.as_bytes());
        buf.push(entry.kind.tag_byte());
        buf.extend_from_slice(entry.semantic_digest.as_bytes());
    }
    ContentHash::compute_typed("hd-sem-dir-v2", &buf)
}

/// The per-file semantic node: deterministic source-local facts extracted from
/// one source blob plus their reformat-stable digest.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SemanticFileNode {
    pub format_version: u8,
    pub language: String,
    pub grammar_version: String,
    pub extractor_version: u32,
    /// Content hash of the raw source blob this node was extracted from.
    pub source_blob: ContentHash,
    /// Hash of the file's residual non-definition token stream — see
    /// [`compute_file_scaffold_hash`]. Binds semantic content that lives outside
    /// any extracted symbol into the file digest.
    pub scaffold_hash: ContentHash,
    /// Symbols sorted by the span-free semantic key.
    pub symbols: Vec<SymbolEntry>,
    /// Scopes sorted by deterministic preorder `local_id`.
    pub scopes: Vec<ScopeEntry>,
    /// Imports sorted by their span-free semantic key; bindings retain source order.
    pub imports: Vec<ImportEntry>,
    /// Occurrences sorted by deterministic source-order `local_id`.
    pub occurrences: Vec<OccurrenceEntry>,
    /// Reformat-stable digest — see [`compute_file_semantic_digest`].
    pub semantic_digest: ContentHash,
}

impl SemanticFileNode {
    pub const FORMAT_VERSION: u8 = 2;

    /// Build a node, sorting the symbols canonically and computing the digest
    /// over the scaffold plus the symbols.
    pub fn new(
        language: impl Into<String>,
        grammar_version: impl Into<String>,
        extractor_version: u32,
        source_blob: ContentHash,
        scaffold_hash: ContentHash,
        facts: SemanticFileFacts,
    ) -> Self {
        let SemanticFileFacts {
            mut symbols,
            mut scopes,
            mut imports,
            mut occurrences,
        } = facts;
        symbols.sort_by(|a, b| a.sort_key().cmp(&b.sort_key()));
        scopes.sort_by_key(|scope| scope.local_id);
        imports.sort_by(|a, b| {
            (a.kind, &a.module_specifier, a.scope, &a.bindings).cmp(&(
                b.kind,
                &b.module_specifier,
                b.scope,
                &b.bindings,
            ))
        });
        occurrences.sort_by_key(|occurrence| occurrence.local_id);
        let semantic_digest =
            compute_file_semantic_digest(scaffold_hash, &symbols, &scopes, &imports, &occurrences);
        Self {
            format_version: Self::FORMAT_VERSION,
            language: language.into(),
            grammar_version: grammar_version.into(),
            extractor_version,
            source_blob,
            scaffold_hash,
            symbols,
            scopes,
            imports,
            occurrences,
            semantic_digest,
        }
    }

    pub fn encode(&self) -> Result<Vec<u8>, SemanticIndexError> {
        rmp_serde::to_vec_named(self).map_err(|err| SemanticIndexError::Encoding(err.to_string()))
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, SemanticIndexError> {
        let node: Self = rmp_serde::from_slice(bytes)
            .map_err(|err| SemanticIndexError::Encoding(err.to_string()))?;
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
        let node: Self = rmp_serde::from_slice(bytes)
            .map_err(|err| SemanticIndexError::Encoding(err.to_string()))?;
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
        let root: Self = rmp_serde::from_slice(bytes)
            .map_err(|err| SemanticIndexError::Encoding(err.to_string()))?;
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
            h(0),
            SemanticFileFacts {
                symbols: vec![sym("foo", &[], SymbolKindTag::Function, (10, 20))],
                ..SemanticFileFacts::default()
            },
        );
        // Same symbol, moved by a reformat (span shifted).
        let b = SemanticFileNode::new(
            "rust",
            "0.24",
            1,
            h(1),
            h(0),
            SemanticFileFacts {
                symbols: vec![sym("foo", &[], SymbolKindTag::Function, (99, 120))],
                ..SemanticFileFacts::default()
            },
        );
        assert_eq!(
            a.semantic_digest, b.semantic_digest,
            "span must not affect the file semantic_digest"
        );
    }

    #[test]
    fn semantic_content_hash_excludes_all_provenance_spans() {
        let source_blob = ContentHash::compute(b"use crate::api::greet; greet();");
        let scope = |span| ScopeEntry {
            local_id: 0,
            parent: None,
            kind: ScopeKind::Module,
            span,
        };
        let import = |module_specifier: &str, span| ImportEntry {
            kind: ImportKindTag::Use,
            module_specifier: module_specifier.to_string(),
            bindings: vec![ImportBinding {
                imported: "greet".to_string(),
                local: "greet".to_string(),
                namespace: SymbolNamespace::Both,
            }],
            scope: 0,
            span,
        };
        let occurrence = |span| OccurrenceEntry {
            local_id: 0,
            role: OccurrenceRole::Call,
            name: "greet".to_string(),
            qualifier: Vec::new(),
            namespace: SymbolNamespace::Value,
            scope: 0,
            span,
        };
        let node = |scope_span, import_spans: [ByteSpan; 2], occurrence_span| {
            SemanticFileNode::new(
                "rust",
                "0.24",
                4,
                source_blob,
                h(0),
                SemanticFileFacts {
                    symbols: vec![],
                    scopes: vec![scope(scope_span)],
                    imports: vec![
                        import("crate::api", import_spans[0]),
                        import("crate::util", import_spans[1]),
                    ],
                    occurrences: vec![occurrence(occurrence_span)],
                },
            )
        };
        let a = node(
            ByteSpan::new(0, 38),
            [ByteSpan::new(0, 22), ByteSpan::new(23, 32)],
            ByteSpan::new(23, 30),
        );
        let b = node(
            ByteSpan::new(10, 48),
            [ByteSpan::new(33, 42), ByteSpan::new(10, 32)],
            ByteSpan::new(33, 40),
        );

        assert_eq!(
            a.semantic_digest, b.semantic_digest,
            "span-only differences must not affect semantic content identity"
        );
        assert_ne!(
            a.encode().unwrap(),
            b.encode().unwrap(),
            "encoded provenance still records the distinct spans"
        );
    }

    #[test]
    fn file_node_roundtrip_preserves_source_local_facts() {
        let node = SemanticFileNode::new(
            "typescript",
            "0.23",
            4,
            h(1),
            h(0),
            SemanticFileFacts {
                symbols: vec![sym("run", &[], SymbolKindTag::Function, (2, 4))],
                scopes: vec![ScopeEntry {
                    local_id: 0,
                    parent: None,
                    kind: ScopeKind::Module,
                    span: ByteSpan::new(0, 64),
                }],
                imports: vec![ImportEntry {
                    kind: ImportKindTag::Import,
                    module_specifier: "./api".to_string(),
                    bindings: vec![ImportBinding {
                        imported: "greet".to_string(),
                        local: "hello".to_string(),
                        namespace: SymbolNamespace::Value,
                    }],
                    scope: 0,
                    span: ByteSpan::new(0, 39),
                }],
                occurrences: vec![OccurrenceEntry {
                    local_id: 0,
                    role: OccurrenceRole::Call,
                    name: "hello".to_string(),
                    qualifier: Vec::new(),
                    namespace: SymbolNamespace::Value,
                    scope: 0,
                    span: ByteSpan::new(50, 55),
                }],
            },
        );

        assert_eq!(
            SemanticFileNode::decode(&node.encode().unwrap()).unwrap(),
            node
        );
    }

    #[test]
    fn file_digest_changes_on_symbol_hash_change() {
        let mut s = sym("foo", &[], SymbolKindTag::Function, (1, 2));
        let d1 = compute_file_semantic_digest(h(0), std::slice::from_ref(&s), &[], &[], &[]);
        s.semantic_hash = ContentHash::compute(b"different-body");
        let d2 = compute_file_semantic_digest(h(0), std::slice::from_ref(&s), &[], &[], &[]);
        assert_ne!(d1, d2);
    }

    #[test]
    fn file_digest_changes_on_scaffold_change() {
        let syms = [sym("foo", &[], SymbolKindTag::Function, (1, 2))];
        let d1 = compute_file_semantic_digest(
            compute_file_scaffold_hash(b"use a;"),
            &syms,
            &[],
            &[],
            &[],
        );
        let d2 = compute_file_semantic_digest(
            compute_file_scaffold_hash(b"use b;"),
            &syms,
            &[],
            &[],
            &[],
        );
        assert_ne!(
            d1, d2,
            "scaffold (non-definition top-level tokens) must affect the file digest"
        );
    }

    #[test]
    fn file_digest_framing_is_unambiguous() {
        // `["a::b"]` must NOT collide with `["a","b"]` (per-element framing),
        // and a name boundary shift must not alias across symbols.
        let one = sym("f", &["a::b"], SymbolKindTag::Function, (0, 0));
        let two = sym("f", &["a", "b"], SymbolKindTag::Function, (0, 0));
        assert_ne!(
            compute_file_semantic_digest(h(0), &[one], &[], &[], &[]),
            compute_file_semantic_digest(h(0), &[two], &[], &[], &[]),
        );
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
            h(0),
            SemanticFileFacts {
                symbols: vec![
                    sym("zed", &[], SymbolKindTag::Function, (1, 1)),
                    sym("abe", &["Impl"], SymbolKindTag::Function, (2, 2)),
                    sym("abe", &[], SymbolKindTag::Function, (3, 3)),
                ],
                ..SemanticFileFacts::default()
            },
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
        assert_eq!(
            sym("foo", &[], SymbolKindTag::Function, (0, 0)).address(),
            "foo"
        );
        assert_eq!(
            sym("open", &["Repository"], SymbolKindTag::Function, (0, 0)).address(),
            "Repository::open"
        );
    }
}
