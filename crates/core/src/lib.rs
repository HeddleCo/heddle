// SPDX-License-Identifier: Apache-2.0
//! Embeddable Heddle facade scaffolding.

pub mod actor;
pub mod context;
pub mod contract;
pub mod diff;
pub mod fsck;
pub mod merge;
pub mod query;
pub mod remote;
pub mod save;
pub mod status;
pub mod thread;
pub mod thread_lifecycle;
pub mod thread_plan;
pub mod thread_shaping;
pub mod undo;
pub mod verify;
pub mod workflow;

pub use actor::{
    ActorChainEntry, ActorDoneOptions, ActorDonePlan, ActorEntryReport, ActorListReport,
    ActorShowReport, ActorSpawnAttachMode, ActorSpawnError, ActorSpawnOptions, ActorSpawnPlan,
    ActorSpawnThreadSource, assemble_actor_entry, build_spawn_entry, complete_actor_entry,
    default_actor_thread_name, filter_actors, filter_actors_ref, is_explicit_identity, list_actors,
    list_actors_from_registry, mark_actor_done, nonempty_attr, plan_actor_done, plan_actor_spawn,
    resolve_spawn_thread_name, show_actor_by_session, show_actor_from_entry,
};
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
    GitConfigContext, HostedPushPlan, IncludedGitRemoteConfigError, PullPlan, PullPlanRequest,
    PushPath, PushPlan, PushPlanRequest, RemoteInfo, RemoteListReport, RemotePreflightBlocker,
    all_threads_uses_single_mirror_push, default_pull_thread_name, default_push_thread_name,
    git_overlay_current_thread_push_ok, git_overlay_thread_mismatch_blocker,
    list_plain_git_remotes, list_remotes, merged_remote_items, plain_git_remote_items,
    plan_hosted_push, plan_pull, plan_push, pull_requires_clean_worktree, pull_will_materialize,
    remote_missing_blocker, resolve_default_remote_name, resolved_default_remote_name,
    show_plain_git_remote, show_remote, transport_mismatch_blocker, uses_git_overlay_mirror_rpc,
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
pub use thread::{
    AvailableGitRef, ThreadActorInfo, ThreadListEntry, ThreadListOptions, ThreadListReport,
    ThreadSummary, ThreadTaskSummary, collect_thread_summaries, find_thread_summary, list_threads,
    split_available_git_refs, thread_is_available_git_ref, thread_is_imported_git_ref,
    visibility_label,
};
pub use thread_lifecycle::{
    CleanWorktreeGuard, ThreadDropDisposition, ThreadDropOptions, ThreadDropPlan,
    ThreadPromoteOptions, ThreadPromotePlan, ThreadRefreshOptions, ThreadRefreshPlan,
    contains_conflict_marker_bytes, format_refresh_conflict_markers, plan_clean_worktree_guard,
    plan_cleanup_thread_drop, plan_thread_drop, plan_thread_promote, plan_thread_refresh,
    promote_confirm_in_place_removal, promote_existing_checkout_path,
    promote_in_place_conversion_candidate, resolve_promote_target_path,
    should_materialize_refresh_conflict_markers, thread_mode_requires_unmount,
};
pub use thread_plan::{
    AutoWorkspaceDefault, ExplicitPathPlacement, ThreadBaseError, ThreadBaseSelection,
    ThreadCreateOptions, ThreadCreatePlan, ThreadPathIsolationError, ThreadPlanError,
    ThreadStartOptions, ThreadStartPlan, WorkspaceModeRequest, active_reservation_blocks_start,
    active_reservation_path_matches, check_explicit_path_isolation,
    classify_explicit_path_placement, explicit_path_allowed_for_git_overlay,
    mode_honors_explicit_path, path_isolation_enforced, plan_thread_create, plan_thread_mode,
    plan_thread_start, select_thread_base, start_requires_clean_worktree, validate_thread_name,
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
