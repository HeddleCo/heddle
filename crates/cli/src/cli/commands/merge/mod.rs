// SPDX-License-Identifier: Apache-2.0
//! Merge command implementation.

use std::{fs, path::Path};

use anyhow::{Result, anyhow};
use objects::{
    object::{Attribution, ChangeId, Tree},
    store::ObjectStore,
};
use refs::Head;
use repo::{
    AgentRegistry, AgentStatus, Repository, Thread, ThreadFreshness, ThreadManager, ThreadState,
    describe_thread_advice,
};
use serde::Serialize;

use super::{
    diff::{DiffOutput, compute_state_diff},
    operator_core::OperatorCommandOutput,
    snapshot::ensure_current_state,
    thread_cmd::refresh_thread_freshness,
};
use crate::{
    cli::{Cli, should_output_json, style, worktree_status_options},
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
    pub semantic_result: String,
    pub conflicts: Vec<String>,
    pub conflict_count: usize,
    pub blockers: Vec<String>,
    pub recommended_action: String,
    pub thread_health: String,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct MergeOutput {
    #[serde(flatten)]
    pub operator: OperatorCommandOutput,
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
    pub semantic_result: Option<String>,
    pub conflict_count: usize,
    pub thread_health: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    renames: Vec<RenameEntry>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    directory_renames: Vec<RenameEntry>,
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
}

struct MergeOutputInput<'a> {
    thread: &'a Option<Thread>,
    preview_report: Option<&'a ThreadPreviewReport>,
    conflicts: Option<Vec<String>>,
    semantic_result: Option<String>,
    conflict_count: Option<usize>,
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
}

#[allow(clippy::too_many_arguments)]
pub fn cmd_merge(
    cli: &Cli,
    track_name: String,
    message: Option<String>,
    no_commit: bool,
    preview: bool,
    with_diff: bool,
    semantic: bool,
    git_commit: bool,
) -> Result<()> {
    let cwd_repo = Repository::open(cli.repo.as_ref().unwrap_or(&std::env::current_dir()?))?;
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
        anyhow::bail!("pre_merge hook vetoed: {}", resp.abort);
    }

    let output = merge_thread_into_current(
        &repo,
        &track_name,
        message,
        no_commit,
        preview,
        with_diff,
        semantic,
        git_commit,
    )?;

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

    emit_merge_output(cli, output)
}

