// SPDX-License-Identifier: Apache-2.0
//! Merge command implementation.

use std::{fs, path::Path};

use anyhow::{Context, Result, anyhow};
use objects::{
    object::{Attribution, ChangeId, ThreadName, Tree},
    store::LocalObjectStore,
};
use oplog::{LocalOpLogBackend, LocalOpLogRecorder, OpBatch, OpRecord};
use refs::Head;
use repo::{
    AgentRegistry, AgentStatus, Repository, Thread, ThreadFreshness, ThreadIntegrationPolicy,
    ThreadManager, ThreadState, describe_thread_advice,
};
use serde::Serialize;
use serde_json::Value;
use sley::Repository as SleyRepository;

use super::{
    action_line::print_nested_next,
    advice::RecoveryAdvice,
    command_catalog::ActionTemplate,
    diff::{DiffOutput, SemanticChangeEntry, compute_state_diff, compute_tree_diff},
    git_overlay_health::{
        RepositoryVerificationState, action_template, build_repository_verification_state,
        override_trust_recommended_action, repository_verification_blocked_advice,
        serialize_empty_action_as_null,
    },
    next_action::{NextActionValidationContext, write_command_json},
    operator_core::{OperatorAction, OperatorCommandOutput, blocked_operator_exit_code},
    ready_cmd::{worktree_dirty, worktree_dirty_paths},
    snapshot::ensure_current_state,
    thread_cmd::{refresh_thread_freshness, thread_not_found_advice},
    thread_landing::{land_command_for_thread, land_local_command},
    worktree_safety::ensure_worktree_clean,
};
use crate::{
    cli::{Cli, output_is_compact, should_output_json, style, worktree_status_options},
    config::UserConfig,
};

mod git_commit;
pub(crate) mod merge_algo;
mod merge_plan;
mod merge_relation;
mod merge_renames;
mod rename_matcher;

use git_commit::{GitCommitInfo, GitCommitPreview};
pub(crate) use merge_algo::{ConflictLabels, MergeStrategy};
use merge_algo::{apply_merged_tree, three_way_merge};
use merge_plan::MergePlan;
use merge_relation::MergeRelationKind;
use repo::{CommitGraphIndex, find_merge_base};

#[derive(Clone, Debug, Serialize)]
struct RenameEntry {
    from: String,
    to: String,
    score: f64,
}

#[derive(Clone, Debug, Serialize)]
pub struct ThreadPreviewReport {
    pub thread: String,
    pub thread_mode: String,
    pub thread_state: String,
    pub freshness: String,
    pub task: Option<String>,
    pub changed_paths: Vec<String>,
    pub changed_path_count: usize,
    pub impact_categories: Vec<String>,
    pub heavy_impact_paths: Vec<String>,
    pub merge_relation: String,
    pub conflicts: Vec<String>,
    pub conflict_count: usize,
    pub blockers: Vec<String>,
    // "" means "no action selected" internally; the wire contract is null
    // (HeddleCo/heddle#645) — the boundary walker rejects raw empties.
    #[serde(serialize_with = "serialize_empty_action_as_null")]
    pub recommended_action: String,
    pub recommended_action_template: Option<ActionTemplate>,
    pub thread_health: String,
}

impl ThreadPreviewReport {
    pub(crate) fn refresh_recommended_action_metadata(&mut self) {
        self.recommended_action_template = action_template(&self.recommended_action);
    }
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct MergeOutput {
    #[serde(flatten)]
    pub operator: OperatorCommandOutput,
    pub would_merge: bool,
    pub applied: bool,
    pub fast_forward: bool,
    pub preview_only: bool,
    pub merge_state: Option<String>,
    pub conflicts: Vec<String>,
    pub preview_summary: Vec<String>,
    pub thread_state: Option<String>,
    pub freshness: Option<String>,
    pub changed_paths: Vec<String>,
    pub changed_path_count: usize,
    pub impact_categories: Vec<String>,
    pub promotion_suggested: bool,
    pub heavy_impact_paths: Vec<String>,
    pub merge_relation: Option<String>,
    pub conflict_count: usize,
    pub thread_health: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    renames: Vec<RenameEntry>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    directory_renames: Vec<RenameEntry>,
    /// Per-symbol deltas produced by the semantic driver
    /// (function_renamed, function_added, function_deleted,
    /// signature_changed, etc.). Present when semantic merge is active so
    /// agents can detect that semantic analysis ran and act on the
    /// rename/symbol mapping programmatically without parsing the
    /// line-by-line `diff` payload. Absent (not `null`) when
    /// `--no-semantic` is set or the build lacks semantic support.
    /// An empty array means "semantic ran but found no symbol-level
    /// deltas" (e.g. non-source files or a no-op fast-forward).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub semantic_changes: Option<Vec<SemanticChangeEntry>>,
    /// Diff between the parent's tip and the thread's tip. Populated
    /// only when the caller passes `--with-diff`. On a successful
    /// non-preview merge the from/to are the pre-merge parent tip and
    /// the thread tip — i.e. the change set that just landed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diff: Option<DiffOutput>,
    /// Preview of the git commit that *would* be written if the user
    /// re-ran without `--preview`. Populated only with
    /// `--git-commit --preview`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub git_commit_preview: Option<GitCommitPreview>,
    /// Real git commit written by `--git-commit` on a non-preview
    /// merge. Populated only after a successful, non-conflict merge
    /// when `--git-commit` was set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub git_commit: Option<GitCommitInfo>,
    #[serde(skip_serializing)]
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(rename = "verification")]
    pub trust: Option<RepositoryVerificationState>,
}

struct MergeOutputInput<'a> {
    repo: &'a Repository,
    thread: &'a Option<Thread>,
    preview_report: Option<&'a ThreadPreviewReport>,
    conflicts: Option<Vec<String>>,
    merge_relation: Option<String>,
    conflict_count: Option<usize>,
    changed_paths: Option<Vec<String>>,
    preview_summary: Vec<String>,
    message: String,
    renames: Vec<RenameEntry>,
    directory_renames: Vec<RenameEntry>,
    merge_state: Option<String>,
    fast_forward: bool,
    preview_only: bool,
    diff: Option<DiffOutput>,
    git_commit_preview: Option<GitCommitPreview>,
    git_commit: Option<GitCommitInfo>,
    /// Extra blockers contributed by post-merge coordination steps
    /// (e.g. `--git-commit` failing on dirty git state). Merged into
    /// the operator's final `blockers` list and force `status` to
    /// `"blocked"` even when the heddle merge itself completed.
    extra_blockers: Vec<String>,
    /// Top-level mirror of `diff.semantic_changes`. Threaded through
    /// `MergeOutputInput` so every return path sets it consistently
    /// (instead of relying on each call site to remember). See the
    /// field doc on `MergeOutput::semantic_changes`.
    semantic_changes: Option<Vec<SemanticChangeEntry>>,
}

struct SourceThreadUncapturedWork {
    checkout_path: String,
    dirty_paths: Vec<String>,
}

#[allow(clippy::too_many_arguments)]
pub fn cmd_merge(
    cli: &Cli,
    track_name: String,
    message: Option<String>,
    no_commit: bool,
    preview: bool,
    with_diff: bool,
    no_semantic: bool,
    git_commit: bool,
) -> Result<()> {
    let cwd_repo = cli.open_repo()?;
    let target_path = cwd_repo.active_worktree_path()?;
    let repo = if target_path == *cwd_repo.root() {
        cwd_repo
    } else {
        Repository::open(&target_path)?
    };

    // `pre_merge` JSON-protocol hook. Veto via non-empty
    // `abort` aborts the merge before any tree work happens.
    let hook_manager = repo::HookManager::new(&repo);
    let hook_ctx = repo::HookContext::new(&repo);
    let pre_merge_payload = serde_json::json!({
        "source": track_name.clone(),
        "target": current_thread_name(&repo),
    });
    if let Ok(Some(resp)) = hook_manager.run_with_payload(
        repo::Hook::PreMerge,
        &hook_ctx,
        &pre_merge_payload,
        std::time::Duration::from_secs(5),
    ) && !resp.abort.is_empty()
    {
        return Err(anyhow!(RecoveryAdvice::hook_veto(
            "pre_merge",
            "merge",
            resp.abort
        )));
    }

    let mut output = merge_thread_into_current(
        &repo,
        &track_name,
        message,
        no_commit,
        preview,
        with_diff,
        no_semantic,
        git_commit,
    )?;
    scope_merge_recommendations_to_cli_repo(cli, &mut output);

    // `post_merge` JSON-protocol hook. Best-effort; can't veto
    // an already-applied merge.
    if !preview {
        let post_merge_payload = serde_json::json!({
            "state_id": output.merge_state.clone().unwrap_or_default(),
        });
        if let Err(err) = hook_manager.run_with_payload(
            repo::Hook::PostMerge,
            &hook_ctx,
            &post_merge_payload,
            std::time::Duration::from_secs(5),
        ) {
            tracing::warn!(error = %err, "post_merge hook error swallowed");
        }
    }

    if preview_did_not_run(&output) {
        return Err(anyhow!(merge_preview_blocked_advice(&output)));
    }

    let exit_code = merge_output_exit_code(&output);
    render_merge_output(cli, &repo, output)?;
    if let Some(code) = exit_code {
        std::process::exit(code);
    }
    Ok(())
}

fn merge_output_exit_code(output: &MergeOutput) -> Option<i32> {
    if output.preview_only && !preview_did_not_run(output) {
        return None;
    }
    blocked_operator_exit_code(&output.operator.status)
}

fn scope_merge_recommendations_to_cli_repo(cli: &Cli, output: &mut MergeOutput) {
    let Some(repo_path) = cli.repo.as_ref() else {
        return;
    };
    output.operator.recommended_action = output
        .operator
        .recommended_action
        .as_deref()
        .map(|action| scope_action_to_repo(action, repo_path));
    output.operator.next_action = output
        .operator
        .next_action
        .as_deref()
        .map(|action| scope_action_to_repo(action, repo_path));
}

fn scope_action_to_repo(action: &str, repo_path: &Path) -> String {
    let Some(rest) = action.strip_prefix("heddle ") else {
        return action.to_string();
    };
    if rest.starts_with("--repo ") || rest.starts_with("-R ") {
        return action.to_string();
    }
    format!(
        "heddle --repo {} {rest}",
        quote_recommended_action_arg(&repo_path.display().to_string())
    )
}

