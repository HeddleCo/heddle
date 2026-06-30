// SPDX-License-Identifier: Apache-2.0
//! Repository verification facade.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    time::Instant,
};

use ::objects::{HeddleError, error::Result, worktree::WorktreeStatus};
use repo::{Repository, Thread, ThreadManager};
use schemars::JsonSchema;
use serde::{Serialize, Serializer};

use crate::{
    ExecutionContext, HeddleReport, MachineOutputKind, OutputDiscriminator, ReportContract,
    schema_for_report,
};

pub type PlainGitProbeFn = fn(&Path) -> Result<Option<PlainGitVerifyProbe>>;
pub type RepositoryTrustFn = fn(&Repository) -> Result<RepositoryVerificationState>;

#[derive(Clone)]
pub struct VerifyOptions {
    pub start_path: Option<PathBuf>,
    pub plain_git_probe: PlainGitProbeFn,
    pub repository_trust: RepositoryTrustFn,
}

impl VerifyOptions {
    pub fn new(plain_git_probe: PlainGitProbeFn, repository_trust: RepositoryTrustFn) -> Self {
        Self {
            start_path: None,
            plain_git_probe,
            repository_trust,
        }
    }

    pub fn with_start_path(mut self, start_path: impl Into<PathBuf>) -> Self {
        self.start_path = Some(start_path.into());
        self
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq, Eq)]
pub struct VerifyReport {
    pub output_kind: &'static str,
    pub clean: bool,
    pub repository_label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repository_context: Option<RepositoryContextInfo>,
    #[serde(flatten)]
    pub trust: RepositoryVerificationState,
    #[serde(skip)]
    #[schemars(skip)]
    pub profile: VerifyProfile,
}

impl VerifyReport {
    pub const CONTRACT: ReportContract = ReportContract {
        schema_name: "verify",
        machine_output_kind: MachineOutputKind::Json,
        output_discriminator: Some(OutputDiscriminator {
            field: "output_kind",
            value: "verify",
        }),
        schema: schema_for_report::<VerifyReport>,
    };
}

impl HeddleReport for VerifyReport {
    const CONTRACT: ReportContract = VerifyReport::CONTRACT;
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct VerifyProfile {
    pub plain_git_probe_ms: u128,
    pub repo_open_ms: u128,
    pub verification_ms: u128,
}

#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq, Eq)]
pub struct RepositoryContextInfo {
    pub kind: String,
    pub parent_repository: Option<String>,
    pub target_thread: Option<String>,
    pub parent_thread: Option<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq, Eq)]
pub struct RepositoryPresentation {
    pub label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context: Option<RepositoryContextInfo>,
}

#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq, Eq)]
pub struct PlainGitVerifyProbe {
    pub trust: RepositoryVerificationState,
}

#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq, Eq)]
pub struct ActionTemplate {
    pub action: String,
    pub argv_template: Vec<String>,
    pub required_inputs: Vec<String>,
    /// Whether an agent may replace placeholders in `argv_template`.
    ///
    /// When `agent_may_fill` is false, treat `action` and `argv_template` as
    /// display-only: do not substitute `<name>`/`<url>` placeholders. Surface
    /// the template to a human or discard it. Substituting and running it will
    /// pass literal `<name>` to Heddle and fail.
    pub agent_may_fill: bool,
}

#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq, Eq)]
pub struct RepositoryVerificationState {
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

pub fn serialize_empty_action_as_null<S>(
    action: &String,
    serializer: S,
) -> std::result::Result<S::Ok, S::Error>
where
    S: Serializer,
{
    if action.is_empty() {
        serializer.serialize_none()
    } else {
        serializer.serialize_some(action)
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq, Eq)]
pub struct MachineContractCoverage {
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
    pub verified_scope_missing_schema_examples: Vec<String>,
    pub verified_scope_accepted_opaque_schema_examples: Vec<String>,
    pub advanced_scope_accepted_opaque_schema_examples: Vec<String>,
    pub accepted_opaque_schema_examples: Vec<String>,
    pub unaccepted_opaque_schema_examples: Vec<String>,
    pub undocumented_schema_examples: Vec<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq, Eq)]
