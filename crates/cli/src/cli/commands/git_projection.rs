// SPDX-License-Identifier: Apache-2.0
//! Git Projection command implementations.

use std::{collections::BTreeMap, path::Path, time::Instant};

use anyhow::{Context, Result, anyhow};
use heddle_core::{
    CommitGitIndexPlan, GitScope, SavePlan, SaveVerb, commit_next_action_from_trust,
    commit_scope_text as core_commit_scope_text, execute_save, plan_commit_git_index,
    plan_commit_git_index_only, plan_git_scope,
    staged_commit_summary as core_staged_commit_summary, tree_leaf_name,
};
use objects::{
    object::{Agent, Blob, ContentHash, Principal, StateId, ThreadName, Tree, TreeEntry},
    store::ObjectStore,
    worktree::{WorktreeIgnoreMatcher, build_worktree_ignore},
};
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
    checkpoint::create_git_checkpoint_with_worktree_status,
    command_catalog::{ActionFields, ActionTemplate},
    git_overlay_txn,
    next_action::{NextActionValidationContext, write_full_command_json},
    snapshot::{
        SnapshotAgentOverrides, build_attribution, is_placeholder_principal,
        placeholder_principal_warning,
        preflight_large_capture_for_git_projection_commit_with_worktree_status, resolve_principal,
    },
    verification_health::RepositoryVerificationState,
};
use crate::{
    cli::{Cli, CommitArgs, should_output_json, style, worktree_status_options},
    config::UserConfig,
    perf::{ProfileField, emit_profile, profile_enabled},
};

const GIT_MODE_FILE: u32 = 0o100644;
const GIT_MODE_FILE_EXECUTABLE: u32 = 0o100755;
const GIT_MODE_SYMLINK: u32 = 0o120000;
const GIT_MODE_COMMIT: u32 = 0o160000;
const GIT_MODE_DIR: u32 = 0o040000;

