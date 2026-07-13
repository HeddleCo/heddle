// SPDX-License-Identifier: Apache-2.0
//! Pure thread create/start planning.
//!
//! Owns decision logic shared by `heddle start`, `heddle thread start`, and
//! `heddle thread create`:
//! - name validation (safe shell-token / reserved structure rules)
//! - default base selection rules
//! - path isolation requirements (pure checks on normalized paths/options)
//! - workspace mode planning from request + host facts
//!
//! Materialization, checkout, registry writes, and repository I/O stay
//! CLI-owned. Callers resolve states/paths first, then invoke these helpers.

use std::path::{Path, PathBuf};

use objects::object::StateId;
use repo::{ThreadId, ThreadIdError, ThreadMode};

// ---------------------------------------------------------------------------
// Options / plan types
// ---------------------------------------------------------------------------

/// Workspace mode as requested by the caller (maps from CLI `--workspace`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WorkspaceModeRequest {
    /// Let Heddle choose (config + host capabilities).
    #[default]
    Auto,
    /// Clonefile/reflink materialized checkout when the host allows.
    Materialized,
    /// Virtualized mount path.
    Virtualized,
    /// Full-copy solid checkout.
    Solid,
}

/// Caller-supplied start inputs for pure preflight planning.
///
/// Field names mirror the CLI `ThreadStartArgs` surface used by
/// `heddle start` / `heddle thread start`. Network, materialization, and
/// actor-registry fields that are not part of pure planning are omitted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadStartOptions {
    pub name: String,
    pub from: Option<String>,
    pub path: Option<PathBuf>,
    pub workspace: WorkspaceModeRequest,
    pub parent_thread: Option<String>,
    pub automated: bool,
    pub task: Option<String>,
    pub shared_target: bool,
    pub hydrate: bool,
}

/// Caller-supplied create inputs for pure preflight planning.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadCreateOptions {
    pub name: String,
    pub ephemeral: bool,
    pub ttl_secs: Option<u32>,
}

/// Pure plan for `heddle thread create` after name validation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadCreatePlan {
    pub name: ThreadId,
    pub ephemeral: bool,
    pub ttl_secs: Option<u32>,
}

/// Pure plan for `heddle start` / `heddle thread start` after option preflight.
///
/// Base resolution, FS path checks, and materialization remain with the
/// caller; this captures what can be decided from options alone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadStartPlan {
    pub name: ThreadId,
    /// True when the caller supplied an explicit `--path`.
    pub has_explicit_path: bool,
    /// Whether the start preflight should require a clean worktree.
    ///
    /// Matches CLI: isolated checkouts (`--path`) refuse a dirty tree so the
    /// parent worktree is not partially moved into the new checkout.
    pub requires_clean_worktree: bool,
    /// Echo of the caller's workspace request (mode is finalized later with
    /// host/config facts via [`plan_thread_mode`]).
    pub workspace: WorkspaceModeRequest,
    pub from: Option<String>,
    pub path: Option<PathBuf>,
    pub parent_thread: Option<String>,
    pub automated: bool,
    pub task: Option<String>,
    pub shared_target: bool,
    pub hydrate: bool,
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Failures from pure thread create/start planning.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ThreadPlanError {
    /// Thread name failed the safe-slug / reserved-structure rule.
    InvalidName(ThreadIdError),
}

impl std::fmt::Display for ThreadPlanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidName(err) => write!(f, "{err}"),
        }
    }
}

impl std::error::Error for ThreadPlanError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::InvalidName(err) => Some(err),
        }
    }
}

impl From<ThreadIdError> for ThreadPlanError {
    fn from(value: ThreadIdError) -> Self {
        Self::InvalidName(value)
    }
}

/// Failures from pure base selection once states are resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ThreadBaseError {
    /// Thread already exists at `existing`, but `--from` resolved to a different state.
    AnchorMismatch {
        existing: StateId,
        requested: StateId,
    },
}

impl std::fmt::Display for ThreadBaseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AnchorMismatch {
                existing,
                requested,
            } => write!(
                f,
                "thread is anchored at {}, but --from resolved to {}",
                existing.short(),
                requested.short()
            ),
        }
    }
}

impl std::error::Error for ThreadBaseError {}

