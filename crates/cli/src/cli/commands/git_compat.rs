// SPDX-License-Identifier: Apache-2.0
//! Git-muscle-memory compatibility shims.

use std::{collections::BTreeMap, fs, path::Path};

use anyhow::{Context, Result, anyhow};
use objects::{
    object::{
        Agent, Blob, ChangeId, ContentHash, EntryType, FileMode, Principal, ThreadName, Tree,
        TreeEntry,
    },
    store::ObjectStore,
    util::gitlink_blob_content,
    worktree::{WorktreeIgnoreMatcher, build_worktree_ignore},
};
use oplog::{OpBatch, OpLogBackend, OpRecord};
use repo::{Repository, RepositoryCapability};
use serde::Serialize;
use sley::{
    BString as GitBString, GitObjectType, Index, IndexEntry, IndexStage, ObjectId,
    Repository as SleyRepository, ShortStatusOptions, ShortStatusRow, StatusUntrackedMode,
    StreamControl,
};

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
        override_trust_recommended_action, plain_git_mutation_preflight_advice,
        repository_verification_blocked_advice,
    },
    next_action::{NextActionValidationContext, write_full_command_json},
    snapshot::{
        SnapshotAgentOverrides, create_snapshot, create_snapshot_from_tree,
        is_placeholder_principal, placeholder_principal_warning,
        preflight_large_capture_for_compat_commit, resolve_principal,
    },
    thread_cmd::cmd_thread,
};
use crate::{
    bridge::git_core::{git_config_identity_with_global_fallback, principal_is_default_unknown},
    cli::{
        Cli, CommitArgs, SwitchArgs, ThreadCommands, should_output_json, style,
        worktree_status_options,
    },
    config::UserConfig,
};

