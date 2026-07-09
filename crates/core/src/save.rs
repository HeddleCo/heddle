// SPDX-License-Identifier: Apache-2.0
//! Shared save primitive for `capture` / `commit` / `checkpoint` / ready auto-capture.
//!
//! CLI verbs become thin shells that build a [`SavePlan`] and call
//! [`execute_save`]. Repo keeps atomic tree/state mutation; this module owns
//! the composition of preflight-adjacent routing, Heddle snapshot, and
//! optional Git-overlay write-through.

use std::time::Instant;

use anyhow::{Context, Result, anyhow};
use heddle_git_projection::{GitProjection, WriteThroughOutcome};
use objects::{
    HeddleError, RecoveryDetails,
    object::{Agent, Attribution, ChangeId, ContentHash, Principal, State, Tree},
};
use oplog::{OpLogBackend, OpRecord};
use refs::Head;
use repo::{
    GitCheckpointRecord, Hook, HookContext, HookManager, Repository, RepositoryCapability,
    SnapshotProfile, WorktreeStatusOptions, refresh_active_thread_metadata,
};
use serde::Serialize;
use sley::Repository as SleyRepository;

use crate::{
    RepositoryVerificationState, build_repository_verification_health_with_worktree_status,
    build_repository_verification_state,
    build_repository_verification_state_with_worktree_status,
};

/// How far a save should write through into Git (Git-overlay only).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GitScope {
    /// Heddle state only — no Git checkpoint (capture; native commit).
    None,
    /// Checkpoint the staged Git index boundary (caller supplies the tree).
    Staged,
    /// Capture/checkpoint the full worktree (or current clean state).
    WorktreeAll,
}

/// Public CLI / facade verb that requested the save.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SaveVerb {
    Capture,
    Commit,
    Checkpoint,
}

/// Inputs for [`execute_save`]. Attribution is resolved by the caller so CLI
/// env/harness/agent precedence stays at the embedding surface.
#[derive(Debug)]
pub struct SavePlan {
    pub verb: SaveVerb,
    pub intent: Option<String>,
    pub confidence: Option<f32>,
    pub attribution: Attribution,
    pub git_scope: GitScope,
    /// When set, snapshot this tree instead of walking the worktree
    /// (staged-index commits).
    pub supplied_tree: Option<Tree>,
    /// Prefer the current HEAD state when present (checkpoint bootstrap path).
    pub reuse_current_state: bool,
    /// After ensuring state, refuse dirty Heddle worktree before Git write-through.
    pub require_clean_worktree: bool,
    pub worktree_status_options: WorktreeStatusOptions,
    /// Run pre/post snapshot hooks when creating a new Heddle state.
    pub run_hooks: bool,
    /// Map post-verify "commit" next actions to `heddle status` (commit UX).
    pub commit_safe_post_verify: bool,
    /// Fold snapshot + GitCheckpoint oplog batches into one undo unit.
    pub coalesce_snapshot_and_checkpoint: bool,
    /// Optional precomputed git-overlay worktree status for verification reuse
    /// on the no-new-state path. Post-mutation paths always recompute.
    pub precomputed_worktree_status:
        Option<repo::Result<Option<objects::worktree::WorktreeStatus>>>,
}

impl SavePlan {
    pub fn capture(intent: impl Into<String>, attribution: Attribution) -> Self {
        Self {
            verb: SaveVerb::Capture,
            intent: Some(intent.into()),
            confidence: None,
            attribution,
            git_scope: GitScope::None,
            supplied_tree: None,
            reuse_current_state: false,
            require_clean_worktree: false,
            worktree_status_options: WorktreeStatusOptions::default(),
            run_hooks: true,
            commit_safe_post_verify: false,
            coalesce_snapshot_and_checkpoint: false,
            precomputed_worktree_status: None,
        }
    }