#[derive(Serialize)]
struct GitProjectionCommitOutput {
    output_kind: &'static str,
    status: &'static str,
    action: &'static str,
    state_id: String,
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

pub async fn cmd_commit_git_projection(cli: &Cli, args: CommitArgs) -> Result<()> {
    let message = require_commit_message(args.message.clone())?;
    let cwd;
    let start = if let Some(path) = cli.repo.as_ref() {
        path
    } else {
        cwd = std::env::current_dir()?;
        &cwd
    };
    git_overlay_txn::preflight_plain_git_mutation(start, "commit")?;

    let repo = Repository::open(start)?;
    // Compute the git-overlay worktree status ONCE up front. The commit mutation
    // preflight here is PRE-mutation and shared by every commit path; the clean
    // fast-path below reuses the same status for its verification preflight and
    // for the checkpoint it triggers, all of which observe the same pre-mutation
    // git state (no Git ref moves until `create_git_checkpoint`). This is the
    // exact `Result` from a full worktree walk that re-reads + SHA-1s every
    // tracked file — before this, the clean fast-path paid that walk 3× before
    // the ref ever moved.
    let preflight_worktree_status_start = Instant::now();
    let git_overlay_facts = git_overlay_txn::gather_mutation_facts(&repo);
    let preflight_worktree_status_ms = preflight_worktree_status_start.elapsed().as_millis();
    git_overlay_txn::preflight_commit(&repo, &git_overlay_facts)?;
    let user_config = UserConfig::load_default().unwrap_or_default();
    let placeholder_principal_warning =
        placeholder_principal_first_commit_warning(&repo, &user_config)?;
    // Heddle-side clean-check: walks the worktree against the current Heddle tree
    // (load index + read + SHA-1 every tracked file + save index). This is a
    // SEPARATE walk from the git-overlay `worktree_status` above (different
    // representation/semantics), so it cannot be threaded from it. It is only
    // needed to distinguish "nothing to commit" / index-only-intent from a real
    // change; the dirty path discards the result. Profiled below so the cost is
    // attributed. Cutting it (cheap is-dirty probe for the dirty path) is a
    // noted L-effort follow-up.
    let mut clean_check_status_ms = 0u128;
    if let Some(state) = repo.current_state()? {
        let tree = repo.require_tree(&state.tree)?;
        let clean_check_start = Instant::now();
        let status = repo.compare_worktree_cached_with_options(
            &tree,
            &worktree_status_options(Some(repo.config())),
        )?;
        clean_check_status_ms = clean_check_start.elapsed().as_millis();
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
            // Reuse the pre-mutation git-overlay worktree status computed at the
            // top: no Git ref has moved on this fast-path, so the verification
            // state is byte-identical to a fresh walk here.
            let trust = git_overlay_txn::preflight_verify_with_worktree_status(
                &repo,
                git_overlay_facts.worktree_status(),
            );
            // `--no-all` forces an index-only commit and must never auto-commit
            // the captured worktree state. On this fast-path the worktree is
            // clean and the index has no staged intent, so an index-only commit
            // has nothing to commit — surface that instead of silently
            // checkpointing the pending capture into Git.
            if args.no_all {
                return Err(anyhow!(nothing_to_commit_advice()));
            }
            if trust.status == "needs_checkpoint" {
                git_overlay_txn::preflight_git_checkpoint_identity(
                    &repo,
                    &user_config,
                    "commit",
                    "heddle commit -m \"...\"",
                )?;
                let git_previous_commit = git_head_oid(repo.root());
                // Thread the same pre-mutation status into the checkpoint so it
                // does not re-run its own pre-mutation worktree walk. The
                // checkpoint then advances the Git ref, so the post-checkpoint
                // `build_repository_verification_state` below stays a FRESH walk.
                let record = create_git_checkpoint_with_worktree_status(
                    &repo,
                    Some(message.as_str()),
                    worktree_status_options(Some(repo.config())),
                    git_overlay_facts.worktree_status(),
                )?;
                let trust = git_overlay_txn::post_verify_commit(&repo);
                let output = GitProjectionCommitOutput {
                    output_kind: "commit",
                    status: "committed",
                    action: "commit",
                    state_id: state.state_id.short(),
                    git_commit: Some(record.git_commit),
                    git_previous_commit,
                    summary: record.summary,
                    confidence: state.confidence,
                    git_index: None,
                    included_pending_capture: Some(state.state_id.short()),
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
                render_git_projection_commit(
                    &output,
                    should_output_json(cli, Some(repo.config())),
                    repo.capability(),
                )?;
                return Ok(());
            }
            if !trust.verified {
                return Err(anyhow!(git_overlay_txn::commit_blocked_by_trust_advice(
                    &trust
                )));
            }
            return Err(anyhow!(nothing_to_commit_advice()));
        }
    }
    if repo.capability() != RepositoryCapability::GitOverlay {
        let attribution = build_attribution(
            &repo,
            &user_config,
            &SnapshotAgentOverrides {
                provider: None,
                model: None,
                session: None,
                segment: None,
                policy: None,
                no_policy: false,
                no_agent: false,
            },
        )?;
        let plan = SavePlan::commit(message.clone(), attribution, GitScope::None)
            .with_confidence(args.confidence)
            .with_worktree_status_options(worktree_status_options(Some(repo.config())));
        debug_assert_eq!(
            plan_git_scope(SaveVerb::Commit, repo.capability(), false, true),
            GitScope::None
        );
        let report = execute_save(&repo, plan)?;
        let trust = git_overlay_txn::post_verify_commit(&repo);
        let output = GitProjectionCommitOutput {
            output_kind: "commit",
            status: "committed",
            action: "commit",
            state_id: report.state_id.short(),
            git_commit: None,
            git_previous_commit: None,
            summary: report.summary,
            confidence: report.confidence,
            git_index: None,
            included_pending_capture: None,
            principal: report.principal.into(),
            agent: report.agent.map(CommitAgentOutput::from),
            placeholder_principal_warning: placeholder_principal_warning.clone(),
            next_action: commit_next_action(&trust),
            next_action_template: None,
            recommended_action: None,
            recommended_action_template: None,
            trust,
        };
        let output = with_commit_action_metadata(output);

        render_git_projection_commit(
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

    git_overlay_txn::preflight_git_checkpoint_identity(
        &repo,
        &user_config,
        "commit",
        "heddle commit -m \"...\"",
    )?;
    git_overlay_txn::preflight_commit_checkpoint_ref_update(&repo, &git_overlay_facts)?;
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
            StagedIndexCommit {
                message: &message,
                confidence: args.confidence,
                intent: index_intent,
                pending_capture,
                git_overlay_facts: &git_overlay_facts,
            },
        )?;
        return Ok(());
    }
    let git_index = GitIndexPlan::from_intent(&index_intent, args.all);

    // Reuse the pre-mutation git-overlay worktree status computed at the top of
    // this command (`worktree_status`) for the dirty-commit path's three
    // PRE-mutation consumers: the large-capture safety preflight, the capture
    // mutation preflight inside the snapshot, and the checkpoint's two
    // preflights. None of these moves a Git ref, so they all observe the same
    // pre-mutation git state and reuse is byte-identical to a fresh walk. Before
    // this, the dirty path re-walked the worktree (re-reading + SHA-1ing every
    // tracked file) four-plus times here — the large-capture preflight, the
    // snapshot preflight, and both checkpoint preflights each ran their own
    // walk. The post-checkpoint verification (`build_repository_verification_state`
    // below) is left FRESH: the checkpoint advances the Git ref, which flips the
    // git-overlay health classification.
    let large_capture_start = Instant::now();
    preflight_large_capture_for_git_projection_commit_with_worktree_status(
        args.force,
        git_overlay_facts.worktree_status(),
    )?;
    let large_capture_preflight_ms = large_capture_start.elapsed().as_millis();

    // Shared save pipeline: Heddle capture + Git checkpoint + oplog coalesce.
    let attribution = build_attribution(
        &repo,
        &user_config,
        &SnapshotAgentOverrides {
            provider: None,
            model: None,
            session: None,
            segment: None,
            policy: None,
            no_policy: false,
            no_agent: false,
        },
    )?;
    debug_assert_eq!(
        plan_git_scope(SaveVerb::Commit, repo.capability(), false, true),
        GitScope::WorktreeAll
    );
    let mut plan = SavePlan::commit(message.clone(), attribution, GitScope::WorktreeAll)
        .with_confidence(args.confidence)
        .with_worktree_status_options(worktree_status_options(Some(repo.config())));
    plan.require_clean_worktree = false; // dirty worktree is the input being saved
    plan.precomputed_worktree_status = Some(clone_git_overlay_worktree_status(
        git_overlay_facts.worktree_status(),
    ));

    let head_before = repo.head().ok().flatten();
    let save_start = Instant::now();
    let report = execute_save(&repo, plan).map_err(|err| {
        // Only remap when a new capture landed and the Git checkpoint step
        // failed after it — otherwise surface the original save error.
        match (head_before.as_ref(), repo.head().ok().flatten()) {
            (before, Some(after)) if before != Some(&after) => {
                anyhow!(git_overlay_txn::commit_checkpoint_failed_advice(
                    &after.short(),
                    Some(message.as_str()),
                    &err,
                    false,
                ))
            }
            _ => err,
        }
    })?;
    let save_ms = save_start.elapsed().as_millis();

    if profile_enabled() {
        emit_profile(
            "commit phases",
            &[
                ProfileField::millis("preflight_worktree_status_ms", preflight_worktree_status_ms),
                ProfileField::millis("clean_check_status_ms", clean_check_status_ms),
                ProfileField::millis("large_capture_preflight_ms", large_capture_preflight_ms),
                ProfileField::millis("save_ms", save_ms),
                ProfileField::millis(
                    "snapshot_tree_walk_ms",
                    report.snapshot_profile.tree_walk_ms,
                ),
                ProfileField::millis(
                    "snapshot_state_ref_oplog_ms",
                    report.snapshot_profile.state_ref_oplog_ms,
                ),
            ],
        );
    }
    let git_commit = report
        .git_commit
        .clone()
        .or_else(|| report.git_checkpoint.as_ref().map(|r| r.git_commit.clone()));
    let trust = git_overlay_txn::post_verify_commit(&repo);
    let output = GitProjectionCommitOutput {
        output_kind: "commit",
        status: "committed",
        action: "commit",
        state_id: report.state_id.short(),
        git_commit,
        git_previous_commit,
        summary: report
            .git_checkpoint
            .as_ref()
            .map(|r| r.summary.clone())
            .unwrap_or(report.summary),
        confidence: report.confidence,
        git_index: Some(git_index),
        included_pending_capture: pending_capture.map(|state| state.short()),
        principal: report.principal.into(),
        agent: report.agent.map(CommitAgentOutput::from),
        placeholder_principal_warning: placeholder_principal_warning.clone(),
        next_action: commit_next_action(&trust),
        next_action_template: None,
        recommended_action: None,
        recommended_action_template: None,
        trust,
    };
    let output = with_commit_action_metadata(output);

    render_git_projection_commit(
        &output,
        should_output_json(cli, Some(repo.config())),
        repo.capability(),
    )?;

    Ok(())
}

fn clone_git_overlay_worktree_status(
    status: &git_overlay_txn::GitOverlayWorktreeStatus,
) -> repo::Result<Option<objects::worktree::WorktreeStatus>> {
    match status {
        Ok(Some(s)) => Ok(Some(objects::worktree::WorktreeStatus {
            modified: s.modified.clone(),
            added: s.added.clone(),
            deleted: s.deleted.clone(),
        })),
        Ok(None) => Ok(None),
        Err(err) => Err(objects::HeddleError::Config(err.to_string())),
    }
}

struct StagedIndexCommit<'a> {
    message: &'a str,
    confidence: Option<f32>,
    intent: GitIndexIntent,
    pending_capture: Option<StateId>,
    git_overlay_facts: &'a git_overlay_txn::GitOverlayMutationFacts,
}

