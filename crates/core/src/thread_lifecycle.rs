// SPDX-License-Identifier: Apache-2.0
//! Pure thread drop / promote / refresh planning.
//!
//! Owns decision logic shared by `heddle thread drop`, `heddle thread promote`,
//! `heddle thread refresh`, and cleanup sweeps:
//! - drop disposition (refuse current / missing / delete-missing / drop steps)
//! - what a drop removes (unmount? checkout? ref? registry?)
//! - promote path defaults and in-place conversion preconditions
//! - refresh checkout selection and conflict-marker materialization (pure)
//!
//! FS materialization, merge apply, mount RPCs, registry I/O, and recovery
//! advice strings stay CLI-owned. Callers resolve path/mode/freshness facts
//! first, then invoke these helpers.

use std::path::{Path, PathBuf};

use repo::{ThreadFreshness, ThreadMode, ThreadState};

// ---------------------------------------------------------------------------
// Shared clean-worktree guard (drop + promote)
// ---------------------------------------------------------------------------

/// Where the clean-worktree preflight should look before mutating a thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CleanWorktreeGuard {
    /// `--force` (or equivalent): skip the clean-worktree check.
    Skip,
    /// Open the isolated execution path (has its own `.heddle`) and check there.
    OnExecutionPath,
    /// Check the caller's repository worktree.
    OnCallerRepo,
}

/// Select the clean-worktree guard from force + execution-path facts.
///
/// Matches CLI drop/promote: when the thread has an isolated checkout that is
/// not the repo root and contains `.heddle`, guard that tree; otherwise guard
/// the caller's repo. Force always skips.
pub fn plan_clean_worktree_guard(
    force: bool,
    execution_path_exists: bool,
    execution_path_is_repo_root: bool,
    execution_path_has_heddle: bool,
) -> CleanWorktreeGuard {
    if force {
        return CleanWorktreeGuard::Skip;
    }
    if execution_path_exists && !execution_path_is_repo_root && execution_path_has_heddle {
        CleanWorktreeGuard::OnExecutionPath
    } else {
        CleanWorktreeGuard::OnCallerRepo
    }
}

/// Whether a thread mode owns a FUSE/virtual mount that must be torn down
/// before the checkout directory is removed or replaced.
pub fn thread_mode_requires_unmount(mode: &ThreadMode) -> bool {
    matches!(mode, ThreadMode::Virtualized)
}

// ---------------------------------------------------------------------------
// Drop
// ---------------------------------------------------------------------------

/// Caller-supplied facts for pure drop preflight (no I/O).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadDropOptions {
    /// Whether a managed thread record was loaded for the requested id/name.
    pub thread_found: bool,
    /// True when the request names the attached current lane (only consulted
    /// when the record is missing).
    pub is_current_lane: bool,
    /// `heddle thread drop --delete-thread` (or cleanup-equivalent).
    pub delete_thread: bool,
    /// Skip clean-worktree preflight.
    pub force: bool,
    /// Record mode when found; ignored when missing.
    pub mode: ThreadMode,
    pub execution_path_exists: bool,
    pub execution_path_is_repo_root: bool,
    pub execution_path_has_heddle: bool,
}

/// Pure plan describing what a successful drop should remove / update.
///
/// FS, mount, registry, and ref mutations remain with the caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadDropPlan {
    pub clean_worktree: CleanWorktreeGuard,
    /// Tear down a virtualized mount before removing the execution path.
    pub unmount_virtualized: bool,
    /// Remove `execution_path` when it exists on disk.
    pub remove_execution_path: bool,
    /// Always drop the per-thread manifest sidecar.
    pub remove_manifest: bool,
    /// Mark the manager record [`ThreadState::Abandoned`].
    pub mark_abandoned: bool,
    /// Strip agent-registry entries matching thread name or id.
    pub strip_agent_registry: bool,
    /// Delete the live thread ref when present (ordinary drop only with
    /// `--delete-thread`; cleanup always requests this).
    pub delete_thread_ref: bool,
}

