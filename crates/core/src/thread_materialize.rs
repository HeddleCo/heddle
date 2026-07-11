// SPDX-License-Identifier: Apache-2.0
//! Pure thread materialization planning.
//!
//! Owns decision logic for the `heddle start` / `heddle thread start`
//! materialization path once a concrete [`ThreadMode`] is known:
//! - checkout path layout (explicit `--path` vs managed default)
//! - ordered materialize step sequence (create dir, copy tree, manifest, …)
//! - reflink vs full-copy policy for bytes-on-disk checkouts
//! - cargo `--shared-target` redirect and advisory flags
//!
//! Filesystem clonefile/copy, mount RPCs, cargo-config writes, and the
//! `start_atomic` transaction stay CLI-owned. Callers resolve host/config
//! facts first, then invoke these helpers.

use std::path::PathBuf;

use repo::ThreadMode;

// ---------------------------------------------------------------------------
// Path layout
// ---------------------------------------------------------------------------

/// Pure plan for where a new thread checkout should land.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckoutPathPlan {
    /// Absolute or caller-resolved path the start path should use.
    pub path: PathBuf,
    /// True when the planned path is the caller's explicit `--path`.
    ///
    /// Virtualized mounts always use the managed default so a user-named
    /// directory is never shadowed by a kernel mount; this is `false` even
    /// if the caller passed `--path`.
    pub from_explicit_path: bool,
}

/// Plan the checkout path for a start once mode is known.
///
/// Rules (matching CLI `start_thread`):
/// - [`ThreadMode::Virtualized`] always uses `managed_default` (ignores
///   explicit `--path`).
/// - [`ThreadMode::Materialized`] / [`ThreadMode::Solid`] honor
///   `explicit_path` when present, else `managed_default`.
///
/// `managed_default` is the fully-built
/// `.heddle/threads/<encoded>/<repo-name>` path from
/// `repo.managed_checkout_path(name)` (or equivalent).
pub fn plan_checkout_path(
    mode: &ThreadMode,
    explicit_path: Option<PathBuf>,
    managed_default: PathBuf,
) -> CheckoutPathPlan {
    match mode {
        ThreadMode::Virtualized => CheckoutPathPlan {
            path: managed_default,
            from_explicit_path: false,
        },
        ThreadMode::Materialized | ThreadMode::Solid => match explicit_path {
            Some(path) => CheckoutPathPlan {
                path,
                from_explicit_path: true,
            },
            None => CheckoutPathPlan {
                path: managed_default,
                from_explicit_path: false,
            },
        },
    }
}

// ---------------------------------------------------------------------------
// Copy policy (reflink vs full copy)
// ---------------------------------------------------------------------------

/// How a bytes-on-disk checkout should populate the tree.
///
/// The CLI / materializer owns the actual `clonefile` / `FICLONE` / copy
/// calls. This only captures the pure policy intent from mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckoutCopyPolicy {
    /// Prefer filesystem reflink/clonefile; fall back to full copy when the
    /// host rejects it. Used for [`ThreadMode::Materialized`].
    PreferReflink,
    /// Always perform a full byte copy. Used for [`ThreadMode::Solid`].
    FullCopy,
    /// No tree copy — virtualized mounts do not materialize bytes.
    None,
}

/// Pure reflink-vs-copy policy from the planned thread mode.
pub fn plan_checkout_copy_policy(mode: &ThreadMode) -> CheckoutCopyPolicy {
    match mode {
        ThreadMode::Materialized => CheckoutCopyPolicy::PreferReflink,
        ThreadMode::Solid => CheckoutCopyPolicy::FullCopy,
        ThreadMode::Virtualized => CheckoutCopyPolicy::None,
    }
}

/// Whether start should warn that an explicit `--workspace materialized`
/// will fall back to per-file copies on this host.
///
/// Auto-mode silently downgrades to solid via [`crate::plan_thread_mode`];
/// an explicit materialized request is honored but the user is told disk
/// usage will match solid.
pub fn should_warn_materialized_without_reflink(
    explicit_materialized_request: bool,
    supports_reflink: bool,
) -> bool {
    explicit_materialized_request && !supports_reflink
}

// ---------------------------------------------------------------------------
// Shared-target redirect / advisory
// ---------------------------------------------------------------------------

/// Active heavy (solid/materialized) threads at or above which a
/// `--shared-target` heads-up is emitted when starting another heavy
/// thread in a Rust workspace without the flag.
pub const ADVISORY_ACTIVE_HEAVY_THREAD_THRESHOLD: usize = 1;

