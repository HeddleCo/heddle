// SPDX-License-Identifier: Apache-2.0
//! Git-muscle-memory compatibility shims.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::Path,
};

use anyhow::{Context, Result, anyhow};
use gix::bstr::{BStr, ByteSlice};
use gix_index::entry::{Mode, Stage};
use objects::{
    object::{Agent, Blob, ChangeId, ContentHash, EntryType, FileMode, Principal, ThreadName, Tree, TreeEntry},
    worktree::should_ignore as should_ignore_path,
};
use oplog::{OpBatch, OpRecord};
use repo::{Repository, RepositoryCapability, git_worktree_status::GitWorktreeEntryState};
use serde::Serialize;

use super::{
    action_line::print_next,
    advice::RecoveryAdvice,
    checkpoint::{
        create_git_checkpoint, create_git_checkpoint_from_index_snapshot,
        preflight_git_checkpoint_ref_update,
    },
    command_catalog::{ActionFields, ActionTemplate},
    git_overlay_health::{
        GitOverlayMutationPreflight, RepositoryVerificationState,
        build_repository_verification_state, git_overlay_mutation_preflight_advice,
        plain_git_mutation_preflight_advice, repository_verification_blocked_advice,
    },
    snapshot::{
        SnapshotAgentOverrides, create_snapshot, create_snapshot_from_tree,
        preflight_large_capture_for_compat_commit, resolve_principal,
    },
    thread_cmd::cmd_thread,
};
use crate::{
    bridge::git_core::{git_config_identity_with_global_fallback, principal_is_default_unknown},
    cli::{
        BranchArgs, Cli, CommitArgs, SwitchArgs, ThreadCommands, ThreadDropArgs, ThreadListArgs,
        ThreadRenameArgs, should_output_json, style, worktree_status_options,
    },
    config::UserConfig,
};

#[derive(Serialize)]
struct CommitCompatOutput {
    output_kind: &'static str,
    status: &'static str,
    action: &'static str,
    change_id: String,
    git_commit: Option<String>,
    git_previous_commit: Option<String>,
    summary: String,
    confidence: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    git_index: Option<GitIndexPlan>,
    #[serde(skip_serializing_if = "Option::is_none")]
    included_pending_capture: Option<String>,
    principal: CommitPrincipalOutput,
    agent: Option<CommitAgentOutput>,
    next_action: Option<String>,
    next_action_argv: Option<Vec<String>>,
    next_action_template: Option<ActionTemplate>,
    recommended_action: Option<String>,
    recommended_action_argv: Option<Vec<String>>,
    recommended_action_template: Option<ActionTemplate>,
    #[serde(rename = "verification")]
    trust: RepositoryVerificationState,
}

#[derive(Serialize)]
struct CommitPrincipalOutput {
    name: String,
    email: String,
}

#[derive(Serialize)]
struct CommitAgentOutput {
    provider: String,
    model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    segment_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    policy_id: Option<String>,
}

impl From<Principal> for CommitPrincipalOutput {
    fn from(principal: Principal) -> Self {
        Self {
            name: principal.name,
            email: principal.email,
        }
    }
}

impl From<Agent> for CommitAgentOutput {
    fn from(agent: Agent) -> Self {
        Self {
            provider: agent.provider,
            model: agent.model,
            session_id: agent.session_id,
            segment_id: agent.segment_id,
            policy_id: agent.policy_id,
        }
    }
}