/// Outcome of pure drop planning before any mutation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ThreadDropDisposition {
    /// Missing record, attached as current checkout, no `--delete-thread`.
    RefuseCurrentCheckout,
    /// Missing record with `--delete-thread`: fall through to thread delete.
    ProceedDeleteMissing,
    /// Missing record and not recoverable via delete.
    NotFound,
    /// Record exists: perform the planned tear-down steps.
    Drop(ThreadDropPlan),
}

/// Pure preflight for `heddle thread drop` / `drop_thread_silent`.
///
/// Rules (matching CLI):
/// 1. Missing + current lane + no delete flag → refuse
/// 2. Missing + delete flag → proceed to delete command
/// 3. Missing otherwise → not found
/// 4. Found → drop plan (unmount if virtualized, remove checkout if present,
///    abandon record, strip agents, optionally delete ref)
pub fn plan_thread_drop(options: &ThreadDropOptions) -> ThreadDropDisposition {
    if !options.thread_found {
        if !options.delete_thread && options.is_current_lane {
            return ThreadDropDisposition::RefuseCurrentCheckout;
        }
        if options.delete_thread {
            return ThreadDropDisposition::ProceedDeleteMissing;
        }
        return ThreadDropDisposition::NotFound;
    }

    ThreadDropDisposition::Drop(ThreadDropPlan {
        clean_worktree: plan_clean_worktree_guard(
            options.force,
            options.execution_path_exists,
            options.execution_path_is_repo_root,
            options.execution_path_has_heddle,
        ),
        unmount_virtualized: thread_mode_requires_unmount(&options.mode),
        remove_execution_path: options.execution_path_exists,
        remove_manifest: true,
        mark_abandoned: true,
        strip_agent_registry: true,
        delete_thread_ref: options.delete_thread,
    })
}

/// Pure plan for a cleanup sweep drop (`thread cleanup`).
///
/// Stronger than ordinary drop: always deletes the live thread ref when
/// present. Clean-worktree is skipped (cleanup already selected merged/stale
/// threads and never runs the force gate).
pub fn plan_cleanup_thread_drop(mode: &ThreadMode, execution_path_exists: bool) -> ThreadDropPlan {
    ThreadDropPlan {
        clean_worktree: CleanWorktreeGuard::Skip,
        unmount_virtualized: thread_mode_requires_unmount(mode),
        remove_execution_path: execution_path_exists,
        remove_manifest: true,
        mark_abandoned: true,
        strip_agent_registry: true,
        delete_thread_ref: true,
    }
}

// ---------------------------------------------------------------------------
// Promote
// ---------------------------------------------------------------------------

/// Caller-supplied facts for pure promote preflight (no I/O).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadPromoteOptions {
    pub force: bool,
    /// Explicit `--path` from the caller, if any.
    pub path: Option<PathBuf>,
    /// Canonical managed checkout path (`repo.managed_checkout_path(id)`).
    /// Used when `path` is `None` so promote lands under the same layout as
    /// `start` / the per-thread manifest (heddle#572).
    pub default_path: PathBuf,
    pub mode: ThreadMode,
    pub execution_path: PathBuf,
    pub materialized_path: Option<PathBuf>,
    pub execution_path_exists: bool,
    pub execution_path_is_repo_root: bool,
    pub execution_path_has_heddle: bool,
}

/// Pure plan for `heddle thread promote`.
///
/// Materialization, mount teardown RPCs, and same-inode path confirmation
/// remain with the caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadPromotePlan {
    /// True when the caller did not supply `--path`.
    pub using_default_path: bool,
    pub target_path: PathBuf,
    pub clean_worktree: CleanWorktreeGuard,
    pub unmount_virtualized: bool,
    /// Candidate checkout to tear down before rematerializing into the default
    /// path (in-place Materialized/Solid conversion). Caller must still confirm
    /// `.heddle` presence and path identity before removing.
    pub in_place_conversion_candidate: Option<PathBuf>,
    /// Resulting workspace mode after a successful promote.
    pub resulting_mode: ThreadMode,
    /// Resulting lifecycle state after a successful promote.
    pub resulting_state: ThreadState,
}

