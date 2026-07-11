// SPDX-License-Identifier: Apache-2.0
//! Embeddable Heddle facade scaffolding.

pub mod context;
pub mod contract;
pub mod diff;
pub mod fsck;
pub mod merge;
pub mod query;
pub mod remote;
pub mod save;
pub mod status;
pub mod thread_shaping;
pub mod undo;
pub mod verify;
pub mod workflow;

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
pub use remote::{
    GitConfigContext, HostedPushPlan, IncludedGitRemoteConfigError, RemoteInfo, RemoteListReport,
    all_threads_uses_single_mirror_push, default_pull_thread_name, default_push_thread_name,
    git_overlay_current_thread_push_ok, list_plain_git_remotes, list_remotes, merged_remote_items,
    plain_git_remote_items, plan_hosted_push, resolve_default_remote_name,
    resolved_default_remote_name, show_plain_git_remote, show_remote, uses_git_overlay_mirror_rpc,
    uses_local_git_overlay_transport,
};
pub use save::{
    GitScope, SavePlan, SaveReport, SaveVerb, execute_save, plan_creates_new_state, plan_git_scope,
    plan_writes_git_checkpoint,
};
pub use status::{
    ActorInfo, ChangesInfo, CoordinationStatus, FastShortStatusProfile, FastShortStatusReport,
    GitImportGuidanceReport, GitIndexPlan, MaterializedThreadInfo, ParallelThreadInfo,
    PlainGitStatusReport, RepositoryVerificationCheck, RepositoryVerificationHealth, StateInfo,
    StatusCombinedVerdict, StatusDetail, StatusOptions, StatusProfile, StatusReport,
    StatusThreadSummary, assess_materialized_threads,
    build_repository_verification_health_with_worktree_status, changes_from_worktree_status,
    changes_path_count, changes_paths, combined_verdict_axes, coordination_axis_clean,
    coordination_label, coordination_severity, fast_short_status_report, git_index_plan_for_root,
    health_severity, human_thread_health, plain_git_status_report, resolve_coordination_with_trust,
    status, status_combined_verdict,
};
pub use thread_shaping::{
    CaptureSplitOptions, NoPathsMatchedDetails, ThreadMoveOptions, ThreadMoveOutput,
    ThreadShapingError, capture_split, thread_move,
};
pub use undo::{
    UndoBatchSummary, UndoHistoryAction, UndoListReport, UndoOperationSummary, UndoPlan,
    batch_status, empty_history_refusal, list_undo_history, list_undo_history_ctx,
    plan_redo_batches, plan_undo_batches, require_nonempty_history, summarize_batch,
    undo_mode_conflict, validate_undo_list_preview_modes,
};
pub use verify::{
    ActionAudience, ActionTemplate, MachineContractCoverage, MachineContractInput,
    PlainGitVerifyProbe, RepositoryContextInfo, RepositoryPresentation, RepositorySetupActionKind,
    RepositorySetupGuidance, RepositoryVerificationState, VerificationCheck, VerifyOptions,
    VerifyProfile, VerifyReport, build_plain_git_verification_probe,
    build_plain_git_verification_probe_with_machine_contract, build_repository_verification_state,
    build_repository_verification_state_with_machine_contract,
    build_repository_verification_state_with_worktree_status,
    build_repository_verification_state_with_worktree_status_and_machine_contract,
    dirty_path_count, repository_mode_label, repository_presentation, repository_setup_action_kind,
    repository_setup_guidance, verify,
};
pub use workflow::{
    AUTO_LAND_CONFIDENCE_RECOVERY_ACTION, AUTO_LAND_CONFIDENCE_THRESHOLD, AutoLandPolicyInput,
    LandPushOptions, LandPushPlan, LandPushPlanError, ReadyDecision, ReadyDecisionInput,
    auto_land_confidence_recovery_action, auto_land_policy_blockers, classify_ready_decision,
    has_integration_target, integrated_land_next_action, integration_blocker_recommended_action,
    integration_blockers, is_heavy_impact_advisory, is_integration_clear,
    land_blockers_for_preview, land_performed_steps, land_skipped_steps, land_warnings_for_preview,
    non_staleness_blockers, plan_land_push, ready_report_recommended_action,
    ready_scoped_next_action, ready_verification_preflight_blocks,
    ready_verification_status_blocks, recovery_scope_checkout, should_squash_land,
};
