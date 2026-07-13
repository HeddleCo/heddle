// SPDX-License-Identifier: Apache-2.0
//! Ready / land workflow domain: pure preflight, policy, and step accounting.
//!
//! Owns decision logic shared by `heddle ready`, `heddle land`, and `heddle sync`:
//! - verification fail-closed preflight for readiness
//! - land push option planning
//! - auto-land confidence / verification policy blockers
//! - non-staleness / heavy-impact classification
//! - land performed / skipped step accounting
//! - integrated-land next-action selection
//! - ready classification and report next-action filtering
//!
//! Network, mutation, capture, and render remain CLI-owned.

use std::path::{Path, PathBuf};

use objects::object::StateId;
use oplog::OpRecord;
use repo::{GitImportGuidance, GitRemoteTrackingStatus, RepositoryOperationStatus, shell_quote};

use crate::{
    RepositoryVerificationState, ThreadPreviewReport,
    status::next_action::{NextActionInput, effective_next_action, non_empty_action},
};

/// Minimum agent confidence allowed for automatic land without re-capture.
pub const AUTO_LAND_CONFIDENCE_THRESHOLD: f32 = 0.75;

/// Recovery breadcrumb when auto-land policy blocks on confidence / tests.
pub const AUTO_LAND_CONFIDENCE_RECOVERY_ACTION: &str =
    "heddle capture -m \"...\" --confidence <confidence>";

// ---------------------------------------------------------------------------
// Ready verification preflight
// ---------------------------------------------------------------------------

/// Statuses that must fail closed before readiness / land preflight can run.
pub fn ready_verification_preflight_blocks(trust: &RepositoryVerificationState) -> bool {
    ready_verification_status_blocks(trust.status.as_str())
}

/// Pure status-string check used by [`ready_verification_preflight_blocks`].
pub fn ready_verification_status_blocks(status: &str) -> bool {
    matches!(
        status,
        "needs_init" | "needs_import" | "needs_reconcile" | "git_branch_advanced"
    )
}

// ---------------------------------------------------------------------------
// Ready classification
// ---------------------------------------------------------------------------

/// Whether a thread preview has an integration target configured.
pub fn has_integration_target(merge_relation: &str) -> bool {
    merge_relation != "no_target"
}

/// Conflict-free and policy-clear: safe to mark ready / land.
pub fn is_integration_clear(conflict_count: usize, blockers: &[String]) -> bool {
    conflict_count == 0 && blockers.is_empty()
}

/// Inputs for classifying a ready-command outcome without performing I/O.
#[derive(Debug, Clone, Copy)]
pub struct ReadyDecisionInput<'a> {
    pub merge_relation: &'a str,
    pub captured: bool,
    /// Whether the thread is already in [`repo::ThreadState::Ready`].
    pub thread_already_ready: bool,
    pub conflict_count: usize,
    pub blockers: &'a [String],
}

/// Pure classification of readiness after preview / policy blockers are known.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReadyDecision {
    pub has_integration_target: bool,
    /// Thread was already ready and no capture ran this invocation.
    pub already_ready: bool,
    /// Clean thread with no integration target configured.
    pub ready_without_target: bool,
    /// Conflict-free and no blockers (would mark Ready when a target exists).
    pub integration_clear: bool,
    /// Operator envelope should report `completed` (ready or no-target clean).
    pub operator_completed: bool,
}

/// Classify ready outcome from preview facts (no I/O).
pub fn classify_ready_decision(input: ReadyDecisionInput<'_>) -> ReadyDecision {
    let has_target = has_integration_target(input.merge_relation);
    let clear = is_integration_clear(input.conflict_count, input.blockers);
    let already_ready = has_target && !input.captured && input.thread_already_ready && clear;
    let ready_without_target = !has_target && clear;
    ReadyDecision {
        has_integration_target: has_target,
        already_ready,
        ready_without_target,
        integration_clear: clear,
        // Matches CLI: completed when no target is configured, or when the
        // thread is (or would be) Ready after this invocation.
        operator_completed: !has_target || clear,
    }
}

