// SPDX-License-Identifier: Apache-2.0
//! CLI verification adapter: Machine-Contract catalog injection, RecoveryAdvice,
//! and thin wrappers around the single core proof owner.
//!
//! **Do not construct Repository Verification Health/State here.** Proof
//! construction lives in `heddle_core::status` / `heddle_core::verify`. This
//! module injects command-catalog coverage, builds refusal advice, and
//! formats setup guidance for text/render paths.

use std::{collections::BTreeSet, path::Path};

use heddle_core::status::next_action::{
    canonical_git_import_ref_command, canonical_git_repair_ref_preview_command,
    heddle_action as core_heddle_action, import_guidance_includes_active_branch,
    remote_tracking_next_action, remote_tracking_status,
};
pub(crate) use heddle_core::{
    ActionTemplate, MachineContractCoverage, MachineContractInput, PlainGitVerifyProbe,
    RepositoryVerificationCheck, RepositoryVerificationHealth, RepositoryVerificationState,
    verify::serialize_empty_action_as_null,
};
// Re-exported for unit tests in operator/thread_shaping modules.
#[cfg(test)]
pub(crate) use heddle_core::VerificationCheck;
use objects::{object::ThreadName, worktree::WorktreeStatus};
use refs::Head;
use repo::{
    CommitGraphIndex, GitOverlayBranchTip, GitRemoteTrackingStatus, OperationKind, OperationScope,
    Repository,
};

use super::{
    advice::RecoveryAdvice,
    command_catalog::{
        ActionFields, build_command_catalog, heddle_action, recommended_action_template,
    },
    schemas::opaque_schema_verbs,
};

pub(crate) type PlainGitVerificationProbe = PlainGitVerifyProbe;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RepositorySetupActionKind {
    Init,
    Adopt,
    GitImport,
    Other,
}

#[derive(Debug, Clone)]
pub(crate) struct RepositorySetupGuidance {
    pub setup_line: String,
    pub effect: String,
}

pub(crate) fn primary_recovery_command(health: &RepositoryVerificationHealth) -> Option<&str> {
    health.recovery_commands.first().map(String::as_str)
}

pub(crate) fn override_trust_recommended_action(
    trust: &mut RepositoryVerificationState,
    action: impl Into<String>,
) {
    let action = action.into();
    let action_fields = ActionFields::from_action(&action);
    trust.recommended_action_template = action_fields.template.clone();
    trust.recommended_action = action.clone();
    if let Some(check) = trust
        .checks
        .iter_mut()
        .find(|check| check.name == "Workflow")
    {
        check.recommended_action_template = action_fields.template;
        check.recommended_action = Some(action);
    }
}
pub(crate) fn trust_visible_worktree_status(
    repo: &Repository,
    trust: &RepositoryVerificationState,
) -> anyhow::Result<Option<WorktreeStatus>> {
    if matches!(
        trust.status.as_str(),
        "needs_import" | "needs_reconcile" | "git_branch_advanced"
    ) {
        return Ok(Some(
            repo.git_overlay_worktree_status()?.unwrap_or_default(),
        ));
    }
    Ok(None)
}

fn machine_contract_is_clean(coverage: &MachineContractCoverage) -> bool {
    coverage.verified_scope_json_commands_without_schema == 0
        && coverage.verified_scope_mutating_commands_without_schema == 0
        && coverage.verified_scope_json_commands_with_accepted_opaque_schema == 0
        && coverage.verified_scope_mutating_commands_with_accepted_opaque_schema == 0
        && coverage.unaccepted_opaque_schema_verbs_total == 0
}

pub(crate) fn machine_contract_status(coverage: &MachineContractCoverage) -> &'static str {
    if !machine_contract_is_clean(coverage) {
        return "schema_gaps";
    } else if coverage.undocumented_schema_verbs_total > 0 {
        return "available_with_doc_gaps";
    }
    "available"
}

/// CLI adapter: injects command-catalog Machine-Contract Proof into core's
/// single Repository Verification State owner.
pub(crate) fn build_repository_verification_state(
    repo: &Repository,
) -> RepositoryVerificationState {
    match heddle_core::verify::build_repository_verification_state_with_machine_contract(
        repo,
        &MachineContractInput::from_coverage(machine_contract_coverage()),
    ) {
        Ok(state) => state,
        Err(error) => degraded_repository_verification_state(repo, error.to_string()),
    }
}

/// Sub-phase timings for [`build_repository_verification_state_profiled`], in
/// milliseconds. Used by `adopt`'s `HEDDLE_PROFILE=1` instrumentation.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct VerificationProfile {
    pub worktree_status_ms: u128,
    pub health_ms: u128,
    pub from_health_ms: u128,
}

/// Profiled adapter around the core verification owner. Classification matches
/// [`build_repository_verification_state`]; only wall-clock phase splits differ.
pub(crate) fn build_repository_verification_state_profiled(
    repo: &Repository,
) -> (RepositoryVerificationState, VerificationProfile) {
    let total_start = std::time::Instant::now();
    let worktree_status_start = std::time::Instant::now();
    let worktree_status = worktree_status_for_verification(repo);
    let worktree_status_ms = worktree_status_start.elapsed().as_millis();

    let health_start = std::time::Instant::now();
    let health = heddle_core::status::build_repository_verification_health_with_worktree_status(
        repo,
        &worktree_status,
    );
    let health_ms = health_start.elapsed().as_millis();

    let from_health_start = std::time::Instant::now();
    let state = heddle_core::verify::build_repository_verification_state_with_worktree_status_and_machine_contract(
        repo,
        health,
        &worktree_status,
        &MachineContractInput::from_coverage(machine_contract_coverage()),
    );
    let from_health_ms = from_health_start.elapsed().as_millis();
    let _ = total_start;

    (
        state,
        VerificationProfile {
            worktree_status_ms,
            health_ms,
            from_health_ms,
        },
    )
}

