// SPDX-License-Identifier: Apache-2.0
//! Repository: high-level interface for Heddle operations.

#[cfg(not(any(feature = "git-overlay", feature = "native")))]
compile_error!(
    "At least one of the `git-overlay` or `native` features must be enabled. \
     See crates/repo/Cargo.toml."
);

pub mod atomic;
pub mod daemon;
#[cfg(feature = "tree-sitter-symbols")]
mod discussion_anchor_travel;
#[cfg(feature = "tree-sitter-symbols")]
mod discussion_snapshot_travel;
mod ephemeral_thread;
mod fsmonitor;
mod git_ref_name;
pub mod git_worktree_status;
mod hooks;
pub mod identity;
pub mod lazy_hydrator;
mod merge_state;
pub mod migration;
pub mod namespace_policy;
pub mod operation_dedup;
mod repository;
mod repository_redaction;
#[path = "repository_resolve_for_command.rs"]
mod repository_resolve_for_command;
#[cfg(feature = "tree-sitter-symbols")]
mod repository_signals;
mod repository_state_visibility;
mod revision_address;
pub use repository_state_visibility::{
    DefaultVisibilityBinding, PutVisibilityOutcome, VisibilityCommitKind, VisibilityCommitOutcome,
    VisibilitySidecarRestore,
};
mod session_storage;
pub mod snapshot_metadata;
pub mod staleness;
mod stash;
mod stat_signature;
/// Re-export of the symbol resolver. The implementation lives in
/// `crates/semantic/src/symbol_resolver.rs` so anchor-travel code in
/// `objects`-adjacent modules can use it without depending on `repo`.
/// Existing callers (`heddle context set ...`, `crates/repo/src/staleness.rs`)
/// continue to import `repo::symbol_resolver::*` unchanged.
#[cfg(feature = "tree-sitter-symbols")]
pub use semantic::symbol_resolver;
mod stack_snapshot;
mod thread_advice;
pub mod thread_manifest;
mod thread_model;
mod thread_record_store;
mod thread_stack;
mod thread_storage;
mod thread_worktree_target;
mod timeline_actions;
mod timeline_materialize;
mod timeline_navigation;
mod timeline_store;
mod timeline_view;
pub mod visibility;
mod worktree_ignore;
pub mod worktree_index;
mod worktree_state;
mod worktree_status_options;

pub mod worktree_walk;