    pub fn commit(
        intent: impl Into<String>,
        attribution: Attribution,
        git_scope: GitScope,
    ) -> Self {
        Self {
            verb: SaveVerb::Commit,
            intent: Some(intent.into()),
            confidence: None,
            attribution,
            git_scope,
            supplied_tree: None,
            reuse_current_state: false,
            require_clean_worktree: matches!(git_scope, GitScope::WorktreeAll),
            worktree_status_options: WorktreeStatusOptions::default(),
            run_hooks: true,
            commit_safe_post_verify: true,
            coalesce_snapshot_and_checkpoint: matches!(
                git_scope,
                GitScope::Staged | GitScope::WorktreeAll
            ),
            precomputed_worktree_status: None,
        }
    }

    pub fn checkpoint(message: Option<String>, attribution: Attribution, staged: bool) -> Self {
        Self {
            verb: SaveVerb::Checkpoint,
            intent: message,
            confidence: None,
            attribution,
            git_scope: if staged {
                GitScope::Staged
            } else {
                GitScope::WorktreeAll
            },
            supplied_tree: None,
            reuse_current_state: true,
            require_clean_worktree: !staged,
            worktree_status_options: WorktreeStatusOptions::default(),
            run_hooks: true,
            commit_safe_post_verify: false,
            coalesce_snapshot_and_checkpoint: false,
            precomputed_worktree_status: None,
        }
    }

    pub fn with_confidence(mut self, confidence: Option<f32>) -> Self {
        self.confidence = confidence;
        self
    }

    pub fn with_supplied_tree(mut self, tree: Tree) -> Self {
        self.supplied_tree = Some(tree);
        self
    }

    pub fn with_worktree_status_options(mut self, options: WorktreeStatusOptions) -> Self {
        self.worktree_status_options = options;
        self
    }

    pub fn with_precomputed_worktree_status(
        mut self,
        status: repo::Result<Option<objects::worktree::WorktreeStatus>>,
    ) -> Self {
        self.precomputed_worktree_status = Some(status);
        self
    }
}

/// Result of a successful save.
#[derive(Debug, Clone)]
pub struct SaveReport {
    pub verb: SaveVerb,
    pub change_id: ChangeId,
    pub content_hash: ContentHash,
    pub intent: Option<String>,
    pub confidence: Option<f32>,
    pub signed: bool,
    pub git_commit: Option<String>,
    pub git_previous_commit: Option<String>,
    pub summary: String,
    pub principal: Principal,
    pub agent: Option<Agent>,
    pub promotion_suggested: bool,
    pub heavy_impact_paths: Vec<String>,
    pub verification: RepositoryVerificationState,
    pub created_new_state: bool,
    pub git_checkpoint: Option<GitCheckpointRecord>,
    pub snapshot_profile: SnapshotProfile,
    pub thread_metadata_ms: u128,
}

/// Pure routing helper: which Git write-through scope a verb should use.
///
/// Used by unit tests and by CLI shells that build a [`SavePlan`] before
/// calling [`execute_save`].
pub fn plan_git_scope(
    verb: SaveVerb,
    capability: RepositoryCapability,
    staged_index_paths: bool,
    include_all_worktree: bool,
) -> GitScope {
    match verb {
        SaveVerb::Capture => GitScope::None,
        SaveVerb::Checkpoint => {
            if staged_index_paths {
                GitScope::Staged
            } else {
                GitScope::WorktreeAll
            }
        }
        SaveVerb::Commit => {
            if capability != RepositoryCapability::GitOverlay {
                GitScope::None
            } else if staged_index_paths && !include_all_worktree {
                GitScope::Staged
            } else {
                GitScope::WorktreeAll
            }
        }
    }
}

/// Whether this plan should create a new Heddle state (vs reusing HEAD).
pub fn plan_creates_new_state(plan: &SavePlan, has_current_state: bool) -> bool {
    if plan.supplied_tree.is_some() {
        return true;
    }
    if plan.reuse_current_state && has_current_state {
        return false;
    }
    // Checkpoint without current state still bootstraps a capture.
    if plan.verb == SaveVerb::Checkpoint && has_current_state {
        return false;
    }
    true
}

/// Whether this plan should perform a Git-overlay write-through.
pub fn plan_writes_git_checkpoint(
    plan: &SavePlan,
    capability: RepositoryCapability,
) -> bool {
    plan.git_scope != GitScope::None && capability == RepositoryCapability::GitOverlay
}