fn quote_recommended_action_arg(value: &str) -> String {
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

/// Resolve current thread name for hook payloads. Returns `""` when
/// HEAD is detached.
fn current_thread_name(repo: &Repository) -> String {
    use refs::Head;
    match repo.head_ref() {
        Ok(Head::Attached { thread }) => thread.to_string(),
        _ => String::new(),
    }
}

fn semantic_merge_enabled(no_semantic: bool) -> bool {
    cfg!(feature = "semantic") && !no_semantic
}

fn merge_strategy_for(use_semantic: bool) -> MergeStrategy {
    if use_semantic {
        MergeStrategy::Semantic
    } else {
        MergeStrategy::HunkOnly
    }
}

fn merge_already_in_progress_advice() -> RecoveryAdvice {
    RecoveryAdvice::safety_refusal(
        "merge_already_in_progress",
        "A merge is already in progress",
        "Inspect the active operation with `heddle status`; resolve it with `heddle continue` or abort it with `heddle resolve --abort`.",
        "merge state is already present for this repository",
        "starting another merge would overwrite or obscure the in-progress conflict state",
        "existing merge state and worktree were left unchanged",
        "heddle status",
        vec![
            "heddle status".to_string(),
            "heddle continue".to_string(),
            "heddle resolve --abort".to_string(),
        ],
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn merge_thread_into_current(
    repo: &Repository,
    track_name: &str,
    message: Option<String>,
    no_commit: bool,
    preview: bool,
    with_diff: bool,
    no_semantic: bool,
    git_commit: bool,
) -> Result<MergeOutput> {
    let use_semantic = semantic_merge_enabled(no_semantic);
    let registry = AgentRegistry::new(repo.heddle_dir());
    let thread_manager = ThreadManager::new(repo.heddle_dir());
    let mut thread = thread_manager.find_by_thread(track_name)?;
    if let Some(ref mut thread) = thread {
        refresh_thread_freshness(repo, thread)?;
    }
    let thread_entry = registry
        .list()?
        .into_iter()
        .filter(|entry| entry.thread == track_name)
        .max_by_key(|entry| entry.started_at);

    let merge_manager = repo.merge_state_manager();
    if merge_manager.is_merge_in_progress() {
        return Err(anyhow!(merge_already_in_progress_advice()));
    }

    if preview {
        ensure_worktree_clean(repo, "merge")?;
    }

    if preview {
        let trust = build_repository_verification_state(repo);
        if trust_blocks_merge_preview(&trust) {
            return Ok(merge_blocked_by_trust_output(
                &thread, None, trust, preview, None,
            ));
        }
    }

    let merge_target_id = repo
        .refs()
        .get_thread(&ThreadName::new(track_name))?
        .ok_or_else(|| anyhow!(thread_not_found_advice(track_name, "merge")))?;

    let current_change = ensure_current_state(
        repo,
        &UserConfig::load_default().unwrap_or_default(),
        Some(format!(
            "Bootstrap git-overlay before merging {}",
            track_name
        )),
    )?;
    let current_state = repo
        .store()
        .get_state(&current_change)?
        .ok_or_else(|| anyhow!("Current state not found"))?;

    ensure_worktree_clean(repo, "merge")?;

    let mut graph = CommitGraphIndex::new(repo);
    // Codex r13 P2: the preview report's content-merge strategy must
    // match the strategy the actual merge plan (below) will use, so
    // the `preview_summary` lines don't contradict the real outcome
    // (e.g. reporting `conflicts: 1 path conflict(s)` on a structural
    // reshape that semantic resolves cleanly).
    let preview_strategy = merge_strategy_for(use_semantic);
    let current_thread = repo
        .current_lane()?
        .unwrap_or_else(|| "detached".to_string());
    // heddle#144: the inner preview report MUST compute its 3-way merge
    // against the actual destination of *this* merge (the operator's
    // current HEAD), not `thread.target_thread`. Otherwise running
    // `heddle merge A` from a thread B whose tip diverges from A's
    // recorded target (often `main`) yields a preview whose
    // `preview_summary` line claims one outcome while the apply path
    // produces another.
    let preview_report = match thread.as_mut() {
        Some(thread) => Some(build_thread_preview_report_with_graph(
            repo,
            &mut graph,
            thread,
            preview,
            preview_strategy,
            Some(PreviewTarget {
                label: &current_thread,
                change_id: current_state.change_id,
            }),
        )?),
        None => None,
    };
    if let Some(thread) = thread.as_ref()
        && let Some(uncaptured) = source_thread_uncaptured_work(repo, thread)?
    {
        return Err(anyhow!(source_thread_uncaptured_work_advice(
            thread, uncaptured, preview,
        )));
    }
    if let Some(output) =
        merge_freshness_preflight_output(repo, &thread, preview_report.as_ref(), preview)
    {
        return Ok(output);
    }
    let preview_summary = build_preview_summary(preview_report.as_ref());
    let current_label = format!("CURRENT ({current_thread})");
    let incoming_label = format!("INCOMING ({track_name})");
    let merge_plan = MergePlan::for_merge_command(
        repo,
        &mut graph,
        &current_state.change_id,
        &merge_target_id,
        ConflictLabels {
            current: &current_label,
            incoming: &incoming_label,
            strategy: merge_strategy_for(use_semantic),
        },
    )?;

    // Helper for the `--with-diff` payload. Each branch picks the right
    // (from, to) once it knows what actually landed — see the per-branch
    // calls below. Pre-fix, the function computed a single
    // `current..merge_target` diff up-front and reused it everywhere; that
    // payload is wrong for non-fast-forward 3-way merges (it can include
    // removals of current-branch edits that the merge actually preserves)
    // and for `AlreadyUpToDate` (it can be non-empty when nothing landed).
    let diff_for = |from: &ChangeId, to: &ChangeId| -> Result<Option<DiffOutput>> {
        if !with_diff {
            return Ok(None);
        }
        Ok(Some(compute_state_diff(repo, from, to, use_semantic, 3)?))
    };
    // heddle#153: surface per-symbol deltas at the top level so agents
    // can detect that semantic analysis ran and act on the rename
    // mapping without digging into `diff.semantic_changes`. We derive
    // this from the (already-computed) diff payload when both semantic
    // merge and `--with-diff` are active; without `--with-diff` the
    // diff isn't computed at all, so there's nothing to mirror. Use
    // `Some(vec![])` (not `None`) on the with-diff+semantic path even
    // when the driver found no symbol changes, so consumers can branch
    // on field presence to detect "semantic mode honored".
    let top_level_semantic = |diff: Option<&DiffOutput>| -> Option<Vec<SemanticChangeEntry>> {
        if !use_semantic || !with_diff {
            return None;
        }
        Some(
            diff.and_then(|d| d.semantic_changes.clone())
                .unwrap_or_default(),
        )
    };

    if merge_plan.relation().kind() == MergeRelationKind::AlreadyUpToDate {
        let trust = build_repository_verification_state(repo);
        if !trust.verified {
            return Ok(merge_blocked_by_trust_output(
                &thread,
                preview_report.as_ref(),
                trust,
                preview,
                Some(merge_plan.relation().as_json_value().to_string()),
            ));
        }
        // Already-up-to-date means the merge doesn't write anything — the
        // current state already contains the target. The honest diff is
        // empty; producing `current..target` would make the JSON falsely
        // claim a change landed.
        let already_up_to_date_diff = if with_diff {
            Some(empty_diff_output(&current_state.change_id))
        } else {
            None
        };
        return Ok(merge_output_from_report(MergeOutputInput {
            repo,
            thread: &thread,
            preview_report: preview_report.as_ref(),
            conflicts: Some(vec![]),
            merge_relation: Some(merge_plan.relation().as_json_value().to_string()),
            conflict_count: Some(0),
            changed_paths: Some(Vec::new()),
            preview_summary: vec![],
            message: "Already up to date".to_string(),
            renames: vec![],
            directory_renames: vec![],
            merge_state: None,
            fast_forward: false,
            preview_only: preview,
            semantic_changes: top_level_semantic(already_up_to_date_diff.as_ref()),
            diff: already_up_to_date_diff,
            git_commit_preview: None,
            git_commit: None,
            extra_blockers: Vec::new(),
        }));
    }

    if merge_plan.relation().kind() == MergeRelationKind::FastForward {
        // Use the parent↔thread-tip diff as the source of truth for
        // which paths the merge writes — see `merge_changed_paths` for
        // why thread.changed_paths can't be relied on here.
        let ff_paths = merge_changed_paths(repo, &current_state.change_id, &merge_target_id)?;

        // FF: current..target IS the change set that lands. Compute once
        // and reuse for any per-branch return below.
        let (ff_renames, ff_directory_renames) =
            fast_forward_renames(repo, &current_state.change_id, &merge_target_id)?;
        let ff_diff = diff_for(&current_state.change_id, &merge_target_id)?
            .map(|diff| diff_with_known_renames(diff, &ff_renames));

        // Pre-flight `--git-commit` validation (real merge only). On
        // preview we skip the dirty-tree check — the operator hasn't
        // committed to landing anything yet, just wants to see the
        // would-be commit message.
        let mut git_commit_blockers: Vec<String> = Vec::new();
        if git_commit
            && !preview
            && let Err(blocked) = git_commit::validate_git_state(repo, &ff_paths)
        {
            git_commit_blockers = blocked.blockers;
        }

        if !git_commit_blockers.is_empty() {
            // Fail loudly *before* advancing heddle state.
            return Ok(merge_output_from_report(MergeOutputInput {
                repo,
                thread: &thread,
                preview_report: preview_report.as_ref(),
                conflicts: Some(vec![]),
                merge_relation: Some("fast_forward".to_string()),
                conflict_count: Some(0),
                changed_paths: Some(ff_paths.clone()),
                preview_summary,
                message: "Fast-forward blocked: --git-commit precondition failed".to_string(),
                renames: ff_renames,
                directory_renames: ff_directory_renames,
                merge_state: None,
                fast_forward: false,
                preview_only: preview,
                semantic_changes: top_level_semantic(ff_diff.as_ref()),
                diff: ff_diff,
                git_commit_preview: None,
                git_commit: None,
                extra_blockers: git_commit_blockers,
            }));
        }

        let git_branch_before = if git_commit && !preview {
            Some(
                repo.git_overlay_current_branch()?
                    .unwrap_or_else(|| "HEAD".to_string()),
            )
        } else {
            None
        };
        let git_oid_before = if git_commit && !preview {
            git_rev_parse_head(repo.root())
        } else {
            None
        };
        let source_git_parent = if git_commit {
            source_git_parent_for_thread(repo, track_name, &merge_target_id)?
        } else {
            None
        };
        let mut git_commit_preview_payload: Option<GitCommitPreview> = None;
        let mut git_commit_info: Option<GitCommitInfo> = None;

        if !preview {
            // Preserve attached-HEAD semantics on fast-forward: if HEAD is
            // attached to a thread, advance that thread's ref so
            // `heddle merge X` from inside thread Y leaves Y pointing at
            // the integrated state. See `Repository::fast_forward_attached`
            // and the regression test
            // `merge_fast_forward_advances_current_thread`.
            //
            // We perform the FF *without recording* an `OpRecord::Goto`
            // and then explicitly record `OpRecord::FastForward` so
            // both ends of the FF are captured. r1 (heddle#99) added the
            // variant to fix stranded-ref-on-undo. r2 added
            // `post_target_id` so redo replays the recorded SHA instead
            // of re-resolving `source_thread → tip` at apply time —
            // closes Codex's non-determinism finding on PR #109.
            let head_before_ff = repo.head_ref()?;
            repo.fast_forward_attached_without_record(&merge_target_id)?;
            match &head_before_ff {
                Head::Attached {
                    thread: target_thread,
                } => {
                    repo.oplog().record_fast_forward(
                        &ThreadName::new(track_name),
                        target_thread,
                        &current_state.change_id,
                        &merge_target_id,
                        Some(&repo.op_scope_key()),
                    )?;
                }
                Head::Detached { state } => {
                    // No attached thread to restore on undo. The generic
                    // `Goto` inverse is sufficient — preserve historic
                    // behavior for detached HEAD.
                    repo.oplog().record_goto(
                        &merge_target_id,
                        Some(state),
                        Some(&repo.op_scope_key()),
                    )?;
                }
            }
            if let Some(entry) = &thread_entry {
                registry.update_status(&entry.session_id, AgentStatus::Merged)?;
            }
            if let Some(thread) = thread.as_mut() {
                thread.state = ThreadState::Merged;
                thread.merged_state = Some(merge_target_id.short());
                thread.current_state = Some(merge_target_id.short());
                thread.updated_at = chrono::Utc::now();
                thread.freshness = ThreadFreshness::Current;
                thread_manager.save(thread)?;
            }

            if git_commit {
                // FF advances heddle to `merge_target_id` (the thread
                // tip). Use that as the `Merge-State` trailer — there's
                // no synthetic merge state on a fast-forward.
                let attribution = Attribution::human(repo.get_principal()?);
                let ff_message = preview_merge_message(repo, &message, thread.as_ref(), track_name);
                let commit_message = git_commit::build_commit_message(
                    &ff_message,
                    &merge_target_id.short(),
                    &attribution,
                );
                let extra_parents = source_git_parent.clone().into_iter().collect::<Vec<_>>();
                let info = git_commit::write_git_commit(
                    repo,
                    &merge_target_id,
                    &ff_paths,
                    &commit_message,
                    &extra_parents,
                )?;
                finalize_merge_git_checkpoint(
                    repo,
                    &merge_target_id,
                    git_branch_before.unwrap_or_else(|| "HEAD".to_string()),
                    git_oid_before,
                    &info.sha,
                    &ff_message,
                )?;
                git_commit_info = Some(info);
            }
        } else if git_commit {
            // Preview path: render the would-be commit message.
            let attribution = Attribution::human(repo.get_principal()?);
            let ff_message = preview_merge_message(repo, &message, thread.as_ref(), track_name);
            let preview_msg = git_commit::build_commit_message(
                &ff_message,
                &merge_target_id.short(),
                &attribution,
            );
            git_commit_preview_payload = Some(GitCommitPreview {
                message: preview_msg,
                files: ff_paths.clone(),
            });
        }
        let output_changed_paths = ff_paths.clone();
        let output_changed_path_count = output_changed_paths.len();

        let recommended_action = if preview {
            if let Some(thread) = thread.as_ref() {
                if thread.state == ThreadState::Ready {
                    mark_merge_previewed(repo, &thread.id)?;
                }
                if let Some(report) = preview_report.as_ref()
                    && !report.blockers.is_empty()
                    && !report.recommended_action.trim().is_empty()
                    && report
                        .blockers
                        .iter()
                        .any(|blocker| is_real_merge_blocker(blocker))
                {
                    Some(report.recommended_action.clone())
                } else if preview_report
                    .as_ref()
                    .is_some_and(preview_needs_readiness_review)
                {
                    None
                } else {
                    Some(land_local_command(&thread.id))
                }
            } else {
                None
            }
        } else {
            None
        };
        return Ok(MergeOutput {
            operator: OperatorCommandOutput {
                status: if preview { "preview" } else { "completed" }.to_string(),
                action: OperatorAction::Merge,
                message: match (preview, git_commit, repo.head_ref()?) {
                    (true, true, Head::Attached { thread }) => {
                        format!(
                            "Would advance {} to {} and write a Git checkpoint commit",
                            thread,
                            merge_target_id.short()
                        )
                    }
                    (true, true, Head::Detached { .. }) => {
                        format!(
                            "Would advance to {} and write a Git checkpoint commit",
                            merge_target_id.short()
                        )
                    }
                    (false, true, Head::Attached { thread }) => {
                        format!(
                            "Advanced {} to {} and wrote a Git checkpoint commit",
                            thread,
                            merge_target_id.short()
                        )
                    }
                    (false, true, Head::Detached { .. }) => {
                        format!(
                            "Advanced to {} and wrote a Git checkpoint commit",
                            merge_target_id.short()
                        )
                    }
                    (true, false, Head::Attached { thread }) => {
                        format!(
                            "Would fast-forward {} to {}",
                            thread,
                            merge_target_id.short()
                        )
                    }
                    (true, false, Head::Detached { .. }) => {
                        format!("Would fast-forward to {}", merge_target_id.short())
                    }
                    (false, false, Head::Attached { thread }) => {
                        format!("Fast-forwarded {} to {}", thread, merge_target_id.short())
                    }
                    (false, false, Head::Detached { .. }) => {
                        format!("Fast-forwarded to {}", merge_target_id.short())
                    }
                },
                // Fast-forward never has conflicts, so anything in
                // the preview-stage `blockers` list is advisory. The
                // operation either advanced state (apply path) or
                // would advance state (preview path) — either way
                // these belong in `warnings`, not `blockers`.
                blockers: Vec::new(),
                warnings: preview_report
                    .as_ref()
                    .map(|r| r.blockers.clone())
                    .unwrap_or_default(),
                next_action: recommended_action.clone(),
                recommended_action: recommended_action.clone(),
            },
            would_merge: preview,
            applied: !preview,
            fast_forward: true,
            preview_only: preview,
            merge_state: (!preview).then(|| merge_target_id.short()),
            conflicts: vec![],
            preview_summary,
            thread_state: thread.as_ref().map(|thread| thread.state.to_string()),
            freshness: thread.as_ref().map(|thread| thread.freshness.to_string()),
            changed_paths: output_changed_paths,
            changed_path_count: output_changed_path_count,
            impact_categories: thread_impacts(&thread),
            promotion_suggested: thread
                .as_ref()
                .map(|thread| thread.promotion_suggested)
                .unwrap_or(false),
            heavy_impact_paths: thread_heavy_paths(&thread),
            merge_relation: Some("fast_forward".to_string()),
            conflict_count: 0,
            thread_health: merge_output_thread_health(thread.as_ref(), preview_report.as_ref()),
            renames: ff_renames,
            directory_renames: ff_directory_renames,
            semantic_changes: top_level_semantic(ff_diff.as_ref()),
            diff: ff_diff,
            git_commit_preview: git_commit_preview_payload,
            git_commit: git_commit_info,
            trust: recommended_action
                .as_ref()
                .map(|action| {
                    let mut trust = build_repository_verification_state(repo);
                    override_trust_recommended_action(&mut trust, action.clone());
                    trust
                })
                .or_else(|| Some(build_repository_verification_state(repo))),
        });
    }

    let merge_base_id = merge_plan
        .relation()
        .merge_base_id()
        .ok_or_else(|| anyhow!("Merge base missing from merge plan"))?;
    let merge_result = merge_plan
        .merge_result()
        .ok_or_else(|| anyhow!("Merge result missing from merge plan"))?;
    let rename_entries: Vec<RenameEntry> = merge_result
        .renames
        .iter()
        .map(|(from, to, score)| RenameEntry {
            from: from.clone(),
            to: to.clone(),
            score: *score,
        })
        .collect();
    let dir_rename_entries: Vec<RenameEntry> = merge_result
        .directory_renames
        .iter()
        .map(|(from, to)| RenameEntry {
            from: from.clone(),
            to: to.clone(),
            score: 1.0,
        })
        .collect();

    if preview {
        // For `--git-commit --preview`, render the would-be commit
        // message so the operator can review it before re-running
        // without `--preview`. We can't surface a real `Merge-State`
        // change-id (no merge state has been written yet) — emit the
        // placeholder `<pending>` and let real-mode produce the final
        // trailer once the merge state exists.
        let git_commit_preview = if git_commit && merge_result.conflicts.is_empty() {
            let preview_message =
                preview_merge_message(repo, &message, thread.as_ref(), track_name);
            let attribution = Attribution::human(repo.get_principal()?);
            let preview_msg =
                git_commit::build_commit_message(&preview_message, "<pending>", &attribution);
            Some(GitCommitPreview {
                message: preview_msg,
                files: merge_changed_paths(repo, &current_state.change_id, &merge_target_id)?,
            })
        } else {
            None
        };
        // 3-way preview diff: report the computed merge tree, not
        // `current..merge_target`. The source-tip diff can show
        // deletions of files that only exist on the destination branch
        // even though a real 3-way merge would preserve them.
        let preview_path_diff = compute_tree_diff(
            repo,
            &current_state.change_id,
            &merge_result.tree,
            "<merged-preview>",
            with_diff && use_semantic,
            if with_diff { 3 } else { 0 },
        )
        .map(|diff| diff_with_known_renames(diff, &rename_entries))?;
        let preview_changed_paths = diff_changed_paths(&preview_path_diff);
        let preview_diff = with_diff.then_some(preview_path_diff);
        if merge_result.conflicts.is_empty()
            && thread
                .as_ref()
                .is_some_and(|thread| thread.state == ThreadState::Ready)
            && let Some(thread) = thread.as_ref()
        {
            mark_merge_previewed(repo, &thread.id)?;
        }
        return Ok(merge_output_from_report(MergeOutputInput {
            repo,
            thread: &thread,
            preview_report: preview_report.as_ref(),
            conflicts: Some(merge_result.conflicts.clone()),
            merge_relation: Some(merge_plan.relation().as_json_value().to_string()),
            conflict_count: Some(merge_plan.relation().conflict_count()),
            changed_paths: Some(preview_changed_paths.clone()),
            preview_summary,
            message: merge_preview_message(
                thread.as_ref(),
                track_name,
                merge_result.conflicts.len(),
                preview_changed_paths.len(),
            ),
            renames: rename_entries.clone(),
            directory_renames: dir_rename_entries.clone(),
            merge_state: None,
            fast_forward: false,
            preview_only: true,
            semantic_changes: top_level_semantic(preview_diff.as_ref()),
            diff: preview_diff,
            git_commit_preview,
            git_commit: None,
            extra_blockers: Vec::new(),
        }));
    }

    apply_merged_tree(repo, &merge_result.tree)?;

    if !merge_result.conflicts.is_empty() {
        merge_manager.start(
            current_state.change_id,
            merge_target_id,
            Some(merge_base_id),
            merge_result.conflicts.clone(),
        )?;
        // Conflicted merge: the merge wrote a partial tree containing
        // conflict markers. Reporting either `current..target` or
        // `current..merge_result.tree` here would be misleading — the
        // user must resolve before any well-defined diff exists. Empty
        // diff is the honest signal.
        let conflict_diff = if with_diff {
            Some(empty_diff_output(&current_state.change_id))
        } else {
            None
        };
        return Ok(merge_output_from_report(MergeOutputInput {
            repo,
            thread: &thread,
            preview_report: preview_report.as_ref(),
            conflicts: Some(merge_result.conflicts.clone()),
            merge_relation: Some(merge_plan.relation().as_json_value().to_string()),
            conflict_count: Some(merge_plan.relation().conflict_count()),
            changed_paths: Some(merge_result.conflicts.clone()),
            preview_summary,
            message: "Merged with conflicts".to_string(),
            renames: rename_entries,
            directory_renames: dir_rename_entries,
            merge_state: None,
            fast_forward: false,
            preview_only: false,
            semantic_changes: top_level_semantic(conflict_diff.as_ref()),
            diff: conflict_diff,
            git_commit_preview: None,
            git_commit: None,
            extra_blockers: Vec::new(),
        }));
    }

    if no_commit {
        // 3-way clean merge, not committed. The actual change set is
        // `current_tree..merge_result.tree`, but the merged tree isn't
        // yet a committed `State` — `compute_state_diff` can't run, and
        // the public `DiffOutput`/`FileChange` constructor surface goes
        // through a private module we can't import here. Document the
        // gap honestly: when the operator passes `--with-diff` together
        // with `--no-commit`, surface `None`; the diff materializes on
        // the post-snapshot path. Re-running without `--no-commit` (or
        // running `heddle diff` against the new state) recovers the
        // full payload.
        let no_commit_path_diff = compute_tree_diff(
            repo,
            &current_state.change_id,
            &merge_result.tree,
            "<merged-no-commit>",
            false,
            0,
        )
        .map(|diff| diff_with_known_renames(diff, &rename_entries))?;
        let no_commit_changed_paths = diff_changed_paths(&no_commit_path_diff);
        let no_commit_diff: Option<DiffOutput> = None;
        return Ok(merge_output_from_report(MergeOutputInput {
            repo,
            thread: &thread,
            preview_report: preview_report.as_ref(),
            conflicts: Some(vec![]),
            merge_relation: Some(merge_plan.relation().as_json_value().to_string()),
            conflict_count: Some(merge_plan.relation().conflict_count()),
            changed_paths: Some(no_commit_changed_paths),
            preview_summary,
            message: "Merge applied (not committed)".to_string(),
            renames: rename_entries,
            directory_renames: dir_rename_entries,
            merge_state: None,
            fast_forward: false,
            preview_only: false,
            semantic_changes: top_level_semantic(no_commit_diff.as_ref()),
            diff: no_commit_diff,
            git_commit_preview: None,
            git_commit: None,
            extra_blockers: Vec::new(),
        }));
    }

    let merge_message =
        message.unwrap_or_else(|| default_merge_message(repo, thread.as_ref(), track_name));

    let attribution = Attribution::human(repo.get_principal()?);
    // If `--git-commit` is set, validate git state *before* writing
    // the heddle merge state. That way a dirty git tree can't leave us
    // with a half-coordinated outcome (heddle merged, git rejected).
    //
    // Derive paths from the parent↔thread-tip diff rather than
    // `thread.changed_paths`: thread metadata is lazily refreshed and
    // can be empty in synthetic / lightweight setups, but the diff is
    // ground truth for what the merge actually wrote.
    let merge_paths: Vec<String> = if git_commit {
        merge_changed_paths(repo, &current_state.change_id, &merge_target_id)?
    } else {
        Vec::new()
    };
    let mut git_commit_blockers: Vec<String> = Vec::new();
    if git_commit {
        if let Err(blocked) = git_commit::validate_git_state(repo, &merge_paths) {
            git_commit_blockers = blocked.blockers;
        }
        // Extended pre-flight: check anything else we can dry-run before
        // writing heddle state. The original `validate_git_state` covers
        // dirty-tree and detached-HEAD; this catches missing commit
        // identity and missing changed paths — both produce
        // post-snapshot failures that leave heddle advanced and git
        // uncommitted. Fail closed BEFORE `snapshot_merge_with_attribution`
        // runs.
        let extended = validate_git_commit_preconditions_extended(repo.root(), &merge_paths);
        git_commit_blockers.extend(extended);
    }
    if !git_commit_blockers.is_empty() {
        // Surface as a `blocked` outcome — heddle hasn't committed
        // anything yet, so the operator can fix git and retry without
        // any cleanup. Empty diff: nothing landed, so nothing to
        // describe.
        let blocked_diff = if with_diff {
            Some(empty_diff_output(&current_state.change_id))
        } else {
            None
        };
        return Ok(merge_output_from_report(MergeOutputInput {
            repo,
            thread: &thread,
            preview_report: preview_report.as_ref(),
            conflicts: Some(vec![]),
            merge_relation: Some(merge_plan.relation().as_json_value().to_string()),
            conflict_count: Some(merge_plan.relation().conflict_count()),
            changed_paths: Some(Vec::new()),
            preview_summary,
            message: "Merge blocked: git --git-commit precondition failed".to_string(),
            renames: rename_entries,
            directory_renames: dir_rename_entries,
            merge_state: None,
            fast_forward: false,
            preview_only: false,
            semantic_changes: top_level_semantic(blocked_diff.as_ref()),
            diff: blocked_diff,
            git_commit_preview: None,
            git_commit: None,
            extra_blockers: git_commit_blockers,
        }));
    }

    let git_branch_before = if git_commit {
        Some(
            repo.git_overlay_current_branch()?
                .unwrap_or_else(|| "HEAD".to_string()),
        )
    } else {
        None
    };
    let git_oid_before = if git_commit {
        git_rev_parse_head(repo.root())
    } else {
        None
    };
    let source_git_parent = if git_commit {
        source_git_parent_for_thread(repo, track_name, &merge_target_id)?
    } else {
        None
    };

    let new_state = repo.snapshot_merge_with_attribution(
        &merge_target_id,
        Some(merge_message.clone()),
        None,
        attribution.clone(),
        Some(merge_base_id),
        false,
    )?;

    if let Some(entry) = &thread_entry {
        registry.update_status(&entry.session_id, AgentStatus::Merged)?;
    }
    if let Some(thread) = thread.as_mut() {
        thread.state = ThreadState::Merged;
        thread.merged_state = Some(new_state.change_id.short());
        thread.current_state = Some(new_state.change_id.short());
        thread.updated_at = chrono::Utc::now();
        thread.freshness = ThreadFreshness::Current;
        thread_manager.save(thread)?;
    }

    // Heddle has advanced. If `--git-commit` is set we attempt the git
    // commit now — but we DON'T `?`-propagate a failure. Up-front
    // validation already drained every dry-runnable failure mode; what
    // remains (hooks rejecting, identity rotated mid-call, concurrent
    // index lock, FS errors) we surface as a structured `blocked`
    // outcome with a precise recovery hint pointing at the intact
    // heddle merge state. The operator can resolve git and re-run
    // `git commit` manually without losing the merge.
    let mut git_commit_info: Option<GitCommitInfo> = None;
    let mut post_snapshot_git_blockers: Vec<String> = Vec::new();
    if git_commit {
        let commit_message = git_commit::build_commit_message(
            &merge_message,
            &new_state.change_id.short(),
            &attribution,
        );
        let extra_parents = source_git_parent.clone().into_iter().collect::<Vec<_>>();
        match git_commit::write_git_commit(
            repo,
            &new_state.change_id,
            &merge_paths,
            &commit_message,
            &extra_parents,
        ) {
            Ok(info) => {
                git_commit_info = Some(info.clone());
                if let Err(err) = finalize_merge_git_checkpoint(
                    repo,
                    &new_state.change_id,
                    git_branch_before.unwrap_or_else(|| "HEAD".to_string()),
                    git_oid_before,
                    &info.sha,
                    &merge_message,
                ) {
                    tracing::warn!(
                        error = %err,
                        state = %new_state.change_id.short(),
                        git_commit = %info.sha,
                        "git commit succeeded after heddle merge, but checkpoint metadata recording failed"
                    );
                    post_snapshot_git_blockers.push(format!(
                        "git commit {} was written for Heddle merge {}, but checkpoint metadata recording failed: {}",
                        info.sha,
                        new_state.change_id.short(),
                        err
                    ));
                    post_snapshot_git_blockers.push(format!(
                        "recovery: Heddle merge state {} and Git commit {} are intact; run `heddle verify` \
                         and use its primary recovery command before undoing this merge",
                        new_state.change_id.short(),
                        info.sha
                    ));
                }
            }
            Err(err) => {
                tracing::warn!(
                    error = %err,
                    state = %new_state.change_id.short(),
                    "git commit failed after heddle merge state was written"
                );
                post_snapshot_git_blockers.push(format!(
                    "git commit failed after heddle merge {} landed: {}",
                    new_state.change_id.short(),
                    err
                ));
                post_snapshot_git_blockers.push(format!(
                    "recovery: heddle merge state {} is intact; resolve the Git checkout issue \
                     (identity, locks, or filesystem errors) and run `heddle commit -m \"{}\"` — do NOT re-run `heddle merge`",
                    new_state.change_id.short(),
                    merge_message
                ));
            }
        }
    }

    // 3-way committed merge: `new_state` is the actual landed state.
    // Compute the diff from current → new_state so the JSON describes
    // the change set the user can audit, NOT `current..merge_target`
    // which can include removals of current-branch edits the merge
    // preserved.
    let committed_path_diff = compute_state_diff(
        repo,
        &current_state.change_id,
        &new_state.change_id,
        with_diff && use_semantic,
        if with_diff { 3 } else { 0 },
    )
    .map(|diff| diff_with_known_renames(diff, &rename_entries))?;
    let committed_changed_paths = diff_changed_paths(&committed_path_diff);
    let committed_diff = with_diff.then_some(committed_path_diff);

    let final_message = if post_snapshot_git_blockers.is_empty() {
        format!("Merged as {}", new_state.change_id.short())
    } else {
        format!(
            "Merged as {} (heddle); git commit failed",
            new_state.change_id.short()
        )
    };

    Ok(merge_output_from_report(MergeOutputInput {
        repo,
        thread: &thread,
        preview_report: preview_report.as_ref(),
        conflicts: Some(vec![]),
        merge_relation: Some(merge_plan.relation().as_json_value().to_string()),
        conflict_count: Some(merge_plan.relation().conflict_count()),
        changed_paths: Some(committed_changed_paths),
        preview_summary,
        message: final_message,
        renames: rename_entries,
        directory_renames: dir_rename_entries,
        merge_state: Some(new_state.change_id.short()),
        fast_forward: false,
        preview_only: false,
        semantic_changes: top_level_semantic(committed_diff.as_ref()),
        diff: committed_diff,
        git_commit_preview: None,
        git_commit: git_commit_info,
        extra_blockers: post_snapshot_git_blockers,
    }))
}

fn mark_merge_previewed(repo: &Repository, thread_id: &str) -> Result<()> {
    let manager = ThreadManager::new(repo.heddle_dir());
    let mut thread = manager
        .load(thread_id)?
        .ok_or_else(|| anyhow!(thread_not_found_advice(thread_id, "mark merge previewed")))?;
    thread.integration_policy_result = ThreadIntegrationPolicy {
        status: Some("previewed".to_string()),
        reason: Some("clean merge preview established land path".to_string()),
        manual_resolution_state: thread.integration_policy_result.manual_resolution_state,
        conflicts_resolved_manually: thread.integration_policy_result.conflicts_resolved_manually,
    };
    manager.save(&thread)?;
    Ok(())
}

/// Build a stand-in commit message for `--git-commit --preview` output.
/// Mirrors the real-mode logic in the apply path but doesn't allocate
/// a heddle merge state — used only for the preview surface.
fn preview_merge_message(
    repo: &Repository,
    explicit: &Option<String>,
    thread: Option<&Thread>,
    track_name: &str,
) -> String {
    if let Some(msg) = explicit.as_ref() {
        return msg.clone();
    }
    default_merge_message(repo, thread, track_name)
}

fn default_merge_message(repo: &Repository, thread: Option<&Thread>, track_name: &str) -> String {
    if let Some(intent) =
        thread.and_then(|thread| state_intent(repo, thread.current_state.as_deref()))
    {
        return intent;
    }
    thread
        .and_then(|thread| thread.task.clone())
        .map(|task| format!("Merge thread '{}' ({task})", track_name))
        .unwrap_or_else(|| format!("Merge thread '{}'", track_name))
}

fn merge_preview_message(
    thread: Option<&Thread>,
    track_name: &str,
    conflict_count: usize,
    diff_changed_path_count: usize,
) -> String {
    let subject = thread
        .map(|thread| thread.id.as_str())
        .unwrap_or(track_name);
    let thread_changed_path_count = thread
        .map(|thread| thread.changed_paths.len())
        .unwrap_or_default();
    let changed_path_count = if thread_changed_path_count == 0 {
        diff_changed_path_count
    } else {
        thread_changed_path_count
    }
    .max(conflict_count);
    if conflict_count > 0 {
        format!(
            "Would merge {subject} with {conflict_count} conflict(s) across {changed_path_count} changed path(s)"
        )
    } else {
        format!("Would merge {subject} cleanly across {changed_path_count} changed path(s)")
    }
}

fn state_intent(repo: &Repository, state: Option<&str>) -> Option<String> {
    let state = state?;
    let state_id = repo.resolve_state(state).ok().flatten()?;
    let state = repo.store().get_state(&state_id).ok().flatten()?;
    state.intent.filter(|intent| !intent.trim().is_empty())
}

fn source_git_parent_for_thread(
    repo: &Repository,
    track_name: &str,
    merge_target_id: &ChangeId,
) -> Result<Option<String>> {
    if repo.capability() != repo::RepositoryCapability::GitOverlay {
        return Ok(None);
    }
    let Some(tip) = repo.git_overlay_branch_tip(track_name)? else {
        return Ok(None);
    };
    let Some(mapped_change) = tip.mapped_change else {
        return Ok(None);
    };
    if mapped_change == *merge_target_id {
        return Ok(Some(tip.git_commit));
    }
    let mut graph = CommitGraphIndex::new(repo);
    if graph
        .is_ancestor(&mapped_change, merge_target_id)
        .unwrap_or(false)
    {
        return Ok(Some(tip.git_commit));
    }
    Ok(None)
}

/// Derive the set of paths the merge will touch by diffing the
/// parent's tip against the thread's tip. Used to drive
/// `--git-commit` staging precisely (no `git add -A`) and to
/// distinguish related vs. unrelated dirt during precondition checks.
///
/// Returns the changed paths (added, modified, deleted), preserving
/// diff-output order. Renames surface as a from→to pair so both sides
/// land in the commit.
fn merge_changed_paths(
    repo: &Repository,
    parent_tip: &ChangeId,
    thread_tip: &ChangeId,
) -> Result<Vec<String>> {
    let diff = compute_state_diff(repo, parent_tip, thread_tip, false, 0)?;
    let mut out = Vec::with_capacity(diff.changes.len());
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for change in diff.changes {
        if seen.insert(change.path.clone()) {
            out.push(change.path);
        }
    }
    Ok(out)
}

fn finalize_merge_git_checkpoint(
    repo: &Repository,
    state: &ChangeId,
    branch: String,
    previous_git_oid: Option<String>,
    git_commit: &str,
    summary: &str,
) -> Result<()> {
    repo.record_git_checkpoint(state, git_commit.to_string(), summary.to_string())
        .with_context(|| {
            format!(
                "recording Git checkpoint metadata for merge state {}",
                state.short()
            )
        })?;
    let ids = repo
        .oplog()
        .record_batch_scoped(
            vec![OpRecord::GitCheckpoint {
                branch,
                state: *state,
                previous_git_oid,
                new_git_oid: git_commit.to_string(),
            }],
            Some(&repo.op_scope_key()),
        )
        .with_context(|| {
            format!(
                "recording Git checkpoint undo entry for merge state {}",
                state.short()
            )
        })?;
    let checkpoint_batch_id = ids
        .first()
        .copied()
        .ok_or_else(|| anyhow!("Git checkpoint undo entry was not recorded"))?;
    let merge_batch = find_recent_merge_batch(repo, state)?;
    repo.oplog()
        .coalesce_batches(merge_batch.id, checkpoint_batch_id)
        .with_context(|| {
            format!(
                "coalescing merge state {} and Git checkpoint {} into one undo batch",
                state.short(),
                git_commit
            )
        })?;
    Ok(())
}

fn find_recent_merge_batch(repo: &Repository, state: &ChangeId) -> Result<OpBatch> {
    repo.oplog()
        .recent_batches_scoped(12, Some(&repo.op_scope_key()))?
        .into_iter()
        .find(|batch| {
            batch
                .entries
                .iter()
                .any(|entry| merge_op_targets_state(&entry.operation, state))
        })
        .ok_or_else(|| {
            anyhow!(
                "merge state {} landed but its oplog batch was not found",
                state.short()
            )
        })
}

fn merge_op_targets_state(op: &OpRecord, state: &ChangeId) -> bool {
    match op {
        OpRecord::Snapshot { new_state, .. } => new_state == state,
        OpRecord::Goto { target, .. } => target == state,
        OpRecord::FastForward { post_target_id, .. } => post_target_id == state,
        OpRecord::Checkpoint {
            state: checkpoint_state,
            ..
        } => checkpoint_state == state,
        // These records don't advance HEAD/thread to the merge state the merge
        // flow tracks.
        // Enumerated explicitly (no wildcard) so a new state-advancing variant
        // must be considered as a possible merge target here (heddle#354 r9).
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

fn git_rev_parse_head(root: &Path) -> Option<String> {
    let git = SleyRepository::discover(root).ok()?;
    git.head().ok()?.oid.map(|id| id.to_string())
}

/// Extended pre-flight for `--git-commit`. Catches dry-runnable failure
/// modes that `validate_git_state` doesn't cover, so they surface as
/// pre-snapshot blockers rather than post-snapshot panics that leave
/// heddle advanced while git is uncommitted:
///
/// - **Empty changed-paths set.** `write_git_commit` errors when the
///   merge produced no paths to commit (`refusing to write an empty
///   git commit`); detect that pre-snapshot.
///
/// Heddle writes Git commits with native plumbing and can author them
/// from Heddle's captured principal when Git config is absent, so
/// `user.name`/`user.email` are not preflight requirements.
///
/// Hooks (`pre-commit`, `commit-msg`) intentionally aren't dry-run here
/// — they have side effects, and a strict dry-run would change semantics
/// vs. the real commit. If those reject, the caller surfaces an
/// actionable recovery hint pointing at the intact heddle merge state.
///
/// Strategy chosen: option (a) from the spec — extend up-front
/// validation and accept that the residual unvalidated failure modes
/// (hooks, race conditions, FS errors) require a recovery hint rather
/// than a rollback. Option (b) — explicit rollback of the heddle merge
/// — would introduce undo semantics that don't compose well with the
/// oplog: a partial rollback hand-rolled here can leave the oplog
/// pointing at a state that no longer matches the worktree.
fn validate_git_commit_preconditions_extended(
    repo_root: &std::path::Path,
    merge_paths: &[String],
) -> Vec<String> {
    let mut blockers = Vec::new();

    if merge_paths.is_empty() {
        blockers.push(
            "merge produced no changed paths — git commit would be empty (use heddle merge \
             without --git-commit when nothing changes)"
                .to_string(),
        );
    }

    if !repo_root.join(".git").exists() {
        // `validate_git_state` already reports this; don't double-report.
        return blockers;
    }

    blockers
}

/// Empty `DiffOutput` keyed at the given change-id. Used for return paths
/// that didn't actually advance state (already-up-to-date, conflicted,
/// pre-snapshot blocked) so the JSON honestly reports "no change set
/// landed" instead of pointing at an arbitrary parent..target diff.
fn empty_diff_output(state_id: &ChangeId) -> DiffOutput {
    DiffOutput::new(
        Some(state_id.short()),
        Some(state_id.short()),
        Vec::new(),
        None,
        None,
        None,
    )
}

/// Shared dir → file type-change handler for merge and cherry-pick.
///
/// Called *after* `remove_tracked_descendants*` has stripped the directory's
/// tracked content. Two outcomes:
///
/// - The directory is now empty → `fs::remove_dir(path)` so the subsequent
///   `materialize_blob` call can write a regular file at this path. Without
///   this step `materialize_blob` fails with a kernel "Is a directory"
///   error because its `remove_file(dest)` precondition can only clear
///   files and symlinks.
/// - The directory still holds heddle-ignored content (`.git/`, `target/`,
///   `node_modules/`, …) → return a clear, actionable error naming the
///   surviving entries. We do NOT silently delete heddle-ignored content
///   to make a type-change land; that would defeat the entire reason
///   tracked-descendants removal exists.
///
/// `path` must already be confirmed to exist as a directory by the caller.
pub(crate) fn prepare_dir_for_file_replacement(path: &Path) -> Result<()> {
    match fs::remove_dir(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) if objects::fs_atomic::is_directory_not_empty(&error) => {
            let surviving = list_surviving_entries(path)
                .unwrap_or_else(|_| vec!["<unable to list>".to_string()]);
            let display = if surviving.is_empty() {
                "<unknown ignored content>".to_string()
            } else {
                surviving.join(", ")
            };
            Err(anyhow!(
                "cannot replace directory {} with a file: contains heddle-ignored content ({}) — move or delete those files manually first",
                path.display(),
                display
            ))
        }
        Err(error) => {
            Err(anyhow::Error::from(error)
                .context(format!("removing directory {}", path.display())))
        }
    }
}

fn list_surviving_entries(path: &Path) -> std::io::Result<Vec<String>> {
    let mut names = Vec::new();
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        if let Some(s) = entry.file_name().to_str() {
            names.push(s.to_string());
        } else {
            names.push(entry.file_name().to_string_lossy().into_owned());
        }
    }
    names.sort();
    Ok(names)
}

pub(crate) fn bench_find_merge_base(
    repo: &Repository,
    state_a: &ChangeId,
    state_b: &ChangeId,
) -> Result<Option<ChangeId>> {
    find_merge_base(repo, state_a, state_b)
}

/// Result of trying a 3-way merge between two thread tips.
pub(crate) enum ThreeWayMergeOutcome {
    /// Clean tree with no conflicts. Tree is allocated in the
    /// `parent_repo` object store.
    Clean {
        tree: Tree,
    },
    /// Conflicts exist. `tree` is the partial merge tree containing
    /// conflict markers, and `paths` lists the conflicting path strings.
    Conflicted {
        tree: Tree,
        paths: Vec<String>,
        base: ChangeId,
    },
    /// Already-integrated or fast-forward — caller can take a
    /// simpler advance path. The contained `target` is the tip the
    /// caller should advance to.
    AlreadyIntegrated {
        target: ChangeId,
    },
    FastForward {
        target: ChangeId,
    },
}

/// Compute a 3-way merge between two thread tips without applying
/// it. Used by `heddle thread refresh` to fall back to merge-style
/// reasoning when the commit-by-commit rebase replay would block on
/// an intermediate state but the final trees actually merge cleanly.
///
/// `parent_repo` is where merge bases / commit graph are queried;
/// the returned `Tree` is allocated in that store and the caller is
/// responsible for applying it to a worktree and snapshotting.
pub(crate) fn try_three_way_merge_between_tips(
    parent_repo: &Repository,
    current_tip: &ChangeId,
    target_tip: &ChangeId,
    labels: ConflictLabels<'_>,
) -> Result<ThreeWayMergeOutcome> {
    let mut graph = CommitGraphIndex::new(parent_repo);
    let plan =
        MergePlan::for_merge_command(parent_repo, &mut graph, current_tip, target_tip, labels)?;
    match plan.relation().kind() {
        MergeRelationKind::AlreadyUpToDate => Ok(ThreeWayMergeOutcome::AlreadyIntegrated {
            target: *target_tip,
        }),
        MergeRelationKind::FastForward => Ok(ThreeWayMergeOutcome::FastForward {
            target: *target_tip,
        }),
        MergeRelationKind::CleanApply => {
            let merge_result = plan
                .merge_result()
                .ok_or_else(|| anyhow!("Merge plan missing merge_result for CleanApply"))?;
            Ok(ThreeWayMergeOutcome::Clean {
                tree: merge_result.tree.clone(),
            })
        }
        MergeRelationKind::Conflicted | MergeRelationKind::AlreadyIntegrated => {
            let merge_result = plan
                .merge_result()
                .ok_or_else(|| anyhow!("Merge plan missing merge_result for Conflicted"))?;
            let base = plan
                .relation()
                .merge_base_id()
                .ok_or_else(|| anyhow!("Merge base missing from conflicted merge plan"))?;
            Ok(ThreeWayMergeOutcome::Conflicted {
                tree: merge_result.tree.clone(),
                paths: merge_result.conflicts.clone(),
                base,
            })
        }
    }
}

/// Apply a pre-computed merged tree to the given repo's worktree.
/// Re-export of the internal helper so callers outside the merge
/// module (notably `thread_cmd::refresh_thread`) can converge on the
/// same tree-application path the merge command uses.
pub(crate) fn apply_merged_tree_external(repo: &Repository, tree: &Tree) -> Result<()> {
    apply_merged_tree(repo, tree)
}

pub(crate) fn bench_three_way_merge(
    repo: &Repository,
    base_tree: &Tree,
    our_tree: &Tree,
    their_tree: &Tree,
) -> Result<(Tree, usize, usize, usize)> {
    let result = three_way_merge(repo, base_tree, our_tree, their_tree)?;
    Ok((
        result.tree,
        result.conflicts.len(),
        result.renames.len(),
        result.directory_renames.len(),
    ))
}

pub(crate) fn bench_detect_renames(
    store: &impl LocalObjectStore,
    base_tree: &Tree,
    branch_tree: &Tree,
) -> Result<(usize, rename_matcher::RenameMatcherStats)> {
    let detection = rename_matcher::detect_renames_with_stats(
        store,
        &rename_matcher::flatten_tree(store, base_tree, "")?,
        &rename_matcher::flatten_tree(store, branch_tree, "")?,
        rename_matcher::RenameMatcherConfig::default(),
    )?;
    Ok((detection.matches.len(), detection.stats))
}

fn fast_forward_renames(
    repo: &Repository,
    from: &ChangeId,
    to: &ChangeId,
) -> Result<(Vec<RenameEntry>, Vec<RenameEntry>)> {
    let from_tree = load_state_tree(repo, from)?;
    let to_tree = load_state_tree(repo, to)?;
    let from_flat = rename_matcher::flatten_tree(repo.store(), &from_tree, "")?;
    let to_flat = rename_matcher::flatten_tree(repo.store(), &to_tree, "")?;
    let matches = rename_matcher::detect_renames(
        repo.store(),
        &from_flat,
        &to_flat,
        rename_matcher::RenameMatcherConfig::default(),
    )?;

    let mut renames: Vec<RenameEntry> = matches
        .values()
        .map(|rename| RenameEntry {
            from: rename.from_path.clone(),
            to: rename.to_path.clone(),
            score: rename.score,
        })
        .collect();
    renames.sort_by(|left, right| left.from.cmp(&right.from).then(left.to.cmp(&right.to)));

    let mut directory_renames: Vec<RenameEntry> = rename_matcher::infer_directory_renames(&matches)
        .into_iter()
        .map(|(from, to)| RenameEntry {
            from,
            to,
            score: 1.0,
        })
        .collect();
    directory_renames
        .sort_by(|left, right| left.from.cmp(&right.from).then(left.to.cmp(&right.to)));

    Ok((renames, directory_renames))
}

fn load_state_tree(repo: &Repository, change_id: &ChangeId) -> Result<Tree> {
    let state = repo
        .store()
        .get_state(change_id)?
        .ok_or_else(|| anyhow!("State '{}' not found", change_id.short()))?;
    repo.store().get_tree(&state.tree)?.ok_or_else(|| {
        anyhow!(
            "State '{}' references missing tree {}",
            change_id.short(),
            state.tree
        )
    })
}

pub(crate) fn build_thread_preview_report(
    repo: &Repository,
    thread: &mut Thread,
    prefer_apply_recommendation: bool,
) -> Result<ThreadPreviewReport> {
    let mut graph = CommitGraphIndex::new(repo);
    // External callers (`heddle sync`, `heddle land`, `heddle ready`)
    // route through the same default merge strategy as `heddle merge`.
    // The merge command path can still opt out by passing an explicit
    // strategy to `_with_graph`.
    build_thread_preview_report_with_graph(
        repo,
        &mut graph,
        thread,
        prefer_apply_recommendation,
        merge_strategy_for(semantic_merge_enabled(false)),
        None,
    )
}

/// Caller-supplied override for the destination side of the preview's
/// 3-way merge. When `Some`, the inner preview MUST compute against
/// this `(label, change_id)` instead of `thread.target_thread`. Used by
/// `merge_thread_into_current` so the preview matches the actual merge
/// — `heddle merge A` from thread B merges A → B, but A's
/// `target_thread` is whatever A was created from (often `main`), so
/// without an override the inner report computes A → main and
/// contradicts the real outcome (heddle#144).
pub(crate) struct PreviewTarget<'a> {
    pub label: &'a str,
    pub change_id: ChangeId,
}

fn build_thread_preview_report_with_graph(
    repo: &Repository,
    graph: &mut CommitGraphIndex<'_>,
    thread: &mut Thread,
    prefer_apply_recommendation: bool,
    strategy: MergeStrategy,
    target_override: Option<PreviewTarget<'_>>,
) -> Result<ThreadPreviewReport> {
    refresh_thread_freshness(repo, thread)?;
    let mut conflicts = Vec::new();
    // Resolve the destination side. Prefer the caller's override (the
    // merge command supplies the actual current HEAD); otherwise fall
    // back to `thread.target_thread` for callers like `ready` / `sync` /
    // `land` that don't carry an explicit merge destination.
    let resolved_target: Option<(String, ChangeId)> = if let Some(ovr) = target_override {
        Some((ovr.label.to_string(), ovr.change_id))
    } else if let Some(name) = thread.target_thread.as_deref() {
        let id = repo
            .refs()
            .get_thread(&ThreadName::new(name))?
            .ok_or_else(|| anyhow!(thread_not_found_advice(name, "merge preview")))?;
        Some((name.to_string(), id))
    } else {
        None
    };

    let mut preview_changed_paths: Option<Vec<String>> = None;
    let merge_relation = if let Some((target_label, target_id)) = resolved_target {
        let thread_id = repo
            .refs()
            .get_thread(&ThreadName::new(&thread.thread))?
            .ok_or_else(|| anyhow!(thread_not_found_advice(&thread.thread, "merge preview")))?;
        let current_label = format!("CURRENT ({target_label})");
        let incoming_label = format!("INCOMING ({})", thread.thread);
        let merge_plan = MergePlan::for_thread_preview(
            repo,
            graph,
            &target_id,
            &thread_id,
            ConflictLabels {
                current: &current_label,
                incoming: &incoming_label,
                strategy,
            },
        )?;
        if let Some(merge_result) = merge_plan.merge_result() {
            conflicts = merge_result.conflicts.clone();
        }
        let merge_relation = merge_plan.relation().as_json_value().to_string();
        if merge_relation != "already_integrated" {
            preview_changed_paths = Some(merge_changed_paths(repo, &target_id, &thread_id)?);
        }
        merge_relation
    } else {
        "no_target".to_string()
    };

    let mut advice =
        describe_thread_advice(thread, false, conflicts.len(), prefer_apply_recommendation);
    if merge_relation == "already_integrated" {
        advice.blockers.clear();
        advice.recommended_action.clear();
        advice.thread_health = "clean".to_string();
    }

    let thread_tip = repo
        .refs()
        .get_thread(&ThreadName::new(&thread.thread))?
        .map(|id| id.short());
    let manual_resolution_current = thread
        .integration_policy_result
        .manual_resolution_state
        .as_deref()
        .zip(thread_tip.as_deref())
        .is_some_and(|(resolved, current)| resolved == current);
    let conflict_count = if manual_resolution_current {
        0
    } else {
        conflicts.len()
    };
    let conflicts = if manual_resolution_current {
        Vec::new()
    } else {
        conflicts
    };
    if manual_resolution_current {
        advice.blockers.clear();
        advice.recommended_action = land_command_for_thread(repo, &thread.id);
        advice.thread_health = "ready".to_string();
    }

    let recommended_action = advice.recommended_action;
    let all_changed_paths = preview_changed_paths.unwrap_or_else(|| thread.changed_paths.clone());
    let changed_path_count = all_changed_paths.len();
    let changed_paths = all_changed_paths.into_iter().take(8).collect();
    Ok(ThreadPreviewReport {
        thread: thread.id.clone(),
        thread_mode: thread.mode.to_string(),
        thread_state: thread.state.to_string(),
        freshness: thread.freshness.to_string(),
        task: thread.task.clone(),
        changed_paths,
        changed_path_count,
        impact_categories: thread
            .impact_categories
            .iter()
            .map(ToString::to_string)
            .collect(),
        heavy_impact_paths: thread.heavy_impact_paths.clone(),
        merge_relation,
        conflict_count,
        conflicts,
        blockers: advice.blockers,
        recommended_action_template: action_template(&recommended_action),
        recommended_action,
        thread_health: advice.thread_health,
    })
}

fn merge_output_from_report(input: MergeOutputInput<'_>) -> MergeOutput {
    let report_conflicts = input.conflicts.unwrap_or_default();
    let diff_changed_paths = input.diff.as_ref().map(diff_changed_paths);
    let changed_paths = if let Some(paths) = input.changed_paths {
        paths
    } else if let Some(thread) = input.thread.as_ref() {
        let paths = thread.changed_paths.clone();
        if paths.is_empty() {
            diff_changed_paths.unwrap_or(paths)
        } else {
            paths
        }
    } else {
        diff_changed_paths.unwrap_or_default()
    };
    let changed_path_count = changed_paths.len();
    // The preview-stage "blockers" list mixes two kinds of items:
    //   1) Real blockers — things that actually prevent the merge from
    //      advancing state (e.g. unresolved conflicts).
    //   2) Recommendations — non-blocking nudges like "promotion
    //      recommended for environment breadth". The merge can and
    //      does proceed when these are present; surfacing them as
    //      `blockers` while also setting `merge_state` produces the
    //      contradictory shape `status: "blocked"` + non-null
    //      `merge_state` + `thread_state: "merged"`.
    //
    // The schema rule is: `blockers` only when `status == "blocked"`
    // and the operation did NOT advance state. Everything else moves
    // to `warnings`.
    let preview_blockers = input
        .preview_report
        .map(|report| report.blockers.clone())
        .unwrap_or_default();
    let preview_warnings: Vec<String> = preview_blockers
        .iter()
        .filter(|item| !is_real_merge_blocker(item))
        .cloned()
        .collect();
    // The only "real" blocker in the merge flow is unresolved
    // conflicts. Stale/promotion/etc. are advisory.
    let mut real_blockers: Vec<String> = if report_conflicts.is_empty() {
        Vec::new()
    } else {
        vec![format!(
            "{} path conflict(s) need manual resolution",
            report_conflicts.len()
        )]
    };
    real_blockers.extend(input.extra_blockers.iter().cloned());

    let status = if !real_blockers.is_empty() {
        "blocked"
    } else {
        "completed"
    };
    let stale_refresh_action = input.preview_report.and_then(|report| {
        (report.freshness == ThreadFreshness::Stale.to_string()).then(|| {
            if report.recommended_action.trim().is_empty() {
                format!(
                    "heddle sync --thread {}",
                    recommended_action_quote(&report.thread)
                )
            } else {
                report.recommended_action.clone()
            }
        })
    });
    let recommended_action: Option<String> = if !report_conflicts.is_empty() {
        // Apply path with conflicts → tell the operator how to
        // resolve. Preview path with conflicts → no actionable
        // command (the operator must pick a strategy first).
        if input.preview_only {
            None
        } else {
            Some("heddle continue".to_string())
        }
    } else if !input.extra_blockers.is_empty() {
        // Coordination blocker. Two shapes:
        //   1. Pre-snapshot (`merge_state` is None): typical
        //      `--git-commit` precondition failure. Nothing landed; surface
        //      status rather than a self-loop back into this merge command.
        //   2. Post-snapshot (`merge_state` is Some): `git commit`
        //      itself failed AFTER heddle advanced. Re-running
        //      `heddle merge` would noop; the safe recovery is the
        //      shared checkpoint template, which records the landed
        //      Heddle state in Git after the checkout issue is fixed.
        Some(coordination_blocker_recommended_action(
            input.merge_state.as_ref(),
        ))
    } else if input.preview_only
        && input.message != "Already up to date"
        && stale_refresh_action.is_some()
    {
        stale_refresh_action
    } else if input.preview_only && input.message != "Already up to date" {
        // Clean preview: the actionable next step is the human landing
        // command. `land` keeps capture, merge, checkpoint, push, and
        // verification in one loop, so the preview does not bounce users back
        // to the lower-level merge apply command.
        if input
            .preview_report
            .is_some_and(preview_needs_readiness_review)
        {
            None
        } else {
            input.thread.as_ref().map(|t| land_local_command(&t.id))
        }
    } else {
        // Clean apply: nothing to do.
        None
    };
    let meaningful_merge = status == "completed" && input.message != "Already up to date";
    let would_merge = input.preview_only && meaningful_merge;
    let applied = !input.preview_only && meaningful_merge;
    MergeOutput {
        operator: OperatorCommandOutput {
            status: status.to_string(),
            action: OperatorAction::Merge,
            message: input.message,
            blockers: real_blockers,
            warnings: preview_warnings,
            next_action: recommended_action.clone(),
            recommended_action: recommended_action.clone(),
        },
        would_merge,
        applied,
        fast_forward: input.fast_forward,
        preview_only: input.preview_only,
        merge_state: input.merge_state,
        conflicts: report_conflicts.clone(),
        preview_summary: input.preview_summary,
        thread_state: input.thread.as_ref().map(|thread| thread.state.to_string()),
        freshness: input
            .thread
            .as_ref()
            .map(|thread| thread.freshness.to_string()),
        changed_paths,
        changed_path_count,
        impact_categories: thread_impacts(input.thread),
        promotion_suggested: input
            .thread
            .as_ref()
            .map(|thread| thread.promotion_suggested)
            .unwrap_or(false),
        heavy_impact_paths: thread_heavy_paths(input.thread),
        merge_relation: input.merge_relation.or_else(|| {
            input
                .preview_report
                .map(|report| report.merge_relation.clone())
        }),
        conflict_count: input
            .conflict_count
            .or_else(|| input.preview_report.map(|report| report.conflict_count))
            .unwrap_or(report_conflicts.len()),
        thread_health: merge_output_thread_health(input.thread.as_ref(), input.preview_report),
        renames: input.renames,
        directory_renames: input.directory_renames,
        semantic_changes: input.semantic_changes,
        diff: input.diff,
        git_commit_preview: input.git_commit_preview,
        git_commit: input.git_commit,
        trust: Some(merge_output_trust(
            input.repo,
            recommended_action.as_deref(),
        )),
    }
}

fn diff_changed_paths(diff: &DiffOutput) -> Vec<String> {
    diff.changes
        .iter()
        .map(|change| change.path.clone())
        .collect()
}

fn diff_with_known_renames(diff: DiffOutput, renames: &[RenameEntry]) -> DiffOutput {
    if renames.is_empty() {
        return diff;
    }
    let DiffOutput {
        from_state,
        to_state,
        changes: original_changes,
        semantic_changes,
        context,
        broader_guidance,
        ..
    } = diff;
    let rename_by_new = renames
        .iter()
        .map(|rename| (rename.to.as_str(), rename.from.as_str()))
        .collect::<std::collections::BTreeMap<_, _>>();
    let removed_old = renames
        .iter()
        .map(|rename| rename.from.as_str())
        .collect::<std::collections::BTreeSet<_>>();
    let mut changes = Vec::with_capacity(original_changes.len());
    for mut change in original_changes {
        if change.kind == "deleted" && removed_old.contains(change.path.as_str()) {
            continue;
        }
        if change.kind == "added"
            && let Some(old_path) = rename_by_new.get(change.path.as_str())
        {
            change.kind = "renamed".to_string();
            change.old_path = Some((*old_path).to_string());
        }
        changes.push(change);
    }
    DiffOutput::new(
        from_state,
        to_state,
        changes,
        semantic_changes,
        context,
        broader_guidance,
    )
}

fn merge_output_thread_health(
    thread: Option<&Thread>,
    preview_report: Option<&ThreadPreviewReport>,
) -> String {
    match thread.map(|thread| &thread.state) {
        Some(ThreadState::Merged | ThreadState::Abandoned) => "clean".to_string(),
        Some(ThreadState::Blocked) => "blocked".to_string(),
        Some(ThreadState::Ready) => "ready".to_string(),
        Some(ThreadState::Draft | ThreadState::Active | ThreadState::Promoted) | None => {
            preview_report
                .map(|report| report.thread_health.clone())
                .unwrap_or_else(|| "active".to_string())
        }
    }
}

fn coordination_blocker_recommended_action(merge_state: Option<&String>) -> String {
    if merge_state.is_some() {
        "heddle commit -m \"...\"".to_string()
    } else {
        "heddle status".to_string()
    }
}

fn merge_output_trust(
    repo: &Repository,
    recommended_action: Option<&str>,
) -> RepositoryVerificationState {
    let mut trust = build_repository_verification_state(repo);
    if let Some(action) = recommended_action {
        override_trust_recommended_action(&mut trust, action);
    }
    trust
}

fn source_thread_uncaptured_work(
    target_repo: &Repository,
    thread: &Thread,
) -> Result<Option<SourceThreadUncapturedWork>> {
    if thread.execution_path.as_os_str().is_empty()
        || thread.execution_path == *target_repo.root()
        || !thread.execution_path.exists()
        || !thread.execution_path.join(".heddle").exists()
    {
        return Ok(None);
    }

    let source_repo = Repository::open(&thread.execution_path)?;
    let options = worktree_status_options(Some(source_repo.config()));
    if !worktree_dirty(&source_repo, &options)? {
        return Ok(None);
    }

    Ok(Some(SourceThreadUncapturedWork {
        checkout_path: thread.execution_path.display().to_string(),
        dirty_paths: worktree_dirty_paths(&source_repo, &options)?,
    }))
}

fn source_thread_uncaptured_work_advice(
    thread: &Thread,
    uncaptured: SourceThreadUncapturedWork,
    preview_only: bool,
) -> RecoveryAdvice {
    let repo_arg = recommended_action_quote(&uncaptured.checkout_path);
    let action = format!("heddle --repo {repo_arg} ready -m \"Save source work\"");
    let capture = format!("heddle --repo {repo_arg} capture -m \"Save source work\"");
    let stash = format!("heddle --repo {repo_arg} stash push -m \"Save source work\"");
    let path_summary = uncaptured_path_summary(&uncaptured.dirty_paths);
    let error = if preview_only {
        format!(
            "Thread '{}' has uncaptured work in {} ({path_summary}); merge preview did not run",
            thread.id, uncaptured.checkout_path
        )
    } else {
        format!(
            "Thread '{}' has uncaptured work in {} ({path_summary}); merge did not run",
            thread.id, uncaptured.checkout_path
        )
    };
    RecoveryAdvice::safety_refusal(
        "source_thread_uncaptured_work",
        error,
        format!("Run `{action}` in the source checkout, then retry the merge."),
        format!(
            "source thread '{}' has {path_summary} in {}",
            thread.id, uncaptured.checkout_path
        ),
        "merging now would ignore live source checkout changes that have not been captured into Heddle",
        "target repository, source checkout, refs, and worktree files were left unchanged",
        action.clone(),
        vec![action, capture, stash],
    )
}

fn uncaptured_path_summary(paths: &[String]) -> String {
    if paths.is_empty() {
        return "uncaptured worktree paths".to_string();
    }
    let shown = paths
        .iter()
        .take(12)
        .cloned()
        .collect::<Vec<_>>()
        .join(", ");
    let overflow = paths.len().saturating_sub(12);
    if overflow == 0 {
        format!("uncaptured path(s): {shown}")
    } else {
        format!("uncaptured path(s): {shown}, and {overflow} more")
    }
}

fn recommended_action_quote(value: &str) -> String {
    let safe = !value.is_empty()
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'/' | b'.' | b'_' | b'-' | b'+'));
    if safe {
        value.to_string()
    } else {
        format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
    }
}

