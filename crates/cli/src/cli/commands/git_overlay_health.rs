// SPDX-License-Identifier: Apache-2.0
//! Shared repository verification contract.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

use objects::{object::ThreadName, worktree::WorktreeStatus};
use refs::Head;
use repo::{
    CommitGraphIndex, GitOverlayBranchTip, GitOverlayImportHint, GitOverlayOutOfBandCommits,
    GitRemoteTrackingStatus, OperationKind, OperationScope, Repository, ThreadManager, ThreadState,
    describe_thread_advice, git_worktree_status::GitWorktreeEntryState, refresh_thread_freshness,
};
use schemars::JsonSchema;
use serde::{Serialize, Serializer};
use sley::{BString as GitBString, Index, Repository as SleyRepository};

use super::{
    advice::RecoveryAdvice,
    command_catalog::{
        ActionFields, ActionTemplate, build_command_catalog, heddle_action,
        recommended_action_template,
    },
    schemas::opaque_schema_verbs,
};
use crate::{cli::worktree_status_options, remote::RemoteConfig};

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub(crate) struct GitOverlayHealth {
    pub status: String,
    pub clean: bool,
    pub summary: String,
    pub recovery_commands: Vec<String>,
    pub checks: Vec<GitOverlayHealthCheck>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub(crate) struct GitOverlayHealthCheck {
    pub name: String,
    pub status: String,
    pub summary: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub details: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub(crate) struct RepositoryVerificationState {
    #[serde(rename = "verified")]
    pub verified: bool,
    pub status: String,
    pub repository_mode: String,
    pub heddle_initialized: bool,
    pub git_branch: Option<String>,
    pub heddle_thread: Option<String>,
    pub worktree_dirty: bool,
    pub worktree_state: String,
    pub import_state: String,
    pub mapping_state: String,
    pub remote_drift: String,
    pub active_operation: Option<String>,
    pub default_remote: Option<String>,
    pub clone_verification: String,
    pub machine_contract: String,
    pub machine_contract_coverage: MachineContractCoverage,
    pub workflow_status: String,
    pub workflow_summary: String,
    pub summary: String,
    #[serde(serialize_with = "serialize_empty_action_as_null")]
    #[schemars(with = "Option<String>")]
    pub recommended_action: String,
    pub recommended_action_template: Option<ActionTemplate>,
    pub recovery_commands: Vec<String>,
    pub recovery_action_templates: Vec<ActionTemplate>,
    pub checks: Vec<VerificationCheck>,
}

pub(crate) fn serialize_empty_action_as_null<S>(
    action: &String,
    serializer: S,
) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    if action.is_empty() {
        serializer.serialize_none()
    } else {
        serializer.serialize_some(action)
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub(crate) struct MachineContractCoverage {
    pub status: String,
    #[serde(rename = "verified_scope")]
    pub verified_scope: String,
    pub advanced_scope: String,
    pub summary: String,
    pub catalog_commands_total: usize,
    pub catalog_mutating_commands_total: usize,
    pub json_commands_total: usize,
    pub json_mutating_commands_total: usize,
    pub json_commands_with_schema: usize,
    pub json_commands_with_accepted_opaque_schema: usize,
    pub json_commands_without_schema: usize,
    #[serde(rename = "verified_scope_json_commands_total")]
    pub verified_scope_json_commands_total: usize,
    #[serde(rename = "verified_scope_json_commands_with_schema")]
    pub verified_scope_json_commands_with_schema: usize,
    #[serde(rename = "verified_scope_json_commands_with_accepted_opaque_schema")]
    pub verified_scope_json_commands_with_accepted_opaque_schema: usize,
    #[serde(rename = "verified_scope_json_commands_without_schema")]
    pub verified_scope_json_commands_without_schema: usize,
    pub advanced_scope_json_commands_total: usize,
    pub advanced_scope_json_commands_with_accepted_opaque_schema: usize,
    pub mutating_commands_total: usize,
    pub mutating_commands_with_schema: usize,
    pub mutating_commands_with_accepted_opaque_schema: usize,
    pub mutating_commands_without_schema: usize,
    #[serde(rename = "verified_scope_mutating_commands_total")]
    pub verified_scope_mutating_commands_total: usize,
    #[serde(rename = "verified_scope_mutating_commands_with_schema")]
    pub verified_scope_mutating_commands_with_schema: usize,
    #[serde(rename = "verified_scope_mutating_commands_with_accepted_opaque_schema")]
    pub verified_scope_mutating_commands_with_accepted_opaque_schema: usize,
    #[serde(rename = "verified_scope_mutating_commands_without_schema")]
    pub verified_scope_mutating_commands_without_schema: usize,
    pub advanced_scope_mutating_commands_total: usize,
    pub advanced_scope_mutating_commands_with_accepted_opaque_schema: usize,
    pub schema_verbs_total: usize,
    pub documented_schema_verbs_total: usize,
    pub undocumented_schema_verbs_total: usize,
    pub opaque_schema_verbs_total: usize,
    pub accepted_opaque_schema_verbs_total: usize,
    pub unaccepted_opaque_schema_verbs_total: usize,
    pub supports_op_id_total: usize,
    pub jsonl_commands_total: usize,
    pub missing_schema_examples: Vec<String>,
    pub missing_mutating_schema_examples: Vec<String>,
    #[serde(rename = "verified_scope_missing_schema_examples")]
    pub verified_scope_missing_schema_examples: Vec<String>,
    #[serde(rename = "verified_scope_accepted_opaque_schema_examples")]
    pub verified_scope_accepted_opaque_schema_examples: Vec<String>,
    pub advanced_scope_accepted_opaque_schema_examples: Vec<String>,
    pub accepted_opaque_schema_examples: Vec<String>,
    pub unaccepted_opaque_schema_examples: Vec<String>,
    pub undocumented_schema_examples: Vec<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub(crate) struct VerificationCheck {
    pub name: String,
    pub status: String,
    pub clean: bool,
    pub summary: String,
    pub recommended_action: Option<String>,
    pub recommended_action_template: Option<ActionTemplate>,
    pub recovery_commands: Vec<String>,
    pub recovery_action_templates: Vec<ActionTemplate>,
    #[serde(default)]
    pub details: BTreeMap<String, String>,
}

#[derive(Debug)]
pub(crate) struct PlainGitVerificationProbe {
    pub root: PathBuf,
    pub git_branch: Option<String>,
    pub import_hint: Option<PlainGitImportHint>,
    pub changes: WorktreeStatus,
    pub trust: RepositoryVerificationState,
}

#[derive(Debug, Clone)]
pub(crate) struct PlainGitImportHint {
    pub current_branch: String,
    pub missing_branch_count: usize,
    pub missing_branches: Vec<String>,
    pub recommended_command: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RepositorySetupActionKind {
    Init,
    Adopt,
    BridgeImport,
    Other,
}

#[derive(Debug, Clone)]
pub(crate) struct RepositorySetupGuidance {
    pub setup_line: String,
    pub effect: String,
}

#[derive(Debug, Clone)]
struct WorkflowThreadAction {
    recommended_action: String,
    actionable_from_current_thread: bool,
}

#[derive(Debug, Clone)]
struct VerificationActionPlan {
    primary_action: String,
    recovery_commands: Vec<String>,
    remote_action: Option<String>,
    workflow_action: Option<String>,
    machine_contract_action: Option<String>,
}

impl GitOverlayHealth {
    pub(crate) fn clean(summary: impl Into<String>, checks: Vec<GitOverlayHealthCheck>) -> Self {
        Self {
            status: "clean".to_string(),
            clean: true,
            summary: summary.into(),
            recovery_commands: Vec::new(),
            checks,
        }
    }

    pub(crate) fn primary_recovery_command(&self) -> Option<&str> {
        self.recovery_commands.first().map(String::as_str)
    }
}

impl RepositoryVerificationState {
    pub(crate) fn from_health(repo: &Repository, health: GitOverlayHealth) -> Self {
        let git_branch = repo.git_overlay_current_branch().ok().flatten();
        let heddle_thread = repo.current_lane().ok().flatten();
        let active_operation = repo.operation_status().ok().flatten().map(|operation| {
            format!(
                "{} {} ({})",
                operation.scope, operation.kind, operation.state
            )
        });
        let remote_drift = repo
            .git_remote_tracking_status()
            .ok()
            .flatten()
            .map(|remote| remote_tracking_status(&remote).to_string())
            .unwrap_or_else(|| "clean".to_string());
        let import_state = health
            .checks
            .iter()
            .find(|check| {
                matches!(check.name.as_str(), "import" | "tag_mapping") && check.status != "clean"
            })
            .or_else(|| health.checks.iter().find(|check| check.name == "import"))
            .map(|check| check.status.clone())
            .unwrap_or_else(|| "clean".to_string());
        let mapping_state = health
            .checks
            .iter()
            .find(|check| {
                matches!(check.name.as_str(), "head_mapping" | "tag_mapping")
                    && git_overlay_mapping_status_blocks(&check.status)
            })
            .or_else(|| {
                health
                    .checks
                    .iter()
                    .find(|check| check.name == "head_mapping")
            })
            .map(|check| check.status.clone())
            .unwrap_or_else(|| "clean".to_string());
        let health_worktree_dirty = health.checks.iter().any(|check| {
            matches!(check.name.as_str(), "worktree" | "heddle_worktree") && check.status != "clean"
        });
        let git_worktree_status = repo.git_overlay_worktree_status().ok().flatten();
        let git_worktree_dirty = git_worktree_status
            .as_ref()
            .is_some_and(|status| !status.is_clean());
        let worktree_dirty = health_worktree_dirty || git_worktree_dirty;
        let ready_threads = ready_thread_actions(repo);
        let actionable_ready_threads = ready_threads
            .iter()
            .filter(|thread| thread.actionable_from_current_thread)
            .collect::<Vec<_>>();
        let workflow_action = actionable_ready_threads
            .first()
            .map(|thread| thread.recommended_action.clone());
        let machine_contract_coverage = machine_contract_coverage();
        let machine_contract_clean = machine_contract_is_clean(&machine_contract_coverage);
        let machine_contract_action =
            (!machine_contract_clean).then(|| "heddle doctor schemas --output json".to_string());
        let action_plan = VerificationActionPlan::from_parts(
            &health,
            remote_sync_action(&health),
            workflow_action,
            machine_contract_action,
        );
        let is_git_overlay = repo.capability() == repo::RepositoryCapability::GitOverlay;
        let checks = verification_checks_from_health(
            &health,
            &action_plan,
            is_git_overlay,
            &ready_threads,
            &machine_contract_coverage,
        );
        let clone_verification = if is_git_overlay {
            if health.clean {
                "verified"
            } else if clone_verification_waits_for_primary_blocker(&health) {
                "not_checked"
            } else {
                "blocked"
            }
        } else {
            "not_applicable"
        }
        .to_string();
        let workflow_status = if !health.clean && ready_threads.is_empty() {
            "not_checked".to_string()
        } else if !health.clean {
            "blocked".to_string()
        } else if !actionable_ready_threads.is_empty() {
            "ready".to_string()
        } else {
            "clean".to_string()
        };
        let workflow_summary = if !health.clean && ready_threads.is_empty() {
            "workflow readiness is checked after the primary verification blocker is resolved"
                .to_string()
        } else if !health.clean {
            format!(
                "{} ready thread(s) are waiting, but merge preview is blocked until repository verification is restored",
                ready_threads.len()
            )
        } else if !actionable_ready_threads.is_empty() {
            format!(
                "{} ready thread(s) are waiting for the next workflow action",
                actionable_ready_threads.len()
            )
        } else if !ready_threads.is_empty() {
            format!(
                "{} ready thread(s) target another thread; switch to the target thread or inspect `heddle thread list` before merging",
                ready_threads.len()
            )
        } else {
            "no ready threads are waiting to land".to_string()
        };
        let worktree_state = if health_worktree_dirty || git_worktree_dirty {
            "dirty"
        } else if !health.clean && find_health_check(&health, "worktree").is_none() {
            "not_checked"
        } else {
            "clean"
        }
        .to_string();
        let verified = health.clean && machine_contract_clean;
        let status = if health.clean && !machine_contract_clean {
            "machine_contract_gaps".to_string()
        } else {
            health.status.clone()
        };
        let summary = if health.clean && !machine_contract_clean {
            machine_contract_summary(&machine_contract_coverage)
        } else {
            health.summary.clone()
        };
        let recommended_action_fields = ActionFields::from_action(&action_plan.primary_action);
        Self {
            verified,
            status,
            repository_mode: repo.capability_label().to_string(),
            heddle_initialized: true,
            git_branch,
            heddle_thread,
            worktree_dirty,
            worktree_state,
            import_state,
            mapping_state,
            remote_drift,
            active_operation,
            default_remote: default_remote_name(repo),
            clone_verification,
            machine_contract: machine_contract_status(&machine_contract_coverage).to_string(),
            machine_contract_coverage,
            workflow_status,
            workflow_summary,
            summary,
            recommended_action_template: recommended_action_fields.template,
            recovery_action_templates: action_templates(&action_plan.recovery_commands),
            recommended_action: action_plan.primary_action,
            recovery_commands: action_plan.recovery_commands,
            checks,
        }
    }
}

impl VerificationActionPlan {
    fn from_parts(
        health: &GitOverlayHealth,
        remote_action: Option<String>,
        workflow_action: Option<String>,
        machine_contract_action: Option<String>,
    ) -> Self {
        if !health.clean {
            let primary_action = health
                .primary_recovery_command()
                .unwrap_or("heddle doctor")
                .to_string();
            return Self {
                primary_action,
                recovery_commands: health.recovery_commands.clone(),
                remote_action: None,
                workflow_action,
                machine_contract_action,
            };
        }

        if let Some(machine_contract_action) = machine_contract_action {
            return Self {
                primary_action: machine_contract_action.clone(),
                recovery_commands: vec![machine_contract_action.clone()],
                remote_action,
                workflow_action,
                machine_contract_action: Some(machine_contract_action),
            };
        }

        let primary_action = workflow_action
            .clone()
            .or_else(|| remote_action.clone())
            .unwrap_or_default();
        Self {
            primary_action,
            recovery_commands: Vec::new(),
            remote_action,
            workflow_action,
            machine_contract_action: None,
        }
    }

    fn blocking_action(&self) -> &str {
        &self.primary_action
    }
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

fn ready_thread_actions(repo: &Repository) -> Vec<WorkflowThreadAction> {
    let current_thread = repo.current_lane().ok().flatten();
    let opened_from_dedicated_checkout = repo
        .heddle_dir()
        .parent()
        .is_some_and(|main_root| main_root != repo.root());
    ThreadManager::new(repo.heddle_dir())
        .list()
        .map(|threads| {
            threads
                .into_iter()
                .filter(|thread| thread.state == ThreadState::Ready)
                .map(|mut thread| {
                    // Ready manifests can go stale when their target advances.
                    let _ = refresh_thread_freshness(repo, &mut thread);
                    let fallback = super::thread_landing::land_local_command(&thread.id);
                    let advice = describe_thread_advice(&thread, false, 0, false);
                    // A dedicated checkout can safely print a parent-repo
                    // merge command; the main checkout must be on the
                    // thread's recorded target before ready work becomes
                    // the primary next action.
                    let actionable_from_current_thread = thread
                        .target_thread
                        .as_deref()
                        .map(|target| {
                            current_thread.as_deref() == Some(target)
                                || opened_from_dedicated_checkout
                        })
                        .unwrap_or(true);
                    WorkflowThreadAction {
                        recommended_action: if advice.recommended_action.is_empty() {
                            fallback
                        } else {
                            advice.recommended_action
                        },
                        actionable_from_current_thread,
                    }
                })
                .collect()
        })
        .unwrap_or_default()
}

fn verification_checks_from_health(
    health: &GitOverlayHealth,
    action_plan: &VerificationActionPlan,
    is_git_overlay: bool,
    ready_threads: &[WorkflowThreadAction],
    machine_contract_coverage: &MachineContractCoverage,
) -> Vec<VerificationCheck> {
    vec![
        if is_git_overlay {
            verification_check(
                "Git",
                true,
                "clean",
                "Git overlay repository is present",
                None,
                Vec::new(),
            )
        } else {
            verification_check(
                "Git",
                true,
                "not_applicable",
                "Heddle-native repository is running in non-overlay mode",
                None,
                Vec::new(),
            )
        },
        verification_check(
            "Heddle",
            true,
            "clean",
            "Heddle data is initialized",
            None,
            Vec::new(),
        ),
        mapping_verification_check(health, action_plan.blocking_action(), is_git_overlay),
        worktree_verification_check(health, action_plan.blocking_action()),
        remote_verification_check(health, action_plan),
        operation_verification_check(health, action_plan.blocking_action()),
        workflow_verification_check(health, action_plan, ready_threads),
        machine_contract_verification_check(machine_contract_coverage, Some(action_plan)),
        clone_verification_check(health, action_plan.blocking_action(), is_git_overlay),
    ]
}

fn machine_contract_verification_check(
    coverage: &MachineContractCoverage,
    action_plan: Option<&VerificationActionPlan>,
) -> VerificationCheck {
    let mut details = BTreeMap::new();
    details.insert("coverage_status".to_string(), coverage.status.clone());
    details.insert("coverage_summary".to_string(), coverage.summary.clone());
    details.insert(
        "catalog_commands_total".to_string(),
        coverage.catalog_commands_total.to_string(),
    );
    details.insert(
        "catalog_mutating_commands_total".to_string(),
        coverage.catalog_mutating_commands_total.to_string(),
    );
    details.insert(
        "json_commands_total".to_string(),
        coverage.json_commands_total.to_string(),
    );
    details.insert(
        "json_mutating_commands_total".to_string(),
        coverage.json_mutating_commands_total.to_string(),
    );
    details.insert(
        "json_commands_with_schema".to_string(),
        coverage.json_commands_with_schema.to_string(),
    );
    details.insert(
        "json_commands_without_schema".to_string(),
        coverage.json_commands_without_schema.to_string(),
    );
    details.insert(
        "verified_scope".to_string(),
        coverage.verified_scope.clone(),
    );
    details.insert(
        "advanced_scope".to_string(),
        coverage.advanced_scope.clone(),
    );
    details.insert(
        "verified_scope_json_commands_total".to_string(),
        coverage.verified_scope_json_commands_total.to_string(),
    );
    details.insert(
        "verified_scope_json_commands_without_schema".to_string(),
        coverage
            .verified_scope_json_commands_without_schema
            .to_string(),
    );
    details.insert(
        "verified_scope_json_commands_with_accepted_opaque_schema".to_string(),
        coverage
            .verified_scope_json_commands_with_accepted_opaque_schema
            .to_string(),
    );
    details.insert(
        "advanced_scope_json_commands_with_accepted_opaque_schema".to_string(),
        coverage
            .advanced_scope_json_commands_with_accepted_opaque_schema
            .to_string(),
    );
    details.insert(
        "mutating_commands_without_schema".to_string(),
        coverage.mutating_commands_without_schema.to_string(),
    );
    details.insert(
        "schema_verbs_total".to_string(),
        coverage.schema_verbs_total.to_string(),
    );
    details.insert(
        "documented_schema_verbs_total".to_string(),
        coverage.documented_schema_verbs_total.to_string(),
    );
    details.insert(
        "undocumented_schema_verbs_total".to_string(),
        coverage.undocumented_schema_verbs_total.to_string(),
    );
    details.insert(
        "supports_op_id_total".to_string(),
        coverage.supports_op_id_total.to_string(),
    );
    if !coverage.missing_schema_examples.is_empty() {
        details.insert(
            "missing_schema_examples".to_string(),
            coverage.missing_schema_examples.join(", "),
        );
    }
    if !coverage.missing_mutating_schema_examples.is_empty() {
        details.insert(
            "missing_mutating_schema_examples".to_string(),
            coverage.missing_mutating_schema_examples.join(", "),
        );
    }
    if !coverage.verified_scope_missing_schema_examples.is_empty() {
        details.insert(
            "verified_scope_missing_schema_examples".to_string(),
            coverage.verified_scope_missing_schema_examples.join(", "),
        );
    }
    if !coverage
        .verified_scope_accepted_opaque_schema_examples
        .is_empty()
    {
        details.insert(
            "verified_scope_accepted_opaque_schema_examples".to_string(),
            coverage
                .verified_scope_accepted_opaque_schema_examples
                .join(", "),
        );
    }
    if !coverage
        .advanced_scope_accepted_opaque_schema_examples
        .is_empty()
    {
        details.insert(
            "advanced_scope_accepted_opaque_schema_examples".to_string(),
            coverage
                .advanced_scope_accepted_opaque_schema_examples
                .join(", "),
        );
    }
    if !coverage.undocumented_schema_examples.is_empty() {
        details.insert(
            "undocumented_schema_examples".to_string(),
            coverage.undocumented_schema_examples.join(", "),
        );
    }
    let mut check = verification_check(
        "Machine contract",
        machine_contract_is_clean(coverage),
        machine_contract_status(coverage),
        &machine_contract_summary(coverage),
        (!machine_contract_is_clean(coverage)).then(|| {
            action_plan
                .and_then(|plan| plan.machine_contract_action.clone())
                .unwrap_or_else(|| "heddle doctor schemas --output json".to_string())
        }),
        if machine_contract_is_clean(coverage) {
            Vec::new()
        } else {
            vec!["heddle doctor schemas --output json".to_string()]
        },
    );
    check.details = details;
    check
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

fn machine_contract_summary(coverage: &MachineContractCoverage) -> String {
    if coverage.json_commands_without_schema == 0
        && coverage.mutating_commands_without_schema == 0
        && coverage.unaccepted_opaque_schema_verbs_total == 0
        && coverage.undocumented_schema_verbs_total == 0
        && coverage.verified_scope_json_commands_with_accepted_opaque_schema == 0
    {
        if coverage.accepted_opaque_schema_verbs_total == 0 {
            "command catalog, JSON error envelopes, schemas, op-id metadata, and schema docs are available".to_string()
        } else {
            format!(
                "verified everyday/agent machine surface is fully concrete; advanced/internal/admin surfaces carry {} accepted opaque schema(s) outside clean verification",
                coverage.accepted_opaque_schema_verbs_total
            )
        }
    } else if coverage.json_commands_without_schema == 0
        && coverage.mutating_commands_without_schema == 0
        && coverage.unaccepted_opaque_schema_verbs_total == 0
        && coverage.undocumented_schema_verbs_total == 0
    {
        format!(
            "verified machine surface has {} opaque schema-backed JSON command(s); advanced/internal/admin scope carries {} accepted opaque schema(s)",
            coverage.verified_scope_json_commands_with_accepted_opaque_schema,
            coverage.accepted_opaque_schema_verbs_total
        )
    } else if coverage.json_commands_without_schema == 0
        && coverage.mutating_commands_without_schema == 0
        && coverage.unaccepted_opaque_schema_verbs_total == 0
    {
        format!(
            "runtime schemas are registered for JSON commands; {} schema verb(s) still need documented samples",
            coverage.undocumented_schema_verbs_total
        )
    } else {
        format!(
            "command catalog, JSON error envelopes, op-id metadata, and schema introspection are available; schema gaps are reported by `heddle doctor schemas` ({})",
            coverage.summary
        )
    }
}

fn workflow_verification_check(
    health: &GitOverlayHealth,
    action_plan: &VerificationActionPlan,
    ready_threads: &[WorkflowThreadAction],
) -> VerificationCheck {
    if let Some(check) = find_health_check(health, "thread_integration_metadata")
        && check.status != "clean"
    {
        return verification_check_from_health(
            "Workflow",
            check,
            action_plan.blocking_action(),
            health,
        );
    }
    if !health.clean {
        let summary = if ready_threads.is_empty() {
            "workflow readiness is checked after the primary verification blocker is resolved"
                .to_string()
        } else {
            format!(
                "{} ready thread(s) are waiting, but merge preview is blocked until repository verification is restored",
                ready_threads.len()
            )
        };
        return verification_check(
            "Workflow",
            false,
            "blocked",
            &summary,
            (!action_plan.blocking_action().is_empty())
                .then(|| action_plan.blocking_action().to_string()),
            health.recovery_commands.clone(),
        );
    }
    let actionable_ready_threads = ready_threads
        .iter()
        .filter(|thread| thread.actionable_from_current_thread)
        .collect::<Vec<_>>();
    if ready_threads.is_empty() {
        return verification_check(
            "Workflow",
            true,
            "clean",
            "no ready threads are waiting to land",
            None,
            Vec::new(),
        );
    }
    if actionable_ready_threads.is_empty() {
        return verification_check(
            "Workflow",
            true,
            "clean",
            &format!(
                "{} ready thread(s) target another thread; switch to the target thread or inspect `heddle thread list` before merging",
                ready_threads.len()
            ),
            None,
            Vec::new(),
        );
    }
    let action = actionable_ready_threads[0].recommended_action.clone();
    verification_check(
        "Workflow",
        true,
        "ready",
        &format!(
            "{} ready thread(s) are waiting for the next workflow action",
            actionable_ready_threads.len()
        ),
        action_plan.workflow_action.clone().or(Some(action)),
        Vec::new(),
    )
}

fn clone_verification_check(
    health: &GitOverlayHealth,
    recommended_action: &str,
    is_git_overlay: bool,
) -> VerificationCheck {
    if !is_git_overlay {
        return verification_check(
            "Clone",
            true,
            "not_applicable",
            "native Heddle state is the checkout authority",
            None,
            Vec::new(),
        );
    }
    if health.clean {
        verification_check(
            "Clone",
            true,
            "verified",
            "Git checkout and Heddle mapping agree",
            None,
            Vec::new(),
        )
    } else if clone_verification_waits_for_primary_blocker(health) {
        verification_check(
            "Clone",
            true,
            "not_checked",
            "clone verification waits for a clean worktree",
            None,
            Vec::new(),
        )
    } else {
        verification_check(
            "Clone",
            false,
            "blocked",
            "clone verification is blocked until verification checks agree",
            (!recommended_action.is_empty()).then(|| recommended_action.to_string()),
            health.recovery_commands.clone(),
        )
    }
}

fn clone_verification_waits_for_primary_blocker(health: &GitOverlayHealth) -> bool {
    matches!(
        health.status.as_str(),
        "dirty_worktree" | "needs_checkpoint"
    )
}

fn mapping_verification_check(
    health: &GitOverlayHealth,
    recommended_action: &str,
    is_git_overlay: bool,
) -> VerificationCheck {
    if !is_git_overlay {
        return verification_check(
            "Mapping",
            true,
            "not_applicable",
            "native Heddle refs do not require Git-overlay mapping",
            None,
            Vec::new(),
        );
    }
    if let Some(mapping) = find_health_check(health, "head_mapping")
        && git_overlay_mapping_status_blocks(&mapping.status)
    {
        return verification_check_from_health("Mapping", mapping, recommended_action, health);
    }
    if let Some(import) = find_health_check(health, "import")
        && import.status != "clean"
        && import.status != "available"
    {
        return verification_check_from_health("Mapping", import, recommended_action, health);
    }
    if let Some(tag_mapping) = find_health_check(health, "tag_mapping")
        && tag_mapping.status != "clean"
    {
        return verification_check_from_health("Mapping", tag_mapping, recommended_action, health);
    }
    if let Some(import) = find_health_check(health, "import") {
        if import.status == "available" {
            let mut check = verification_check(
                "Mapping",
                true,
                "available",
                &format!("Optional Git-only branch available: {}", import.summary),
                None,
                Vec::new(),
            );
            check.details = import.details.clone();
            return check;
        }
        return verification_check_from_health("Mapping", import, recommended_action, health);
    }
    if let Some(mapping) = find_health_check(health, "head_mapping") {
        return verification_check_from_health("Mapping", mapping, recommended_action, health);
    }
    verification_check(
        "Mapping",
        true,
        "clean",
        "Git branch tips map to imported Heddle state",
        None,
        Vec::new(),
    )
}

fn git_overlay_mapping_status_blocks(status: &str) -> bool {
    !matches!(status, "clean" | "git_backed")
}

fn worktree_verification_check(
    health: &GitOverlayHealth,
    recommended_action: &str,
) -> VerificationCheck {
    for name in ["worktree", "heddle_worktree"] {
        if let Some(check) = find_health_check(health, name)
            && check.status != "clean"
        {
            return verification_check_from_health("Worktree", check, recommended_action, health);
        }
    }
    if let Some(check) = find_health_check(health, "worktree") {
        return verification_check_from_health("Worktree", check, recommended_action, health);
    }
    if !health.clean {
        return verification_check(
            "Worktree",
            false,
            "not_checked",
            "worktree agreement is checked after the primary verification blocker is resolved",
            (!recommended_action.is_empty()).then(|| recommended_action.to_string()),
            health.recovery_commands.clone(),
        );
    }
    verification_check(
        "Worktree",
        true,
        "clean",
        "worktree has no uncommitted Git/Heddle disagreement",
        None,
        Vec::new(),
    )
}

fn remote_verification_check(
    health: &GitOverlayHealth,
    action_plan: &VerificationActionPlan,
) -> VerificationCheck {
    if let Some(check) = find_health_check(health, "remote_tracking") {
        if check.status == "remote_ahead" {
            let mut trust = verification_check(
                "Remote",
                true,
                &check.status,
                &check.summary,
                action_plan.remote_action.clone(),
                Vec::new(),
            );
            trust.details = check.details.clone();
            return trust;
        }
        if check.status == "remote_untracked" {
            let mut trust = verification_check(
                "Remote",
                true,
                &check.status,
                &check.summary,
                action_plan.remote_action.clone(),
                Vec::new(),
            );
            trust.details = check.details.clone();
            return trust;
        }
        return verification_check_from_health(
            "Remote",
            check,
            action_plan.blocking_action(),
            health,
        );
    }
    if !health.clean {
        return verification_check(
            "Remote",
            false,
            "not_checked",
            "remote drift is checked after the primary verification blocker is resolved",
            (!action_plan.blocking_action().is_empty())
                .then(|| action_plan.blocking_action().to_string()),
            health.recovery_commands.clone(),
        );
    }
    verification_check(
        "Remote",
        true,
        "clean",
        "no unresolved remote drift detected",
        None,
        Vec::new(),
    )
}

fn operation_verification_check(
    health: &GitOverlayHealth,
    recommended_action: &str,
) -> VerificationCheck {
    if let Some(check) = find_health_check(health, "operation") {
        return verification_check_from_health("Operation", check, recommended_action, health);
    }
    verification_check(
        "Operation",
        true,
        "clean",
        "no repository operation in progress",
        None,
        Vec::new(),
    )
}

fn verification_check_from_health(
    public_name: &str,
    check: &GitOverlayHealthCheck,
    recommended_action: &str,
    health: &GitOverlayHealth,
) -> VerificationCheck {
    let clean = check.status == "clean";
    let mut trust = verification_check(
        public_name,
        clean,
        &check.status,
        &check.summary,
        (!clean && !recommended_action.is_empty()).then(|| recommended_action.to_string()),
        if clean {
            Vec::new()
        } else {
            health.recovery_commands.clone()
        },
    );
    trust.details = check.details.clone();
    trust
}

fn find_health_check<'a>(
    health: &'a GitOverlayHealth,
    name: &str,
) -> Option<&'a GitOverlayHealthCheck> {
    health.checks.iter().find(|check| check.name == name)
}

fn head_mapping_is_git_backed(checks: &[GitOverlayHealthCheck]) -> bool {
    checks
        .iter()
        .any(|check| check.name == "head_mapping" && check.status == "git_backed")
}

fn verification_check(
    name: &str,
    clean: bool,
    status: &str,
    summary: &str,
    recommended_action: Option<String>,
    recovery_commands: Vec<String>,
) -> VerificationCheck {
    let action = ActionFields::from_optional_action_ref(recommended_action.as_deref());
    let recovery_action_templates = action_templates(&recovery_commands);
    VerificationCheck {
        name: name.to_string(),
        status: status.to_string(),
        clean,
        summary: summary.to_string(),
        recommended_action,
        recommended_action_template: action.template,
        recovery_commands,
        recovery_action_templates,
        details: BTreeMap::new(),
    }
}

pub(crate) fn build_repository_verification_state(
    repo: &Repository,
) -> RepositoryVerificationState {
    let health = build_git_overlay_health(repo);
    RepositoryVerificationState::from_health(repo, health)
}

pub(crate) fn unimported_git_history_advice(
    repo: &Repository,
    action: &str,
) -> anyhow::Result<Option<RecoveryAdvice>> {
    if repo.capability() != repo::RepositoryCapability::GitOverlay {
        return Ok(None);
    }

    let Some(hint) = repo.git_overlay_import_hint()? else {
        return Ok(None);
    };
    if !import_hint_includes_active_branch(&hint) {
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
    let primary_command = "heddle bridge git status".to_string();
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
    let trust = build_repository_verification_state(repo);
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
    if preflight.check_verification
        && let Some(advice) = verification_blocking_mutation_advice(repo, action)?
    {
        return Ok(Some(advice));
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
        RepositorySetupActionKind::BridgeImport => {
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
        RepositorySetupActionKind::BridgeImport => {
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
    } else if action.starts_with("heddle bridge git import") {
        RepositorySetupActionKind::BridgeImport
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

fn detached_head_recovery_commands(repo: &Repository) -> Vec<String> {
    vec![detached_head_primary_recovery(repo)]
}

fn heddle_worktree_is_clean(repo: &Repository) -> bool {
    let Ok(Some(state)) = repo.current_state() else {
        return false;
    };
    let Ok(tree) = repo.require_tree(&state.tree) else {
        return false;
    };
    repo.compare_worktree_cached_with_options(&tree, &worktree_status_options(Some(repo.config())))
        .map(|status| status.is_clean())
        .unwrap_or(false)
}

fn dirty_details(status: &WorktreeStatus) -> BTreeMap<String, String> {
    let mut paths = Vec::new();
    paths.extend(
        status
            .modified
            .iter()
            .map(|path| path.display().to_string()),
    );
    paths.extend(status.added.iter().map(|path| path.display().to_string()));
    paths.extend(status.deleted.iter().map(|path| path.display().to_string()));

    let mut details = BTreeMap::new();
    details.insert("dirty_path_count".to_string(), paths.len().to_string());
    if !paths.is_empty() {
        details.insert("dirty_paths".to_string(), paths.join(", "));
    }
    details
}

pub(crate) fn build_plain_git_verification_probe(
    start: &Path,
) -> anyhow::Result<Option<PlainGitVerificationProbe>> {
    let git_repo = match SleyRepository::discover(start) {
        Ok(repo) => repo,
        Err(_) => return Ok(None),
    };
    let Some(workdir) = git_repo.workdir() else {
        return Ok(None);
    };
    let root = workdir
        .canonicalize()
        .unwrap_or_else(|_| workdir.to_path_buf());
    if root.join(".heddle").exists() {
        return Ok(None);
    }

    let git_branch = plain_git_current_branch(&git_repo);
    let git_branches = plain_git_local_branches(&git_repo);
    let git_tags = plain_git_local_tags(&git_repo);
    let changes = plain_git_worktree_status(&root, &git_repo)?;

    let default_remote = git_default_remote_name_from_repo(&git_repo);
    let init = "heddle init".to_string();
    let setup_action = init.clone();
    let setup_recovery_commands = vec![init.clone()];
    let import_hint = None;
    let machine_contract_coverage = machine_contract_coverage();
    let machine_contract_clean = machine_contract_is_clean(&machine_contract_coverage);
    let action_plan = VerificationActionPlan {
        primary_action: setup_action.clone(),
        recovery_commands: setup_recovery_commands.clone(),
        remote_action: None,
        workflow_action: None,
        machine_contract_action: (!machine_contract_clean)
            .then(|| "heddle doctor schemas --output json".to_string()),
    };
    let mut details = BTreeMap::new();
    details.insert("path".to_string(), root.display().to_string());
    if let Some(branch) = &git_branch {
        details.insert("git_branch".to_string(), branch.clone());
    }
    if let Some(remote) = &default_remote {
        details.insert("default_remote".to_string(), remote.clone());
    }
    details.insert(
        "git_branch_count".to_string(),
        git_branches.len().to_string(),
    );
    details.insert("git_tag_count".to_string(), git_tags.len().to_string());
    let setup_action_fields = ActionFields::from_action(&setup_action);
    let mut checks = vec![
        VerificationCheck {
            name: "Git".to_string(),
            status: "present".to_string(),
            clean: true,
            summary: "plain Git repository found".to_string(),
            recommended_action: None,
            recommended_action_template: None,
            recovery_commands: Vec::new(),
            recovery_action_templates: Vec::new(),
            details,
        },
        VerificationCheck {
            name: "Heddle".to_string(),
            status: "needs_init".to_string(),
            clean: false,
            summary: "Heddle data is not initialized".to_string(),
            recommended_action: Some(setup_action.clone()),
            recommended_action_template: setup_action_fields.template.clone(),
            recovery_commands: setup_recovery_commands.clone(),
            recovery_action_templates: action_templates(&setup_recovery_commands),
            details: BTreeMap::new(),
        },
        VerificationCheck {
            name: "Mapping".to_string(),
            status: "git_backed".to_string(),
            clean: true,
            summary: "Git refs will stay in Git storage after Heddle initialization".to_string(),
            recommended_action: None,
            recommended_action_template: None,
            recovery_commands: Vec::new(),
            recovery_action_templates: Vec::new(),
            details: BTreeMap::new(),
        },
    ];
    checks.push(verification_check(
        "Worktree",
        changes.is_clean(),
        if changes.is_clean() {
            "clean"
        } else {
            "dirty_worktree"
        },
        if changes.is_clean() {
            "Git worktree is clean"
        } else {
            "Git worktree has uncommitted changes"
        },
        None,
        Vec::new(),
    ));
    checks.push(verification_check(
        "Remote",
        false,
        "unknown",
        "remote drift is checked after Heddle initialization",
        None,
        Vec::new(),
    ));
    checks.push(verification_check(
        "Operation",
        true,
        "clean",
        "no Heddle operation in progress",
        None,
        Vec::new(),
    ));
    checks.push(verification_check(
        "Workflow",
        false,
        "not_checked",
        "workflow readiness is checked after Heddle initialization",
        None,
        Vec::new(),
    ));
    checks.push(machine_contract_verification_check(
        &machine_contract_coverage,
        Some(&action_plan),
    ));
    checks.push(verification_check(
        "Clone",
        true,
        "not_applicable",
        "clone verification is not applicable to this checkout",
        None,
        Vec::new(),
    ));
    let setup_action_fields = ActionFields::from_action(&setup_action);
    let trust = RepositoryVerificationState {
        verified: false,
        status: "needs_init".to_string(),
        repository_mode: "plain-git".to_string(),
        heddle_initialized: false,
        git_branch: git_branch.clone(),
        heddle_thread: None,
        worktree_dirty: !changes.is_clean(),
        worktree_state: if changes.is_clean() { "clean" } else { "dirty" }.to_string(),
        import_state: "git_backed".to_string(),
        mapping_state: "git_backed".to_string(),
        remote_drift: "unknown".to_string(),
        active_operation: None,
        default_remote,
        clone_verification: "not_applicable".to_string(),
        machine_contract: machine_contract_status(&machine_contract_coverage).to_string(),
        machine_contract_coverage,
        workflow_status: "not_checked".to_string(),
        workflow_summary: "workflow readiness is checked after Heddle initialization".to_string(),
        summary: "Git repository has not been initialized for Heddle".to_string(),
        recommended_action_template: setup_action_fields.template,
        recovery_action_templates: action_templates(&setup_recovery_commands),
        recommended_action: setup_action,
        recovery_commands: setup_recovery_commands,
        checks,
    };
    Ok(Some(PlainGitVerificationProbe {
        root,
        git_branch,
        import_hint,
        changes,
        trust,
    }))
}

fn plain_git_current_branch(git_repo: &SleyRepository) -> Option<String> {
    git_repo.head().ok()?.branch_name().map(str::to_string)
}

fn plain_git_local_branches(git_repo: &SleyRepository) -> Vec<String> {
    let Ok(branches) = git_repo.references().list_refs() else {
        return Vec::new();
    };
    let mut names = branches
        .into_iter()
        .filter_map(|branch| branch.name.strip_prefix("refs/heads/").map(str::to_string))
        .filter(|branch| !branch.trim().is_empty())
        .collect::<Vec<_>>();
    names.sort();
    names.dedup();
    names
}

fn plain_git_local_tags(git_repo: &SleyRepository) -> Vec<String> {
    let Ok(tags) = git_repo.references().list_refs() else {
        return Vec::new();
    };
    let mut names = tags
        .into_iter()
        .filter_map(|tag| tag.name.strip_prefix("refs/tags/").map(str::to_string))
        .filter(|tag| !tag.trim().is_empty())
        .collect::<Vec<_>>();
    names.sort();
    names.dedup();
    names
}

fn plain_git_worktree_status(
    root: &Path,
    git_repo: &SleyRepository,
) -> anyhow::Result<WorktreeStatus> {
    let index = plain_git_index_or_empty(git_repo).map_err(|error| {
        anyhow::anyhow!(
            "failed to inspect Git index at '{}': {error}",
            root.display()
        )
    })?;
    let head_index = plain_git_head_index_or_empty(git_repo).map_err(|error| {
        anyhow::anyhow!(
            "failed to inspect Git HEAD tree at '{}': {error}",
            root.display()
        )
    })?;

    let mut head_entries = BTreeMap::new();
    for entry in &head_index.entries {
        head_entries.insert(plain_git_path(&entry.path), (entry.oid, entry.mode));
    }
    let mut index_entries = BTreeMap::new();
    for entry in &index.entries {
        index_entries.insert(plain_git_path(&entry.path), (entry.oid, entry.mode));
    }

    let mut added = BTreeSet::new();
    let mut modified = BTreeSet::new();
    let mut deleted = BTreeSet::new();

    for (path, (oid, mode)) in &index_entries {
        match head_entries.get(path) {
            None => {
                added.insert(PathBuf::from(path));
            }
            Some((head_oid, head_mode)) if (head_oid, head_mode) != (oid, mode) => {
                modified.insert(PathBuf::from(path));
            }
            Some(_) => {}
        }
    }
    for path in head_entries.keys() {
        if !index_entries.contains_key(path) {
            deleted.insert(PathBuf::from(path));
        }
    }

    for (path, (oid, mode)) in &index_entries {
        match repo::git_worktree_status::git_worktree_entry_state(root, path, *oid, *mode, None)? {
            GitWorktreeEntryState::Clean => {}
            GitWorktreeEntryState::Deleted => {
                deleted.insert(PathBuf::from(path));
            }
            GitWorktreeEntryState::Modified => {
                modified.insert(PathBuf::from(path));
            }
        }
    }

    let tracked_paths: BTreeSet<&str> = index_entries.keys().map(String::as_str).collect();
    for path in plain_git_untracked_paths(root, &tracked_paths)? {
        added.insert(PathBuf::from(path));
    }

    // `git rm --cached path` produces both an index-vs-HEAD deletion
    // and a fresh untracked entry for the same path; both signals are
    // load-bearing for `heddle status`/`diff`/verification. Only
    // suppress `modified` duplicates here — `added` and `deleted` for
    // the same path are two different views (worktree-vs-index and
    // index-vs-HEAD) of the same intentional change.
    for path in &added {
        modified.remove(path);
    }
    for path in &deleted {
        modified.remove(path);
    }

    Ok(WorktreeStatus {
        modified: modified.into_iter().collect(),
        added: added.into_iter().collect(),
        deleted: deleted.into_iter().collect(),
    })
}

fn plain_git_untracked_paths(
    root: &Path,
    tracked_paths: &BTreeSet<&str>,
) -> anyhow::Result<Vec<String>> {
    let mut paths = Vec::new();
    let walker = ignore::WalkBuilder::new(root)
        .hidden(false)
        .git_ignore(true)
        .git_exclude(true)
        .git_global(true)
        .filter_entry(|entry| !plain_git_excluded_walk_entry(entry.path()))
        .build();
    for entry in walker {
        let entry = entry?;
        let file_type = entry.file_type();
        if !file_type.is_some_and(|file_type| file_type.is_file() || file_type.is_symlink()) {
            continue;
        }
        let path = plain_git_repo_relative_path(root, entry.path())?;
        if !tracked_paths.contains(path.as_str()) {
            paths.push(path);
        }
    }
    paths.sort();
    paths.dedup();
    Ok(paths)
}

fn plain_git_excluded_walk_entry(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name == ".git" || name == ".heddle")
}

fn plain_git_repo_relative_path(root: &Path, path: &Path) -> anyhow::Result<String> {
    let relative = path.strip_prefix(root).map_err(|error| {
        anyhow::anyhow!(
            "failed to relativize Git worktree path '{}': {}",
            path.display(),
            error
        )
    })?;
    Ok(path_to_plain_git_path(relative))
}

fn path_to_plain_git_path(path: &Path) -> String {
    path.components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

fn plain_git_empty_index() -> Index {
    Index {
        version: 2,
        entries: Vec::new(),
        extensions: Vec::new(),
        checksum: None,
    }
}

fn plain_git_index_or_empty(
    git_repo: &SleyRepository,
) -> std::result::Result<Index, sley::GitError> {
    git_repo
        .open_index()
        .map(|index| index.unwrap_or_else(plain_git_empty_index))
}

fn plain_git_head_index_or_empty(
    git_repo: &SleyRepository,
) -> std::result::Result<Index, sley::GitError> {
    let head = git_repo.head()?;
    let Some(oid) = head.oid else {
        return Ok(plain_git_empty_index());
    };
    let commit = git_repo.read_commit(&oid)?;
    git_repo.index_from_tree(&commit.tree)
}

fn plain_git_path(path: &GitBString) -> String {
    String::from_utf8_lossy(path.as_bytes()).into_owned()
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

fn remote_sync_action(health: &GitOverlayHealth) -> Option<String> {
    find_health_check(health, "remote_tracking").and_then(|check| {
        matches!(check.status.as_str(), "remote_ahead" | "remote_untracked")
            .then(|| "heddle push".to_string())
    })
}

pub(crate) fn remote_tracking_status(remote: &GitRemoteTrackingStatus) -> &'static str {
    if remote.upstream.is_empty() {
        return "remote_untracked";
    }
    if remote.upstream_is_undone_checkpoint && remote.ahead == 0 && remote.behind > 0 {
        return "remote_contains_undone_checkpoint";
    }
    match (remote.ahead, remote.behind) {
        (0, 0) => "clean",
        (0, _) => "remote_behind",
        (_, 0) => "remote_ahead",
        _ => "remote_diverged",
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RemoteDriftDecision {
    pub status: &'static str,
    pub verified_as_clean: bool,
    pub primary_action: Option<String>,
    pub recovery_commands: Vec<String>,
    pub requires_clean_worktree: bool,
}

pub(crate) fn remote_tracking_next_action(remote: &GitRemoteTrackingStatus) -> Option<String> {
    match remote_tracking_status(remote) {
        "clean" => None,
        "remote_untracked" => Some(remote_untracked_action(remote)),
        "remote_contains_undone_checkpoint" => Some(heddle_action(["push", "--force"])),
        "remote_behind" => Some("heddle pull".to_string()),
        "remote_ahead" => Some("heddle push".to_string()),
        "remote_diverged" => {
            let upstream = remote.upstream.trim();
            if upstream.is_empty() {
                Some("heddle fetch".to_string())
            } else {
                Some(canonical_bridge_import_ref_command(upstream))
            }
        }
        _ => None,
    }
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
            primary_action: Some(remote_untracked_action(remote)),
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
            primary_action: Some(heddle_action(["push", "--force"])),
            recovery_commands: vec![
                heddle_action(["push", "--force"]),
                heddle_action(["undo", "--redo"]),
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
            let import = canonical_bridge_import_ref_command(upstream);
            let reconcile = canonical_bridge_reconcile_ref_preview_command(None, upstream);
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

fn remote_untracked_action(remote: &GitRemoteTrackingStatus) -> String {
    if remote.next_action.trim().is_empty() {
        "heddle push".to_string()
    } else {
        remote.next_action.clone()
    }
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

fn default_remote_name(repo: &Repository) -> Option<String> {
    RemoteConfig::open(repo)
        .ok()
        .and_then(|cfg| cfg.default_name().map(str::to_string))
        .or_else(|| {
            (repo.capability() == repo::RepositoryCapability::GitOverlay)
                .then(|| git_default_remote_name(repo.root()))
                .flatten()
        })
}

fn git_default_remote_name(root: &Path) -> Option<String> {
    let repo = SleyRepository::discover(root).ok()?;
    git_default_remote_name_from_repo(&repo)
}

fn git_default_remote_name_from_repo(repo: &SleyRepository) -> Option<String> {
    repo.remote_names()
        .ok()?
        .into_iter()
        .find(|name| name == "origin")
}

pub(crate) fn build_git_overlay_health(repo: &Repository) -> GitOverlayHealth {
    build_git_overlay_health_inner(repo, None)
}

/// `status` hot-path variant: reuse the caller's already-computed git-overlay
/// worktree status instead of recomputing it. `git_overlay_worktree_status`
/// re-reads + SHA-1s every tracked file (~950ms on a 10k-file worktree); the
/// `status` command otherwise pays it twice (here and in `build_status_output`).
/// `worktree_status` is the exact `Result` from `git_overlay_worktree_status()`,
/// so the clean/dirty/degraded classification stays byte-identical.
pub(crate) fn build_git_overlay_health_with_worktree_status(
    repo: &Repository,
    worktree_status: &repo::Result<Option<WorktreeStatus>>,
) -> GitOverlayHealth {
    build_git_overlay_health_inner(repo, Some(worktree_status))
}

fn build_git_overlay_health_inner(
    repo: &Repository,
    precomputed_worktree_status: Option<&repo::Result<Option<WorktreeStatus>>>,
) -> GitOverlayHealth {
    if repo.capability() != repo::RepositoryCapability::GitOverlay {
        return build_native_heddle_health(repo);
    }
    if repo.root().join(".heddle/objectstore").is_file() && !repo.root().join(".git").exists() {
        return GitOverlayHealth::clean(
            "Heddle-managed isolated checkout; Git verification belongs to the parent checkout",
            vec![GitOverlayHealthCheck {
                name: "worktree".to_string(),
                status: "clean".to_string(),
                summary: "No .git directory is present in this isolated checkout".to_string(),
                details: BTreeMap::new(),
            }],
        );
    }

    let mut checks = Vec::new();

    match repo.operation_status() {
        Ok(Some(operation)) => {
            checks.push(GitOverlayHealthCheck {
                name: "operation".to_string(),
                status: "operation_in_progress".to_string(),
                summary: operation.message.clone(),
                details: BTreeMap::new(),
            });
            return GitOverlayHealth {
                status: "operation_in_progress".to_string(),
                clean: false,
                summary: operation.message,
                recovery_commands: vec![operation.next_action],
                checks,
            };
        }
        Ok(None) => checks.push(GitOverlayHealthCheck {
            name: "operation".to_string(),
            status: "clean".to_string(),
            summary: "no Git or Heddle operation in progress".to_string(),
            details: BTreeMap::new(),
        }),
        Err(error) => {
            checks.push(GitOverlayHealthCheck {
                name: "operation".to_string(),
                status: "degraded".to_string(),
                summary: error.to_string(),
                details: BTreeMap::new(),
            });
            return degraded(checks, "Could not inspect in-progress operations");
        }
    }

    match repo.git_overlay_head_is_detached() {
        Ok(true) => {
            let mut details = BTreeMap::new();
            if let Ok(Some(commit)) = repo.git_overlay_detached_head_commit() {
                details.insert("git_commit".to_string(), commit);
            }
            checks.push(GitOverlayHealthCheck {
                name: "head_mapping".to_string(),
                status: "detached_head".to_string(),
                summary: "Git HEAD is detached; attach a branch before mutating this Git overlay"
                    .to_string(),
                details,
            });
            return GitOverlayHealth {
                status: "detached_head".to_string(),
                clean: false,
                summary: "Git HEAD is detached; attach a branch before mutating this Git overlay"
                    .to_string(),
                recovery_commands: detached_head_recovery_commands(repo),
                checks,
            };
        }
        Ok(false) => {}
        Err(error) => {
            checks.push(GitOverlayHealthCheck {
                name: "head_mapping".to_string(),
                status: "degraded".to_string(),
                summary: error.to_string(),
                details: BTreeMap::new(),
            });
            return degraded(checks, "Could not inspect Git HEAD state");
        }
    }

    let import_hint = match repo.git_overlay_import_hint() {
        Ok(hint) => hint,
        Err(error) => {
            checks.push(GitOverlayHealthCheck {
                name: "import".to_string(),
                status: "degraded".to_string(),
                summary: error.to_string(),
                details: BTreeMap::new(),
            });
            return degraded(checks, "Could not inspect Git import state");
        }
    };

    match current_branch_tip(repo) {
        Ok(Some(tip))
            if !tip.history_imported
                && repo.current_state().ok().flatten().is_some()
                && import_hint
                    .as_ref()
                    .is_some_and(import_hint_includes_active_branch) =>
        {
            let out_of_band = repo
                .git_overlay_out_of_band_commits(&tip.git_commit)
                .ok()
                .flatten();
            let out_of_band_clause = out_of_band_commit_clause(out_of_band.as_ref());
            let mut details = BTreeMap::new();
            details.insert("git_branch".to_string(), tip.branch.clone());
            details.insert("git_commit".to_string(), tip.git_commit.clone());
            if let Some(out_of_band) = &out_of_band {
                details.insert(
                    "out_of_band_commit_count".to_string(),
                    out_of_band.count.to_string(),
                );
                if out_of_band.truncated {
                    details.insert(
                        "out_of_band_commit_count_truncated".to_string(),
                        "true".to_string(),
                    );
                }
            }
            checks.push(GitOverlayHealthCheck {
                name: "head_mapping".to_string(),
                status: "git_branch_advanced".to_string(),
                summary: format!(
                    "Git branch '{}' advanced to commit {} outside Heddle{}",
                    tip.branch, tip.git_commit, out_of_band_clause
                ),
                details,
            });
            if let Some(hint) = &import_hint
                && import_hint_includes_active_branch(hint)
            {
                checks.push(GitOverlayHealthCheck {
                    name: "import".to_string(),
                    status: "needs_import".to_string(),
                    summary: format!(
                        "{} Git branch tip(s) still need Heddle import",
                        hint.missing_branch_count
                    ),
                    details: BTreeMap::new(),
                });
            }
            return GitOverlayHealth {
                status: "git_branch_advanced".to_string(),
                clean: false,
                summary: format!(
                    "Git branch '{}' advanced outside Heddle{}; import the new Git tip to restore the mapping",
                    tip.branch, out_of_band_clause
                ),
                recovery_commands: vec![canonical_adopt_ref_command(&tip.branch)],
                checks,
            };
        }
        Ok(Some(tip)) if !tip.history_imported => {
            let mut details = BTreeMap::new();
            details.insert("git_branch".to_string(), tip.branch.clone());
            details.insert("git_commit".to_string(), tip.git_commit.clone());
            checks.push(GitOverlayHealthCheck {
                name: "head_mapping".to_string(),
                status: "git_backed".to_string(),
                summary: format!(
                    "Git branch '{}' resolves directly to Git commit {}",
                    tip.branch,
                    short_oid(&tip.git_commit)
                ),
                details,
            });
        }
        Ok(Some(tip)) => checks.push(GitOverlayHealthCheck {
            name: "head_mapping".to_string(),
            status: "clean".to_string(),
            summary: format!("Git branch '{}' maps to imported Heddle state", tip.branch),
            details: BTreeMap::new(),
        }),
        Ok(None) => checks.push(GitOverlayHealthCheck {
            name: "head_mapping".to_string(),
            status: "clean".to_string(),
            summary: "No attached Git branch to map".to_string(),
            details: BTreeMap::new(),
        }),
        Err(error) => {
            checks.push(GitOverlayHealthCheck {
                name: "head_mapping".to_string(),
                status: "degraded".to_string(),
                summary: error.to_string(),
                details: BTreeMap::new(),
            });
            return degraded(checks, "Could not inspect Git/Heddle branch mapping");
        }
    }

    match import_hint {
        Some(hint) if import_hint_includes_active_branch(&hint) => {
            return needs_import(checks, hint);
        }
        Some(hint) => checks.push(GitOverlayHealthCheck {
            name: "import".to_string(),
            status: "available".to_string(),
            summary: format!(
                "{} other Git branch tip(s) are available to import",
                hint.missing_branch_count
            ),
            details: BTreeMap::new(),
        }),
        None => checks.push(GitOverlayHealthCheck {
            name: "import".to_string(),
            status: "clean".to_string(),
            summary: "Git refs are read directly from Git storage".to_string(),
            details: BTreeMap::new(),
        }),
    }

    // Reuse the caller's computation on the `status` path; fall back to a fresh
    // probe (only reached here, after the early returns above) otherwise.
    let computed_worktree_status;
    let worktree_status = match precomputed_worktree_status {
        Some(status) => status,
        None => {
            computed_worktree_status = repo.git_overlay_worktree_status();
            &computed_worktree_status
        }
    };
    match worktree_status {
        Ok(Some(status)) if !status.is_clean() => {
            let changed = status.modified.len() + status.added.len() + status.deleted.len();
            if heddle_worktree_is_clean(repo) {
                checks.push(GitOverlayHealthCheck {
                    name: "worktree".to_string(),
                    status: "needs_checkpoint".to_string(),
                    summary: format!(
                        "{changed} Git worktree path(s) are captured in Heddle but not checkpointed to Git"
                    ),
                    details: dirty_details(status),
                });
                return GitOverlayHealth {
                    status: "needs_checkpoint".to_string(),
                    clean: false,
                    summary: format!(
                        "{changed} Git worktree path(s) are captured in Heddle but not checkpointed to Git"
                    ),
                    recovery_commands: vec!["heddle checkpoint -m \"...\"".to_string()],
                    checks,
                };
            }
            checks.push(GitOverlayHealthCheck {
                name: "worktree".to_string(),
                status: "dirty_worktree".to_string(),
                summary: format!("{changed} Git worktree path(s) have uncommitted changes"),
                details: dirty_details(status),
            });
            return GitOverlayHealth {
                status: "dirty_worktree".to_string(),
                clean: false,
                summary: format!("{changed} Git worktree path(s) have uncommitted changes"),
                recovery_commands: vec![
                    "heddle commit -m \"...\"".to_string(),
                    "heddle capture -m \"...\"".to_string(),
                    "heddle stash push -m \"...\"".to_string(),
                ],
                checks,
            };
        }
        Ok(Some(_)) => checks.push(GitOverlayHealthCheck {
            name: "worktree".to_string(),
            status: "clean".to_string(),
            summary: "Git worktree is clean".to_string(),
            details: BTreeMap::new(),
        }),
        Ok(None) => checks.push(GitOverlayHealthCheck {
            name: "worktree".to_string(),
            status: "clean".to_string(),
            summary: "Git worktree status is not available; Heddle status remains authoritative"
                .to_string(),
            details: BTreeMap::new(),
        }),
        Err(error) => {
            checks.push(GitOverlayHealthCheck {
                name: "worktree".to_string(),
                status: "degraded".to_string(),
                summary: error.to_string(),
                details: BTreeMap::new(),
            });
            return degraded(checks, "Could not inspect Git worktree status");
        }
    }

    match clean_git_branch_reconcile_check(repo) {
        Ok(Some(check)) => {
            let status = check.status.clone();
            let summary = check.summary.clone();
            let ref_name = check
                .details
                .get("git_branch")
                .cloned()
                .unwrap_or_else(|| "<branch>".to_string());
            let recovery = if status == "needs_checkpoint" {
                "heddle checkpoint -m \"...\"".to_string()
            } else {
                canonical_bridge_reconcile_ref_preview_command(None, &ref_name)
            };
            checks.push(check);
            return GitOverlayHealth {
                status,
                clean: false,
                summary,
                recovery_commands: vec![recovery],
                checks,
            };
        }
        Ok(None) => {}
        Err(error) => {
            checks.push(GitOverlayHealthCheck {
                name: "head_mapping".to_string(),
                status: "degraded".to_string(),
                summary: error.to_string(),
                details: BTreeMap::new(),
            });
            return degraded(checks, "Could not inspect Git/Heddle branch agreement");
        }
    }

    if !head_mapping_is_git_backed(&checks)
        && let Ok(Some(state)) = repo.current_state()
        && let Ok(tree) = repo.require_tree(&state.tree)
        && let Ok(status) = repo.compare_worktree_cached_with_options(
            &tree,
            &worktree_status_options(Some(repo.config())),
        )
        && !status.is_clean()
    {
        let changed = status.modified.len() + status.added.len() + status.deleted.len();
        checks.push(GitOverlayHealthCheck {
            name: "heddle_worktree".to_string(),
            status: "dirty_worktree".to_string(),
            summary: format!("{changed} Heddle worktree path(s) differ from the current state"),
            details: dirty_details(&status),
        });
        return GitOverlayHealth {
            status: "dirty_worktree".to_string(),
            clean: false,
            summary: format!("{changed} Heddle worktree path(s) differ from the current state"),
            recovery_commands: vec![
                "heddle commit -m \"...\"".to_string(),
                "heddle capture -m \"...\"".to_string(),
                "heddle stash push -m \"...\"".to_string(),
            ],
            checks,
        };
    }

    match tag_mapping_check(repo) {
        Ok(Some(check)) => {
            let status = check.status.clone();
            let summary = check.summary.clone();
            let recovery_commands = tag_mapping_recovery_commands(&check);
            checks.push(check);
            return GitOverlayHealth {
                status,
                clean: false,
                summary,
                recovery_commands,
                checks,
            };
        }
        Ok(None) => checks.push(GitOverlayHealthCheck {
            name: "tag_mapping".to_string(),
            status: "clean".to_string(),
            summary: "Git tags visible to this checkout map to Heddle markers".to_string(),
            details: BTreeMap::new(),
        }),
        Err(error) => {
            checks.push(GitOverlayHealthCheck {
                name: "tag_mapping".to_string(),
                status: "degraded".to_string(),
                summary: error.to_string(),
                details: BTreeMap::new(),
            });
            return degraded(checks, "Could not inspect Git tag mapping");
        }
    }

    match stale_integration_metadata_check(repo) {
        Ok(Some(check)) => {
            let summary = check.summary.clone();
            checks.push(check);
            return GitOverlayHealth {
                status: "stale_integration_metadata".to_string(),
                clean: false,
                summary,
                recovery_commands: vec!["heddle thread list".to_string()],
                checks,
            };
        }
        Ok(None) => checks.push(GitOverlayHealthCheck {
            name: "thread_integration_metadata".to_string(),
            status: "clean".to_string(),
            summary: "merged thread metadata agrees with target history".to_string(),
            details: BTreeMap::new(),
        }),
        Err(error) => {
            checks.push(GitOverlayHealthCheck {
                name: "thread_integration_metadata".to_string(),
                status: "degraded".to_string(),
                summary: error.to_string(),
                details: BTreeMap::new(),
            });
            return degraded(checks, "Could not inspect thread integration metadata");
        }
    }

    match repo.git_remote_tracking_status() {
        Ok(Some(remote)) => return remote_drift(repo, checks, remote),
        Ok(None) => checks.push(GitOverlayHealthCheck {
            name: "remote_tracking".to_string(),
            status: "clean".to_string(),
            summary: "No Git upstream drift detected".to_string(),
            details: BTreeMap::new(),
        }),
        Err(error) => {
            checks.push(GitOverlayHealthCheck {
                name: "remote_tracking".to_string(),
                status: "degraded".to_string(),
                summary: error.to_string(),
                details: BTreeMap::new(),
            });
            return degraded(checks, "Could not inspect Git upstream drift");
        }
    }

    GitOverlayHealth::clean("Git overlay and Heddle agree", checks)
}

fn tag_mapping_check(repo: &Repository) -> anyhow::Result<Option<GitOverlayHealthCheck>> {
    let mut mismatched = Vec::new();

    for tip in repo.git_overlay_tag_tips()? {
        let marker = repo
            .refs()
            .get_marker(&objects::object::MarkerName::new(&tip.tag))?;
        match (marker, tip.mapped_change) {
            (Some(existing), Some(mapped)) if existing == mapped => {}
            (Some(existing), Some(mapped)) => mismatched.push(format!(
                "{} (marker {}; Git tag {})",
                tip.tag,
                existing.short(),
                mapped.short()
            )),
            (Some(_), None) | (None, _) => {}
        }
    }

    if mismatched.is_empty() {
        return Ok(None);
    }

    let mut details = BTreeMap::new();
    details.insert("mismatched_tags".to_string(), mismatched.join(", "));
    details.insert(
        "mismatched_tag_count".to_string(),
        mismatched.len().to_string(),
    );
    Ok(Some(GitOverlayHealthCheck {
        name: "tag_mapping".to_string(),
        status: "tag_marker_mismatch".to_string(),
        summary: format!(
            "{} Git tag marker(s) disagree with Heddle markers: {}",
            mismatched.len(),
            crate::cli::render::preview_list(&mismatched, mismatched.len())
        ),
        details,
    }))
}

fn short_oid(oid: &str) -> &str {
    oid.get(..12).unwrap_or(oid)
}

fn tag_mapping_recovery_commands(check: &GitOverlayHealthCheck) -> Vec<String> {
    let tag_names = check
        .details
        .get("missing_tags")
        .or_else(|| check.details.get("mismatched_tags"))
        .or_else(|| check.details.get("unmapped_tags"))
        .map(|tags| {
            tags.split(',')
                .filter_map(|tag| tag.split_whitespace().next())
                .filter(|tag| !tag.is_empty())
                .map(ToString::to_string)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    if tag_names.len() == 1 {
        vec![canonical_adopt_ref_command(&tag_names[0])]
    } else {
        vec!["heddle adopt".to_string()]
    }
}

fn stale_integration_metadata_check(
    repo: &Repository,
) -> anyhow::Result<Option<GitOverlayHealthCheck>> {
    let manager = ThreadManager::new(repo.heddle_dir());
    let mut stale = Vec::new();
    let mut graph = CommitGraphIndex::new(repo);

    for thread in manager.list()? {
        if thread.state != ThreadState::Merged {
            continue;
        }
        let Some(target_thread) = thread.target_thread.as_deref() else {
            continue;
        };
        let Some(target_tip) = repo.refs().get_thread(&ThreadName::new(target_thread))? else {
            continue;
        };
        let candidate = thread
            .current_state
            .as_deref()
            .or(thread.merged_state.as_deref())
            .and_then(|state| repo.resolve_state(state).ok().flatten())
            .or_else(|| {
                repo.refs()
                    .get_thread(&ThreadName::new(&thread.thread))
                    .ok()
                    .flatten()
            });
        let Some(candidate) = candidate else {
            continue;
        };
        if !graph.is_ancestor(&candidate, &target_tip).unwrap_or(false) {
            stale.push(format!(
                "{} claims merged into {} at {}, but target is {}",
                thread.thread,
                target_thread,
                candidate.short(),
                target_tip.short()
            ));
        }
    }

    if stale.is_empty() {
        return Ok(None);
    }

    let mut details = BTreeMap::new();
    details.insert("stale_thread_count".to_string(), stale.len().to_string());
    details.insert("stale_threads".to_string(), stale.join("; "));
    Ok(Some(GitOverlayHealthCheck {
        name: "thread_integration_metadata".to_string(),
        status: "stale_integration_metadata".to_string(),
        summary: format!(
            "{} merged thread record(s) are no longer contained in their target history",
            stale.len()
        ),
        details,
    }))
}

fn build_native_heddle_health(repo: &Repository) -> GitOverlayHealth {
    let mut checks = vec![GitOverlayHealthCheck {
        name: "capability".to_string(),
        status: "clean".to_string(),
        summary: "native Heddle repository".to_string(),
        details: BTreeMap::new(),
    }];

    match repo.operation_status() {
        Ok(Some(operation)) => {
            checks.push(GitOverlayHealthCheck {
                name: "operation".to_string(),
                status: "operation_in_progress".to_string(),
                summary: operation.message.clone(),
                details: BTreeMap::new(),
            });
            return GitOverlayHealth {
                status: "operation_in_progress".to_string(),
                clean: false,
                summary: operation.message,
                recovery_commands: vec![operation.next_action],
                checks,
            };
        }
        Ok(None) => checks.push(GitOverlayHealthCheck {
            name: "operation".to_string(),
            status: "clean".to_string(),
            summary: "no Heddle operation in progress".to_string(),
            details: BTreeMap::new(),
        }),
        Err(error) => {
            checks.push(GitOverlayHealthCheck {
                name: "operation".to_string(),
                status: "degraded".to_string(),
                summary: error.to_string(),
                details: BTreeMap::new(),
            });
            return degraded(checks, "Could not inspect in-progress operations");
        }
    }

    let worktree_status = repo.current_state().and_then(|state| {
        let Some(state) = state else {
            return Ok(WorktreeStatus::default());
        };
        let tree = repo.require_tree(&state.tree)?;
        repo.compare_worktree_cached_with_options(
            &tree,
            &worktree_status_options(Some(repo.config())),
        )
    });
    match worktree_status {
        Ok(status) if !status.is_clean() => {
            let changed = status.modified.len() + status.added.len() + status.deleted.len();
            checks.push(GitOverlayHealthCheck {
                name: "heddle_worktree".to_string(),
                status: "uncaptured".to_string(),
                summary: format!(
                    "{changed} Heddle worktree path(s) are not captured in the current state"
                ),
                details: dirty_details(&status),
            });
            GitOverlayHealth {
                status: "uncaptured".to_string(),
                clean: false,
                summary: format!(
                    "{changed} Heddle worktree path(s) are not captured in the current state"
                ),
                recovery_commands: vec![
                    "heddle commit -m \"...\"".to_string(),
                    "heddle capture -m \"...\"".to_string(),
                    "heddle stash push -m \"...\"".to_string(),
                ],
                checks,
            }
        }
        Ok(_) => {
            checks.push(GitOverlayHealthCheck {
                name: "heddle_worktree".to_string(),
                status: "clean".to_string(),
                summary: "Heddle worktree matches the current state".to_string(),
                details: BTreeMap::new(),
            });
            GitOverlayHealth::clean(
                "Heddle-native repository is verified in non-overlay mode",
                checks,
            )
        }
        Err(error) => {
            checks.push(GitOverlayHealthCheck {
                name: "heddle_worktree".to_string(),
                status: "degraded".to_string(),
                summary: error.to_string(),
                details: BTreeMap::new(),
            });
            degraded(checks, "Could not inspect Heddle worktree status")
        }
    }
}

fn needs_import(
    mut checks: Vec<GitOverlayHealthCheck>,
    hint: GitOverlayImportHint,
) -> GitOverlayHealth {
    checks.push(GitOverlayHealthCheck {
        name: "import".to_string(),
        status: "needs_import".to_string(),
        summary: format!(
            "{} Git branch tip(s) still need Heddle import",
            hint.missing_branch_count
        ),
        details: BTreeMap::new(),
    });
    GitOverlayHealth {
        status: "needs_import".to_string(),
        clean: false,
        summary: format!(
            "{} Git branch tip(s) still need Heddle import",
            hint.missing_branch_count
        ),
        recovery_commands: vec![hint.recommended_command],
        checks,
    }
}

pub(crate) fn canonical_adopt_ref_command(ref_name: &str) -> String {
    heddle_action(["adopt", "--ref", ref_name])
}

pub(crate) fn canonical_bridge_import_ref_command(ref_name: &str) -> String {
    heddle_action(["bridge", "git", "import", "--ref", ref_name])
}

pub(crate) fn canonical_bridge_reconcile_ref_preview_command(
    prefer: Option<&str>,
    ref_name: &str,
) -> String {
    match prefer {
        Some(prefer) => heddle_action([
            "bridge",
            "git",
            "reconcile",
            "--prefer",
            prefer,
            "--ref",
            ref_name,
            "--preview",
        ]),
        None => heddle_action(["bridge", "git", "reconcile", "--ref", ref_name, "--preview"]),
    }
}

pub(crate) fn canonical_bridge_reconcile_ref_command(prefer: &str, ref_name: &str) -> String {
    heddle_action([
        "bridge",
        "git",
        "reconcile",
        "--prefer",
        prefer,
        "--ref",
        ref_name,
    ])
}

pub(crate) fn import_hint_includes_active_branch(hint: &GitOverlayImportHint) -> bool {
    hint.missing_branches
        .iter()
        .any(|branch| branch == &hint.current_branch)
}

/// Render the "(N out-of-band git commits detected)" clause for the
/// `git_branch_advanced` report. Empty when the count is unavailable so the
/// report degrades to the countless wording instead of failing.
fn out_of_band_commit_clause(out_of_band: Option<&GitOverlayOutOfBandCommits>) -> String {
    match out_of_band {
        Some(out_of_band) if out_of_band.truncated => {
            format!(" ({}+ out-of-band git commits detected)", out_of_band.count)
        }
        Some(out_of_band) if out_of_band.count == 1 => {
            " (1 out-of-band git commit detected)".to_string()
        }
        Some(out_of_band) => format!(" ({} out-of-band git commits detected)", out_of_band.count),
        None => String::new(),
    }
}

fn remote_drift(
    repo: &Repository,
    mut checks: Vec<GitOverlayHealthCheck>,
    remote: GitRemoteTrackingStatus,
) -> GitOverlayHealth {
    let decision = remote_drift_decision(repo, &remote);
    let mut details = BTreeMap::new();
    details.insert("branch".to_string(), remote.branch.clone());
    details.insert("upstream".to_string(), remote.upstream.clone());
    details.insert("ahead".to_string(), remote.ahead.to_string());
    details.insert("behind".to_string(), remote.behind.to_string());
    if let Some(local_oid) = &remote.local_oid {
        details.insert("local_oid".to_string(), local_oid.clone());
    }
    if let Some(upstream_oid) = &remote.upstream_oid {
        details.insert("upstream_oid".to_string(), upstream_oid.clone());
    }
    if remote.upstream_is_undone_checkpoint {
        details.insert(
            "upstream_is_undone_checkpoint".to_string(),
            "true".to_string(),
        );
    }
    checks.push(GitOverlayHealthCheck {
        name: "remote_tracking".to_string(),
        status: decision.status.to_string(),
        summary: remote.message.clone(),
        details,
    });
    if decision.verified_as_clean {
        return GitOverlayHealth {
            status: "clean".to_string(),
            clean: true,
            summary: match decision.status {
                "remote_ahead" => "Git and Heddle agree; local commits are ready to push",
                "remote_untracked" => "Git and Heddle agree; branch is local-only until pushed",
                _ => "Git and Heddle agree",
            }
            .to_string(),
            recovery_commands: Vec::new(),
            checks,
        };
    }
    GitOverlayHealth {
        status: decision.status.to_string(),
        clean: false,
        summary: remote.message,
        recovery_commands: decision.recovery_commands,
        checks,
    }
}

fn degraded(mut checks: Vec<GitOverlayHealthCheck>, summary: &str) -> GitOverlayHealth {
    checks.push(GitOverlayHealthCheck {
        name: "contract".to_string(),
        status: "degraded".to_string(),
        summary: "health could not be proven clean".to_string(),
        details: BTreeMap::new(),
    });
    GitOverlayHealth {
        status: "degraded".to_string(),
        clean: false,
        summary: summary.to_string(),
        recovery_commands: vec!["heddle doctor".to_string()],
        checks,
    }
}

fn current_branch_tip(repo: &Repository) -> anyhow::Result<Option<GitOverlayBranchTip>> {
    let Some(branch) = repo.git_overlay_current_branch()? else {
        return Ok(None);
    };
    repo.git_overlay_branch_tip(&branch).map_err(Into::into)
}

fn branch_tip_needs_reconcile(repo: &Repository, tip: &GitOverlayBranchTip) -> bool {
    let Some(mapped) = tip.mapped_change else {
        return false;
    };
    let Ok(Some(current)) = repo.current_state() else {
        return false;
    };
    mapped != current.change_id
}

fn clean_git_branch_reconcile_check(
    repo: &Repository,
) -> anyhow::Result<Option<GitOverlayHealthCheck>> {
    let Some(tip) = current_branch_tip(repo)? else {
        return Ok(None);
    };
    if !tip.history_imported || !branch_tip_needs_reconcile(repo, &tip) {
        return Ok(None);
    }
    let Some(current) = repo.current_state()? else {
        return Ok(None);
    };
    let Some(mapped) = tip.mapped_change else {
        return Ok(None);
    };
    let relation = mapped_change_relation(repo, &mapped, &current.change_id);
    if relation == "git_behind_heddle"
        && repo
            .latest_git_checkpoint_for_change(&current.change_id)?
            .is_none()
        && heddle_worktree_is_clean(repo)
    {
        let mut details = dirty_details(&WorktreeStatus::default());
        details.insert("git_branch".to_string(), tip.branch.clone());
        details.insert("git_commit".to_string(), tip.git_commit.clone());
        details.insert("git_mapped_state".to_string(), mapped.to_string());
        details.insert(
            "heddle_thread_state".to_string(),
            current.change_id.to_string(),
        );
        details.insert("relation".to_string(), relation.to_string());
        return Ok(Some(GitOverlayHealthCheck {
            name: "worktree".to_string(),
            status: "needs_checkpoint".to_string(),
            summary: format!(
                "Heddle state {} is captured but not checkpointed to Git",
                current.change_id.short()
            ),
            details,
        }));
    }
    let mut details = BTreeMap::new();
    details.insert("git_branch".to_string(), tip.branch.clone());
    details.insert("git_commit".to_string(), tip.git_commit.clone());
    details.insert("git_mapped_state".to_string(), mapped.to_string());
    details.insert(
        "heddle_thread_state".to_string(),
        current.change_id.to_string(),
    );
    details.insert("relation".to_string(), relation.to_string());
    Ok(Some(GitOverlayHealthCheck {
        name: "head_mapping".to_string(),
        status: "needs_reconcile".to_string(),
        summary: format!(
            "Git branch '{}' points at {}, but Heddle thread state is {}; preview the Git/Heddle mapping before saving new work",
            tip.branch,
            mapped.short(),
            current.change_id.short()
        ),
        details,
    }))
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

#[cfg(test)]
mod tests {
    use objects::object::ThreadName;
    use repo::{GitRemoteTrackingStatus, Repository};
    use sley::Repository as SleyRepository;
    use tempfile::TempDir;

    use super::{
        GitOverlayHealth, RepositoryVerificationState, VerificationActionPlan, action_template,
        canonical_bridge_import_ref_command, canonical_bridge_reconcile_ref_preview_command,
        machine_contract_coverage, remote_drift_decision, remote_tracking_next_action,
        repository_setup_guidance, repository_verification_blocked_advice,
    };
    use crate::cli::commands::build_command_catalog;

    fn verification_state(
        recommended_action: impl Into<String>,
        recovery_commands: Vec<String>,
    ) -> RepositoryVerificationState {
        RepositoryVerificationState {
            verified: false,
            status: "needs_reconcile".to_string(),
            repository_mode: "git_overlay".to_string(),
            heddle_initialized: true,
            git_branch: Some("main".to_string()),
            heddle_thread: Some("main".to_string()),
            worktree_dirty: false,
            worktree_state: "clean".to_string(),
            import_state: "imported".to_string(),
            mapping_state: "needs_reconcile".to_string(),
            remote_drift: "none".to_string(),
            active_operation: None,
            default_remote: None,
            clone_verification: "verified".to_string(),
            machine_contract: "verified".to_string(),
            machine_contract_coverage: machine_contract_coverage(),
            workflow_status: "blocked".to_string(),
            workflow_summary: "Git and Heddle disagree".to_string(),
            summary: "Git and Heddle disagree".to_string(),
            recommended_action: recommended_action.into(),
            recommended_action_template: None,
            recovery_commands,
            recovery_action_templates: Vec::new(),
            checks: Vec::new(),
        }
    }

    #[test]
    fn repository_setup_guidance_distinguishes_init_from_adopt() {
        let mut init = verification_state("heddle init", vec!["heddle init".to_string()]);
        init.status = "needs_init".to_string();
        init.repository_mode = "plain-git".to_string();
        init.heddle_initialized = false;
        init.import_state = "git_backed".to_string();
        init.mapping_state = "git_backed".to_string();

        let guidance = repository_setup_guidance(&init).expect("init guidance");
        assert!(guidance.setup_line.contains("initialize Heddle"));
        assert!(guidance.setup_line.contains("heddle init"));
        assert!(guidance.effect.contains("Git commits stay in Git storage"));

        let mut convert = verification_state(
            "heddle adopt --ref main",
            vec!["heddle adopt --ref main".to_string()],
        );
        convert.status = "needs_import".to_string();
        convert.repository_mode = "git-overlay".to_string();
        convert.import_state = "needs_import".to_string();
        convert.mapping_state = "needs_import".to_string();

        let guidance = repository_setup_guidance(&convert).expect("conversion guidance");
        assert!(
            guidance
                .setup_line
                .contains("connect this branch with heddle adopt --ref main")
        );
        assert!(guidance.effect.contains("adoption imports Git history"));
    }

    #[test]
    fn canonical_git_overlay_ref_commands_quote_parseable_refs() {
        let import = canonical_bridge_import_ref_command("feature with spaces");
        assert_eq!(
            action_template(&import)
                .expect("import command should expose a template")
                .argv_template[1..],
            ["bridge", "git", "import", "--ref", "feature with spaces"]
        );

        let reconcile =
            canonical_bridge_reconcile_ref_preview_command(Some("heddle"), "feature 'quoted'");
        assert_eq!(
            action_template(&reconcile)
                .expect("reconcile command should expose a template")
                .argv_template[1..],
            [
                "bridge",
                "git",
                "reconcile",
                "--prefer",
                "heddle",
                "--ref",
                "feature 'quoted'",
                "--preview"
            ]
        );
    }

    #[test]
    fn repository_verification_blocked_advice_uses_verify_when_no_action_exists() {
        let trust = verification_state("", Vec::new());

        let advice = repository_verification_blocked_advice(
            "repository_verification_blocked",
            "blocked",
            "retrying the operation",
            &trust,
            "unsafe",
            "would change",
            "nothing changed",
            None,
        );

        assert_eq!(advice.primary_command, "heddle verify");
        assert_eq!(advice.recovery_commands, vec!["heddle verify"]);
        assert_eq!(
            advice.hint,
            "Run `heddle verify` before retrying the operation."
        );
    }

    #[test]
    fn repository_verification_blocked_advice_preserves_trust_recovery_commands() {
        let trust = verification_state(
            "heddle bridge git reconcile --ref main --preview",
            vec![
                "heddle bridge git reconcile --ref main --preview".to_string(),
                "heddle verify".to_string(),
            ],
        );

        let advice = repository_verification_blocked_advice(
            "repository_verification_blocked",
            "blocked",
            "retrying the operation",
            &trust,
            "unsafe",
            "would change",
            "nothing changed",
            None,
        );

        assert_eq!(
            advice.primary_command,
            "heddle bridge git reconcile --ref main --preview"
        );
        assert_eq!(advice.recovery_commands, trust.recovery_commands);
    }

    #[test]
    fn repository_verification_blocked_advice_keeps_primary_override_first() {
        let trust = verification_state(
            "heddle bridge git import --ref origin/main",
            vec!["heddle bridge git import --ref origin/main".to_string()],
        );

        let advice = repository_verification_blocked_advice(
            "git_checkpoint_preflight_blocked",
            "blocked",
            "retrying `heddle commit`",
            &trust,
            "unsafe",
            "would change",
            "nothing changed",
            Some("heddle pull origin main --preview".to_string()),
        );

        assert_eq!(advice.primary_command, "heddle pull origin main --preview");
        assert_eq!(
            advice.recovery_commands,
            vec!["heddle pull origin main --preview", "heddle verify"]
        );
    }

    #[test]
    fn verification_action_plan_keeps_blockers_above_guidance() {
        let clean_health = GitOverlayHealth::clean("clean", Vec::new());

        let machine_gap = VerificationActionPlan::from_parts(
            &clean_health,
            Some("heddle push".to_string()),
            Some("heddle land --thread feature --no-push".to_string()),
            Some("heddle doctor schemas --output json".to_string()),
        );
        assert_eq!(
            machine_gap.primary_action,
            "heddle doctor schemas --output json"
        );
        assert_eq!(
            machine_gap.recovery_commands,
            vec!["heddle doctor schemas --output json"]
        );
        assert_eq!(machine_gap.remote_action.as_deref(), Some("heddle push"));
        assert_eq!(
            machine_gap.workflow_action.as_deref(),
            Some("heddle land --thread feature --no-push")
        );

        let workflow_waiting = VerificationActionPlan::from_parts(
            &clean_health,
            Some("heddle push".to_string()),
            Some("heddle land --thread feature --no-push".to_string()),
            None,
        );
        assert_eq!(
            workflow_waiting.primary_action,
            "heddle land --thread feature --no-push"
        );

        let publish_guidance = VerificationActionPlan::from_parts(
            &clean_health,
            Some("heddle push".to_string()),
            None,
            None,
        );
        assert_eq!(publish_guidance.primary_action, "heddle push");
    }

    #[test]
    fn remote_tracking_next_action_covers_basic_git_states_without_repo_context() {
        assert_eq!(
            remote_tracking_next_action(&remote("main", "origin/main", 0, 1, "heddle pull"))
                .as_deref(),
            Some("heddle pull")
        );
        assert_eq!(
            remote_tracking_next_action(&remote("main", "origin/main", 1, 0, "heddle push"))
                .as_deref(),
            Some("heddle push")
        );
        assert_eq!(
            remote_tracking_next_action(&remote("main", "origin/main", 1, 1, "heddle fetch"))
                .as_deref(),
            Some("heddle bridge git import --ref origin/main")
        );
        assert_eq!(
            remote_tracking_next_action(&remote("main", "", 1, 0, "heddle push")).as_deref(),
            Some("heddle push")
        );
    }

    #[test]
    fn remote_drift_decision_prefers_import_until_upstream_thread_matches_git_tip() {
        let (_temp, repo) = test_repo();
        let diverged = remote("main", "origin/main", 1, 1, "heddle fetch");

        let unimported = remote_drift_decision(&repo, &diverged);
        assert_eq!(unimported.status, "remote_diverged");
        assert_eq!(
            unimported.primary_action.as_deref(),
            Some("heddle bridge git import --ref origin/main")
        );
        assert_eq!(
            unimported.recovery_commands,
            vec![
                "heddle bridge git import --ref origin/main",
                "heddle bridge git reconcile --ref origin/main --preview"
            ]
        );

        let head = repo.head().unwrap().expect("test repo should have a head");
        repo.refs()
            .set_thread(&ThreadName::new("origin/main"), &head)
            .unwrap();
        let stale_thread = remote_drift_decision(&repo, &diverged);
        assert_eq!(
            stale_thread.primary_action.as_deref(),
            Some("heddle bridge git import --ref origin/main")
        );
        assert_eq!(
            stale_thread.recovery_commands,
            vec![
                "heddle bridge git import --ref origin/main",
                "heddle bridge git reconcile --ref origin/main --preview"
            ]
        );
    }

    #[test]
    fn remote_drift_decision_treats_local_only_branch_as_clean_publishable_state() {
        let (_temp, repo) = test_repo();
        let untracked = remote("scratch", "", 0, 0, "heddle push");

        let decision = remote_drift_decision(&repo, &untracked);
        assert_eq!(decision.status, "remote_untracked");
        assert!(decision.verified_as_clean);
        assert_eq!(decision.primary_action.as_deref(), Some("heddle push"));
        assert!(decision.recovery_commands.is_empty());
        assert!(!decision.requires_clean_worktree);
    }

    fn remote(
        branch: &str,
        upstream: &str,
        ahead: usize,
        behind: usize,
        next_action: &str,
    ) -> GitRemoteTrackingStatus {
        GitRemoteTrackingStatus {
            branch: branch.to_string(),
            upstream: upstream.to_string(),
            ahead,
            behind,
            local_oid: None,
            upstream_oid: None,
            upstream_is_undone_checkpoint: false,
            message: "remote fixture".to_string(),
            next_action: next_action.to_string(),
        }
    }

    fn test_repo() -> (TempDir, Repository) {
        let temp = TempDir::new().unwrap();
        let repo = Repository::init_default(temp.path()).unwrap();
        (temp, repo)
    }

    /// `git rm --cached path` keeps the file in the worktree but
    /// stages the deletion: Git reports both `D path` (index vs HEAD)
    /// and `?? path` (worktree). Both signals must survive the
    /// dedup step or downstream code mistakes a tracked removal for a
    /// new file.
    #[test]
    fn plain_git_worktree_status_preserves_staged_removal_alongside_untracked() {
        use std::{path::PathBuf, process::Command};

        let dir = TempDir::new().expect("tempdir");
        let root = dir.path();

        let run_git = |args: &[&str]| -> Option<bool> {
            let output = Command::new("git")
                .arg("-C")
                .arg(root)
                .args(args)
                .env("GIT_AUTHOR_NAME", "Test")
                .env("GIT_AUTHOR_EMAIL", "test@example.com")
                .env("GIT_COMMITTER_NAME", "Test")
                .env("GIT_COMMITTER_EMAIL", "test@example.com")
                .output()
                .ok()?;
            Some(output.status.success())
        };

        let Some(true) = run_git(&["init", "--quiet"]) else {
            eprintln!("git not on PATH or init failed — skipping");
            return;
        };
        for cmd in [
            ["config", "user.email", "test@example.com"].as_slice(),
            ["config", "user.name", "Test"].as_slice(),
        ] {
            if !matches!(run_git(cmd), Some(true)) {
                eprintln!("git config failed — skipping");
                return;
            }
        }
        std::fs::write(root.join("file.txt"), "hello").expect("write file");
        for cmd in [
            ["add", "file.txt"].as_slice(),
            ["commit", "-m", "initial", "--quiet"].as_slice(),
            ["rm", "--cached", "--quiet", "file.txt"].as_slice(),
        ] {
            if !matches!(run_git(cmd), Some(true)) {
                eprintln!("git command failed — skipping");
                return;
            }
        }

        let git_repo = SleyRepository::discover(root).expect("open git repo");
        let status = super::plain_git_worktree_status(root, &git_repo).expect("status");

        let target = PathBuf::from("file.txt");
        assert!(
            status.added.iter().any(|path| path == &target),
            "untracked worktree copy must still appear as added: {status:?}"
        );
        assert!(
            status.deleted.iter().any(|path| path == &target),
            "staged removal must not be wiped by the untracked entry: {status:?}"
        );
        assert!(
            !status.modified.iter().any(|path| path == &target),
            "no modified entry for `git rm --cached` path: {status:?}"
        );
    }

    #[test]
    fn machine_contract_coverage_counts_the_same_rows_as_command_catalog() {
        let catalog = build_command_catalog();
        let catalog_json = catalog
            .commands
            .iter()
            .filter(|command| command.supports_json)
            .count();
        let catalog_op_id = catalog
            .commands
            .iter()
            .filter(|command| command.supports_op_id)
            .count();
        let catalog_jsonl = catalog
            .commands
            .iter()
            .filter(|command| command.json_kind == "jsonl" || command.json_kind == "json_or_jsonl")
            .count();
        let catalog_mutating = catalog
            .commands
            .iter()
            .filter(|command| command.mutates)
            .count();
        let json_with_schema = catalog
            .commands
            .iter()
            .filter(|command| {
                command.supports_json
                    && command.schema_verbs.iter().any(|verb| {
                        !crate::cli::commands::schemas::opaque_schema_verbs()
                            .contains(&verb.as_str())
                    })
            })
            .count();
        let mutating_json = catalog
            .commands
            .iter()
            .filter(|command| command.supports_json && command.mutates)
            .count();

        let coverage = machine_contract_coverage();
        assert_eq!(coverage.catalog_commands_total, catalog.commands.len());
        assert_eq!(coverage.catalog_mutating_commands_total, catalog_mutating);
        assert_eq!(coverage.json_commands_total, catalog_json);
        assert_eq!(coverage.json_mutating_commands_total, mutating_json);
        assert_eq!(coverage.json_commands_with_schema, json_with_schema);
        assert!(
            coverage.json_commands_with_accepted_opaque_schema > 0,
            "remaining opaque schemas should be counted separately from concrete coverage"
        );
        assert_eq!(coverage.verified_scope, "everyday_and_agent");
        assert_eq!(coverage.advanced_scope, "advanced_internal_admin");
        assert!(
            coverage.verified_scope_json_commands_total > 0,
            "verified machine scope should include everyday and agent-facing JSON commands"
        );
        assert_eq!(
            coverage.verified_scope_json_commands_with_accepted_opaque_schema, 0,
            "verified machine scope must not rely on opaque schemas"
        );
        assert!(
            coverage.advanced_scope_json_commands_with_accepted_opaque_schema > 0,
            "advanced machine scope should report opaque schemas outside clean verification"
        );
        assert_eq!(
            coverage.verified_scope_json_commands_without_schema, 0,
            "verified machine scope must have registered schemas"
        );
        assert_eq!(coverage.mutating_commands_total, mutating_json);
        assert_eq!(coverage.supports_op_id_total, catalog_op_id);
        assert_eq!(coverage.jsonl_commands_total, catalog_jsonl);
        assert_eq!(coverage.json_commands_without_schema, 0);
        assert_eq!(coverage.mutating_commands_without_schema, 0);
    }
}