/// Pure decision for whether `--shared-target` should write a cargo
/// `target-dir` redirect after checkout materialize.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SharedTargetRedirectDecision {
    /// Apply `.cargo/config.toml` redirect after materialize.
    Apply,
    /// Flag set but workspace has no top-level `Cargo.toml` — no-op (debug).
    SkipNonRustWorkspace,
    /// Flag not set, or mode is not bytes-on-disk.
    NotApplicable,
}

/// Plan the `--shared-target` cargo-config redirect from pure inputs.
///
/// `is_rust_workspace` is a caller-supplied fact (typically presence of a
/// top-level `Cargo.toml`). Directory creation and config writes remain
/// CLI-owned.
pub fn plan_shared_target_redirect(
    requested: bool,
    mode: &ThreadMode,
    is_rust_workspace: bool,
) -> SharedTargetRedirectDecision {
    if !requested || !mode_is_bytes_on_disk(mode) {
        return SharedTargetRedirectDecision::NotApplicable;
    }
    if is_rust_workspace {
        SharedTargetRedirectDecision::Apply
    } else {
        SharedTargetRedirectDecision::SkipNonRustWorkspace
    }
}

/// Whether [`plan_shared_target_redirect`] selected an apply.
pub fn shared_target_redirect_applies(decision: SharedTargetRedirectDecision) -> bool {
    matches!(decision, SharedTargetRedirectDecision::Apply)
}

/// Whether the workspace looks busy enough for a `--shared-target` heads-up
/// (Rust + active heavy-thread population), independent of the start flags.
///
/// Callers typically supply
/// `is_rust_workspace && active_heavy_thread_count >= threshold` after probing
/// the repo; this keeps the threshold comparison pure and unit-testable.
pub fn shared_target_workspace_is_busy(
    is_rust_workspace: bool,
    active_heavy_thread_count: usize,
) -> bool {
    is_rust_workspace && active_heavy_thread_count >= ADVISORY_ACTIVE_HEAVY_THREAD_THRESHOLD
}

/// Whether start should print the `--shared-target` heads-up advisory.
///
/// Heuristic (matching CLI): flag not requested, mode is solid/materialized,
/// and the workspace is busy ([`shared_target_workspace_is_busy`]).
///
/// `workspace_is_busy` must reflect the *pre-start* population (before the
/// new thread is recorded). Callers may pass a precomputed I/O oracle
/// (e.g. CLI `should_advise_shared_target(repo)`) as `workspace_is_busy`.
pub fn should_advise_shared_target(
    shared_target_requested: bool,
    mode: &ThreadMode,
    workspace_is_busy: bool,
) -> bool {
    !shared_target_requested && mode_is_bytes_on_disk(mode) && workspace_is_busy
}

// ---------------------------------------------------------------------------
// Hydrate + mode predicates
// ---------------------------------------------------------------------------

/// Whether a mode materializes a real on-disk checkout (vs a virtual mount).
pub fn mode_is_bytes_on_disk(mode: &ThreadMode) -> bool {
    matches!(mode, ThreadMode::Solid | ThreadMode::Materialized)
}

/// Whether `--hydrate` should run for this mode.
///
/// Hydrate only applies to solid/materialized checkouts.
pub fn plan_hydrate(hydrate_requested: bool, mode: &ThreadMode) -> bool {
    hydrate_requested && mode_is_bytes_on_disk(mode)
}

/// Whether a materialized-thread manifest sidecar should be written.
///
/// Only [`ThreadMode::Materialized`] records the per-thread manifest.
pub fn plan_write_manifest(mode: &ThreadMode) -> bool {
    matches!(mode, ThreadMode::Materialized)
}

// ---------------------------------------------------------------------------
// Materialize step sequence
// ---------------------------------------------------------------------------

/// One step in the atomic start materialization sequence.
///
/// Order matches `start_atomic::StartThread::apply`. Execution (FS, refs,
/// mounts) remains CLI-owned; this is the pure checklist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MaterializeStep {
    /// Create the materialization target directory (transaction first step).
    CreateTargetDir,
    /// CAS-write the thread ref (+ staged `ThreadCreate` when brand-new).
    WriteThreadRef,
    /// Materialize `.heddle` metadata + worktree bytes under the target.
    MaterializeCheckout { copy_policy: CheckoutCopyPolicy },
    /// Write `.heddle/threads/<name>/manifest.toml` (materialized only).
    WriteManifest,
    /// Write `.cargo/config.toml` shared `target-dir` redirect.
    WriteCargoConfigRedirect,
    /// Symlink ignored dirs from the parent (`--hydrate`).
    HydrateIgnoredDirs,
    /// Establish the FUSE/virtual mount (virtualized only).
    EstablishVirtualizedMount,
    /// Converge the ThreadManager record.
    WriteThreadRecord,
}