/// Drop self-merge / land recommendations when the thread has no target.
pub fn ready_report_recommended_action(
    merge_relation: &str,
    recommended_action: &str,
) -> Option<String> {
    if merge_relation == "no_target" {
        return None;
    }
    non_empty_action(Some(recommended_action)).map(str::to_string)
}

/// Ready-scoped next-action selection (operation → thread fallback → publish).
pub fn ready_scoped_next_action(
    operation: Option<&RepositoryOperationStatus>,
    remote_tracking: Option<&GitRemoteTrackingStatus>,
    import_hint: Option<&GitImportGuidance>,
    thread_action: Option<&str>,
) -> String {
    effective_next_action(
        NextActionInput::default(operation, remote_tracking, import_hint, thread_action).ready(),
    )
}

/// Whether land should squash the thread into one Git commit on write-through.
pub fn should_squash_land(no_squash: bool, config_squash: bool) -> bool {
    !no_squash && config_squash
}

// ---------------------------------------------------------------------------
// Auto-land policy
// ---------------------------------------------------------------------------

/// Facts needed for auto-land policy without opening the object store.
#[derive(Debug, Clone, Copy)]
pub struct AutoLandPolicyInput {
    pub agent_authored: bool,
    pub confidence: Option<f32>,
    pub tests_passed: Option<bool>,
}

/// Policy blockers that prevent automatic land (confidence / verification).
pub fn auto_land_policy_blockers(input: AutoLandPolicyInput) -> Vec<String> {
    let mut blockers = Vec::new();
    if input.agent_authored
        && let Some(confidence) = input.confidence
        && confidence < AUTO_LAND_CONFIDENCE_THRESHOLD
    {
        blockers.push(format!(
            "confidence {:.2} is below the auto-land threshold of {AUTO_LAND_CONFIDENCE_THRESHOLD:.2}",
            confidence
        ));
    }
    if matches!(input.tests_passed, Some(false)) {
        blockers.push("verification summary reports failing tests".to_string());
    }
    blockers
}

/// Combine preview blockers with auto-land policy, honoring manual resolution.
pub fn integration_blockers(
    manual_resolution_current: bool,
    preview_blockers: &[String],
    policy: AutoLandPolicyInput,
) -> Vec<String> {
    let mut blockers = if manual_resolution_current {
        Vec::new()
    } else {
        non_staleness_blockers(preview_blockers)
    };
    blockers.extend(auto_land_policy_blockers(policy));
    blockers
}

/// Recovery breadcrumb for confidence / verification policy blockers.
pub fn integration_blocker_recommended_action(
    blockers: &[String],
    scope_to_checkout: Option<&Path>,
) -> Option<String> {
    blockers
        .iter()
        .any(|blocker| {
            blocker.starts_with("confidence ")
                || blocker == "verification summary reports failing tests"
        })
        .then(|| auto_land_confidence_recovery_action(scope_to_checkout))
}

/// Scope the confidence recovery capture to the thread's checkout when needed.
pub fn auto_land_confidence_recovery_action(scope_to_checkout: Option<&Path>) -> String {
    match scope_to_checkout {
        Some(path) => format!(
            "heddle --repo {} {}",
            shell_quote(&path.display().to_string()),
            AUTO_LAND_CONFIDENCE_RECOVERY_ACTION
                .strip_prefix("heddle ")
                .expect("recovery action is a heddle command"),
        ),
        None => AUTO_LAND_CONFIDENCE_RECOVERY_ACTION.to_string(),
    }
}

/// Returns the thread checkout when it is a real, distinct path from the
/// current checkout (so recovery breadcrumbs must re-scope via `--repo`).
pub fn recovery_scope_checkout(execution_path: &Path, current_checkout: &Path) -> Option<PathBuf> {
    if execution_path.as_os_str().is_empty() {
        return None;
    }
    let canonical = |path: &Path| path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    (canonical(execution_path) != canonical(current_checkout)).then(|| execution_path.to_path_buf())
}

// ---------------------------------------------------------------------------
// Blocker classification / land preview surface
// ---------------------------------------------------------------------------

/// Heavy-impact lines are advisories for land, not hard blockers for sync.
pub fn is_heavy_impact_advisory(blocker: &str) -> bool {
    blocker.to_lowercase().contains("heavy-impact change")
}

