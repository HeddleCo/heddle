// SPDX-License-Identifier: Apache-2.0
//! Embeddable Heddle facade scaffolding.

pub mod context;
pub mod contract;
pub mod diff;
pub mod fsck;
pub mod query;
pub mod status;
pub mod thread_shaping;
pub mod verify;

pub use context::{ExecutionContext, ExecutionContextBuilder, Verbosity};
pub use contract::{
    HeddleReport, MachineOutputKind, OutputDiscriminator, ReportContract, schema_for_report,
};
pub use diff::{
    ContextSnippet, DiffOptions, DiffReport, DiffStats, FileChange, FileContextEntry, FileEolState,
    LineCounts, LineDiff, PlainGitDiffProbe, SemanticChangeEntry, SymlinkChange,
    change_line_counts, compute_state_diff, compute_tree_diff, diff, diff_worktree_status,
    plain_git_head_diff, render_diff_patch, render_diff_patch_bytes, should_render_modified_pair,
    trim_added_decorations_for_display, write_diff_patch,
};
pub use fsck::{FsckError, FsckOptions, FsckReport, fsck};
pub use objects::{
    CollectingWarnings, HeddleError, NoopProgress, NoopWarnings, ProgressEvent, ProgressSink,
    TaskId, Warning, WarningSink,
};
pub use query::{QueryHit, QueryReport, QueryRequest, query};
pub use status::{
    ActorInfo, ChangesInfo, CoordinationStatus, FastShortStatusProfile, FastShortStatusReport,
    GitIndexPlan, GitOverlayHealth, GitOverlayHealthCheck, GitOverlayImportHintReport,
    MaterializedThreadInfo, ParallelThreadInfo, StateInfo, StatusDetail, StatusOptions,
    StatusProfile, StatusReport, StatusThreadSummary, assess_materialized_threads,
    changes_from_worktree_status, changes_path_count, changes_paths, fast_short_status_report,
    status,
};
pub use thread_shaping::{
    CaptureSplitOptions, NoPathsMatchedDetails, ThreadMoveOptions, ThreadMoveOutput,
    ThreadShapingError, capture_split, thread_move,
};
pub use verify::{
    ActionTemplate, MachineContractCoverage, PlainGitVerifyProbe, RepositoryContextInfo,
    RepositoryPresentation, RepositoryVerificationState, VerificationCheck, VerifyOptions,
    VerifyProfile, VerifyReport, dirty_path_count, repository_mode_label, repository_presentation,
    verify,
};
