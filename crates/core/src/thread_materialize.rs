// SPDX-License-Identifier: Apache-2.0
//! Pure thread materialization planning.
//!
//! Owns decision logic for the `heddle start` / `heddle thread start`
//! materialization path once a concrete [`ThreadMode`] is known:
//! - checkout path layout (explicit `--path` vs managed default)
//! - ordered materialize step sequence (create dir, copy tree, manifest, …)
//! - typed start-transaction effect kinds and reverse cleanup lists
//! - target-dir claim → checkout / self-created-dir rewind actions
//! - path safety vs `.heddle/threads` layout and relative-path normalization
//! - empty-dir adoption / claim-intent pure validators
//! - effect staging preconditions (claim established, shared-target present)
//! - classification of mid-apply `anyhow` failures into [`HeddleError`]
//! - reflink vs full-copy policy for bytes-on-disk checkouts
//! - cargo `--shared-target` redirect and advisory flags
//!
//! Filesystem clonefile/copy, mount RPCs, cargo-config writes, and the
//! `start_atomic` transaction stay CLI-owned. Callers resolve host/config
//! facts first, then invoke these helpers.

use std::path::{Component, Path, PathBuf};

use objects::HeddleError;
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

// ---------------------------------------------------------------------------
// Transaction effect kinds + start transaction plan
// ---------------------------------------------------------------------------

/// Payload-free kind of a durable effect the start transaction can stage.
///
/// Mirrors [`MaterializeStep`] without the copy-policy payload so applied-
/// effect lists and cleanup planners stay `Copy`. Execution remains CLI-owned
/// (`start_atomic::StartThread::apply`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartEffectKind {
    CreateTargetDir,
    WriteThreadRef,
    MaterializeCheckout,
    WriteManifest,
    WriteCargoConfigRedirect,
    HydrateIgnoredDirs,
    EstablishVirtualizedMount,
    WriteThreadRecord,
}

impl MaterializeStep {
    /// Strip the copy-policy payload to the pure effect kind.
    pub fn effect_kind(&self) -> StartEffectKind {
        match self {
            Self::CreateTargetDir => StartEffectKind::CreateTargetDir,
            Self::WriteThreadRef => StartEffectKind::WriteThreadRef,
            Self::MaterializeCheckout { .. } => StartEffectKind::MaterializeCheckout,
            Self::WriteManifest => StartEffectKind::WriteManifest,
            Self::WriteCargoConfigRedirect => StartEffectKind::WriteCargoConfigRedirect,
            Self::HydrateIgnoredDirs => StartEffectKind::HydrateIgnoredDirs,
            Self::EstablishVirtualizedMount => StartEffectKind::EstablishVirtualizedMount,
            Self::WriteThreadRecord => StartEffectKind::WriteThreadRecord,
        }
    }
}

/// Typed start-transaction plan: ordered effect kinds + mode flags.
///
/// Built from the same inputs as [`plan_thread_materialize`]; preferred when
/// callers need effect-kind lists (cleanup, ledger assertions) rather than
/// the payload-bearing [`MaterializeStep`] sequence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartTransactionPlan {
    /// Forward-apply order of durable effects.
    pub effects: Vec<StartEffectKind>,
    pub copy_policy: CheckoutCopyPolicy,
    pub write_manifest: bool,
    pub apply_shared_target: bool,
    pub hydrate: bool,
    pub virtualized_mount: bool,
}

/// Build the typed start-transaction plan from mode + post-decision flags.
pub fn plan_start_transaction(
    mode: &ThreadMode,
    apply_shared_target: bool,
    hydrate_requested: bool,
) -> StartTransactionPlan {
    let materialize = plan_thread_materialize(mode, apply_shared_target, hydrate_requested);
    StartTransactionPlan {
        effects: materialize
            .steps
            .iter()
            .map(MaterializeStep::effect_kind)
            .collect(),
        copy_policy: materialize.copy_policy,
        write_manifest: materialize.write_manifest,
        apply_shared_target: materialize.apply_shared_target,
        hydrate: materialize.hydrate,
        virtualized_mount: materialize.virtualized_mount,
    }
}

// ---------------------------------------------------------------------------
// Target-dir claim → pure rewind actions
// ---------------------------------------------------------------------------

/// What the target-dir claim established about the worktree leaf (pure).
///
/// CLI `start_atomic` captures an open directory handle alongside this kind;
/// rewinds and writers key on the kind, never a stale plan-time bool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetDirClaimKind {
    /// This start created the leaf as a fresh empty directory.
    Created,
    /// This start adopted a pre-existing real empty directory.
    AdoptedEmpty,
}

/// Pure checkout-dir rewind action for a settled (or absent) target claim.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckoutRewindPlan {
    /// Clear contents, then remove the directory if it still occupies the path.
    ClearAndRemoveDir,
    /// Clear only the contents this start wrote; leave the directory itself.
    ClearContentsOnly,
    /// Claim never established — touch nothing.
    TouchNothing,
}