pub async fn cmd_commit_compat(cli: &Cli, args: CommitArgs) -> Result<()> {
    let message = require_commit_message(args.message.clone())?;
    let cwd;
    let start = if let Some(path) = cli.repo.as_ref() {
        path
    } else {
        cwd = std::env::current_dir()?;
        &cwd
    };
    if let Some(advice) = plain_git_mutation_preflight_advice(start, "commit")? {
        return Err(anyhow!(advice));
    }

    let repo = Repository::open(start)?;
    if let Some(advice) = git_overlay_mutation_preflight_advice(
        &repo,
        "commit",
        GitOverlayMutationPreflight::commit_like(),
    )? {
        return Err(anyhow!(advice));
    }
    let user_config = UserConfig::load_default().unwrap_or_default();
    if let Some(state) = repo.current_state()? {
        let tree = repo.require_tree(&state.tree)?;
        let status = repo.compare_worktree_cached_with_options(
            &tree,
            &worktree_status_options(Some(repo.config())),
        )?;
        if status.is_clean() {
            let trust = build_repository_verification_state(&repo);
            if trust.status == "needs_checkpoint" {
                preflight_git_checkpoint_identity(&repo, &user_config, "commit")?;
                let git_previous_commit = git_head_oid(repo.root());
                let record = create_git_checkpoint(
                    &repo,
                    Some(message.as_str()),
                    worktree_status_options(Some(repo.config())),
                )?;
                let trust = build_repository_verification_state(&repo);
                let output = CommitCompatOutput {
                    output_kind: "commit",
                    status: "committed",
                    action: "commit",
                    change_id: state.change_id.short(),
                    git_commit: Some(record.git_commit),
                    git_previous_commit,
                    summary: record.summary,
                    confidence: state.confidence,
                    git_index: None,
                    included_pending_capture: Some(state.change_id.short()),
                    principal: state.attribution.principal.into(),
                    agent: state.attribution.agent.map(CommitAgentOutput::from),
                    next_action: commit_next_action(&trust),
                    next_action_argv: None,
                    next_action_template: None,
                    recommended_action: None,
                    recommended_action_argv: None,
                    recommended_action_template: None,
                    trust,
                };
                let output = with_commit_action_metadata(output);
                render_commit_compat(&output, should_output_json(cli, Some(repo.config())))?;
                return Ok(());
            }
            if !trust.verified {
                return Err(anyhow!(commit_blocked_by_trust_advice(&trust)));
            }
            return Err(anyhow!(nothing_to_commit_advice()));
        }
    }
    if repo.capability() != RepositoryCapability::GitOverlay {
        let snapshot = create_snapshot(
            &repo,
            &user_config,
            Some(message.clone()),
            args.confidence,
            SnapshotAgentOverrides {
                provider: None,
                model: None,
                session: None,
                segment: None,
                policy: None,
                no_policy: false,
                no_agent: false,
            },
        )?;
        let captured_state = repo
            .current_state()?
            .ok_or_else(|| anyhow!("capture succeeded but no current state was recorded"))?;
        let trust = build_repository_verification_state(&repo);
        let output = CommitCompatOutput {
            output_kind: "commit",
            status: "committed",
            action: "commit",
            change_id: snapshot.change_id,
            git_commit: None,
            git_previous_commit: None,
            summary: snapshot.message,
            confidence: captured_state.confidence,
            git_index: None,
            included_pending_capture: None,
            principal: captured_state.attribution.principal.into(),
            agent: captured_state
                .attribution
                .agent
                .map(CommitAgentOutput::from),
            next_action: commit_next_action(&trust),
            next_action_argv: None,
            next_action_template: None,
            recommended_action: None,
            recommended_action_argv: None,
            recommended_action_template: None,
            trust,
        };
        let output = with_commit_action_metadata(output);

        render_commit_compat(&output, should_output_json(cli, Some(repo.config())))?;
        return Ok(());
    }

    preflight_git_checkpoint_identity(&repo, &user_config, "commit")?;
    preflight_git_checkpoint_ref_update(&repo, "commit")?;
    let git_previous_commit = git_head_oid(repo.root());
    let index_intent = git_index_intent(&repo)?;
    let pending_capture = pending_capture_before_commit(&repo)?;
    if !args.all && !index_intent.staged_paths.is_empty() {
        commit_staged_index(
            cli,
            &repo,
            &user_config,
            &message,
            args.confidence,
            index_intent,
            pending_capture,
        )?;
        return Ok(());
    }
    let git_index = GitIndexPlan::from_intent(&index_intent, args.all);

    preflight_large_capture_for_compat_commit(&repo, args.force)?;
    let snapshot = create_snapshot(
        &repo,
        &user_config,
        Some(message.clone()),
        args.confidence,
        SnapshotAgentOverrides {
            provider: None,
            model: None,
            session: None,
            segment: None,
            policy: None,
            no_policy: false,
            no_agent: false,
        },
    )?;
    let captured_state = repo
        .current_state()?
        .ok_or_else(|| anyhow!("capture succeeded but no current state was recorded"))?;
    let snapshot_batch = find_recent_snapshot_batch(&repo, &captured_state.change_id)?;
    let record = create_git_checkpoint(
        &repo,
        Some(message.as_str()),
        worktree_status_options(Some(repo.config())),
    )
    .map_err(|err| {
        anyhow!(commit_checkpoint_failed_advice(
            &snapshot.change_id,
            Some(message.as_str()),
            &err
        ))
    })?;
    let checkpoint_batch = find_recent_git_checkpoint_batch(&repo, &record.git_commit)?;
    repo.oplog()
        .coalesce_batches(snapshot_batch.id, checkpoint_batch.id)
        .context(
            "commit completed but failed to record capture and Git checkpoint as one undo batch",
        )?;

    let trust = build_repository_verification_state(&repo);
    let output = CommitCompatOutput {
        output_kind: "commit",
        status: "committed",
        action: "commit",
        change_id: snapshot.change_id,
        git_commit: Some(record.git_commit),
        git_previous_commit,
        summary: record.summary,
        confidence: captured_state.confidence,
        git_index: Some(git_index),
        included_pending_capture: pending_capture.map(|state| state.short()),
        principal: captured_state.attribution.principal.into(),
        agent: captured_state
            .attribution
            .agent
            .map(CommitAgentOutput::from),
        next_action: commit_next_action(&trust),
        next_action_argv: None,
        next_action_template: None,
        recommended_action: None,
        recommended_action_argv: None,
        recommended_action_template: None,
        trust,
    };
    let output = with_commit_action_metadata(output);

    render_commit_compat(&output, should_output_json(cli, Some(repo.config())))?;

    Ok(())
}

