// SPDX-License-Identifier: Apache-2.0
//! The shared local/hosted projection of semantic facts onto v2 analysis.
//! No transport, storage, provider or LLM dependency is involved in extraction.
use prost::Message;
use semantic::{behavior as model, semantic_index};

use crate::contract as api;

pub struct BehaviorAnalyzer {
    pub analysis: api::RecordRef,
    pub base: api::RevisionRef,
    pub source: api::RevisionRef,
}

pub struct FileBehavior {
    pub changes: Vec<api::BehaviorChange>,
    pub coverage: api::BehaviorAnalysisCoverage,
}

fn digest(parts: &[&[u8]]) -> Vec<u8> {
    let mut hash = blake3::Hasher::new_derive_key("heddle-behavior-analysis-v1");
    for part in parts {
        hash.update(&(part.len() as u64).to_le_bytes());
        hash.update(part);
    }
    hash.finalize().as_bytes().to_vec()
}
fn id(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn versions() -> api::BehaviorVersions {
    api::BehaviorVersions {
        analyzer: "heddle-rust-conditional-values".into(),
        analyzer_version: "1".into(),
        grammar_version: semantic_index::grammar_version(semantic::parser::Language::Rust).into(),
        symbol_extractor_version: semantic_index::EXTRACTOR_VERSION,
        expression_extractor_version: model::EXTRACTOR_VERSION,
        binding_resolver_version: model::BINDING_VERSION,
        comparison_version: model::COMPARISON_VERSION,
    }
}

fn exact(revision: &api::RevisionRef) -> bool {
    revision.spool.as_ref().is_some_and(|s| !s.id.is_empty())
        && match &revision.revision {
            Some(api::revision_ref::Revision::State(s)) => s.value.len() == 32,
            Some(api::revision_ref::Revision::GitCommitOid(s)) => {
                matches!(s.len(), 40 | 64)
                    && s.bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            }
            None => false,
        }
}

impl BehaviorAnalyzer {
    pub fn new(base: api::RevisionRef, source: api::RevisionRef) -> Result<Self, &'static str> {
        if !exact(&base) || !exact(&source) {
            return Err("analysis requires exact base and source revisions");
        }
        let analysis = api::RecordRef {
            spool: source.spool.clone(),
            id: id(&digest(&[
                &base.encode_to_vec(),
                &source.encode_to_vec(),
                &versions().encode_to_vec(),
            ])),
        };
        Ok(Self {
            analysis,
            base,
            source,
        })
    }

    /// Inputs must come from the given exact revision pair. Hosted callers first
    /// apply source-visibility and reachability checks through their read scope.
    pub fn compare(
        &self,
        path: &str,
        base: Option<&str>,
        source: Option<&str>,
        symbols: &[String],
    ) -> FileBehavior {
        let result = model::compare_file(path, base, source, symbols);
        let location = |source: &model::Location, symbol: &str| api::BehaviorSource {
            revision: Some(if source.side == model::Side::Base {
                self.base.clone()
            } else {
                self.source.clone()
            }),
            path: path.into(),
            blob_hash: if source.side == model::Side::Base {
                result.base_blob
            } else {
                result.source_blob
            }
            .map(|h| h.as_bytes().to_vec())
            .unwrap_or_default(),
            start_byte: source.span.start,
            end_byte: source.span.end,
            text: source.text.clone(),
            symbol_address: symbol.into(),
        };
        let dependencies = {
            let base_extraction = result.base_blob.map(model::extraction_key);
            let source_extraction = result.source_blob.map(model::extraction_key);
            let bytes = |value: Option<model::ContentHash>| {
                value.map(|h| h.as_bytes().to_vec()).unwrap_or_default()
            };
            let base_bindings = bytes(base_extraction.map(model::binding_key));
            let source_bindings = bytes(source_extraction.map(model::binding_key));
            api::BehaviorDependencies {
                base_extraction: bytes(base_extraction),
                source_extraction: bytes(source_extraction),
                comparison: digest(&[
                    self.analysis.id.as_bytes(),
                    path.as_bytes(),
                    &base_bindings,
                    &source_bindings,
                    &versions().encode_to_vec(),
                ]),
                base_bindings,
                source_bindings,
            }
        };
        let changes = result
            .changes
            .iter()
            .map(|c| {
                let loc = |s: &model::Location| Some(location(s, &c.symbol_address));
                api::BehaviorChange {
                    analysis: Some(self.analysis.clone()),
                    id: id(&digest(&[&dependencies.comparison, c.id.as_bytes()])),
                    base: Some(self.base.clone()),
                    source: Some(self.source.clone()),
                    path: path.into(),
                    symbol_address: c.symbol_address.clone(),
                    support: support(c.support),
                    limitations: limitations(&c.limitations),
                    versions: Some(versions()),
                    dependencies: Some(dependencies.clone()),
                    operations: c
                        .operations
                        .iter()
                        .map(|o| api::BehaviorOperation {
                            id: o.id.clone(),
                            symbol_address: o.symbol.address(),
                            semantic_hash: o.symbol.semantic_hash.as_bytes().to_vec(),
                            source: loc(&o.source),
                            provenance: api::BehaviorProvenance::Syntax as i32,
                        })
                        .collect(),
                    assignments: c
                        .assignments
                        .iter()
                        .map(|a| api::BehaviorAssignment {
                            id: a.id.clone(),
                            operation_id: a.operation_id.clone(),
                            target: a.target.clone(),
                            kind: match a.kind {
                                model::AssignmentKind::FieldInitializer => {
                                    api::behavior_assignment::Kind::FieldInitializer
                                }
                                model::AssignmentKind::Assignment => {
                                    api::behavior_assignment::Kind::Assignment
                                }
                            } as i32,
                            value_id: a.value_id.clone(),
                            source: loc(&a.source),
                            provenance: api::BehaviorProvenance::Syntax as i32,
                        })
                        .collect(),
                    expressions: c
                        .expressions
                        .iter()
                        .map(|e| api::BehaviorExpression {
                            id: e.id.clone(),
                            source: loc(&e.source),
                            normalized_hash: e.normalized_hash.as_bytes().to_vec(),
                            provenance: api::BehaviorProvenance::Syntax as i32,
                            conditional: e.conditional.as_ref().map(|condition| {
                                api::BehaviorConditional {
                                    predicate_id: condition.predicate_id.clone(),
                                    branches: condition
                                        .branches
                                        .iter()
                                        .map(|b| api::BehaviorBranch {
                                            id: b.id.clone(),
                                            label: match b.label {
                                                model::BranchLabel::True => {
                                                    api::behavior_branch::Label::True
                                                }
                                                model::BranchLabel::False => {
                                                    api::behavior_branch::Label::False
                                                }
                                            }
                                                as i32,
                                            result_id: b.result_id.clone(),
                                            source: loc(&b.source),
                                            provenance: api::BehaviorProvenance::Syntax as i32,
                                        })
                                        .collect(),
                                }
                            }),
                        })
                        .collect(),
                    bindings: c
                        .bindings
                        .iter()
                        .map(|b| api::BehaviorBinding {
                            id: b.id.clone(),
                            occurrence_id: b.occurrence_id.clone(),
                            name: b.name.clone(),
                            value_id: b.value_id.clone(),
                            declaration: b.declaration.as_ref().and_then(loc),
                            lexical_scope: b.lexical_scope.map(|s| api::BehaviorByteSpan {
                                start_byte: s.start,
                                end_byte: s.end,
                            }),
                            support: support(b.support),
                            limitations: limitations(&b.limitations),
                            provenance: api::BehaviorProvenance::BindingResolution as i32,
                        })
                        .collect(),
                    correspondences: c
                        .correspondences
                        .iter()
                        .map(|c| api::BehaviorCorrespondence {
                            id: c.id.clone(),
                            base_ids: c.base_ids.clone(),
                            source_ids: c.source_ids.clone(),
                            kind: correspondence(c.kind),
                            reasons: c.reasons.iter().copied().map(reason).collect(),
                            provenance: api::BehaviorProvenance::StructuralComparison as i32,
                        })
                        .collect(),
                }
            })
            .collect();
        let scope = |s: &model::Scope| api::BehaviorScope {
            path: path.into(),
            symbol_address: s.symbol_address.clone(),
            target: s.target.clone(),
            support: support(s.support),
            limitations: limitations(&s.limitations),
            sources: s
                .sources
                .iter()
                .map(|l| location(l, &s.symbol_address))
                .collect(),
        };
        FileBehavior {
            changes,
            coverage: api::BehaviorAnalysisCoverage {
                analysis: Some(self.analysis.clone()),
                base: Some(self.base.clone()),
                source: Some(self.source.clone()),
                analyzed: result.analyzed.iter().map(scope).collect(),
                omitted: result.omitted.iter().map(scope).collect(),
                selection_exhausted: result.exhausted,
                versions: Some(versions()),
            },
        }
    }
}