const GIT_MODE_FILE: u32 = 0o100644;
const GIT_MODE_FILE_EXECUTABLE: u32 = 0o100755;
const GIT_MODE_SYMLINK: u32 = 0o120000;
const GIT_MODE_COMMIT: u32 = 0o160000;
const GIT_MODE_DIR: u32 = 0o040000;

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
    #[serde(skip)]
    placeholder_principal_warning: Option<String>,
    next_action: Option<String>,
    next_action_template: Option<ActionTemplate>,
    recommended_action: Option<String>,
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
    let placeholder_principal_warning =
        placeholder_principal_first_commit_warning(&repo, &user_config)?;
    if let Some(state) = repo.current_state()? {
        let tree = repo.require_tree(&state.tree)?;
        let status = repo.compare_worktree_cached_with_options(
            &tree,
            &worktree_status_options(Some(repo.config())),
        )?;
        // A clean worktree (matches Heddle's current tree) can still
        // hide real index-only intent on a Git-overlay checkout — e.g.
        // `git rm --cached path` stages a deletion without touching
        // the file on disk. Treating that as "nothing to commit" would
        // silently drop the staged removal, so fall through to the
        // Git-overlay staged-index path below when one exists.
        let has_staged_index_intent = !args.all
            && repo.capability() == RepositoryCapability::GitOverlay
            && !git_index_intent_for_repo(&repo)?.staged_paths.is_empty();
        if status.is_clean() && !has_staged_index_intent {
            let trust = build_repository_verification_state(&repo);
            // `--no-all` forces an index-only commit and must never auto-commit
            // the captured worktree state. On this fast-path the worktree is
            // clean and the index has no staged intent, so an index-only commit
            // has nothing to commit — surface that instead of silently
            // checkpointing the pending capture into Git.
            if args.no_all {
                return Err(anyhow!(nothing_to_commit_advice()));
            }
            if trust.status == "needs_checkpoint" {
                preflight_git_checkpoint_identity(&repo, &user_config, "commit")?;
                let git_previous_commit = git_head_oid(repo.root());
                let record = create_git_checkpoint(
                    &repo,
                    Some(message.as_str()),
                    worktree_status_options(Some(repo.config())),
                )?;
                let trust = commit_safe_trust(build_repository_verification_state(&repo));
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
                    placeholder_principal_warning: placeholder_principal_warning.clone(),
                    next_action: commit_next_action(&trust),
                    next_action_template: None,
                    recommended_action: None,
                    recommended_action_template: None,
                    trust,
                };
                let output = with_commit_action_metadata(output);
                render_commit_compat(
                    &output,
                    should_output_json(cli, Some(repo.config())),
                    repo.capability(),
                )?;
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
        let trust = commit_safe_trust(build_repository_verification_state(&repo));
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
            placeholder_principal_warning: placeholder_principal_warning.clone(),
            next_action: commit_next_action(&trust),
            next_action_template: None,
            recommended_action: None,
            recommended_action_template: None,
            trust,
        };
        let output = with_commit_action_metadata(output);

        render_commit_compat(
            &output,
            should_output_json(cli, Some(repo.config())),
            repo.capability(),
        )?;
        return Ok(());
    }

    let index_intent = git_index_intent_for_repo(&repo)?;
    if args.no_all && !args.all && index_intent.staged_paths.is_empty() {
        // `--no-all` is index-only. With no staged paths the index is identical
        // to HEAD (empty index, or index == HEAD), so there is nothing genuinely
        // staged. Surface the standard nothing-to-commit outcome BEFORE the
        // commit preflights: identity config and ref-update availability are
        // irrelevant for a commit that was never going to write anything, so an
        // unconfigured identity or blocked ref update must not mask the
        // nothing-to-commit result.
        return Err(anyhow!(nothing_to_commit_advice()));
    }

    preflight_git_checkpoint_identity(&repo, &user_config, "commit")?;
    preflight_git_checkpoint_ref_update(&repo, "commit")?;
    let git_previous_commit = git_head_oid(repo.root());
    let pending_capture = pending_capture_before_commit(&repo)?;
    if !args.all && (args.no_all || !index_intent.staged_paths.is_empty()) {
        // The `--no-all` + empty-index case short-circuited above, so reaching
        // here always has staged paths present (either via `--no-all` with real
        // staged changes, or the non-`--no-all` disjunct that requires them).
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
            &err,
            false,
        ))
    })?;
    let checkpoint_batch = find_recent_git_checkpoint_batch(&repo, &record.git_commit)?;
    repo.oplog()
        .coalesce_batches(snapshot_batch.id, checkpoint_batch.id)
        .context(
            "commit completed but failed to record capture and Git checkpoint as one undo batch",
        )?;

    let trust = commit_safe_trust(build_repository_verification_state(&repo));
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
        placeholder_principal_warning: placeholder_principal_warning.clone(),
        next_action: commit_next_action(&trust),
        next_action_template: None,
        recommended_action: None,
        recommended_action_template: None,
        trust,
    };
    let output = with_commit_action_metadata(output);

    render_commit_compat(
        &output,
        should_output_json(cli, Some(repo.config())),
        repo.capability(),
    )?;

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
            &err,
            true,
        ))
    })?;
    let checkpoint_batch = find_recent_git_checkpoint_batch(repo, &record.git_commit)?;
    repo.oplog()
        .coalesce_batches(snapshot_batch.id, checkpoint_batch.id)
        .context(
            "commit completed but failed to record capture and Git checkpoint as one undo batch",
        )?;

    let trust = commit_safe_trust(build_repository_verification_state(repo));
    let output = CommitCompatOutput {
        output_kind: "commit",
        status: "committed",
        action: "commit",
        change_id: snapshot.change_id,
        git_commit: Some(record.git_commit),
        git_previous_commit,
        summary: staged_commit_summary(&record.summary, &intent),
        confidence: captured_state.confidence,
        git_index: Some(GitIndexPlan::index_only(&intent)),
        included_pending_capture: pending_capture.map(|state| state.short()),
        principal: captured_state.attribution.principal.into(),
        agent: captured_state
            .attribution
            .agent
            .map(CommitAgentOutput::from),
        placeholder_principal_warning: placeholder_principal_first_commit_warning(
            repo,
            user_config,
        )?,
        next_action: commit_next_action(&trust),
        next_action_template: None,
        recommended_action: None,
        recommended_action_template: None,
        trust,
    };
    let output = with_commit_action_metadata(output);
    render_commit_compat(
        &output,
        should_output_json(cli, Some(repo.config())),
        repo.capability(),
    )?;
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

    /// Plan for an index-only commit: checkpoint exactly the staged index (which
    /// may be empty, as on the `--no-all` path) and preserve every unstaged or
    /// untracked worktree path. Never sweeps the worktree.
    pub(crate) fn index_only(intent: &GitIndexIntent) -> Self {
        let (unstaged_paths, untracked_paths) = split_extra_paths(&intent.extra_paths);
        Self {
            commit_mode: "staged_index",
            has_staged_changes: !intent.staged_paths.is_empty(),
            staged_paths: intent.staged_paths.clone(),
            unstaged_paths,
            untracked_paths,
            will_commit: intent.staged_paths.clone(),
            preserved_after_commit: intent.extra_paths.clone(),
        }
    }
}

