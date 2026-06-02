// SPDX-License-Identifier: Apache-2.0
//! Repository: high-level interface for Heddle operations.

#[cfg(not(any(feature = "git-overlay", feature = "native")))]
compile_error!(
    "At least one of the `git-overlay` or `native` features must be enabled. \
     See crates/repo/Cargo.toml."
);

pub mod atomic;
pub mod daemon;
mod ephemeral_thread;
mod fsmonitor;
pub mod git_worktree_status;
mod hooks;
pub mod lazy_hydrator;
mod merge_state;
pub mod migration;
pub mod operation_dedup;
mod repository;
mod repository_redaction;
#[cfg(feature = "tree-sitter-symbols")]
mod repository_signals;
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
mod worktree_ignore;
pub mod worktree_index;
mod worktree_state;
mod worktree_status_options;

pub mod worktree_walk;

// Re-export commonly used types from underlying crates.
pub use ephemeral_thread::{CollapsedThread, collapse_expired_ephemeral_threads};
pub use fsmonitor::{ChangeMonitorReport, run_local_monitor_helper};
pub use hooks::{Hook, HookContext, HookManager, HookResponse};
pub use merge_state::{MergeState, MergeStateManager};
pub use objects::{
    error::{HeddleError as StoreError, HeddleError, Result},
    store::{
        AgentUsageSummary, FsStore, ObjectStore, ShallowInfo, SharedStore,
        agent_registry::{AgentEntry, AgentRegistry, AgentStatus, generate_agent_id},
    },
};
pub use repository::{
    BlobHydrator, ChangeMonitorInspection, ChangedPathFilter, ChangedPathFilters, CommitGraphIndex,
    CommitGraphInspection, ContextSuggestion, ContextSuggestionTier, DiffKind, GitCheckpointRecord,
    GitOverlayBranchTip, GitOverlayImportHint, GitRemoteTrackingStatus, HIGH_SUGGESTION_THRESHOLD,
    HistoryQuery, HostedConfig, MAJOR_REWRITE_THRESHOLD_PCT, MEDIUM_SUGGESTION_THRESHOLD,
    MissingBlob, OperationKind, OperationScope, OutputFormat, PackFilesInspection,
    PartialFetchInspection, PullPlannerCacheInspection, RedactConfig, RefCountsInspection,
    RefSummaryIndexInspection, RepoConfig, Repository, RepositoryCapability,
    RepositoryMaintenanceRunReport, RepositoryOperationStatus,
    RepositoryPerformanceInspectionReport, SUGGESTION_WINDOW, SnapshotExecution, SnapshotProfile,
    ThreadCaptureOutcome, TreeBuildProfile, TrustedKey, UntrackedSet, UntrackedSubtree,
    WarmCanonicalStoreStats, WorktreeCompareProfile, WorktreeIndexInspection,
    WorktreeStatusDetailed, compute_rewrite_pct, find_merge_base, is_major_rewrite,
    is_synthetic_root,
};
pub use repository_redaction::{PurgeOutcome, RemoveRedactionOutcome};
pub use session_storage::SessionManager;
pub use snapshot_metadata::{
    ABSENT_CONFIDENCE_DISPLAY, ThreadMetadataRefresh, classify_impact_categories,
    compute_heavy_impact_paths, format_confidence,
    refresh_active_thread_metadata, refresh_thread_freshness, summarize_confidence,
    summarize_verification, update_thread_state_from_state,
};
pub use stash::{StashEntry, StashManager};
pub use stack_snapshot::{
    REPOSITORY_SNAPSHOT_SCHEMA_VERSION, RepositorySnapshot, StackNextAction, ThreadSnapshot,
};
pub use thread_advice::{
    RecommendedAction, ThreadAdvice, describe_thread_advice, describe_thread_advice_with_initial,
};
pub use thread_stack::{
    PlanRebaseError, StackNode, StackRebasePlan, StackRebaseStep, ThreadStack, compute_stacks,
    plan_stack_rebase, stack_for,
};
pub use thread_model::{
    ConfidenceBand, EphemeralMarker, ThreadConfidenceSummary, ThreadFreshness, ThreadId,
    ThreadImpactCategory, ThreadIntegrationPolicy, ThreadLifecycleState, ThreadMode, ThreadRecord,
    ThreadRuntimeOverlay, ThreadState, ThreadVerificationSummary, ThreadView,
};
pub use thread_record_store::{FilesystemThreadRecordStore, ThreadRecordStore};
pub use thread_storage::{SyncedThreadMetadata, SyncedThreadMetadataStore, Thread, ThreadManager};
pub use worktree_index::{DirectoryCacheEntry, IndexEntry, WorktreeIndex};
pub use worktree_state::WorktreeState;
pub use worktree_status_options::{
    FsMonitorConfig, FsMonitorMode, FsMonitorSettings, WorktreeStatusOptions,
};
pub type Config = RepoConfig;