// Re-export commonly used types from underlying crates.
pub use ephemeral_thread::{CollapsedThread, collapse_expired_ephemeral_threads};
pub use fsmonitor::{ChangeMonitorReport, run_local_monitor_helper};
pub use git_ref_name::{
    GitRefContentNamespace, GitRefKind, GitRefName, GitRefNamespace, ParsedGitRef,
    REMOTE_NAME_FOR_LOCAL_GIT_REPO, is_reserved_git_remote_name,
};
pub use hooks::{Hook, HookContext, HookManager, HookResponse};
pub use merge_state::{MergeState, MergeStateManager};
pub use objects::{
    error::{HeddleError as StoreError, HeddleError, Result},
    object::{
        BranchCreatedV1, CursorMovedV1, NativeToolCallRefV1, TIMELINE_OPERATION_SCHEMA_VERSION,
        TimelineBranchId, TimelineBranchReason, TimelineCodecError, TimelineCursorMoveReason,
        TimelineLabel, TimelineOperationBodyV1, TimelineOperationEnvelope, TimelineOperationId,
        TimelineOperationIdParseError, TimelineOperationKind, TimelineStepId,
        TimelineToolCallStatus, TimelineToolPayloadMetadata, ToolCallFinishedV1, ToolCallStartedV1,
    },
    store::{
        AgentUsageSummary, FsStore, ObjectStore, ShallowInfo,
        agent_registry::{AgentEntry, AgentRegistry, AgentStatus, generate_agent_id},
    },
};
#[cfg(feature = "async-source")]
pub use repository::query_history_async;
pub use repository::{
    BlobHydrator, ChangeMonitorInspection, ChangedPathFilter, ChangedPathFilters,
    CheckoutMaterialization, CommitGraphIndex, CommitGraphInspection, ContextSuggestion,
    ContextSuggestionTier, DiffKind, GitCheckpointRecord, GitImportGuidance, GitOverlayBranchTip,
    GitOverlayOutOfBandCommits, GitRemoteTrackingStatus, HIGH_SUGGESTION_THRESHOLD, HistoryQuery,
    HostedConfig, MAJOR_REWRITE_THRESHOLD_PCT, MEDIUM_SUGGESTION_THRESHOLD, MissingBlob,
    OperationKind, OperationScope, OutputFormat, PackFilesInspection, PartialFetchInspection,
    PullPlannerCacheInspection, RedactConfig, RefCountsInspection, RefSummaryIndexInspection,
    RepoConfig, Repository, RepositoryCapability, RepositoryMaintenanceRunReport,
    RepositoryOperationStatus, RepositoryPerformanceInspectionReport, ResignOutcome,
    SUGGESTION_WINDOW, SnapshotExecution, SnapshotProfile, SpoolFacet, ThreadCaptureOutcome,
    TreeBuildProfile, TrustedKey, UntrackedSet, UntrackedSubtree, WarmCanonicalStoreStats,
    WorktreeCompareProfile, WorktreeIndexInspection, WorktreeStatusDetailed, compute_rewrite_pct,
    find_merge_base, is_major_rewrite, is_synthetic_root,
};
#[cfg(feature = "async-source")]
pub use repository::{find_merge_base_async, is_ancestor_async};
pub use repository_redaction::{PurgeOutcome, RemoveRedactionOutcome};
pub use repository_resolve_for_command::{
    EmptyHeadBootstrap, ResolvePolicy, ResolvedState, StateResolveError, StateResolveFailure,
    resolve_state_for_command,
};
pub use revision_address::{RevisionAddress, RevisionAddressParseError};
pub use session_storage::SessionManager;
pub use snapshot_metadata::{
    ABSENT_CONFIDENCE_DISPLAY, ThreadMetadataRefresh, classify_impact_categories,
    compute_heavy_impact_paths, format_confidence, refresh_active_thread_metadata,
    refresh_thread_freshness, summarize_confidence, summarize_verification,
    update_thread_state_from_state,
};
pub use stack_snapshot::{
    REPOSITORY_SNAPSHOT_SCHEMA_VERSION, RepositorySnapshot, StackNextAction, ThreadSnapshot,
};
pub use stash::{StashEntry, StashManager};
pub use thread_advice::{
    RecommendedAction, ThreadAdvice, describe_thread_advice, describe_thread_advice_with_initial,
    shell_quote, thread_flag,
};
pub use thread_model::{
    ConfidenceBand, EphemeralMarker, ThreadConfidenceSummary, ThreadFreshness, ThreadId,
    ThreadIdError, ThreadImpactCategory, ThreadIntegrationPolicy, ThreadMode, ThreadRecord,
    ThreadRuntimeOverlay, ThreadState, ThreadVerificationSummary, ThreadView, validate_thread_id,
};
pub use thread_record_store::FilesystemThreadRecordStore;
pub use thread_stack::{
    PlanRebaseError, StackNode, StackRebasePlan, StackRebaseStep, ThreadStack, compute_stacks,
    plan_stack_rebase, stack_for,
};
pub use thread_storage::{SyncedThreadMetadata, Thread, ThreadManager};
pub use thread_worktree_target::{
    ThreadWorktreeTargetDisposition, ThreadWorktreeTargetError, validate_thread_worktree_target,
};
pub use timeline_actions::{TimelineForkOutcome, TimelineRecoverOutcome, TimelineResetOutcome};
pub use timeline_materialize::{
    TimelineMaterializationBlocker, TimelineMaterializationBoundaryStatus,
    TimelineMaterializationRecoveryBlocker, TimelineMaterializationRecoveryOutcome,
    TimelineMaterializationRecoveryStatus, TimelineMaterializeMode, TimelineMaterializeOutcome,
    TimelineMaterializeStatus, TimelineSeekBranchConstraint, TimelineSeekPreview,
    TimelineSeekSelector,
};
pub use timeline_navigation::{
    TimelineNavigationActionAvailability, TimelineNavigationBranch, TimelineNavigationCursor,
    TimelineNavigationRecovery, TimelineNavigationRecoveryStatus, TimelineNavigationSnapshot,
    TimelineNavigationStep,
};
pub use timeline_store::{TimelineMaterializationRecoveryRecord, TimelineStore};
pub use timeline_view::{
    TimelineBranchKey, TimelineBranchSummary, TimelineCursorMoveRecord, TimelineNativeToolKey,
    TimelineSeekTarget, TimelineStepKey, TimelineStepSummary, TimelineThreadStatus, TimelineView,
};
pub use visibility::{
    AudienceParseError, AudienceTier, ScopeDropCounts, filter_for_audience,
    filter_for_audience_with_drops, visible,
};
pub use worktree_index::{DirectoryCacheEntry, IndexEntry, WorktreeIndex};
pub use worktree_state::WorktreeState;
pub use worktree_status_options::{
    FsMonitorConfig, FsMonitorMode, FsMonitorSettings, WorktreeStatusOptions,
};
pub type Config = RepoConfig;