/// Structured pure plan for the start materialization path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadMaterializePlan {
    /// Ordered steps for the start transaction apply path.
    pub steps: Vec<MaterializeStep>,
    pub copy_policy: CheckoutCopyPolicy,
    pub write_manifest: bool,
    pub apply_shared_target: bool,
    pub hydrate: bool,
    pub virtualized_mount: bool,
}

/// Build the pure materialize plan from mode + post-decision flags.
///
/// `apply_shared_target` should already be the result of
/// [`shared_target_redirect_applies`] (or equivalent). `hydrate_requested`
/// is the raw CLI flag; this function gates it on mode via [`plan_hydrate`].
pub fn plan_thread_materialize(
    mode: &ThreadMode,
    apply_shared_target: bool,
    hydrate_requested: bool,
) -> ThreadMaterializePlan {
    let copy_policy = plan_checkout_copy_policy(mode);
    let write_manifest = plan_write_manifest(mode);
    let hydrate = plan_hydrate(hydrate_requested, mode);
    let virtualized_mount = matches!(mode, ThreadMode::Virtualized);
    // Shared-target only applies to bytes-on-disk modes; ignore a stale true
    // for virtualized so the step list stays honest.
    let apply_shared_target = apply_shared_target && mode_is_bytes_on_disk(mode);

    let mut steps = vec![
        MaterializeStep::CreateTargetDir,
        MaterializeStep::WriteThreadRef,
    ];
    match mode {
        ThreadMode::Solid | ThreadMode::Materialized => {
            steps.push(MaterializeStep::MaterializeCheckout { copy_policy });
            if write_manifest {
                steps.push(MaterializeStep::WriteManifest);
            }
            if apply_shared_target {
                steps.push(MaterializeStep::WriteCargoConfigRedirect);
            }
            if hydrate {
                steps.push(MaterializeStep::HydrateIgnoredDirs);
            }
        }
        ThreadMode::Virtualized => {
            steps.push(MaterializeStep::EstablishVirtualizedMount);
        }
    }
    steps.push(MaterializeStep::WriteThreadRecord);

    ThreadMaterializePlan {
        steps,
        copy_policy,
        write_manifest,
        apply_shared_target,
        hydrate,
        virtualized_mount,
    }
}