fn commit_staged_index(
    cli: &Cli,
    repo: &Repository,
    user_config: &UserConfig,
    message: &str,
    confidence: Option<f32>,
    intent: GitIndexIntent,
    pending_capture: Option<ChangeId>,
) -> Result<()> {
    let index_tree = git_index_tree(repo)?;
    let snapshot = create_snapshot_from_tree(
        repo,
        user_config,
        index_tree,
        Some(message.to_string()),
        confidence,
        SnapshotAgentOverrides {
            provider: None,
            model: None,
            session: None,
            segment: None,
            policy: None,
            no_policy: false,
            no_agent: false,
        },
    )?;
    let captured_state = repo
        .current_state()?
        .ok_or_else(|| anyhow!("capture succeeded but no current state was recorded"))?;
    let snapshot_batch = find_recent_snapshot_batch(repo, &captured_state.change_id)?;
    let git_previous_commit = git_head_oid(repo.root());
    let record = create_git_checkpoint_from_index_snapshot(
        repo,
        Some(message),
        worktree_status_options(Some(repo.config())),
    )
    .map_err(|err| {
        anyhow!(commit_checkpoint_failed_advice(
            &snapshot.change_id,
            Some(message),
            &err
        ))
    })?;
    let checkpoint_batch = find_recent_git_checkpoint_batch(repo, &record.git_commit)?;
    repo.oplog()
        .coalesce_batches(snapshot_batch.id, checkpoint_batch.id)
        .context(
            "commit completed but failed to record capture and Git checkpoint as one undo batch",
        )?;

    let trust = build_repository_verification_state(repo);
    let output = CommitCompatOutput {
        output_kind: "commit",
        status: "committed",
        action: "commit",
        change_id: snapshot.change_id,
        git_commit: Some(record.git_commit),
        git_previous_commit,
        summary: staged_commit_summary(&record.summary, &intent),
        confidence: captured_state.confidence,
        git_index: Some(GitIndexPlan::from_intent(&intent, false)),
        included_pending_capture: pending_capture.map(|state| state.short()),
        principal: captured_state.attribution.principal.into(),
        agent: captured_state
            .attribution
            .agent
            .map(CommitAgentOutput::from),
        next_action: commit_next_action(&trust),
        next_action_argv: None,
        next_action_template: None,
        recommended_action: None,
        recommended_action_argv: None,
        recommended_action_template: None,
        trust,
    };
    let output = with_commit_action_metadata(output);
    render_commit_compat(&output, should_output_json(cli, Some(repo.config())))?;
    Ok(())
}

fn preflight_git_checkpoint_identity(
    repo: &Repository,
    user_config: &UserConfig,
    action: &str,
) -> Result<()> {
    let principal = resolve_principal(repo, user_config)?;
    if !principal_is_default_unknown(&principal) {
        return Ok(());
    }
    if git_config_identity_with_global_fallback(repo.root())?.is_some() {
        return Ok(());
    }
    Err(anyhow!(missing_git_checkpoint_identity_advice(action)))
}

fn missing_git_checkpoint_identity_advice(action: &str) -> RecoveryAdvice {
    RecoveryAdvice::safety_refusal(
        "git_checkpoint_identity_required",
        format!("Refusing to {action}: no accountable identity is configured for the Git commit"),
        "Configure `HEDDLE_PRINCIPAL_NAME` and `HEDDLE_PRINCIPAL_EMAIL`, set .heddle principal, or configure Git user.name/user.email before retrying.",
        "Heddle would otherwise have to write Unknown <unknown@example.com> into the Git commit",
        format!("{action} would create an auditable Git checkpoint without a real author identity"),
        "Git refs, Heddle refs, Git checkpoint metadata, and worktree files were left unchanged",
        "heddle init --principal-name <name> --principal-email <email>",
        vec![
            "heddle init --principal-name <name> --principal-email <email>".to_string(),
            "heddle commit -m \"...\"".to_string(),
        ],
    )
}

fn staged_commit_summary(summary: &str, intent: &GitIndexIntent) -> String {
    if intent.extra_paths.is_empty() {
        return summary.to_string();
    }
    format!(
        "{summary} (committed {} staged path(s); left {} unstaged/untracked path(s) in the worktree)",
        intent.staged_paths.len(),
        intent.extra_paths.len()
    )
}

fn require_commit_message(message: Option<String>) -> Result<String> {
    match message {
        Some(message) if !message.trim().is_empty() => Ok(message),
        _ => Err(anyhow!(missing_commit_message_advice())),
    }
}

fn missing_commit_message_advice() -> RecoveryAdvice {
    RecoveryAdvice::safety_refusal(
        "missing_commit_message",
        "refusing to commit without a message",
        "Provide a short message with `heddle commit -m \"...\"`.",
        "no commit message was supplied with -m/--message/--intent",
        "committing without a message would create a weak provenance record",
        "repository state, refs, metadata, Git checkpoints, and worktree files were left unchanged",
        "heddle commit -m \"...\"",
        vec!["heddle commit -m \"...\"".to_string()],
    )
}

