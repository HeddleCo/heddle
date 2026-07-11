// SPDX-License-Identifier: Apache-2.0
//! Status facade and report contract.

pub mod next_action;
pub mod verdict;

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    time::Instant,
};

use chrono::Utc;
use cli_shared::remote::RemoteConfig;
use objects::{
    HeddleError,
    error::Result,
    object::{Principal, State, ThreadName},
    store::{AgentEntry, AgentRegistry, AgentStatus},
    worktree::{WorktreeStatus, build_worktree_ignore},
};
use refs::Head;
use repo::{
    AgentUsageSummary, CommitGraphIndex, GitImportGuidance, GitOverlayBranchTip,
    GitOverlayOutOfBandCommits, GitRemoteTrackingStatus, RepoConfig, Repository,
    RepositoryCapability, RepositoryOperationStatus, Thread, ThreadFreshness, ThreadImpactCategory,
    ThreadManager, ThreadMode, ThreadState, WorktreeCompareProfile,
    describe_thread_advice_with_initial, is_synthetic_root, refresh_thread_freshness,
};
use schemars::JsonSchema;
use serde::Serialize;
use serde_json::Value;
use sley::{
    Repository as SleyRepository, ShortStatusOptions, ShortStatusRow, StatusUntrackedMode,
    StreamControl,
};
pub use verdict::{
    StatusCombinedVerdict, combined_verdict_axes, coordination_axis_clean, coordination_label,
    coordination_severity, health_severity, human_thread_health, resolve_coordination_with_trust,
    status_combined_verdict,
};

use self::next_action::{
    NextActionInput, canonical_adopt_ref_command, canonical_git_import_ref_command,
    canonical_git_repair_ref_preview_command, contextual_thread_action, effective_next_action,
    heddle_action, non_empty_action, remote_tracking_next_action, remote_tracking_status,
};
use crate::{
    ActionTemplate, ExecutionContext, HeddleReport, MachineOutputKind, OutputDiscriminator,
    ReportContract, RepositoryContextInfo, RepositoryVerificationState, VerificationCheck,
    schema_for_report,
    verify::{
        MachineContractInput, action_template, action_templates,
        build_plain_git_verification_probe_with_machine_contract,
        build_repository_verification_state_with_worktree_status_and_machine_contract,
        repository_mode_label, serialize_empty_action_as_null,
    },
};

#[derive(Clone)]
pub struct StatusOptions {
    pub start_path: Option<PathBuf>,
    pub detail: StatusDetail,
    pub worktree_status_options: repo::WorktreeStatusOptions,
    pub machine_contract_input: MachineContractInput,
}

impl StatusOptions {
    pub fn new(detail: StatusDetail, worktree_status_options: repo::WorktreeStatusOptions) -> Self {
        Self {
            start_path: None,
            detail,
            worktree_status_options,
            machine_contract_input: MachineContractInput::default(),
        }
    }

    pub fn with_start_path(mut self, start_path: impl Into<PathBuf>) -> Self {
        self.start_path = Some(start_path.into());
        self
    }