/// Resolve current thread name for hook payloads. Returns `""` when
/// HEAD is detached.
fn current_thread_name(repo: &Repository) -> String {
    use refs::Head;
    match repo.head_ref() {
        Ok(Head::Attached { thread }) => thread,
        _ => String::new(),
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn merge_thread_into_current(
    repo: &Repository,
    track_name: &str,
    message: Option<String>,
    no_commit: bool,
    preview: bool,
    with_diff: bool,
    semantic: bool,
    git_commit: bool,
) -> Result<MergeOutput> {
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
        return Err(anyhow!(
            "A merge is already in progress. Resolve conflicts or use 'heddle resolve --abort'."
        ));
    }

    let merge_target_id = repo
        .refs()
        .get_thread(track_name)?
        .ok_or_else(|| anyhow!("Thread '{}' not found", track_name))?;

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

    let current_tree = repo
        .store()
        .get_tree(&current_state.tree)?
        .ok_or_else(|| anyhow!("Current tree not found"))?;

    let status = repo.compare_worktree_cached_with_options(
        &current_tree,
        &worktree_status_options(Some(repo.config())),
    )?;
    if !status.is_clean() {
        return Err(anyhow!(
            "Cannot merge: your worktree has uncaptured changes.\nCapture them with `heddle capture`, or finish the current operation with `heddle continue` / `heddle abort` before merging."
        ));
    }

    let mut graph = CommitGraphIndex::new(repo);
    // Codex r13 P2: the preview report's content-merge strategy must
    // match the strategy the actual merge plan (below) will use, so
    // the `preview_summary` lines don't contradict the real outcome
    // (e.g. reporting `conflicts: 1 path conflict(s)` on a structural
    // reshape that semantic resolves cleanly).
    let preview_strategy = if semantic {
        merge_algo::MergeStrategy::Semantic
    } else {
        merge_algo::MergeStrategy::HunkOnly
    };
    let preview_report = match thread.as_mut() {
        Some(thread) => Some(build_thread_preview_report_with_graph(
            repo,
            &mut graph,
            thread,
            preview,
            preview_strategy,
        )?),
        None => None,
    };
    let preview_summary = build_preview_summary(preview_report.as_ref());
    let current_thread = repo
        .current_lane()?
        .unwrap_or_else(|| "detached".to_string());
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
            strategy: if semantic {
                merge_algo::MergeStrategy::Semantic
            } else {
                merge_algo::MergeStrategy::HunkOnly
            },
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
        Ok(Some(compute_state_diff(repo, from, to, semantic, 3)?))
    };

    if merge_plan.relation().kind() == MergeRelationKind::AlreadyUpToDate {
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
            thread: &thread,
            preview_report: preview_report.as_ref(),
            conflicts: Some(vec![]),
            semantic_result: Some(merge_plan.relation().semantic_result().to_string()),
            conflict_count: Some(0),
            preview_summary: vec![],
            message: "Already up to date".to_string(),
            renames: vec![],
            directory_renames: vec![],
            merge_state: None,
            fast_forward: false,
            preview_only: preview,
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
        let ff_paths: Vec<String> = if git_commit {
            merge_changed_paths(repo, &current_state.change_id, &merge_target_id)?
        } else {
            thread_paths(&thread)
        };

        // FF: current..target IS the change set that lands. Compute once
        // and reuse for any per-branch return below.
        let ff_diff = diff_for(&current_state.change_id, &merge_target_id)?;

        // Pre-flight `--git-commit` validation (real merge only). On
        // preview we skip the dirty-tree check — the operator hasn't
        // committed to landing anything yet, just wants to see the
        // would-be commit message.
        let mut git_commit_blockers: Vec<String> = Vec::new();
        if git_commit
            && !preview
            && let Err(blocked) = git_commit::validate_git_state(repo.root(), &ff_paths)
        {
            git_commit_blockers = blocked.blockers;
        }

        if !git_commit_blockers.is_empty() {
            // Fail loudly *before* advancing heddle state.
            return Ok(merge_output_from_report(MergeOutputInput {
                thread: &thread,
                preview_report: preview_report.as_ref(),
                conflicts: Some(vec![]),
                semantic_result: Some("fast_forward".to_string()),
                conflict_count: Some(0),
                preview_summary,
                message: "Fast-forward blocked: --git-commit precondition failed".to_string(),
                renames: vec![],
                directory_renames: vec![],
                merge_state: None,
                fast_forward: false,
                preview_only: preview,
                diff: ff_diff,
                git_commit_preview: None,
                git_commit: None,
                extra_blockers: git_commit_blockers,
            }));
        }

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
            // and then explicitly record `OpRecord::FastForwardV2` so
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
                        track_name,
                        target_thread,
                        &current_state.change_id,
                        &merge_target_id,
                        Some(&repo.op_scope()),
                    )?;
                }
                Head::Detached { state } => {
                    // No attached thread to restore on undo. The generic
                    // `Goto` inverse is sufficient — preserve historic
                    // behavior for detached HEAD.
                    repo.oplog().record_goto(
                        &merge_target_id,
                        Some(state),
                        Some(&repo.op_scope()),
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
                let ff_message = preview_merge_message(&message, thread.as_ref(), track_name);
                let commit_message = git_commit::build_commit_message(
                    &ff_message,
                    &merge_target_id.short(),
                    &attribution,
                );
                git_commit_info = Some(git_commit::write_git_commit(
                    repo.root(),
                    &ff_paths,
                    &commit_message,
                )?);
            }
        } else if git_commit {
            // Preview path: render the would-be commit message.
            let attribution = Attribution::human(repo.get_principal()?);
            let ff_message = preview_merge_message(&message, thread.as_ref(), track_name);
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

        return Ok(MergeOutput {
            operator: OperatorCommandOutput {
                status: if preview { "preview" } else { "completed" }.to_string(),
                action: "merge".to_string(),
                message: match (preview, repo.head_ref()?) {
                    (true, Head::Attached { thread }) => {
                        format!(
                            "Would fast-forward {} to {}",
                            thread,
                            merge_target_id.short()
                        )
                    }
                    (true, Head::Detached { .. }) => {
                        format!("Would fast-forward to {}", merge_target_id.short())
                    }
                    (false, Head::Attached { thread }) => {
                        format!("Fast-forwarded {} to {}", thread, merge_target_id.short())
                    }
                    (false, Head::Detached { .. }) => {
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
                next_action: if preview {
                    Some(format!("heddle merge {}", track_name))
                } else {
                    None
                },
                recommended_action: if preview {
                    Some(format!("heddle merge {}", track_name))
                } else {
                    None
                },
            },
            fast_forward: true,
            preview_only: preview,
            merge_state: (!preview).then(|| merge_target_id.short()),
            conflicts: vec![],
            preview_summary,
            thread_state: thread.as_ref().map(|thread| thread.state.to_string()),
            freshness: thread.as_ref().map(|thread| thread.freshness.to_string()),
            changed_paths: thread_paths(&thread),
            changed_path_count: thread_path_count(&thread),
            impact_categories: thread_impacts(&thread),
            promotion_suggested: thread
                .as_ref()
                .map(|thread| thread.promotion_suggested)
                .unwrap_or(false),
            heavy_impact_paths: thread_heavy_paths(&thread),
            semantic_result: Some("fast_forward".to_string()),
            conflict_count: 0,
            thread_health: preview_report
                .as_ref()
                .map(|r| r.thread_health.clone())
                .unwrap_or_else(|| "clean".to_string()),
            renames: vec![],
            directory_renames: vec![],
            diff: ff_diff,
            git_commit_preview: git_commit_preview_payload,
            git_commit: git_commit_info,
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
            let preview_message = preview_merge_message(&message, thread.as_ref(), track_name);
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
        // 3-way preview diff: best-effort approximation. The actual
        // merge hasn't been computed against the worktree yet, so we
        // can't return the precise change set that would land. Use
        // `current..merge_target` as the closest pre-merge proxy and
        // document the caveat — the same payload the apply path used
        // pre-fix, but now confined to the preview surface where it's
        // legitimate.
        let preview_diff = diff_for(&current_state.change_id, &merge_target_id)?;
        return Ok(merge_output_from_report(MergeOutputInput {
            thread: &thread,
            preview_report: preview_report.as_ref(),
            conflicts: Some(merge_result.conflicts.clone()),
            semantic_result: Some(merge_plan.relation().semantic_result().to_string()),
            conflict_count: Some(merge_plan.relation().conflict_count()),
            preview_summary,
            message: "Preview complete".to_string(),
            renames: rename_entries.clone(),
            directory_renames: dir_rename_entries.clone(),
            merge_state: None,
            fast_forward: false,
            preview_only: true,
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
            thread: &thread,
            preview_report: preview_report.as_ref(),
            conflicts: Some(merge_result.conflicts.clone()),
            semantic_result: Some(merge_plan.relation().semantic_result().to_string()),
            conflict_count: Some(merge_plan.relation().conflict_count()),
            preview_summary,
            message: "Merged with conflicts".to_string(),
            renames: rename_entries,
            directory_renames: dir_rename_entries,
            merge_state: None,
            fast_forward: false,
            preview_only: false,
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
        let no_commit_diff: Option<DiffOutput> = None;
        return Ok(merge_output_from_report(MergeOutputInput {
            thread: &thread,
            preview_report: preview_report.as_ref(),
            conflicts: Some(vec![]),
            semantic_result: Some(merge_plan.relation().semantic_result().to_string()),
            conflict_count: Some(merge_plan.relation().conflict_count()),
            preview_summary,
            message: "Merge applied (not committed)".to_string(),
            renames: rename_entries,
            directory_renames: dir_rename_entries,
            merge_state: None,
            fast_forward: false,
            preview_only: false,
            diff: no_commit_diff,
            git_commit_preview: None,
            git_commit: None,
            extra_blockers: Vec::new(),
        }));
    }

    let merge_message = message.unwrap_or_else(|| {
        thread
            .as_ref()
            .and_then(|thread| thread.task.clone())
            .map(|task| format!("Merge thread '{}' ({task})", track_name))
            .unwrap_or_else(|| format!("Merge thread '{}'", track_name))
    });

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
        if let Err(blocked) = git_commit::validate_git_state(repo.root(), &merge_paths) {
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
            thread: &thread,
            preview_report: preview_report.as_ref(),
            conflicts: Some(vec![]),
            semantic_result: Some(merge_plan.relation().semantic_result().to_string()),
            conflict_count: Some(merge_plan.relation().conflict_count()),
            preview_summary,
            message: "Merge blocked: git --git-commit precondition failed".to_string(),
            renames: rename_entries,
            directory_renames: dir_rename_entries,
            merge_state: None,
            fast_forward: false,
            preview_only: false,
            diff: blocked_diff,
            git_commit_preview: None,
            git_commit: None,
            extra_blockers: git_commit_blockers,
        }));
    }

    let new_state = repo.snapshot_merge_with_attribution(
        &merge_target_id,
        Some(merge_message.clone()),
        None,
        attribution.clone(),
        Some(merge_base_id),
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
        match git_commit::write_git_commit(repo.root(), &merge_paths, &commit_message) {
            Ok(info) => git_commit_info = Some(info),
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
                    "recovery: heddle merge state {} is intact; resolve the git issue \
                     (hooks, identity, locks) and re-run `git add -- {}` then \
                     `git commit -m '<message>'` — do NOT re-run `heddle merge`",
                    new_state.change_id.short(),
                    merge_paths.join(" ")
                ));
            }
        }
    }

    // 3-way committed merge: `new_state` is the actual landed state.
    // Compute the diff from current → new_state so the JSON describes
    // the change set the user can audit, NOT `current..merge_target`
    // which can include removals of current-branch edits the merge
    // preserved.
    let committed_diff = diff_for(&current_state.change_id, &new_state.change_id)?;

    let final_message = if post_snapshot_git_blockers.is_empty() {
        format!("Merged as {}", new_state.change_id.short())
    } else {
        format!(
            "Merged as {} (heddle); git commit failed",
            new_state.change_id.short()
        )
    };

    Ok(merge_output_from_report(MergeOutputInput {
        thread: &thread,
        preview_report: preview_report.as_ref(),
        conflicts: Some(vec![]),
        semantic_result: Some(merge_plan.relation().semantic_result().to_string()),
        conflict_count: Some(merge_plan.relation().conflict_count()),
        preview_summary,
        message: final_message,
        renames: rename_entries,
        directory_renames: dir_rename_entries,
        merge_state: Some(new_state.change_id.short()),
        fast_forward: false,
        preview_only: false,
        diff: committed_diff,
        git_commit_preview: None,
        git_commit: git_commit_info,
        extra_blockers: post_snapshot_git_blockers,
    }))
}