/// Drop staleness and heavy-impact advisories from a blocker list.
pub fn non_staleness_blockers(blockers: &[String]) -> Vec<String> {
    blockers
        .iter()
        .filter(|blocker| {
            !blocker.contains(" is stale against ") && !is_heavy_impact_advisory(blocker)
        })
        .cloned()
        .collect()
}

/// Expand preview conflicts into land blockers, then sort/dedup.
pub fn land_blockers_for_preview(
    preview: &ThreadPreviewReport,
    blockers: &[String],
) -> Vec<String> {
    let mut out = blockers.to_vec();
    if preview.conflict_count > 0 {
        out.push(format!(
            "{} path conflict(s) need manual resolution",
            preview.conflict_count
        ));
        out.extend(
            preview
                .conflicts
                .iter()
                .map(|path| format!("conflict: {path}")),
        );
    }
    out.sort();
    out.dedup();
    out
}

/// Heavy-impact advisories for land (warnings, not hard blockers).
pub fn land_warnings_for_preview(preview: &ThreadPreviewReport) -> Vec<String> {
    let mut warnings = preview
        .blockers
        .iter()
        .filter(|blocker| is_heavy_impact_advisory(blocker))
        .cloned()
        .collect::<Vec<_>>();
    if warnings.is_empty() && !preview.heavy_impact_paths.is_empty() {
        warnings.push(format!(
            "Heavy-impact change: {} — review broader impact before merging",
            preview.heavy_impact_paths.join(", ")
        ));
    }
    warnings.sort();
    warnings.dedup();
    warnings
}

// ---------------------------------------------------------------------------
// Land step accounting + post-integrate next action
// ---------------------------------------------------------------------------

/// Whether a land/preview blocker is the heavy-impact manual-review advisory.
pub fn is_manual_review_blocker(blocker: &str) -> bool {
    blocker.starts_with("Heavy-impact change:")
}

/// Human text for a land performed/skipped step token.
pub fn land_text_step(step: &str) -> String {
    match step {
        "capture" => "saved".to_string(),
        "sync" => "refreshed".to_string(),
        "merge" => "merged".to_string(),
        "checkpoint" => "committed".to_string(),
        "capture(no changes)" => "no unsaved changes".to_string(),
        "sync(current)" => "already refreshed".to_string(),
        "merge(blocked)" => "merge blocked".to_string(),
        "checkpoint(not needed)" => "no Git commit needed".to_string(),
        "checkpoint(not reached)" => "Git commit not reached".to_string(),
        other => other.to_string(),
    }
}

/// Scope a `heddle …` recommended action to an explicit `--repo` path.
pub fn scope_action_to_repo(action: &str, repo_path: &str) -> String {
    let Some(rest) = action.strip_prefix("heddle ") else {
        return action.to_string();
    };
    if rest.starts_with("--repo ") || rest.starts_with("-R ") {
        return action.to_string();
    }
    format!(
        "heddle --repo {} {rest}",
        quote_recommended_action_arg(repo_path)
    )
}

/// Quote a recommended-action path/arg when it is not shell-safe bare.
pub fn quote_recommended_action_arg(value: &str) -> String {
    if !value.is_empty()
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'/' | b'.' | b'_' | b'-' | b'+'))
    {
        value.to_string()
    } else {
        format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
    }
}

/// Ready summary labels from preview merge relation strings.
pub fn ready_merge_type_label(result: &str) -> String {
    match result {
        "fast_forward" => "fast-forward".to_string(),
        "already_integrated" => "already integrated".to_string(),
        "no_target" => "none configured".to_string(),
        other => other.replace('_', " "),
    }
}

/// Ready status line for operator text (`clean` vs thread health).
pub fn ready_status_summary(
    merge_relation: &str,
    blockers_empty: bool,
    thread_health: &str,
) -> String {
    if merge_relation == "no_target" && blockers_empty {
        "clean".to_string()
    } else {
        thread_health.replace('_', " ")
    }
}

/// Integration column for ready summary.
pub fn ready_integration_summary(merge_relation: &str) -> String {
    match merge_relation {
        "no_target" => "n/a (no integration target configured)".to_string(),
        "not_checked" => "not checked (readiness checks did not run)".to_string(),
        "blocked" => "not checked (repository verification is blocked)".to_string(),
        _ => "configured".to_string(),
    }
}