fn merge_blocked_by_trust_output(
    thread: &Option<Thread>,
    preview_report: Option<&ThreadPreviewReport>,
    trust: RepositoryVerificationState,
    preview_only: bool,
    merge_relation: Option<String>,
) -> MergeOutput {
    MergeOutput {
        operator: OperatorCommandOutput::blocked_by_repository_verification(
            OperatorAction::Merge,
            trust_blocked_merge_message(&trust, preview_only),
            &trust,
        ),
        would_merge: false,
        applied: false,
        fast_forward: false,
        preview_only,
        merge_state: None,
        conflicts: Vec::new(),
        preview_summary: Vec::new(),
        thread_state: thread.as_ref().map(|thread| thread.state.to_string()),
        freshness: thread.as_ref().map(|thread| thread.freshness.to_string()),
        changed_paths: thread_paths(thread),
        changed_path_count: thread_path_count(thread),
        impact_categories: thread_impacts(thread),
        promotion_suggested: thread
            .as_ref()
            .map(|thread| thread.promotion_suggested)
            .unwrap_or(false),
        heavy_impact_paths: thread_heavy_paths(thread),
        merge_relation: merge_relation
            .or_else(|| preview_report.map(|report| report.merge_relation.clone())),
        conflict_count: 0,
        thread_health: trust.status.clone(),
        renames: Vec::new(),
        directory_renames: Vec::new(),
        semantic_changes: None,
        diff: None,
        git_commit_preview: None,
        git_commit: None,
        trust: Some(trust),
    }
}