/// True when `root` is itself the top of a Git worktree, not merely
/// nested inside one. A Heddle thread checkout now lives under the parent
/// repo's `.heddle/threads/` (heddle#572); it's a *native* isolated
/// checkout that shares the parent's object store but is NOT a Git
/// worktree of its own. Bare git discovery walks up the directory tree,
/// so from inside such a checkout it would find the PARENT repo's `.git`
/// and read its index/HEAD as though they belonged to the checkout.
/// Requiring the discovered worktree to equal `root` keeps git-index
/// inspection scoped to genuine git-overlay roots — and matches the
/// pre-#572 behaviour where a sibling checkout had no git above it at all.
fn git_worktree_rooted_at(root: &Path) -> bool {
    match SleyRepository::discover(root) {
        Ok(git) => git_worktree_matches_root(&git, root),
        Err(_) => false,
    }
}

fn git_worktree_matches_root(git: &SleyRepository, root: &Path) -> bool {
    git.workdir()
        .is_some_and(|workdir| paths_equal(&workdir, root))
}

fn paths_equal(left: &Path, right: &Path) -> bool {
    let left = left.canonicalize();
    let right = right.canonicalize();
    match (left, right) {
        (Ok(left), Ok(right)) => left == right,
        _ => false,
    }
}

pub(crate) fn git_index_plan_for_root(root: &Path) -> Result<Option<GitIndexPlan>> {
    if !git_worktree_rooted_at(root) {
        return Ok(None);
    }
    Ok(Some(GitIndexPlan::from_intent(
        &git_index_intent_for_root(root)?,
        false,
    )))
}