// ---------------------------------------------------------------------------
// Name validation / create + start preflight
// ---------------------------------------------------------------------------

/// Validate a thread name against the shared safe-slug rule.
///
/// This is the single creation-boundary check used by start and create. It
/// rejects empty names, shell metacharacters, `..` segments, and leading `/`
/// or `-` (see [`repo::validate_thread_id`]).
pub fn validate_thread_name(name: &str) -> Result<ThreadId, ThreadPlanError> {
    ThreadId::new(name).map_err(ThreadPlanError::from)
}

/// Pure preflight for `heddle thread create`.
pub fn plan_thread_create(
    options: &ThreadCreateOptions,
) -> Result<ThreadCreatePlan, ThreadPlanError> {
    let name = validate_thread_name(&options.name)?;
    Ok(ThreadCreatePlan {
        name,
        ephemeral: options.ephemeral,
        ttl_secs: options.ttl_secs,
    })
}

/// Pure option preflight for `heddle start` / `heddle thread start`.
///
/// Validates the name and records pure path-isolation flags derived from
/// options. Does not open the repository or touch the filesystem.
pub fn plan_thread_start(options: &ThreadStartOptions) -> Result<ThreadStartPlan, ThreadPlanError> {
    let name = validate_thread_name(&options.name)?;
    let has_explicit_path = options.path.is_some();
    Ok(ThreadStartPlan {
        name,
        has_explicit_path,
        requires_clean_worktree: start_requires_clean_worktree(has_explicit_path),
        workspace: options.workspace,
        from: options.from.clone(),
        path: options.path.clone(),
        parent_thread: options.parent_thread.clone(),
        automated: options.automated,
        task: options.task.clone(),
        shared_target: options.shared_target,
        hydrate: options.hydrate,
    })
}

/// Whether `heddle start` must refuse a dirty worktree for these options.
///
/// Explicit `--path` materializes an isolated checkout; dirty parent trees
/// are refused so unsaved work is not partially copied into the new tree.
pub fn start_requires_clean_worktree(has_explicit_path: bool) -> bool {
    has_explicit_path
}

// ---------------------------------------------------------------------------
// Base selection
// ---------------------------------------------------------------------------

/// Outcome of pure base selection after `--from` / existing tip are resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ThreadBaseSelection {
    /// Use this already-resolved change id as the thread base.
    Use(StateId),
    /// No existing tip and no `--from`: caller must use current HEAD / bootstrap.
    RequireCurrent,
}

/// Select the base state for a new or resumed thread start.
///
/// Rules (matching CLI `start_thread`):
/// 1. Existing tip + matching `--from` (or no `--from`) → use existing tip
/// 2. Existing tip + mismatched `--from` → [`ThreadBaseError::AnchorMismatch`]
/// 3. No existing tip + `--from` → use the resolved `--from`
/// 4. Neither → [`ThreadBaseSelection::RequireCurrent`]
pub fn select_thread_base(
    requested_from: Option<StateId>,
    existing_tip: Option<StateId>,
) -> Result<ThreadBaseSelection, ThreadBaseError> {
    match (requested_from, existing_tip) {
        (Some(requested), Some(existing)) if requested != existing => {
            Err(ThreadBaseError::AnchorMismatch {
                existing,
                requested,
            })
        }
        (Some(_), Some(existing)) => Ok(ThreadBaseSelection::Use(existing)),
        (None, Some(existing)) => Ok(ThreadBaseSelection::Use(existing)),
        (Some(requested), None) => Ok(ThreadBaseSelection::Use(requested)),
        (None, None) => Ok(ThreadBaseSelection::RequireCurrent),
    }
}

// ---------------------------------------------------------------------------
// Path isolation
// ---------------------------------------------------------------------------

/// Where an explicit `--path` sits relative to the repository.
///
/// Paths must already be normalized (absolute, `..` resolved) by the caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExplicitPathPlacement {
    /// Under the repo's `.heddle/` metadata tree (managed checkouts) — allowed.
    UnderHeddleDir,
    /// Outside the repository entirely — allowed.
    OutsideRepo,
    /// Inside the tracked working tree but not under `.heddle/` — refused on
    /// git-overlay (would show up as nested unsaved work).
    InsideTrackedTree,
}