fn merge_freshness_preflight_output(
    repo: &Repository,
    thread: &Option<Thread>,
    preview_report: Option<&ThreadPreviewReport>,
    preview_only: bool,
) -> Option<MergeOutput> {
    if thread
        .as_ref()
        .is_some_and(|thread| thread.state == ThreadState::Merged)
    {
        return None;
    }
    let report =
        preview_report.filter(|report| report.freshness == ThreadFreshness::Stale.to_string())?;
    Some(stale_thread_merge_blocked_output(
        repo,
        thread,
        report,
        preview_only,
    ))
}

fn stale_thread_merge_blocked_output(
    repo: &Repository,
    thread: &Option<Thread>,
    preview_report: &ThreadPreviewReport,
    preview_only: bool,
) -> MergeOutput {
    let recommended_action = if preview_report.recommended_action.trim().is_empty() {
        format!(
            "heddle sync --thread {}",
            recommended_action_quote(&preview_report.thread)
        )
    } else {
        preview_report.recommended_action.clone()
    };
    let blockers = if preview_report.blockers.is_empty() {
        vec![format!(
            "Thread '{}' is stale against '{}'",
            preview_report.thread,
            thread
                .as_ref()
                .and_then(|thread| thread.target_thread.as_deref())
                .unwrap_or("its target thread")
        )]
    } else {
        preview_report.blockers.clone()
    };
    let conflict_suffix = if preview_report.conflict_count > 0 {
        format!(
            " and has {} path conflict(s)",
            preview_report.conflict_count
        )
    } else {
        String::new()
    };

    MergeOutput {
        operator: OperatorCommandOutput {
            status: "blocked".to_string(),
            action: OperatorAction::Merge,
            message: format!(
                "Thread '{}' is stale{}; merge {}did not run",
                preview_report.thread,
                conflict_suffix,
                if preview_only { "preview " } else { "" }
            ),
            blockers,
            warnings: Vec::new(),
            next_action: Some(recommended_action.clone()),
            recommended_action: Some(recommended_action.clone()),
        },
        would_merge: false,
        applied: false,
        fast_forward: false,
        preview_only,
        merge_state: None,
        conflicts: preview_report.conflicts.clone(),
        preview_summary: build_stale_preview_summary(preview_report),
        thread_state: thread.as_ref().map(|thread| thread.state.to_string()),
        freshness: Some(preview_report.freshness.clone()),
        changed_paths: preview_report.changed_paths.clone(),
        changed_path_count: preview_report.changed_path_count,
        impact_categories: preview_report.impact_categories.clone(),
        promotion_suggested: !preview_report.heavy_impact_paths.is_empty(),
        heavy_impact_paths: preview_report.heavy_impact_paths.clone(),
        merge_relation: Some(preview_report.merge_relation.clone()),
        conflict_count: preview_report.conflict_count,
        thread_health: "blocked".to_string(),
        renames: Vec::new(),
        directory_renames: Vec::new(),
        semantic_changes: None,
        diff: None,
        git_commit_preview: None,
        git_commit: None,
        trust: Some(merge_output_trust(repo, Some(&recommended_action))),
    }
}