/// Resolve the promote target path and whether the default was used.
pub fn resolve_promote_target_path(
    path: Option<PathBuf>,
    default_path: PathBuf,
) -> (PathBuf, bool) {
    match path {
        Some(explicit) => (explicit, false),
        None => (default_path, true),
    }
}

/// Existing checkout path preferred for identity / in-place conversion checks.
///
/// Prefers a non-empty `materialized_path`, else falls back to `execution_path`.
pub fn promote_existing_checkout_path(
    materialized_path: Option<&Path>,
    execution_path: &Path,
) -> PathBuf {
    materialized_path
        .filter(|p| !p.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| execution_path.to_path_buf())
}

/// Whether promote should consider tearing down the thread's own existing
/// checkout before writing a solid tree at the default path.
///
/// Final removal still requires FS checks (`.heddle` exists, same directory as
/// target) via [`promote_confirm_in_place_removal`].
pub fn promote_in_place_conversion_candidate(
    using_default_path: bool,
    mode: &ThreadMode,
    existing: PathBuf,
) -> Option<PathBuf> {
    if using_default_path && matches!(mode, ThreadMode::Materialized | ThreadMode::Solid) {
        Some(existing)
    } else {
        None
    }
}

/// Confirm in-place conversion teardown after FS identity facts are known.
///
/// `same_as_target` should be true when the candidate and promote target
/// resolve to the same directory (canonicalized when both exist).
pub fn promote_confirm_in_place_removal(
    candidate: Option<&Path>,
    existing_has_heddle: bool,
    same_as_target: bool,
) -> bool {
    let Some(existing) = candidate else {
        return false;
    };
    !existing.as_os_str().is_empty() && existing_has_heddle && same_as_target
}

/// Pure option preflight for `heddle thread promote`.
pub fn plan_thread_promote(options: &ThreadPromoteOptions) -> ThreadPromotePlan {
    let (target_path, using_default_path) =
        resolve_promote_target_path(options.path.clone(), options.default_path.clone());
    let existing = promote_existing_checkout_path(
        options.materialized_path.as_deref(),
        &options.execution_path,
    );
    ThreadPromotePlan {
        using_default_path,
        target_path,
        clean_worktree: plan_clean_worktree_guard(
            options.force,
            options.execution_path_exists,
            options.execution_path_is_repo_root,
            options.execution_path_has_heddle,
        ),
        unmount_virtualized: thread_mode_requires_unmount(&options.mode),
        in_place_conversion_candidate: promote_in_place_conversion_candidate(
            using_default_path,
            &options.mode,
            existing,
        ),
        resulting_mode: ThreadMode::Solid,
        resulting_state: ThreadState::Promoted,
    }
}

// ---------------------------------------------------------------------------
// Refresh
// ---------------------------------------------------------------------------

/// Caller-supplied facts for pure refresh preflight (no I/O).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadRefreshOptions {
    /// Whether the thread record has a `target_thread` configured.
    pub has_target_thread: bool,
    pub freshness: ThreadFreshness,
    /// True when `execution_path` is empty (branch-like / in-repo checkout).
    pub execution_path_empty: bool,
    /// Whether the caller's current lane matches this thread.
    pub is_current_lane: bool,
}

/// Pure disposition for `heddle thread refresh` before rebase/merge I/O.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ThreadRefreshPlan {
    /// No integration target configured on the thread record.
    MissingTarget,
    /// Already current relative to target — no rebase/merge needed.
    AlreadyCurrent,
    /// Branch-like thread (empty execution path) but not the current checkout.
    RequiresCurrentCheckout,
    /// Rebase/merge against the caller's open repository (current lane).
    ProceedOnCurrentRepo,
    /// Open `execution_path` and refresh that isolated checkout.
    ProceedOnExecutionPath,
}