/// Classify an explicit start path relative to repo root / heddle dir.
///
/// `requested`, `repo_root`, and `heddle_dir` must be absolute normalized
/// paths. Equality and prefix checks are lexical on those normalized forms.
pub fn classify_explicit_path_placement(
    requested: &Path,
    repo_root: &Path,
    heddle_dir: &Path,
) -> ExplicitPathPlacement {
    if requested == heddle_dir || requested.starts_with(heddle_dir) {
        return ExplicitPathPlacement::UnderHeddleDir;
    }
    if requested == repo_root || requested.starts_with(repo_root) {
        return ExplicitPathPlacement::InsideTrackedTree;
    }
    ExplicitPathPlacement::OutsideRepo
}

/// Whether an explicit path placement is allowed for a git-overlay repo.
pub fn explicit_path_allowed_for_git_overlay(placement: ExplicitPathPlacement) -> bool {
    !matches!(placement, ExplicitPathPlacement::InsideTrackedTree)
}

/// Whether path isolation is enforced for this capability.
///
/// Git-overlay refuses checkouts inside the tracked tree; native heddle
/// currently skips this containment guard (matches CLI).
pub fn path_isolation_enforced(is_git_overlay: bool) -> bool {
    is_git_overlay
}

/// Pure path-isolation check for an explicit start path.
///
/// Returns `Ok(())` when the path is allowed, or
/// [`ThreadPathIsolationError::InsideTrackedTree`] when a git-overlay start
/// would land inside the tracked working tree.
pub fn check_explicit_path_isolation(
    is_git_overlay: bool,
    requested: &Path,
    repo_root: &Path,
    heddle_dir: &Path,
) -> Result<(), ThreadPathIsolationError> {
    if !path_isolation_enforced(is_git_overlay) {
        return Ok(());
    }
    let placement = classify_explicit_path_placement(requested, repo_root, heddle_dir);
    if explicit_path_allowed_for_git_overlay(placement) {
        Ok(())
    } else {
        Err(ThreadPathIsolationError::InsideTrackedTree {
            requested: requested.to_path_buf(),
            repo_root: repo_root.to_path_buf(),
        })
    }
}

/// Failures from pure path isolation checks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ThreadPathIsolationError {
    /// Explicit path is inside the tracked worktree of a git-overlay repo.
    InsideTrackedTree {
        requested: PathBuf,
        repo_root: PathBuf,
    },
}

impl std::fmt::Display for ThreadPathIsolationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InsideTrackedTree {
                requested,
                repo_root,
            } => write!(
                f,
                "refusing thread start path '{}' inside repository '{}'",
                requested.display(),
                repo_root.display()
            ),
        }
    }
}

impl std::error::Error for ThreadPathIsolationError {}

// ---------------------------------------------------------------------------
// Active reservation (pure diagnostics)
// ---------------------------------------------------------------------------

/// Whether an active writer reservation blocks starting the same thread name.
///
/// Any live reservation refuses a new start. The reserved path is only used
/// for diagnostics (CLI still surfaces the existing path when present).
pub fn active_reservation_blocks_start(has_active_reservation: bool) -> bool {
    has_active_reservation
}

/// Whether a requested path matches the path held by an active reservation.
///
/// When `requested` is `None`, there is nothing to compare and the result is
/// `true` (no path mismatch to report). When the reservation has no path,
/// the result is `false`.
pub fn active_reservation_path_matches(
    reserved_path: Option<&Path>,
    requested_path: Option<&Path>,
) -> bool {
    match (reserved_path, requested_path) {
        (_, None) => true,
        (None, Some(_)) => false,
        (Some(reserved), Some(requested)) => reserved == requested,
    }
}

// ---------------------------------------------------------------------------
// Workspace mode planning
// ---------------------------------------------------------------------------

/// Config default used when `--workspace auto` and no explicit path forces
/// a bytes-on-disk mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutoWorkspaceDefault {
    Materialized,
    Virtualized,
    Solid,
    /// Config says "auto" again → treat as materialized candidate.
    Auto,
}