/// Convenience: ordered step list only.
pub fn plan_materialize_steps(
    mode: &ThreadMode,
    apply_shared_target: bool,
    hydrate_requested: bool,
) -> Vec<MaterializeStep> {
    plan_thread_materialize(mode, apply_shared_target, hydrate_requested).steps
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_checkout_path_honors_explicit_for_bytes_modes() {
        let managed = PathBuf::from("/repo/.heddle/threads/a/repo");
        let explicit = PathBuf::from("/tmp/work");

        let solid = plan_checkout_path(&ThreadMode::Solid, Some(explicit.clone()), managed.clone());
        assert_eq!(solid.path, explicit);
        assert!(solid.from_explicit_path);

        let materialized = plan_checkout_path(&ThreadMode::Materialized, None, managed.clone());
        assert_eq!(materialized.path, managed);
        assert!(!materialized.from_explicit_path);
    }

    #[test]
    fn plan_checkout_path_virtualized_ignores_explicit() {
        let managed = PathBuf::from("/repo/.heddle/threads/v/repo");
        let plan = plan_checkout_path(
            &ThreadMode::Virtualized,
            Some(PathBuf::from("/tmp/user-named")),
            managed.clone(),
        );
        assert_eq!(plan.path, managed);
        assert!(!plan.from_explicit_path);
    }

    #[test]
    fn copy_policy_matches_mode() {
        assert_eq!(
            plan_checkout_copy_policy(&ThreadMode::Materialized),
            CheckoutCopyPolicy::PreferReflink
        );
        assert_eq!(
            plan_checkout_copy_policy(&ThreadMode::Solid),
            CheckoutCopyPolicy::FullCopy
        );
        assert_eq!(
            plan_checkout_copy_policy(&ThreadMode::Virtualized),
            CheckoutCopyPolicy::None
        );
    }

    #[test]
    fn warn_only_for_explicit_materialized_without_reflink() {
        assert!(should_warn_materialized_without_reflink(true, false));
        assert!(!should_warn_materialized_without_reflink(true, true));
        assert!(!should_warn_materialized_without_reflink(false, false));
    }

    #[test]
    fn shared_target_redirect_decisions() {
        assert_eq!(
            plan_shared_target_redirect(true, &ThreadMode::Materialized, true),
            SharedTargetRedirectDecision::Apply
        );
        assert_eq!(
            plan_shared_target_redirect(true, &ThreadMode::Solid, false),
            SharedTargetRedirectDecision::SkipNonRustWorkspace
        );
        assert_eq!(
            plan_shared_target_redirect(true, &ThreadMode::Virtualized, true),
            SharedTargetRedirectDecision::NotApplicable
        );
        assert_eq!(
            plan_shared_target_redirect(false, &ThreadMode::Materialized, true),
            SharedTargetRedirectDecision::NotApplicable
        );
        assert!(shared_target_redirect_applies(
            SharedTargetRedirectDecision::Apply
        ));
        assert!(!shared_target_redirect_applies(
            SharedTargetRedirectDecision::SkipNonRustWorkspace
        ));
    }

    #[test]
    fn shared_target_advisory_requires_busy_heavy_without_flag() {
        assert!(shared_target_workspace_is_busy(true, 1));
        assert!(!shared_target_workspace_is_busy(true, 0));
        assert!(!shared_target_workspace_is_busy(false, 5));

        assert!(should_advise_shared_target(
            false,
            &ThreadMode::Materialized,
            true
        ));
        assert!(should_advise_shared_target(false, &ThreadMode::Solid, true));
        assert!(!should_advise_shared_target(
            true,
            &ThreadMode::Materialized,
            true
        ));
        assert!(!should_advise_shared_target(
            false,
            &ThreadMode::Virtualized,
            true
        ));
        assert!(!should_advise_shared_target(
            false,
            &ThreadMode::Materialized,
            false
        ));
    }

    #[test]
    fn plan_hydrate_and_manifest_gate_on_mode() {
        assert!(plan_hydrate(true, &ThreadMode::Solid));
        assert!(plan_hydrate(true, &ThreadMode::Materialized));
        assert!(!plan_hydrate(true, &ThreadMode::Virtualized));
        assert!(!plan_hydrate(false, &ThreadMode::Solid));

        assert!(plan_write_manifest(&ThreadMode::Materialized));
        assert!(!plan_write_manifest(&ThreadMode::Solid));
        assert!(!plan_write_manifest(&ThreadMode::Virtualized));
    }

    #[test]
    fn materialize_steps_materialized_with_shared_and_hydrate() {
        let plan = plan_thread_materialize(&ThreadMode::Materialized, true, true);
        assert_eq!(plan.copy_policy, CheckoutCopyPolicy::PreferReflink);
        assert!(plan.write_manifest);
        assert!(plan.apply_shared_target);
        assert!(plan.hydrate);
        assert!(!plan.virtualized_mount);
        assert_eq!(
            plan.steps,
            vec![
                MaterializeStep::CreateTargetDir,
                MaterializeStep::WriteThreadRef,
                MaterializeStep::MaterializeCheckout {
                    copy_policy: CheckoutCopyPolicy::PreferReflink
                },
                MaterializeStep::WriteManifest,
                MaterializeStep::WriteCargoConfigRedirect,
                MaterializeStep::HydrateIgnoredDirs,
                MaterializeStep::WriteThreadRecord,
            ]
        );
    }

    #[test]
    fn materialize_steps_solid_minimal() {
        let steps = plan_materialize_steps(&ThreadMode::Solid, false, false);
        assert_eq!(
            steps,
            vec![
                MaterializeStep::CreateTargetDir,
                MaterializeStep::WriteThreadRef,
                MaterializeStep::MaterializeCheckout {
                    copy_policy: CheckoutCopyPolicy::FullCopy
                },
                MaterializeStep::WriteThreadRecord,
            ]
        );
    }

    #[test]
    fn materialize_steps_virtualized() {
        let plan = plan_thread_materialize(&ThreadMode::Virtualized, true, true);
        assert_eq!(plan.copy_policy, CheckoutCopyPolicy::None);
        assert!(!plan.write_manifest);
        assert!(
            !plan.apply_shared_target,
            "virtualized ignores shared-target"
        );
        assert!(!plan.hydrate, "virtualized ignores hydrate");
        assert!(plan.virtualized_mount);
        assert_eq!(
            plan.steps,
            vec![
                MaterializeStep::CreateTargetDir,
                MaterializeStep::WriteThreadRef,
                MaterializeStep::EstablishVirtualizedMount,
                MaterializeStep::WriteThreadRecord,
            ]
        );
    }

    #[test]
    fn mode_is_bytes_on_disk_predicate() {
        assert!(mode_is_bytes_on_disk(&ThreadMode::Solid));
        assert!(mode_is_bytes_on_disk(&ThreadMode::Materialized));
        assert!(!mode_is_bytes_on_disk(&ThreadMode::Virtualized));
    }
}