#[derive(Default)]
pub(crate) struct GitIndexIntent {
    pub(crate) staged_paths: Vec<String>,
    pub(crate) extra_paths: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct GitIndexPlan {
    pub(crate) commit_mode: &'static str,
    pub(crate) has_staged_changes: bool,
    pub(crate) staged_paths: Vec<String>,
    pub(crate) unstaged_paths: Vec<String>,
    pub(crate) untracked_paths: Vec<String>,
    pub(crate) will_commit: Vec<String>,
    pub(crate) preserved_after_commit: Vec<String>,
}

impl GitIndexPlan {
    pub(crate) fn from_intent(intent: &GitIndexIntent, include_all: bool) -> Self {
        let (unstaged_paths, untracked_paths) = split_extra_paths(&intent.extra_paths);
        let has_staged_changes = !intent.staged_paths.is_empty();
        let mut will_commit = Vec::new();
        if has_staged_changes {
            will_commit.extend(intent.staged_paths.iter().cloned());
        }
        if include_all || !has_staged_changes {
            will_commit.extend(unstaged_paths.iter().cloned());
            will_commit.extend(untracked_paths.iter().cloned());
        }
        let commit_mode = if has_staged_changes && include_all {
            "worktree_all_explicit"
        } else if has_staged_changes {
            "staged_index"
        } else if will_commit.is_empty() {
            "none"
        } else {
            "worktree_all"
        };
        let preserved_after_commit = if has_staged_changes && !include_all {
            intent.extra_paths.clone()
        } else {
            Vec::new()
        };
        Self {
            commit_mode,
            has_staged_changes,
            staged_paths: intent.staged_paths.clone(),
            unstaged_paths,
            untracked_paths,
            will_commit,
            preserved_after_commit,
        }
    }
}

pub(crate) fn git_index_plan_for_root(root: &Path) -> Result<Option<GitIndexPlan>> {
    if gix::discover(root).is_err() {
        return Ok(None);
    }
    Ok(Some(GitIndexPlan::from_intent(
        &git_index_intent_for_root(root)?,
        false,
    )))
}

pub(crate) fn git_index_plan_for_repo(repo: &Repository) -> Result<Option<GitIndexPlan>> {
    if gix::discover(repo.root()).is_err() {
        return Ok(None);
    }
    Ok(Some(GitIndexPlan::from_intent(
        &git_index_intent(repo)?,
        false,
    )))
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

fn git_index_intent(repo: &Repository) -> Result<GitIndexIntent> {
    let ignore_patterns = repo.ignore_patterns()?;
    git_index_intent_for_root_with_ignore(repo.root(), &ignore_patterns)
}

pub(crate) fn git_index_intent_for_root(root: &Path) -> Result<GitIndexIntent> {
    let ignore_patterns = git_ignore_patterns_for_root(root)?;
    git_index_intent_for_root_with_ignore(root, &ignore_patterns)
}

fn git_index_intent_for_root_with_ignore(
    root: &Path,
    ignore_patterns: &[String],
) -> Result<GitIndexIntent> {
    let git = gix::discover(root).context("failed to inspect Git index before commit")?;
    let index = git
        .index_or_empty()
        .context("failed to inspect Git index before commit")?;
    let head_tree = git
        .head_tree_id_or_empty()
        .context("failed to inspect Git index before commit")?;
    let head_index = git
        .index_from_tree(head_tree.as_ref())
        .context("failed to inspect Git index before commit")?;

    let head_entries = index_entries_by_path(&head_index);
    let index_entries = index_entries_by_path(&index);
    let mut intent = GitIndexIntent::default();

    for (path, entry) in &index_entries {
        if head_entries.get(path) != Some(entry) {
            intent.staged_paths.push(path.clone());
        }
    }
    for path in head_entries.keys() {
        if !index_entries.contains_key(path) {
            intent.staged_paths.push(path.clone());
        }
    }

    let tracked_paths: BTreeSet<String> = index_entries.keys().cloned().collect();
    let gitlink_paths: BTreeSet<String> = index_entries
        .iter()
        .filter(|(_, entry)| entry.mode == Mode::COMMIT)
        .map(|(path, _)| path.clone())
        .collect();
    for (path, entry) in &index_entries {
        if entry.mode == Mode::COMMIT {
            continue;
        }
        if worktree_entry_changed(root, path, entry)? {
            intent.extra_paths.push(format!("unstaged: {path}"));
        }
    }
    for path in untracked_worktree_paths(root, &tracked_paths, &gitlink_paths, ignore_patterns)? {
        intent.extra_paths.push(format!("untracked: {path}"));
    }

    Ok(intent)
}

fn git_ignore_patterns_for_root(root: &Path) -> Result<Vec<String>> {
    let git = gix::discover(root).context("failed to inspect Git ignore files before commit")?;
    let mut patterns = Vec::new();
    append_ignore_file_patterns(&mut patterns, &root.join(".gitignore"))?;
    append_ignore_file_patterns(&mut patterns, &git.git_dir().join("info").join("exclude"))?;
    Ok(patterns)
}

fn append_ignore_file_patterns(patterns: &mut Vec<String>, path: &Path) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let contents = fs::read_to_string(path)
        .with_context(|| format!("failed to read ignore file {}", path.display()))?;
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

fn git_index_tree(repo: &Repository) -> Result<Tree> {
    let git = gix::discover(repo.root()).context("failed to inspect Git index before commit")?;
    let index = git
        .index_or_empty()
        .context("failed to inspect Git index before commit")?;
    let mut builder = IndexTreeBuilder::default();

    index
        .entries_with_paths_by_filter_map(|path, entry| Some((bstr_path(path), entry.clone())))
        .try_for_each(|(_, (path, entry))| -> Result<()> {
            if entry.stage() != Stage::Unconflicted {
                return Err(anyhow!(unmerged_git_index_advice(&path)));
            }
            let node = index_entry_node(repo, &git, &path, &entry)?;
            builder.insert(&path, node)
        })?;

    builder.into_tree(repo)
}

#[derive(Default)]
struct IndexTreeBuilder {
    entries: BTreeMap<String, IndexTreeNode>,
}

enum IndexTreeNode {
    Blob(TreeEntry),
    Tree(IndexTreeBuilder),
}

impl IndexTreeBuilder {
    fn insert(&mut self, path: &str, node: IndexTreeNode) -> Result<()> {
        let mut parts = path.split('/').filter(|part| !part.is_empty());
        let first = parts
            .next()
            .ok_or_else(|| anyhow!("Git index contained an empty path"))?
            .to_string();
        let rest = parts.collect::<Vec<_>>();
        if rest.is_empty() {
            if self.entries.contains_key(&first) {
                return Err(anyhow!("Git index contains duplicate path '{path}'"));
            }
            self.entries.insert(first, node);
            return Ok(());
        }

        let child = self
            .entries
            .entry(first.clone())
            .or_insert_with(|| IndexTreeNode::Tree(IndexTreeBuilder::default()));
        let IndexTreeNode::Tree(builder) = child else {
            return Err(anyhow!(
                "Git index contains both file and directory entries at '{first}'"
            ));
        };
        builder.insert(&rest.join("/"), node)
    }

    fn into_tree(self, repo: &Repository) -> Result<Tree> {
        let mut entries = Vec::new();
        for (name, node) in self.entries {
            match node {
                IndexTreeNode::Blob(mut entry) => {
                    entry.name = name;
                    entries.push(entry);
                }
                IndexTreeNode::Tree(builder) => {
                    let tree = builder.into_tree(repo)?;
                    let hash = repo.store().put_tree(&tree)?;
                    entries.push(TreeEntry::directory(name, hash)?);
                }
            }
        }
        Ok(Tree::from_entries(entries))
    }
}

fn index_entry_node(
    repo: &Repository,
    git: &gix::Repository,
    path: &str,
    entry: &gix_index::Entry,
) -> Result<IndexTreeNode> {
    let tree_entry = match entry.mode {
        mode if mode == Mode::FILE || mode == Mode::FILE_EXECUTABLE => {
            let hash = import_index_blob(repo, git, entry.id, path)?;
            TreeEntry {
                name: leaf_name(path),
                mode: if entry.mode == Mode::FILE_EXECUTABLE {
                    FileMode::Executable
                } else {
                    FileMode::Normal
                },
                entry_type: EntryType::Blob,
                hash,
            }
        }
        mode if mode == Mode::SYMLINK => {
            let hash = import_index_blob(repo, git, entry.id, path)?;
            TreeEntry {
                name: leaf_name(path),
                mode: FileMode::Symlink,
                entry_type: EntryType::Symlink,
                hash,
            }
        }
        mode if mode == Mode::COMMIT => {
            let hash = import_index_gitlink(repo, entry.id)?;
            TreeEntry {
                name: leaf_name(path),
                mode: FileMode::Normal,
                entry_type: EntryType::Blob,
                hash,
            }
        }
        mode if mode == Mode::DIR => {
            return Err(anyhow!(sparse_git_index_advice(path)));
        }
        _ => {
            return Err(anyhow!(
                "Git index path '{path}' has unsupported mode {:?}",
                entry.mode
            ));
        }
    };
    Ok(IndexTreeNode::Blob(tree_entry))
}

fn import_index_blob(
    repo: &Repository,
    git: &gix::Repository,
    oid: gix::ObjectId,
    path: &str,
) -> Result<ContentHash> {
    let mut blob = git
        .find_blob(oid)
        .with_context(|| format!("failed to read staged Git blob for '{path}'"))?;
    let blob = Blob::new(blob.take_data());
    Ok(repo.store().put_blob(&blob)?)
}

fn import_index_gitlink(repo: &Repository, oid: gix::ObjectId) -> Result<ContentHash> {
    let blob = Blob::new(format!("heddle-submodule: {oid}").into_bytes());
    Ok(repo.store().put_blob(&blob)?)
}

fn leaf_name(path: &str) -> String {
    path.rsplit('/').next().unwrap_or(path).to_string()
}

fn unmerged_git_index_advice(path: &str) -> RecoveryAdvice {
    RecoveryAdvice::safety_refusal(
        "git_index_unmerged",
        format!("Git index has unresolved conflict stages at {path}"),
        "Resolve the Git index conflict, stage the resolved files, then retry `heddle commit -m \"...\"`.",
        format!("path '{path}' has non-stage-0 entries in the Git index"),
        "committing an unresolved multi-stage index would lose conflict-side information",
        "no Heddle capture, Git checkpoint, refs, index, or worktree files were changed",
        "heddle status",
        vec!["heddle status".to_string()],
    )
}

fn sparse_git_index_advice(path: &str) -> RecoveryAdvice {
    RecoveryAdvice::safety_refusal(
        "git_index_sparse_entry",
        format!("Git index contains sparse directory entry {path}"),
        "Expand the sparse index or commit with `heddle commit --all -m \"...\"` after materializing the desired files.",
        format!("path '{path}' is a sparse directory entry"),
        "Heddle cannot yet prove the exact staged tree for sparse index directory entries",
        "no Heddle capture, Git checkpoint, refs, index, or worktree files were changed",
        "heddle status",
        vec!["heddle status".to_string()],
    )
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct IndexEntryIntent {
    id: gix::ObjectId,
    mode: Mode,
}

fn index_entries_by_path(index: &gix_index::File) -> BTreeMap<String, IndexEntryIntent> {
    index
        .entries_with_paths_by_filter_map(|path, entry| {
            Some(IndexEntryIntent {
                id: entry.id,
                mode: entry.mode,
            })
            .map(|entry| (bstr_path(path), entry))
        })
        .map(|(_, pair)| pair)
        .collect()
}

fn worktree_entry_changed(root: &Path, path: &str, entry: &IndexEntryIntent) -> Result<bool> {
    let state = repo::git_worktree_status::git_worktree_entry_state(
        root,
        path,
        entry.id,
        entry.mode.bits(),
    )
    .with_context(|| format!("failed to inspect worktree path before commit: {path}"))?;
    Ok(state != GitWorktreeEntryState::Clean)
}

fn untracked_worktree_paths(
    root: &Path,
    tracked_paths: &BTreeSet<String>,
    gitlink_paths: &BTreeSet<String>,
    ignore_patterns: &[String],
) -> Result<Vec<String>> {
    // The `ignore` crate walks the tree honoring per-directory
    // `.gitignore` files at every level (plus `.git/info/exclude` and
    // the user-global gitignore) — matching git-commit's own rules.
    // The previous walkdir+flat-list approach only loaded the
    // top-level `.gitignore`, so a nested `src/.gitignore *.tmp`
    // would have failed to ignore `src/foo.tmp`. The `ignore_patterns`
    // arg layers heddle's own ignore policy on top.
    let extra_patterns: Vec<String> = ignore_patterns.to_vec();
    // Clone the gitlink set into the closure so the walker prunes
    // any submodule subtree — `tracked_paths` only contains the
    // submodule path itself, so without this prune every file under
    // a clean submodule worktree would be reported as untracked.
    let gitlinks = gitlink_paths.clone();
    let root_owned = root.to_path_buf();
    let walker = ignore::WalkBuilder::new(root)
        .hidden(false)
        .git_ignore(true)
        .git_exclude(true)
        .git_global(true)
        .filter_entry(move |entry| {
            if is_git_or_heddle_dir(entry.path()) {
                return false;
            }
            if let Ok(relative) = entry.path().strip_prefix(&root_owned)
                && !relative.as_os_str().is_empty()
            {
                let rel = pathbuf_to_git_path(relative);
                if gitlinks.contains(&rel) {
                    return false;
                }
            }
            true
        })
        .build();
    let mut paths = Vec::new();
    for entry in walker {
        let entry = entry.context("failed to inspect worktree before commit")?;
        let file_type = entry.file_type();
        if !file_type.is_some_and(|file_type| file_type.is_file() || file_type.is_symlink()) {
            continue;
        }
        let Ok(relative) = entry.path().strip_prefix(root) else {
            continue;
        };
        if relative.as_os_str().is_empty() {
            continue;
        }
        if should_ignore_path(relative, &extra_patterns) {
            continue;
        }
        let path = repo_relative_string(root, entry.path())?;
        if !tracked_paths.contains(&path) {
            paths.push(path);
        }
    }
    Ok(paths)
}

fn is_git_or_heddle_dir(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name == ".git" || name == ".heddle")
}

fn repo_relative_string(root: &Path, path: &Path) -> Result<String> {
    let relative = path
        .strip_prefix(root)
        .with_context(|| format!("failed to relativize path {}", path.display()))?;
    Ok(pathbuf_to_git_path(relative))
}

fn pathbuf_to_git_path(path: &Path) -> String {
    path.components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

fn bstr_path(path: &BStr) -> String {
    path.to_str_lossy().into_owned()
}

fn commit_next_action(trust: &RepositoryVerificationState) -> Option<String> {
    if !trust.recommended_action.trim().is_empty() {
        return Some(trust.recommended_action.clone());
    }
    if !trust.verified {
        return Some("heddle verify".to_string());
    }
    trust
        .default_remote
        .as_ref()
        .map(|_| "heddle push".to_string())
}

fn pending_capture_before_commit(repo: &Repository) -> Result<Option<ChangeId>> {
    if repo.capability() != RepositoryCapability::GitOverlay {
        return Ok(None);
    }
    let Some(current) = repo.current_state()? else {
        return Ok(None);
    };
    let Some(branch) = repo.git_overlay_current_branch()? else {
        return Ok(None);
    };
    let Some(tip) = repo.git_overlay_branch_tip(&branch)? else {
        return Ok(None);
    };
    let Some(tip) = tip.mapped_change else {
        return Ok(None);
    };
    if tip == current.change_id {
        return Ok(None);
    }
    if repo
        .latest_git_checkpoint_for_change(&current.change_id)?
        .is_some()
    {
        return Ok(None);
    }
    Ok(Some(current.change_id))
}

fn with_commit_action_metadata(mut output: CommitCompatOutput) -> CommitCompatOutput {
    output.recommended_action = output.next_action.clone();
    let next_action = ActionFields::from_optional_action_ref(output.next_action.as_deref());
    let recommended_action =
        ActionFields::from_optional_action_ref(output.recommended_action.as_deref());
    output.next_action_argv = next_action.argv;
    output.next_action_template = next_action.template;
    output.recommended_action_argv = recommended_action.argv;
    output.recommended_action_template = recommended_action.template;
    output
}

fn nothing_to_commit_advice() -> RecoveryAdvice {
    RecoveryAdvice::safety_refusal(
        "nothing_to_commit",
        "nothing to commit: worktree has no changes eligible for Heddle capture",
        "Inspect the worktree with `heddle status`; make changes before running `heddle commit -m \"...\"`.",
        "the worktree has no modified, deleted, or untracked paths relative to the current Heddle state",
        "commit would not capture a new Heddle state or write a meaningful Git checkpoint",
        "repository state was left unchanged",
        "heddle status",
        vec!["heddle status".to_string()],
    )
}

fn commit_blocked_by_trust_advice(trust: &RepositoryVerificationState) -> RecoveryAdvice {
    repository_verification_blocked_advice(
        "commit_blocked_by_verification",
        format!(
            "refusing to report nothing to commit: repository verification is blocked ({})",
            trust.status
        ),
        "retrying `heddle commit`",
        trust,
        format!(
            "repository verification status is {}: {}",
            trust.status, trust.summary
        ),
        "claiming nothing to commit could hide a Git/Heddle/import/operation disagreement",
        "no capture, Git checkpoint, refs, or worktree files were changed",
        None,
    )
}

fn commit_checkpoint_failed_advice(
    change_id: &str,
    message: Option<&str>,
    err: &anyhow::Error,
) -> RecoveryAdvice {
    let recovery = checkpoint_recovery_command(message);
    RecoveryAdvice::safety_refusal(
        "commit_checkpoint_failed",
        format!("capture {change_id} was preserved, but checkpoint failed: {err}"),
        format!("Resolve the checkpoint issue, then run `{recovery}`."),
        "the Heddle capture succeeded but the Git checkpoint step failed",
        "retrying `heddle commit` could create a duplicate capture instead of checkpointing the preserved state",
        format!("captured Heddle state {change_id} was preserved"),
        recovery.clone(),
        vec![recovery],
    )
}

fn checkpoint_recovery_command(message: Option<&str>) -> String {
    format!(
        "heddle checkpoint -m {}",
        shell_double_quoted(message.unwrap_or("checkpoint"))
    )
}

fn shell_double_quoted(value: &str) -> String {
    let mut quoted = String::from("\"");
    for ch in value.chars() {
        match ch {
            '\\' | '"' | '$' | '`' => {
                quoted.push('\\');
                quoted.push(ch);
            }
            '\n' => quoted.push_str("\\n"),
            _ => quoted.push(ch),
        }
    }
    quoted.push('"');
    quoted
}

fn find_recent_snapshot_batch(repo: &Repository, state: &ChangeId) -> Result<OpBatch> {
    repo.oplog()
        .recent_batches_scoped(8, Some(&repo.op_scope()))?
        .into_iter()
        .find(|batch| {
            batch.entries.iter().any(|entry| {
                matches!(
                    &entry.operation,
                    OpRecord::Snapshot { new_state, .. } if new_state == state
                )
            })
        })
        .ok_or_else(|| anyhow!("capture succeeded but its oplog batch was not found"))
}

fn find_recent_git_checkpoint_batch(repo: &Repository, git_commit: &str) -> Result<OpBatch> {
    repo.oplog()
        .recent_batches_scoped(8, Some(&repo.op_scope()))?
        .into_iter()
        .find(|batch| {
            batch.entries.iter().any(|entry| {
                matches!(
                    &entry.operation,
                    OpRecord::GitCheckpoint { new_git_oid, .. } if new_git_oid == git_commit
                )
            })
        })
        .ok_or_else(|| anyhow!("Git checkpoint succeeded but its oplog batch was not found"))
}

fn git_head_oid(root: &Path) -> Option<String> {
    let git = gix::discover(root).ok()?;
    git.head_id().ok().map(|id| id.to_string())
}

fn render_commit_compat(output: &CommitCompatOutput, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string(&output)?);
    } else {
        println!(
            "{}",
            match &output.git_commit {
                Some(git_commit) => format!(
                    "Committed {} as Git commit {}",
                    style::change_id(&output.change_id),
                    style::dim(&git_commit[..std::cmp::min(12, git_commit.len())])
                ),
                None => format!(
                    "Committed Heddle state {}",
                    style::change_id(&output.change_id)
                ),
            }
        );
        if let (Some(before), Some(after)) = (&output.git_previous_commit, &output.git_commit)
            && before != after
        {
            println!(
                "Git HEAD moved: {} -> {}",
                style::dim(&before[..std::cmp::min(12, before.len())]),
                style::dim(&after[..std::cmp::min(12, after.len())])
            );
        }
        if let Some(pending) = &output.included_pending_capture {
            println!(
                "Included prior Heddle-only save {}; this Git commit checkpoints the resulting state.",
                style::change_id(pending)
            );
        }
        println!(
            "Saved by: {}",
            style::principal(&output.principal.name, &output.principal.email)
        );
        if let Some(agent) = &output.agent {
            println!(
                "Agent: {}/{}",
                style::bold(&agent.provider),
                style::dim(&agent.model)
            );
        }
        if let Some(plan) = &output.git_index {
            println!("Commit scope: {}", commit_scope_text(plan));
            if !plan.will_commit.is_empty() {
                println!("Included: {}", plan.will_commit.join(", "));
            }
            if !plan.preserved_after_commit.is_empty() {
                println!(
                    "Left in worktree: {}",
                    plan.preserved_after_commit.join(", ")
                );
            }
        }
        if let Some(next) = &output.next_action {
            print_next(next);
        } else if output.trust.verified {
            println!("Verification: clean");
        }
    }

    Ok(())
}

fn commit_scope_text(plan: &GitIndexPlan) -> &'static str {
    match plan.commit_mode {
        "staged_index" => {
            "staged Git index only; unstaged and untracked paths stay in the worktree"
        }
        "worktree_all_explicit" => "all staged, unstaged, and untracked worktree changes (--all)",
        "worktree_all" => "all unstaged and untracked worktree changes",
        "none" => "no Git paths",
        _ => "Git worktree changes",
    }
}