/// Plan the concrete [`ThreadMode`] from the caller request and host facts.
///
/// Rules (matching CLI `resolve_thread_mode`):
/// - Explicit workspace modes win as-is (including materialized without
///   reflinks — honesty messaging stays CLI-side).
/// - Auto with an explicit `--path` candidates materialized (navigable
///   checkout), then may downgrade to solid when the filesystem lacks
///   reflinks.
/// - Auto without path uses `auto_default`, then the same reflink downgrade.
pub fn plan_thread_mode(
    workspace: WorkspaceModeRequest,
    has_explicit_path: bool,
    auto_default: AutoWorkspaceDefault,
    supports_reflink: bool,
) -> ThreadMode {
    match workspace {
        WorkspaceModeRequest::Materialized => ThreadMode::Materialized,
        WorkspaceModeRequest::Virtualized => ThreadMode::Virtualized,
        WorkspaceModeRequest::Solid => ThreadMode::Solid,
        WorkspaceModeRequest::Auto => {
            let candidate = if has_explicit_path {
                ThreadMode::Materialized
            } else {
                match auto_default {
                    AutoWorkspaceDefault::Materialized | AutoWorkspaceDefault::Auto => {
                        ThreadMode::Materialized
                    }
                    AutoWorkspaceDefault::Virtualized => ThreadMode::Virtualized,
                    AutoWorkspaceDefault::Solid => ThreadMode::Solid,
                }
            };
            if candidate == ThreadMode::Materialized && !supports_reflink {
                ThreadMode::Solid
            } else {
                candidate
            }
        }
    }
}