/// Freshness column for ready summary.
pub fn ready_freshness_summary(merge_relation: &str, freshness: &str) -> String {
    match merge_relation {
        "no_target" => "n/a (no integration target configured)".to_string(),
        "not_checked" => "not checked (readiness checks did not run)".to_string(),
        "blocked" => "not checked (repository verification is blocked)".to_string(),
        _ => freshness.replace('_', " "),
    }
}

/// Merge-type column for ready summary.
pub fn ready_merge_type_summary(merge_relation: &str) -> String {
    match merge_relation {
        "no_target" => "n/a (no integration target configured)".to_string(),
        "not_checked" => "not checked (readiness checks did not run)".to_string(),
        "blocked" => "not checked (repository verification is blocked)".to_string(),
        other => ready_merge_type_label(other),
    }
}

/// Steps that actually ran during land.
pub fn land_performed_steps(
    captured: bool,
    synced: bool,
    integrated: bool,
    checkpointed: bool,
) -> Vec<String> {
    [
        (captured, "capture"),
        (synced, "sync"),
        (integrated, "merge"),
        (checkpointed, "checkpoint"),
    ]
    .into_iter()
    .filter(|&(done, _step)| done)
    .map(|(_done, step)| step.to_string())
    .collect()
}

/// Steps skipped (with reason tokens) during land.
pub fn land_skipped_steps(
    captured: bool,
    synced: bool,
    integrated: bool,
    checkpointed: bool,
) -> Vec<String> {
    [
        (!captured, "capture(no changes)"),
        (!synced, "sync(current)"),
        (!integrated, "merge(blocked)"),
        (!checkpointed && integrated, "checkpoint(not needed)"),
        (!checkpointed && !integrated, "checkpoint(not reached)"),
    ]
    .into_iter()
    .filter(|&(skipped, _step)| skipped)
    .map(|(_skipped, step)| step.to_string())
    .collect()
}

/// Next action after a successful local land (push if trust says so, else cleanup).
pub fn integrated_land_next_action(
    integrated: bool,
    trust_recommended_action: &str,
) -> Option<String> {
    if !integrated {
        return None;
    }
    if matches!(trust_recommended_action, "heddle push" | "git push") {
        Some(trust_recommended_action.to_string())
    } else {
        Some("heddle thread cleanup --merged --dry-run".to_string())
    }
}

/// Checkpoint / squash message for land write-through.
///
/// Precedence: explicit non-empty message → land subject when preferred →
/// current intent → task → `Land <thread_id>`.
pub fn land_checkpoint_message(
    explicit: Option<&str>,
    prefer_land_subject: bool,
    thread_id: &str,
    intent: Option<&str>,
    task: Option<&str>,
) -> String {
    if let Some(message) = explicit.filter(|message| !message.trim().is_empty()) {
        return message.to_string();
    }
    if prefer_land_subject {
        return format!("Land {thread_id}");
    }
    if let Some(intent) = intent.filter(|intent| !intent.trim().is_empty()) {
        return intent.to_string();
    }
    if let Some(task) = task.filter(|task| !task.trim().is_empty()) {
        return task.to_string();
    }
    format!("Land {thread_id}")
}

/// Whether a change id matches a short or full display form from operator text.
pub fn state_id_matches_display(short: &str, full: &str, display: &str) -> bool {
    short == display || full == display
}