/// Plan how the checkout rewind treats the target leaf for a claim outcome.
///
/// Rules (matching CLI `rewind_checkout`):
/// - [`TargetDirClaimKind::Created`] → clear + remove dir
/// - [`TargetDirClaimKind::AdoptedEmpty`] → clear contents only
/// - `None` → touch nothing (refused/unestablished leaf)
pub fn plan_checkout_rewind(claim: Option<TargetDirClaimKind>) -> CheckoutRewindPlan {
    match claim {
        Some(TargetDirClaimKind::Created) => CheckoutRewindPlan::ClearAndRemoveDir,
        Some(TargetDirClaimKind::AdoptedEmpty) => CheckoutRewindPlan::ClearContentsOnly,
        None => CheckoutRewindPlan::TouchNothing,
    }
}

/// Pure self-created target-dir removal action (the create-step inverse).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelfCreatedDirRewindPlan {
    /// Remove the leaf only if the claim identity still occupies the path.
    RemoveIfStillAtPath,
    /// Adopted or unestablished — never remove the leaf.
    TouchNothing,
}

/// Plan the create-step inverse for a settled (or absent) target claim.
///
/// Only a [`TargetDirClaimKind::Created`] claim removes the leaf; adopted
/// dirs and `None` are left untouched (matching CLI `remove_self_created_dir`).
pub fn plan_self_created_dir_rewind(claim: Option<TargetDirClaimKind>) -> SelfCreatedDirRewindPlan {
    match claim {
        Some(TargetDirClaimKind::Created) => SelfCreatedDirRewindPlan::RemoveIfStillAtPath,
        Some(TargetDirClaimKind::AdoptedEmpty) | None => SelfCreatedDirRewindPlan::TouchNothing,
    }
}

// ---------------------------------------------------------------------------
// Path safety (threads root layout + relative remainder normalization)
// ---------------------------------------------------------------------------

/// Lexical containment: `path` is `root` or a strict descendant.
///
/// Paths must already be absolute and free of `..` (caller-normalized). Uses
/// [`Path::starts_with`], so a root of `/a` does not match `/ab`.
pub fn path_is_under_or_equal(path: &Path, root: &Path) -> bool {
    path == root || path.starts_with(root)
}

/// Lexical strict descent: under `root` but not equal to it.
pub fn path_is_strict_descendant(path: &Path, root: &Path) -> bool {
    path != root && path.starts_with(root)
}

/// Pure classification of a candidate checkout path vs heddle layout.
///
/// Mirrors the lexical half of CLI `validate_worktree_target` (heddle#572):
/// managed checkouts live under `.heddle/threads/<seg>/<leaf>`, never on the
/// threads root or bare per-thread dir, and never on other heddle storage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThreadsRootPathClass {
    /// Under `.heddle/threads/` as a per-thread checkout slot (allowed).
    ManagedCheckoutSlot,
    /// Exactly `.heddle/threads` — forbidden.
    ThreadsRoot,
    /// Direct child of threads root (`threads/<seg>` without leaf) — forbidden.
    BareThreadDir,
    /// Under `.heddle/` but outside `threads/` — forbidden storage.
    HeddleStorage,
    /// Outside `.heddle/` entirely (user `--path` or external) — allowed here.
    OutsideHeddle,
}

/// Classify `path` relative to `heddle_dir` / `threads_root`.
///
/// Inputs must be absolute normalized paths (no `..`). Nested-in-existing-
/// thread checks need reserved regions from the caller; see
/// [`path_is_nested_in_reserved_region`].
pub fn classify_path_vs_threads_root(
    path: &Path,
    heddle_dir: &Path,
    threads_root: &Path,
) -> ThreadsRootPathClass {
    if path_is_under_or_equal(path, threads_root) {
        if path == threads_root {
            return ThreadsRootPathClass::ThreadsRoot;
        }
        if path.parent() == Some(threads_root) {
            return ThreadsRootPathClass::BareThreadDir;
        }
        return ThreadsRootPathClass::ManagedCheckoutSlot;
    }
    if path_is_under_or_equal(path, heddle_dir) {
        return ThreadsRootPathClass::HeddleStorage;
    }
    ThreadsRootPathClass::OutsideHeddle
}

/// Whether [`classify_path_vs_threads_root`] accepts the layout class.
///
/// Nested-in-existing-thread is a separate pure check
/// ([`path_is_nested_in_reserved_region`]).
pub fn threads_root_path_layout_allowed(class: ThreadsRootPathClass) -> bool {
    matches!(
        class,
        ThreadsRootPathClass::ManagedCheckoutSlot | ThreadsRootPathClass::OutsideHeddle
    )
}

