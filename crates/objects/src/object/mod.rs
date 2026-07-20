// SPDX-License-Identifier: Apache-2.0
//! Core object primitives extracted from the monolith.

#[macro_use]
mod versioned_blob;

mod action_id;
mod action_operation;
mod action_struct;
mod blob;
pub mod collaboration;
mod diff;
mod discussion;
mod hash;
mod identifiers;
mod operation_id;
mod redaction;
mod risk_signal;
mod semantic_change;
mod semantic_index;
mod session;
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
mod suggestion_core;
mod timeline;
mod tree;
mod tree_diff;
mod tree_path;
pub mod tree_walk;
mod visibility_tier;

pub use action_id::ActionId;
pub use action_operation::Operation;
pub use action_struct::Action;
pub use blob::Blob;
pub use collaboration::*;
pub use diff::{DiffKind, FileChange, FileChangeSet};
pub use discussion::{
    Discussion, DiscussionError, DiscussionId, DiscussionResolution, DiscussionTurn,
    DiscussionsBlob, generate_discussion_id,
};
pub use hash::{ChangeId, ChangeIdParseError, ContentHash, StateId, StateIdParseError};
pub use identifiers::{MarkerName, Scope, ThreadName};
pub use operation_id::{OperationId, OperationIdParseError};
pub use redaction::{
    REDACTION_SIGNING_PAYLOAD_VERSION_TAG, Redaction, RedactionError, RedactionsBlob,
};
pub use risk_signal::{
    MAX_REASON_LEN, ProducerId, RiskSignal, RiskSignalBlob, RiskSignalError, RiskSignalKind,
    SignalAnchor,
};
pub use semantic_change::{ChangeImportance, ModificationKind, SemanticChange};
pub use semantic_index::{
    SemanticEntryKind, SemanticFileNode, SemanticIndexError, SemanticIndexRoot, SemanticTreeEntry,
    SemanticTreeNode, SymbolEntry, SymbolKindTag, compute_dir_semantic_digest,
    compute_file_semantic_digest, compute_symbol_semantic_hash,
};
pub use session::{Session, SessionSegment, generate_session_id};
pub use spool_id::{SpoolId, SpoolIdParseError};
pub use staleness_core::{
    StalenessStatus, annotation_status_for_source,
    annotation_status_for_source_with_symbol_resolver, extract_line_range, resolve_current_symbol,
};
pub use state_attachment::{StateAttachment, StateAttachmentBody, StateAttachmentId};
pub use state_attribution::{Agent, Attribution, Principal};
pub use state_context::{
    Annotation, AnnotationKind, AnnotationRevision, AnnotationScope, AnnotationStatus, ContextBlob,
    ContextError, ContextTarget,
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
    ConflictError, ConflictResolution, ConflictSide, ConflictSymbol, StructuredConflict,
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
pub(crate) use tree::TreeDecodeError;
pub use tree::{
    EntryType, FileMode, Tree, TreeEntry, TreeEntryTarget, TreeError,
    validate_name as validate_tree_entry_name,
};
#[cfg(feature = "async-source")]
pub use tree_diff::diff_trees_visit_async;
pub use tree_diff::{diff_trees, diff_trees_visit};
#[cfg(feature = "async-source")]
pub use tree_path::resolve_tree_path_async;
pub use tree_path::{
    LeafPolicy, ResolvedTreeTarget, TreePathResolveError, resolve_tree_path, split_path,
};
pub use tree_walk::{TreeIntegrityEvent, walk_tree_integrity};
pub use visibility_tier::VisibilityTier;