pub(crate) fn git_index_plan_for_repo(repo: &Repository) -> Result<Option<GitIndexPlan>> {
    let Some(git) = repo.git_overlay_sley_repository()? else {
        return Ok(None);
    };
    if !git_worktree_matches_root(&git, repo.root()) {
        return Ok(None);
    }
    Ok(Some(GitIndexPlan::from_intent(
        &git_index_intent(repo, &git)?,
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

fn empty_git_index() -> Index {
    Index {
        version: 2,
        entries: Vec::new(),
        extensions: Vec::new(),
        checksum: None,
    }
}

fn index_or_empty(git: &SleyRepository) -> Result<Index> {
    Ok(git.open_index()?.unwrap_or_else(empty_git_index))
}

fn git_index_intent(repo: &Repository, git: &SleyRepository) -> Result<GitIndexIntent> {
    let ignore_patterns = repo.ignore_patterns()?;
    git_index_intent_for_root_with_ignore_and_repo(repo.root(), &ignore_patterns, git)
}

fn git_index_intent_for_repo(repo: &Repository) -> Result<GitIndexIntent> {
    let git = repo
        .git_overlay_sley_repository()?
        .ok_or_else(|| anyhow!("failed to inspect Git index before commit"))?;
    git_index_intent(repo, &git)
}

pub(crate) fn git_index_intent_for_root(root: &Path) -> Result<GitIndexIntent> {
    let ignore_patterns = git_ignore_patterns_for_root(root)?;
    git_index_intent_for_root_with_ignore(root, &ignore_patterns)
}

fn git_index_intent_for_root_with_ignore(
    root: &Path,
    ignore_patterns: &[String],
) -> Result<GitIndexIntent> {
    let git =
        SleyRepository::discover(root).context("failed to inspect Git index before commit")?;
    git_index_intent_for_root_with_ignore_and_repo(root, ignore_patterns, &git)
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
    .with_context(|| {
        format!(
            "failed to inspect Git status before commit at {}",
            root.display()
        )
    })?;

    Ok(intent)
}

fn append_status_row_to_index_intent(
    intent: &mut GitIndexIntent,
    ignore_matcher: &WorktreeIgnoreMatcher,
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

fn git_ignore_patterns_for_root(root: &Path) -> Result<Vec<String>> {
    let git = SleyRepository::discover(root)
        .context("failed to inspect Git ignore files before commit")?;
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
    let git = repo
        .git_overlay_sley_repository()?
        .ok_or_else(|| anyhow!("failed to inspect Git index before commit"))?;
    let index = index_or_empty(&git).context("failed to inspect Git index before commit")?;
    let mut builder = IndexTreeBuilder::default();

    for entry in index.entries {
        let path = git_path_from_bstring(&entry.path);
        if entry.stage() != IndexStage::Normal {
            return Err(anyhow!(unmerged_git_index_advice(&path)));
        }
        let node = index_entry_node(repo, &git, &path, &entry)?;
        builder.insert(&path, node)?;
    }

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
    git: &SleyRepository,
    path: &str,
    entry: &IndexEntry,
) -> Result<IndexTreeNode> {
    let tree_entry = match entry.mode {
        mode if mode == GIT_MODE_FILE || mode == GIT_MODE_FILE_EXECUTABLE => {
            let hash = import_index_blob(repo, git, entry.oid, path)?;
            TreeEntry {
                name: leaf_name(path),
                mode: if entry.mode == GIT_MODE_FILE_EXECUTABLE {
                    FileMode::Executable
                } else {
                    FileMode::Normal
                },
                entry_type: EntryType::Blob,
                hash,
            }
        }
        mode if mode == GIT_MODE_SYMLINK => {
            let hash = import_index_blob(repo, git, entry.oid, path)?;
            TreeEntry {
                name: leaf_name(path),
                mode: FileMode::Symlink,
                entry_type: EntryType::Symlink,
                hash,
            }
        }
        mode if mode == GIT_MODE_COMMIT => {
            let hash = import_index_gitlink(repo, entry.oid)?;
            TreeEntry {
                name: leaf_name(path),
                mode: FileMode::Normal,
                entry_type: EntryType::Blob,
                hash,
            }
        }
        mode if mode == GIT_MODE_DIR => {
            return Err(anyhow!(sparse_git_index_advice(path)));
        }
        _ => {
            return Err(anyhow!(
                "Git index path '{path}' has unsupported mode {:o}",
                entry.mode
            ));
        }
    };
    Ok(IndexTreeNode::Blob(tree_entry))
}

fn import_index_blob(
    repo: &Repository,
    git: &SleyRepository,
    oid: ObjectId,
    path: &str,
) -> Result<ContentHash> {
    let object = git
        .read_object(&oid)
        .with_context(|| format!("failed to read staged Git blob for '{path}'"))?;
    if object.object_type != GitObjectType::Blob {
        return Err(anyhow!(
            "Git index path '{path}' points at {}, not a blob",
            object.object_type.as_str()
        ));
    }
    let blob = Blob::new(object.body.clone());
    Ok(repo.store().put_blob(&blob)?)
}

fn import_index_gitlink(repo: &Repository, oid: ObjectId) -> Result<ContentHash> {
    let blob = Blob::new(gitlink_blob_content(oid));
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

fn git_path_from_bstring(path: &GitBString) -> String {
    String::from_utf8_lossy(path.as_bytes()).into_owned()
}

fn commit_safe_trust(mut trust: RepositoryVerificationState) -> RepositoryVerificationState {
    if is_commit_action(&trust.recommended_action) {
        override_trust_recommended_action(&mut trust, "heddle status");
    }
    let status_action = "heddle status".to_string();
    let status_template = ActionFields::from_action(&status_action).template;
    for check in &mut trust.checks {
        if check
            .recommended_action
            .as_deref()
            .is_some_and(is_commit_action)
        {
            check.recommended_action = Some(status_action.clone());
            check.recommended_action_template = status_template.clone();
        }
    }
    trust
}

fn is_commit_action(action: &str) -> bool {
    matches!(
        action.trim(),
        "heddle commit"
            | "heddle commit -m \"...\""
            | "heddle commit -m \"...\" --confidence <confidence>"
    ) || action.trim().starts_with("heddle commit ")
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
    output.next_action_template = next_action.template;
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
    index_only: bool,
) -> RecoveryAdvice {
    let recovery = checkpoint_recovery_command(message, index_only);
    RecoveryAdvice::safety_refusal(
        "commit_checkpoint_failed",
        format!("capture {change_id} was preserved, but checkpoint failed: {err}"),
        format!("Resolve the checkpoint issue, then run `{recovery}`."),
        "the Heddle capture succeeded but the Git checkpoint step failed",
        "retrying through the canonical save path keeps the Git checkpoint repair on the supported surface",
        format!("captured Heddle state {change_id} was preserved"),
        recovery.clone(),
        vec![recovery],
    )
}

fn checkpoint_recovery_command(message: Option<&str>, index_only: bool) -> String {
    // The Heddle state already exists when checkpoint recovery is offered. The
    // retry must repair only the Git checkpoint instead of re-entering commit
    // and minting another capture from the same tree.
    let scope = if index_only {
        " --from-index-snapshot"
    } else {
        ""
    };
    format!(
        "heddle checkpoint{scope} -m {}",
        shell_double_quoted(message.unwrap_or("commit"))
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
    let git = SleyRepository::discover(root).ok()?;
    git.head().ok()?.oid.map(|id| id.to_string())
}

fn placeholder_principal_first_commit_warning(
    repo: &Repository,
    user_config: &UserConfig,
) -> Result<Option<String>> {
    if !current_state_is_bootstrap(repo)? {
        return Ok(None);
    }
    let principal = resolve_principal(repo, user_config)?;
    if is_placeholder_principal(&principal) {
        return Ok(Some(placeholder_principal_warning(&principal)));
    }
    Ok(None)
}

fn current_state_is_bootstrap(repo: &Repository) -> Result<bool> {
    let Some(state) = repo.current_state()? else {
        return Ok(true);
    };
    Ok(state
        .intent
        .as_deref()
        .is_none_or(|intent| intent.trim().is_empty()))
}

fn render_commit_compat(
    output: &CommitCompatOutput,
    json: bool,
    repository_capability: RepositoryCapability,
) -> Result<()> {
    if json {
        write_full_command_json(
            output,
            NextActionValidationContext::new(&["commit"], repository_capability),
        )?;
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
        if let Some(warning) = output.placeholder_principal_warning.as_deref() {
            eprintln!("{}", style::warn(warning));
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

pub async fn cmd_switch_compat(cli: &Cli, args: SwitchArgs) -> Result<()> {
    if args.create {
        let path = args.target.replace('/', "-");
        let primary = format!("heddle start {} --path ../{}", args.target, path);
        return Err(anyhow!(RecoveryAdvice::safety_refusal(
            "git_checkout_create_branch",
            "`heddle switch -c` / `git checkout -b` are guided to Heddle's isolated thread flow",
            format!(
                "Create a Heddle thread with `{primary}` so the new work has its own checkout, provenance, and ready/land path."
            ),
            "Git-style branch creation would hide whether the user wants an in-place thread or an isolated checkout",
            "Heddle did not create a branch, move HEAD, or write the worktree",
            "repository refs, metadata, and worktree files were left unchanged",
            primary.clone(),
            vec![primary],
        )));
    }
    let repo = cli.open_repo()?;
    if refs::validate_ref_name(&args.target).is_ok()
        && repo
            .refs()
            .get_thread(&ThreadName::new(&args.target))?
            .is_some()
    {
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
                "heddle switch <state>".to_string(),
            ],
        )));
    }
    super::goto::cmd_switch_state_checkout(cli, args.target, args.force)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commit_checkpoint_failure_advice_preserves_capture_and_exact_recovery() {
        let error = anyhow!("git write failed");
        let advice =
            commit_checkpoint_failed_advice("change-123", Some("say \"hello\""), &error, false);

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

    // heddle#485: the staged-index checkpoint-failure recovery must retry only
    // the Git checkpoint against the already-preserved state. Re-entering commit
    // would create a duplicate capture from the same staged index tree.
    #[test]
    fn commit_checkpoint_failure_advice_retries_index_snapshot_checkpoint() {
        let error = anyhow!("git write failed");
        let advice =
            commit_checkpoint_failed_advice("change-456", Some("index only"), &error, true);

        assert_eq!(
            advice.primary_command,
            "heddle checkpoint --from-index-snapshot -m \"index only\""
        );
        assert_eq!(
            advice.recovery_commands,
            vec!["heddle checkpoint --from-index-snapshot -m \"index only\""]
        );
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
            recommended_action_template: None,
            recovery_commands: vec!["heddle continue".to_string()],
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
}