/// Pure layout + reserved-region validation for a worktree target under the
/// threads root / heddle storage policy.
///
/// `reserved_regions` is `(region_root, exempt_exact_path)` pairs: candidate is
/// nested when it starts with `region_root` unless it equals `exempt_exact`
/// (self-thread re-materialize exemption). Callers enumerate durable thread
/// records; this stays FS-free.
pub fn validate_threads_root_path_safety(
    path: &Path,
    heddle_dir: &Path,
    threads_root: &Path,
    reserved_regions: &[(PathBuf, Option<PathBuf>)],
) -> Result<(), ThreadsRootPathSafetyError> {
    let class = classify_path_vs_threads_root(path, heddle_dir, threads_root);
    match class {
        ThreadsRootPathClass::ThreadsRoot => Err(ThreadsRootPathSafetyError::IsThreadsRoot {
            path: path.to_path_buf(),
        }),
        ThreadsRootPathClass::BareThreadDir => Err(ThreadsRootPathSafetyError::IsBareThreadDir {
            path: path.to_path_buf(),
        }),
        ThreadsRootPathClass::HeddleStorage => Err(ThreadsRootPathSafetyError::IsHeddleStorage {
            path: path.to_path_buf(),
        }),
        ThreadsRootPathClass::OutsideHeddle => Ok(()),
        ThreadsRootPathClass::ManagedCheckoutSlot => {
            for (region, exempt) in reserved_regions {
                if path_is_nested_in_reserved_region(path, region, exempt.as_deref()) {
                    return Err(ThreadsRootPathSafetyError::NestedInReserved {
                        path: path.to_path_buf(),
                        reserved: region.clone(),
                    });
                }
            }
            Ok(())
        }
    }
}

/// Failures from pure threads-root / heddle-storage path safety checks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ThreadsRootPathSafetyError {
    IsThreadsRoot { path: PathBuf },
    IsBareThreadDir { path: PathBuf },
    IsHeddleStorage { path: PathBuf },
    NestedInReserved { path: PathBuf, reserved: PathBuf },
}

impl std::fmt::Display for ThreadsRootPathSafetyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::IsThreadsRoot { path } => write!(
                f,
                "worktree target '{}' is the threads root (not a per-thread leaf)",
                path.display()
            ),
            Self::IsBareThreadDir { path } => write!(
                f,
                "worktree target '{}' is a bare thread dir (checkout leaf required)",
                path.display()
            ),
            Self::IsHeddleStorage { path } => write!(
                f,
                "worktree target '{}' is under heddle storage (outside threads/)",
                path.display()
            ),
            Self::NestedInReserved { path, reserved } => write!(
                f,
                "worktree target '{}' is nested inside reserved region '{}'",
                path.display(),
                reserved.display()
            ),
        }
    }
}

impl std::error::Error for ThreadsRootPathSafetyError {}

/// Whether `candidate` falls inside `reserved_dir`, with optional exact exempt.
///
/// Matching CLI `is_inside_existing_thread` for one reserved region: under the
/// region is nested, unless `candidate == exempt_exact` (self-thread checkout).
pub fn path_is_nested_in_reserved_region(
    candidate: &Path,
    reserved_dir: &Path,
    exempt_exact: Option<&Path>,
) -> bool {
    if !candidate.starts_with(reserved_dir) {
        return false;
    }
    if let Some(exempt) = exempt_exact
        && candidate == exempt
    {
        return false;
    }
    true
}

/// Whether a path component sequence is free of `..` escape segments.
pub fn path_components_are_safe(path: &Path) -> bool {
    !path.components().any(|c| matches!(c, Component::ParentDir))
}

/// Failures from pure relative-remainder normalization.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelativePathNormalizeError {
    /// Remainder contained `..`, a root, or a prefix component.
    UnsafeComponent,
}

impl std::fmt::Display for RelativePathNormalizeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnsafeComponent => {
                write!(f, "path remainder contains an unsafe path component")
            }
        }
    }
}

impl std::error::Error for RelativePathNormalizeError {}

/// Append unresolved remainder components onto a resolved base.
///
/// Matches CLI `canonicalize_existing_ancestor` remainder handling:
/// - `Normal` → push
/// - `CurDir` → ignore
/// - `ParentDir` / `Prefix` / `RootDir` → refuse (escape / re-root)
///
/// Callers supply the already-canonical existing ancestor as `base` and the
/// non-existing tail as `remainder` (from `path.strip_prefix(ancestor)`).
pub fn append_safe_relative_components(
    mut base: PathBuf,
    remainder: &Path,
) -> Result<PathBuf, RelativePathNormalizeError> {
    for component in remainder.components() {
        match component {
            Component::Normal(part) => base.push(part),
            Component::CurDir => {}
            Component::ParentDir | Component::Prefix(_) | Component::RootDir => {
                return Err(RelativePathNormalizeError::UnsafeComponent);
            }
        }
    }
    Ok(base)
}

// ---------------------------------------------------------------------------
// Empty-dir adoption / create-intent pure validators
// ---------------------------------------------------------------------------

/// Plan-time intent for the create-target-dir step (before FS create/adopt).
///
/// Derived solely from `plan_worktree_target`'s `target_dir_created` bool; the
/// runtime claim may still land as AdoptedEmpty if a concurrent create races
/// the transaction ([`CreateDirAttempt::AlreadyExists`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetDirCreateIntent {
    /// Leaf was absent at plan time — attempt `create_dir`, adopt on race.
    AttemptCreate,
    /// Leaf was a pre-existing empty dir — adopt only, never create/remove.
    AdoptOnly,
}