pub async fn cmd_branch_compat(cli: &Cli, args: BranchArgs) -> Result<()> {
    let delete = args.delete || args.force_delete;
    let command = match (args.name, args.new_name, delete, args.move_branch) {
        (None, None, false, false) => ThreadCommands::List(ThreadListArgs::default()),
        (Some(name), None, true, false) => ThreadCommands::Drop(ThreadDropArgs {
            thread: name,
            delete_thread: true,
            force: args.force_delete,
        }),
        (Some(old), Some(new), false, true) => {
            ThreadCommands::Rename(ThreadRenameArgs { old, new })
        }
        (Some(name), None, false, false) => ThreadCommands::Create {
            name,
            ephemeral: false,
            ttl_secs: None,
        },
        _ => return Err(anyhow!(branch_usage_alternatives_advice())),
    };
    cmd_thread(cli, command).await
}

fn branch_usage_alternatives_advice() -> RecoveryAdvice {
    RecoveryAdvice::safety_refusal(
        "branch_usage_alternatives",
        "unsupported branch arguments",
        "Use one supported shape: `heddle branch`, `heddle branch <name>`, `heddle branch -m <old> <new>`, or `heddle branch -d <name>`.",
        "the supplied branch flags and positionals do not match a supported Heddle thread operation",
        "accepting unsupported Git branch syntax could create, rename, or delete the wrong thread",
        "Heddle did not create, rename, or delete a thread; refs, metadata, and worktree files were left unchanged",
        "heddle branch",
        vec![
            "heddle branch".to_string(),
            "heddle branch <name>".to_string(),
            "heddle branch -m <old> <new>".to_string(),
            "heddle branch -d <name>".to_string(),
        ],
    )
}