/// Pure preflight for refresh checkout selection and no-op / refusal gates.
///
/// Rules (matching CLI `refresh_thread`):
/// 1. No target → missing target
/// 2. Freshness current → already current
/// 3. Empty execution path + current lane → proceed on caller repo
/// 4. Empty execution path + not current → requires current checkout
/// 5. Non-empty execution path → proceed on that path
pub fn plan_thread_refresh(options: &ThreadRefreshOptions) -> ThreadRefreshPlan {
    if !options.has_target_thread {
        return ThreadRefreshPlan::MissingTarget;
    }
    if options.freshness == ThreadFreshness::Current {
        return ThreadRefreshPlan::AlreadyCurrent;
    }
    if options.execution_path_empty {
        if options.is_current_lane {
            ThreadRefreshPlan::ProceedOnCurrentRepo
        } else {
            ThreadRefreshPlan::RequiresCurrentCheckout
        }
    } else {
        ThreadRefreshPlan::ProceedOnExecutionPath
    }
}

/// Whether existing file bytes already contain full conflict-marker triplets.
///
/// Used so refresh does not overwrite a user-edited conflicted file when
/// materializing markers after a conflicted 3-way merge.
pub fn contains_conflict_marker_bytes(content: &[u8]) -> bool {
    content
        .windows("<<<<<<<".len())
        .any(|window| window == b"<<<<<<<")
        && content
            .windows("=======".len())
            .any(|window| window == b"=======")
        && content
            .windows(">>>>>>>".len())
            .any(|window| window == b">>>>>>>")
}

/// Whether refresh should write conflict markers for a path given its current
/// on-disk content.
pub fn should_materialize_refresh_conflict_markers(existing: &[u8]) -> bool {
    !contains_conflict_marker_bytes(existing)
}