pub struct VerificationCheck {
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

pub fn verify(ctx: &ExecutionContext, opts: VerifyOptions) -> Result<VerifyReport> {
    let fallback;
    let start = if let Some(start) = opts.start_path.as_deref() {
        start
    } else if let Some(start) = ctx.start_path() {
        start
    } else {
        fallback = std::env::current_dir().map_err(HeddleError::Io)?;
        fallback.as_path()
    };

    let probe_start = Instant::now();
    let plain_git_probe = (opts.plain_git_probe)(start)?;
    let plain_git_probe_ms = probe_start.elapsed().as_millis();
    let mut profile = VerifyProfile {
        plain_git_probe_ms,
        ..VerifyProfile::default()
    };

    if let Some(probe) = plain_git_probe {
        return Ok(VerifyReport {
            output_kind: "verify",
            clean: probe.trust.verified,
            repository_label: repository_mode_label("plain-git", "git-only"),
            repository_context: None,
            trust: probe.trust,
            profile,
        });
    }

    let opened;
    let repo = if let Some(repo) = ctx.repo() {
        repo
    } else {
        let repo_open_start = Instant::now();
        opened = Repository::open(start)?;
        profile.repo_open_ms = repo_open_start.elapsed().as_millis();
        &opened
    };
    let verification_start = Instant::now();
    let trust = (opts.repository_trust)(repo)?;
    profile.verification_ms = verification_start.elapsed().as_millis();
    let presentation = repository_presentation(repo, None, None);
    Ok(VerifyReport {
        output_kind: "verify",
        clean: trust.verified,
        repository_label: presentation.label,
        repository_context: presentation.context,
        trust,
        profile,
    })
}

/// Human-facing repository mode label. JSON keeps the exact repository mode
/// values; text output uses product language instead of storage implementation
/// names.
pub fn repository_mode_label(capability: &str, storage_model: &str) -> String {
    if capability == "git-overlay" || storage_model == "git+heddle-sidecar" {
        "Git + Heddle".to_string()
    } else if capability == "plain-git" || storage_model == "git-only" {
        "Git repo (setup needed)".to_string()
    } else if capability == "native"
        || capability == "native-heddle"
        || storage_model == "heddle-native"
    {
        "Heddle native".to_string()
    } else {
        capability.to_string()
    }
}

/// Presentation-only repository identity. This deliberately leaves
/// `Repository::capability_label()` untouched: an isolated checkout that shares
/// a Git-overlay object store is still technically opened through the native
/// Heddle storage path, but user-facing status should say what manages it.
pub fn repository_presentation(
    repo: &Repository,
    target_thread: Option<&str>,
    parent_thread: Option<&str>,
) -> RepositoryPresentation {
    if let Some(parent_root) = managed_git_overlay_parent_root(repo) {
        let thread = current_child_thread(repo);
        let target_thread = target_thread.map(ToString::to_string).or_else(|| {
            thread
                .as_ref()
                .and_then(|thread| thread.target_thread.clone())
        });
        let parent_thread = parent_thread.map(ToString::to_string).or_else(|| {
            thread
                .as_ref()
                .and_then(|thread| thread.parent_thread.clone())
        });
        return RepositoryPresentation {
            label: "Git + Heddle isolated checkout".to_string(),
            context: Some(RepositoryContextInfo {
                kind: "git-overlay-isolated-checkout".to_string(),
                parent_repository: Some(parent_root.display().to_string()),
                target_thread,
                parent_thread,
            }),
        };
    }

    RepositoryPresentation {
        label: repository_mode_label(repo.capability_label(), repo.storage_model_label()),
        context: None,
    }
}

fn managed_git_overlay_parent_root(repo: &Repository) -> Option<PathBuf> {
    let parent_root = repo.heddle_dir().parent()?;
    if paths_equal(parent_root, repo.root()) {
        return None;
    }
    parent_root
        .join(".git")
        .exists()
        .then(|| parent_root.to_path_buf())
}

fn current_child_thread(repo: &Repository) -> Option<Thread> {
    let manager = ThreadManager::new(repo.heddle_dir());
    if let Ok(Some(thread)) = manager.find_by_execution_root(repo.root()) {
        return Some(thread);
    }
    let lane = repo.current_lane().ok().flatten()?;
    manager.find_by_thread(&lane).ok().flatten()
}

fn paths_equal(left: &Path, right: &Path) -> bool {
    let left = left.canonicalize().unwrap_or_else(|_| left.to_path_buf());
    let right = right.canonicalize().unwrap_or_else(|_| right.to_path_buf());
    left == right
}

pub fn dirty_path_count(status: &WorktreeStatus) -> usize {
    status.modified.len() + status.added.len() + status.deleted.len()
}