pub async fn cmd_switch_compat(cli: &Cli, args: SwitchArgs) -> Result<()> {
    if args.create {
        let path = args.target.replace('/', "-");
        let primary = format!("heddle start {} --path ../{}", args.target, path);
        return Err(anyhow!(RecoveryAdvice::safety_refusal(
            "git_checkout_create_branch",
            "`heddle switch -c` / `heddle checkout -b` are guided to Heddle's isolated thread flow",
            format!(
                "Create a Heddle thread with `{primary}` so the new work has its own checkout, provenance, and ready/ship path."
            ),
            "Git-style branch creation would hide whether the user wants an in-place thread or an isolated checkout",
            "Heddle did not create a branch, move HEAD, or write the worktree",
            "repository refs, metadata, and worktree files were left unchanged",
            primary.clone(),
            vec![primary],
        )));
    }
    let repo = Repository::open(cli.repo.as_ref().unwrap_or(&std::env::current_dir()?))?;
    if repo.refs().get_thread(&ThreadName::new(&args.target))?.is_some() {
        return cmd_thread(
            cli,
            ThreadCommands::Switch {
                name: args.target,
                print_cd_path: args.print_cd_path,
                force: args.force,
            },
        )
        .await;
    }
    if args.print_cd_path {
        return Err(anyhow!(RecoveryAdvice::safety_refusal(
            "switch_print_cd_path_requires_thread",
            "`--print-cd-path` only applies when switching to a thread",
            "Use `heddle switch --print-cd-path <thread>` for a materialized thread, or omit `--print-cd-path` when checking out a state.",
            "the target did not resolve to a Heddle thread with a checkout path",
            "checking out a state would move the worktree but could not report a thread checkout path",
            "Heddle did not move HEAD or write the worktree",
            "heddle switch <thread> --print-cd-path",
            vec![
                "heddle switch <thread> --print-cd-path".to_string(),
                "heddle checkout <state>".to_string(),
            ],
        )));
    }
    super::goto::cmd_goto(cli, args.target, args.force)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commit_checkpoint_failure_advice_preserves_capture_and_exact_recovery() {
        let error = anyhow!("git write failed");
        let advice = commit_checkpoint_failed_advice("change-123", Some("say \"hello\""), &error);

        assert_eq!(advice.kind, "commit_checkpoint_failed");
        assert!(advice.error.contains("capture change-123 was preserved"));
        assert!(advice.error.contains("git write failed"));
        assert_eq!(
            advice.primary_command,
            "heddle checkpoint -m \"say \\\"hello\\\"\""
        );
        assert_eq!(
            advice.recovery_commands,
            vec!["heddle checkpoint -m \"say \\\"hello\\\"\""]
        );
        assert!(advice.preserved.contains("change-123"));
    }

    #[test]
    fn nothing_to_commit_advice_names_status_recovery() {
        let advice = nothing_to_commit_advice();

        assert_eq!(advice.kind, "nothing_to_commit");
        assert_eq!(advice.primary_command, "heddle status");
        assert!(advice.error.contains("nothing to commit"));
        assert!(advice.primary_hint().contains("heddle status"));
    }

    #[test]
    fn commit_blocked_by_trust_advice_uses_trust_recovery() {
        let machine_contract_coverage =
            crate::cli::commands::git_overlay_health::machine_contract_coverage();
        let trust = RepositoryVerificationState {
            verified: false,
            status: "operation_in_progress".to_string(),
            repository_mode: "git-overlay".to_string(),
            heddle_initialized: true,
            git_branch: Some("main".to_string()),
            heddle_thread: Some("main".to_string()),
            worktree_dirty: false,
            worktree_state: "clean".to_string(),
            import_state: "clean".to_string(),
            mapping_state: "clean".to_string(),
            remote_drift: "clean".to_string(),
            active_operation: Some("Git merge (in-progress)".to_string()),
            default_remote: None,
            clone_verification: "not_applicable".to_string(),
            machine_contract: crate::cli::commands::git_overlay_health::machine_contract_status(
                &machine_contract_coverage,
            )
            .to_string(),
            machine_contract_coverage,
            workflow_status: "clean".to_string(),
            workflow_summary: "no ready threads are waiting to land".to_string(),
            summary: "Git merge is in progress".to_string(),
            recommended_action: "heddle continue".to_string(),
            recommended_action_argv: Some(vec!["heddle".to_string(), "continue".to_string()]),
            recommended_action_template: None,
            recovery_commands: vec!["heddle continue".to_string()],
            recovery_command_argv: vec![vec!["heddle".to_string(), "continue".to_string()]],
            recovery_action_templates: Vec::new(),
            checks: Vec::new(),
        };

        let advice = commit_blocked_by_trust_advice(&trust);

        assert_eq!(advice.kind, "commit_blocked_by_verification");
        assert_eq!(advice.primary_command, "heddle continue");
        assert_eq!(advice.recovery_commands, vec!["heddle continue"]);
        assert!(advice.error.contains("repository verification is blocked"));
        assert!(advice.unsafe_condition.contains("Git merge is in progress"));
    }

    #[test]
    fn branch_usage_alternatives_advice_is_typed() {
        let advice = branch_usage_alternatives_advice();

        assert_eq!(advice.kind, "branch_usage_alternatives");
        assert_eq!(advice.primary_command, "heddle branch");
        assert!(advice.primary_hint().contains("heddle branch -m"));
        assert!(advice.unsafe_condition.contains("branch flags"));
        assert!(advice.would_change.contains("wrong thread"));
        assert!(advice.preserved.contains("did not create"));
        assert!(
            advice
                .recovery_commands
                .iter()
                .any(|command| command == "heddle branch -d <name>")
        );
    }
}