/// Pure create-intent from the plan-time `target_dir_created` observation.
pub fn plan_target_dir_create_intent(plan_created: bool) -> TargetDirCreateIntent {
    if plan_created {
        TargetDirCreateIntent::AttemptCreate
    } else {
        TargetDirCreateIntent::AdoptOnly
    }
}

/// Outcome of a pure-facing `create_dir` attempt the caller reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CreateDirAttempt {
    /// `create_dir` returned Ok — this start owns the leaf.
    Created,
    /// Leaf already exists — must re-validate and possibly adopt empty.
    AlreadyExists,
}

/// Observed shape of the worktree leaf (caller supplies FS facts).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetLeafShape {
    /// Path does not exist.
    Absent,
    /// Real empty directory (non-symlink).
    EmptyDirectory,
    /// Real non-empty directory.
    NonEmptyDirectory,
    /// Symlink leaf.
    Symlink,
    /// Regular file or other non-directory.
    NotDirectory,
}

/// Why a leaf cannot be adopted as an empty directory claim.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetLeafRefusal {
    DoesNotExist,
    IsSymlink,
    NotDirectory,
    NotEmpty,
}

impl TargetLeafRefusal {
    /// Reason fragment for CLI `target_dir_shape_refusal` messages.
    pub fn as_reason_str(self) -> &'static str {
        match self {
            Self::DoesNotExist => "does not exist",
            Self::IsSymlink => "is a symlink",
            Self::NotDirectory => "is not a directory",
            Self::NotEmpty => "is not empty",
        }
    }
}

/// Pure: classify metadata facts into a leaf shape (emptiness supplied when dir).
///
/// `is_empty` is only consulted when the leaf is a real directory; callers may
/// pass `false` when they have not inspected children for non-dirs.
pub fn classify_target_leaf_shape(
    exists: bool,
    is_symlink: bool,
    is_dir: bool,
    is_empty: bool,
) -> TargetLeafShape {
    if !exists {
        return TargetLeafShape::Absent;
    }
    if is_symlink {
        return TargetLeafShape::Symlink;
    }
    if !is_dir {
        return TargetLeafShape::NotDirectory;
    }
    if is_empty {
        TargetLeafShape::EmptyDirectory
    } else {
        TargetLeafShape::NonEmptyDirectory
    }
}

/// Pure empty-dir adoption gate: only [`TargetLeafShape::EmptyDirectory`] ok.
pub fn validate_empty_dir_adoption(shape: TargetLeafShape) -> Result<(), TargetLeafRefusal> {
    match shape {
        TargetLeafShape::EmptyDirectory => Ok(()),
        TargetLeafShape::Absent => Err(TargetLeafRefusal::DoesNotExist),
        TargetLeafShape::Symlink => Err(TargetLeafRefusal::IsSymlink),
        TargetLeafShape::NotDirectory => Err(TargetLeafRefusal::NotDirectory),
        TargetLeafShape::NonEmptyDirectory => Err(TargetLeafRefusal::NotEmpty),
    }
}

/// Pure claim kind after a successful create or a successful empty-dir adopt.
pub fn claim_kind_for_create_attempt(attempt: CreateDirAttempt) -> Option<TargetDirClaimKind> {
    match attempt {
        CreateDirAttempt::Created => Some(TargetDirClaimKind::Created),
        // AlreadyExists still requires adoption validation; kind is decided then.
        CreateDirAttempt::AlreadyExists => None,
    }
}

/// Pure claim kind once empty-dir adoption is validated.
pub fn claim_kind_after_empty_dir_adoption() -> TargetDirClaimKind {
    TargetDirClaimKind::AdoptedEmpty
}

/// Pure: whether a settled claim is present (writers/stage_checkout require it).
pub fn require_established_claim(
    claim: Option<TargetDirClaimKind>,
) -> Result<TargetDirClaimKind, TargetLeafRefusal> {
    claim.ok_or(TargetLeafRefusal::DoesNotExist)
}

// ---------------------------------------------------------------------------
// Effect staging preconditions
// ---------------------------------------------------------------------------

/// Caller-supplied facts for pure start-effect staging gates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StartEffectStagingFacts {
    /// Whether [`TargetDirClaimKind`] was established by create-target-dir.
    pub claim_established: bool,
    /// Whether a shared-target redirect dir was supplied for this start.
    pub has_shared_target_dir: bool,
}

/// Why a planned effect must not run its FS forward yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartEffectPreconditionError {
    /// Effect writes through the claimed checkout but claim is missing.
    ClaimNotEstablished,
    /// Cargo-config redirect is in the plan but no shared-target dir was given.
    SharedTargetDirMissing,
}

impl std::fmt::Display for StartEffectPreconditionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ClaimNotEstablished => {
                write!(f, "start effect requires an established target-dir claim")
            }
            Self::SharedTargetDirMissing => write!(
                f,
                "start plan includes cargo-config redirect but no shared_target_dir"
            ),
        }
    }
}

impl std::error::Error for StartEffectPreconditionError {}