/// Whether an oplog record advances HEAD/thread to a land merge target.
pub fn op_targets_merge_state(op: &OpRecord, merge_state: &str) -> bool {
    match op {
        OpRecord::Snapshot { new_state, .. } => {
            state_id_matches_display(&new_state.short(), &new_state.to_string_full(), merge_state)
        }
        OpRecord::Checkpoint { state, .. } => {
            state_id_matches_display(&state.short(), &state.to_string_full(), merge_state)
        }
        OpRecord::Goto { target, .. } => {
            state_id_matches_display(&target.short(), &target.to_string_full(), merge_state)
        }
        OpRecord::FastForward { post_target_id, .. } => state_id_matches_display(
            &post_target_id.short(),
            &post_target_id.to_string_full(),
            merge_state,
        ),
        // Enumerated explicitly (no wildcard) so a new state-advancing variant
        // must be considered here (heddle#354 r9).
        OpRecord::ThreadCreate { .. }
        | OpRecord::ThreadDelete { .. }
        | OpRecord::ThreadUpdate { .. }
        | OpRecord::Fork { .. }
        | OpRecord::Collapse { .. }
        | OpRecord::MarkerCreate { .. }
        | OpRecord::MarkerDelete { .. }
        | OpRecord::TransactionAbort { .. }
        | OpRecord::EphemeralThreadCollapse { .. }
        | OpRecord::ConflictResolved { .. }
        | OpRecord::TransactionCommit { .. }
        | OpRecord::Redact { .. }
        | OpRecord::Purge { .. }
        | OpRecord::GitCheckpoint { .. }
        | OpRecord::RemoteThreadUpdate { .. }
        | OpRecord::RemoteThreadDelete { .. }
        | OpRecord::UndoRecoveryUpdate { .. }
        | OpRecord::StateVisibilitySet { .. }
        | OpRecord::StateVisibilityPromote { .. } => false,
    }
}

/// Convenience: match a [`StateId`] against operator display text.
pub fn state_id_matches_op_display(id: &StateId, display: &str) -> bool {
    state_id_matches_display(&id.short(), &id.to_string_full(), display)
}

#[cfg(test)]
mod tests {
    use repo::{OperationKind, OperationScope};

    use super::*;
    use crate::status::next_action as core_next_action;

    fn bare_trust(status: &str) -> RepositoryVerificationState {
        RepositoryVerificationState {
            verified: false,
            status: status.to_string(),
            repository_mode: "native".to_string(),
            heddle_initialized: true,
            git_branch: None,
            heddle_thread: None,
            worktree_dirty: false,
            worktree_state: "clean".to_string(),
            import_state: "ok".to_string(),
            mapping_state: "ok".to_string(),
            remote_drift: "none".to_string(),
            active_operation: None,
            default_remote: None,
            clone_verification: "not_applicable".to_string(),
            machine_contract: "not_checked".to_string(),
            machine_contract_coverage: crate::MachineContractCoverage::not_checked(),
            workflow_status: "idle".to_string(),
            workflow_summary: String::new(),
            summary: status.to_string(),
            recommended_action: "heddle verify".to_string(),
            recommended_action_template: None,
            recovery_commands: Vec::new(),
            recovery_action_templates: Vec::new(),
            checks: Vec::new(),
        }
    }

    fn preview(merge_relation: &str) -> ThreadPreviewReport {
        ThreadPreviewReport {
            thread: "feature".to_string(),
            thread_mode: "solid".to_string(),
            thread_state: "ready".to_string(),
            freshness: "current".to_string(),
            task: None,
            changed_paths: Vec::new(),
            changed_path_count: 0,
            impact_categories: Vec::new(),
            heavy_impact_paths: Vec::new(),
            merge_relation: merge_relation.to_string(),
            conflicts: Vec::new(),
            conflict_count: 0,
            blockers: Vec::new(),
            recommended_action: "heddle land --thread feature".to_string(),
            recommended_action_template: None,
            thread_health: "ready".to_string(),
        }
    }

    #[test]
    fn ready_preflight_blocks_setup_and_mapping_statuses() {
        for status in [
            "needs_init",
            "needs_import",
            "needs_reconcile",
            "git_branch_advanced",
        ] {
            assert!(
                ready_verification_preflight_blocks(&bare_trust(status)),
                "{status} should block ready preflight"
            );
        }
        assert!(!ready_verification_preflight_blocks(&bare_trust("clean")));
        assert!(!ready_verification_preflight_blocks(&bare_trust(
            "dirty_worktree"
        )));
    }