/// Whether the planned mode honors an explicit `--path` for checkout placement.
///
/// Virtualized mounts always use a Heddle-managed path so a user-named
/// directory is never shadowed by a kernel mount.
pub fn mode_honors_explicit_path(mode: &ThreadMode) -> bool {
    matches!(mode, ThreadMode::Materialized | ThreadMode::Solid)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_thread_name_accepts_safe_slugs() {
        assert!(validate_thread_name("feature/auth").is_ok());
        assert!(validate_thread_name("v1.2").is_ok());
        assert!(validate_thread_name("team@scope").is_ok());
    }

    #[test]
    fn validate_thread_name_rejects_spaces_and_leading_dash() {
        assert!(matches!(
            validate_thread_name("bad name"),
            Err(ThreadPlanError::InvalidName(_))
        ));
        assert!(matches!(
            validate_thread_name("-flaglike"),
            Err(ThreadPlanError::InvalidName(_))
        ));
        assert!(matches!(
            validate_thread_name(""),
            Err(ThreadPlanError::InvalidName(_))
        ));
    }

    #[test]
    fn plan_thread_create_validates_name() {
        let plan = plan_thread_create(&ThreadCreateOptions {
            name: "scratch".into(),
            ephemeral: true,
            ttl_secs: Some(60),
        })
        .unwrap();
        assert_eq!(plan.name.as_str(), "scratch");
        assert!(plan.ephemeral);
        assert_eq!(plan.ttl_secs, Some(60));

        assert!(
            plan_thread_create(&ThreadCreateOptions {
                name: "has space".into(),
                ephemeral: false,
                ttl_secs: None,
            })
            .is_err()
        );
    }

    #[test]
    fn plan_thread_start_sets_clean_worktree_for_explicit_path() {
        let with_path = plan_thread_start(&ThreadStartOptions {
            name: "a".into(),
            from: None,
            path: Some(PathBuf::from("/tmp/a")),
            workspace: WorkspaceModeRequest::Auto,
            parent_thread: None,
            automated: false,
            task: None,
            shared_target: false,
            hydrate: false,
        })
        .unwrap();
        assert!(with_path.has_explicit_path);
        assert!(with_path.requires_clean_worktree);

        let without = plan_thread_start(&ThreadStartOptions {
            name: "a".into(),
            from: None,
            path: None,
            workspace: WorkspaceModeRequest::Solid,
            parent_thread: None,
            automated: false,
            task: None,
            shared_target: false,
            hydrate: false,
        })
        .unwrap();
        assert!(!without.has_explicit_path);
        assert!(!without.requires_clean_worktree);
    }

    #[test]
    fn select_thread_base_rules() {
        let a = StateId::from_bytes([4; 32]);
        let b = StateId::from_bytes([5; 32]);
        assert_ne!(a, b);

        assert_eq!(
            select_thread_base(None, Some(a)).unwrap(),
            ThreadBaseSelection::Use(a)
        );
        assert_eq!(
            select_thread_base(Some(a), None).unwrap(),
            ThreadBaseSelection::Use(a)
        );
        assert_eq!(
            select_thread_base(Some(a), Some(a)).unwrap(),
            ThreadBaseSelection::Use(a)
        );
        assert_eq!(
            select_thread_base(None, None).unwrap(),
            ThreadBaseSelection::RequireCurrent
        );
        assert_eq!(
            select_thread_base(Some(b), Some(a)).unwrap_err(),
            ThreadBaseError::AnchorMismatch {
                existing: a,
                requested: b,
            }
        );
    }

    #[test]
    fn explicit_path_placement_classifies_containment() {
        let root = Path::new("/repo");
        let heddle = Path::new("/repo/.heddle");
        assert_eq!(
            classify_explicit_path_placement(Path::new("/repo/.heddle/threads/x"), root, heddle),
            ExplicitPathPlacement::UnderHeddleDir
        );
        assert_eq!(
            classify_explicit_path_placement(Path::new("/repo/src"), root, heddle),
            ExplicitPathPlacement::InsideTrackedTree
        );
        assert_eq!(
            classify_explicit_path_placement(Path::new("/tmp/sibling"), root, heddle),
            ExplicitPathPlacement::OutsideRepo
        );
    }

    #[test]
    fn path_isolation_enforced_only_for_git_overlay() {
        let root = Path::new("/repo");
        let heddle = Path::new("/repo/.heddle");
        let inside = Path::new("/repo/nested");
        assert!(
            check_explicit_path_isolation(true, inside, root, heddle).is_err(),
            "git-overlay must refuse tracked-tree paths"
        );
        assert!(
            check_explicit_path_isolation(false, inside, root, heddle).is_ok(),
            "native heddle skips this containment guard"
        );
        assert!(
            check_explicit_path_isolation(true, Path::new("/repo/.heddle/t"), root, heddle).is_ok()
        );
        assert!(check_explicit_path_isolation(true, Path::new("/out"), root, heddle).is_ok());
    }

    #[test]
    fn active_reservation_helpers() {
        assert!(active_reservation_blocks_start(true));
        assert!(!active_reservation_blocks_start(false));
        assert!(active_reservation_path_matches(
            Some(Path::new("/a")),
            Some(Path::new("/a"))
        ));
        assert!(!active_reservation_path_matches(
            Some(Path::new("/a")),
            Some(Path::new("/b"))
        ));
        assert!(!active_reservation_path_matches(
            None,
            Some(Path::new("/a"))
        ));
        assert!(active_reservation_path_matches(Some(Path::new("/a")), None));
    }

    #[test]
    fn plan_thread_mode_auto_and_explicit() {
        assert_eq!(
            plan_thread_mode(
                WorkspaceModeRequest::Solid,
                false,
                AutoWorkspaceDefault::Virtualized,
                true
            ),
            ThreadMode::Solid
        );
        assert_eq!(
            plan_thread_mode(
                WorkspaceModeRequest::Auto,
                true,
                AutoWorkspaceDefault::Virtualized,
                true
            ),
            ThreadMode::Materialized,
            "explicit path pulls Auto toward navigable materialized"
        );
        assert_eq!(
            plan_thread_mode(
                WorkspaceModeRequest::Auto,
                true,
                AutoWorkspaceDefault::Virtualized,
                false
            ),
            ThreadMode::Solid,
            "materialized auto candidate downgrades without reflink"
        );
        assert_eq!(
            plan_thread_mode(
                WorkspaceModeRequest::Auto,
                false,
                AutoWorkspaceDefault::Virtualized,
                true
            ),
            ThreadMode::Virtualized
        );
        assert_eq!(
            plan_thread_mode(
                WorkspaceModeRequest::Materialized,
                false,
                AutoWorkspaceDefault::Solid,
                false
            ),
            ThreadMode::Materialized,
            "explicit materialized is not silently downgraded"
        );
        assert!(mode_honors_explicit_path(&ThreadMode::Solid));
        assert!(!mode_honors_explicit_path(&ThreadMode::Virtualized));
    }
}