fn commit_staged_index(
    cli: &Cli,
    repo: &Repository,
    user_config: &UserConfig,
    staged: StagedIndexCommit<'_>,
) -> Result<()> {
    let StagedIndexCommit {
        message,
        confidence,
        intent,
        pending_capture,
        git_overlay_facts,
    } = staged;
    let index_tree = git_index_tree(repo)?;
    let attribution = build_attribution(
        repo,
        user_config,
        &SnapshotAgentOverrides {
            provider: None,
            model: None,
            session: None,
            segment: None,
            policy: None,
            no_policy: false,
            no_agent: false,
        },
    )?;
    debug_assert_eq!(
        plan_git_scope(SaveVerb::Commit, repo.capability(), true, false),
        GitScope::Staged
    );
    let mut plan = SavePlan::commit(message.to_string(), attribution, GitScope::Staged)
        .with_confidence(confidence)
        .with_supplied_tree(index_tree)
        .with_worktree_status_options(worktree_status_options(Some(repo.config())));
    plan.require_clean_worktree = false;
    plan.precomputed_worktree_status = Some(clone_git_overlay_worktree_status(
        git_overlay_facts.worktree_status(),
    ));
    let git_previous_commit = git_head_oid(repo.root());
    let head_before = repo.head().ok().flatten();
    let report = execute_save(repo, plan).map_err(|err| {
        match (head_before.as_ref(), repo.head().ok().flatten()) {
            (before, Some(after)) if before != Some(&after) => {
                anyhow!(git_overlay_txn::commit_checkpoint_failed_advice(
                    &after.short(),
                    Some(message),
                    &err,
                    true,
                ))
            }
            _ => err,
        }
    })?;
    let summary_base = report
        .git_checkpoint
        .as_ref()
        .map(|r| r.summary.as_str())
        .unwrap_or(report.summary.as_str());
    let trust = git_overlay_txn::post_verify_commit(repo);
    let output = GitProjectionCommitOutput {
        output_kind: "commit",
        status: "committed",
        action: "commit",
        state_id: report.state_id.short(),
        git_commit: report.git_commit.clone(),
        git_previous_commit,
        summary: staged_commit_summary(summary_base, &intent),
        confidence: report.confidence,
        git_index: Some(GitIndexPlan::index_only(&intent)),
        included_pending_capture: pending_capture.map(|state| state.short()),
        principal: report.principal.into(),
        agent: report.agent.map(CommitAgentOutput::from),
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
    render_git_projection_commit(
        &output,
        should_output_json(cli, Some(repo.config())),
        repo.capability(),
    )?;
    Ok(())
}

fn staged_commit_summary(summary: &str, intent: &GitIndexIntent) -> String {
    core_staged_commit_summary(summary, intent.staged_paths.len(), intent.extra_paths.len())
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

impl From<CommitGitIndexPlan> for GitIndexPlan {
    fn from(plan: CommitGitIndexPlan) -> Self {
        Self {
            commit_mode: plan.commit_mode,
            has_staged_changes: plan.has_staged_changes,
            staged_paths: plan.staged_paths,
            unstaged_paths: plan.unstaged_paths,
            untracked_paths: plan.untracked_paths,
            will_commit: plan.will_commit,
            preserved_after_commit: plan.preserved_after_commit,
        }
    }
}

impl GitIndexPlan {
    pub(crate) fn from_intent(intent: &GitIndexIntent, include_all: bool) -> Self {
        plan_commit_git_index(&intent.staged_paths, &intent.extra_paths, include_all).into()
    }

    /// Plan for an index-only commit: checkpoint exactly the staged index (which
    /// may be empty, as on the `--no-all` path) and preserve every unstaged or
    /// untracked worktree path. Never sweeps the worktree.
    pub(crate) fn index_only(intent: &GitIndexIntent) -> Self {
        plan_commit_git_index_only(&intent.staged_paths, &intent.extra_paths).into()
    }
}

// Plain-Git observe-path index planning lives in
// `heddle_core::git_index_plan_for_root` / `plain_git_status_report`. Overlay
// commit paths below use repo-scoped `git_index_intent_for_repo`.

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
                    entry.set_name(name)?;
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
            TreeEntry::file(
                leaf_name(path),
                hash,
                entry.mode == GIT_MODE_FILE_EXECUTABLE,
            )?
        }
        mode if mode == GIT_MODE_SYMLINK => {
            let hash = import_index_blob(repo, git, entry.oid, path)?;
            TreeEntry::symlink(leaf_name(path), hash)?
        }
        mode if mode == GIT_MODE_COMMIT => TreeEntry::gitlink(leaf_name(path), entry.oid)?,
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

fn leaf_name(path: &str) -> String {
    tree_leaf_name(path)
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

fn commit_next_action(trust: &RepositoryVerificationState) -> Option<String> {
    commit_next_action_from_trust(
        &trust.recommended_action,
        trust.verified,
        trust.default_remote.is_some(),
    )
}

fn pending_capture_before_commit(repo: &Repository) -> Result<Option<StateId>> {
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
    if tip == current.state_id {
        return Ok(None);
    }
    if repo
        .latest_git_checkpoint_for_change(&current.state_id)?
        .is_some()
    {
        return Ok(None);
    }
    Ok(Some(current.state_id))
}

fn with_commit_action_metadata(mut output: GitProjectionCommitOutput) -> GitProjectionCommitOutput {
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

fn render_git_projection_commit(
    output: &GitProjectionCommitOutput,
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
                    style::state_id(&output.state_id),
                    style::dim(&git_commit[..std::cmp::min(12, git_commit.len())])
                ),
                None => format!(
                    "Committed Heddle state {}",
                    style::state_id(&output.state_id)
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
                style::state_id(pending)
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
    core_commit_scope_text(plan.commit_mode)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_to_commit_advice_names_status_recovery() {
        let advice = nothing_to_commit_advice();

        assert_eq!(advice.kind, "nothing_to_commit");
        assert_eq!(advice.primary_command, "heddle status");
        assert!(advice.error.contains("nothing to commit"));
        assert!(advice.primary_hint().contains("heddle status"));
    }
}