/// Execute a save: optional Heddle snapshot + optional Git checkpoint write-through.
///
/// Callers own clap validation (missing message/intent) and plain-Git refusal.
/// Mutation composition, hooks, thread metadata, Git write-through, and post
/// verification live here.
pub fn execute_save(repo: &Repository, plan: SavePlan) -> Result<SaveReport> {
    // A plan that asks for a Git checkpoint on a non-overlay repo is a hard
    // error: `plan_writes_git_checkpoint` silently returns false for native
    // repos, so guard on the raw `git_scope` intent instead (the previous
    // `plan_writes_git_checkpoint(..) && capability != GitOverlay` was
    // self-contradictory and never fired).
    if plan.git_scope != GitScope::None
        && repo.capability() != RepositoryCapability::GitOverlay
    {
        return Err(anyhow!(HeddleError::recovery(
            RecoveryDetails::safety_refusal(
                "native_checkpoint_unavailable",
                "heddle checkpoint is only available in Git-overlay repositories",
                "Use `heddle commit -m \"...\"` to save Heddle state in a native checkout.",
                "this checkout is not a Git-overlay repository",
                "checkpoint would try to write a Git commit where no active Git store is bound",
                "repository state, refs, and worktree files were left unchanged",
            ),
        )));
    }

    let has_current = repo.current_state()?.is_some();
    let mut created_new_state = false;
    let mut snapshot_profile = SnapshotProfile::default();
    let mut thread_metadata_ms = 0u128;
    let mut promotion_suggested = false;
    let mut heavy_impact_paths = Vec::new();
    let mut snapshot_change_id: Option<ChangeId> = None;

    let mut state = if plan_creates_new_state(&plan, has_current) {
        created_new_state = true;
        let execution = create_heddle_state(repo, &plan)?;
        snapshot_profile = execution.profile;
        thread_metadata_ms = execution.thread_metadata_ms;
        promotion_suggested = execution.promotion_suggested;
        heavy_impact_paths = execution.heavy_impact_paths;
        snapshot_change_id = Some(execution.state.change_id);
        execution.state
    } else {
        repo.current_state()?
            .ok_or_else(|| anyhow!("no captured state found for save"))?
    };

    let mut git_commit = None;
    let mut git_previous_commit = None;
    let mut git_checkpoint = None;

    if plan_writes_git_checkpoint(&plan, repo.capability()) {
        if plan.require_clean_worktree {
            let tree = repo.require_tree(&state.tree)?;
            let status =
                repo.compare_worktree_cached_detailed_with_options(&tree, &plan.worktree_status_options)?;
            if !status.is_clean() {
                return Err(anyhow!(HeddleError::recovery(
                    RecoveryDetails::safety_refusal(
                        "dirty_worktree",
                        "Save or stash worktree changes before checkpoint",
                        "Save the work with `heddle commit -m \"...\"`, then retry the checkpoint.",
                        "the current Heddle state was left unchanged; these paths have not been captured",
                        "checkpoint would write a Git commit that does not include dirty worktree paths",
                        "the current Heddle state was left unchanged; these paths have not been captured",
                    ),
                )));
            }
        }

        if let Some(existing) = repo.latest_git_checkpoint_for_change(&state.change_id)? {
            git_commit = Some(existing.git_commit.clone());
            git_checkpoint = Some(existing);
        } else {
            let previous = git_rev_parse_head(repo.root());
            git_previous_commit = previous.clone();
            let summary = checkpoint_summary(&plan, &state);
            let record = write_git_checkpoint(repo, &state, summary)?;
            if plan.coalesce_snapshot_and_checkpoint
                && let Some(change_id) = snapshot_change_id.as_ref()
            {
                coalesce_snapshot_and_checkpoint(repo, change_id, &record.git_commit)?;
            }
            git_commit = Some(record.git_commit.clone());
            git_checkpoint = Some(record);
        }
    }

    // Post-mutation verification is always fresh when we created state or wrote
    // a Git checkpoint (those mutations flip health classification). Otherwise
    // reuse a caller-supplied worktree status to avoid a redundant walk.
    let mut verification = if created_new_state || git_checkpoint.is_some() {
        build_repository_verification_state(repo)?
    } else if let Some(status) = &plan.precomputed_worktree_status {
        let health = build_repository_verification_health_with_worktree_status(repo, status);
        build_repository_verification_state_with_worktree_status(repo, health, status)
    } else {
        build_repository_verification_state(repo)?
    };
    if plan.commit_safe_post_verify {
        soften_commit_next_action(&mut verification);
    }

    let summary = match plan.verb {
        SaveVerb::Capture => format!(
            "Captured state {} ({})",
            state.change_id.short(),
            state.hash().short()
        ),
        SaveVerb::Commit => plan
            .intent
            .clone()
            .unwrap_or_else(|| format!("Commit {}", state.change_id.short())),
        SaveVerb::Checkpoint => git_checkpoint
            .as_ref()
            .map(|r| r.summary.clone())
            .unwrap_or_else(|| format!("Checkpoint {}", state.change_id.short())),
    };

    Ok(SaveReport {
        verb: plan.verb,
        change_id: state.change_id,
        content_hash: state.hash(),
        intent: state.intent.clone(),
        confidence: state.confidence,
        signed: state.signature.is_some(),
        git_commit,
        git_previous_commit,
        summary,
        principal: state.attribution.principal.clone(),
        agent: state.attribution.agent.clone(),
        promotion_suggested,
        heavy_impact_paths,
        verification,
        created_new_state,
        git_checkpoint,
        snapshot_profile,
        thread_metadata_ms,
    })
}