    #[test]
    fn ready_decision_classifies_already_ready_and_no_target() {
        let clear = classify_ready_decision(ReadyDecisionInput {
            merge_relation: "fast_forward",
            captured: false,
            thread_already_ready: true,
            conflict_count: 0,
            blockers: &[],
        });
        assert!(clear.already_ready);
        assert!(clear.integration_clear);
        assert!(clear.operator_completed);

        let no_target = classify_ready_decision(ReadyDecisionInput {
            merge_relation: "no_target",
            captured: false,
            thread_already_ready: false,
            conflict_count: 0,
            blockers: &[],
        });
        assert!(no_target.ready_without_target);
        assert!(!no_target.has_integration_target);
        assert!(no_target.operator_completed);

        let blocked = classify_ready_decision(ReadyDecisionInput {
            merge_relation: "conflicted",
            captured: false,
            thread_already_ready: false,
            conflict_count: 1,
            blockers: &["conflict".to_string()],
        });
        assert!(!blocked.integration_clear);
        assert!(!blocked.operator_completed);
    }

    #[test]
    fn ready_suppresses_action_without_target() {
        assert_eq!(
            ready_report_recommended_action("no_target", "heddle merge main --preview"),
            None
        );
        assert_eq!(
            ready_report_recommended_action("fast_forward", "heddle land --thread feature"),
            Some("heddle land --thread feature".to_string())
        );
    }

    #[test]
    fn ready_scoped_next_action_matches_core_matrix() {
        let operation = RepositoryOperationStatus {
            scope: OperationScope::Heddle,
            kind: OperationKind::Merge,
            in_progress: true,
            state: "in_progress".to_string(),
            message: "merge in progress".to_string(),
            next_action: "heddle continue".to_string(),
        };
        let remote_ahead = GitRemoteTrackingStatus {
            branch: "feature".to_string(),
            upstream: "origin/feature".to_string(),
            ahead: 1,
            behind: 0,
            local_oid: Some("local".to_string()),
            upstream_oid: Some("upstream".to_string()),
            upstream_is_undone_checkpoint: false,
            message: String::new(),
            next_action: String::new(),
        };
        let fallback = Some("heddle land --thread feature");
        let scoped = ready_scoped_next_action(Some(&operation), None, None, fallback);
        let core = core_next_action::effective_next_action(
            core_next_action::NextActionInput::default(Some(&operation), None, None, fallback)
                .ready(),
        );
        assert_eq!(scoped, core);
        assert_eq!(scoped, "heddle continue");

        let publish = ready_scoped_next_action(None, Some(&remote_ahead), None, None);
        assert_eq!(
            publish,
            core_next_action::effective_next_action(
                core_next_action::NextActionInput::default(None, Some(&remote_ahead), None, None,)
                    .ready(),
            )
        );
    }

    fn auto_land_policy_blocks_low_confidence_and_failing_tests() {
        let blockers = auto_land_policy_blockers(AutoLandPolicyInput {
            agent_authored: true,
            confidence: Some(0.40),
            tests_passed: Some(false),
        });
        assert_eq!(
            blockers,
            vec![
                "confidence 0.40 is below the auto-land threshold of 0.75".to_string(),
                "verification summary reports failing tests".to_string(),
            ]
        );
        assert!(
            auto_land_policy_blockers(AutoLandPolicyInput {
                agent_authored: false,
                confidence: Some(0.10),
                tests_passed: Some(true),
            })
            .is_empty()
        );
    }

    #[test]
    fn confidence_blocker_recovery_scopes_to_thread_checkout() {
        let blockers = vec!["confidence 0.40 is below the auto-land threshold of 0.75".to_string()];
        let action = integration_blocker_recommended_action(
            &blockers,
            Some(Path::new("/work/threads/agent-thread")),
        )
        .expect("confidence blocker must yield recovery");
        assert_eq!(
            action,
            "heddle --repo /work/threads/agent-thread commit -m \"...\" --confidence <confidence>"
        );

        let unscoped =
            integration_blocker_recommended_action(&blockers, None).expect("unscoped recovery");
        assert_eq!(unscoped, AUTO_LAND_CONFIDENCE_RECOVERY_ACTION);

        assert!(
            integration_blocker_recommended_action(
                &["3 path conflict(s) need manual resolution".to_string()],
                None
            )
            .is_none()
        );
    }