/// Format conflict markers for a refresh conflict (CURRENT / INCOMING).
///
/// Ensures each side ends with a newline before the next marker line so tools
/// that parse line-based conflict markers see clean boundaries.
pub fn format_refresh_conflict_markers(ours: &[u8], theirs: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(ours.len() + theirs.len() + 64);
    out.extend_from_slice(b"<<<<<<< CURRENT\n");
    out.extend_from_slice(ours);
    if !ours.ends_with(b"\n") {
        out.push(b'\n');
    }
    out.extend_from_slice(b"=======\n");
    out.extend_from_slice(theirs);
    if !theirs.ends_with(b"\n") {
        out.push(b'\n');
    }
    out.extend_from_slice(b">>>>>>> INCOMING\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn drop_opts(found: bool) -> ThreadDropOptions {
        ThreadDropOptions {
            thread_found: found,
            is_current_lane: false,
            delete_thread: false,
            force: false,
            mode: ThreadMode::Materialized,
            execution_path_exists: true,
            execution_path_is_repo_root: false,
            execution_path_has_heddle: true,
        }
    }

    #[test]
    fn plan_thread_drop_refuses_missing_current_lane() {
        let mut opts = drop_opts(false);
        opts.is_current_lane = true;
        assert_eq!(
            plan_thread_drop(&opts),
            ThreadDropDisposition::RefuseCurrentCheckout
        );
    }

    #[test]
    fn plan_thread_drop_delete_missing_record() {
        let mut opts = drop_opts(false);
        opts.delete_thread = true;
        assert_eq!(
            plan_thread_drop(&opts),
            ThreadDropDisposition::ProceedDeleteMissing
        );
    }

    #[test]
    fn plan_thread_drop_not_found() {
        assert_eq!(
            plan_thread_drop(&drop_opts(false)),
            ThreadDropDisposition::NotFound
        );
    }

    #[test]
    fn plan_thread_drop_steps_for_virtualized_with_delete() {
        let mut opts = drop_opts(true);
        opts.mode = ThreadMode::Virtualized;
        opts.delete_thread = true;
        match plan_thread_drop(&opts) {
            ThreadDropDisposition::Drop(plan) => {
                assert_eq!(plan.clean_worktree, CleanWorktreeGuard::OnExecutionPath);
                assert!(plan.unmount_virtualized);
                assert!(plan.remove_execution_path);
                assert!(plan.remove_manifest);
                assert!(plan.mark_abandoned);
                assert!(plan.strip_agent_registry);
                assert!(plan.delete_thread_ref);
            }
            other => panic!("expected Drop, got {other:?}"),
        }
    }

    #[test]
    fn plan_thread_drop_force_skips_clean_guard() {
        let mut opts = drop_opts(true);
        opts.force = true;
        opts.execution_path_exists = false;
        match plan_thread_drop(&opts) {
            ThreadDropDisposition::Drop(plan) => {
                assert_eq!(plan.clean_worktree, CleanWorktreeGuard::Skip);
                assert!(!plan.remove_execution_path);
                assert!(!plan.delete_thread_ref);
                assert!(!plan.unmount_virtualized);
            }
            other => panic!("expected Drop, got {other:?}"),
        }
    }

    #[test]
    fn plan_cleanup_thread_drop_always_deletes_ref() {
        let plan = plan_cleanup_thread_drop(&ThreadMode::Solid, true);
        assert_eq!(plan.clean_worktree, CleanWorktreeGuard::Skip);
        assert!(plan.delete_thread_ref);
        assert!(plan.remove_execution_path);
        assert!(!plan.unmount_virtualized);

        let virt = plan_cleanup_thread_drop(&ThreadMode::Virtualized, false);
        assert!(virt.unmount_virtualized);
        assert!(!virt.remove_execution_path);
    }

    #[test]
    fn plan_clean_worktree_guard_variants() {
        assert_eq!(
            plan_clean_worktree_guard(true, true, false, true),
            CleanWorktreeGuard::Skip
        );
        assert_eq!(
            plan_clean_worktree_guard(false, true, false, true),
            CleanWorktreeGuard::OnExecutionPath
        );
        assert_eq!(
            plan_clean_worktree_guard(false, true, true, true),
            CleanWorktreeGuard::OnCallerRepo
        );
        assert_eq!(
            plan_clean_worktree_guard(false, false, false, false),
            CleanWorktreeGuard::OnCallerRepo
        );
    }

    #[test]
    fn plan_thread_promote_default_path_and_solid_result() {
        let plan = plan_thread_promote(&ThreadPromoteOptions {
            force: false,
            path: None,
            default_path: PathBuf::from("/repo/.heddle/threads/feat/repo"),
            mode: ThreadMode::Materialized,
            execution_path: PathBuf::from("/repo/.heddle/threads/feat/repo"),
            materialized_path: Some(PathBuf::from("/repo/.heddle/threads/feat/repo")),
            execution_path_exists: true,
            execution_path_is_repo_root: false,
            execution_path_has_heddle: true,
        });
        assert!(plan.using_default_path);
        assert_eq!(
            plan.target_path,
            PathBuf::from("/repo/.heddle/threads/feat/repo")
        );
        assert_eq!(plan.clean_worktree, CleanWorktreeGuard::OnExecutionPath);
        assert!(!plan.unmount_virtualized);
        assert_eq!(
            plan.in_place_conversion_candidate.as_deref(),
            Some(Path::new("/repo/.heddle/threads/feat/repo"))
        );
        assert_eq!(plan.resulting_mode, ThreadMode::Solid);
        assert_eq!(plan.resulting_state, ThreadState::Promoted);
    }

    #[test]
    fn plan_thread_promote_explicit_path_skips_in_place_candidate() {
        let plan = plan_thread_promote(&ThreadPromoteOptions {
            force: true,
            path: Some(PathBuf::from("/tmp/out")),
            default_path: PathBuf::from("/repo/.heddle/threads/feat/repo"),
            mode: ThreadMode::Virtualized,
            execution_path: PathBuf::from("/mnt/feat"),
            materialized_path: None,
            execution_path_exists: true,
            execution_path_is_repo_root: false,
            execution_path_has_heddle: false,
        });
        assert!(!plan.using_default_path);
        assert_eq!(plan.target_path, PathBuf::from("/tmp/out"));
        assert_eq!(plan.clean_worktree, CleanWorktreeGuard::Skip);
        assert!(plan.unmount_virtualized);
        assert!(plan.in_place_conversion_candidate.is_none());
    }

    #[test]
    fn promote_existing_prefers_materialized_path() {
        assert_eq!(
            promote_existing_checkout_path(Some(Path::new("/mat")), Path::new("/exec")),
            PathBuf::from("/mat")
        );
        assert_eq!(
            promote_existing_checkout_path(Some(Path::new("")), Path::new("/exec")),
            PathBuf::from("/exec")
        );
        assert_eq!(
            promote_existing_checkout_path(None, Path::new("/exec")),
            PathBuf::from("/exec")
        );
    }

    #[test]
    fn promote_confirm_in_place_removal_requires_identity() {
        let candidate = PathBuf::from("/repo/.heddle/threads/feat/repo");
        assert!(promote_confirm_in_place_removal(
            Some(&candidate),
            true,
            true
        ));
        assert!(!promote_confirm_in_place_removal(
            Some(&candidate),
            false,
            true
        ));
        assert!(!promote_confirm_in_place_removal(
            Some(&candidate),
            true,
            false
        ));
        assert!(!promote_confirm_in_place_removal(None, true, true));
        assert!(!promote_confirm_in_place_removal(
            Some(Path::new("")),
            true,
            true
        ));
    }

    #[test]
    fn plan_thread_refresh_dispositions() {
        let base = ThreadRefreshOptions {
            has_target_thread: true,
            freshness: ThreadFreshness::Stale,
            execution_path_empty: false,
            is_current_lane: false,
        };
        assert_eq!(
            plan_thread_refresh(&ThreadRefreshOptions {
                has_target_thread: false,
                ..base.clone()
            }),
            ThreadRefreshPlan::MissingTarget
        );
        assert_eq!(
            plan_thread_refresh(&ThreadRefreshOptions {
                freshness: ThreadFreshness::Current,
                ..base.clone()
            }),
            ThreadRefreshPlan::AlreadyCurrent
        );
        assert_eq!(
            plan_thread_refresh(&ThreadRefreshOptions {
                execution_path_empty: true,
                is_current_lane: false,
                ..base.clone()
            }),
            ThreadRefreshPlan::RequiresCurrentCheckout
        );
        assert_eq!(
            plan_thread_refresh(&ThreadRefreshOptions {
                execution_path_empty: true,
                is_current_lane: true,
                ..base.clone()
            }),
            ThreadRefreshPlan::ProceedOnCurrentRepo
        );
        assert_eq!(
            plan_thread_refresh(&base),
            ThreadRefreshPlan::ProceedOnExecutionPath
        );
    }

    #[test]
    fn conflict_marker_detection_and_format() {
        let marked = b"<<<<<<< CURRENT\na\n=======\nb\n>>>>>>> INCOMING\n";
        assert!(contains_conflict_marker_bytes(marked));
        assert!(!should_materialize_refresh_conflict_markers(marked));
        assert!(!contains_conflict_marker_bytes(b"clean content"));
        assert!(should_materialize_refresh_conflict_markers(b"clean"));

        let formatted = format_refresh_conflict_markers(b"ours-line", b"theirs-line\n");
        assert_eq!(
            formatted,
            b"<<<<<<< CURRENT\nours-line\n=======\ntheirs-line\n>>>>>>> INCOMING\n"
        );
        let already_nl = format_refresh_conflict_markers(b"a\n", b"b\n");
        assert_eq!(
            already_nl,
            b"<<<<<<< CURRENT\na\n=======\nb\n>>>>>>> INCOMING\n"
        );
    }
}