/// Core-owned verification state with a reused worktree-status `Result`.
pub(crate) fn build_repository_verification_state_with_worktree_status(
    repo: &Repository,
    worktree_status: &repo::Result<Option<WorktreeStatus>>,
) -> RepositoryVerificationState {
    let health = heddle_core::status::build_repository_verification_health_with_worktree_status(
        repo,
        worktree_status,
    );
    heddle_core::verify::build_repository_verification_state_with_worktree_status_and_machine_contract(
        repo,
        health,
        worktree_status,
        &MachineContractInput::from_coverage(machine_contract_coverage()),
    )
}

/// Core-owned health proof (CLI may inspect recovery commands for diagnose text).
pub(crate) fn build_verification_health(repo: &Repository) -> RepositoryVerificationHealth {
    let worktree_status = worktree_status_for_verification(repo);
    heddle_core::status::build_repository_verification_health_with_worktree_status(
        repo,
        &worktree_status,
    )
}

/// Health proof reusing a caller's worktree-status `Result`.
///
/// Kept as the catalog-injection-free health adapter for hot paths that already
/// hold a worktree status `Result` (mirrors the state-with-worktree adapter).
#[allow(dead_code)]
pub(crate) fn build_verification_health_with_worktree_status(
    repo: &Repository,
    worktree_status: &repo::Result<Option<WorktreeStatus>>,
) -> RepositoryVerificationHealth {
    heddle_core::status::build_repository_verification_health_with_worktree_status(
        repo,
        worktree_status,
    )
}

fn worktree_status_for_verification(
    repo: &Repository,
) -> repo::Result<Option<WorktreeStatus>> {
    if repo.capability() == repo::RepositoryCapability::GitOverlay {
        repo.git_overlay_worktree_status()
    } else {
        let Some(state) = repo.current_state()? else {
            return Ok(Some(WorktreeStatus::default()));
        };
        let tree = repo.require_tree(&state.tree)?;
        repo.compare_worktree_cached(&tree).map(Some)
    }
}

fn degraded_repository_verification_state(
    repo: &Repository,
    summary: String,
) -> RepositoryVerificationState {
    let coverage = machine_contract_coverage();
    RepositoryVerificationState {
        verified: false,
        status: "degraded".to_string(),
        repository_mode: repo.capability_label().to_string(),
        heddle_initialized: true,
        git_branch: repo.git_overlay_current_branch().ok().flatten(),
        heddle_thread: repo.current_lane().ok().flatten(),
        worktree_dirty: false,
        worktree_state: "not_checked".to_string(),
        import_state: "not_checked".to_string(),
        mapping_state: "not_checked".to_string(),
        remote_drift: "not_checked".to_string(),
        active_operation: None,
        default_remote: None,
        clone_verification: "not_checked".to_string(),
        machine_contract: machine_contract_status(&coverage).to_string(),
        machine_contract_coverage: coverage,
        workflow_status: "not_checked".to_string(),
        workflow_summary: "workflow readiness is checked after repository verification is restored"
            .to_string(),
        summary,
        recommended_action: "heddle doctor".to_string(),
        recommended_action_template: action_template("heddle doctor"),
        recovery_commands: vec!["heddle doctor".to_string()],
        recovery_action_templates: action_templates(&["heddle doctor".to_string()]),
        checks: Vec::new(),
    }
}
/// Sub-phase timings for [`build_repository_verification_state_profiled`], in
/// milliseconds. Used by `adopt`'s `HEDDLE_PROFILE=1` instrumentation to attribute
/// Sub-phase timings for [`build_repository_verification_state_profiled`], in
/// milliseconds. Used by `adopt`'s `HEDDLE_PROFILE=1` instrumentation to attribute
/// the post-import verification cost across its three phases.
/// [`build_repository_verification_state`] that additionally reports per-phase
/// timings. Classification is identical; the only difference is the elapsed
/// [`build_repository_verification_state`] that additionally reports per-phase
/// timings. Classification is identical; the only difference is the elapsed
/// clocks around each phase.
/// Verification-state build that reuses an already-computed git-overlay worktree
/// status instead of re-walking + re-SHA-1ing every tracked file. Used by the
/// Verification-state build that reuses an already-computed git-overlay worktree
/// status instead of re-walking + re-SHA-1ing every tracked file. Used by the
/// `checkpoint` hot path, where the same status would otherwise be recomputed
/// Verification-state build that reuses an already-computed git-overlay worktree
/// status instead of re-walking + re-SHA-1ing every tracked file. Used by the
/// `checkpoint` hot path, where the same status would otherwise be recomputed
/// across the preflight, the verification preflight, and the output build. The
/// Verification-state build that reuses an already-computed git-overlay worktree
/// status instead of re-walking + re-SHA-1ing every tracked file. Used by the
/// `checkpoint` hot path, where the same status would otherwise be recomputed
/// across the preflight, the verification preflight, and the output build. The
/// classification is byte-identical to [`build_repository_verification_state`]
/// Verification-state build that reuses an already-computed git-overlay worktree
/// status instead of re-walking + re-SHA-1ing every tracked file. Used by the
/// `checkpoint` hot path, where the same status would otherwise be recomputed
/// across the preflight, the verification preflight, and the output build. The
/// classification is byte-identical to [`build_repository_verification_state`]
/// because it threads the exact `Result` from `git_overlay_worktree_status()`.
pub(crate) fn unimported_git_history_advice(
    repo: &Repository,
    action: &str,
) -> anyhow::Result<Option<RecoveryAdvice>> {
    if repo.capability() != repo::RepositoryCapability::GitOverlay {
        return Ok(None);
    }

    let Some(hint) = repo.git_import_guidance()? else {
        return Ok(None);
    };
    if !import_guidance_includes_active_branch(&hint) {
        return Ok(None);
    }
    let missing = hint.missing_branches;
    let primary_command = hint.recommended_command;
    let branch_summary = crate::cli::render::preview_list(&missing, missing.len());
    Ok(Some(RecoveryAdvice::safety_refusal(
        "git_history_needs_import",
        format!("Refusing to {action}: Git history has not been imported into Heddle"),
        format!("Run `{primary_command}` before retrying `heddle {action}`."),
        format!("Git branch(es) waiting for Heddle import: {branch_summary}"),
        format!(
            "{action} would write new Heddle state before Heddle has adopted the existing Git history"
        ),
        "Git refs, Heddle refs, and worktree files were left unchanged",
        primary_command.clone(),
        vec![primary_command],
    )))
}
pub(crate) fn raw_git_operation_mutation_advice(
    repo: &Repository,
    action: &str,
) -> anyhow::Result<Option<RecoveryAdvice>> {
    let Some(operation) = repo.operation_status()? else {
        return Ok(None);
    };
    if !matches!(operation.scope, OperationScope::Git) {
        return Ok(None);
    }
    let primary_command = "heddle verify".to_string();
    let hint = raw_git_operation_recovery_hint(&operation.kind, &primary_command, action);
    Ok(Some(RecoveryAdvice::safety_refusal(
        "raw_git_operation_in_progress",
        format!(
            "Refusing to {action}: an externally-started Git {} is in progress",
            operation.kind
        ),
        hint,
        format!(
            "Git {} is {}; Heddle cannot safely turn sequencer state into a saved change inside the no-git runtime",
            operation.kind, operation.state
        ),
        format!(
            "{action} would capture worktree/index contents while Git still has unresolved sequencer metadata"
        ),
        "Git refs, Git sequencer files, Heddle refs, and worktree files were left unchanged",
        primary_command.clone(),
        vec![primary_command, "heddle verify".to_string()],
    )))
}
fn raw_git_operation_recovery_hint(
    kind: &OperationKind,
    primary_command: &str,
    action: &str,
) -> String {
    format!(
        "Inspect with `{primary_command}`. Heddle did not start this raw Git {kind}, so finish or abort it with the Git-compatible tool that started it, then run `heddle verify` for the exact adoption command before retrying `heddle {action}`."
    )
}
pub(crate) fn verification_blocking_mutation_advice(
    repo: &Repository,
    action: &str,
) -> anyhow::Result<Option<RecoveryAdvice>> {
    verification_blocking_mutation_advice_with_trust(
        repo,
        action,
        build_repository_verification_state(repo),
    )
}
fn verification_blocking_mutation_advice_with_trust(
    repo: &Repository,
    action: &str,
    trust: RepositoryVerificationState,
) -> anyhow::Result<Option<RecoveryAdvice>> {
    if trust.status != "needs_reconcile" {
        return Ok(None);
    }
    if uncheckpointed_heddle_state_is_ahead_of_git(repo)? {
        return Ok(None);
    }
    Ok(Some(repository_verification_blocked_advice(
        "repository_verification_blocked",
        format!(
            "Refusing to {action}: repository verification is blocked ({})",
            trust.status
        ),
        format!("retrying `heddle {action}`"),
        &trust,
        format!(
            "repository verification status is {}: {}",
            trust.status, trust.summary
        ),
        format!("{action} would write new Heddle or Git state while Git and Heddle disagree"),
        "Git refs, Heddle refs, Git checkpoint metadata, and worktree files were left unchanged",
        None,
    )))
}
fn uncheckpointed_heddle_state_is_ahead_of_git(repo: &Repository) -> anyhow::Result<bool> {
    let Some(tip) = current_branch_tip(repo)? else {
        return Ok(false);
    };
    let Some(mapped) = tip.mapped_change else {
        return Ok(false);
    };
    let Some(current) = repo.current_state()? else {
        return Ok(false);
    };
    if mapped == current.change_id {
        return Ok(false);
    }
    if mapped_change_relation(repo, &mapped, &current.change_id) != "git_behind_heddle" {
        return Ok(false);
    }
    Ok(repo
        .latest_git_checkpoint_for_change(&current.change_id)?
        .is_none())
}