struct CreatedState {
    state: State,
    profile: SnapshotProfile,
    thread_metadata_ms: u128,
    promotion_suggested: bool,
    heavy_impact_paths: Vec<String>,
}

fn create_heddle_state(repo: &Repository, plan: &SavePlan) -> Result<CreatedState> {
    let hook_manager = HookManager::new(repo);
    let hook_ctx = HookContext::new(repo);

    if plan.run_hooks {
        hook_manager.run(Hook::PreSnapshot, &hook_ctx)?;
        let pre_capture_payload = serde_json::json!({
            "thread": current_thread_name(repo),
            "intent": plan.intent.clone().unwrap_or_default(),
        });
        let pre_capture_response = hook_manager.run_with_payload(
            Hook::PreSnapshot,
            &hook_ctx,
            &pre_capture_payload,
            std::time::Duration::from_secs(5),
        )?;
        if let Some(resp) = pre_capture_response
            && !resp.abort.is_empty()
        {
            return Err(anyhow!(HeddleError::recovery(
                RecoveryDetails::safety_refusal(
                    "hook_veto",
                    format!("pre_capture hook aborted capture: {}", resp.abort),
                    "Address the hook veto reason, then retry the save.",
                    "a repository hook refused the capture",
                    "capture would have written a new Heddle state",
                    "repository state, refs, metadata, and worktree files were left unchanged",
                ),
            )));
        }
    }

    let mut execution = if let Some(tree) = plan.supplied_tree.clone() {
        repo.snapshot_tree_with_attribution_profiled(
            tree,
            plan.intent.clone(),
            plan.confidence,
            plan.attribution.clone(),
        )?
    } else {
        repo.snapshot_with_attribution_profiled(
            plan.intent.clone(),
            plan.confidence,
            plan.attribution.clone(),
        )?
    };

    let thread_metadata_start = Instant::now();
    let refresh = refresh_active_thread_metadata(repo, &execution.state, &execution.tree)?;
    let thread_metadata_ms = thread_metadata_start.elapsed().as_millis();

    if plan.run_hooks {
        hook_manager.run(Hook::PostSnapshot, &hook_ctx)?;
        let post_capture_payload = serde_json::json!({
            "state_id": execution.state.change_id.to_string_full(),
        });
        if let Err(err) = hook_manager.run_with_payload(
            Hook::PostSnapshot,
            &hook_ctx,
            &post_capture_payload,
            std::time::Duration::from_secs(5),
        ) {
            tracing::warn!(error = %err, "post_capture hook error swallowed");
        }
    }

    Ok(CreatedState {
        state: execution.state,
        profile: std::mem::take(&mut execution.profile),
        thread_metadata_ms,
        promotion_suggested: refresh.promotion_suggested,
        heavy_impact_paths: refresh.heavy_impact_paths,
    })
}