/// Build a stand-in commit message for `--git-commit --preview` output.
/// Mirrors the real-mode logic in the apply path but doesn't allocate
/// a heddle merge state — used only for the preview surface.
fn preview_merge_message(
    explicit: &Option<String>,
    thread: Option<&Thread>,
    track_name: &str,
) -> String {
    if let Some(msg) = explicit.as_ref() {
        return msg.clone();
    }
    thread
        .and_then(|thread| thread.task.clone())
        .map(|task| format!("Merge thread '{}' ({task})", track_name))
        .unwrap_or_else(|| format!("Merge thread '{}'", track_name))
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

/// Extended pre-flight for `--git-commit`. Catches dry-runnable failure
/// modes that `validate_git_state` doesn't cover, so they surface as
/// pre-snapshot blockers rather than post-snapshot panics that leave
/// heddle advanced while git is uncommitted:
///
/// - **Missing commit identity.** `git commit` without
///   `user.name`/`user.email` errors out with a multi-line stderr that
///   the operator only sees AFTER heddle has merged. We probe both with
///   `git config --get` and report concretely.
/// - **Empty changed-paths set.** `write_git_commit` errors when the
///   merge produced no paths to commit (`refusing to write an empty
///   git commit`); detect that pre-snapshot.
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
    use std::process::Command;

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

    let probe = |key: &str| -> Option<String> {
        Command::new("git")
            .arg("-C")
            .arg(repo_root)
            .args(["config", "--get", key])
            .output()
            .ok()
            .filter(|out| out.status.success())
            .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
            .filter(|v| !v.is_empty())
    };

    if probe("user.name").is_none() {
        blockers.push(
            "git user.name is not configured (set with `git config user.name \"...\"` \
             before re-running --git-commit)"
                .to_string(),
        );
    }
    if probe("user.email").is_none() {
        blockers.push(
            "git user.email is not configured (set with `git config user.email \"...\"` \
             before re-running --git-commit)"
                .to_string(),
        );
    }

    blockers
}