/// Whether this effect writes through the claimed checkout leaf.
pub fn effect_requires_established_claim(effect: StartEffectKind) -> bool {
    matches!(
        effect,
        StartEffectKind::MaterializeCheckout
            | StartEffectKind::WriteManifest
            | StartEffectKind::WriteCargoConfigRedirect
            | StartEffectKind::HydrateIgnoredDirs
    )
}

/// Pure preconditions an effect needs before its FS forward may run.
///
/// Mirrors CLI `start_atomic` apply gates: claim-using writers refuse when the
/// create-target-dir step never established a claim; cargo-config refuses when
/// the plan includes a redirect without a shared-target dir.
pub fn validate_start_effect_preconditions(
    effect: StartEffectKind,
    facts: StartEffectStagingFacts,
) -> Result<(), StartEffectPreconditionError> {
    if effect_requires_established_claim(effect) && !facts.claim_established {
        return Err(StartEffectPreconditionError::ClaimNotEstablished);
    }
    if matches!(effect, StartEffectKind::WriteCargoConfigRedirect) && !facts.has_shared_target_dir {
        return Err(StartEffectPreconditionError::SharedTargetDirMissing);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Cleanup / rewind step lists from applied effects
// ---------------------------------------------------------------------------

/// One reverse-order cleanup action implied by a successfully applied effect.
///
/// Order from [`plan_start_cleanup`] is reverse-apply (last applied first),
/// matching the atomic mutation rewind ledger. FS/ref execution stays CLI-owned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartCleanupStep {
    /// Converge thread records back to the pre-start snapshot.
    RestoreThreadRecord,
    /// Tear down the virtualized mount if this start established it.
    UnmountVirtualized,
    /// Unlink hydrate dep symlinks (+ restore the exclude file if written).
    UnwindHydrate,
    /// Restore prior cargo config bytes, or remove a config this start created.
    RestoreCargoConfig,
    /// Restore prior manifest bytes, or remove a manifest this start created.
    RestoreManifest,
    /// Clear per-root materialize sidecars, then apply the checkout-dir rewind.
    RewindCheckout { plan: CheckoutRewindPlan },
    /// CAS-guarded rollback of the thread ref to its pre-start expectation.
    RollbackThreadRef,
    /// Remove a self-created target dir (create-step inverse).
    RemoveSelfCreatedDir { plan: SelfCreatedDirRewindPlan },
}

/// Build reverse cleanup steps for effects that successfully applied.
///
/// `applied` is forward-apply order (the subset of [`StartTransactionPlan::effects`]
/// whose forwards completed). `target_claim` is the settled claim from the
/// create-target-dir step (`None` when that step refused or never ran).
///
/// Partial starts are expressible by passing only the applied prefix — e.g. a
/// failure mid-hydrate yields applied effects through `HydrateIgnoredDirs` and
/// rewinds every hydrate link plus the checkout.
pub fn plan_start_cleanup(
    applied: &[StartEffectKind],
    target_claim: Option<TargetDirClaimKind>,
) -> Vec<StartCleanupStep> {
    let checkout_plan = plan_checkout_rewind(target_claim);
    let self_created_plan = plan_self_created_dir_rewind(target_claim);
    let mut steps = Vec::with_capacity(applied.len());
    for effect in applied.iter().rev() {
        match effect {
            StartEffectKind::WriteThreadRecord => {
                steps.push(StartCleanupStep::RestoreThreadRecord);
            }
            StartEffectKind::EstablishVirtualizedMount => {
                steps.push(StartCleanupStep::UnmountVirtualized);
            }
            StartEffectKind::HydrateIgnoredDirs => {
                steps.push(StartCleanupStep::UnwindHydrate);
            }
            StartEffectKind::WriteCargoConfigRedirect => {
                steps.push(StartCleanupStep::RestoreCargoConfig);
            }
            StartEffectKind::WriteManifest => {
                steps.push(StartCleanupStep::RestoreManifest);
            }
            StartEffectKind::MaterializeCheckout => {
                steps.push(StartCleanupStep::RewindCheckout {
                    plan: checkout_plan,
                });
            }
            StartEffectKind::WriteThreadRef => {
                steps.push(StartCleanupStep::RollbackThreadRef);
            }
            StartEffectKind::CreateTargetDir => {
                steps.push(StartCleanupStep::RemoveSelfCreatedDir {
                    plan: self_created_plan,
                });
            }
        }
    }
    steps
}

// ---------------------------------------------------------------------------
// Mid-apply error classification
// ---------------------------------------------------------------------------

/// Classify an `anyhow` error from a materialize/hydrate helper into the
/// [`HeddleError`] the start transaction's `Result` requires.
///
/// Mirrors CLI `start_atomic::apply_error` (heddle#571): must NOT blanket-wrap
/// every failure as a `Conflict`. A plain I/O failure mid-materialize (e.g.
/// `clonefile`/`FICLONE` ENOENT) must surface as `Io` so diagnosis and
/// `exit::from_error` kind-keyed classification stay correct.
///
/// Recovery rules:
/// - an already-structured [`HeddleError`] keeps its variant;
/// - an error whose chain bottoms out in a `std::io::Error` becomes
///   [`HeddleError::Io`], preserving both the original `ErrorKind` and the
///   full `anyhow` context (`{err:#}`) as the message;
/// - only a genuinely-unclassifiable error falls back to
///   [`HeddleError::Conflict`].
pub fn classify_materialize_error(err: anyhow::Error) -> HeddleError {
    match err.downcast::<HeddleError>() {
        Ok(heddle) => heddle,
        Err(err) => match err
            .downcast_ref::<std::io::Error>()
            .map(std::io::Error::kind)
        {
            Some(kind) => HeddleError::Io(std::io::Error::new(kind, format!("{err:#}"))),
            None => HeddleError::Conflict(format!("{err:#}")),
        },
    }
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

    #[test]
    fn start_transaction_plan_matches_materialize_steps() {
        let plan = plan_start_transaction(&ThreadMode::Materialized, true, true);
        assert_eq!(plan.copy_policy, CheckoutCopyPolicy::PreferReflink);
        assert!(plan.write_manifest);
        assert!(plan.apply_shared_target);
        assert!(plan.hydrate);
        assert!(!plan.virtualized_mount);
        assert_eq!(
            plan.effects,
            vec![
                StartEffectKind::CreateTargetDir,
                StartEffectKind::WriteThreadRef,
                StartEffectKind::MaterializeCheckout,
                StartEffectKind::WriteManifest,
                StartEffectKind::WriteCargoConfigRedirect,
                StartEffectKind::HydrateIgnoredDirs,
                StartEffectKind::WriteThreadRecord,
            ]
        );

        let virtualized = plan_start_transaction(&ThreadMode::Virtualized, true, true);
        assert_eq!(
            virtualized.effects,
            vec![
                StartEffectKind::CreateTargetDir,
                StartEffectKind::WriteThreadRef,
                StartEffectKind::EstablishVirtualizedMount,
                StartEffectKind::WriteThreadRecord,
            ]
        );
        assert!(virtualized.virtualized_mount);
        assert!(!virtualized.apply_shared_target);
        assert!(!virtualized.hydrate);
    }

    #[test]
    fn materialize_step_effect_kind_strips_payload() {
        assert_eq!(
            MaterializeStep::MaterializeCheckout {
                copy_policy: CheckoutCopyPolicy::FullCopy
            }
            .effect_kind(),
            StartEffectKind::MaterializeCheckout
        );
        assert_eq!(
            MaterializeStep::WriteManifest.effect_kind(),
            StartEffectKind::WriteManifest
        );
    }

    #[test]
    fn checkout_and_self_created_rewind_plans_key_on_claim() {
        assert_eq!(
            plan_checkout_rewind(Some(TargetDirClaimKind::Created)),
            CheckoutRewindPlan::ClearAndRemoveDir
        );
        assert_eq!(
            plan_checkout_rewind(Some(TargetDirClaimKind::AdoptedEmpty)),
            CheckoutRewindPlan::ClearContentsOnly
        );
        assert_eq!(plan_checkout_rewind(None), CheckoutRewindPlan::TouchNothing);

        assert_eq!(
            plan_self_created_dir_rewind(Some(TargetDirClaimKind::Created)),
            SelfCreatedDirRewindPlan::RemoveIfStillAtPath
        );
        assert_eq!(
            plan_self_created_dir_rewind(Some(TargetDirClaimKind::AdoptedEmpty)),
            SelfCreatedDirRewindPlan::TouchNothing
        );
        assert_eq!(
            plan_self_created_dir_rewind(None),
            SelfCreatedDirRewindPlan::TouchNothing
        );
    }

    #[test]
    fn start_cleanup_reverses_applied_effects_for_created_claim() {
        let applied = plan_start_transaction(&ThreadMode::Materialized, true, true).effects;
        let cleanup = plan_start_cleanup(&applied, Some(TargetDirClaimKind::Created));
        assert_eq!(
            cleanup,
            vec![
                StartCleanupStep::RestoreThreadRecord,
                StartCleanupStep::UnwindHydrate,
                StartCleanupStep::RestoreCargoConfig,
                StartCleanupStep::RestoreManifest,
                StartCleanupStep::RewindCheckout {
                    plan: CheckoutRewindPlan::ClearAndRemoveDir
                },
                StartCleanupStep::RollbackThreadRef,
                StartCleanupStep::RemoveSelfCreatedDir {
                    plan: SelfCreatedDirRewindPlan::RemoveIfStillAtPath
                },
            ]
        );
    }

    #[test]
    fn start_cleanup_partial_hydrate_with_adopted_claim() {
        // Applied through hydrate; record not yet written. Adopted empty user --path.
        let applied = [
            StartEffectKind::CreateTargetDir,
            StartEffectKind::WriteThreadRef,
            StartEffectKind::MaterializeCheckout,
            StartEffectKind::HydrateIgnoredDirs,
        ];
        let cleanup = plan_start_cleanup(&applied, Some(TargetDirClaimKind::AdoptedEmpty));
        assert_eq!(
            cleanup,
            vec![
                StartCleanupStep::UnwindHydrate,
                StartCleanupStep::RewindCheckout {
                    plan: CheckoutRewindPlan::ClearContentsOnly
                },
                StartCleanupStep::RollbackThreadRef,
                StartCleanupStep::RemoveSelfCreatedDir {
                    plan: SelfCreatedDirRewindPlan::TouchNothing
                },
            ]
        );
    }

    #[test]
    fn start_cleanup_virtualized_and_refused_claim() {
        let applied = plan_start_transaction(&ThreadMode::Virtualized, false, false).effects;
        let cleanup = plan_start_cleanup(&applied, None);
        assert_eq!(
            cleanup,
            vec![
                StartCleanupStep::RestoreThreadRecord,
                StartCleanupStep::UnmountVirtualized,
                StartCleanupStep::RollbackThreadRef,
                StartCleanupStep::RemoveSelfCreatedDir {
                    plan: SelfCreatedDirRewindPlan::TouchNothing
                },
            ]
        );
    }

    #[test]
    fn classify_materialize_error_preserves_io_and_does_not_mislabel_as_conflict() {
        let bare_io = anyhow::Error::new(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "No such file or directory (os error 2)",
        ));
        let mapped = classify_materialize_error(bare_io);
        assert!(
            matches!(mapped, HeddleError::Io(_)),
            "a bare io error must surface as Io, got {mapped:?}"
        );
        assert!(
            !format!("{mapped}").starts_with("conflict:"),
            "io error must not be reported as a conflict: {mapped}"
        );

        let structured_io = anyhow::Error::new(HeddleError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "No such file or directory (os error 2)",
        )));
        assert!(
            matches!(
                classify_materialize_error(structured_io),
                HeddleError::Io(_)
            ),
            "a propagated HeddleError::Io must keep its variant"
        );

        let conflict = anyhow::Error::new(HeddleError::Conflict("real merge conflict".to_string()));
        assert!(
            matches!(
                classify_materialize_error(conflict),
                HeddleError::Conflict(_)
            ),
            "a genuine conflict must remain a conflict"
        );
    }

    #[test]
    fn threads_root_path_safety_classifies_layout() {
        let heddle = PathBuf::from("/repo/.heddle");
        let threads = heddle.join("threads");

        assert_eq!(
            classify_path_vs_threads_root(&threads, &heddle, &threads),
            ThreadsRootPathClass::ThreadsRoot
        );
        assert_eq!(
            classify_path_vs_threads_root(&threads.join("feat"), &heddle, &threads),
            ThreadsRootPathClass::BareThreadDir
        );
        assert_eq!(
            classify_path_vs_threads_root(&threads.join("feat").join("repo"), &heddle, &threads),
            ThreadsRootPathClass::ManagedCheckoutSlot
        );
        assert_eq!(
            classify_path_vs_threads_root(&heddle.join("objects"), &heddle, &threads),
            ThreadsRootPathClass::HeddleStorage
        );
        assert_eq!(
            classify_path_vs_threads_root(Path::new("/tmp/work"), &heddle, &threads),
            ThreadsRootPathClass::OutsideHeddle
        );

        assert!(threads_root_path_layout_allowed(
            ThreadsRootPathClass::ManagedCheckoutSlot
        ));
        assert!(threads_root_path_layout_allowed(
            ThreadsRootPathClass::OutsideHeddle
        ));
        assert!(!threads_root_path_layout_allowed(
            ThreadsRootPathClass::ThreadsRoot
        ));
        assert!(!threads_root_path_layout_allowed(
            ThreadsRootPathClass::BareThreadDir
        ));
        assert!(!threads_root_path_layout_allowed(
            ThreadsRootPathClass::HeddleStorage
        ));
    }

    #[test]
    fn threads_root_path_safety_rejects_nested_reserved() {
        let heddle = PathBuf::from("/repo/.heddle");
        let threads = heddle.join("threads");
        let thread_dir = threads.join("feat");
        let checkout = thread_dir.join("repo");
        let nested = checkout.join("nested");

        assert!(path_is_nested_in_reserved_region(
            &nested,
            &thread_dir,
            Some(checkout.as_path())
        ));
        assert!(!path_is_nested_in_reserved_region(
            &checkout,
            &thread_dir,
            Some(checkout.as_path())
        ));

        let err = validate_threads_root_path_safety(
            &nested,
            &heddle,
            &threads,
            &[(thread_dir.clone(), Some(checkout.clone()))],
        )
        .unwrap_err();
        assert!(matches!(
            err,
            ThreadsRootPathSafetyError::NestedInReserved { .. }
        ));

        assert!(
            validate_threads_root_path_safety(
                &checkout,
                &heddle,
                &threads,
                &[(thread_dir, Some(checkout.clone()))],
            )
            .is_ok()
        );
    }

    #[test]
    fn append_safe_relative_components_refuses_escape() {
        let base = PathBuf::from("/resolved/ancestor");
        assert_eq!(
            append_safe_relative_components(base.clone(), Path::new("a/b")).unwrap(),
            PathBuf::from("/resolved/ancestor/a/b")
        );
        assert_eq!(
            append_safe_relative_components(base.clone(), Path::new("a/./b")).unwrap(),
            PathBuf::from("/resolved/ancestor/a/b")
        );
        assert_eq!(
            append_safe_relative_components(base.clone(), Path::new("a/../b")).unwrap_err(),
            RelativePathNormalizeError::UnsafeComponent
        );
        assert!(!path_components_are_safe(Path::new("/a/../b")));
        assert!(path_components_are_safe(Path::new("/a/b")));
        assert!(path_is_strict_descendant(
            Path::new("/repo/.heddle/threads/x/y"),
            Path::new("/repo/.heddle/threads")
        ));
        assert!(!path_is_strict_descendant(
            Path::new("/repo/.heddle/threads"),
            Path::new("/repo/.heddle/threads")
        ));
    }

    #[test]
    fn empty_dir_adoption_and_create_intent() {
        assert_eq!(
            plan_target_dir_create_intent(true),
            TargetDirCreateIntent::AttemptCreate
        );
        assert_eq!(
            plan_target_dir_create_intent(false),
            TargetDirCreateIntent::AdoptOnly
        );

        assert_eq!(
            classify_target_leaf_shape(false, false, false, false),
            TargetLeafShape::Absent
        );
        assert_eq!(
            classify_target_leaf_shape(true, true, true, true),
            TargetLeafShape::Symlink
        );
        assert_eq!(
            classify_target_leaf_shape(true, false, false, false),
            TargetLeafShape::NotDirectory
        );
        assert_eq!(
            classify_target_leaf_shape(true, false, true, true),
            TargetLeafShape::EmptyDirectory
        );
        assert_eq!(
            classify_target_leaf_shape(true, false, true, false),
            TargetLeafShape::NonEmptyDirectory
        );

        assert!(validate_empty_dir_adoption(TargetLeafShape::EmptyDirectory).is_ok());
        assert_eq!(
            validate_empty_dir_adoption(TargetLeafShape::NonEmptyDirectory).unwrap_err(),
            TargetLeafRefusal::NotEmpty
        );
        assert_eq!(
            validate_empty_dir_adoption(TargetLeafShape::Symlink).unwrap_err(),
            TargetLeafRefusal::IsSymlink
        );
        assert_eq!(TargetLeafRefusal::NotEmpty.as_reason_str(), "is not empty");

        assert_eq!(
            claim_kind_for_create_attempt(CreateDirAttempt::Created),
            Some(TargetDirClaimKind::Created)
        );
        assert_eq!(
            claim_kind_for_create_attempt(CreateDirAttempt::AlreadyExists),
            None
        );
        assert_eq!(
            claim_kind_after_empty_dir_adoption(),
            TargetDirClaimKind::AdoptedEmpty
        );
        assert!(require_established_claim(Some(TargetDirClaimKind::Created)).is_ok());
        assert!(require_established_claim(None).is_err());
    }

    #[test]
    fn effect_staging_preconditions_gate_claim_and_shared_target() {
        let no_claim = StartEffectStagingFacts {
            claim_established: false,
            has_shared_target_dir: true,
        };
        let with_claim = StartEffectStagingFacts {
            claim_established: true,
            has_shared_target_dir: false,
        };
        let full = StartEffectStagingFacts {
            claim_established: true,
            has_shared_target_dir: true,
        };

        assert!(
            validate_start_effect_preconditions(StartEffectKind::CreateTargetDir, no_claim).is_ok()
        );
        assert!(
            validate_start_effect_preconditions(StartEffectKind::WriteThreadRef, no_claim).is_ok()
        );
        assert_eq!(
            validate_start_effect_preconditions(StartEffectKind::MaterializeCheckout, no_claim)
                .unwrap_err(),
            StartEffectPreconditionError::ClaimNotEstablished
        );
        assert_eq!(
            validate_start_effect_preconditions(
                StartEffectKind::WriteCargoConfigRedirect,
                with_claim
            )
            .unwrap_err(),
            StartEffectPreconditionError::SharedTargetDirMissing
        );
        assert!(
            validate_start_effect_preconditions(StartEffectKind::WriteCargoConfigRedirect, full)
                .is_ok()
        );
        assert!(effect_requires_established_claim(
            StartEffectKind::MaterializeCheckout
        ));
        assert!(!effect_requires_established_claim(
            StartEffectKind::CreateTargetDir
        ));
    }

    #[test]
    fn classify_materialize_error_preserves_context_when_reclassifying_io() {
        use anyhow::Context as _;

        let with_ctx = Err::<(), _>(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "os error 13",
        ))
        .context("writing .cargo/config.toml to /work/.cargo/config.toml")
        .unwrap_err();

        let mapped = classify_materialize_error(with_ctx);
        assert!(
            matches!(&mapped, HeddleError::Io(io) if io.kind() == std::io::ErrorKind::PermissionDenied),
            "io kind must survive reclassification, got {mapped:?}"
        );
        let msg = format!("{mapped}");
        assert!(
            msg.contains(".cargo/config.toml") && msg.contains("writing"),
            "reclassified io error must retain the path/action context: {msg}"
        );
    }
}