    pub fn with_machine_contract_input(mut self, input: MachineContractInput) -> Self {
        self.machine_contract_input = input;
        self
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusDetail {
    ShortText,
    CompactMachine,
    DefaultText,
    Full,
}

impl StatusDetail {
    fn short_path(self) -> bool {
        matches!(self, Self::ShortText | Self::CompactMachine)
    }

    fn needs_full_walk(self) -> bool {
        matches!(self, Self::Full)
    }

    fn needs_remote_tracking(self) -> bool {
        matches!(self, Self::ShortText | Self::Full)
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[schemars(rename = "StatusSchema")]
pub struct StatusReport {
    pub output_kind: &'static str,
    pub repository_capability: String,
    pub repository_label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repository_context: Option<RepositoryContextInfo>,
    pub storage_model: String,
    pub hosted_enabled: bool,
    #[serde(skip)]
    #[schemars(skip)]
    pub validation_capability: RepositoryCapability,
    #[schemars(with = "Option<serde_json::Value>")]
    pub operation: Option<RepositoryOperationStatus>,
    #[schemars(with = "Option<serde_json::Value>")]
    pub remote_tracking: Option<GitRemoteTrackingStatus>,
    #[serde(rename = "verification")]
    pub trust: RepositoryVerificationState,
    pub git_index: Option<GitIndexPlan>,
    #[serde(skip)]
    #[schemars(skip)]
    pub import_guidance: Option<GitImportGuidanceReport>,
    #[serde(skip)]
    #[schemars(skip)]
    pub verification_health: RepositoryVerificationHealth,
    pub thread: Option<String>,
    pub base_state: Option<String>,
    pub base_root: Option<String>,
    pub current_state: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub execution_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub heddle_session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actor: Option<ActorInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub harness: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking_level: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(with = "Option<serde_json::Value>")]
    pub usage_summary: Option<AgentUsageSummary>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_progress_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub report_flush_state: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attach_reason: Option<String>,
    #[schemars(with = "Option<String>")]
    pub thread_mode: Option<ThreadMode>,
    #[schemars(with = "Option<String>")]
    pub thread_state: Option<ThreadState>,
    #[schemars(with = "Option<String>")]
    pub freshness: Option<ThreadFreshness>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_thread: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_thread: Option<String>,
    pub child_threads: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task: Option<String>,
    pub promotion_suggested: bool,
    #[schemars(with = "Vec<String>")]
    pub impact_categories: Vec<ThreadImpactCategory>,
    pub heavy_impact_paths: Vec<String>,
    #[serde(skip)]
    #[schemars(skip)]
    pub changed_paths: Vec<String>,
    pub changed_path_count: usize,
    pub worktree_changed_path_count: usize,
    pub thread_changed_path_count: usize,
    pub blockers: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub identity_notice: Option<String>,
    #[serde(serialize_with = "serialize_empty_action_as_null")]
    #[schemars(with = "Option<String>")]
    pub recommended_action: String,
    pub recommended_action_template: Option<ActionTemplate>,
    pub recovery_commands: Vec<String>,
    pub recovery_action_templates: Vec<ActionTemplate>,
    pub thread_health: String,
    pub coordination_status: CoordinationStatus,
    #[serde(skip)]
    #[schemars(skip)]
    pub coordination_blocked_by_trust: bool,
    pub is_isolated: bool,
    pub parallel_threads: Vec<ParallelThreadInfo>,
    pub state: Option<StateInfo>,
    pub git_checkpoint: Option<GitCheckpointInfo>,
    pub changes: ChangesInfo,
    #[serde(default)]
    pub materialized_threads: Vec<MaterializedThreadInfo>,
    #[serde(skip)]
    #[schemars(skip)]
    pub profile: StatusProfile,
}

impl StatusReport {
    pub const CONTRACT: ReportContract = ReportContract {
        schema_name: "status",
        machine_output_kind: MachineOutputKind::JsonOrJsonLines,
        output_discriminator: Some(OutputDiscriminator {
            field: "output_kind",
            value: "status",
        }),
        schema: status_report_schema,
    };
}

impl HeddleReport for StatusReport {
    const CONTRACT: ReportContract = StatusReport::CONTRACT;
}

fn status_report_schema() -> Value {
    let mut schema = schema_for_report::<StatusReport>();
    require_schema_field(&mut schema, "recommended_action");
    replace_property_schema(
        &mut schema,
        "thread_mode",
        serde_json::json!({
            "anyOf": [
                {
                    "type": "string",
                    "enum": ["materialized", "virtualized", "solid"]
                },
                { "type": "null" }
            ]
        }),
    );
    schema
}

fn require_schema_field(schema: &mut Value, field: &str) {
    let Some(object) = schema.as_object_mut() else {
        return;
    };
    let required = object
        .entry("required".to_string())
        .or_insert_with(|| serde_json::json!([]));
    let Some(required) = required.as_array_mut() else {
        return;
    };
    if !required
        .iter()
        .any(|candidate| candidate.as_str() == Some(field))
    {
        required.push(Value::String(field.to_string()));
    }
}

fn replace_property_schema(schema: &mut Value, field: &str, replacement: Value) {
    let Some(properties) = schema
        .get_mut("properties")
        .and_then(|properties| properties.as_object_mut())
    else {
        return;
    };
    properties.insert(field.to_string(), replacement);
}

#[derive(Debug, Clone, Default)]
pub struct StatusProfile {
    pub repo_open_ms: u128,
    pub current_state_ms: u128,
    pub operation_ms: u128,
    pub remote_tracking_ms: u128,
    pub import_hint_ms: u128,
    pub git_overlay_status_ms: u128,
    pub verification_ms: u128,
    pub git_index_ms: u128,
    pub worktree_status_ms: u128,
    pub thread_summary_ms: u128,
    pub parallel_threads_ms: u128,
    pub late_state_ms: u128,
    pub materialized_threads_ms: u128,
    pub advice_ms: u128,
    pub build_total_ms: u128,
    pub worktree_profile: Option<WorktreeCompareProfile>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct RepositoryVerificationHealth {
    pub status: String,
    pub clean: bool,
    pub summary: String,
    pub recovery_commands: Vec<String>,
    pub checks: Vec<RepositoryVerificationCheck>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct RepositoryVerificationCheck {
    pub name: String,
    pub status: String,
    pub summary: String,
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub details: std::collections::BTreeMap<String, String>,
}

pub fn build_repository_verification_health_with_worktree_status(
    repo: &Repository,
    worktree_status: &Result<Option<WorktreeStatus>>,
) -> RepositoryVerificationHealth {
    if repo.capability() != RepositoryCapability::GitOverlay {
        // An in-progress operation (e.g. a conflicted merge awaiting `heddle
        // continue`/`heddle abort`) takes precedence over worktree dirtiness:
        // the health, and the recommended action derived from it, must point
        // at completing the operation, not at capturing the half-merged tree.
        // The pre-facade `build_native_heddle_health` checked this first;
        // dropping it made native `status`/`thread show`/`doctor` recommend
        // `heddle commit` mid-merge instead of `heddle continue`.
        match repo.operation_status() {
            Ok(Some(operation)) => {
                return RepositoryVerificationHealth {
                    status: "operation_in_progress".to_string(),
                    clean: false,
                    summary: operation.message.clone(),
                    recovery_commands: vec![operation.next_action.clone()],
                    checks: vec![RepositoryVerificationCheck {
                        name: "operation".to_string(),
                        status: "operation_in_progress".to_string(),
                        summary: operation.message,
                        details: Default::default(),
                    }],
                };
            }
            Ok(None) => {}
            Err(error) => {
                return degraded_health(
                    vec![RepositoryVerificationCheck {
                        name: "operation".to_string(),
                        status: "degraded".to_string(),
                        summary: error.to_string(),
                        details: Default::default(),
                    }],
                    "Could not inspect in-progress operations",
                );
            }
        }
        // A native repo's worktree dirtiness is derived from the current state
        // tree, NOT from the git-overlay walk. Callers that share a single
        // `git_overlay_worktree_status()` result (e.g. `ready`) hand us
        // `Ok(None)` on native repos — that means "not computed for native",
        // NOT "clean". Re-derive the native status ourselves in that case so
        // uncaptured worktree edits stay honest (matches the pre-facade
        // `build_native_heddle_health` behavior).
        let computed_native_status;
        let effective_status: &Result<Option<WorktreeStatus>> = match worktree_status {
            Ok(Some(_)) | Err(_) => worktree_status,
            Ok(None) => {
                computed_native_status = native_worktree_status(repo);
                &computed_native_status
            }
        };
        return match effective_status {
            Ok(Some(status)) if !status.is_clean() => {
                let changed = status.modified.len() + status.added.len() + status.deleted.len();
                let summary = format!(
                    "{changed} Heddle worktree path(s) are not captured in the current state"
                );
                RepositoryVerificationHealth {
                    status: "uncaptured".to_string(),
                    clean: false,
                    summary: summary.clone(),
                    recovery_commands: vec![
                        "heddle commit -m \"...\"".to_string(),
                        "heddle capture -m \"...\"".to_string(),
                    ],
                    checks: vec![RepositoryVerificationCheck {
                        name: "heddle_worktree".to_string(),
                        status: "uncaptured".to_string(),
                        summary,
                        details: dirty_details(status),
                    }],
                }
            }
            Ok(_) => clean_health(
                "Heddle-native repository is verified in non-overlay mode",
                vec![RepositoryVerificationCheck {
                    name: "heddle_worktree".to_string(),
                    status: "clean".to_string(),
                    summary: "Heddle worktree matches the current state".to_string(),
                    details: Default::default(),
                }],
            ),
            Err(error) => degraded_health(
                vec![RepositoryVerificationCheck {
                    name: "heddle_worktree".to_string(),
                    status: "degraded".to_string(),
                    summary: error.to_string(),
                    details: Default::default(),
                }],
                "Could not inspect Heddle worktree status",
            ),
        };
    }
    if repo.root().join(".heddle/objectstore").is_file() && !repo.root().join(".git").exists() {
        return clean_health(
            "Heddle-managed isolated checkout; Git verification belongs to the parent checkout",
            vec![RepositoryVerificationCheck {
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
            checks.push(RepositoryVerificationCheck {
                name: "operation".to_string(),
                status: "operation_in_progress".to_string(),
                summary: operation.message.clone(),
                details: Default::default(),
            });
            return RepositoryVerificationHealth {
                status: "operation_in_progress".to_string(),
                clean: false,
                summary: operation.message,
                recovery_commands: vec![operation.next_action],
                checks,
            };
        }
        Ok(None) => checks.push(RepositoryVerificationCheck {
            name: "operation".to_string(),
            status: "clean".to_string(),
            summary: "no Git or Heddle operation in progress".to_string(),
            details: Default::default(),
        }),
        Err(error) => {
            checks.push(RepositoryVerificationCheck {
                name: "operation".to_string(),
                status: "degraded".to_string(),
                summary: error.to_string(),
                details: Default::default(),
            });
            return degraded_health(checks, "Could not inspect in-progress operations");
        }
    }

    match repo.git_overlay_head_is_detached() {
        Ok(true) => {
            let mut details = BTreeMap::new();
            if let Ok(Some(commit)) = repo.git_overlay_detached_head_commit() {
                details.insert("git_commit".to_string(), commit);
            }
            checks.push(RepositoryVerificationCheck {
                name: "head_mapping".to_string(),
                status: "detached_head".to_string(),
                summary: "Git HEAD is detached; attach a branch before mutating this Git overlay"
                    .to_string(),
                details,
            });
            return RepositoryVerificationHealth {
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
            checks.push(RepositoryVerificationCheck {
                name: "head_mapping".to_string(),
                status: "degraded".to_string(),
                summary: error.to_string(),
                details: Default::default(),
            });
            return degraded_health(checks, "Could not inspect Git HEAD state");
        }
    }

    let import_hint = match repo.git_import_guidance() {
        Ok(hint) => hint,
        Err(error) => {
            checks.push(RepositoryVerificationCheck {
                name: "import".to_string(),
                status: "degraded".to_string(),
                summary: error.to_string(),
                details: BTreeMap::new(),
            });
            return degraded_health(checks, "Could not inspect Git import state");
        }
    };

    match current_branch_tip(repo) {
        Ok(Some(tip))
            if !tip.history_imported
                && repo.current_state().ok().flatten().is_some()
                && import_hint
                    .as_ref()
                    .is_some_and(import_guidance_includes_active_branch) =>
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
            checks.push(RepositoryVerificationCheck {
                name: "head_mapping".to_string(),
                status: "git_branch_advanced".to_string(),
                summary: format!(
                    "Git branch '{}' advanced to commit {} outside Heddle{}",
                    tip.branch, tip.git_commit, out_of_band_clause
                ),
                details,
            });
            if let Some(hint) = &import_hint
                && import_guidance_includes_active_branch(hint)
            {
                checks.push(RepositoryVerificationCheck {
                    name: "import".to_string(),
                    status: "needs_import".to_string(),
                    summary: format!(
                        "{} Git branch tip(s) still need Heddle import",
                        hint.missing_branch_count
                    ),
                    details: BTreeMap::new(),
                });
            }
            return RepositoryVerificationHealth {
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
        Ok(Some(tip)) if !tip.history_imported => checks.push(RepositoryVerificationCheck {
            name: "head_mapping".to_string(),
            status: "git_backed".to_string(),
            summary: format!(
                "Git branch '{}' resolves directly to Git commit {}",
                tip.branch,
                short_oid(&tip.git_commit)
            ),
            details: BTreeMap::from([
                ("git_branch".to_string(), tip.branch),
                ("git_commit".to_string(), tip.git_commit),
            ]),
        }),
        Ok(Some(tip)) => checks.push(RepositoryVerificationCheck {
            name: "head_mapping".to_string(),
            status: "clean".to_string(),
            summary: format!("Git branch '{}' maps to imported Heddle state", tip.branch),
            details: BTreeMap::new(),
        }),
        Ok(None) => checks.push(RepositoryVerificationCheck {
            name: "head_mapping".to_string(),
            status: "clean".to_string(),
            summary: "No attached Git branch to map".to_string(),
            details: BTreeMap::new(),
        }),
        Err(error) => {
            checks.push(RepositoryVerificationCheck {
                name: "head_mapping".to_string(),
                status: "degraded".to_string(),
                summary: error.to_string(),
                details: BTreeMap::new(),
            });
            return degraded_health(checks, "Could not inspect Git/Heddle branch mapping");
        }
    }

    match import_hint {
        Some(hint) if import_guidance_includes_active_branch(&hint) => {
            return needs_import(checks, hint);
        }
        Some(hint) => checks.push(RepositoryVerificationCheck {
            name: "import".to_string(),
            status: "available".to_string(),
            summary: format!(
                "{} other Git branch tip(s) are available to import",
                hint.missing_branch_count
            ),
            details: BTreeMap::new(),
        }),
        None => checks.push(RepositoryVerificationCheck {
            name: "import".to_string(),
            status: "clean".to_string(),
            summary: "Git refs are read directly from Git storage".to_string(),
            details: BTreeMap::new(),
        }),
    }

    match worktree_status {
        Ok(Some(status)) if !status.is_clean() => {
            let changed = status.modified.len() + status.added.len() + status.deleted.len();
            checks.push(RepositoryVerificationCheck {
                name: "worktree".to_string(),
                status: if heddle_worktree_is_clean(repo) {
                    "needs_checkpoint".to_string()
                } else {
                    "dirty_worktree".to_string()
                },
                summary: if heddle_worktree_is_clean(repo) {
                    format!(
                        "{changed} Git worktree path(s) are captured in Heddle but not checkpointed to Git"
                    )
                } else {
                    format!("{changed} Git worktree path(s) have uncommitted changes")
                },
                details: dirty_details(status),
            });
            if heddle_worktree_is_clean(repo) {
                return RepositoryVerificationHealth {
                    status: "needs_checkpoint".to_string(),
                    clean: false,
                    summary: format!(
                        "{changed} Git worktree path(s) are captured in Heddle but not checkpointed to Git"
                    ),
                    recovery_commands: vec!["heddle checkpoint -m \"...\"".to_string()],
                    checks,
                };
            }
            RepositoryVerificationHealth {
                status: "dirty_worktree".to_string(),
                clean: false,
                summary: format!("{changed} Git worktree path(s) have uncommitted changes"),
                recovery_commands: vec![
                    "heddle commit -m \"...\"".to_string(),
                    "heddle capture -m \"...\"".to_string(),
                    "heddle stash push -m \"...\"".to_string(),
                ],
                checks,
            }
        }
        Ok(_) => {
            checks.push(RepositoryVerificationCheck {
                name: "worktree".to_string(),
                status: "clean".to_string(),
                summary: "Git worktree is clean".to_string(),
                details: Default::default(),
            });
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
                        canonical_git_repair_ref_preview_command(None, &ref_name)
                    };
                    checks.push(check);
                    return RepositoryVerificationHealth {
                        status,
                        clean: false,
                        summary,
                        recovery_commands: vec![recovery],
                        checks,
                    };
                }
                Ok(None) => {}
                Err(error) => {
                    checks.push(RepositoryVerificationCheck {
                        name: "head_mapping".to_string(),
                        status: "degraded".to_string(),
                        summary: error.to_string(),
                        details: BTreeMap::new(),
                    });
                    return degraded_health(
                        checks,
                        "Could not inspect Git/Heddle branch agreement",
                    );
                }
            }
            if !head_mapping_is_git_backed(&checks)
                && let Ok(Some(state)) = repo.current_state()
                && let Ok(tree) = repo.require_tree(&state.tree)
                && let Ok(status) = repo.compare_worktree_cached_with_options(
                    &tree,
                    &core_worktree_status_options(repo),
                )
                && !status.is_clean()
            {
                let changed = status.modified.len() + status.added.len() + status.deleted.len();
                checks.push(RepositoryVerificationCheck {
                    name: "heddle_worktree".to_string(),
                    status: "dirty_worktree".to_string(),
                    summary: format!(
                        "{changed} Heddle worktree path(s) differ from the current state"
                    ),
                    details: dirty_details(&status),
                });
                return RepositoryVerificationHealth {
                    status: "dirty_worktree".to_string(),
                    clean: false,
                    summary: format!(
                        "{changed} Heddle worktree path(s) differ from the current state"
                    ),
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
                    let summary = check.summary.clone();
                    let recovery_commands = tag_mapping_recovery_commands(&check);
                    checks.push(check);
                    return RepositoryVerificationHealth {
                        status: "tag_marker_mismatch".to_string(),
                        clean: false,
                        summary,
                        recovery_commands,
                        checks,
                    };
                }
                Ok(None) => checks.push(RepositoryVerificationCheck {
                    name: "tag_mapping".to_string(),
                    status: "clean".to_string(),
                    summary: "Git tags visible to this checkout map to Heddle markers".to_string(),
                    details: Default::default(),
                }),
                Err(error) => {
                    checks.push(RepositoryVerificationCheck {
                        name: "tag_mapping".to_string(),
                        status: "degraded".to_string(),
                        summary: error.to_string(),
                        details: Default::default(),
                    });
                    return degraded_health(checks, "Could not inspect Git tag mapping");
                }
            }
            match stale_integration_metadata_check(repo) {
                Ok(Some(check)) => {
                    let summary = check.summary.clone();
                    checks.push(check);
                    return RepositoryVerificationHealth {
                        status: "stale_integration_metadata".to_string(),
                        clean: false,
                        summary,
                        recovery_commands: vec!["heddle thread list".to_string()],
                        checks,
                    };
                }
                Ok(None) => checks.push(RepositoryVerificationCheck {
                    name: "thread_integration_metadata".to_string(),
                    status: "clean".to_string(),
                    summary: "merged thread metadata agrees with target history".to_string(),
                    details: BTreeMap::new(),
                }),
                Err(error) => {
                    checks.push(RepositoryVerificationCheck {
                        name: "thread_integration_metadata".to_string(),
                        status: "degraded".to_string(),
                        summary: error.to_string(),
                        details: BTreeMap::new(),
                    });
                    return degraded_health(
                        checks,
                        "Could not inspect thread integration metadata",
                    );
                }
            }
            match repo.git_remote_tracking_status() {
                Ok(Some(remote)) => remote_drift_health(repo, checks, remote),
                Ok(None) => {
                    checks.push(RepositoryVerificationCheck {
                        name: "remote_tracking".to_string(),
                        status: "clean".to_string(),
                        summary: "No Git upstream drift detected".to_string(),
                        details: Default::default(),
                    });
                    clean_health("Git overlay and Heddle agree", checks)
                }
                Err(error) => {
                    checks.push(RepositoryVerificationCheck {
                        name: "remote_tracking".to_string(),
                        status: "degraded".to_string(),
                        summary: error.to_string(),
                        details: Default::default(),
                    });
                    degraded_health(checks, "Could not inspect Git upstream drift")
                }
            }
        }
        Err(error) => {
            checks.push(RepositoryVerificationCheck {
                name: "worktree".to_string(),
                status: "degraded".to_string(),
                summary: error.to_string(),
                details: Default::default(),
            });
            degraded_health(checks, "Could not inspect Git overlay worktree")
        }
    }
}

fn needs_import(
    mut checks: Vec<RepositoryVerificationCheck>,
    hint: GitImportGuidance,
) -> RepositoryVerificationHealth {
    checks.push(RepositoryVerificationCheck {
        name: "import".to_string(),
        status: "needs_import".to_string(),
        summary: format!(
            "{} Git branch tip(s) still need Heddle import",
            hint.missing_branch_count
        ),
        details: BTreeMap::new(),
    });
    RepositoryVerificationHealth {
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

fn tag_mapping_check(repo: &Repository) -> anyhow::Result<Option<RepositoryVerificationCheck>> {
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
    details.insert(
        "mismatched_tag_count".to_string(),
        mismatched.len().to_string(),
    );
    details.insert("mismatched_tags".to_string(), mismatched.join(", "));
    Ok(Some(RepositoryVerificationCheck {
        name: "tag_mapping".to_string(),
        status: "tag_marker_mismatch".to_string(),
        summary: format!(
            "{} Git tag marker(s) disagree with Heddle markers: {}",
            mismatched.len(),
            mismatched.join(", ")
        ),
        details,
    }))
}

fn tag_mapping_recovery_commands(check: &RepositoryVerificationCheck) -> Vec<String> {
    let tags = check
        .details
        .get("mismatched_tags")
        .map(|tags| {
            tags.split(',')
                .filter_map(|tag| tag.split_whitespace().next())
                .filter(|tag| !tag.is_empty())
                .map(ToString::to_string)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    if tags.len() == 1 {
        vec![format!("heddle adopt --ref {}", tags[0])]
    } else {
        vec!["heddle adopt".to_string()]
    }
}

fn short_oid(oid: &str) -> &str {
    oid.get(..12).unwrap_or(oid)
}

fn current_branch_tip(repo: &Repository) -> anyhow::Result<Option<GitOverlayBranchTip>> {
    let Some(branch) = repo.git_overlay_current_branch()? else {
        return Ok(None);
    };
    repo.git_overlay_branch_tip(&branch).map_err(Into::into)
}

fn detached_head_recovery_commands(repo: &Repository) -> Vec<String> {
    vec![detached_head_primary_recovery(repo)]
}

fn detached_head_primary_recovery(repo: &Repository) -> String {
    match repo.refs().read_head() {
        Ok(Head::Attached { thread }) if !thread.trim().is_empty() => {
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

fn branch_tip_needs_reconcile(repo: &Repository, tip: &GitOverlayBranchTip) -> bool {
    let Some(mapped) = tip.mapped_change else {
        return false;
    };
    let Ok(Some(current)) = thread_tip_for_branch(repo, &tip.branch) else {
        return false;
    };
    mapped != current
}

fn clean_git_branch_reconcile_check(
    repo: &Repository,
) -> anyhow::Result<Option<RepositoryVerificationCheck>> {
    let Some(tip) = current_branch_tip(repo)? else {
        return Ok(None);
    };
    if !tip.history_imported || !branch_tip_needs_reconcile(repo, &tip) {
        return Ok(None);
    }
    let Some(current_change) = thread_tip_for_branch(repo, &tip.branch)? else {
        return Ok(None);
    };
    let Some(mapped) = tip.mapped_change else {
        return Ok(None);
    };
    let relation = mapped_change_relation(repo, &mapped, &current_change);
    if relation == "git_behind_heddle"
        && repo
            .latest_git_checkpoint_for_change(&current_change)?
            .is_none()
        && heddle_worktree_is_clean(repo)
    {
        let mut details = dirty_details(&WorktreeStatus::default());
        details.insert("git_branch".to_string(), tip.branch.clone());
        details.insert("git_commit".to_string(), tip.git_commit.clone());
        details.insert("git_mapped_state".to_string(), mapped.to_string());
        details.insert(
            "heddle_thread_state".to_string(),
            current_change.to_string(),
        );
        details.insert("relation".to_string(), relation.to_string());
        return Ok(Some(RepositoryVerificationCheck {
            name: "worktree".to_string(),
            status: "needs_checkpoint".to_string(),
            summary: format!(
                "Heddle state {} is captured but not checkpointed to Git",
                current_change.short()
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
        current_change.to_string(),
    );
    details.insert("relation".to_string(), relation.to_string());
    Ok(Some(RepositoryVerificationCheck {
        name: "head_mapping".to_string(),
        status: "needs_reconcile".to_string(),
        summary: format!(
            "Git branch '{}' points at {}, but Heddle thread state is {}; preview the Git/Heddle mapping before saving new work",
            tip.branch,
            mapped.short(),
            current_change.short()
        ),
        details,
    }))
}

fn thread_tip_for_branch(
    repo: &Repository,
    branch: &str,
) -> Result<Option<objects::object::ChangeId>> {
    repo.refs().get_thread(&ThreadName::new(branch))
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

fn head_mapping_is_git_backed(checks: &[RepositoryVerificationCheck]) -> bool {
    checks
        .iter()
        .any(|check| check.name == "head_mapping" && check.status == "git_backed")
}

fn stale_integration_metadata_check(
    repo: &Repository,
) -> anyhow::Result<Option<RepositoryVerificationCheck>> {
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
    Ok(Some(RepositoryVerificationCheck {
        name: "thread_integration_metadata".to_string(),
        status: "stale_integration_metadata".to_string(),
        summary: format!(
            "{} merged thread record(s) are no longer contained in their target history",
            stale.len()
        ),
        details,
    }))
}

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

fn core_worktree_status_options(repo: &Repository) -> repo::WorktreeStatusOptions {
    repo::WorktreeStatusOptions {
        fsmonitor: repo.config().worktree.fsmonitor.into(),
    }
}

/// Derive a native repo's worktree dirtiness from its current-state tree.
/// A repo without a current state is treated as clean. Used when a caller
/// only supplied a git-overlay walk (`Ok(None)` on native repos) so the
/// native verification path can still report uncaptured edits honestly.
fn native_worktree_status(repo: &Repository) -> Result<Option<WorktreeStatus>> {
    let Some(state) = repo.current_state()? else {
        return Ok(Some(WorktreeStatus::default()));
    };
    let tree = repo.require_tree(&state.tree)?;
    repo.compare_worktree_cached_with_options(&tree, &core_worktree_status_options(repo))
        .map(Some)
}

pub fn default_remote_name(repo: &Repository) -> Option<String> {
    RemoteConfig::open(repo)
        .ok()
        .and_then(|cfg| cfg.default_name().map(str::to_string))
        .or_else(|| {
            (repo.capability() == RepositoryCapability::GitOverlay)
                .then(|| git_default_remote_name(repo.root()))
                .flatten()
        })
}

fn git_default_remote_name(root: &Path) -> Option<String> {
    let repo = SleyRepository::discover(root).ok()?;
    git_default_remote_name_from_repo(&repo)
}

pub(crate) fn git_default_remote_name_from_repo(repo: &SleyRepository) -> Option<String> {
    repo.remote_names()
        .ok()?
        .into_iter()
        .find(|name| name == "origin")
}

fn heddle_worktree_is_clean(repo: &Repository) -> bool {
    let Ok(Some(state)) = repo.current_state() else {
        return false;
    };
    let Ok(tree) = repo.require_tree(&state.tree) else {
        return false;
    };
    repo.compare_worktree_cached_with_options(&tree, &core_worktree_status_options(repo))
        .map(|status| status.is_clean())
        .unwrap_or(false)
}

fn remote_drift_health(
    repo: &Repository,
    mut checks: Vec<RepositoryVerificationCheck>,
    remote: GitRemoteTrackingStatus,
) -> RepositoryVerificationHealth {
    let status = remote_tracking_status(&remote);
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
    checks.push(RepositoryVerificationCheck {
        name: "remote_tracking".to_string(),
        status: status.to_string(),
        summary: remote.message.clone(),
        details,
    });
    let recovery_commands = remote_drift_recovery_commands(repo, &remote, status);
    if matches!(status, "clean" | "remote_ahead" | "remote_untracked") {
        return RepositoryVerificationHealth {
            status: "clean".to_string(),
            clean: true,
            summary: "Git overlay verified".to_string(),
            recovery_commands: Vec::new(),
            checks,
        };
    }
    RepositoryVerificationHealth {
        status: status.to_string(),
        clean: false,
        summary: remote.message,
        recovery_commands,
        checks,
    }
}

fn remote_drift_recovery_commands(
    repo: &Repository,
    remote: &GitRemoteTrackingStatus,
    status: &str,
) -> Vec<String> {
    match status {
        "remote_behind" => vec!["heddle pull".to_string()],
        "remote_diverged" => {
            let upstream = remote.upstream.trim();
            if upstream.is_empty() {
                return vec!["heddle fetch".to_string()];
            }
            let import = canonical_git_import_ref_command(upstream);
            let reconcile = canonical_git_repair_ref_preview_command(None, upstream);
            if upstream_thread_matches_current_git_tip(repo, upstream) {
                vec![reconcile]
            } else {
                vec![import, reconcile]
            }
        }
        "remote_contains_undone_checkpoint" => {
            vec![
                "heddle push --force".to_string(),
                "heddle undo --redo".to_string(),
            ]
        }
        _ => remote_tracking_next_action(remote).into_iter().collect(),
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

fn clean_health(
    summary: impl Into<String>,
    checks: Vec<RepositoryVerificationCheck>,
) -> RepositoryVerificationHealth {
    RepositoryVerificationHealth {
        status: "clean".to_string(),
        clean: true,
        summary: summary.into(),
        recovery_commands: Vec::new(),
        checks,
    }
}

fn degraded_health(
    checks: Vec<RepositoryVerificationCheck>,
    summary: &str,
) -> RepositoryVerificationHealth {
    RepositoryVerificationHealth {
        status: "degraded".to_string(),
        clean: false,
        summary: summary.to_string(),
        recovery_commands: vec!["heddle diagnose".to_string()],
        checks,
    }
}

fn dirty_details(status: &WorktreeStatus) -> std::collections::BTreeMap<String, String> {
    let mut details = std::collections::BTreeMap::new();
    let count = status.modified.len() + status.added.len() + status.deleted.len();
    details.insert("dirty_path_count".to_string(), count.to_string());
    let mut paths = status
        .modified
        .iter()
        .chain(status.added.iter())
        .chain(status.deleted.iter())
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>();
    paths.sort();
    if !paths.is_empty() {
        details.insert("dirty_paths".to_string(), paths.join(", "));
    }
    details
}

fn import_guidance_includes_active_branch(hint: &GitImportGuidance) -> bool {
    hint.missing_branches
        .iter()
        .any(|branch| branch == &hint.current_branch)
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct GitImportGuidanceReport {
    pub current_branch: String,
    pub missing_branch_count: usize,
    pub missing_branches: Vec<String>,
    pub recommended_command: String,
}

impl From<GitImportGuidance> for GitImportGuidanceReport {
    fn from(hint: GitImportGuidance) -> Self {
        Self {
            current_branch: hint.current_branch,
            missing_branch_count: hint.missing_branch_count,
            missing_branches: hint.missing_branches,
            recommended_command: hint.recommended_command,
        }
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct GitIndexPlan {
    pub commit_mode: &'static str,
    pub has_staged_changes: bool,
    pub staged_paths: Vec<String>,
    pub unstaged_paths: Vec<String>,
    pub untracked_paths: Vec<String>,
    pub will_commit: Vec<String>,
    pub preserved_after_commit: Vec<String>,
}

#[derive(Default)]
struct GitIndexIntent {
    staged_paths: Vec<String>,
    extra_paths: Vec<String>,
}

impl GitIndexPlan {
    fn from_intent(intent: &GitIndexIntent) -> Self {
        let (unstaged_paths, untracked_paths) = split_extra_paths(&intent.extra_paths);
        let has_staged_changes = !intent.staged_paths.is_empty();
        let mut will_commit = Vec::new();
        if has_staged_changes {
            will_commit.extend(intent.staged_paths.iter().cloned());
        } else {
            will_commit.extend(unstaged_paths.iter().cloned());
            will_commit.extend(untracked_paths.iter().cloned());
        }
        let preserved_after_commit = if has_staged_changes {
            intent.extra_paths.clone()
        } else {
            Vec::new()
        };
        Self {
            commit_mode: if has_staged_changes {
                "staged_index"
            } else {
                "worktree"
            },
            has_staged_changes,
            staged_paths: intent.staged_paths.clone(),
            unstaged_paths,
            untracked_paths,
            will_commit,
            preserved_after_commit,
        }
    }
}

const GIT_MODE_COMMIT: u32 = 0o160000;

pub fn git_index_plan_for_repo(repo: &Repository) -> Result<Option<GitIndexPlan>> {
    let Some(git) = repo.git_overlay_sley_repository()? else {
        return Ok(None);
    };
    if !git_worktree_matches_root(&git, repo.root()) {
        return Ok(None);
    }
    let ignore_patterns = repo.ignore_patterns()?;
    Ok(Some(GitIndexPlan::from_intent(
        &git_index_intent_for_root_with_ignore_and_repo(repo.root(), &ignore_patterns, &git)?,
    )))
}

/// Build a Git index plan for a worktree root without requiring a Heddle
/// repository (plain-Git observe path).
pub fn git_index_plan_for_root(root: &Path) -> Result<Option<GitIndexPlan>> {
    let git = match SleyRepository::discover(root) {
        Ok(git) => git,
        Err(_) => return Ok(None),
    };
    if !git_worktree_matches_root(&git, root) {
        return Ok(None);
    }
    let ignore_patterns = git_ignore_patterns_for_root(root, &git)?;
    Ok(Some(GitIndexPlan::from_intent(
        &git_index_intent_for_root_with_ignore_and_repo(root, &ignore_patterns, &git)?,
    )))
}

fn git_ignore_patterns_for_root(root: &Path, git: &SleyRepository) -> Result<Vec<String>> {
    let mut patterns = Vec::new();
    append_ignore_file_patterns(&mut patterns, &root.join(".gitignore"))?;
    append_ignore_file_patterns(&mut patterns, &git.git_dir().join("info").join("exclude"))?;
    Ok(patterns)
}

fn append_ignore_file_patterns(patterns: &mut Vec<String>, path: &Path) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let contents = fs::read_to_string(path).map_err(|err| {
        HeddleError::Config(format!(
            "failed to read ignore file {}: {err}",
            path.display()
        ))
    })?;
    for line in contents.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if !patterns.iter().any(|pattern| pattern == trimmed) {
            patterns.push(trimmed.to_string());
        }
    }
    Ok(())
}

fn git_worktree_matches_root(git: &SleyRepository, root: &Path) -> bool {
    git.workdir()
        .is_some_and(|workdir| paths_equal(&workdir, root))
}

fn split_extra_paths(extra_paths: &[String]) -> (Vec<String>, Vec<String>) {
    let mut unstaged_paths = Vec::new();
    let mut untracked_paths = Vec::new();
    for path in extra_paths {
        if let Some(path) = path.strip_prefix("unstaged: ") {
            unstaged_paths.push(path.to_string());
        } else if let Some(path) = path.strip_prefix("untracked: ") {
            untracked_paths.push(path.to_string());
        }
    }
    (unstaged_paths, untracked_paths)
}

fn git_index_intent_for_root_with_ignore_and_repo(
    root: &Path,
    ignore_patterns: &[String],
    git: &SleyRepository,
) -> Result<GitIndexIntent> {
    let ignore_matcher = build_worktree_ignore(ignore_patterns);
    let mut intent = GitIndexIntent::default();
    git.stream_short_status_with_options(
        ShortStatusOptions {
            untracked_mode: StatusUntrackedMode::All,
            ..ShortStatusOptions::default()
        },
        |entry| {
            append_status_row_to_index_intent(&mut intent, &ignore_matcher, entry);
            Ok(StreamControl::Continue)
        },
    )
    .map_err(|err| {
        HeddleError::Config(format!(
            "failed to inspect Git status before commit at {}: {err}",
            root.display()
        ))
    })?;
    Ok(intent)
}

fn append_status_row_to_index_intent(
    intent: &mut GitIndexIntent,
    ignore_matcher: &objects::worktree::WorktreeIgnoreMatcher,
    entry: ShortStatusRow<'_>,
) {
    let path = String::from_utf8_lossy(entry.path).into_owned();
    if path.is_empty() {
        return;
    }
    if entry.index == b'?' && entry.worktree == b'?' {
        if !ignore_matcher.is_ignored(Path::new(&path)) {
            intent.extra_paths.push(format!("untracked: {path}"));
        }
        return;
    }
    if entry.index != b' ' && entry.index != b'!' {
        intent.staged_paths.push(path.clone());
    }
    if entry.worktree != b' '
        && entry.worktree != b'!'
        && !status_row_is_gitlink_worktree_only(entry)
    {
        intent.extra_paths.push(format!("unstaged: {path}"));
    }
}

fn status_row_is_gitlink_worktree_only(entry: ShortStatusRow<'_>) -> bool {
    entry.index == b' '
        && (entry.index_mode == Some(GIT_MODE_COMMIT)
            || entry.head_mode == Some(GIT_MODE_COMMIT)
            || entry.worktree_mode == Some(GIT_MODE_COMMIT))
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct MaterializedThreadInfo {
    pub name: String,
    pub state_id: String,
    pub tree_hash_short: String,
    pub file_count: usize,
    pub stale: bool,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ActorInfo {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ParallelThreadInfo {
    pub name: String,
    pub coordination_status: CoordinationStatus,
    pub current_state: Option<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct StateInfo {
    pub change_id: String,
    pub content_hash: String,
    pub intent: Option<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct GitCheckpointInfo {
    pub git_commit: String,
    pub committed_at: String,
}

#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct ChangesInfo {
    pub modified: Vec<String>,
    pub added: Vec<String>,
    pub deleted: Vec<String>,
}

impl ChangesInfo {
    pub fn is_empty(&self) -> bool {
        self.modified.is_empty() && self.added.is_empty() && self.deleted.is_empty()
    }
}

#[derive(Debug, Clone, Copy, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum CoordinationStatus {
    Clean,
    Ahead,
    Diverged,
    Blocked,
    MergeReady,
}

impl std::fmt::Display for CoordinationStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Clean => write!(f, "clean"),
            Self::Ahead => write!(f, "ahead"),
            Self::Diverged => write!(f, "diverged"),
            Self::Blocked => write!(f, "blocked"),
            Self::MergeReady => write!(f, "merge-ready"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct StatusThreadSummary {
    pub name: String,
    pub base_state: Option<String>,
    pub base_root: Option<String>,
    pub current_state: Option<String>,
    pub path: Option<String>,
    pub execution_path: Option<String>,
    pub session_id: Option<String>,
    pub heddle_session_id: Option<String>,
    pub actor: Option<ActorInfo>,
    pub harness: Option<String>,
    pub thinking_level: Option<String>,
    pub usage_summary: Option<AgentUsageSummary>,
    pub last_progress_at: Option<String>,
    pub report_flush_state: Option<String>,
    pub attach_reason: Option<String>,
    pub thread_mode: Option<ThreadMode>,
    pub thread_state: Option<ThreadState>,
    pub freshness: Option<ThreadFreshness>,
    pub target_thread: Option<String>,
    pub parent_thread: Option<String>,
    pub child_threads: Vec<String>,
    pub task: Option<String>,
    pub promotion_suggested: bool,
    pub impact_categories: Vec<ThreadImpactCategory>,
    pub heavy_impact_paths: Vec<String>,
    pub changed_paths: Vec<String>,
    pub verification_summary: repo::ThreadVerificationSummary,
    pub confidence_summary: repo::ThreadConfidenceSummary,
    pub integration_policy_result: repo::ThreadIntegrationPolicy,
    pub coordination_status: CoordinationStatus,
    pub is_current: bool,
    pub is_isolated: bool,
}

pub fn collect_thread_summaries(repo: &Repository) -> Result<Vec<StatusThreadSummary>> {
    let thread_refs = repo.refs().list_threads()?;
    let current = repo.current_lane()?;
    let manager = ThreadManager::new(repo.heddle_dir());
    let mut names: BTreeSet<String> = thread_refs.iter().map(ToString::to_string).collect();
    names.extend(current.iter().cloned());
    names.extend(manager.list()?.into_iter().map(|thread| thread.thread));

    // Load the agent registry once for the whole summary walk. Per-thread
    // `AgentRegistry::list()` re-reads the same on-disk table and dominated
    // `thread_summary_ms` when many threads were present.
    let registry_entries = AgentRegistry::new(repo.heddle_dir()).list()?;

    let mut summaries = Vec::new();
    for name in names {
        if let Some(summary) = find_thread_summary_with_agents(repo, &name, &registry_entries)? {
            summaries.push(summary);
        }
    }
    let mut children_by_parent = std::collections::BTreeMap::<String, Vec<String>>::new();
    for summary in &summaries {
        if let Some(parent) = &summary.parent_thread {
            children_by_parent
                .entry(parent.clone())
                .or_default()
                .push(summary.name.clone());
        }
    }
    for summary in &mut summaries {
        summary.child_threads = children_by_parent
            .remove(&summary.name)
            .map(|mut children| {
                children.sort();
                children
            })
            .unwrap_or_default();
    }
    summaries.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(summaries)
}

pub fn find_thread_summary_single(
    repo: &Repository,
    name: &str,
) -> Result<Option<StatusThreadSummary>> {
    let registry_entries = AgentRegistry::new(repo.heddle_dir()).list()?;
    find_thread_summary_with_agents(repo, name, &registry_entries)
}

fn find_thread_summary_with_agents(
    repo: &Repository,
    name: &str,
    registry_entries: &[AgentEntry],
) -> Result<Option<StatusThreadSummary>> {
    let current = repo.current_lane()?;
    let is_current = current.as_deref() == Some(name);
    let manager = ThreadManager::new(repo.heddle_dir());
    let thread = manager.find_by_thread(name)?;
    let ref_state = repo.refs().get_thread(&ThreadName::new(name))?;
    if thread.is_none()
        && ref_state.is_none()
        && !(is_current && repo.capability() == RepositoryCapability::GitOverlay)
    {
        return Ok(None);
    }
    let mut thread =
        thread.unwrap_or_else(|| synthetic_thread(repo, name, ref_state.map(|id| id.short())));
    let _ = refresh_thread_freshness(repo, &mut thread);
    let entries: Vec<&AgentEntry> = registry_entries
        .iter()
        .filter(|entry| entry.thread == name)
        .collect();
    Ok(Some(thread_summary_from_thread(
        repo,
        thread,
        is_current,
        primary_agent_entry_refs(&entries),
    )))
}

fn synthetic_thread(repo: &Repository, name: &str, current_state: Option<String>) -> Thread {
    Thread {
        id: name.to_string(),
        thread: name.to_string(),
        target_thread: None,
        parent_thread: None,
        mode: ThreadMode::Materialized,
        state: ThreadState::Active,
        base_state: current_state.clone().unwrap_or_default(),
        base_root: String::new(),
        current_state,
        merged_state: None,
        task: None,
        execution_path: repo.root().to_path_buf(),
        materialized_path: None,
        changed_paths: Vec::new(),
        impact_categories: Vec::new(),
        heavy_impact_paths: Vec::new(),
        promotion_suggested: false,
        freshness: ThreadFreshness::Unknown,
        verification_summary: Default::default(),
        confidence_summary: Default::default(),
        integration_policy_result: Default::default(),
        created_at: Utc::now(),
        updated_at: Utc::now(),
        ephemeral: None,
        auto: false,
        shared_target_dir: None,
    }
}

fn thread_summary_from_thread(
    repo: &Repository,
    thread: Thread,
    is_current: bool,
    primary: Option<&AgentEntry>,
) -> StatusThreadSummary {
    let thread_state = thread.state;
    let coordination_status = coordination_status_for_thread_state(&thread_state);
    let path = thread
        .materialized_path
        .as_ref()
        .map(|path| path.display().to_string())
        .or_else(|| {
            primary
                .and_then(|entry| entry.path.as_ref())
                .map(|path| path.display().to_string())
        });
    let execution_path = if thread.execution_path == repo.root() {
        None
    } else {
        Some(thread.execution_path.display().to_string())
    };
    let git_backed_tip = is_current
        && repo.capability() == RepositoryCapability::GitOverlay
        && thread.current_state.is_none();
    StatusThreadSummary {
        name: thread.thread,
        base_state: non_empty(thread.base_state),
        base_root: non_empty(thread.base_root),
        current_state: thread.current_state,
        path,
        execution_path,
        session_id: primary.map(|entry| entry.session_id.clone()),
        heddle_session_id: primary.and_then(|entry| entry.heddle_session_id.clone()),
        actor: primary.and_then(|entry| match (&entry.provider, &entry.model) {
            (None, None) => None,
            (provider, model) => Some(ActorInfo {
                provider: provider.clone(),
                model: model.clone(),
            }),
        }),
        harness: primary.and_then(|entry| entry.harness.clone()),
        thinking_level: primary.and_then(|entry| entry.thinking_level.clone()),
        usage_summary: primary.map(|entry| entry.usage_summary.clone()),
        last_progress_at: primary
            .and_then(|entry| entry.last_progress_at)
            .map(|time| time.to_rfc3339()),
        report_flush_state: primary.and_then(|entry| entry.report_flush_state.clone()),
        attach_reason: primary
            .and_then(|entry| entry.attach_reason.clone())
            .or_else(|| git_backed_tip.then(|| "using Git-backed branch tip".to_string())),
        thread_mode: Some(thread.mode),
        thread_state: Some(thread_state),
        freshness: Some(thread.freshness),
        target_thread: thread.target_thread,
        parent_thread: thread.parent_thread,
        child_threads: Vec::new(),
        task: thread.task,
        promotion_suggested: thread.promotion_suggested,
        impact_categories: thread.impact_categories,
        heavy_impact_paths: thread.heavy_impact_paths,
        changed_paths: thread.changed_paths,
        verification_summary: thread.verification_summary,
        confidence_summary: thread.confidence_summary,
        integration_policy_result: thread.integration_policy_result,
        coordination_status,
        is_current,
        is_isolated: thread.materialized_path.is_some(),
    }
}

fn primary_agent_entry_refs<'a>(entries: &[&'a AgentEntry]) -> Option<&'a AgentEntry> {
    entries
        .iter()
        .copied()
        .filter(|entry| entry.status == AgentStatus::Active)
        .max_by_key(|entry| entry.started_at)
        .or_else(|| entries.iter().copied().max_by_key(|entry| entry.started_at))
}

fn non_empty(value: String) -> Option<String> {
    (!value.is_empty()).then_some(value)
}

fn coordination_status_for_thread_state(state: &ThreadState) -> CoordinationStatus {
    match state {
        ThreadState::Blocked => CoordinationStatus::Blocked,
        ThreadState::Ready => CoordinationStatus::MergeReady,
        ThreadState::Merged | ThreadState::Abandoned => CoordinationStatus::Clean,
        ThreadState::Active | ThreadState::Draft | ThreadState::Promoted => {
            CoordinationStatus::Clean
        }
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct FastShortStatusReport {
    pub subject: String,
    pub health: String,
    pub changes: ChangesInfo,
    #[serde(skip)]
    #[schemars(skip)]
    pub profile: FastShortStatusProfile,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct FastShortStatusProfile {
    pub git_discover_ms: u128,
    pub config_ms: u128,
    pub sley_status_ms: u128,
    pub branch_ms: u128,
    pub remote_ms: u128,
    pub total_ms: u128,
}

/// Typed plain-Git status observe report (no `.heddle` metadata yet).
///
/// Assembled by [`plain_git_status_report`]; CLI maps options, calls core, and
/// renders. Machine JSON uses this shape directly (including empty
/// `recommended_action` → `null`).
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct PlainGitStatusReport {
    pub output_kind: &'static str,
    pub repository_capability: String,
    pub repository_label: String,
    pub storage_model: String,
    pub heddle_initialized: bool,
    pub git_branch: Option<String>,
    pub path: String,
    #[serde(rename = "verification")]
    pub trust: RepositoryVerificationState,
    #[serde(serialize_with = "serialize_empty_action_as_null")]
    #[schemars(with = "Option<String>")]
    pub recommended_action: String,
    pub recommended_action_template: Option<ActionTemplate>,
    pub recovery_commands: Vec<String>,
    pub recovery_action_templates: Vec<ActionTemplate>,
    pub thread_health: String,
    pub changed_path_count: usize,
    pub changes: ChangesInfo,
    pub git_index: Option<GitIndexPlan>,
}

/// Build a plain-Git status report when `start` is a Git worktree without
/// Heddle metadata. Returns `Ok(None)` when the path is not a plain-Git observe
/// target (no Git, or `.heddle` already present).
pub fn plain_git_status_report(
    start: &Path,
    machine_contract_input: &MachineContractInput,
) -> Result<Option<PlainGitStatusReport>> {
    let Some(probe) =
        build_plain_git_verification_probe_with_machine_contract(start, machine_contract_input)?
    else {
        return Ok(None);
    };
    let changes = changes_from_worktree_status(&probe.changes);
    let changed_path_count = probe.changes.change_count();
    let trust = probe.trust;
    let git_index = git_index_plan_for_root(&probe.root)?;
    Ok(Some(PlainGitStatusReport {
        output_kind: "status",
        repository_capability: "plain-git".to_string(),
        repository_label: repository_mode_label("plain-git", "git-only"),
        storage_model: "git-only".to_string(),
        heddle_initialized: false,
        git_branch: probe.git_branch,
        path: probe.root.display().to_string(),
        recommended_action: trust.recommended_action.clone(),
        recommended_action_template: trust.recommended_action_template.clone(),
        recovery_commands: trust.recovery_commands.clone(),
        recovery_action_templates: trust.recovery_action_templates.clone(),
        thread_health: trust.status.clone(),
        changed_path_count,
        changes,
        git_index,
        trust,
    }))
}

pub fn status(ctx: &ExecutionContext, opts: StatusOptions) -> Result<StatusReport> {
    let fallback;
    let start = if let Some(start) = opts.start_path.as_deref() {
        start
    } else if let Some(start) = ctx.start_path() {
        start
    } else {
        fallback = std::env::current_dir().map_err(HeddleError::Io)?;
        fallback.as_path()
    };

    // When the caller already injected an open `Repository`, reuse it and
    // report `repo_open_ms = 0` so profiles stay truthful about open cost
    // inside this facade (callers that open in their shell attribute that
    // cost themselves).
    let opened;
    let (repo, repo_open_ms) = if let Some(repo) = ctx.repo() {
        (repo, 0)
    } else {
        let repo_open_start = Instant::now();
        opened = Repository::open(start)?;
        (&opened, repo_open_start.elapsed().as_millis())
    };
    let body_start = Instant::now();

    let current_state_start = Instant::now();
    let current_state = repo.current_state()?;
    let current_state_ms = current_state_start.elapsed().as_millis();

    let operation_start = Instant::now();
    let operation = repo.operation_status()?;
    let operation_ms = operation_start.elapsed().as_millis();

    let remote_tracking_start = Instant::now();
    let remote_tracking = if opts.detail.needs_remote_tracking() {
        repo.git_remote_tracking_status().unwrap_or(None)
    } else {
        None
    };
    let remote_tracking_ms = remote_tracking_start.elapsed().as_millis();

    let import_hint_start = Instant::now();
    let import_hint = if opts.detail.short_path() {
        None
    } else {
        repo.git_import_guidance().unwrap_or(None)
    };
    let import_hint_ms = import_hint_start.elapsed().as_millis();

    let git_overlay_status_start = Instant::now();
    let git_worktree_status_result = repo.git_overlay_worktree_status();
    let git_overlay_status_ms = git_overlay_status_start.elapsed().as_millis();

    let verification_start = Instant::now();
    let verification_health = build_repository_verification_health_with_worktree_status(
        repo,
        &git_worktree_status_result,
    );
    let trust = build_repository_verification_state_with_worktree_status_and_machine_contract(
        repo,
        verification_health.clone(),
        &git_worktree_status_result,
        &opts.machine_contract_input,
    );
    let verification_ms = verification_start.elapsed().as_millis();
    let remote_tracking =
        remote_tracking.map(|remote| remote_tracking_with_verification_action(remote, &trust));

    let git_worktree_status = git_worktree_status_result.unwrap_or(None);

    let git_index_start = Instant::now();
    let git_index = git_index_plan_for_repo(repo)?;
    let git_index_ms = git_index_start.elapsed().as_millis();

    let identity_notice = first_capture_identity_notice(ctx, repo, current_state.as_ref())?;
    let git_clean_mapping_blocker = matches!(
        trust.status.as_str(),
        "needs_import" | "needs_reconcile" | "git_branch_advanced"
    ) && git_worktree_status
        .as_ref()
        .is_some_and(WorktreeStatus::is_clean);
    let git_backed_mapping = trust.mapping_state == "git_backed";

    let worktree_status_start = Instant::now();
    let (changes, worktree_profile) = if git_clean_mapping_blocker {
        (ChangesInfo::default(), None)
    } else if let Some(status) = git_worktree_status.as_ref()
        && !status.is_clean()
        && trust.status != "needs_checkpoint"
    {
        (changes_from_worktree_status(status), None)
    } else if git_backed_mapping {
        (
            git_worktree_status
                .as_ref()
                .map(changes_from_worktree_status)
                .unwrap_or_default(),
            None,
        )
    } else if let Some(ref state) = current_state {
        let tree = repo.require_tree(&state.tree)?;
        let (status, profile) = repo
            .compare_worktree_cached_profiled_with_options(&tree, &opts.worktree_status_options)?;
        (changes_from_worktree_status(&status), Some(profile))
    } else if let Some(status) = git_worktree_status {
        (changes_from_worktree_status(&status), None)
    } else {
        let tree = objects::object::Tree::new();
        let (status, profile) = repo
            .compare_worktree_cached_profiled_with_options(&tree, &opts.worktree_status_options)?;
        let mut changes = changes_from_worktree_status(&status);
        changes.modified.clear();
        changes.deleted.clear();
        (changes, Some(profile))
    };
    let worktree_status_ms = worktree_status_start.elapsed().as_millis();

    if opts.detail.short_path() {
        return Ok(build_short_path_report(ShortPathInputs {
            repo,
            current_state: current_state.as_ref(),
            operation,
            remote_tracking,
            verification_health,
            trust,
            import_hint,
            git_index,
            identity_notice,
            changes,
            profile: StatusProfile {
                repo_open_ms,
                current_state_ms,
                operation_ms,
                remote_tracking_ms,
                import_hint_ms,
                git_overlay_status_ms,
                verification_ms,
                git_index_ms,
                worktree_status_ms,
                build_total_ms: body_start.elapsed().as_millis(),
                worktree_profile,
                ..StatusProfile::default()
            },
        }));
    }

    let thread_summary_start = Instant::now();
    let track_name = repo.current_lane()?;
    let full_thread_summaries = if opts.detail.needs_full_walk() {
        Some(collect_thread_summaries(repo)?)
    } else {
        None
    };
    let thread_summary = match (track_name.as_deref(), full_thread_summaries.as_ref()) {
        (Some(thread), Some(summaries)) => summaries
            .iter()
            .find(|summary| summary.name == thread)
            .cloned(),
        (Some(thread), None) => find_thread_summary_single(repo, thread)?,
        (None, _) => None,
    };
    let thread_summary_ms = thread_summary_start.elapsed().as_millis();

    let parallel_threads_start = Instant::now();
    let parallel_threads = if let Some(summaries) = full_thread_summaries {
        summaries
            .into_iter()
            .filter(|thread| !thread.is_current)
            .filter(|thread| {
                matches!(
                    thread.coordination_status,
                    CoordinationStatus::Ahead
                        | CoordinationStatus::Blocked
                        | CoordinationStatus::Diverged
                        | CoordinationStatus::MergeReady
                )
            })
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };
    let parallel_threads_ms = parallel_threads_start.elapsed().as_millis();

    let late_state_start = Instant::now();
    let state_info = current_state.as_ref().map(|s| StateInfo {
        change_id: s.change_id.short(),
        content_hash: s.compute_hash().short(),
        intent: s.intent.clone(),
    });
    let current_state_short = current_state.as_ref().map(|state| state.change_id.short());
    let git_checkpoint = if trust.status == "needs_checkpoint" {
        None
    } else {
        current_state
            .as_ref()
            .and_then(|state| {
                repo.latest_git_checkpoint_for_change(&state.change_id)
                    .ok()
                    .flatten()
            })
            .map(|record| GitCheckpointInfo {
                git_commit: record.git_commit,
                committed_at: record.committed_at,
            })
    };

    let materialized_start = Instant::now();
    let materialized_threads = assess_materialized_threads(repo);
    let materialized_ms = materialized_start.elapsed().as_millis();
    let target_thread = thread_summary
        .as_ref()
        .and_then(|thread| thread.target_thread.clone());
    let parent_thread = thread_summary
        .as_ref()
        .and_then(|thread| thread.parent_thread.clone());
    let presentation =
        crate::repository_presentation(repo, target_thread.as_deref(), parent_thread.as_deref());

    let output = StatusReport {
        output_kind: "status",
        repository_capability: repo.capability_label().to_string(),
        repository_label: presentation.label,
        repository_context: presentation.context,
        storage_model: repo.storage_model_label().to_string(),
        hosted_enabled: repo.hosted_enabled(),
        validation_capability: repo.capability(),
        import_guidance: import_hint.clone().map(Into::into),
        verification_health: verification_health.clone(),
        trust: trust.clone(),
        operation,
        remote_tracking,
        git_index,
        thread: track_name.clone(),
        base_state: thread_summary
            .as_ref()
            .and_then(|thread| thread.base_state.clone())
            .or_else(|| current_state_short.clone()),
        base_root: thread_summary
            .as_ref()
            .and_then(|thread| thread.base_root.clone()),
        current_state: thread_summary
            .as_ref()
            .and_then(|thread| thread.current_state.clone())
            .or_else(|| current_state_short.clone()),
        path: thread_summary
            .as_ref()
            .and_then(|thread| thread.path.clone()),
        execution_path: thread_summary
            .as_ref()
            .and_then(|thread| thread.execution_path.clone()),
        session_id: thread_summary
            .as_ref()
            .and_then(|thread| thread.session_id.clone()),
        heddle_session_id: thread_summary
            .as_ref()
            .and_then(|thread| thread.heddle_session_id.clone()),
        actor: thread_summary
            .as_ref()
            .and_then(|thread| thread.actor.clone()),
        harness: thread_summary
            .as_ref()
            .and_then(|thread| thread.harness.clone()),
        thinking_level: thread_summary
            .as_ref()
            .and_then(|thread| thread.thinking_level.clone()),
        usage_summary: thread_summary
            .as_ref()
            .and_then(|thread| thread.usage_summary.clone()),
        last_progress_at: thread_summary
            .as_ref()
            .and_then(|thread| thread.last_progress_at.clone()),
        report_flush_state: thread_summary
            .as_ref()
            .and_then(|thread| thread.report_flush_state.clone()),
        attach_reason: thread_summary
            .as_ref()
            .and_then(|thread| thread.attach_reason.clone()),
        thread_mode: thread_summary
            .as_ref()
            .and_then(|thread| thread.thread_mode.clone()),
        thread_state: thread_summary
            .as_ref()
            .and_then(|thread| thread.thread_state.clone()),
        freshness: thread_summary
            .as_ref()
            .and_then(|thread| thread.freshness.clone()),
        target_thread,
        parent_thread,
        child_threads: thread_summary
            .as_ref()
            .map(|thread| thread.child_threads.clone())
            .unwrap_or_default(),
        task: thread_summary
            .as_ref()
            .and_then(|thread| thread.task.clone()),
        promotion_suggested: thread_summary
            .as_ref()
            .map(|thread| thread.promotion_suggested)
            .unwrap_or(false),
        impact_categories: thread_summary
            .as_ref()
            .map(|thread| thread.impact_categories.clone())
            .unwrap_or_default(),
        heavy_impact_paths: thread_summary
            .as_ref()
            .map(|thread| thread.heavy_impact_paths.clone())
            .unwrap_or_default(),
        changed_paths: Vec::new(),
        changed_path_count: thread_summary
            .as_ref()
            .map(|thread| thread.changed_paths.len())
            .unwrap_or_default(),
        worktree_changed_path_count: changes_path_count(&changes),
        thread_changed_path_count: captured_thread_path_count(thread_summary.as_ref(), &changes),
        blockers: Vec::new(),
        identity_notice,
        recommended_action: String::new(),
        recommended_action_template: None,
        recovery_commands: trust.recovery_commands.clone(),
        recovery_action_templates: trust.recovery_action_templates.clone(),
        thread_health: "clean".to_string(),
        coordination_status: thread_summary
            .as_ref()
            .map(|thread| thread.coordination_status)
            .unwrap_or(CoordinationStatus::Clean),
        coordination_blocked_by_trust: false,
        is_isolated: thread_summary
            .as_ref()
            .map(|thread| thread.is_isolated)
            .unwrap_or(false),
        parallel_threads: parallel_threads
            .into_iter()
            .map(|thread| ParallelThreadInfo {
                name: thread.name,
                coordination_status: thread.coordination_status,
                current_state: thread.current_state,
            })
            .collect(),
        state: state_info,
        git_checkpoint,
        changes,
        materialized_threads,
        profile: StatusProfile::default(),
    };
    let late_state_ms = late_state_start.elapsed().as_millis();
    let advice_start = Instant::now();
    let mut output = apply_status_advice(
        repo,
        output,
        current_state.as_ref(),
        &thread_summary,
        import_hint,
        git_backed_mapping,
    );
    output.profile = StatusProfile {
        repo_open_ms,
        current_state_ms,
        operation_ms,
        remote_tracking_ms,
        import_hint_ms,
        git_overlay_status_ms,
        verification_ms,
        git_index_ms,
        worktree_status_ms,
        thread_summary_ms,
        parallel_threads_ms,
        late_state_ms,
        materialized_threads_ms: materialized_ms,
        advice_ms: advice_start.elapsed().as_millis(),
        build_total_ms: body_start.elapsed().as_millis(),
        worktree_profile,
    };
    Ok(output)
}

struct ShortPathInputs<'a> {
    repo: &'a Repository,
    current_state: Option<&'a State>,
    operation: Option<RepositoryOperationStatus>,
    remote_tracking: Option<GitRemoteTrackingStatus>,
    verification_health: RepositoryVerificationHealth,
    trust: RepositoryVerificationState,
    import_hint: Option<GitImportGuidance>,
    git_index: Option<GitIndexPlan>,
    identity_notice: Option<String>,
    changes: ChangesInfo,
    profile: StatusProfile,
}

fn build_short_path_report(input: ShortPathInputs<'_>) -> StatusReport {
    let recommended_action = effective_next_action(
        NextActionInput::default(
            input.operation.as_ref(),
            input.remote_tracking.as_ref(),
            None,
            None,
        )
        .with_verification(&input.trust),
    );
    let worktree_clean = input.changes.is_empty();
    let recommended_action =
        first_save_recommendation(input.repo, input.current_state, worktree_clean)
            .unwrap_or(recommended_action);
    let presentation = crate::repository_presentation(input.repo, None, None);
    let recommended_action_template = action_template(&recommended_action);
    // Short path still needs the current lane for prompt segments and short
    // subject lines; read it from the already-open repo (no second open).
    let thread = input.repo.current_lane().ok().flatten();
    StatusReport {
        output_kind: "status",
        repository_capability: input.repo.capability_label().to_string(),
        repository_label: presentation.label,
        repository_context: presentation.context,
        storage_model: input.repo.storage_model_label().to_string(),
        hosted_enabled: input.repo.hosted_enabled(),
        validation_capability: input.repo.capability(),
        import_guidance: input.import_hint.map(Into::into),
        verification_health: input.verification_health,
        trust: input.trust.clone(),
        operation: input.operation,
        remote_tracking: input.remote_tracking,
        git_index: input.git_index,
        thread,
        base_state: None,
        base_root: None,
        current_state: input.current_state.map(|state| state.change_id.short()),
        path: None,
        execution_path: None,
        session_id: None,
        heddle_session_id: None,
        actor: None,
        harness: None,
        thinking_level: None,
        usage_summary: None,
        last_progress_at: None,
        report_flush_state: None,
        attach_reason: None,
        thread_mode: None,
        thread_state: None,
        freshness: None,
        target_thread: None,
        parent_thread: None,
        child_threads: Vec::new(),
        task: None,
        promotion_suggested: false,
        impact_categories: Vec::new(),
        heavy_impact_paths: Vec::new(),
        changed_paths: changes_paths(&input.changes).into_iter().collect(),
        changed_path_count: changes_path_count(&input.changes),
        worktree_changed_path_count: changes_path_count(&input.changes),
        thread_changed_path_count: 0,
        blockers: if input.trust.verified {
            Vec::new()
        } else {
            input
                .trust
                .checks
                .iter()
                .filter(|check| {
                    !check.clean
                        && check.status != "not_checked"
                        && !check
                            .summary
                            .contains("checked after the primary verification blocker")
                })
                .map(|check| format!("{}: {}", check.name, check.summary))
                .collect()
        },
        identity_notice: input.identity_notice,
        recommended_action_template,
        recommended_action,
        recovery_commands: input.trust.recovery_commands.clone(),
        recovery_action_templates: input.trust.recovery_action_templates.clone(),
        thread_health: input.trust.status.clone(),
        coordination_status: if input.trust.verified {
            CoordinationStatus::Clean
        } else {
            CoordinationStatus::Blocked
        },
        coordination_blocked_by_trust: !input.trust.verified,
        is_isolated: false,
        parallel_threads: Vec::new(),
        state: None,
        git_checkpoint: None,
        changes: input.changes,
        materialized_threads: assess_materialized_threads(input.repo),
        profile: input.profile,
    }
}

fn apply_status_advice(
    repo: &Repository,
    output: StatusReport,
    current_state: Option<&State>,
    thread_summary: &Option<StatusThreadSummary>,
    import_hint: Option<GitImportGuidance>,
    git_backed_mapping: bool,
) -> StatusReport {
    let has_changes = !output.changes.is_empty();
    let checkpointed_clean = output.git_checkpoint.is_some() && !has_changes;
    let thread_stub = output.thread.as_ref().map(|thread| Thread {
        id: thread.clone(),
        thread: thread.clone(),
        target_thread: output.target_thread.clone(),
        parent_thread: thread_summary
            .as_ref()
            .and_then(|thread| thread.parent_thread.clone()),
        mode: output
            .thread_mode
            .clone()
            .unwrap_or(ThreadMode::Materialized),
        state: output.thread_state.clone().unwrap_or(ThreadState::Active),
        base_state: output.base_state.clone().unwrap_or_default(),
        base_root: output.base_root.clone().unwrap_or_default(),
        current_state: output.current_state.clone(),
        merged_state: None,
        task: output.task.clone(),
        execution_path: output
            .execution_path
            .as_ref()
            .map(PathBuf::from)
            .unwrap_or_else(|| repo.root().to_path_buf()),
        materialized_path: output.path.as_ref().map(PathBuf::from),
        changed_paths: thread_summary
            .as_ref()
            .map(|thread| thread.changed_paths.clone())
            .unwrap_or_default(),
        impact_categories: output.impact_categories.clone(),
        heavy_impact_paths: output.heavy_impact_paths.clone(),
        promotion_suggested: output.promotion_suggested && !checkpointed_clean,
        freshness: match output.freshness.clone().unwrap_or(ThreadFreshness::Unknown) {
            ThreadFreshness::Unknown if checkpointed_clean => ThreadFreshness::Current,
            freshness => freshness,
        },
        verification_summary: thread_summary
            .as_ref()
            .map(|thread| thread.verification_summary.clone())
            .unwrap_or_default(),
        confidence_summary: thread_summary
            .as_ref()
            .map(|thread| thread.confidence_summary.clone())
            .unwrap_or_default(),
        integration_policy_result: thread_summary
            .as_ref()
            .map(|thread| thread.integration_policy_result.clone())
            .unwrap_or_default(),
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
        ephemeral: None,
        auto: false,
        shared_target_dir: None,
    });
    let initial_state = current_state.map(is_synthetic_root).unwrap_or(true);
    let advice = thread_stub.as_ref().map(|thread| {
        describe_thread_advice_with_initial(thread, has_changes, 0, false, initial_state)
    });
    let mut trust = output.trust.clone();
    if let Some(operation) = output.operation.as_ref()
        && trust.recommended_action != operation.next_action
    {
        override_trust_recommended_action(&mut trust, operation.next_action.clone());
    }
    if has_changes
        && output.validation_capability != RepositoryCapability::GitOverlay
        && output.operation.is_none()
        && trust.verified
    {
        let dirty_paths = changes_paths(&output.changes)
            .into_iter()
            .collect::<Vec<_>>();
        let dirty_summary = format!(
            "{} Heddle worktree path(s) are not captured in the current state",
            dirty_paths.len()
        );
        trust.verified = false;
        trust.status = "uncaptured".to_string();
        trust.worktree_dirty = true;
        trust.worktree_state = "dirty".to_string();
        trust.summary = dirty_summary.clone();
        trust.recommended_action = "heddle commit -m \"...\"".to_string();
        trust.recommended_action_template = action_template(&trust.recommended_action);
        trust.recovery_commands = vec![trust.recommended_action.clone()];
        trust.recovery_action_templates = action_templates(&trust.recovery_commands);
        let mut details = BTreeMap::new();
        details.insert(
            "dirty_path_count".to_string(),
            dirty_paths.len().to_string(),
        );
        if !dirty_paths.is_empty() {
            details.insert("dirty_paths".to_string(), dirty_paths.join(", "));
        }
        let worktree_check = VerificationCheck {
            name: "Worktree".to_string(),
            status: "uncaptured".to_string(),
            clean: false,
            summary: dirty_summary,
            recommended_action: Some(trust.recommended_action.clone()),
            recommended_action_template: trust.recommended_action_template.clone(),
            recovery_commands: trust.recovery_commands.clone(),
            recovery_action_templates: trust.recovery_action_templates.clone(),
            details,
        };
        if let Some(check) = trust
            .checks
            .iter_mut()
            .find(|check| check.name == "Worktree")
        {
            *check = worktree_check;
        } else {
            trust.checks.insert(0, worktree_check);
        }
    }
    if trust.status != "needs_checkpoint"
        && let Some(thread) = output.thread.as_deref()
        && !trust.recommended_action.is_empty()
    {
        let contextual = contextual_thread_action(
            repo,
            thread,
            output.target_thread.as_deref(),
            &trust.recommended_action,
        );
        if contextual != trust.recommended_action {
            override_trust_recommended_action(&mut trust, contextual);
        }
    }
    let thread_health = advice.as_ref().map(|advice| advice.thread_health.as_str());
    let thread_action = advice
        .as_ref()
        .map(|advice| advice.recommended_action.as_str());
    let fallback = if trust.status == "needs_checkpoint" {
        non_empty_action(Some(trust.recommended_action.as_str()))
    } else {
        non_empty_action(thread_action)
            .or_else(|| non_empty_action(Some(trust.recommended_action.as_str())))
    };
    let recommended_action = effective_next_action(
        NextActionInput::default(
            output.operation.as_ref(),
            output.remote_tracking.as_ref(),
            import_hint.as_ref(),
            fallback,
        )
        .current_thread(thread_health)
        .with_verification(&trust),
    );
    let recommended_action = if trust.status != "needs_checkpoint"
        && let Some(thread) = output.thread.as_deref()
    {
        contextual_thread_action(
            repo,
            thread,
            output.target_thread.as_deref(),
            &recommended_action,
        )
    } else {
        recommended_action
    };
    if trust.verified
        && !recommended_action.is_empty()
        && trust.recommended_action != recommended_action
    {
        override_trust_recommended_action(&mut trust, recommended_action.clone());
    }
    let recommended_action =
        if git_backed_mapping && trust.status != "needs_checkpoint" && output.operation.is_none() {
            if has_changes {
                "heddle commit -m \"...\"".to_string()
            } else {
                String::new()
            }
        } else {
            if output.operation.is_some() {
                recommended_action
            } else {
                first_save_recommendation(repo, current_state, !has_changes)
                    .unwrap_or(recommended_action)
            }
        };
    let thread_health = if trust.verified {
        if git_backed_mapping {
            if has_changes {
                "dirty_worktree".to_string()
            } else {
                "clean".to_string()
            }
        } else {
            advice
                .as_ref()
                .map(|advice| advice.thread_health.clone())
                .unwrap_or_else(|| "clean".to_string())
        }
    } else {
        trust.status.clone()
    };
    let needs_checkpoint = trust.status == "needs_checkpoint";
    let mut trust_blockers = trust
        .checks
        .iter()
        .filter(|check| {
            !check.clean
                && check.status != "not_checked"
                && (check.name != "Clone" || check.status != "blocked")
                && !check
                    .summary
                    .contains("checked after the primary verification blocker")
        })
        .map(|check| {
            let name = if output.validation_capability != RepositoryCapability::GitOverlay
                && check.name == "Worktree"
                && check.status == "uncaptured"
            {
                "Verification"
            } else {
                check.name.as_str()
            };
            format!("{name}: {}", check.summary)
        })
        .collect::<Vec<_>>();
    let blocked_by_trust = !trust.verified;
    if blocked_by_trust && trust_blockers.is_empty() && !trust.summary.trim().is_empty() {
        trust_blockers.push(format!("Verification: {}", trust.summary));
    }
    let display_thread_summary = (!git_backed_mapping)
        .then_some(thread_summary.as_ref())
        .flatten();
    let worktree_changed_path_count = changes_path_count(&output.changes);
    let thread_changed_path_count =
        captured_thread_path_count(display_thread_summary, &output.changes);
    let (coordination_status, coordination_blocked_by_trust) = resolve_coordination_with_trust(
        output.coordination_status,
        blocked_by_trust,
        needs_checkpoint,
    );
    let recommended_action_template = action_template(&recommended_action);
    StatusReport {
        blockers: if blocked_by_trust {
            trust_blockers
        } else {
            advice
                .as_ref()
                .map(|advice| advice.blockers.clone())
                .unwrap_or_default()
        },
        identity_notice: output.identity_notice,
        recommended_action: recommended_action.clone(),
        recommended_action_template,
        recovery_commands: trust.recovery_commands.clone(),
        recovery_action_templates: trust.recovery_action_templates.clone(),
        thread_health,
        coordination_status,
        coordination_blocked_by_trust,
        thread_state: output.thread_state,
        changed_paths: changed_paths(display_thread_summary, &output.changes),
        changed_path_count: if trust.verified {
            changed_path_count(display_thread_summary, &output.changes)
        } else {
            changes_path_count(&output.changes)
        },
        worktree_changed_path_count,
        thread_changed_path_count,
        trust,
        ..output
    }
}

fn override_trust_recommended_action(trust: &mut RepositoryVerificationState, action: String) {
    let template = action_template(&action);
    trust.recommended_action = action.clone();
    trust.recommended_action_template = template.clone();
    if let Some(check) = trust
        .checks
        .iter_mut()
        .find(|check| check.name == "Workflow")
    {
        check.recommended_action = Some(action);
        check.recommended_action_template = template;
    }
}

fn paths_equal(left: &Path, right: &Path) -> bool {
    let left = left.canonicalize();
    let right = right.canonicalize();
    match (left, right) {
        (Ok(left), Ok(right)) => left == right,
        _ => false,
    }
}

fn first_capture_identity_notice(
    ctx: &ExecutionContext,
    repo: &Repository,
    current_state: Option<&State>,
) -> Result<Option<String>> {
    if !current_state.map(is_synthetic_root).unwrap_or(true) {
        return Ok(None);
    }
    let principal = resolve_principal(repo, ctx.config())?;
    if principal_is_default_unknown(&principal) {
        return Ok(Some(
            "no principal configured; the first capture/checkpoint would use Unknown <unknown@example.com>. Set HEDDLE_PRINCIPAL_NAME and HEDDLE_PRINCIPAL_EMAIL or run `heddle init --principal-name <name> --principal-email <email>`.".to_string(),
        ));
    }
    Ok(None)
}

fn resolve_principal(repo: &Repository, user_config: &cli_shared::UserConfig) -> Result<Principal> {
    if let Some(principal) = Principal::from_env() {
        return Ok(principal);
    }
    if let Some(config) = &repo.config().principal {
        return Ok(Principal::new(&config.name, &config.email));
    }
    let principal = repo.get_principal()?;
    if !principal_is_default_unknown(&principal) {
        return Ok(principal);
    }
    if let Some(config) = &user_config.principal {
        return Ok(Principal::new(&config.name, &config.email));
    }
    Ok(principal)
}

/// Whether principal is the built-in unknown placeholder (exact match).
pub fn principal_is_default_unknown(principal: &Principal) -> bool {
    principal.name == "Unknown" && principal.email == "unknown@example.com"
}

/// Broader refuse-to-capture identity check: empty fields or default unknown.
pub fn principal_lacks_accountable_identity(name: &str, email: &str) -> bool {
    let name = name.trim();
    let email = email.trim();
    name.is_empty() || email.is_empty() || (name == "Unknown" && email == "unknown@example.com")
}

/// Large-capture safety gate (Git-overlay worktree size).
///
/// Returns true when capture should require `--force`.
pub fn large_capture_requires_force(
    total_changes: usize,
    delete_count: usize,
    add_count: usize,
) -> bool {
    total_changes > 100 || delete_count > 25 || add_count > 100
}

pub fn fast_short_status_report(start: &Path) -> Result<Option<FastShortStatusReport>> {
    let total_start = Instant::now();
    let discover_start = Instant::now();
    let git = match SleyRepository::discover(start) {
        Ok(git) => git,
        Err(_) => return Ok(None),
    };
    let Some(workdir) = git.workdir() else {
        return Ok(None);
    };
    let git_discover_ms = discover_start.elapsed().as_millis();

    let config_start = Instant::now();
    let repo_kind = fast_short_repo_kind(&workdir)?;
    if matches!(repo_kind, FastShortRepoKind::Fallback) {
        return Ok(None);
    }
    let config_ms = config_start.elapsed().as_millis();

    let status_start = Instant::now();
    let changes = fast_sley_changes(&git)?;
    let sley_status_ms = status_start.elapsed().as_millis();

    let branch_start = Instant::now();
    let branch = fast_git_branch(&git)?;
    let subject = branch.as_deref().unwrap_or("detached").to_string();
    let branch_ms = branch_start.elapsed().as_millis();

    let remote_start = Instant::now();
    let remote_health = match repo_kind {
        FastShortRepoKind::PlainGit | FastShortRepoKind::Fallback => None,
        FastShortRepoKind::GitOverlay => branch
            .as_deref()
            .map(|branch| fast_remote_health(&git, branch))
            .transpose()?
            .flatten(),
    };
    let remote_ms = remote_start.elapsed().as_millis();
    let health = if changes.is_empty() {
        match repo_kind {
            FastShortRepoKind::PlainGit => "setup needed".to_string(),
            FastShortRepoKind::GitOverlay | FastShortRepoKind::Fallback => {
                remote_health.unwrap_or("clean").to_string()
            }
        }
    } else {
        String::new()
    };
    Ok(Some(FastShortStatusReport {
        subject,
        health,
        changes,
        profile: FastShortStatusProfile {
            git_discover_ms,
            config_ms,
            sley_status_ms,
            branch_ms,
            remote_ms,
            total_ms: total_start.elapsed().as_millis(),
        },
    }))
}

enum FastShortRepoKind {
    PlainGit,
    GitOverlay,
    Fallback,
}

fn fast_short_repo_kind(workdir: &Path) -> Result<FastShortRepoKind> {
    let heddle_dir = workdir.join(".heddle");
    if !heddle_dir.exists() {
        return Ok(FastShortRepoKind::PlainGit);
    }
    if heddle_dir.join("objectstore").is_file() {
        return Ok(FastShortRepoKind::Fallback);
    }
    let config_path = heddle_dir.join("config.toml");
    if !config_path.is_file() {
        return Ok(FastShortRepoKind::Fallback);
    }
    RepoConfig::load(&config_path)?;
    Ok(FastShortRepoKind::GitOverlay)
}

fn fast_sley_changes(git: &SleyRepository) -> Result<ChangesInfo> {
    let mut changes = ChangesInfo::default();
    git.stream_short_status_with_options(
        ShortStatusOptions {
            untracked_mode: StatusUntrackedMode::All,
            ..ShortStatusOptions::default()
        },
        |entry| {
            append_fast_status_row(&mut changes, entry);
            Ok(StreamControl::Continue)
        },
    )
    .map_err(sley_error)?;
    Ok(changes)
}

fn append_fast_status_row(changes: &mut ChangesInfo, entry: ShortStatusRow<'_>) {
    let path = String::from_utf8_lossy(entry.path).into_owned();
    if path.is_empty() || ignored_git_overlay_status_path(&path) {
        return;
    }
    if entry.index == b'?' && entry.worktree == b'?' {
        changes.added.push(path);
    } else if entry.index == b'D' || entry.worktree == b'D' {
        changes.deleted.push(path);
    } else if entry.index == b'A'
        || entry.index == b'R'
        || entry.index == b'C'
        || entry.head_oid.is_none()
    {
        changes.added.push(path);
    } else {
        changes.modified.push(path);
    }
}

fn ignored_git_overlay_status_path(path: &str) -> bool {
    path == ".heddle" || path.starts_with(".heddle/")
}

fn fast_git_branch(git: &SleyRepository) -> Result<Option<String>> {
    Ok(git
        .head()
        .ok()
        .and_then(|head| head.branch_name().map(str::to_string)))
}

fn fast_remote_health(git: &SleyRepository, branch: &str) -> Result<Option<&'static str>> {
    let Some(head) = git.head().ok().and_then(|head| head.oid) else {
        return Ok(None);
    };
    if git
        .find_reference(&format!("refs/heads/{branch}"))
        .map_err(sley_error)?
        .is_some()
        && let Some(tracking_ref) = fast_configured_tracking_ref(git, branch)?
        && let Some(upstream) = fast_rev_parse(git, &tracking_ref)
    {
        return fast_remote_health_for_pair(git, head, upstream);
    }

    let remotes = git.remote_names().map_err(sley_error)?;
    for remote in &remotes {
        if remote.trim().is_empty() {
            continue;
        }
        let remote_ref = format!("refs/remotes/{remote}/{branch}");
        let Some(upstream) = fast_rev_parse(git, &remote_ref) else {
            continue;
        };
        if upstream == head {
            return Ok(None);
        }
        return fast_remote_health_for_pair(git, head, upstream);
    }

    if remotes.is_empty() {
        Ok(None)
    } else {
        Ok(Some("ready to push"))
    }
}

fn fast_configured_tracking_ref(git: &SleyRepository, branch: &str) -> Result<Option<String>> {
    let config = git.config_snapshot().map_err(sley_error)?;
    let Some(remote) = config.get("branch", Some(branch), "remote") else {
        return Ok(None);
    };
    let Some(merge) = config.get("branch", Some(branch), "merge") else {
        return Ok(None);
    };
    if remote == "." {
        return Ok(Some(merge.to_string()));
    }
    let Some(short) = merge.strip_prefix("refs/heads/") else {
        return Ok(None);
    };
    Ok(Some(format!("refs/remotes/{remote}/{short}")))
}

fn fast_rev_parse(git: &SleyRepository, rev: &str) -> Option<sley::ObjectId> {
    git.rev_parse(rev).ok()
}

fn fast_remote_health_for_pair(
    git: &SleyRepository,
    head: sley::ObjectId,
    upstream: sley::ObjectId,
) -> Result<Option<&'static str>> {
    if head == upstream {
        return Ok(None);
    }
    let db = sley::ObjectDatabase::from_git_dir(git.common_dir(), git.object_format());
    let (ahead, behind) = sley::plumbing::sley_rev::ahead_behind_counts(
        git.git_dir(),
        git.object_format(),
        &db,
        &head,
        &upstream,
    )
    .map_err(sley_error)?;
    Ok(match (ahead, behind) {
        (0, 0) => None,
        (_, 0) => Some("ready to push"),
        (0, _) => Some("behind upstream"),
        _ => Some("remote_diverged"),
    })
}

fn sley_error(err: sley::GitError) -> HeddleError {
    HeddleError::Config(err.to_string())
}

pub fn assess_materialized_threads(repo: &Repository) -> Vec<MaterializedThreadInfo> {
    let summaries = match repo::thread_manifest::list_thread_manifests(repo.heddle_dir()) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    summaries
        .into_iter()
        .map(|summary| {
            let stale = match repo.refs().get_thread(&ThreadName::new(&summary.thread)) {
                Ok(Some(head)) => head != summary.state_id,
                _ => false,
            };
            let tree_hash = summary.tree_hash.to_string();
            MaterializedThreadInfo {
                name: summary.thread,
                state_id: summary.state_id.short(),
                tree_hash_short: tree_hash[..std::cmp::min(12, tree_hash.len())].to_string(),
                file_count: summary.file_count,
                stale,
            }
        })
        .collect()
}

pub fn changes_from_worktree_status(status: &WorktreeStatus) -> ChangesInfo {
    ChangesInfo {
        modified: status
            .modified
            .iter()
            .map(|p| p.display().to_string())
            .collect(),
        added: status
            .added
            .iter()
            .map(|p| p.display().to_string())
            .collect(),
        deleted: status
            .deleted
            .iter()
            .map(|p| p.display().to_string())
            .collect(),
    }
}

pub fn changes_path_count(changes: &ChangesInfo) -> usize {
    changes_paths(changes).len()
}

pub fn changes_paths(changes: &ChangesInfo) -> BTreeSet<String> {
    let mut paths = BTreeSet::new();
    paths.extend(changes.modified.iter().cloned());
    paths.extend(changes.added.iter().cloned());
    paths.extend(changes.deleted.iter().cloned());
    paths
}

fn changed_path_count(thread: Option<&StatusThreadSummary>, changes: &ChangesInfo) -> usize {
    let mut paths = BTreeSet::new();
    if let Some(thread) = thread {
        paths.extend(thread.changed_paths.iter().cloned());
    }
    paths.extend(changes.modified.iter().cloned());
    paths.extend(changes.added.iter().cloned());
    paths.extend(changes.deleted.iter().cloned());
    paths.len()
}

fn changed_paths(thread: Option<&StatusThreadSummary>, changes: &ChangesInfo) -> Vec<String> {
    let mut paths = BTreeSet::new();
    if let Some(thread) = thread {
        paths.extend(thread.changed_paths.iter().cloned());
    }
    paths.extend(changes.modified.iter().cloned());
    paths.extend(changes.added.iter().cloned());
    paths.extend(changes.deleted.iter().cloned());
    paths.into_iter().collect()
}

fn captured_thread_path_count(
    thread: Option<&StatusThreadSummary>,
    changes: &ChangesInfo,
) -> usize {
    let Some(thread) = thread else {
        return 0;
    };
    let dirty_paths = changes_paths(changes);
    thread
        .changed_paths
        .iter()
        .filter(|path| !dirty_paths.contains(*path))
        .count()
}

fn first_save_recommendation(
    repo: &Repository,
    current_state: Option<&State>,
    worktree_clean: bool,
) -> Option<String> {
    if !worktree_clean || repo.capability() != RepositoryCapability::NativeHeddle {
        return None;
    }
    let empty_log = current_state.map(is_synthetic_root).unwrap_or(true);
    empty_log.then(|| "heddle commit -m \"...\"".to_string())
}

fn remote_tracking_with_verification_action(
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
mod tests {
    use super::*;

    fn slow_path_bucket(row: &ShortStatusRow<'_>) -> &'static str {
        if row.index == b'?' && row.worktree == b'?' {
            "added"
        } else if row.index == b'D' || row.worktree == b'D' {
            "deleted"
        } else if row.index == b'A'
            || row.index == b'R'
            || row.index == b'C'
            || row.head_oid.is_none()
        {
            "added"
        } else {
            "modified"
        }
    }

    fn fast_path_bucket(row: ShortStatusRow<'_>) -> &'static str {
        let mut changes = ChangesInfo::default();
        append_fast_status_row(&mut changes, row);
        match (
            changes.added.len(),
            changes.deleted.len(),
            changes.modified.len(),
        ) {
            (1, 0, 0) => "added",
            (0, 1, 0) => "deleted",
            (0, 0, 1) => "modified",
            other => panic!("fast path produced unexpected bucket counts: {other:?}"),
        }
    }

    fn status_row<'a>(
        index: u8,
        worktree: u8,
        path: &'a [u8],
        in_head: bool,
    ) -> ShortStatusRow<'a> {
        ShortStatusRow {
            index,
            worktree,
            path,
            head_mode: None,
            index_mode: None,
            worktree_mode: None,
            head_oid: in_head.then(|| sley::ObjectId::null(sley::ObjectFormat::Sha1)),
            index_oid: None,
            submodule: None,
        }
    }

    #[test]
    fn fast_short_status_agrees_with_slow_path_on_ad_rename_copy() {
        let cases: &[(u8, u8, bool, &str)] = &[
            (b'A', b'D', false, "AD: staged-add then worktree-deleted"),
            (b'R', b' ', true, "R: renamed"),
            (b'C', b' ', true, "C: copied"),
            (b'A', b' ', false, "A: staged add"),
            (b'M', b' ', true, "M: modified"),
            (b' ', b'M', true, "worktree-modified"),
            (b'D', b' ', true, "D: staged delete"),
            (b' ', b'D', true, "worktree delete"),
            (b'?', b'?', false, "untracked"),
        ];
        for &(index, worktree, in_head, label) in cases {
            let path = label.as_bytes();
            let fast = fast_path_bucket(status_row(index, worktree, path, in_head));
            let slow = slow_path_bucket(&status_row(index, worktree, path, in_head));
            assert_eq!(
                fast, slow,
                "fast and slow short-status classification disagree for {label}",
            );
        }
    }

    #[test]
    fn status_uses_injected_repo_without_reopening_start_path() {
        let temp = tempfile::tempdir().expect("temp repo");
        repo::Repository::init_default(temp.path()).expect("init repo");
        let repo = Repository::open(temp.path()).expect("open repo");
        // If status re-opened from start_path it would fail — prove injection.
        let bogus = temp.path().join("not-a-repo-start");
        let ctx = ExecutionContext::builder()
            .start_path(&bogus)
            .repo(repo)
            .build();

        let report = status(
            &ctx,
            StatusOptions::new(
                StatusDetail::ShortText,
                repo::WorktreeStatusOptions::default(),
            )
            .with_start_path(&bogus),
        )
        .expect("status with injected repo must not re-open start_path");

        assert_eq!(report.output_kind, "status");
        assert_eq!(
            report.profile.repo_open_ms, 0,
            "injected repo must report zero facade open cost"
        );
        assert!(!report.trust.status.is_empty());
    }

    #[test]
    fn status_default_core_path_produces_complete_embedder_report() {
        let temp = tempfile::tempdir().expect("temp repo");
        repo::Repository::init_default(temp.path()).expect("init repo");
        let ctx = ExecutionContext::builder().start_path(temp.path()).build();

        let report = status(
            &ctx,
            StatusOptions::new(
                StatusDetail::DefaultText,
                repo::WorktreeStatusOptions::default(),
            )
            .with_start_path(temp.path()),
        )
        .expect("core status");

        assert_eq!(report.output_kind, "status");
        assert!(!report.repository_label.is_empty());
        assert!(!report.verification_health.status.is_empty());
        assert!(!report.trust.status.is_empty());
        assert_eq!(report.trust.machine_contract, "not_checked");
        assert_eq!(report.trust.machine_contract_coverage.status, "not_checked");
        assert!(
            report
                .trust
                .checks
                .iter()
                .any(|check| check.name == "Machine contract" && check.status == "not_checked")
        );
    }

    #[test]
    fn verify_default_core_path_produces_complete_embedder_report() {
        let temp = tempfile::tempdir().expect("temp repo");
        repo::Repository::init_default(temp.path()).expect("init repo");
        let ctx = ExecutionContext::builder().start_path(temp.path()).build();

        let report = crate::verify::verify(
            &ctx,
            crate::verify::VerifyOptions::new().with_start_path(temp.path()),
        )
        .expect("core verify");

        assert_eq!(report.output_kind, "verify");
        assert!(!report.repository_label.is_empty());
        assert!(report.trust.heddle_initialized);
        assert!(!report.trust.status.is_empty());
        assert_eq!(report.trust.machine_contract, "not_checked");
        assert_eq!(report.trust.machine_contract_coverage.status, "not_checked");
        assert!(
            report
                .trust
                .checks
                .iter()
                .any(|check| check.name == "Machine contract" && check.status == "not_checked")
        );
    }

    /// Empty `recommended_action` must serialize as `null`, never `""` — the
    /// serialization-boundary walker hard-fails the whole command on a raw
    /// empty. Pins the safe-by-construction wire shape for plain-Git status.
    #[test]
    fn plain_git_status_serializes_empty_recommended_action_as_null() {
        let trust = RepositoryVerificationState {
            verified: true,
            status: "verified".to_string(),
            repository_mode: "plain-git".to_string(),
            heddle_initialized: false,
            git_branch: Some("main".to_string()),
            heddle_thread: None,
            worktree_dirty: false,
            worktree_state: "clean".to_string(),
            import_state: "not_applicable".to_string(),
            mapping_state: "not_applicable".to_string(),
            remote_drift: "clean".to_string(),
            active_operation: None,
            default_remote: None,
            clone_verification: "not_applicable".to_string(),
            machine_contract: "not_checked".to_string(),
            machine_contract_coverage: MachineContractInput::default().coverage,
            workflow_status: "clean".to_string(),
            workflow_summary: "no ready threads are waiting to land".to_string(),
            summary: "plain Git repository".to_string(),
            recommended_action: String::new(),
            recommended_action_template: None,
            recovery_commands: Vec::new(),
            recovery_action_templates: Vec::new(),
            checks: Vec::new(),
        };
        let output = PlainGitStatusReport {
            output_kind: "status",
            repository_capability: "plain-git".to_string(),
            repository_label: repository_mode_label("plain-git", "git-only"),
            storage_model: "git-only".to_string(),
            heddle_initialized: false,
            git_branch: Some("main".to_string()),
            path: "/tmp/repo".to_string(),
            recommended_action: trust.recommended_action.clone(),
            recommended_action_template: trust.recommended_action_template.clone(),
            recovery_commands: trust.recovery_commands.clone(),
            recovery_action_templates: trust.recovery_action_templates.clone(),
            thread_health: trust.status.clone(),
            changed_path_count: 0,
            changes: ChangesInfo::default(),
            git_index: None,
            trust,
        };

        let value = serde_json::to_value(&output).unwrap();
        assert!(value["recommended_action"].is_null());
        assert!(value["verification"]["recommended_action"].is_null());
    }

    #[test]
    fn plain_git_status_report_assembles_for_git_only_worktree() {
        let temp = tempfile::tempdir().expect("temp dir");
        let root = temp.path();
        SleyRepository::init(root).expect("init plain git repository");
        fs::write(root.join("README"), "hello\n").expect("write file");

        let report = plain_git_status_report(root, &MachineContractInput::default())
            .expect("plain git status")
            .expect("probe present");
        assert_eq!(report.output_kind, "status");
        assert_eq!(report.repository_capability, "plain-git");
        assert_eq!(report.storage_model, "git-only");
        assert!(!report.heddle_initialized);
        assert!(!report.repository_label.is_empty());
        assert!(!report.trust.status.is_empty());
        assert!(report.changed_path_count > 0 || !report.changes.is_empty());
    }

    #[test]
    fn plain_git_status_report_skips_heddle_repos() {
        let temp = tempfile::tempdir().expect("temp repo");
        repo::Repository::init_default(temp.path()).expect("init repo");
        let report = plain_git_status_report(temp.path(), &MachineContractInput::default())
            .expect("plain git status");
        assert!(report.is_none());
    }
}