// Explicit mappings keep the transport taxonomy separate from the parser's
// domain model; neither serializes Rust discriminants as a wire contract.
macro_rules! enum_map {
    ($fn:ident, $from:ident, $to:ident, $($variant:ident),+ $(,)?) => {
        fn $fn(value: model::$from) -> i32 { match value { $(model::$from::$variant => api::$to::$variant as i32),+ } }
    };
}
enum_map!(
    support,
    Support,
    BehaviorSupport,
    Supported,
    Partial,
    Unsupported,
    Unavailable
);
enum_map!(
    limitation,
    Limitation,
    BehaviorLimitation,
    ParseError,
    Language,
    MissingElse,
    Syntax,
    MutableBinding,
    UnresolvedBinding,
    AmbiguousMatch,
    Budget,
    MissingSource
);
enum_map!(
    correspondence,
    CorrespondenceKind,
    BehaviorCorrespondenceKind,
    Retained,
    Replaced,
    Added,
    Removed,
    Unmatched,
    Ambiguous
);
enum_map!(
    reason,
    MatchReason,
    BehaviorMatchReason,
    SameTargetInMatchedSymbol,
    ExactNormalizedExpression,
    ResolvedBinding,
    SameBranchLabel
);
fn limitations(values: &[model::Limitation]) -> Vec<i32> {
    values.iter().copied().map(limitation).collect()
}