fn trust_blocks_merge_preview(trust: &RepositoryVerificationState) -> bool {
    trust
        .checks
        .iter()
        .any(|check| !check.clean && matches!(check.name.as_str(), "Mapping" | "Operation"))
}

fn trust_blocked_merge_message(trust: &RepositoryVerificationState, preview_only: bool) -> String {
    if preview_only {
        format!(
            "Repository verification is blocked; merge preview did not run: {}",
            trust.summary
        )
    } else {
        format!(
            "Repository verification is blocked; merge did not run: {}",
            trust.summary
        )
    }
}

fn preview_did_not_run(output: &MergeOutput) -> bool {
    output.preview_only
        && output.operator.status == "blocked"
        && output.operator.message.contains("preview did not run")
}

fn merge_preview_blocked_advice(output: &MergeOutput) -> RecoveryAdvice {
    let primary_command = output
        .operator
        .recommended_action
        .as_deref()
        .or(output.operator.next_action.as_deref())
        .filter(|action| !action.trim().is_empty())
        .unwrap_or("heddle verify");
    let blockers = if output.operator.blockers.is_empty() {
        output.operator.message.clone()
    } else {
        output.operator.blockers.join("; ")
    };
    let mut advice = if let Some(trust) = output.trust.as_ref() {
        repository_verification_blocked_advice(
            "merge_preview_blocked",
            output.operator.message.clone(),
            "retrying the merge preview",
            trust,
            blockers,
            "the merge preview would otherwise describe a stale or unverifiable integration path",
            "repository state, refs, and worktree files were left unchanged",
            Some(primary_command.to_string()),
        )
    } else {
        RecoveryAdvice::safety_refusal(
            "merge_preview_blocked",
            output.operator.message.clone(),
            format!("Run `{primary_command}` before retrying the merge preview."),
            blockers,
            "the merge preview would otherwise describe a stale or unverifiable integration path",
            "repository state, refs, and worktree files were left unchanged",
            primary_command.to_string(),
            vec![primary_command.to_string()],
        )
    };
    if output.conflict_count > 0 {
        advice.extra_json_fields.insert(
            "conflict_count".to_string(),
            Value::Number(output.conflict_count.into()),
        );
        advice.extra_json_fields.insert(
            "conflicts".to_string(),
            Value::Array(
                output
                    .conflicts
                    .iter()
                    .cloned()
                    .map(Value::String)
                    .collect(),
            ),
        );
    }
    if let Some(merge_relation) = output.merge_relation.as_ref() {
        advice.extra_json_fields.insert(
            "merge_relation".to_string(),
            Value::String(merge_relation.clone()),
        );
    }
    advice
}