    #[test]
    fn non_staleness_drops_stale_and_heavy_impact() {
        let blockers = vec![
            "Thread 'agent-thread' is stale against 'main'".to_string(),
            "Heavy-impact change: crates/wire/src/lib.rs — review broader impact before merging"
                .to_string(),
            "confidence 0.40 is below the auto-land threshold of 0.75".to_string(),
        ];
        assert_eq!(
            non_staleness_blockers(&blockers),
            vec!["confidence 0.40 is below the auto-land threshold of 0.75".to_string()]
        );
    }

    #[test]
    fn land_warnings_surface_heavy_impact_review() {
        let mut report = preview("would_merge");
        report.heavy_impact_paths = vec!["crates/wire/src/lib.rs".to_string()];
        report.blockers = vec![
            "Heavy-impact change: crates/wire/src/lib.rs — review broader impact before merging"
                .to_string(),
        ];
        assert_eq!(
            land_warnings_for_preview(&report),
            vec![
                "Heavy-impact change: crates/wire/src/lib.rs — review broader impact before merging"
                    .to_string()
            ]
        );
    }

    #[test]
    fn land_step_accounting_and_next_action() {
        assert_eq!(
            land_performed_steps(true, false, true, true),
            vec!["capture", "merge", "checkpoint"]
        );
        assert!(land_skipped_steps(true, true, true, true).is_empty());
        assert_eq!(
            integrated_land_next_action(true, "heddle push"),
            Some("heddle push".to_string())
        );
        assert_eq!(
            integrated_land_next_action(true, "git push"),
            Some("git push".to_string())
        );
        assert_eq!(integrated_land_next_action(false, "heddle push"), None);
    }

    #[test]
    fn recovery_scope_checkout_distinguishes_isolated_from_in_thread() {
        assert_eq!(
            recovery_scope_checkout(
                Path::new("/work/threads/agent-thread"),
                Path::new("/work/parent"),
            ),
            Some(PathBuf::from("/work/threads/agent-thread")),
        );
        assert_eq!(
            recovery_scope_checkout(
                Path::new("/work/threads/agent-thread"),
                Path::new("/work/threads/agent-thread"),
            ),
            None,
        );
        assert_eq!(
            recovery_scope_checkout(Path::new(""), Path::new("/work/parent")),
            None,
        );
    }

    #[test]
    fn should_squash_respects_no_squash_and_config() {
        assert!(should_squash_land(false, true));
        assert!(!should_squash_land(true, true));
        assert!(!should_squash_land(false, false));
    }

    #[test]
    fn land_checkpoint_message_precedence() {
        assert_eq!(
            land_checkpoint_message(Some("explicit"), false, "t", Some("intent"), Some("task")),
            "explicit"
        );
        assert_eq!(
            land_checkpoint_message(Some("  "), true, "t", Some("intent"), None),
            "Land t"
        );
        assert_eq!(
            land_checkpoint_message(None, false, "t", Some("intent"), Some("task")),
            "intent"
        );
        assert_eq!(
            land_checkpoint_message(None, false, "t", None, Some("task")),
            "task"
        );
        assert_eq!(
            land_checkpoint_message(None, false, "t", None, None),
            "Land t"
        );
    }

    #[test]
    fn state_id_matches_short_or_full() {
        assert!(state_id_matches_display("abc", "abcdef", "abc"));
        assert!(state_id_matches_display("abc", "abcdef", "abcdef"));
        assert!(!state_id_matches_display("abc", "abcdef", "zzz"));
    }

    #[test]
    fn land_text_scope_and_manual_review() {
        assert_eq!(land_text_step("capture"), "saved");
        assert_eq!(land_text_step("merge(blocked)"), "merge blocked");
        assert!(is_manual_review_blocker("Heavy-impact change: Cargo.lock"));
        assert!(!is_manual_review_blocker("stale"));
        assert_eq!(
            scope_action_to_repo("heddle land main", "/tmp/repo"),
            "heddle --repo /tmp/repo land main"
        );
        assert_eq!(ready_merge_type_label("fast_forward"), "fast-forward");
        assert_eq!(
            ready_merge_type_label("already_integrated"),
            "already integrated"
        );
        assert_eq!(ready_merge_type_label("no_target"), "none configured");
    }
}