fn write_git_checkpoint(
    repo: &Repository,
    state: &State,
    summary: String,
) -> Result<GitCheckpointRecord> {
    let branch = repo
        .git_overlay_current_branch()?
        .unwrap_or_else(|| "HEAD".to_string());
    let previous_git_oid = git_rev_parse_head(repo.root());
    let mut bridge = GitProjection::new(repo);
    let git_commit = match bridge
        .write_through_current_checkout_with_message(state.change_id, summary.clone())?
    {
        WriteThroughOutcome::Wrote(git_commit) => git_commit.to_string(),
        WriteThroughOutcome::Skipped(reason) => {
            return Err(anyhow!(HeddleError::recovery(
                RecoveryDetails::safety_refusal(
                    "checkpoint_git_write_skipped",
                    format!("Git checkpoint write-through was skipped: {reason}"),
                    "Inspect `heddle verify`, resolve the skip reason, then retry `heddle checkpoint -m \"...\"`.",
                    format!("write-through skipped: {reason}"),
                    "checkpoint would need to write the current Heddle state into the Git branch and index",
                    "the current Heddle state was preserved; no Git checkpoint record was written",
                ),
            )));
        }
    };
    let record = repo.record_git_checkpoint(&state.change_id, git_commit.clone(), summary)?;
    repo.oplog().record_batch_scoped(
        vec![OpRecord::GitCheckpoint {
            branch,
            state: state.change_id,
            previous_git_oid,
            new_git_oid: git_commit,
        }],
        Some(&repo.op_scope()),
    )?;
    Ok(record)
}

fn coalesce_snapshot_and_checkpoint(
    repo: &Repository,
    change_id: &ChangeId,
    git_commit: &str,
) -> Result<()> {
    let snapshot_batch = repo
        .oplog()
        .recent_batches_scoped(8, Some(&repo.op_scope()))?
        .into_iter()
        .find(|batch| {
            batch.entries.iter().any(|entry| {
                matches!(
                    &entry.operation,
                    OpRecord::Snapshot { new_state, .. } if new_state == change_id
                )
            })
        })
        .ok_or_else(|| anyhow!("capture succeeded but its oplog batch was not found"))?;
    let checkpoint_batch = repo
        .oplog()
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
        .ok_or_else(|| anyhow!("Git checkpoint succeeded but its oplog batch was not found"))?;
    repo.oplog()
        .coalesce_batches(snapshot_batch.id, checkpoint_batch.id)
        .context(
            "commit completed but failed to record capture and Git checkpoint as one undo batch",
        )?;
    Ok(())
}

fn checkpoint_summary(plan: &SavePlan, state: &State) -> String {
    plan.intent
        .clone()
        .or_else(|| state.intent.clone())
        .unwrap_or_else(|| format!("Checkpoint {}", state.change_id.short()))
}

fn current_thread_name(repo: &Repository) -> String {
    match repo.head_ref() {
        Ok(Head::Attached { thread }) => thread.to_string(),
        _ => String::new(),
    }
}

fn git_rev_parse_head(root: &std::path::Path) -> Option<String> {
    let git = SleyRepository::discover(root).ok()?;
    git.head().ok()?.oid.map(|id| id.to_string())
}

fn soften_commit_next_action(trust: &mut RepositoryVerificationState) {
    if is_commit_action(&trust.recommended_action) {
        trust.recommended_action = "heddle status".to_string();
        trust.recommended_action_template = None;
    }
    for check in &mut trust.checks {
        if check
            .recommended_action
            .as_deref()
            .is_some_and(is_commit_action)
        {
            check.recommended_action = Some("heddle status".to_string());
            check.recommended_action_template = None;
        }
    }
}