impl super::compact::CompactProjection for MergeOutput {
    fn compact(&self) -> super::compact::CompactOutput {
        let mut compact = self.operator.compact();
        compact.changed_paths = Some(self.changed_paths.clone());
        compact.changed_path_count = Some(self.changed_path_count);
        compact.conflicts = Some(self.conflicts.clone());
        compact.conflict_count = Some(self.conflict_count);
        compact
    }
}

fn render_merge_output(cli: &Cli, repo: &Repository, output: MergeOutput) -> Result<()> {
    if should_output_json(cli, None) {
        write_command_json(
            &output,
            output_is_compact(cli),
            NextActionValidationContext::new(&["merge"], repo.capability()),
        )?;
    } else {
        // Fast-forward is the happy-path success message; colour the
        // verb (`Fast-forwarded`) accent-green and dim the change-id
        // target. We re-derive the styling here rather than mutating
        // `output.message` so JSON consumers still receive the
        // unstyled string. Other outcomes (preview, conflict messages)
        // stay plain — they're informational, not status. The two
        // prefixes (`Fast-forwarded to` for detached HEAD,
        // `Fast-forwarded <thread> to` for attached HEAD) are both
        // recognised so the styled rendering still leads with the
        // thread name when one is available.
        if output.fast_forward
            && let Some(rest) = output.operator.message.strip_prefix("Fast-forwarded ")
        {
            println!("{} {}", style::accent("Fast-forwarded"), style::dim(rest));
        } else if output.fast_forward
            && let Some(rest) = output.operator.message.strip_prefix("Would fast-forward ")
        {
            println!("{} {}", style::warn("Would fast-forward"), style::dim(rest));
        } else {
            println!("{}", output.operator.message);
        }
        for line in &output.preview_summary {
            println!("  {}", line);
        }
        if !output.conflicts.is_empty() {
            for conflict in &output.conflicts {
                // C-prefixed conflict line: the `C` carries the
                // signal, so it's the only saturated character.
                println!("  {} {}", style::error("C"), conflict);
            }
        }
        for rename in &output.renames {
            println!(
                "  {} {} → {} ({:.0}%)",
                style::accent("R"),
                rename.from,
                rename.to,
                rename.score * 100.0
            );
        }
        if let Some(diff) = &output.diff {
            println!();
            crate::cli::commands::diff::print_stat(diff);
            crate::cli::commands::diff::print_diff(diff);
        }
        if let Some(git_commit) = &output.git_commit {
            let display_len = std::cmp::min(12, git_commit.sha.len());
            println!(
                "  git commit: {}",
                style::dim(&git_commit.sha[..display_len])
            );
        }
        if let Some(next) = output
            .operator
            .recommended_action
            .as_ref()
            .or(output.operator.next_action.as_ref())
        {
            print_nested_next(next);
        }
    }
    Ok(())
}