fn current_branch_tip(repo: &Repository) -> anyhow::Result<Option<GitOverlayBranchTip>> {
    let Some(branch) = repo.git_overlay_current_branch()? else {
        return Ok(None);
    };
    repo.git_overlay_branch_tip(&branch).map_err(Into::into)
}

fn mapped_change_relation(
    repo: &Repository,
    git_mapped: &objects::object::ChangeId,
    heddle_current: &objects::object::ChangeId,
) -> &'static str {
    let mut graph = CommitGraphIndex::new(repo);
    let git_is_ancestor = graph
        .is_ancestor(git_mapped, heddle_current)
        .unwrap_or(false);
    let heddle_is_ancestor = graph
        .is_ancestor(heddle_current, git_mapped)
        .unwrap_or(false);
    match (git_is_ancestor, heddle_is_ancestor) {
        (true, false) => "git_behind_heddle",
        (false, true) => "git_ahead_of_heddle",
        (true, true) => "same",
        (false, false) => "diverged",
    }
}
#[derive(Debug, Clone, Copy)]
pub(crate) struct GitOverlayMutationPreflight {
    pub check_detached_head: bool,
    pub check_unimported_git_history: bool,
    pub check_raw_git_operation: bool,
    pub check_verification: bool,
}
impl GitOverlayMutationPreflight {
    pub(crate) fn capture_like() -> Self {
        Self {
            check_detached_head: false,
            check_unimported_git_history: true,
            check_raw_git_operation: true,
            check_verification: true,
        }
    }

    pub(crate) fn checkpoint_like() -> Self {
        Self {
            check_detached_head: true,
            check_unimported_git_history: true,
            check_raw_git_operation: false,
            check_verification: true,
        }
    }