fn is_commit_action(action: &str) -> bool {
    let trimmed = action.trim();
    trimmed == "heddle commit"
        || trimmed.starts_with("heddle commit ")
        || trimmed == "commit"
        || trimmed.starts_with("commit ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use repo::RepositoryCapability;

    #[test]
    fn capture_always_uses_git_scope_none() {
        assert_eq!(
            plan_git_scope(SaveVerb::Capture, RepositoryCapability::GitOverlay, true, true),
            GitScope::None
        );
        assert_eq!(
            plan_git_scope(SaveVerb::Capture, RepositoryCapability::NativeHeddle, false, false),
            GitScope::None
        );
    }

    #[test]
    fn commit_native_never_writes_git() {
        assert_eq!(
            plan_git_scope(SaveVerb::Commit, RepositoryCapability::NativeHeddle, true, true),
            GitScope::None
        );
    }

    #[test]
    fn commit_git_overlay_routes_staged_vs_worktree() {
        assert_eq!(
            plan_git_scope(SaveVerb::Commit, RepositoryCapability::GitOverlay, true, false),
            GitScope::Staged
        );
        assert_eq!(
            plan_git_scope(SaveVerb::Commit, RepositoryCapability::GitOverlay, true, true),
            GitScope::WorktreeAll
        );
        assert_eq!(
            plan_git_scope(SaveVerb::Commit, RepositoryCapability::GitOverlay, false, false),
            GitScope::WorktreeAll
        );
    }

    #[test]
    fn checkpoint_routes_staged_flag() {
        assert_eq!(
            plan_git_scope(SaveVerb::Checkpoint, RepositoryCapability::GitOverlay, true, false),
            GitScope::Staged
        );
        assert_eq!(
            plan_git_scope(SaveVerb::Checkpoint, RepositoryCapability::GitOverlay, false, false),
            GitScope::WorktreeAll
        );
    }

    #[test]
    fn plan_creates_new_state_routing() {
        let attr = Attribution::human(Principal::new("Ada", "ada@example.com"));
        let capture = SavePlan::capture("wip", attr.clone());
        assert!(plan_creates_new_state(&capture, true));
        assert!(plan_creates_new_state(&capture, false));

        let checkpoint = SavePlan::checkpoint(Some("cp".into()), attr.clone(), false);
        assert!(!plan_creates_new_state(&checkpoint, true));
        assert!(plan_creates_new_state(&checkpoint, false));

        let staged = SavePlan::commit("msg", attr, GitScope::Staged)
            .with_supplied_tree(Tree::new());
        assert!(plan_creates_new_state(&staged, true));
    }

    #[test]
    fn plan_writes_git_checkpoint_respects_scope_and_capability() {
        let attr = Attribution::human(Principal::new("Ada", "ada@example.com"));
        let capture = SavePlan::capture("wip", attr.clone());
        assert!(!plan_writes_git_checkpoint(
            &capture,
            RepositoryCapability::GitOverlay
        ));

        let commit = SavePlan::commit("msg", attr.clone(), GitScope::WorktreeAll);
        assert!(plan_writes_git_checkpoint(
            &commit,
            RepositoryCapability::GitOverlay
        ));
        assert!(!plan_writes_git_checkpoint(
            &commit,
            RepositoryCapability::NativeHeddle
        ));

        let none = SavePlan::commit("msg", attr, GitScope::None);
        assert!(!plan_writes_git_checkpoint(
            &none,
            RepositoryCapability::GitOverlay
        ));
    }

    #[test]
    fn save_plan_builders_set_expected_defaults() {
        let attr = Attribution::human(Principal::new("Ada", "ada@example.com"));
        let capture = SavePlan::capture("intent", attr.clone());
        assert_eq!(capture.verb, SaveVerb::Capture);
        assert_eq!(capture.git_scope, GitScope::None);
        assert!(!capture.coalesce_snapshot_and_checkpoint);

        let commit = SavePlan::commit("msg", attr.clone(), GitScope::WorktreeAll);
        assert_eq!(commit.verb, SaveVerb::Commit);
        assert!(commit.coalesce_snapshot_and_checkpoint);
        assert!(commit.commit_safe_post_verify);

        let staged = SavePlan::checkpoint(None, attr, true);
        assert_eq!(staged.git_scope, GitScope::Staged);
        assert!(!staged.require_clean_worktree);
        assert!(staged.reuse_current_state);
    }
}
