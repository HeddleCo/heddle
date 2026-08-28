// SPDX-License-Identifier: Apache-2.0
//! Core object primitives extracted from the monolith.

#[macro_use]
mod versioned_blob;

mod action_id;
mod action_operation;
mod action_struct;
mod annotated_tag;
mod audience_tier;
mod blob;
pub mod collaboration;
mod diff;
mod discussion;
mod frontier_ref;
mod hash;
mod identifiers;
mod key_binding;
pub mod manifest;
mod operation_id;
mod redaction;
mod risk_signal;
mod semantic_change;
mod semantic_edges;
mod semantic_graph_query;
mod semantic_index;
mod semantic_reverse_deps;
mod session;
mod source;
mod spool_id;
mod staleness_core;
mod state_attachment;
mod state_attribution;
mod state_context;
mod state_core;
mod state_provenance;
mod state_review;
mod state_visibility;
mod structured_conflict;
#[cfg(test)]
mod structured_conflict_tests;
mod suggestion_core;
mod timeline;
mod tree;
mod tree_canonical;
mod tree_diff;
mod tree_path;
mod tree_source;
mod tree_stream;
pub mod tree_walk;
mod visibility_tier;

pub use action_id::ActionId;
pub use action_operation::Operation;
pub use action_struct::Action;
pub use annotated_tag::{AnnotatedTag, AnnotatedTagError, AnnotatedTagMarker};
pub use audience_tier::{AudienceParseError, AudienceTier, visible};
pub use blob::Blob;
pub use collaboration::*;
pub use diff::{DiffKind, FileChange, FileChangeSet};
pub use discussion::{
    Discussion, DiscussionError, DiscussionId, DiscussionReference, DiscussionReferenceKind,
    DiscussionResolution, DiscussionTurn, DiscussionsBlob, generate_discussion_id,
};
pub use frontier_ref::{
    GIT_SYNTHETIC_FRONTIER_PREFIX, SYNTHETIC_FRONTIER_PREFIX, SyntheticFrontierName,
    SyntheticFrontierNameError,
};
pub use hash::{ChangeId, ChangeIdParseError, ContentHash, StateId, StateIdParseError};
pub use identifiers::{
    MarkerName, RESERVED_REF_SEGMENT, ReservedRefNameError, Scope, ThreadName,
    is_reserved_heddle_namespace,
};
pub use key_binding::{
    KEY_BINDING_REGISTRY_SIGNING_PAYLOAD_VERSION_TAG, KEY_BINDING_SIGNING_PAYLOAD_VERSION_TAG,
    KeyBinding, KeyBindingError, KeyBindingRegistry, KeyRole,
};
pub use manifest::{
    BuiltManifest, FsckFinding, FsckOptions, FsckReport, FsckRule, ManifestBinding,
    ManifestBuildError, ManifestFacet, ManifestKey, ManifestNode, ManifestNodeSource,
    ManifestObject, ManifestObjectKind, ManifestOwnerKind, PackRangeAudit, PackRangeClaim,
    PackRecord, build_manifest, expand_manifest, fsck_manifest, fsck_manifest_with,
    fsck_pack_range,
};
pub use operation_id::{OperationId, OperationIdParseError};
pub use redaction::{
    PURGE_SIGNING_PAYLOAD_VERSION_TAG, PurgeEvidence, REDACTION_SIGNING_PAYLOAD_VERSION_TAG,
    Redaction, RedactionError, RedactionsBlob,
};
pub use risk_signal::{
    MAX_REASON_LEN, ProducerId, RiskSignal, RiskSignalBlob, RiskSignalError, RiskSignalKind,
    SignalAnchor,
};
pub use semantic_change::{ChangeImportance, ModificationKind, SemanticChange};
pub use semantic_edges::{BindingDelta, FileBindingDelta, ResolvedSemanticEdge, SemanticEdgeKind};
pub use semantic_graph_query::{
    SemanticGraphQueryKind, SemanticGraphQueryRequest, SemanticGraphQueryResponse, SemanticGraphRef,
};
pub use semantic_index::{
    ByteSpan, ImportBinding, ImportEntry, ImportKindTag, OccurrenceEntry, OccurrenceRole,
    ScopeEntry, ScopeKind, SemanticEntryKind, SemanticFileFacts, SemanticFileNode,
    SemanticIndexError, SemanticIndexRoot, SemanticTreeEntry, SemanticTreeNode, SymbolEntry,
    SymbolKindTag, SymbolNamespace, compute_dir_semantic_digest, compute_file_scaffold_hash,
    compute_file_semantic_digest, compute_symbol_semantic_hash,
};
pub use semantic_reverse_deps::ReverseDependencyIndex;
pub use session::{Session, SessionSegment, generate_session_id};
#[cfg(feature = "async-source")]
pub use source::AsyncObjectSource;
pub use source::ObjectSource;
pub use spool_id::{SpoolId, SpoolIdParseError};
pub use staleness_core::{
    StalenessStatus, annotation_status_for_source,
    annotation_status_for_source_with_symbol_resolver, extract_line_range, resolve_current_symbol,
};
pub use state_attachment::{
    StateAttachment, StateAttachmentBody, StateAttachmentId, StateAttachmentKind,
};
pub use state_attribution::{Agent, Attribution, Principal};
pub use state_context::{
    Annotation, AnnotationAnchorStatus, AnnotationKind, AnnotationRevision, AnnotationScope,
    AnnotationStatus, ContextBlob, ContextError, ContextTarget,
};
pub use state_core::{
    ChangeLineage, ChangeLineageKind, SignatureStatus, State, StateSignature, Status, Verification,
    parse_commit_extension_headers,
};
pub use state_provenance::{FileProvenance, LineSpan, Origin, OriginSet, ProvenanceError};
pub use state_review::{
    ReviewKind, ReviewScope, ReviewSignature, ReviewSignatureError, ReviewSignaturesBlob,
    SymbolAnchor, signing_payload,
};
pub use state_visibility::{
    STATE_VISIBILITY_SIGNING_PAYLOAD_VERSION_TAG, StateVisibility, StateVisibilityBlob,
    StateVisibilityError,
};
pub use structured_conflict::{
    ConflictError, ConflictRange, ConflictRegion, ConflictSide, StructuredConflict,
};
pub use suggestion_core::{
    ContextSuggestion, ContextSuggestionTier, HIGH_SUGGESTION_THRESHOLD,
    MAJOR_REWRITE_THRESHOLD_PCT, MEDIUM_SUGGESTION_THRESHOLD, SUGGESTION_WINDOW, SuggestionInputs,
    SuggestionSignal, score_suggestions,
};
pub use timeline::{
    BranchCreatedV1, CursorMovedV1, NativeToolCallRefV1, TIMELINE_OPERATION_SCHEMA_VERSION,
    TimelineBranchId, TimelineBranchReason, TimelineCodecError, TimelineCursorMoveReason,
    TimelineLabel, TimelineOperationBodyV1, TimelineOperationEnvelope, TimelineOperationId,
    TimelineOperationIdParseError, TimelineOperationKind, TimelineStepId, TimelineToolCallStatus,
    TimelineToolPayloadMetadata, ToolCallFinishedV1, ToolCallStartedV1,
};
pub use tree::{
    EntryType, FileMode, Tree, TreeDecodeError, TreeEntry, TreeEntryTarget, TreeError,
    validate_name as validate_tree_entry_name,
};
pub use tree_canonical::{
    TREE_BLOCK_ENCODING_VERSION, TREE_BLOCK_MIN_ENTRIES, TREE_CANONICAL_MAGIC,
    TREE_DELTA_ANCHOR_INTERVAL, TREE_DELTA_ENCODING_VERSION, TREE_DELTA_HEADER_LEN,
    TREE_DELTA_MAGIC, TREE_DELTA_MAX_OPS, TREE_ENCODING_VERSION, TREE_HEADER_LEN,
    TREE_LEAN_ENCODING_VERSION, TREE_LEAN_MAGIC, TreeDeltaHeader, TreeDeltaOp, TreeHeader,
    apply_tree_delta, decode_header, decode_lean_prefix, decode_tree_delta,
    decode_tree_delta_header, decode_tree_delta_header_prefix, decode_tree_delta_ops,
    decode_tree_delta_ops_prefix, encode_lean_entry, encode_tree_delta, is_canonical_tree,
    is_delta_tree, is_lean_tree, is_streamable_tree, tree_delta,
};
#[cfg(feature = "async-source")]
pub use tree_diff::diff_trees_visit_async;
pub use tree_diff::{diff_trees, diff_trees_visit};
#[cfg(feature = "async-source")]
pub use tree_path::resolve_tree_path_async;
pub use tree_path::{
    LeafPolicy, ResolvedTreeTarget, TreePathResolveError, resolve_tree_path, split_path,
};
pub use tree_source::{
    BytesTreeSource, FileTreeSource, OpenedTreeBody, TreeBodyIntegrity, TreeByteSource,
};
pub use tree_stream::{
    TreeEntryReader, TreePage, TreePageLimits, TreeResumeCursor, TreeStreamError,
};
pub use tree_walk::{TreeIntegrityEvent, walk_tree_integrity};
pub use visibility_tier::VisibilityTier;
