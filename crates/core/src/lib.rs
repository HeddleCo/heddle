// SPDX-License-Identifier: Apache-2.0
//! Embeddable Heddle facade scaffolding.

pub mod actor;
pub mod agent_fanout;
pub mod agent_ops;
pub mod approval_plan;
pub mod clean_plan;
pub mod clone_plan;
pub mod collapse_plan;
pub mod completion_plan;
pub mod context;
pub mod context_plan;
pub mod contract;
pub mod daemon_plan;
pub mod diagnose_plan;
pub mod diff;
pub mod doctor_docs_plan;
pub mod doctor_schemas_plan;
pub mod fsck;
pub mod gc_plan;
pub mod git_projection_io_plan;
pub mod harness_json;
pub mod harness_policy;
pub mod hook_plan;
pub mod index_plan;
pub mod init_plan;
pub mod integration_plan;
pub mod log_plan;
pub mod maintenance_plan;
pub mod marker_plan;
pub mod merge;
pub mod monitor_plan;
pub mod onboarding;
pub mod oplog_plan;
pub mod oss_plan;
pub mod prove_plan;
pub mod purge_plan;
pub mod query;
pub mod rebase_plan;
pub mod redact_plan;
pub mod remote;
pub mod resolve_plan;
pub mod retro_plan;
pub mod revert_plan;
pub mod run_plan;
pub mod save;
pub mod semantic_plan;
pub mod shell_plan;
pub mod source_authority;
pub mod spool_plan;
pub mod stash_plan;
pub mod status;
pub mod switch_plan;
pub mod thread;
pub mod thread_lifecycle;
pub mod thread_materialize;
pub mod thread_plan;
pub mod thread_shaping;
pub mod timeline_plan;
pub mod try_plan;
pub mod undo;
pub mod verify;
pub mod visibility_plan;
pub mod watch_plan;
pub mod workflow;