/// Empty `DiffOutput` keyed at the given change-id. Used for return paths
/// that didn't actually advance state (already-up-to-date, conflicted,
/// pre-snapshot blocked) so the JSON honestly reports "no change set
/// landed" instead of pointing at an arbitrary parent..target diff.
fn empty_diff_output(state_id: &ChangeId) -> DiffOutput {
    DiffOutput {
        from_state: Some(state_id.short()),
        to_state: Some(state_id.short()),
        changes: Vec::new(),
        semantic_changes: None,
        context: None,
        broader_guidance: None,
    }
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
    /// Conflicts exist. `paths` lists the conflicting path strings.
    Conflicted {
        paths: Vec<String>,
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
            Ok(ThreeWayMergeOutcome::Conflicted {
                paths: merge_result.conflicts.clone(),
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
    store: &dyn ObjectStore,
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

pub(crate) fn build_thread_preview_report(
    repo: &Repository,
    thread: &mut Thread,
    prefer_apply_recommendation: bool,
) -> Result<ThreadPreviewReport> {
    let mut graph = CommitGraphIndex::new(repo);
    // External callers (`heddle sync`, `heddle ship`, `heddle ready`)
    // don't have a `--semantic` flag today; preserve the historic
    // hunk-only preview behaviour. The merge command path threads its
    // own strategy by calling `_with_graph` directly.
    build_thread_preview_report_with_graph(
        repo,
        &mut graph,
        thread,
        prefer_apply_recommendation,
        merge_algo::MergeStrategy::HunkOnly,
    )
}

fn build_thread_preview_report_with_graph(
    repo: &Repository,
    graph: &mut CommitGraphIndex<'_>,
    thread: &mut Thread,
    prefer_apply_recommendation: bool,
    strategy: MergeStrategy,
) -> Result<ThreadPreviewReport> {
    refresh_thread_freshness(repo, thread)?;
    let mut conflicts = Vec::new();
    let semantic_result = if let Some(target_thread) = thread.target_thread.as_deref() {
        let target_id = repo
            .refs()
            .get_thread(target_thread)?
            .ok_or_else(|| anyhow!("Target thread '{}' not found", target_thread))?;
        let thread_id = repo
            .refs()
            .get_thread(&thread.thread)?
            .ok_or_else(|| anyhow!("Thread '{}' not found", thread.thread))?;
        let current_label = format!("CURRENT ({target_thread})");
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
        merge_plan.relation().semantic_result().to_string()
    } else {
        "no_target".to_string()
    };

    let mut advice =
        describe_thread_advice(thread, false, conflicts.len(), prefer_apply_recommendation);
    if semantic_result == "already_integrated" {
        advice.blockers.clear();
        advice.recommended_action.clear();
        advice.thread_health = "clean".to_string();
    }

    let thread_tip = repo.refs().get_thread(&thread.thread)?.map(|id| id.short());
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
        advice.recommended_action = format!("heddle ship --thread {}", thread.id);
        advice.thread_health = "ready".to_string();
    }

    Ok(ThreadPreviewReport {
        thread: thread.id.clone(),
        thread_mode: thread.mode.to_string(),
        thread_state: thread.state.to_string(),
        freshness: thread.freshness.to_string(),
        task: thread.task.clone(),
        changed_paths: thread.changed_paths.iter().take(8).cloned().collect(),
        changed_path_count: thread.changed_paths.len(),
        impact_categories: thread
            .impact_categories
            .iter()
            .map(ToString::to_string)
            .collect(),
        heavy_impact_paths: thread.heavy_impact_paths.clone(),
        semantic_result,
        conflict_count,
        conflicts,
        blockers: advice.blockers,
        recommended_action: advice.recommended_action,
        thread_health: advice.thread_health,
    })
}

fn merge_output_from_report(input: MergeOutputInput<'_>) -> MergeOutput {
    let report_conflicts = input.conflicts.unwrap_or_default();
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
        //      `--git-commit` precondition failure. Fix git and re-run
        //      `heddle merge`.
        //   2. Post-snapshot (`merge_state` is Some): `git commit`
        //      itself failed AFTER heddle advanced. Re-running
        //      `heddle merge` would noop — the recovery is to fix git
        //      and re-run `git commit`. Defer to the explicit recovery
        //      hint inside `blockers` rather than emit a misleading
        //      next-action.
        if input.merge_state.is_some() {
            Some("see blockers for git recovery steps; do NOT re-run heddle merge".to_string())
        } else {
            Some("resolve git state and re-run merge".to_string())
        }
    } else if input.preview_only {
        // Clean preview: the actionable next step is the real merge.
        // The preview report's recommended_action may say "refresh"
        // or "promote" — those are warnings now, not the next step.
        input
            .thread
            .as_ref()
            .map(|t| format!("heddle merge {}", t.id))
    } else {
        // Clean apply: nothing to do.
        None
    };

    MergeOutput {
        operator: OperatorCommandOutput {
            status: status.to_string(),
            action: "merge".to_string(),
            message: input.message,
            blockers: real_blockers,
            warnings: preview_warnings,
            next_action: recommended_action.clone(),
            recommended_action,
        },
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
        changed_paths: thread_paths(input.thread),
        changed_path_count: thread_path_count(input.thread),
        impact_categories: thread_impacts(input.thread),
        promotion_suggested: input
            .thread
            .as_ref()
            .map(|thread| thread.promotion_suggested)
            .unwrap_or(false),
        heavy_impact_paths: thread_heavy_paths(input.thread),
        semantic_result: input.semantic_result.or_else(|| {
            input
                .preview_report
                .map(|report| report.semantic_result.clone())
        }),
        conflict_count: input
            .conflict_count
            .or_else(|| input.preview_report.map(|report| report.conflict_count))
            .unwrap_or(report_conflicts.len()),
        thread_health: input
            .preview_report
            .map(|report| report.thread_health.clone())
            .unwrap_or_else(|| "active".to_string()),
        renames: input.renames,
        directory_renames: input.directory_renames,
        diff: input.diff,
        git_commit_preview: input.git_commit_preview,
        git_commit: input.git_commit,
    }
}

fn emit_merge_output(cli: &Cli, output: MergeOutput) -> Result<()> {
    if should_output_json(cli, None) {
        println!("{}", serde_json::to_string(&output)?);
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
        if let Some(next) = output
            .operator
            .recommended_action
            .as_ref()
            .or(output.operator.next_action.as_ref())
        {
            println!("  recommended action: {}", style::bold(next));
        }
    }
    Ok(())
}

/// Classifies an advisory string from `describe_thread_advice` as a
/// real merge blocker (something that prevents `heddle merge` from
/// advancing state) versus a non-blocking nudge.
///
/// Real blockers are conflict-shaped strings ("path conflict(s) need
/// manual resolution", "needs attention before integration"). Items
/// like "Heavy-impact change …" or "Thread '…' is stale against …"
/// are advisory: the merge succeeds anyway and the user can act on
/// them later. Mis-classifying advisory items as blockers causes the
/// "merge succeeded but status:blocked" contradiction that this
/// helper exists to prevent.
fn is_real_merge_blocker(advisory: &str) -> bool {
    let lower = advisory.to_lowercase();
    lower.contains("path conflict") || lower.contains("needs attention before integration")
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
        if report.thread_state == "blocked" && !report.blockers.is_empty() {
            lines.push(format!("blocked: {}", report.blockers.join("; ")));
        }
        lines.push(format!("workspace: {}", report.thread_mode));
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
        lines.push(format!("semantic preview: {}", report.semantic_result));
        if report.conflict_count > 0 {
            lines.push(format!(
                "conflicts: {} path conflict(s)",
                report.conflict_count
            ));
        }
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

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
    /// Up-front identity check: when a git overlay exists but `user.name`
    /// isn't configured, `validate_git_commit_preconditions_extended`
    /// must surface a precise blocker that names the missing key and
    /// the `git config` command to fix it. Without this, the failure
    /// mode is "heddle merge succeeds; `git commit` fails inside
    /// `write_git_commit` deep in the stack" — leaving heddle advanced
    /// with no git commit on top.
    #[test]
    fn extended_validation_flags_missing_git_user_name() {
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
        // Make sure no user.name leaks in from global config: clear it
        // explicitly at the local level by writing an empty value
        // override is fragile; instead set local user.email but unset
        // user.name.
        // We can't reliably "unset" a global value from a test, so we
        // probe global config first — if user.name is already set
        // globally, the test isn't meaningful here.
        let global_name_set = Command::new("git")
            .args(["config", "--global", "--get", "user.name"])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if global_name_set {
            // Local override to empty — `git config user.name ""`
            // still counts as "set" to the get probe. Skip.
            eprintln!("global user.name is set — skipping (probe would pass)");
            return;
        }

        let blockers =
            validate_git_commit_preconditions_extended(dir.path(), &["dummy.txt".to_string()]);
        assert!(
            blockers.iter().any(|b| b.contains("git user.name")),
            "missing user.name must surface as a blocker: {blockers:?}"
        );
        assert!(
            blockers.iter().any(|b| b.contains("git config user.name")),
            "blocker must include the recovery command: {blockers:?}"
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
