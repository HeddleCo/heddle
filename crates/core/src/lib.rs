// SPDX-License-Identifier: Apache-2.0
//! Embeddable Heddle facade scaffolding.

pub mod context;
pub mod contract;
pub mod diff;
pub mod fsck;
pub mod merge;
pub mod query;
pub mod save;
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
pub use fsck::{FsckError, FsckOptions, FsckRepair, FsckReport, fsck};
pub use merge::{
    GitCommitInfo, GitCommitPreview, MergeAttemptPlan, MergeOptions, MergePlan, MergeRelation,
    MergeRelationKind, MergeReport, OperatorAction as MergeOperatorAction,
    OperatorCommandOutput as MergeOperatorCommandOutput, PreviewTarget, ThreadPreviewReport,
    ThreeWayMergeOutcome, apply_merged_tree, apply_merged_tree_external, bench_detect_renames,
    bench_find_merge_base, bench_three_way_merge, build_thread_preview_report,
    ensure_worktree_clean, merge_thread, merge_thread_into_current,
    merge_thread_into_current_with_machine_contract, prepare_dir_for_file_replacement,
    try_three_way_merge_between_tips,
};
pub use objects::{
    CollectingWarnings, HeddleError, NoopProgress, NoopWarnings, ProgressEvent, ProgressSink,
    TaskId, Warning, WarningSink,
};
pub use query::{QueryHit, QueryReport, QueryRequest, query};
pub use save::{
    GitScope, SavePlan, SaveReport, SaveVerb, execute_save, plan_creates_new_state, plan_git_scope,
    plan_writes_git_checkpoint,
};
pub use status::{
    ActorInfo, ChangesInfo, CoordinationStatus, FastShortStatusProfile, FastShortStatusReport,
    GitImportGuidanceReport, GitIndexPlan, MaterializedThreadInfo, ParallelThreadInfo,
    RepositoryVerificationCheck, RepositoryVerificationHealth, StateInfo, StatusDetail,
    StatusOptions, StatusProfile, StatusReport, StatusThreadSummary, assess_materialized_threads,
    build_repository_verification_health_with_worktree_status, changes_from_worktree_status,
    changes_path_count, changes_paths, fast_short_status_report, status,
};
pub use thread_shaping::{
    CaptureSplitOptions, NoPathsMatchedDetails, ThreadMoveOptions, ThreadMoveOutput,
    ThreadShapingError, capture_split, thread_move,
};
pub use verify::{
    ActionAudience, ActionTemplate, MachineContractCoverage, MachineContractInput,
    PlainGitVerifyProbe, RepositoryContextInfo, RepositoryPresentation,
    RepositoryVerificationState, VerificationCheck, VerifyOptions, VerifyProfile, VerifyReport,
    build_plain_git_verification_probe, build_plain_git_verification_probe_with_machine_contract,
    build_repository_verification_state, build_repository_verification_state_with_machine_contract,
    build_repository_verification_state_with_worktree_status,
    build_repository_verification_state_with_worktree_status_and_machine_contract,
    dirty_path_count, repository_mode_label, repository_presentation, verify,
};
