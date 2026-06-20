// SPDX-License-Identifier: Apache-2.0
//! Core object primitives extracted from the monolith.

#[macro_use]
mod versioned_blob;

mod action_id;
mod action_operation;
mod action_struct;
mod blob;
mod diff;
mod discussion;
mod hash;
mod identifiers;
mod operation_id;
mod redaction;
mod risk_signal;
mod semantic_change;
mod session;
mod state_attribution;
mod state_context;
mod state_core;
mod state_provenance;
mod state_review;
mod state_visibility;
mod staleness_core;
mod structured_conflict;
mod suggestion_core;
mod timeline;
mod tree;
mod tree_diff;
mod visibility_tier;

pub use action_id::ActionId;
pub use action_operation::Operation;
pub use action_struct::Action;
pub use blob::Blob;
pub use diff::{DiffKind, FileChange, FileChangeSet};
pub use discussion::{
    Discussion, DiscussionError, DiscussionId, DiscussionResolution, DiscussionTurn,
    DiscussionsBlob,
};
pub use hash::{ChangeId, ChangeIdParseError, ContentHash};
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
pub use session::{Session, SessionSegment, generate_session_id};
pub use state_attribution::{Agent, Attribution, Principal};
pub use state_context::{
    Annotation, AnnotationKind, AnnotationRevision, AnnotationScope, AnnotationStatus, ContextBlob,
    ContextError, ContextTarget,
};
pub use state_core::{
    SignatureStatus, State, StateSignature, Status, Verification, parse_commit_extension_headers,
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
pub use staleness_core::{
    StalenessStatus, annotation_status_for_source,
    annotation_status_for_source_with_symbol_resolver, extract_line_range, resolve_current_symbol,
};
pub use structured_conflict::{
    ConflictError, ConflictResolution, ConflictSide, ConflictSymbol, StructuredConflict,
};
pub use suggestion_core::{
    ContextSuggestion, ContextSuggestionTier, HIGH_SUGGESTION_THRESHOLD,
    MAJOR_REWRITE_THRESHOLD_PCT, MEDIUM_SUGGESTION_THRESHOLD, SUGGESTION_WINDOW,
    SuggestionInputs, SuggestionSignal, score_suggestions,
};
pub use timeline::{
    BranchCreatedV1, CursorMovedV1, NativeToolCallRefV1, TIMELINE_OPERATION_SCHEMA_VERSION,
    TimelineBranchId, TimelineBranchReason, TimelineCodecError, TimelineCursorMoveReason,
    TimelineLabel, TimelineOperationBodyV1, TimelineOperationEnvelope, TimelineOperationId,
    TimelineOperationIdParseError, TimelineOperationKind, TimelineStepId, TimelineToolCallStatus,
    TimelineToolPayloadMetadata, ToolCallFinishedV1, ToolCallStartedV1,
};
pub use tree::{
    EntryType, FileMode, Tree, TreeEntry, TreeError, validate_name as validate_tree_entry_name,
};
#[cfg(feature = "async-source")]
pub use tree_diff::diff_trees_visit_async;
pub use tree_diff::{diff_trees, diff_trees_visit};
pub use visibility_tier::VisibilityTier;