/// Classifies an advisory string from `describe_thread_advice` as a
/// real merge blocker (something that prevents `heddle merge` from
/// advancing state) versus a non-blocking nudge.
///
/// Real blockers are conflict-shaped strings ("path conflict(s) need
/// manual resolution") or explicit policy gates such as heavy-impact
/// review. Lower-confidence review nudges remain advisory; treating
/// every "needs attention" string as a blocker causes the "merge
/// succeeded but status:blocked" contradiction that this helper exists
/// to prevent.
fn preview_needs_readiness_review(report: &ThreadPreviewReport) -> bool {
    report.thread_state != ThreadState::Ready.to_string() && !report.heavy_impact_paths.is_empty()
}

fn is_real_merge_blocker(advisory: &str) -> bool {
    let lower = advisory.to_lowercase();
    lower.contains("path conflict") || lower.contains("heavy-impact change")
}

fn thread_paths(thread: &Option<Thread>) -> Vec<String> {
    thread
        .as_ref()
        .map(|thread| thread.changed_paths.clone())
        .unwrap_or_default()
}

fn thread_path_count(thread: &Option<Thread>) -> usize {
    thread
        .as_ref()
        .map(|thread| thread.changed_paths.len())
        .unwrap_or(0)
}

fn thread_impacts(thread: &Option<Thread>) -> Vec<String> {
    thread
        .as_ref()
        .map(|thread| {
            thread
                .impact_categories
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default()
}

fn thread_heavy_paths(thread: &Option<Thread>) -> Vec<String> {
    thread
        .as_ref()
        .map(|thread| thread.heavy_impact_paths.clone())
        .unwrap_or_default()
}

fn build_preview_summary(report: Option<&ThreadPreviewReport>) -> Vec<String> {
    let mut lines = Vec::new();
    if let Some(report) = report {
        let real_blockers = report
            .blockers
            .iter()
            .filter(|blocker| is_real_merge_blocker(blocker))
            .cloned()
            .collect::<Vec<_>>();
        if !real_blockers.is_empty() {
            lines.push(format!("blocked: {}", real_blockers.join("; ")));
        }
        lines.push(format!(
            "checkout: {}",
            thread_mode_summary(&report.thread_mode)
        ));
        lines.push(format!("sync: {}", report.freshness));
        if let Some(task) = &report.task {
            lines.push(format!("task: {}", task));
        }
        if !report.changed_paths.is_empty() {
            lines.push(format!(
                "changed paths: {}",
                report.changed_paths.join(", ")
            ));
        }
        if !report.impact_categories.is_empty() {
            lines.push(format!(
                "impact categories: {}",
                report.impact_categories.join(", ")
            ));
        }
        if !report.heavy_impact_paths.is_empty() {
            lines.push(format!(
                "heavy-impact change: {} — review broader impact before merging",
                crate::cli::render::preview_list(
                    &report.heavy_impact_paths,
                    report.heavy_impact_paths.len(),
                )
            ));
        }
        lines.push(format!(
            "merge type: {}",
            merge_relation_summary(&report.merge_relation)
        ));
        if report.conflict_count > 0 {
            lines.push(format!(
                "conflicts: {} path conflict(s)",
                report.conflict_count
            ));
        }
    }
    lines
}

fn build_stale_preview_summary(report: &ThreadPreviewReport) -> Vec<String> {
    let mut lines = Vec::new();
    if !report.blockers.is_empty() {
        lines.push(format!("blocked: {}", report.blockers.join("; ")));
    }
    lines.push(format!(
        "checkout: {}",
        thread_mode_summary(&report.thread_mode)
    ));
    lines.push(format!("sync: {}", report.freshness));
    if let Some(task) = &report.task {
        lines.push(format!("task: {}", task));
    }
    if !report.changed_paths.is_empty() {
        lines.push(format!(
            "changed paths: {}",
            report.changed_paths.join(", ")
        ));
    }
    if !report.impact_categories.is_empty() {
        lines.push(format!(
            "impact categories: {}",
            report.impact_categories.join(", ")
        ));
    }
    if !report.heavy_impact_paths.is_empty() {
        lines.push(format!(
            "heavy-impact change: {} — review broader impact before merging",
            crate::cli::render::preview_list(
                &report.heavy_impact_paths,
                report.heavy_impact_paths.len(),
            )
        ));
    }
    lines.push(format!(
        "merge type: {}",
        merge_relation_summary(&report.merge_relation)
    ));
    if report.conflict_count > 0 {
        lines.push(format!(
            "conflicts: {} path conflict(s)",
            report.conflict_count
        ));
    }
    lines
}

fn thread_mode_summary(mode: &str) -> &str {
    match mode {
        "solid" => "main checkout",
        "materialized" => "disk checkout",
        "virtualized" => "virtual checkout",
        other => other,
    }
}

fn merge_relation_summary(result: &str) -> String {
    result.replace('_', "-")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_in_progress_refusal_uses_typed_recovery_advice() {
        let advice = merge_already_in_progress_advice();

        assert_eq!(advice.kind, "merge_already_in_progress");
        assert_eq!(advice.primary_command, "heddle status");
        assert!(advice.error.contains("merge is already in progress"));
        assert!(advice.primary_hint().contains("heddle continue"));
        assert!(
            advice
                .recovery_commands
                .contains(&"heddle resolve --abort".to_string())
        );
        assert!(advice.preserved.contains("left unchanged"));
    }

    /// Empty directory case: `prepare_dir_for_file_replacement` removes
    /// it so the materializer can write a regular file at the same path.
    /// Without this step, `materialize_blob` blows up deep in the
    /// materializer with a "Is a directory" I/O error.
    #[test]
    fn prepare_dir_for_file_replacement_removes_empty_directory() {
        let dir = tempfile::TempDir::new().unwrap();
        let target = dir.path().join("entry");
        fs::create_dir(&target).unwrap();

        prepare_dir_for_file_replacement(&target).expect("empty dir is removable");

        assert!(
            !target.exists(),
            "empty directory must be removed so a file can take its place"
        );
    }

    /// Non-empty directory case (heddle-ignored content remains): the
    /// helper must error with an actionable message naming the offending
    /// content. Silently deleting heddle-ignored content to make a
    /// type-change land would defeat the entire reason
    /// `remove_tracked_descendants` exists.
    #[test]
    fn prepare_dir_for_file_replacement_errors_on_non_empty_directory() {
        let dir = tempfile::TempDir::new().unwrap();
        let target = dir.path().join("entry");
        fs::create_dir(&target).unwrap();
        // Simulate heddle-ignored content (e.g. `target/`, `node_modules/`)
        // that `remove_tracked_descendants_with_source` left in place
        // because it isn't in the source tree.
        fs::create_dir(target.join("node_modules")).unwrap();
        fs::write(target.join("node_modules").join("dep.js"), "ignored").unwrap();

        let err = prepare_dir_for_file_replacement(&target)
            .expect_err("non-empty dir must error rather than silently delete");
        let msg = err.to_string();
        assert!(
            msg.contains("cannot replace directory"),
            "missing 'cannot replace directory' phrase: {msg}"
        );
        assert!(
            msg.contains("heddle-ignored content"),
            "missing 'heddle-ignored content' phrase: {msg}"
        );
        assert!(
            msg.contains("node_modules"),
            "error must list the offending entry: {msg}"
        );
        // Content must survive the failed call — the helper is
        // load-bearing precisely because it does NOT touch ignored
        // content.
        assert!(
            target.join("node_modules").join("dep.js").exists(),
            "ignored content must NOT be deleted by the failure path"
        );
    }

    /// Missing-path case: a NotFound error is harmless — the path is
    /// already gone, so the materializer can write the new file freely.
    #[test]
    fn prepare_dir_for_file_replacement_tolerates_missing_path() {
        let dir = tempfile::TempDir::new().unwrap();
        let target = dir.path().join("entry");
        // Don't create it.

        prepare_dir_for_file_replacement(&target).expect("missing dir is a no-op, not an error");
    }

    /// `empty_diff_output` is the schema-honest payload for return paths
    /// where heddle didn't actually advance state (already-up-to-date,
    /// conflicted, pre-snapshot blocked). The shape must round-trip as
    /// JSON cleanly: both `from_state` and `to_state` are populated with
    /// the same change-id and `changes` is an empty array.
    #[test]
    fn extended_validation_does_not_require_git_cli_identity() {
        use std::process::Command;

        let dir = tempfile::TempDir::new().unwrap();
        // Initialize a git repo with no user.name.
        let init_status = Command::new("git")
            .arg("-C")
            .arg(dir.path())
            .args(["init", "--quiet"])
            .status();
        let Ok(status) = init_status else {
            eprintln!("git not on PATH — skipping");
            return;
        };
        if !status.success() {
            return;
        }
        let blockers =
            validate_git_commit_preconditions_extended(dir.path(), &["dummy.txt".to_string()]);
        assert!(
            blockers.is_empty(),
            "native Git commit writing should not require a Git CLI/config identity; Heddle can author from captured principal: {blockers:?}"
        );
    }

    /// Empty merge-paths case: `write_git_commit` errors with "merge
    /// produced no changed paths" inside `git_commit.rs`, which only
    /// surfaces AFTER `snapshot_merge_with_attribution` has advanced
    /// heddle. The up-front check catches it before snapshot.
    #[test]
    fn extended_validation_flags_empty_changed_paths() {
        let dir = tempfile::TempDir::new().unwrap();
        let blockers = validate_git_commit_preconditions_extended(dir.path(), &[]);
        assert!(
            blockers
                .iter()
                .any(|b| b.contains("merge produced no changed paths")),
            "empty merge_paths must surface as a blocker: {blockers:?}"
        );
    }

    /// Negative case: when the directory isn't a git repo, the
    /// extended check returns early without spurious identity blockers
    /// (the existing `validate_git_state` reports the "no git
    /// repository" blocker; the extended check shouldn't double-report).
    #[test]
    fn extended_validation_skips_identity_check_when_no_git_dir() {
        let dir = tempfile::TempDir::new().unwrap();
        let blockers = validate_git_commit_preconditions_extended(dir.path(), &["a".to_string()]);
        // Only the `merge_paths.is_empty()` check fires before the
        // `.git` short-circuit; with non-empty paths it should be
        // empty (the absent-`.git` check is `validate_git_state`'s
        // job).
        assert!(
            !blockers.iter().any(|b| b.contains("git user.name")),
            "must not report identity blockers without a git overlay: {blockers:?}"
        );
        assert!(
            !blockers.iter().any(|b| b.contains("git user.email")),
            "must not report identity blockers without a git overlay: {blockers:?}"
        );
    }

    #[test]
    fn coordination_blocker_recommendations_are_machine_actions() {
        let merge_state = "hd-landed123".to_string();
        let post_snapshot = coordination_blocker_recommended_action(Some(&merge_state));
        assert_eq!(post_snapshot, "heddle commit -m \"...\"");
        assert!(
            action_template(&post_snapshot).is_some(),
            "commit placeholder should carry a fillable template"
        );

        let pre_snapshot = coordination_blocker_recommended_action(None);
        assert_eq!(pre_snapshot, "heddle status");
        assert!(
            action_template(&pre_snapshot).is_some(),
            "status action should carry a template"
        );
        for action in [post_snapshot, pre_snapshot] {
            assert!(
                !action.contains("resolve git state")
                    && !action.contains("see blockers")
                    && !action.contains("do NOT"),
                "recommended actions must be Heddle commands/templates, not prose: {action}"
            );
        }
    }

    #[test]
    fn empty_diff_output_is_self_consistent_and_serializes() {
        let id = objects::object::ChangeId::generate();
        let out = empty_diff_output(&id);

        assert_eq!(out.from_state.as_deref(), Some(id.short()).as_deref());
        assert_eq!(out.to_state.as_deref(), Some(id.short()).as_deref());
        assert!(
            out.changes.is_empty(),
            "empty_diff_output must report no changes — that's the whole point"
        );
        assert!(out.semantic_changes.is_none());

        let json = serde_json::to_value(&out).unwrap();
        assert_eq!(
            json["changes"].as_array().unwrap().len(),
            0,
            "`changes` array must serialize as empty, not be omitted"
        );
        assert_eq!(
            json["from_state"], json["to_state"],
            "self-loop semantics: from == to when no change landed"
        );
    }
}
