// SPDX-License-Identifier: Apache-2.0
//! Conservative, source-backed Rust conditional-value comparison.
//!
//! These are structural facts, not a control-flow/effects model. Source-local
//! identities include exact blobs and structural occurrence order; matching
//! across revisions uses symbol/target and normalized tokens, never node IDs.

pub use objects::object::ContentHash;
use objects::object::{ByteSpan, SymbolEntry};

mod compare;
mod extract;

pub const EXTRACTOR_VERSION: u32 = 1;
pub const BINDING_VERSION: u32 = 1;
pub const COMPARISON_VERSION: u32 = 1;
pub const MAX_SOURCE_BYTES: usize = 1024 * 1024;
pub const MAX_ASSIGNMENTS: usize = 512;

pub fn extraction_key(blob: ContentHash) -> ContentHash {
    extract::hash(&[
        b"extraction",
        blob.as_bytes(),
        crate::semantic_index::grammar_version(crate::parser::Language::Rust).as_bytes(),
        &crate::semantic_index::EXTRACTOR_VERSION.to_le_bytes(),
        &EXTRACTOR_VERSION.to_le_bytes(),
    ])
}

pub fn binding_key(extraction: ContentHash) -> ContentHash {
    // Extraction binds the whole source blob, not just a symbol digest.
    extract::hash(&[
        b"bindings",
        extraction.as_bytes(),
        &BINDING_VERSION.to_le_bytes(),
    ])
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    Base,
    Source,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Support {
    Supported,
    Partial,
    Unsupported,
    Unavailable,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Limitation {
    ParseError,
    Language,
    MissingElse,
    Syntax,
    MutableBinding,
    UnresolvedBinding,
    AmbiguousMatch,
    Budget,
    MissingSource,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Location {
    pub side: Side,
    pub span: ByteSpan,
    pub text: String,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Operation {
    pub id: String,
    pub symbol: SymbolEntry,
    pub source: Location,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AssignmentKind {
    FieldInitializer,
    Assignment,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Assignment {
    pub id: String,
    pub operation_id: String,
    pub target: String,
    pub kind: AssignmentKind,
    pub value_id: String,
    pub source: Location,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BranchLabel {
    True,
    False,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Branch {
    pub id: String,
    pub label: BranchLabel,
    pub result_id: String,
    pub source: Location,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Conditional {
    pub predicate_id: String,
    pub branches: Vec<Branch>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Expression {
    pub id: String,
    pub source: Location,
    pub normalized_hash: ContentHash,
    pub conditional: Option<Conditional>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Binding {
    pub id: String,
    pub occurrence_id: String,
    pub name: String,
    pub value_id: Option<String>,
    pub declaration: Option<Location>,
    pub lexical_scope: Option<ByteSpan>,
    pub support: Support,
    pub limitations: Vec<Limitation>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CorrespondenceKind {
    Retained,
    Replaced,
    Added,
    Removed,
    Unmatched,
    Ambiguous,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MatchReason {
    SameTargetInMatchedSymbol,
    ExactNormalizedExpression,
    ResolvedBinding,
    SameBranchLabel,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Correspondence {
    pub id: String,
    pub base_ids: Vec<String>,
    pub source_ids: Vec<String>,
    pub kind: CorrespondenceKind,
    pub reasons: Vec<MatchReason>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Change {
    pub id: String,
    pub symbol_address: String,
    pub support: Support,
    pub limitations: Vec<Limitation>,
    pub operations: Vec<Operation>,
    pub assignments: Vec<Assignment>,
    pub expressions: Vec<Expression>,
    pub bindings: Vec<Binding>,
    pub correspondences: Vec<Correspondence>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Scope {
    pub symbol_address: String,
    pub target: String,
    pub support: Support,
    pub limitations: Vec<Limitation>,
    pub sources: Vec<Location>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Comparison {
    pub base_blob: Option<ContentHash>,
    pub source_blob: Option<ContentHash>,
    pub changes: Vec<Change>,
    pub analyzed: Vec<Scope>,
    pub omitted: Vec<Scope>,
    pub exhausted: bool,
}

/// Analyze one exact path pair. `None` means absent source, not an empty file.
/// Selectors are exact qualified symbol addresses. The host bounds file count
/// and total bytes; this function also bounds each parser input independently.
pub fn compare_file(
    path: &str,
    base: Option<&str>,
    source: Option<&str>,
    symbols: &[String],
) -> Comparison {
    let mut comparison = Comparison {
        base_blob: base.map(|s| ContentHash::compute_typed("blob", s.as_bytes())),
        source_blob: source.map(|s| ContentHash::compute_typed("blob", s.as_bytes())),
        changes: vec![],
        analyzed: vec![],
        omitted: vec![],
        exhausted: true,
    };
    compare::run(path, base, source, symbols, &mut comparison);
    comparison
}

#[cfg(test)]
mod tests;