    pub(crate) fn commit_like() -> Self {
        Self {
            check_detached_head: true,
            check_unimported_git_history: true,
            check_raw_git_operation: true,
            check_verification: true,
        }
    }
}
pub(crate) fn plain_git_mutation_preflight_advice(
    start: &std::path::Path,
    action: &str,
) -> anyhow::Result<Option<RecoveryAdvice>> {
    Ok(build_plain_git_verification_probe(start)?
        .map(|probe| plain_git_mutation_advice(&probe, action)))
}
pub(crate) fn plain_git_setup_advice(
    probe: &PlainGitVerificationProbe,
    command: &str,
    requested_target: Option<&str>,
) -> RecoveryAdvice {
    let primary = if probe.trust.recommended_action.is_empty() {
        "heddle init".to_string()
    } else {
        probe.trust.recommended_action.clone()
    };
    let mut recovery_commands = probe.trust.recovery_commands.clone();
    if recovery_commands.is_empty() {
        recovery_commands.push(primary.clone());
    }
    let retry = requested_target
        .map(|target| format!("heddle {command} {target}"))
        .unwrap_or_else(|| format!("heddle {command}"));
    let mut advice = RecoveryAdvice::safety_refusal(
        "plain_git_needs_init",
        "Heddle is not initialized for this Git repo",
        format!("Run `{primary}` to create the Heddle sidecar, then retry `{retry}`."),
        format!(
            "plain Git repository at '{}' has no .heddle metadata",
            probe.root.display()
        ),
        format!(
            "`heddle {command}` needs Heddle history before it can inspect Heddle states without guessing"
        ),
        "observe-only command; Heddle metadata, Git refs, index, and worktree files were left unchanged",
        primary,
        recovery_commands,
    );
    advice.extra_json_fields.insert(
        "repository_capability".to_string(),
        serde_json::Value::String("plain-git".to_string()),
    );
    advice.extra_json_fields.insert(
        "storage_model".to_string(),
        serde_json::Value::String("git".to_string()),
    );
    advice.extra_json_fields.insert(
        "requested_command".to_string(),
        serde_json::Value::String(command.to_string()),
    );
    if let Some(target) = requested_target {
        advice.extra_json_fields.insert(
            "requested_target".to_string(),
            serde_json::Value::String(target.to_string()),
        );
    }
    if let Ok(verification) = serde_json::to_value(&probe.trust) {
        advice
            .extra_json_fields
            .insert("verification".to_string(), verification);
    }
    advice
}
pub(crate) fn git_overlay_mutation_preflight_advice(
    repo: &Repository,
    action: &str,
    preflight: GitOverlayMutationPreflight,
) -> anyhow::Result<Option<RecoveryAdvice>> {
    git_overlay_mutation_preflight_advice_inner(repo, action, preflight, None)
}
/// `checkpoint` hot-path variant of [`git_overlay_mutation_preflight_advice`]
/// that reuses an already-computed git-overlay worktree status for the
/// `checkpoint` hot-path variant of [`git_overlay_mutation_preflight_advice`]
/// that reuses an already-computed git-overlay worktree status for the
/// `check_verification` branch instead of re-walking the worktree. All other
/// `checkpoint` hot-path variant of [`git_overlay_mutation_preflight_advice`]
/// that reuses an already-computed git-overlay worktree status for the
/// `check_verification` branch instead of re-walking the worktree. All other
/// branches and the resulting advice are identical.
/// `checkpoint` hot-path variant of [`git_overlay_mutation_preflight_advice`]
/// that reuses an already-computed git-overlay worktree status for the
/// `check_verification` branch instead of re-walking the worktree. All other
/// branches and the resulting advice are identical.
pub(crate) fn git_overlay_mutation_preflight_advice_with_worktree_status(
    repo: &Repository,
    action: &str,
    preflight: GitOverlayMutationPreflight,
    worktree_status: &repo::Result<Option<WorktreeStatus>>,
) -> anyhow::Result<Option<RecoveryAdvice>> {
    git_overlay_mutation_preflight_advice_inner(repo, action, preflight, Some(worktree_status))
}
fn git_overlay_mutation_preflight_advice_inner(
    repo: &Repository,
    action: &str,
    preflight: GitOverlayMutationPreflight,
    worktree_status: Option<&repo::Result<Option<WorktreeStatus>>>,
) -> anyhow::Result<Option<RecoveryAdvice>> {
    if repo.capability() != repo::RepositoryCapability::GitOverlay {
        return Ok(None);
    }
    if preflight.check_detached_head && repo.git_overlay_head_is_detached()? {
        return Ok(Some(detached_git_head_mutation_advice(repo, action)));
    }
    if preflight.check_unimported_git_history
        && let Some(advice) = unimported_git_history_advice(repo, action)?
    {
        return Ok(Some(advice));
    }
    if preflight.check_raw_git_operation
        && let Some(advice) = raw_git_operation_mutation_advice(repo, action)?
    {
        return Ok(Some(advice));
    }
    if preflight.check_verification {
        let advice = match worktree_status {
            Some(status) => verification_blocking_mutation_advice_with_trust(
                repo,
                action,
                build_repository_verification_state_with_worktree_status(repo, status),
            )?,
            None => verification_blocking_mutation_advice(repo, action)?,
        };
        if let Some(advice) = advice {
            return Ok(Some(advice));
        }
    }
    Ok(None)
}
#[allow(clippy::too_many_arguments)]
pub(crate) fn repository_verification_blocked_advice(
    kind: &'static str,
    error: impl Into<String>,
    retry_context: impl Into<String>,
    trust: &RepositoryVerificationState,
    unsafe_condition: impl Into<String>,
    would_change: impl Into<String>,
    preserved: impl Into<String>,
    primary_override: Option<String>,
) -> RecoveryAdvice {
    let primary_command =
        primary_override.unwrap_or_else(|| repository_verification_primary_command(trust));
    let recovery_commands = repository_verification_recovery_commands(trust, &primary_command);
    RecoveryAdvice::safety_refusal(
        kind,
        error,
        format!("Run `{}` before {}.", primary_command, retry_context.into()),
        unsafe_condition,
        would_change,
        preserved,
        primary_command,
        recovery_commands,
    )
}
pub(crate) fn repository_verification_primary_command(
    trust: &RepositoryVerificationState,
) -> String {
    if trust.recommended_action.trim().is_empty() {
        "heddle verify".to_string()
    } else {
        trust.recommended_action.clone()
    }
}
pub(crate) fn repository_verification_blockers(trust: &RepositoryVerificationState) -> Vec<String> {
    trust
        .checks
        .iter()
        .filter(|check| !check.clean)
        .map(|check| format!("{}: {}", check.name, check.summary))
        .collect()
}
pub(crate) fn repository_verification_recovery_commands(
    trust: &RepositoryVerificationState,
    primary_command: &str,
) -> Vec<String> {
    if primary_command != trust.recommended_action && !trust.recommended_action.trim().is_empty() {
        vec![primary_command.to_string(), "heddle verify".to_string()]
    } else if trust.recovery_commands.is_empty() {
        vec![primary_command.to_string()]
    } else {
        trust.recovery_commands.clone()
    }
}
pub(crate) fn repository_setup_guidance(
    trust: &RepositoryVerificationState,
) -> Option<RepositorySetupGuidance> {
    if !matches!(trust.status.as_str(), "needs_init" | "needs_import") {
        return None;
    }
    let action = trust.recommended_action.trim();
    if action.is_empty() {
        return None;
    }
    let kind = repository_setup_action_kind(action);
    let setup_line = match kind {
        RepositorySetupActionKind::Init => {
            format!("Git repo detected; initialize Heddle with {action}")
        }
        RepositorySetupActionKind::Adopt => {
            format!("Git repo detected; connect this branch with {action}")
        }
        RepositorySetupActionKind::GitImport => {
            format!("Git history not imported; import it with {action}")
        }
        RepositorySetupActionKind::Other => {
            format!("Run {action} to clear the primary setup blocker")
        }
    };
    let worktree_tail = if trust.worktree_state == "clean" {
        "and the Git worktree stays clean"
    } else {
        "and existing Git worktree changes stay untouched"
    };
    let effect = match kind {
        RepositorySetupActionKind::Init => format!(
            ".heddle metadata will be created; Git commits stay in Git storage, {worktree_tail}."
        ),
        RepositorySetupActionKind::Adopt
            if trust.repository_mode == "plain-git" && !trust.heddle_initialized =>
        {
            format!(".heddle metadata will be created, Git history imported, {worktree_tail}.")
        }
        RepositorySetupActionKind::Adopt => {
            format!(".heddle metadata is present; adoption imports Git history {worktree_tail}.")
        }
        RepositorySetupActionKind::GitImport => {
            format!(".heddle metadata is present; Git history import runs {worktree_tail}.")
        }
        RepositorySetupActionKind::Other => {
            format!("The recommended setup command runs {worktree_tail}.")
        }
    };
    Some(RepositorySetupGuidance { setup_line, effect })
}
fn repository_setup_action_kind(action: &str) -> RepositorySetupActionKind {
    if action == "heddle init" {
        RepositorySetupActionKind::Init
    } else if action.starts_with("heddle adopt") {
        RepositorySetupActionKind::Adopt
    } else if action.starts_with("heddle import git") {
        RepositorySetupActionKind::GitImport
    } else {
        RepositorySetupActionKind::Other
    }
}
pub(crate) fn plain_git_mutation_advice(
    probe: &PlainGitVerificationProbe,
    action: &str,
) -> RecoveryAdvice {
    let primary_command = if probe.trust.recommended_action.trim().is_empty() {
        "heddle init".to_string()
    } else {
        probe.trust.recommended_action.clone()
    };
    let recovery_commands = if probe.trust.recovery_commands.is_empty() {
        vec![primary_command.clone()]
    } else {
        probe.trust.recovery_commands.clone()
    };
    let dirty_detail = if probe.changes.is_clean() {
        "Git worktree is clean".to_string()
    } else {
        let mut paths = Vec::new();
        paths.extend(
            probe
                .changes
                .modified
                .iter()
                .map(|path| path.display().to_string()),
        );
        paths.extend(
            probe
                .changes
                .added
                .iter()
                .map(|path| path.display().to_string()),
        );
        paths.extend(
            probe
                .changes
                .deleted
                .iter()
                .map(|path| path.display().to_string()),
        );
        format!(
            "Git worktree has {} dirty path(s): {}",
            paths.len(),
            crate::cli::render::preview_list(&paths, paths.len())
        )
    };
    RecoveryAdvice::safety_refusal(
        "git_repo_needs_init",
        format!("Refusing to {action}: Heddle is not initialized for this Git repository"),
        format!("Run `{primary_command}` before retrying `heddle {action}`."),
        format!(
            "plain Git repository at {} has no .heddle metadata; {}",
            probe.root.display(),
            dirty_detail
        ),
        format!("{action} needs Heddle metadata before it can safely write Heddle state"),
        "Git refs, Heddle metadata, and worktree files were left unchanged",
        primary_command,
        recovery_commands,
    )
}
pub(crate) fn detached_git_head_mutation_advice(repo: &Repository, action: &str) -> RecoveryAdvice {
    let primary_command = detached_head_primary_recovery(repo);
    RecoveryAdvice::safety_refusal(
        "git_head_detached",
        format!("Refusing to {action}: Git HEAD is detached"),
        format!("Run `{primary_command}` before retrying `heddle {action}`."),
        "Git HEAD points directly to a commit instead of an attached branch",
        format!(
            "{action} would need to write a Git checkpoint through a branch and could reattach or advance the wrong ref"
        ),
        "Git refs, Heddle refs, Git checkpoints, and worktree files were left unchanged",
        primary_command.clone(),
        vec![primary_command],
    )
}
fn detached_head_primary_recovery(repo: &Repository) -> String {
    match repo.refs().read_head() {
        Ok(Head::Attached { thread }) if !thread.trim().is_empty() => {
            // `switch` takes the thread as a positional; a leading-dash id needs
            // the `--` separator so clap binds it as a value, not a flag.
            // (heddle#464 close-the-class.)
            return if thread.starts_with('-') {
                heddle_action(["switch", "--", thread.as_str()])
            } else {
                heddle_action(["switch", thread.as_str()])
            };
        }
        _ => {}
    }
    if let Ok(Some(detached_commit)) = repo.git_overlay_detached_head_commit()
        && let Ok(branch_tips) = repo.git_overlay_branch_tips()
        && let Some(tip) = branch_tips
            .iter()
            .filter(|tip| tip.history_imported)
            .find(|tip| tip.git_commit == detached_commit)
    {
        return heddle_action(["switch", tip.branch.as_str()]);
    }
    "heddle switch <branch>".to_string()
}
pub(crate) fn build_plain_git_verification_probe(
    start: &Path,
) -> anyhow::Result<Option<PlainGitVerificationProbe>> {
    Ok(
        heddle_core::verify::build_plain_git_verification_probe_with_machine_contract(
            start,
            &MachineContractInput::from_coverage(machine_contract_coverage()),
        )?,
    )
}
pub(crate) fn action_template(action: &str) -> Option<ActionTemplate> {
    recommended_action_template(action)
}
pub(crate) fn action_templates(commands: &[String]) -> Vec<ActionTemplate> {
    commands
        .iter()
        .filter_map(|command| action_template(command))
        .collect()
}
pub(crate) fn machine_contract_coverage() -> MachineContractCoverage {
    const EXAMPLE_LIMIT: usize = 8;
    let catalog = build_command_catalog();
    let commands = catalog.commands;
    let mut json_commands_total: usize = 0;
    let mut json_commands_with_schema: usize = 0;
    let mut json_commands_with_accepted_opaque_schema: usize = 0;
    let mut verified_scope_json_commands_total: usize = 0;
    let mut verified_scope_json_commands_with_schema: usize = 0;
    let mut verified_scope_json_commands_with_accepted_opaque_schema: usize = 0;
    let mut catalog_mutating_commands_total: usize = 0;
    let mut mutating_commands_total: usize = 0;
    let mut mutating_commands_with_schema: usize = 0;
    let mut mutating_commands_with_accepted_opaque_schema: usize = 0;
    let mut verified_scope_mutating_commands_total: usize = 0;
    let mut verified_scope_mutating_commands_with_schema: usize = 0;
    let mut verified_scope_mutating_commands_with_accepted_opaque_schema: usize = 0;
    let mut schema_verbs = BTreeSet::new();
    let mut documented_schema_verbs = BTreeSet::new();
    let opaque_schema_verb_set: BTreeSet<&str> = opaque_schema_verbs().iter().copied().collect();
    let mut supports_op_id_total: usize = 0;
    let mut jsonl_commands_total: usize = 0;
    let mut missing_schema_examples = Vec::new();
    let mut missing_mutating_schema_examples = Vec::new();
    let mut verified_scope_missing_schema_examples = Vec::new();
    let mut verified_scope_accepted_opaque_schema_examples = Vec::new();
    let mut advanced_scope_accepted_opaque_schema_examples = Vec::new();
    let mut accepted_opaque_schema_examples = Vec::new();

    for command in &commands {
        let is_verified_scope = machine_contract_verified_scope(command);
        let has_concrete_schema = command
            .schema_verbs
            .iter()
            .any(|verb| !opaque_schema_verb_set.contains(verb.as_str()));
        let has_accepted_opaque_schema = command
            .schema_verbs
            .iter()
            .any(|verb| opaque_schema_verb_set.contains(verb.as_str()));
        if command.mutates {
            catalog_mutating_commands_total += 1;
        }
        if command.supports_json {
            json_commands_total += 1;
            if has_concrete_schema {
                json_commands_with_schema += 1;
            } else if has_accepted_opaque_schema {
                json_commands_with_accepted_opaque_schema += 1;
                if accepted_opaque_schema_examples.len() < EXAMPLE_LIMIT {
                    accepted_opaque_schema_examples.push(command.display.clone());
                }
                if !is_verified_scope
                    && advanced_scope_accepted_opaque_schema_examples.len() < EXAMPLE_LIMIT
                {
                    advanced_scope_accepted_opaque_schema_examples.push(command.display.clone());
                }
            } else if missing_schema_examples.len() < EXAMPLE_LIMIT {
                missing_schema_examples.push(command.display.clone());
            }
            if is_verified_scope {
                verified_scope_json_commands_total += 1;
                if has_concrete_schema {
                    verified_scope_json_commands_with_schema += 1;
                } else if has_accepted_opaque_schema {
                    verified_scope_json_commands_with_accepted_opaque_schema += 1;
                    if verified_scope_accepted_opaque_schema_examples.len() < EXAMPLE_LIMIT {
                        verified_scope_accepted_opaque_schema_examples
                            .push(command.display.clone());
                    }
                } else if verified_scope_missing_schema_examples.len() < EXAMPLE_LIMIT {
                    verified_scope_missing_schema_examples.push(command.display.clone());
                }
            }
        }
        if command.mutates && command.supports_json {
            mutating_commands_total += 1;
            if has_concrete_schema {
                mutating_commands_with_schema += 1;
            } else if has_accepted_opaque_schema {
                mutating_commands_with_accepted_opaque_schema += 1;
            } else if missing_mutating_schema_examples.len() < EXAMPLE_LIMIT {
                missing_mutating_schema_examples.push(command.display.clone());
            }
            if is_verified_scope {
                verified_scope_mutating_commands_total += 1;
                if has_concrete_schema {
                    verified_scope_mutating_commands_with_schema += 1;
                } else if has_accepted_opaque_schema {
                    verified_scope_mutating_commands_with_accepted_opaque_schema += 1;
                }
            }
        }
        if command.supports_op_id {
            supports_op_id_total += 1;
        }
        if command.json_kind == "jsonl" || command.json_kind == "json_or_jsonl" {
            jsonl_commands_total += 1;
        }
        schema_verbs.extend(command.schema_verbs.iter().map(String::as_str));
        documented_schema_verbs.extend(command.documented_schema_verbs.iter().map(String::as_str));
    }
    schema_verbs.insert("error");
    documented_schema_verbs.insert("error");

    let json_commands_without_schema = json_commands_total
        .saturating_sub(json_commands_with_schema)
        .saturating_sub(json_commands_with_accepted_opaque_schema);
    let mutating_commands_without_schema = mutating_commands_total
        .saturating_sub(mutating_commands_with_schema)
        .saturating_sub(mutating_commands_with_accepted_opaque_schema);
    let verified_scope_json_commands_without_schema = verified_scope_json_commands_total
        .saturating_sub(verified_scope_json_commands_with_schema)
        .saturating_sub(verified_scope_json_commands_with_accepted_opaque_schema);
    let verified_scope_mutating_commands_without_schema = verified_scope_mutating_commands_total
        .saturating_sub(verified_scope_mutating_commands_with_schema)
        .saturating_sub(verified_scope_mutating_commands_with_accepted_opaque_schema);
    let advanced_scope_json_commands_total =
        json_commands_total.saturating_sub(verified_scope_json_commands_total);
    let advanced_scope_json_commands_with_accepted_opaque_schema =
        json_commands_with_accepted_opaque_schema
            .saturating_sub(verified_scope_json_commands_with_accepted_opaque_schema);
    let advanced_scope_mutating_commands_total =
        mutating_commands_total.saturating_sub(verified_scope_mutating_commands_total);
    let advanced_scope_mutating_commands_with_accepted_opaque_schema =
        mutating_commands_with_accepted_opaque_schema
            .saturating_sub(verified_scope_mutating_commands_with_accepted_opaque_schema);
    let undocumented_schema_examples: Vec<String> = schema_verbs
        .difference(&documented_schema_verbs)
        .take(EXAMPLE_LIMIT)
        .map(|verb| (*verb).to_string())
        .collect();
    let accepted_opaque_schema_verbs: BTreeSet<&str> = schema_verbs
        .intersection(&opaque_schema_verb_set)
        .copied()
        .collect();
    let schema_verbs_total = schema_verbs.len();
    let documented_schema_verbs_total = documented_schema_verbs.len();
    let undocumented_schema_verbs_total = schema_verbs.difference(&documented_schema_verbs).count();
    let opaque_schema_verbs_total = accepted_opaque_schema_verbs.len();
    let accepted_opaque_schema_verbs_total = accepted_opaque_schema_verbs.len();
    let unaccepted_opaque_schema_verbs_total = 0;
    let unaccepted_opaque_schema_examples = Vec::new();
    let status = if verified_scope_json_commands_without_schema == 0
        && verified_scope_mutating_commands_without_schema == 0
        && verified_scope_json_commands_with_accepted_opaque_schema == 0
        && verified_scope_mutating_commands_with_accepted_opaque_schema == 0
        && undocumented_schema_verbs_total == 0
    {
        "available".to_string()
    } else if verified_scope_json_commands_without_schema == 0
        && verified_scope_mutating_commands_without_schema == 0
        && undocumented_schema_verbs_total == 0
        && unaccepted_opaque_schema_verbs_total == 0
    {
        "available_with_opaque_schemas".to_string()
    } else if verified_scope_json_commands_without_schema == 0
        && verified_scope_mutating_commands_without_schema == 0
        && unaccepted_opaque_schema_verbs_total == 0
    {
        "available_with_doc_gaps".to_string()
    } else {
        "available_with_schema_gaps".to_string()
    };
    let summary = if status == "available" {
        if accepted_opaque_schema_verbs_total == 0 {
            format!(
                "{} command(s), {} JSON command(s), verified everyday/agent machine surface has concrete schemas",
                commands.len(),
                json_commands_total
            )
        } else {
            format!(
                "{} command(s), {} JSON command(s), {} mutating command(s), {} mutating JSON command(s); verified everyday/agent machine surface has {} concrete schema-backed JSON command(s); advanced/internal/admin surfaces carry {} accepted opaque schema(s) outside clean verification",
                commands.len(),
                json_commands_total,
                catalog_mutating_commands_total,
                mutating_commands_total,
                verified_scope_json_commands_with_schema,
                accepted_opaque_schema_verbs_total
            )
        }
    } else if status == "available_with_opaque_schemas" {
        format!(
            "{} command(s), {} JSON command(s), verified everyday/agent machine surface has {} concrete schema-backed and {} accepted opaque schema-backed command(s)",
            commands.len(),
            json_commands_total,
            verified_scope_json_commands_with_schema,
            verified_scope_json_commands_with_accepted_opaque_schema
        )
    } else if status == "available_with_doc_gaps" {
        format!(
            "{} command(s), {} JSON command(s), {} concrete schema-backed and {} accepted opaque; {} runtime schema verb(s) need documented samples",
            commands.len(),
            json_commands_total,
            json_commands_with_schema,
            json_commands_with_accepted_opaque_schema,
            undocumented_schema_verbs_total
        )
    } else {
        format!(
            "{} command(s), {} JSON command(s), {} concrete schema-backed, {} accepted opaque, {} missing schemas ({} mutating)",
            commands.len(),
            json_commands_total,
            json_commands_with_schema,
            json_commands_with_accepted_opaque_schema,
            json_commands_without_schema,
            mutating_commands_without_schema
        )
    };

    MachineContractCoverage {
        status,
        verified_scope: "everyday_and_agent".to_string(),
        advanced_scope: "advanced_internal_admin".to_string(),
        summary,
        catalog_commands_total: commands.len(),
        catalog_mutating_commands_total,
        json_commands_total,
        json_mutating_commands_total: mutating_commands_total,
        json_commands_with_schema,
        json_commands_with_accepted_opaque_schema,
        json_commands_without_schema,
        verified_scope_json_commands_total,
        verified_scope_json_commands_with_schema,
        verified_scope_json_commands_with_accepted_opaque_schema,
        verified_scope_json_commands_without_schema,
        advanced_scope_json_commands_total,
        advanced_scope_json_commands_with_accepted_opaque_schema,
        mutating_commands_total,
        mutating_commands_with_schema,
        mutating_commands_with_accepted_opaque_schema,
        mutating_commands_without_schema,
        verified_scope_mutating_commands_total,
        verified_scope_mutating_commands_with_schema,
        verified_scope_mutating_commands_with_accepted_opaque_schema,
        verified_scope_mutating_commands_without_schema,
        advanced_scope_mutating_commands_total,
        advanced_scope_mutating_commands_with_accepted_opaque_schema,
        schema_verbs_total,
        documented_schema_verbs_total,
        undocumented_schema_verbs_total,
        opaque_schema_verbs_total,
        accepted_opaque_schema_verbs_total,
        unaccepted_opaque_schema_verbs_total,
        supports_op_id_total,
        jsonl_commands_total,
        missing_schema_examples,
        missing_mutating_schema_examples,
        verified_scope_missing_schema_examples,
        verified_scope_accepted_opaque_schema_examples,
        advanced_scope_accepted_opaque_schema_examples,
        accepted_opaque_schema_examples,
        unaccepted_opaque_schema_examples,
        undocumented_schema_examples,
    }
}
fn machine_contract_verified_scope(command: &super::command_catalog::CommandCatalogEntry) -> bool {
    command.help_visibility == "everyday"
        || matches!(
            command.path.first().map(String::as_str),
            Some("actor" | "agent" | "commands" | "schemas" | "session")
        )
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RemoteDriftDecision {
    pub status: &'static str,
    pub verified_as_clean: bool,
    pub primary_action: Option<String>,
    pub recovery_commands: Vec<String>,
    pub requires_clean_worktree: bool,
}
pub(crate) fn remote_drift_decision(
    repo: &Repository,
    remote: &GitRemoteTrackingStatus,
) -> RemoteDriftDecision {
    let status = remote_tracking_status(remote);
    match status {
        "clean" => RemoteDriftDecision {
            status,
            verified_as_clean: true,
            primary_action: None,
            recovery_commands: Vec::new(),
            requires_clean_worktree: false,
        },
        "remote_untracked" => RemoteDriftDecision {
            status,
            verified_as_clean: true,
            primary_action: remote_tracking_next_action(remote),
            recovery_commands: Vec::new(),
            requires_clean_worktree: false,
        },
        "remote_ahead" => RemoteDriftDecision {
            status,
            verified_as_clean: true,
            primary_action: Some("heddle push".to_string()),
            recovery_commands: Vec::new(),
            requires_clean_worktree: false,
        },
        "remote_contains_undone_checkpoint" => RemoteDriftDecision {
            status,
            verified_as_clean: false,
            primary_action: remote_tracking_next_action(remote),
            recovery_commands: vec![
                core_heddle_action(["push", "--force"]),
                core_heddle_action(["undo", "--redo"]),
            ],
            requires_clean_worktree: true,
        },
        "remote_behind" => RemoteDriftDecision {
            status,
            verified_as_clean: false,
            primary_action: Some("heddle pull".to_string()),
            recovery_commands: vec!["heddle pull".to_string()],
            requires_clean_worktree: true,
        },
        "remote_diverged" => {
            let upstream = remote.upstream.trim();
            if upstream.is_empty() {
                return RemoteDriftDecision {
                    status,
                    verified_as_clean: false,
                    primary_action: Some("heddle fetch".to_string()),
                    recovery_commands: vec!["heddle fetch".to_string()],
                    requires_clean_worktree: false,
                };
            }
            let import = canonical_git_import_ref_command(upstream);
            let reconcile = canonical_git_repair_ref_preview_command(None, upstream);
            let imported = upstream_thread_matches_current_git_tip(repo, upstream);
            RemoteDriftDecision {
                status,
                verified_as_clean: false,
                primary_action: Some(if imported {
                    reconcile.clone()
                } else {
                    import.clone()
                }),
                recovery_commands: if imported {
                    vec![reconcile]
                } else {
                    vec![import, reconcile]
                },
                requires_clean_worktree: false,
            }
        }
        _ => RemoteDriftDecision {
            status,
            verified_as_clean: false,
            primary_action: Some("heddle verify".to_string()),
            recovery_commands: vec!["heddle verify".to_string()],
            requires_clean_worktree: false,
        },
    }
}
fn upstream_thread_matches_current_git_tip(repo: &Repository, upstream: &str) -> bool {
    let Some(thread_tip) = repo
        .refs()
        .get_thread(&ThreadName::new(upstream))
        .ok()
        .flatten()
    else {
        return false;
    };
    repo.git_overlay_mapped_change_for_branch(upstream)
        .or(Ok(None))
        .and_then(|mapped| {
            if mapped.is_some() {
                Ok(mapped)
            } else {
                repo.git_overlay_mapped_change_for_remote_tracking_ref(upstream)
            }
        })
        .ok()
        .flatten()
        .is_some_and(|mapped_tip| mapped_tip == thread_tip)
}
pub(crate) fn remote_drift_primary_action(repo: &Repository) -> Option<String> {
    repo.git_remote_tracking_status()
        .ok()
        .flatten()
        .and_then(|remote| remote_drift_decision(repo, &remote).primary_action)
}
pub(crate) fn remote_tracking_with_verification_action(
    mut remote: GitRemoteTrackingStatus,
    trust: &RepositoryVerificationState,
) -> GitRemoteTrackingStatus {
    let remote_status = remote_tracking_status(&remote);
    if trust.status == remote_status && !trust.recommended_action.trim().is_empty() {
        remote.next_action = trust.recommended_action.clone();
    }
    remote
}

#[cfg(test)]
mod tests;