pub use actor::{
    ActorChainEntry, ActorDoneOptions, ActorDonePlan, ActorEntryReport, ActorListReport,
    ActorShowReport, assemble_actor_entry, complete_actor_entry, filter_actors, filter_actors_ref,
    list_actors, list_actors_from_registry, mark_actor_done, plan_actor_done,
    show_actor_by_session, show_actor_from_entry,
};
pub use agent_fanout::{
    FanoutBaseFacts, FanoutBaseSelection, FanoutCommandSpec, FanoutLaneAvailability,
    FanoutLanePreflightBlock, FanoutLaneReport, FanoutNodeSpec, FanoutPlan, FanoutPlanError,
    FanoutPlanReport, FanoutPlanRequest, FanoutTaskPlaceholder, assemble_fanout_plan_report,
    assemble_fanout_start_commands, check_fanout_start_preflight, ensure_unique_thread_names,
    fanout_child_body, fanout_parent_body, fanout_parent_delegated_by, fanout_start_attach_rule,
    parse_fanout_lane, parse_fanout_lanes, plan_fanout, select_fanout_base,
    select_fanout_parent_thread,
};
pub use agent_ops::{
    AgentCaptureOptions, AgentCapturePlan, AgentCapturePlanError, AgentCaptureThreadCheck,
    AgentExplainReport, AgentReadyOptions, AgentReadyPlan, AgentReadyPlanError,
    AgentReservationListReport, AgentReservationReport, assemble_agent_explain,
    assemble_agent_reservation, assemble_agent_reservation_list, check_agent_capture_thread,
    default_attach_reason_message, filter_agent_reservations, filter_agent_reservations_ref,
    plan_agent_capture, plan_agent_ready,
};
pub use approval_plan::{
    EligibilitySummary, approval_recorded_message, approval_revoked_message,
    approvals_empty_message, approvals_header, eligibility_allowed_message,
    eligibility_approvals_counted_message, eligibility_blocked_message, format_unix_secs_display,
    format_unix_secs_label, plan_eligibility_summary, short_state_id, state_id_bytes_to_string,
    timestamp_secs_u64, unmet_requirement_line,
};
pub use clean_plan::{
    clean_empty_message, clean_path_line, clean_paths_header, clean_result_lines, clean_result_text,
};
pub use clone_plan::{
    AdoptPlan, AdoptPlanError, AdoptPlanOptions, CloneMode, ClonePlan, ClonePlanError,
    ClonePlanFacts, ClonePlanOptions, CloneRemoteSource, CloneSecurityPreflight,
    MonorepoCloneJsonReport, MonorepoClonePlan, MonorepoCloneResultSummary, MonorepoEdgeFacts,
    MonorepoEdgeSkipReason, MonorepoExecutionPlan, MonorepoExecutionProgress,
    MonorepoNodeExecution, MonorepoNodeExecutionError, MonorepoNodeExecutionStep,
    MonorepoNodeFacts, MonorepoNodePlan, MonorepoNodeStepOptions, MonorepoPlacedJsonRow,
    MonorepoPlacedNodeSummary, MonorepoSkippedChild, MonorepoSkippedJsonRow, UnsupportedCloneFlag,
    absolute_path, assemble_clone_security_preflight, assemble_monorepo_clone_json_report,
    assemble_monorepo_clone_result_summary, looks_like_git_overlay_url, looks_like_local_path,
    monorepo_execution_progress, monorepo_rel_display, normalize_clone_depth, plan_adopt,
    plan_clone, plan_monorepo_clone, plan_monorepo_execution, plan_monorepo_node_steps,
    resolve_adopt_start_path, resolve_clone_destination, select_clone_mode,
    validate_clone_destination, validate_clone_mode_options, validate_monorepo_clone_options,
    validate_monorepo_execution, validate_monorepo_node_execution,
};
pub use collapse_plan::{
    CollapsePlan, collapse_has_source_states, collapse_states_required_kind, plan_collapse,
};
pub use context::{ExecutionContext, ExecutionContextBuilder, Verbosity};
pub use context_plan::{
    ContextContentPlanError, ContextRmPlanError, annotation_passes_filters,
    annotation_status_label, audit_duplicate_count, audit_staleness_key, audit_target_key,
    context_target_kind_and_label, count_active_annotations, filter_annotations,
    next_annotation_tags, plan_annotation_content_source, plan_context_rm,
    suggestion_tier_human_label, suggestion_tier_token, supersede_reuses_original_scope,
    supersede_reuses_original_target,
};
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
pub use gc_plan::{
    GcDryRunPlan, gc_consolidated_mirror_message, gc_dry_run_messages, gc_dry_run_pack_message,
    gc_dry_run_prune_message, gc_pack_message, gc_pinned_redactions_message,
    gc_preserved_redactions_message, gc_prune_loose_message, gc_pruned_git_mapping_message,
    gc_status_token, plan_gc_dry_run,
};
pub use harness_json::{
    VerificationClaimPolicyFacts, VerificationClaimTrustFacts, first_value_string, map_from_pairs,
    merge_string_vec, opencode_tool_name, opencode_tool_status, parse_relay_payload,
    raw_git_preservation_command, repository_verification_allows_success_claim, value_array_join,
    value_cost_micros, value_cost_micros_u64, value_string, value_string_array, value_u64,
    value_u64_string,
};
pub use harness_policy::{
    ExplicitAgentBind, HarnessFingerprint, HarnessKind, HarnessProbeDecision, SegmentRotation,
    SessionAttachDecision, SessionAttachFacts, SessionAttachRule, SessionLookupFact, SessionPolicy,
    TokenSidFact, WorktreeSessionFact, decide_harness_probe, decide_session_attach,
    detect_harness_kind, fingerprint_harness_from_hints, segment_rotation_policy,
    should_rotate_segment,
};
pub use hook_plan::{
    HookInstallSourceKind, HookInstallSourcePlan, hook_install_empty_stdin_kind,
    hook_install_source_required_kind, hook_unknown_kind, plan_hook_install_source,
};
pub use init_plan::{
    InitPrincipalPlan, SET_PRINCIPAL_COMMAND, init_recommended_action, init_side_effects,
    principal_is_unconfigured, resolve_absolute_path, select_init_principal,
};
pub use log_plan::{
    ReflogLine, extract_scope_bytes, fit_author, format_missing_blobs_suffix, parse_reflog_line,
    session_list_status, short_oid, summarize_context_line, summarize_paths,
    timeline_branch_reason, timeline_cursor_reason, timeline_label, timeline_recovery_status,
    timeline_tool_status, truncate_with_ellipsis, yes_no,
};
pub use marker_plan::{
    MarkerDeleteSelector, MarkerDeleteSelectorError, marker_bulk_delete_message,
    marker_create_message, marker_delete_message, marker_list_filter_matches,
    marker_prefix_is_valid, plan_marker_delete_selector,
};
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
pub use onboarding::{
    OnboardingAction, OnboardingFacts, OnboardingMode, OnboardingPlan, OnboardingRepositoryState,
    plan_repository_onboarding,
};
pub use oplog_plan::{
    OPLOG_RECOVER_DEFAULT_STRATEGY, OplogRecoverFacts, OplogRecoverStatus,
    oplog_recover_damaged_bytes, oplog_recover_damaged_range_display, oplog_recover_detail_fields,
    oplog_recover_entries_lost_display, oplog_recover_headline, oplog_recover_headline_from_facts,
    oplog_recover_shows_detail, oplog_recover_shows_strategy_field, plan_oplog_recover,
    plan_oplog_recover_status,
};
// prove_plan timestamp helpers intentionally not re-exported at crate root (collide with
// approval_plan::timestamp_secs_u64 / format_unix_secs_label). Use heddle_core::prove_plan::*.
pub use prove_plan::{
    HostRepoPlanError, ProofStatusKind, proof_status_label, proof_submit_followup,
    require_host_repo,
};
pub use purge_plan::{PurgeApplyPlan, plan_purge_apply, purge_apply_message, purge_force_command};
pub use query::{QueryHit, QueryReport, QueryRequest, query};
pub use rebase_plan::{
    RebaseContinuePlan, RebaseStartFacts, RebaseStartPlan, no_rebase_in_progress_kind,
    plan_rebase_abort, plan_rebase_continue, plan_rebase_start, rebase_target_not_found_kind,
    rebase_target_required_kind,
};
pub use redact_plan::{RedactionSignatureStatus, redaction_signature_status, short_public_key};
pub use remote::{
    ALL_THREADS_MIRROR_COVERS_NOTE, COMMITS_SEEN_SCOPE, FORCE_DISCARD_WARNING, GIT_NOTES_REF,
    GIT_NOTES_VISIBILITY_WARNING, GitConfigContext, GitOverlayPushTracking, GitRemoteConfigured,
    GitUpstreamConfigured, HostedPullResult, HostedPullResultFields, HostedPushPlan,
    HostedPushResult, HostedPushResultFields, IncludedGitRemoteConfigError, LocalTransferSummary,
    MultiRefPushProgress, PullExecutionFacts, PullFailure, PullOutcome, PullOutcomeText, PullPlan,
    PullPlanRequest, PushExecutionFacts, PushFailure, PushOutcome, PushOutcomeText, PushPath,
    PushPlan, PushPlanRequest, RemoteInfo, RemoteListReport, RemotePreflightBlocker,
    UNKNOWN_TRANSPORT_ERROR, all_threads_mirror_coverage_note, all_threads_uses_single_mirror_push,
    build_pull_outcome, build_push_outcome, default_pull_thread_name, default_push_thread_name,
    first_multi_thread_push_failure, format_connected_to, format_mirror_failure_text,
    format_mirror_success_text, format_multi_ref_push_progress, format_multi_thread_refs_detail,
    format_pull_outcome_text, format_pulling_from, format_push_outcome_text, format_pushing_to,
    format_ref_list, format_remote_state_detail, git_overlay_current_thread_push_ok,
    git_overlay_pull_execution_facts, git_overlay_push_execution_facts,
    git_overlay_push_scope_description, git_overlay_ref_scope, git_overlay_thread_mismatch_blocker,
    heddle_pull_execution_facts, heddle_pull_execution_facts_from_hosted,
    heddle_pull_execution_facts_from_local, heddle_single_push_execution_facts,
    heddle_single_push_execution_facts_from_hosted, heddle_single_push_execution_facts_from_local,
    hosted_path_contains_internal_user_namespace, hosted_spool_display_path,
    is_native_transport_mismatch, list_plain_git_remotes, list_remotes, local_pull_changed,
    looks_like_git_remote_url, looks_like_remote_location, merged_remote_items,
    message_indicates_already_exists, multi_ref_progress_from_hosted_thread, multi_ref_push_begin,
    multi_ref_thread_failed, multi_ref_thread_succeeded_hosted, multi_ref_thread_succeeded_local,
    multi_thread_failed_names, multi_thread_push_execution_facts, multi_thread_reported_refs,
    named_thread_tip_mismatch_failure, parse_hosted_pull_result, parse_hosted_push_result,
    plain_git_remote_items, plan_hosted_push, plan_pull, plan_push, pull_requires_clean_worktree,
    pull_should_materialize, pull_status, pull_tip_changed, pull_will_materialize,
    push_scope_label, push_status, redact_internal_hosted_paths, refuse_named_thread_tip_overwrite,
    remote_advice_kind, remote_missing_blocker, remote_pull_failure, remote_push_failure,
    remote_urls_match, resolve_default_remote_name, resolved_default_remote_name,
    show_plain_git_remote, show_remote, summarize_pull_outcome, summarize_push_outcome,
    transport_error_message, transport_mismatch_blocker, uses_git_overlay_mirror_rpc,
    uses_local_git_overlay_transport,
};
pub use resolve_plan::{
    ResolveSideSelection, contains_line_start_conflict_markers, path_is_active_conflict,
    plan_resolve_side, resolve_requires_marker_check, unresolved_conflict_paths,
};
pub use revert_plan::{
    RevertMessageMode, RevertOutcome, RevertPlan, RevertSuccessFacts,
    default_revert_commit_message, no_changes_to_revert_kind, no_changes_to_revert_summary,
    plan_revert, revert_has_no_changes, revert_inspect_command, revert_success_message,
};
pub use save::{
    CommitGitIndexPlan, GitScope, SavePlan, SaveReport, SaveVerb, commit_next_action_from_trust,
    commit_scope_text, execute_save, plan_commit_git_index, plan_commit_git_index_only,
    plan_creates_new_state, plan_git_scope, plan_writes_git_checkpoint, split_git_extra_paths,
    staged_commit_summary, tree_leaf_name,
};
pub use semantic_plan::{
    HOT_EVENT_KIND_TOKENS, HotEventKindToken, hot_event_kind_label, human_event_kind,
    parse_hot_event_kind_token,
};
#[cfg(feature = "semantic")]
pub use semantic_plan::{hot_event_kind_token, human_hot_event_kind, map_hot_event_kind};
pub use stash_plan::{
    STASH_DEFAULT_LIST_MESSAGE, StashEntryOpPlan, StashMessageMode, StashMutationReport,
    StashOutcomeStatus, StashPushPlan, StashShowBuckets, StashShowChangeKind,
    bucket_stash_show_changes, format_stash_list_line, plan_stash_entry_op, plan_stash_push,
    stash_entry_op_should_refuse, stash_list_entry_message, stash_list_is_empty,
    stash_mutation_message, stash_push_should_refuse, stash_show_change_prefix,
    stash_show_is_empty, stash_stack_is_empty,
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
    health_severity, human_thread_health, large_capture_requires_force, plain_git_status_report,
    principal_is_default_unknown, principal_lacks_accountable_identity,
    resolve_coordination_with_trust, status, status_combined_verdict,
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
pub use thread_materialize::{
    ADVISORY_ACTIVE_HEAVY_THREAD_THRESHOLD, CheckoutCopyPolicy, CheckoutPathPlan,
    CheckoutRewindPlan, CreateDirAttempt, MaterializeStep, RelativePathNormalizeError,
    SelfCreatedDirRewindPlan, SharedTargetRedirectDecision, StartCleanupStep, StartEffectKind,
    StartEffectPreconditionError, StartEffectStagingFacts, StartTransactionPlan,
    TargetDirClaimKind, TargetDirCreateIntent, TargetLeafRefusal, TargetLeafShape,
    ThreadMaterializePlan, ThreadsRootPathClass, ThreadsRootPathSafetyError,
    append_safe_relative_components, claim_kind_after_empty_dir_adoption,
    claim_kind_for_create_attempt, classify_materialize_error, classify_path_vs_threads_root,
    classify_target_leaf_shape, effect_requires_established_claim, mode_is_bytes_on_disk,
    path_components_are_safe, path_is_nested_in_reserved_region, path_is_strict_descendant,
    path_is_under_or_equal, plan_checkout_copy_policy, plan_checkout_path, plan_checkout_rewind,
    plan_hydrate, plan_materialize_steps, plan_self_created_dir_rewind,
    plan_shared_target_redirect, plan_start_cleanup, plan_start_transaction,
    plan_target_dir_create_intent, plan_thread_materialize, plan_write_manifest,
    require_established_claim, shared_target_redirect_applies, shared_target_workspace_is_busy,
    should_advise_shared_target, should_warn_materialized_without_reflink,
    threads_root_path_layout_allowed, validate_empty_dir_adoption,
    validate_start_effect_preconditions, validate_threads_root_path_safety,
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
pub use timeline_plan::{
    TimelinePlanError, TimelineSelection, TimelineTargetOptions, parse_branch_reason,
    parse_materialize_mode, parse_tool_status, plan_timeline_target,
    timeline_materialization_recovery_status, timeline_materialize_status,
};
pub use undo::{
    LiveThreadWorktree, PurgeOpRef, RedactOpRef, RedactionUndoBatchFacts, RequiredStateRef,
    ThreadWorktreeHazard, UndoApplyPlan, UndoApplyPreflightError, UndoApplyStep, UndoBatchSummary,
    UndoHistoryAction, UndoListReport, UndoOperationSummary, UndoPlan, UnsupportedRedoOp,
    batch_status, check_redaction_redo_supported, check_redaction_undo_safe,
    check_states_reachable, check_thread_worktree_undo_safe, collect_redaction_undo_facts,
    collect_redo_required_states, collect_thread_worktree_hazards, collect_undo_required_states,
    collect_unsupported_redo_ops, empty_history_refusal, human_operation_description,
    human_post_undo_trust_status, human_undo_redo_message, list_undo_history,
    list_undo_history_ctx, live_materialized_path_blocks_undo, machine_undo_redo_message,
    plan_redo_apply_steps, plan_redo_batches, plan_undo_apply, plan_undo_apply_steps,
    plan_undo_batches, require_nonempty_history, summarize_batch, undo_mode_conflict,
    validate_undo_list_preview_modes,
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
pub use visibility_plan::{visibility_tier_kind, visibility_tier_label};
pub use workflow::{
    AUTO_LAND_CONFIDENCE_RECOVERY_ACTION, AUTO_LAND_CONFIDENCE_THRESHOLD, AutoLandPolicyInput,
    ReadyDecision, ReadyDecisionInput, auto_land_confidence_recovery_action,
    auto_land_policy_blockers, classify_ready_decision, has_integration_target,
    integrated_land_next_action, integration_blocker_recommended_action, integration_blockers,
    is_heavy_impact_advisory, is_integration_clear, is_manual_review_blocker,
    land_blockers_for_preview, land_checkpoint_message, land_performed_steps, land_skipped_steps,
    land_text_step, land_warnings_for_preview, non_staleness_blockers, op_targets_merge_state,
    quote_recommended_action_arg, ready_freshness_summary, ready_integration_summary,
    ready_merge_type_label, ready_merge_type_summary, ready_report_recommended_action,
    ready_scoped_next_action, ready_status_summary, ready_verification_preflight_blocks,
    ready_verification_status_blocks, recovery_scope_checkout, scope_action_to_repo,
    should_squash_land, state_id_matches_display, state_id_matches_op_display,
};
