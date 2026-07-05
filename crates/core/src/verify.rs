// SPDX-License-Identifier: Apache-2.0
//! Repository verification facade.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    time::Instant,
};

use ::objects::{HeddleError, error::Result, worktree::WorktreeStatus};
use repo::{
    Repository, Thread, ThreadManager, describe_thread_advice,
    git_worktree_status::GitWorktreeEntryState, refresh_thread_freshness,
};
use schemars::JsonSchema;
use serde::{Serialize, Serializer};

use crate::{
    ExecutionContext, HeddleReport, MachineOutputKind, OutputDiscriminator, ReportContract,
    schema_for_report,
};

use crate::status::{
    GitOverlayHealth, build_git_overlay_health_with_worktree_status, default_remote_name,
    git_default_remote_name_from_repo,
};
use crate::status::next_action::remote_tracking_status;
use sley::{BString as GitBString, Index, Repository as SleyRepository};

#[derive(Clone)]
pub struct VerifyOptions {
    pub start_path: Option<PathBuf>,
    pub machine_contract_input: MachineContractInput,
    pub action_audience: ActionAudience,
}

impl VerifyOptions {
    pub fn new() -> Self {
        Self {
            start_path: None,
            machine_contract_input: MachineContractInput::default(),
            action_audience: ActionAudience::Human,
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

    pub fn with_action_audience(mut self, audience: ActionAudience) -> Self {
        self.action_audience = audience;
        self
    }
}

impl Default for VerifyOptions {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, Copy, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ActionAudience {
    Human,
    Agent,
    Script,
}

#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq, Eq)]
pub struct MachineContractInput {
    pub coverage: MachineContractCoverage,
}

impl MachineContractInput {
    pub fn from_coverage(coverage: MachineContractCoverage) -> Self {
        Self { coverage }
    }
}

impl Default for MachineContractInput {
    fn default() -> Self {
        Self {
            coverage: MachineContractCoverage::not_checked(),
        }
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq, Eq)]
pub struct VerifyReport {
    pub output_kind: &'static str,
    pub clean: bool,
    pub repository_label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repository_context: Option<RepositoryContextInfo>,
    #[serde(rename = "verification")]
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

#[derive(Debug, Serialize, JsonSchema)]
pub struct PlainGitVerifyProbe {
    #[schemars(with = "String")]
    pub root: PathBuf,
    pub git_branch: Option<String>,
    #[serde(skip)]
    #[schemars(skip)]
    pub changes: WorktreeStatus,
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

impl MachineContractCoverage {
    pub fn not_checked() -> Self {
        Self {
            status: "not_checked".to_string(),
            verified_scope: "not_checked".to_string(),
            advanced_scope: "not_checked".to_string(),
            summary: "Machine-contract proof was not supplied by this embedder".to_string(),
            catalog_commands_total: 0,
            catalog_mutating_commands_total: 0,
            json_commands_total: 0,
            json_mutating_commands_total: 0,
            json_commands_with_schema: 0,
            json_commands_with_accepted_opaque_schema: 0,
            json_commands_without_schema: 0,
            verified_scope_json_commands_total: 0,
            verified_scope_json_commands_with_schema: 0,
            verified_scope_json_commands_with_accepted_opaque_schema: 0,
            verified_scope_json_commands_without_schema: 0,
            advanced_scope_json_commands_total: 0,
            advanced_scope_json_commands_with_accepted_opaque_schema: 0,
            mutating_commands_total: 0,
            mutating_commands_with_schema: 0,
            mutating_commands_with_accepted_opaque_schema: 0,
            mutating_commands_without_schema: 0,
            verified_scope_mutating_commands_total: 0,
            verified_scope_mutating_commands_with_schema: 0,
            verified_scope_mutating_commands_with_accepted_opaque_schema: 0,
            verified_scope_mutating_commands_without_schema: 0,
            advanced_scope_mutating_commands_total: 0,
            advanced_scope_mutating_commands_with_accepted_opaque_schema: 0,
            schema_verbs_total: 0,
            documented_schema_verbs_total: 0,
            undocumented_schema_verbs_total: 0,
            opaque_schema_verbs_total: 0,
            accepted_opaque_schema_verbs_total: 0,
            unaccepted_opaque_schema_verbs_total: 0,
            supports_op_id_total: 0,
            jsonl_commands_total: 0,
            missing_schema_examples: Vec::new(),
            missing_mutating_schema_examples: Vec::new(),
            verified_scope_missing_schema_examples: Vec::new(),
            verified_scope_accepted_opaque_schema_examples: Vec::new(),
            advanced_scope_accepted_opaque_schema_examples: Vec::new(),
            accepted_opaque_schema_examples: Vec::new(),
            unaccepted_opaque_schema_examples: Vec::new(),
            undocumented_schema_examples: Vec::new(),
        }
    }
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

pub fn build_plain_git_verification_probe(start: &Path) -> Result<Option<PlainGitVerifyProbe>> {
    build_plain_git_verification_probe_with_machine_contract(
        start,
        &MachineContractInput::default(),
    )
}

pub fn build_plain_git_verification_probe_with_machine_contract(
    start: &Path,
    machine_contract_input: &MachineContractInput,
) -> Result<Option<PlainGitVerifyProbe>> {
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
    let setup_action = "heddle init".to_string();
    let recovery_commands = vec![setup_action.clone()];
    let machine_contract_coverage = machine_contract_input.coverage.clone();
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
            recommended_action_template: action_template(&setup_action),
            recovery_commands: recovery_commands.clone(),
            recovery_action_templates: action_templates(&recovery_commands),
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
    checks.push(machine_contract_verification_check(&machine_contract_coverage));
    checks.push(verification_check(
        "Clone",
        true,
        "not_applicable",
        "clone verification is not applicable to this checkout",
        None,
        Vec::new(),
    ));

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
        recommended_action: setup_action.clone(),
        recommended_action_template: action_template(&setup_action),
        recovery_commands: recovery_commands.clone(),
        recovery_action_templates: action_templates(&recovery_commands),
        checks,
    };
    Ok(Some(PlainGitVerifyProbe {
        root,
        git_branch,
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

fn plain_git_worktree_status(root: &Path, git_repo: &SleyRepository) -> Result<WorktreeStatus> {
    let index = plain_git_index_or_empty(git_repo).map_err(|error| {
        HeddleError::Config(format!(
            "failed to inspect Git index at '{}': {error}",
            root.display()
        ))
    })?;
    let head_index = plain_git_head_index_or_empty(git_repo).map_err(|error| {
        HeddleError::Config(format!(
            "failed to inspect Git HEAD tree at '{}': {error}",
            root.display()
        ))
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

fn plain_git_untracked_paths(root: &Path, tracked_paths: &BTreeSet<&str>) -> Result<Vec<String>> {
    let mut paths = Vec::new();
    let walker = ignore::WalkBuilder::new(root)
        .hidden(false)
        .git_ignore(true)
        .git_exclude(true)
        .git_global(true)
        .filter_entry(|entry| !plain_git_excluded_walk_entry(entry.path()))
        .build();
    for entry in walker {
        let entry = entry.map_err(|error| HeddleError::Config(error.to_string()))?;
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

fn plain_git_repo_relative_path(root: &Path, path: &Path) -> Result<String> {
    let relative = path.strip_prefix(root).map_err(|error| {
        HeddleError::Config(format!(
            "failed to relativize Git worktree path '{}': {}",
            path.display(),
            error
        ))
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

pub fn build_repository_verification_state(
    repo: &Repository,
) -> Result<RepositoryVerificationState> {
    build_repository_verification_state_with_machine_contract(
        repo,
        &MachineContractInput::default(),
    )
}

pub fn build_repository_verification_state_with_machine_contract(
    repo: &Repository,
    machine_contract_input: &MachineContractInput,
) -> Result<RepositoryVerificationState> {
    let worktree_status = if repo.capability() == repo::RepositoryCapability::GitOverlay {
        repo.git_overlay_worktree_status()
    } else {
        native_worktree_status(repo)
    };
    let health = build_git_overlay_health_with_worktree_status(repo, &worktree_status);
    Ok(build_repository_verification_state_with_worktree_status_and_machine_contract(
        repo,
        health,
        &worktree_status,
        machine_contract_input,
    ))
}

fn native_worktree_status(repo: &Repository) -> Result<Option<WorktreeStatus>> {
    let Some(state) = repo.current_state()? else {
        return Ok(Some(WorktreeStatus::default()));
    };
    let tree = repo.require_tree(&state.tree)?;
    repo.compare_worktree_cached(&tree).map(Some)
}

pub fn build_repository_verification_state_with_worktree_status(
    repo: &Repository,
    health: GitOverlayHealth,
    worktree_status: &Result<Option<WorktreeStatus>>,
) -> RepositoryVerificationState {
    build_repository_verification_state_with_worktree_status_and_machine_contract(
        repo,
        health,
        worktree_status,
        &MachineContractInput::default(),
    )
}

pub fn build_repository_verification_state_with_worktree_status_and_machine_contract(
    repo: &Repository,
    health: GitOverlayHealth,
    worktree_status: &Result<Option<WorktreeStatus>>,
    machine_contract_input: &MachineContractInput,
) -> RepositoryVerificationState {
    let git_branch = repo.git_overlay_current_branch().ok().flatten();
    let heddle_thread = repo.current_lane().ok().flatten();
    let active_operation = repo.operation_status().ok().flatten().map(|operation| {
        format!("{} {} ({})", operation.scope, operation.kind, operation.state)
    });
    let remote_drift = repo
        .git_remote_tracking_status()
        .ok()
        .flatten()
        .map(|remote| remote_tracking_status(&remote).to_string())
        .unwrap_or_else(|| "clean".to_string());
    let is_git_overlay = repo.capability() == repo::RepositoryCapability::GitOverlay;
    let import_state = health
        .checks
        .iter()
        .find(|check| check.name == "import" && check.status != "clean")
        .or_else(|| health.checks.iter().find(|check| check.name == "import"))
        .map(|check| check.status.clone())
        .unwrap_or_else(|| {
            if is_git_overlay {
                "git_backed".to_string()
            } else {
                "clean".to_string()
            }
        });
    let mapping_state = health
        .checks
        .iter()
        .find(|check| {
            matches!(check.name.as_str(), "head_mapping" | "tag_mapping")
                && !verification_status_is_clean(&check.status)
        })
        .or_else(|| {
            health
                .checks
                .iter()
                .find(|check| check.name == "head_mapping")
        })
        .map(|check| check.status.clone())
        .unwrap_or_else(|| {
            if is_git_overlay {
                "git_backed".to_string()
            } else {
                "clean".to_string()
            }
        });
    let git_worktree_dirty = matches!(
        worktree_status,
        Ok(Some(status)) if !status.is_clean()
    );
    let worktree_dirty = git_worktree_dirty
        || health
            .checks
            .iter()
            .any(|check| {
                matches!(check.name.as_str(), "worktree" | "heddle_worktree")
                    && check.status != "clean"
            });
    let machine_contract_coverage = machine_contract_input.coverage.clone();
    let machine_contract_clean = machine_contract_is_clean(&machine_contract_coverage);
    let mut recovery_commands = health.recovery_commands.clone();
    let remote_action = remote_sync_action(&health);
    let (workflow_status, workflow_summary) = workflow_status(repo, heddle_thread.as_deref());
    let workflow_action = if health.clean && workflow_status == "ready" {
        workflow_primary_action(repo)
    } else {
        None
    };
    if health.clean && !machine_contract_clean {
        recovery_commands.push("heddle doctor schemas --output json".to_string());
    }
    let recommended_action = if health.clean {
        if !machine_contract_clean {
            "heddle doctor schemas --output json".to_string()
        } else {
            workflow_action
                .clone()
                .or_else(|| remote_action.clone())
                .unwrap_or_default()
        }
    } else {
        recovery_commands.first().cloned().unwrap_or_default()
    };
    let checks = verification_checks_from_health(
        &health,
        &machine_contract_coverage,
        is_git_overlay,
        &workflow_status,
        &workflow_summary,
    );
    RepositoryVerificationState {
        verified: health.clean && machine_contract_clean,
        status: if health.clean && !machine_contract_clean {
            "machine_contract_gaps".to_string()
        } else {
            health.status.clone()
        },
        repository_mode: repo.capability_label().to_string(),
        heddle_initialized: true,
        git_branch,
        heddle_thread,
        worktree_dirty,
        worktree_state: if worktree_dirty { "dirty" } else { "clean" }.to_string(),
        import_state,
        mapping_state,
        remote_drift,
        active_operation,
        default_remote: default_remote_name(repo),
        clone_verification: if repo.capability() == repo::RepositoryCapability::GitOverlay {
            if health.clean {
                "verified"
            } else if matches!(health.status.as_str(), "dirty_worktree" | "needs_checkpoint") {
                "not_checked"
            } else {
                "blocked"
            }
        } else {
            "not_applicable"
        }
        .to_string(),
        machine_contract: machine_contract_status(&machine_contract_coverage).to_string(),
        machine_contract_coverage,
        workflow_status,
        workflow_summary,
        summary: health.summary,
        recommended_action: recommended_action.clone(),
        recommended_action_template: action_template(&recommended_action),
        recovery_commands: recovery_commands.clone(),
        recovery_action_templates: action_templates(&recovery_commands),
        checks,
    }
}

fn verification_checks_from_health(
    health: &GitOverlayHealth,
    coverage: &MachineContractCoverage,
    is_git_overlay: bool,
    workflow_status: &str,
    workflow_summary: &str,
) -> Vec<VerificationCheck> {
    let mut checks = vec![
        git_verification_check(is_git_overlay),
        verification_check(
            "Heddle",
            true,
            "clean",
            "Heddle data is initialized",
            None,
            Vec::new(),
        ),
        mapping_verification_check(health, is_git_overlay),
        worktree_verification_check(health),
        remote_verification_check(health),
        operation_verification_check(health),
        workflow_verification_check(health, workflow_status, workflow_summary),
    ];
    checks.push(machine_contract_verification_check(coverage));
    checks.push(clone_verification_check(health, is_git_overlay));
    checks
}

fn machine_contract_verification_check(coverage: &MachineContractCoverage) -> VerificationCheck {
    let mut details = BTreeMap::new();
    details.insert("coverage_status".to_string(), coverage.status.clone());
    details.insert("coverage_summary".to_string(), coverage.summary.clone());
    details.insert("verified_scope".to_string(), coverage.verified_scope.clone());
    details.insert("advanced_scope".to_string(), coverage.advanced_scope.clone());
    details.insert(
        "catalog_commands_total".to_string(),
        coverage.catalog_commands_total.to_string(),
    );
    details.insert(
        "json_commands_total".to_string(),
        coverage.json_commands_total.to_string(),
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
        "json_commands_with_accepted_opaque_schema".to_string(),
        coverage
            .json_commands_with_accepted_opaque_schema
            .to_string(),
    );
    details.insert(
        "verified_scope_json_commands_total".to_string(),
        coverage.verified_scope_json_commands_total.to_string(),
    );
    let mut check = verification_check(
        "Machine contract",
        machine_contract_is_clean(coverage),
        machine_contract_status(coverage),
        &coverage.summary,
        (!machine_contract_is_clean(coverage))
            .then(|| "heddle doctor schemas --output json".to_string()),
        if machine_contract_is_clean(coverage) {
            Vec::new()
        } else {
            vec!["heddle doctor schemas --output json".to_string()]
        },
    );
    check.details = details;
    check
}

fn git_verification_check(is_git_overlay: bool) -> VerificationCheck {
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
    }
}

fn mapping_verification_check(
    health: &GitOverlayHealth,
    is_git_overlay: bool,
) -> VerificationCheck {
    if !is_git_overlay {
        return verification_check(
            "Mapping",
            true,
            "not_applicable",
            "native Heddle refs do not require Git Projection Mapping",
            None,
            Vec::new(),
        );
    }
    if let Some(check) = health.checks.iter().find(|check| {
        check.name == "head_mapping" && !verification_status_is_clean(&check.status)
    }) {
        return verification_check_from_health("Mapping", check, health);
    }
    if let Some(check) = find_health_check(health, "import")
        && check.status != "clean"
    {
        return verification_check_from_health("Mapping", check, health);
    }
    if let Some(check) = find_health_check(health, "tag_mapping")
        && check.status != "clean"
    {
        return verification_check_from_health("Mapping", check, health);
    }
    if let Some(check) = find_health_check(health, "head_mapping") {
        if check.status == "git_backed" && health.status == "dirty_worktree" {
            return verification_check(
                "Mapping",
                true,
                "clean",
                "Git-backed branch mapping is not blocking verification",
                None,
                Vec::new(),
            );
        }
        return verification_check_from_health("Mapping", check, health);
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

fn worktree_verification_check(health: &GitOverlayHealth) -> VerificationCheck {
    for name in ["worktree", "heddle_worktree"] {
        if let Some(check) = find_health_check(health, name)
            && check.status != "clean"
        {
            return verification_check_from_health("Worktree", check, health);
        }
    }
    for name in ["worktree", "heddle_worktree"] {
        if let Some(check) = find_health_check(health, name) {
            return verification_check_from_health("Worktree", check, health);
        }
    }
    if !health.clean {
        return verification_check(
            "Worktree",
            false,
            "not_checked",
            "worktree agreement is checked after the primary verification blocker is resolved",
            health.recovery_commands.first().cloned(),
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

fn remote_verification_check(health: &GitOverlayHealth) -> VerificationCheck {
    if let Some(check) = find_health_check(health, "remote_tracking") {
        if matches!(check.status.as_str(), "remote_ahead" | "remote_untracked") {
            let mut remote_check = verification_check(
                "Remote",
                true,
                &check.status,
                &check.summary,
                remote_sync_action(health),
                Vec::new(),
            );
            remote_check.details = check.details.clone();
            return remote_check;
        }
        return verification_check_from_health("Remote", check, health);
    }
    verification_check(
        "Remote",
        true,
        "clean",
        "remote tracking has no blocking drift",
        None,
        Vec::new(),
    )
}

fn operation_verification_check(health: &GitOverlayHealth) -> VerificationCheck {
    if let Some(check) = find_health_check(health, "operation") {
        return verification_check_from_health("Operation", check, health);
    }
    verification_check(
        "Operation",
        true,
        "clean",
        "no Git or Heddle operation in progress",
        None,
        Vec::new(),
    )
}

fn workflow_verification_check(
    health: &GitOverlayHealth,
    workflow_status: &str,
    workflow_summary: &str,
) -> VerificationCheck {
    if let Some(check) = find_health_check(health, "thread_integration_metadata")
        && check.status != "clean"
    {
        return verification_check_from_health("Workflow", check, health);
    }
    if !health.clean {
        return verification_check(
            "Workflow",
            false,
            "blocked",
            "workflow readiness is checked after the primary verification blocker is resolved",
            health.recovery_commands.first().cloned(),
            health.recovery_commands.clone(),
        );
    }
    verification_check(
        "Workflow",
        true,
        workflow_status,
        workflow_summary,
        None,
        Vec::new(),
    )
}

fn clone_verification_check(
    health: &GitOverlayHealth,
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
        return verification_check(
            "Clone",
            true,
            "verified",
            "Git checkout and Heddle mapping agree",
            None,
            Vec::new(),
        );
    }
    if matches!(health.status.as_str(), "dirty_worktree" | "needs_checkpoint") {
        return verification_check(
            "Clone",
            true,
            "not_checked",
            "clone verification waits for a clean worktree",
            None,
            Vec::new(),
        );
    }
    verification_check(
        "Clone",
        false,
        "blocked",
        "clone verification is blocked until verification checks agree",
        health.recovery_commands.first().cloned(),
        health.recovery_commands.clone(),
    )
}

fn verification_check_from_health(
    name: &str,
    health_check: &crate::status::GitOverlayHealthCheck,
    health: &GitOverlayHealth,
) -> VerificationCheck {
    let recommended_action = (!verification_status_is_clean(&health_check.status))
        .then(|| health.recovery_commands.first().cloned())
        .flatten();
    let recovery_commands = if recommended_action.is_some() {
        health.recovery_commands.clone()
    } else {
        Vec::new()
    };
    let mut check = verification_check(
        name,
        verification_status_is_clean(&health_check.status),
        &health_check.status,
        &health_check.summary,
        recommended_action,
        recovery_commands,
    );
    check.details = health_check.details.clone();
    check
}

fn remote_sync_action(health: &GitOverlayHealth) -> Option<String> {
    find_health_check(health, "remote_tracking").and_then(|check| {
        matches!(check.status.as_str(), "remote_ahead" | "remote_untracked")
            .then(|| "heddle push".to_string())
    })
}

fn find_health_check<'a>(
    health: &'a GitOverlayHealth,
    name: &str,
) -> Option<&'a crate::status::GitOverlayHealthCheck> {
    health.checks.iter().find(|check| check.name == name)
}

fn verification_status_is_clean(status: &str) -> bool {
    matches!(
        status,
        "clean"
            | "available"
            | "git_backed"
            | "not_applicable"
            | "verified"
            | "remote_ahead"
            | "remote_untracked"
    )
}

fn workflow_status(repo: &Repository, current_thread: Option<&str>) -> (String, String) {
    let ready_threads = ThreadManager::new(repo.heddle_dir())
        .list()
        .unwrap_or_default()
        .into_iter()
        .filter(|thread| thread.state == repo::ThreadState::Ready)
        .collect::<Vec<_>>();
    if ready_threads.is_empty() {
        return (
            "clean".to_string(),
            "no ready thread actions require attention".to_string(),
        );
    }
    if ready_threads.iter().all(|thread| {
        thread
            .target_thread
            .as_deref()
            .zip(current_thread)
            .is_some_and(|(target, current)| target != current)
    }) {
        return (
            "clean".to_string(),
            "ready thread actions target another thread".to_string(),
        );
    }
    (
        "ready".to_string(),
        "ready thread actions are waiting to land".to_string(),
    )
}

fn workflow_primary_action(repo: &Repository) -> Option<String> {
    let current_thread = repo.current_lane().ok().flatten();
    let opened_from_dedicated_checkout = repo
        .heddle_dir()
        .parent()
        .is_some_and(|main_root| main_root != repo.root());
    ThreadManager::new(repo.heddle_dir())
        .list()
        .ok()?
        .into_iter()
        .filter(|thread| thread.state == repo::ThreadState::Ready)
        .find_map(|mut thread| {
            let _ = refresh_thread_freshness(repo, &mut thread);
            let actionable = thread
                .target_thread
                .as_deref()
                .map(|target| {
                    current_thread.as_deref() == Some(target) || opened_from_dedicated_checkout
                })
                .unwrap_or(true);
            if !actionable {
                return None;
            }
            let advice = describe_thread_advice(&thread, false, 0, false);
            (!advice.recommended_action.trim().is_empty()).then_some(advice.recommended_action)
        })
}

fn verification_check(
    name: &str,
    clean: bool,
    status: &str,
    summary: &str,
    recommended_action: Option<String>,
    recovery_commands: Vec<String>,
) -> VerificationCheck {
    VerificationCheck {
        name: name.to_string(),
        status: status.to_string(),
        clean,
        summary: summary.to_string(),
        recommended_action: recommended_action.clone(),
        recommended_action_template: recommended_action
            .as_deref()
            .and_then(action_template),
        recovery_action_templates: action_templates(&recovery_commands),
        recovery_commands,
        details: BTreeMap::new(),
    }
}

pub fn action_template(action: &str) -> Option<ActionTemplate> {
    let trimmed = action.trim();
    if trimmed.is_empty() {
        return None;
    }
    recommended_action_templates()
        .iter()
        .find(|template| template.action == trimmed)
        .cloned()
        .or_else(|| concrete_action_template(trimmed))
}

pub fn action_templates(commands: &[String]) -> Vec<ActionTemplate> {
    commands
        .iter()
        .filter_map(|command| action_template(command))
        .collect()
}

fn concrete_action_template(action: &str) -> Option<ActionTemplate> {
    if action.contains("...") || (action.contains('<') && action.contains('>')) {
        return None;
    }
    let argv = split_action(action).ok()?;
    (argv.first().map(String::as_str) == Some("heddle")).then(|| ActionTemplate {
        action: action.to_string(),
        argv_template: normalize_heddle_argv(argv),
        required_inputs: Vec::new(),
        agent_may_fill: false,
    })
}

fn recommended_action_templates() -> Vec<ActionTemplate> {
    [
        ("heddle capture -m \"...\"", &["heddle", "capture", "-m", "<message>"][..], &["message"][..], true),
        ("heddle checkpoint -m \"...\"", &["heddle", "checkpoint", "-m", "<message>"][..], &["message"][..], true),
        ("heddle commit -m \"...\"", &["heddle", "commit", "-m", "<message>"][..], &["message"][..], true),
        ("heddle commit --all -m \"...\"", &["heddle", "commit", "--all", "-m", "<message>"][..], &["message"][..], true),
        ("heddle init", &["heddle", "init"][..], &[][..], false),
        ("heddle init --principal-name <name> --principal-email <email>", &["heddle", "init", "--principal-name", "<name>", "--principal-email", "<email>"][..], &["name", "email"][..], true),
        ("heddle ready -m \"...\"", &["heddle", "ready", "-m", "<message>"][..], &["message"][..], true),
        ("heddle status", &["heddle", "status"][..], &[][..], false),
        ("heddle switch <branch>", &["heddle", "switch", "<branch>"][..], &["branch"][..], false),
        ("heddle verify", &["heddle", "verify"][..], &[][..], false),
        ("heddle diagnose", &["heddle", "diagnose"][..], &[][..], false),
        ("heddle doctor schemas --output json", &["heddle", "doctor", "schemas", "--output", "json"][..], &[][..], false),
    ]
    .into_iter()
    .map(|(action, argv_template, required_inputs, agent_may_fill)| ActionTemplate {
        action: action.to_string(),
        argv_template: normalize_heddle_argv(
            argv_template.iter().map(|arg| (*arg).to_string()).collect(),
        ),
        required_inputs: required_inputs
            .iter()
            .map(|input| (*input).to_string())
            .collect(),
        agent_may_fill,
    })
    .collect()
}

fn normalize_heddle_argv(mut argv: Vec<String>) -> Vec<String> {
    if argv.first().is_some_and(|first| first == "heddle") {
        argv[0] = heddle_argv0();
    }
    argv
}

fn heddle_argv0() -> String {
    match std::env::current_exe() {
        Ok(path) => {
            let file_name = path.file_name().and_then(|name| name.to_str());
            if matches!(file_name, Some("heddle") | Some("heddle.exe")) {
                path.display().to_string()
            } else {
                "heddle".to_string()
            }
        }
        Err(_) => "heddle".to_string(),
    }
}

fn split_action(action: &str) -> std::result::Result<Vec<String>, String> {
    let mut args = Vec::new();
    let mut current = String::new();
    let mut chars = action.chars().peekable();
    let mut in_single_quote = false;
    let mut in_double_quote = false;
    while let Some(ch) = chars.next() {
        match (ch, in_single_quote, in_double_quote) {
            ('\'', false, false) => in_single_quote = true,
            ('\'', true, false) => in_single_quote = false,
            ('"', false, false) => in_double_quote = true,
            ('"', false, true) => in_double_quote = false,
            ('\\', false, _) => match chars.next() {
                Some(next) => current.push(next),
                None => current.push('\\'),
            },
            (ch, false, false) if ch.is_whitespace() => {
                if !current.is_empty() {
                    args.push(std::mem::take(&mut current));
                }
            }
            (ch, _, _) => current.push(ch),
        }
    }
    if in_single_quote || in_double_quote {
        return Err("unterminated quote".to_string());
    }
    if !current.is_empty() {
        args.push(current);
    }
    Ok(args)
}

fn machine_contract_is_clean(coverage: &MachineContractCoverage) -> bool {
    if matches!(coverage.status.as_str(), "not_checked" | "not_applicable") {
        return true;
    }
    coverage.verified_scope_json_commands_without_schema == 0
        && coverage.verified_scope_mutating_commands_without_schema == 0
        && coverage.undocumented_schema_verbs_total == 0
        && coverage.unaccepted_opaque_schema_verbs_total == 0
}

pub fn machine_contract_status(coverage: &MachineContractCoverage) -> &'static str {
    match coverage.status.as_str() {
        "not_checked" => "not_checked",
        "not_applicable" => "not_applicable",
        _ if machine_contract_is_clean(coverage) => "available",
        _ => "available_with_schema_gaps",
    }
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
    let plain_git_probe = build_plain_git_verification_probe_with_machine_contract(
        start,
        &opts.machine_contract_input,
    )?;
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
    let trust = build_repository_verification_state_with_machine_contract(
        repo,
        &opts.machine_contract_input,
    )?;
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
