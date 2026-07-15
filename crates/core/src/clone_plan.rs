// SPDX-License-Identifier: Apache-2.0
//! Pure clone and adopt planning.
//!
//! Owns decision logic shared by `heddle clone` and `heddle adopt`:
//! - destination path validation and absolute-resolution policy
//! - remote mode selection (local path vs network hosted vs git-overlay URL)
//! - security preflight flag assembly (no network I/O)
//! - adopt start-path resolution and path-conflict policy
//! - monorepo recursive clone: child selection, path anchoring, work order
//! - monorepo per-node execution steps (validate dest, init, fetch, materialize, map)
//! - monorepo step ordering validation, progress labels, and result summary
//!
//! Filesystem mutations, hosted RPC, git import, and recovery-advice
//! rendering stay CLI-owned. Callers gather cheap facts (path existence,
//! RemoteTarget parse result, git/.heddle probes, resolved monorepo trees),
//! invoke these helpers, then execute I/O from the plan.

use std::path::{Path, PathBuf};

use objects::object::StateId;
use serde::Serialize;

// ---------------------------------------------------------------------------
// Clone options / facts
// ---------------------------------------------------------------------------

/// Caller-supplied clone inputs for pure preflight planning.
///
/// Field names mirror the CLI `heddle clone` surface. Network connect,
/// repository init, and worktree materialization are omitted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClonePlanOptions {
    pub remote: String,
    pub local: PathBuf,
    pub thread: Option<String>,
    /// Raw `--depth` (including `Some(0)`); normalized in the plan.
    pub depth: Option<u32>,
    pub lazy: bool,
    pub filter: Option<String>,
    pub recursive: bool,
    /// CLI `--insecure`: allow cleartext to non-loopback hosts on network paths.
    pub insecure: bool,
}

/// Cheap facts the CLI gathers before planning (no clone network/FS body).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClonePlanFacts {
    /// Whether the destination path already exists on disk.
    pub destination_exists: bool,
    /// Remote classification after `RemoteTarget::parse` and local probes.
    pub remote_source: CloneRemoteSource,
}

/// How the CLI classified the remote for mode selection.
///
/// Network socket resolution and path existence for `file://` / raw paths
/// remain caller-owned (`RemoteTarget::parse`). This enum carries only the
/// pure facts needed to choose an execution mode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CloneRemoteSource {
    /// Local filesystem path (`file://` or existing directory).
    Local {
        path: PathBuf,
        /// `.heddle` metadata directory present at the source.
        has_heddle: bool,
        /// Source opens as a Git repository (overlay path candidate).
        is_git: bool,
    },
    /// Hosted/network heddle endpoint (DNS/socket already resolved by CLI).
    Network {
        /// Whether a repository path component was present on the URL.
        has_repo_path: bool,
    },
    /// `RemoteTarget::parse` failed; string-shape helpers select fallbacks.
    Unparsed,
}

// ---------------------------------------------------------------------------
// Clone plan / mode / security
// ---------------------------------------------------------------------------

/// Execution mode selected by [`plan_clone`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CloneMode {
    /// Local Heddle repository (`.heddle` present or non-git local path).
    LocalHeddle { remote_path: PathBuf },
    /// Local Git repository without Heddle metadata → git-overlay clone.
    LocalGitOverlay { remote_path: PathBuf },
    /// Unparsed remote that looks like a Git URL (`https://`, `git@`, …).
    GitOverlayUrl,
    /// Hosted/network clone; `recursive` selects monorepo vs single-spool.
    NetworkHosted { recursive: bool },
}

impl CloneMode {
    /// Short label for unsupported-option error context.
    pub fn kind_label(&self) -> &'static str {
        match self {
            Self::LocalHeddle { .. } => "local",
            Self::LocalGitOverlay { .. } | Self::GitOverlayUrl => "git-overlay",
            Self::NetworkHosted { recursive: true } => "monorepo",
            Self::NetworkHosted { recursive: false } => "network",
        }
    }

    pub fn is_network(&self) -> bool {
        matches!(self, Self::NetworkHosted { .. })
    }

    pub fn is_git_overlay(&self) -> bool {
        matches!(self, Self::LocalGitOverlay { .. } | Self::GitOverlayUrl)
    }
}

/// Security flags assembled for network clone sessions (no connect performed).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloneSecurityPreflight {
    /// Pass to `HostedSession::with_allow_insecure` / client config.
    pub allow_insecure: bool,
    /// Caller must build a hosted session and validate TLS/auth before any
    /// destination `create_dir_all` / `Repository::init`.
    pub requires_network_session: bool,
}

/// Pure clone orchestration plan. CLI executes FS / hosted / git I/O from it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClonePlan {
    pub destination: PathBuf,
    pub remote: String,
    pub mode: CloneMode,
    pub thread: Option<String>,
    /// Normalized depth (`None` when absent or zero).
    pub depth: Option<u32>,
    pub lazy: bool,
    pub filter: Option<String>,
    pub recursive: bool,
    /// Network effective lazy: `lazy || filter.is_some()`.
    pub effective_lazy: bool,
    pub security: CloneSecurityPreflight,
}

// ---------------------------------------------------------------------------
// Clone errors
// ---------------------------------------------------------------------------

/// Flag that cannot be combined with the selected clone mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnsupportedCloneFlag {
    Filter,
    Lazy,
    Depth,
}

impl UnsupportedCloneFlag {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Filter => "--filter",
            Self::Lazy => "--lazy",
            Self::Depth => "--depth",
        }
    }
}

/// Failures from pure clone planning.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClonePlanError {
    /// Destination path already exists.
    DestinationExists { path: PathBuf },
    /// `--recursive` requires a hosted/network remote.
    MonorepoRequiresHosted { remote: String },
    /// Unparsed remote that looks like a local path (missing source).
    RemoteLooksLikeMissingLocalPath { remote: String },
    /// Unparsed remote that is neither local-shaped nor a git URL.
    InvalidRemoteUrl { remote: String },
    /// Option rejected for the selected mode.
    UnsupportedOption {
        flag: UnsupportedCloneFlag,
        /// Mode label (`local`, `git-overlay`, `monorepo`, …).
        mode: &'static str,
        /// Optional filter value for messaging.
        value: Option<String>,
    },
}

impl std::fmt::Display for ClonePlanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DestinationExists { path } => {
                write!(f, "local path '{}' already exists", path.display())
            }
            Self::MonorepoRequiresHosted { remote } => write!(
                f,
                "--recursive monorepo clone requires a hosted spool remote; '{remote}' is not one"
            ),
            Self::RemoteLooksLikeMissingLocalPath { remote } => {
                write!(f, "remote repository '{remote}' does not exist")
            }
            Self::InvalidRemoteUrl { remote } => write!(f, "invalid remote URL: {remote}"),
            Self::UnsupportedOption { flag, mode, value } => {
                let flag_label = value
                    .as_deref()
                    .map(|v| format!("{} {v}", flag.as_str()))
                    .unwrap_or_else(|| flag.as_str().to_string());
                write!(f, "{flag_label} is not supported for {mode} clones")
            }
        }
    }
}

impl std::error::Error for ClonePlanError {}

// ---------------------------------------------------------------------------
// Adopt options / plan / errors
// ---------------------------------------------------------------------------

/// Caller-supplied adopt inputs for pure path planning.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdoptPlanOptions {
    /// Positional path argument.
    pub path: Option<PathBuf>,
    /// Global `--repo` / `-C` path when set.
    pub repo_flag: Option<PathBuf>,
    /// Process working directory (for relative → absolute resolution).
    pub cwd: PathBuf,
    /// Explicit `--ref` values (empty means import all local branches/tags).
    pub refs: Vec<String>,
}

/// Pure adopt preflight plan. CLI discovers Git root, bootstraps, and imports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdoptPlan {
    /// Start path for Git discovery (not yet canonicalized).
    pub start_path: PathBuf,
    pub refs: Vec<String>,
    /// True when no explicit `--ref` was supplied.
    pub import_all_refs: bool,
}

/// Failures from pure adopt planning.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdoptPlanError {
    /// Positional path and `--repo` disagree after absolute resolution.
    PathConflict { positional: PathBuf, repo: PathBuf },
}

impl std::fmt::Display for AdoptPlanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::PathConflict { positional, repo } => write!(
                f,
                "adopt path '{}' conflicts with --repo '{}'",
                positional.display(),
                repo.display()
            ),
        }
    }
}

impl std::error::Error for AdoptPlanError {}

// ---------------------------------------------------------------------------
// Pure path helpers
// ---------------------------------------------------------------------------

/// Absolute-resolution policy: join relative paths against `cwd`.
///
/// Does not canonicalize or require the path to exist. Callers that need a
/// stable on-disk identity may canonicalize after planning when the path
/// exists.
pub fn absolute_path(path: &Path, cwd: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    }
}

/// Resolve a clone destination against `cwd` without requiring it to exist.
pub fn resolve_clone_destination(local: &Path, cwd: &Path) -> PathBuf {
    absolute_path(local, cwd)
}

/// Destination validation: refuse when the path already exists.
pub fn validate_clone_destination(
    destination: &Path,
    destination_exists: bool,
) -> Result<(), ClonePlanError> {
    if destination_exists {
        Err(ClonePlanError::DestinationExists {
            path: destination.to_path_buf(),
        })
    } else {
        Ok(())
    }
}

/// Normalize `--depth`: `0` and missing mean full history (`None`).
pub fn normalize_clone_depth(depth: Option<u32>) -> Option<u32> {
    depth.filter(|depth| *depth > 0)
}

/// Whether an unparsed remote string looks like a filesystem path.
///
/// Matches CLI: absolute paths, `.` / `..`, `./` / `../`, and `~/`.
pub fn looks_like_local_path(remote: &str) -> bool {
    let path = Path::new(remote);
    path.is_absolute()
        || remote == "."
        || remote == ".."
        || remote.starts_with("./")
        || remote.starts_with("../")
        || remote.starts_with("~/")
}

/// Whether an unparsed remote string looks like a Git clone URL.
///
/// Matches CLI: any `://` scheme or SCP-style `git@` host.
pub fn looks_like_git_overlay_url(remote: &str) -> bool {
    remote.contains("://") || remote.starts_with("git@")
}

/// Resolve adopt start path from positional / `--repo` / cwd.
///
/// Pure policy (no canonicalize). CLI may canonicalize when the path exists.
pub fn resolve_adopt_start_path(
    positional: Option<&Path>,
    repo_flag: Option<&Path>,
    cwd: &Path,
) -> Result<PathBuf, AdoptPlanError> {
    match (positional, repo_flag) {
        (Some(positional), Some(repo_path)) => {
            if absolute_path(positional, cwd) != absolute_path(repo_path, cwd) {
                return Err(AdoptPlanError::PathConflict {
                    positional: positional.to_path_buf(),
                    repo: repo_path.to_path_buf(),
                });
            }
            Ok(positional.to_path_buf())
        }
        (Some(positional), None) => Ok(positional.to_path_buf()),
        (None, Some(repo_path)) => Ok(repo_path.to_path_buf()),
        (None, None) => Ok(cwd.to_path_buf()),
    }
}

// ---------------------------------------------------------------------------
// Security preflight assembly
// ---------------------------------------------------------------------------

/// Assemble security flags for the selected clone mode without connecting.
pub fn assemble_clone_security_preflight(
    mode: &CloneMode,
    insecure: bool,
) -> CloneSecurityPreflight {
    if mode.is_network() {
        CloneSecurityPreflight {
            allow_insecure: insecure,
            requires_network_session: true,
        }
    } else {
        CloneSecurityPreflight {
            allow_insecure: false,
            requires_network_session: false,
        }
    }
}

// ---------------------------------------------------------------------------
// Mode selection + option gates
// ---------------------------------------------------------------------------

/// Select clone mode from remote classification and flags.
pub fn select_clone_mode(
    remote: &str,
    recursive: bool,
    source: &CloneRemoteSource,
) -> Result<CloneMode, ClonePlanError> {
    match source {
        CloneRemoteSource::Local {
            path,
            has_heddle,
            is_git,
        } => {
            if recursive {
                return Err(ClonePlanError::MonorepoRequiresHosted {
                    remote: remote.to_string(),
                });
            }
            if !has_heddle && *is_git {
                Ok(CloneMode::LocalGitOverlay {
                    remote_path: path.clone(),
                })
            } else {
                Ok(CloneMode::LocalHeddle {
                    remote_path: path.clone(),
                })
            }
        }
        CloneRemoteSource::Network { .. } => Ok(CloneMode::NetworkHosted { recursive }),
        CloneRemoteSource::Unparsed => {
            if recursive {
                return Err(ClonePlanError::MonorepoRequiresHosted {
                    remote: remote.to_string(),
                });
            }
            if looks_like_local_path(remote) {
                return Err(ClonePlanError::RemoteLooksLikeMissingLocalPath {
                    remote: remote.to_string(),
                });
            }
            if looks_like_git_overlay_url(remote) {
                Ok(CloneMode::GitOverlayUrl)
            } else {
                Err(ClonePlanError::InvalidRemoteUrl {
                    remote: remote.to_string(),
                })
            }
        }
    }
}

/// Reject flags that the selected mode cannot honor.
pub fn validate_clone_mode_options(
    mode: &CloneMode,
    depth: Option<u32>,
    lazy: bool,
    filter: Option<&str>,
) -> Result<(), ClonePlanError> {
    match mode {
        CloneMode::LocalGitOverlay { .. } | CloneMode::GitOverlayUrl => {
            if let Some(value) = filter {
                return Err(ClonePlanError::UnsupportedOption {
                    flag: UnsupportedCloneFlag::Filter,
                    mode: mode.kind_label(),
                    value: Some(value.to_string()),
                });
            }
            if lazy {
                return Err(ClonePlanError::UnsupportedOption {
                    flag: UnsupportedCloneFlag::Lazy,
                    mode: mode.kind_label(),
                    value: None,
                });
            }
            if depth.is_some() {
                return Err(ClonePlanError::UnsupportedOption {
                    flag: UnsupportedCloneFlag::Depth,
                    mode: mode.kind_label(),
                    value: None,
                });
            }
        }
        CloneMode::LocalHeddle { .. } => {
            if let Some(value) = filter {
                return Err(ClonePlanError::UnsupportedOption {
                    flag: UnsupportedCloneFlag::Filter,
                    mode: mode.kind_label(),
                    value: Some(value.to_string()),
                });
            }
            if lazy {
                return Err(ClonePlanError::UnsupportedOption {
                    flag: UnsupportedCloneFlag::Lazy,
                    mode: mode.kind_label(),
                    value: Some("true".to_string()),
                });
            }
        }
        CloneMode::NetworkHosted { recursive: true } => {
            if filter.is_some() {
                return Err(ClonePlanError::UnsupportedOption {
                    flag: UnsupportedCloneFlag::Filter,
                    mode: mode.kind_label(),
                    value: None,
                });
            }
            if lazy {
                return Err(ClonePlanError::UnsupportedOption {
                    flag: UnsupportedCloneFlag::Lazy,
                    mode: mode.kind_label(),
                    value: None,
                });
            }
            if depth.is_some() {
                return Err(ClonePlanError::UnsupportedOption {
                    flag: UnsupportedCloneFlag::Depth,
                    mode: mode.kind_label(),
                    value: None,
                });
            }
        }
        CloneMode::NetworkHosted { recursive: false } => {}
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Top-level planners
// ---------------------------------------------------------------------------

/// Plan a clone from pure options and caller-gathered facts.
///
/// Does not create directories, open repositories, or perform network I/O.
pub fn plan_clone(
    options: &ClonePlanOptions,
    facts: &ClonePlanFacts,
) -> Result<ClonePlan, ClonePlanError> {
    validate_clone_destination(&options.local, facts.destination_exists)?;

    let mode = select_clone_mode(&options.remote, options.recursive, &facts.remote_source)?;
    let depth = normalize_clone_depth(options.depth);
    validate_clone_mode_options(&mode, depth, options.lazy, options.filter.as_deref())?;

    let security = assemble_clone_security_preflight(&mode, options.insecure);
    let effective_lazy = if mode.is_network() {
        options.lazy || options.filter.is_some()
    } else {
        false
    };

    Ok(ClonePlan {
        destination: options.local.clone(),
        remote: options.remote.clone(),
        mode,
        thread: options.thread.clone(),
        depth,
        lazy: options.lazy,
        filter: options.filter.clone(),
        recursive: options.recursive,
        effective_lazy,
        security,
    })
}

/// Plan adopt path preflight from pure options.
///
/// Does not open Git repositories or import history.
pub fn plan_adopt(options: &AdoptPlanOptions) -> Result<AdoptPlan, AdoptPlanError> {
    let start_path = resolve_adopt_start_path(
        options.path.as_deref(),
        options.repo_flag.as_deref(),
        &options.cwd,
    )?;
    Ok(AdoptPlan {
        start_path,
        refs: options.refs.clone(),
        import_all_refs: options.refs.is_empty(),
    })
}

// ---------------------------------------------------------------------------
// Monorepo clone planning (recursive hosted)
// ---------------------------------------------------------------------------
//
// After the CLI calls ResolveMonorepo, it maps the transport tree into pure
// [`MonorepoNodeFacts`] and invokes [`plan_monorepo_clone`]. Placement rules:
// - Root node at relative path `""` (the clone destination itself).
// - Each selected child edge mounts at `<parent_rel>/<mount_name>`.
// - Edges with a child subtree are selected and walked; edges without a child
//   are recorded as skipped (unreadable / cycle / depth-bounded / unspecified)
//   and are never fatal.
// - A node with no content state still yields a materialize step (empty
//   checkout) so the monorepo layout stays coherent.
// Work order is pre-order: a parent's node always precedes its children.
// Hosted RPC and per-node materialize I/O stay CLI-owned.

/// Why a monorepo child edge was not selected for materialization.
///
/// Transport-free mirror of hosted `EdgeSkip`. Labels are stable for JSON and
/// human reporting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MonorepoEdgeSkipReason {
    Unspecified,
    Unreadable,
    Cycle,
    DepthBounded,
}

impl MonorepoEdgeSkipReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unspecified => "unspecified",
            Self::Unreadable => "unreadable",
            Self::Cycle => "cycle",
            Self::DepthBounded => "depth-bounded",
        }
    }

    /// Map wire `EdgeSkip` discriminant (proto i32) without taking gRPC types.
    ///
    /// Proto layout: Unspecified=0, Unreadable=1, Cycle=2, DepthBounded=3.
    /// Unknown values map to [`None`] so callers can fall back or omit.
    pub fn from_wire_i32(value: i32) -> Option<Self> {
        match value {
            0 => Some(Self::Unspecified),
            1 => Some(Self::Unreadable),
            2 => Some(Self::Cycle),
            3 => Some(Self::DepthBounded),
            _ => None,
        }
    }
}

/// Relative path label for monorepo placement lines (`""` → `.`).
pub fn monorepo_rel_display(rel_path: &Path) -> String {
    if rel_path.as_os_str().is_empty() {
        ".".to_string()
    } else {
        rel_path.display().to_string()
    }
}

/// Machine-facing monorepo clone envelope fields (CLI wraps with serde_json).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MonorepoCloneJsonReport {
    pub output_kind: &'static str,
    pub action: &'static str,
    pub status: &'static str,
    pub success: bool,
    pub transport: &'static str,
    pub local: String,
    pub placed: Vec<MonorepoPlacedJsonRow>,
    pub skipped: Vec<MonorepoSkippedJsonRow>,
}

/// One placed node in monorepo clone JSON.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MonorepoPlacedJsonRow {
    pub spool_id: String,
    pub path: String,
    pub content_state: Option<String>,
}

/// One skipped edge in monorepo clone JSON.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MonorepoSkippedJsonRow {
    pub child_spool_id: String,
    pub mount_name: String,
    pub path: String,
    pub reason: String,
}

/// Pure JSON-oriented report from a monorepo result summary + local path.
pub fn assemble_monorepo_clone_json_report(
    local_path: &Path,
    summary: &MonorepoCloneResultSummary,
) -> MonorepoCloneJsonReport {
    let placed = summary
        .placed
        .iter()
        .map(|node| MonorepoPlacedJsonRow {
            spool_id: node.spool_id.clone(),
            path: node.rel_path.display().to_string(),
            content_state: node.content_state.map(|s| s.to_string()),
        })
        .collect();
    let skipped = summary
        .skipped
        .iter()
        .map(|sk| MonorepoSkippedJsonRow {
            child_spool_id: sk.child_spool_id.clone(),
            mount_name: sk.mount_name.clone(),
            path: sk.rel_path.display().to_string(),
            reason: sk.reason_label().to_string(),
        })
        .collect();
    MonorepoCloneJsonReport {
        output_kind: "clone_monorepo",
        action: "clone",
        status: "cloned",
        success: true,
        transport: "heddle",
        local: local_path.display().to_string(),
        placed,
        skipped,
    }
}

/// Pure facts for one edge under a monorepo node (caller-mapped from ResolveMonorepo).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MonorepoEdgeFacts {
    /// Mount name inside the parent (directory segment under the parent path).
    pub mount_name: String,
    /// Child spool id the edge points at.
    pub child_spool_id: String,
    /// When `Some`, the edge is selected and the subtree is walked. When `None`,
    /// the edge is withheld (see [`skip_reason`]).
    pub child: Option<MonorepoNodeFacts>,
    /// Reason recorded when `child` is `None`. Ignored when `child` is present.
    /// Missing reason with no child maps to [`MonorepoEdgeSkipReason::Unspecified`].
    pub skip_reason: Option<MonorepoEdgeSkipReason>,
}

/// Pure facts for one resolved monorepo node (no gRPC / network types).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MonorepoNodeFacts {
    pub spool_id: String,
    /// Content-facet state to materialize. `None` = empty checkout at the mount.
    /// For the root this is the spool's content head; for descendants the server
    /// already pins the parent's edge-anchored state into this field.
    pub content_state: Option<StateId>,
    pub edges: Vec<MonorepoEdgeFacts>,
}

/// One per-node materialize step in monorepo work order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MonorepoNodePlan {
    pub spool_id: String,
    pub content_state: Option<StateId>,
    /// Destination path relative to the clone root. Root is `""`.
    pub rel_path: PathBuf,
}

impl MonorepoNodePlan {
    /// Absolute destination for this node given the clone root.
    pub fn dest_path(&self, clone_root: &Path) -> PathBuf {
        if self.rel_path.as_os_str().is_empty() {
            clone_root.to_path_buf()
        } else {
            clone_root.join(&self.rel_path)
        }
    }
}

/// A child edge that was not selected, with the reason. Reported; never fatal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MonorepoSkippedChild {
    pub child_spool_id: String,
    pub mount_name: String,
    /// Path the child would have mounted at (relative to clone root).
    pub rel_path: PathBuf,
    pub reason: MonorepoEdgeSkipReason,
}

impl MonorepoSkippedChild {
    pub fn reason_label(&self) -> &'static str {
        self.reason.as_str()
    }
}

/// Ordered monorepo clone plan: selected nodes (pre-order) plus withheld edges.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MonorepoClonePlan {
    /// Selected nodes in pre-order (root first). Parent always precedes children.
    pub nodes: Vec<MonorepoNodePlan>,
    /// Child edges recorded but not descended.
    pub skipped: Vec<MonorepoSkippedChild>,
}

/// Reject `--depth` / `--lazy` / `--filter` for recursive monorepo clones.
///
/// These knobs change single-spool pull semantics and do not compose across the
/// anchored-state monorepo walk in the first cut.
pub fn validate_monorepo_clone_options(
    depth: Option<u32>,
    lazy: bool,
    filter: Option<&str>,
) -> Result<(), ClonePlanError> {
    validate_clone_mode_options(
        &CloneMode::NetworkHosted { recursive: true },
        depth,
        lazy,
        filter,
    )
}

/// Plan monorepo materialize order from pure child-tree facts.
///
/// Applies path anchoring and child selection rules. Does not perform hosted
/// RPC or write to disk.
pub fn plan_monorepo_clone(root: &MonorepoNodeFacts) -> MonorepoClonePlan {
    let mut plan = MonorepoClonePlan::default();
    walk_monorepo_node(&mut plan, root, PathBuf::new());
    plan
}

fn walk_monorepo_node(plan: &mut MonorepoClonePlan, node: &MonorepoNodeFacts, rel_path: PathBuf) {
    // Always emit a node plan (including empty content) so the mount exists.
    plan.nodes.push(MonorepoNodePlan {
        spool_id: node.spool_id.clone(),
        content_state: node.content_state,
        rel_path: rel_path.clone(),
    });

    for edge in &node.edges {
        let child_rel = rel_path.join(&edge.mount_name);
        match &edge.child {
            Some(child) => walk_monorepo_node(plan, child, child_rel),
            None => {
                let reason = edge
                    .skip_reason
                    .unwrap_or(MonorepoEdgeSkipReason::Unspecified);
                plan.skipped.push(MonorepoSkippedChild {
                    child_spool_id: edge.child_spool_id.clone(),
                    mount_name: edge.mount_name.clone(),
                    rel_path: child_rel,
                    reason,
                });
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Monorepo per-node execution scaffolding (pure step list)
// ---------------------------------------------------------------------------
//
// [`plan_monorepo_clone`] decides *which* nodes to place and *where*. The
// helpers below decide *how* each selected node is materialized as an ordered
// list of pure steps. CLI matches on each step and performs FS / hosted I/O
// (create dirs, `Repository::init`, fetch_state, goto, origin mapping).

/// One pure execution step for materializing a single monorepo node.
///
/// Order is fixed by [`plan_monorepo_node_steps`]. Fetch/materialize carry the
/// content-state payload so the CLI does not re-branch on `Option`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MonorepoNodeExecutionStep {
    /// Ensure mount destination is usable (parent dirs, create dest path).
    ValidateDest,
    /// Initialize a Heddle repository at the mount (`Repository::init`).
    InitRepo,
    /// Hosted fetch of the node's content-state object closure.
    FetchContent { state: StateId },
    /// Materialize worktree from the fetched state
    /// (`goto_from_materialized_state`).
    MaterializeState { state: StateId },
    /// Seed origin/remote mapping so the placed spool tracks its upstream.
    RecordMapping,
}

impl MonorepoNodeExecutionStep {
    /// Stable short label for tests and diagnostics.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::ValidateDest => "validate_dest",
            Self::InitRepo => "init_repo",
            Self::FetchContent { .. } => "fetch_content",
            Self::MaterializeState { .. } => "materialize_state",
            Self::RecordMapping => "record_mapping",
        }
    }
}

/// Mode flags that gate optional per-node monorepo materialize steps.
///
/// Fetch/materialize are gated by [`MonorepoNodePlan::content_state`] (not by
/// these flags). First cut only toggles origin mapping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MonorepoNodeStepOptions {
    /// When true, emit [`MonorepoNodeExecutionStep::RecordMapping`] (default).
    pub record_mapping: bool,
}

impl Default for MonorepoNodeStepOptions {
    fn default() -> Self {
        Self {
            record_mapping: true,
        }
    }
}

/// One selected monorepo node plus its ordered pure execution steps.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MonorepoNodeExecution {
    pub node: MonorepoNodePlan,
    pub steps: Vec<MonorepoNodeExecutionStep>,
}

/// Aggregate monorepo execution plan: per-node steps in pre-order + skipped edges.
///
/// Built from a [`MonorepoClonePlan`] via [`plan_monorepo_execution`]. Preserves
/// work order: parent node steps always complete before a child's.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MonorepoExecutionPlan {
    /// Selected nodes with steps, same pre-order as [`MonorepoClonePlan::nodes`].
    pub nodes: Vec<MonorepoNodeExecution>,
    /// Child edges recorded but not descended (copied from the clone plan).
    pub skipped: Vec<MonorepoSkippedChild>,
}

impl MonorepoExecutionPlan {
    /// Number of selected nodes (placement count).
    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }
}

/// Plan ordered pure steps for one monorepo node.
///
/// Always emits ValidateDest → InitRepo. When `node.content_state` is set,
/// appends FetchContent then MaterializeState with that state. When
/// `options.record_mapping` is true (default), appends RecordMapping.
/// Empty content still produces ValidateDest + InitRepo (+ optional mapping)
/// so the mount is an initialized empty repo.
pub fn plan_monorepo_node_steps(
    node: &MonorepoNodePlan,
    options: &MonorepoNodeStepOptions,
) -> Vec<MonorepoNodeExecutionStep> {
    let mut steps = vec![
        MonorepoNodeExecutionStep::ValidateDest,
        MonorepoNodeExecutionStep::InitRepo,
    ];
    if let Some(state) = node.content_state {
        steps.push(MonorepoNodeExecutionStep::FetchContent { state });
        steps.push(MonorepoNodeExecutionStep::MaterializeState { state });
    }
    if options.record_mapping {
        steps.push(MonorepoNodeExecutionStep::RecordMapping);
    }
    steps
}

/// Expand a monorepo clone worklist into per-node pure execution steps.
///
/// Does not perform I/O. Skipped edges are copied through unchanged.
pub fn plan_monorepo_execution(
    clone_plan: &MonorepoClonePlan,
    options: &MonorepoNodeStepOptions,
) -> MonorepoExecutionPlan {
    MonorepoExecutionPlan {
        nodes: clone_plan
            .nodes
            .iter()
            .map(|node| MonorepoNodeExecution {
                node: node.clone(),
                steps: plan_monorepo_node_steps(node, options),
            })
            .collect(),
        skipped: clone_plan.skipped.clone(),
    }
}

// ---------------------------------------------------------------------------
// Monorepo step validation, progress labels, result summary (pure)
// ---------------------------------------------------------------------------
//
// [`plan_monorepo_node_steps`] emits ordered steps; the helpers below check
// ordering invariants before I/O, name unstyled progress labels for a step
// inside a multi-node walk, and assemble placed/skipped counts for the
// clone result. CLI still owns FS / hosted RPC and TTY styling.

/// Failures from pure monorepo node step ordering validation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MonorepoNodeExecutionError {
    /// Step list is empty (planner always emits at least ValidateDest + InitRepo).
    EmptySteps,
    /// Required scaffold step missing.
    MissingStep { step: &'static str },
    /// A step appeared before its prerequisites or after a later-ranked step.
    OutOfOrder {
        step: &'static str,
        detail: &'static str,
    },
    /// [`MonorepoNodeExecutionStep::MaterializeState`] without a prior Fetch.
    MaterializeWithoutFetch,
    /// [`MonorepoNodeExecutionStep::FetchContent`] not followed by Materialize.
    FetchWithoutMaterialize,
    /// Fetch and Materialize carry different content-state ids.
    FetchMaterializeStateMismatch {
        fetch: StateId,
        materialize: StateId,
    },
}

impl std::fmt::Display for MonorepoNodeExecutionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptySteps => write!(f, "monorepo node execution steps are empty"),
            Self::MissingStep { step } => {
                write!(f, "monorepo node execution missing required step '{step}'")
            }
            Self::OutOfOrder { step, detail } => {
                write!(
                    f,
                    "monorepo node execution step '{step}' out of order: {detail}"
                )
            }
            Self::MaterializeWithoutFetch => {
                write!(f, "monorepo MaterializeState requires FetchContent first")
            }
            Self::FetchWithoutMaterialize => write!(
                f,
                "monorepo FetchContent requires a following MaterializeState"
            ),
            Self::FetchMaterializeStateMismatch { fetch, materialize } => write!(
                f,
                "monorepo FetchContent state {fetch} does not match MaterializeState {materialize}"
            ),
        }
    }
}

impl std::error::Error for MonorepoNodeExecutionError {}

/// Rank used only for ordering checks (lower must not follow higher).
fn monorepo_step_rank(step: &MonorepoNodeExecutionStep) -> u8 {
    match step {
        MonorepoNodeExecutionStep::ValidateDest => 0,
        MonorepoNodeExecutionStep::InitRepo => 1,
        MonorepoNodeExecutionStep::FetchContent { .. } => 2,
        MonorepoNodeExecutionStep::MaterializeState { .. } => 3,
        MonorepoNodeExecutionStep::RecordMapping => 4,
    }
}

/// Validate ordering invariants for one node's pure monorepo steps.
///
/// Invariants:
/// - Non-empty; must include ValidateDest then InitRepo (scaffold).
/// - Steps appear at most once and in rank order (ValidateDest → InitRepo →
///   optional FetchContent → optional MaterializeState → optional RecordMapping).
/// - InitRepo precedes Fetch / Materialize / RecordMapping.
/// - FetchContent and MaterializeState are paired with the same [`StateId`].
///
/// Does not perform I/O. Plans from [`plan_monorepo_node_steps`] always pass.
pub fn validate_monorepo_node_execution(
    steps: &[MonorepoNodeExecutionStep],
) -> Result<(), MonorepoNodeExecutionError> {
    if steps.is_empty() {
        return Err(MonorepoNodeExecutionError::EmptySteps);
    }

    let mut seen_validate = false;
    let mut seen_init = false;
    let mut pending_fetch: Option<StateId> = None;
    let mut last_rank: Option<u8> = None;

    for step in steps {
        let rank = monorepo_step_rank(step);
        if let Some(prev) = last_rank
            && rank <= prev
        {
            return Err(MonorepoNodeExecutionError::OutOfOrder {
                step: step.as_str(),
                detail: "steps must be unique and strictly increasing in rank",
            });
        }
        last_rank = Some(rank);

        match step {
            MonorepoNodeExecutionStep::ValidateDest => {
                seen_validate = true;
            }
            MonorepoNodeExecutionStep::InitRepo => {
                if !seen_validate {
                    return Err(MonorepoNodeExecutionError::OutOfOrder {
                        step: step.as_str(),
                        detail: "InitRepo requires ValidateDest first",
                    });
                }
                seen_init = true;
            }
            MonorepoNodeExecutionStep::FetchContent { state } => {
                if !seen_init {
                    return Err(MonorepoNodeExecutionError::OutOfOrder {
                        step: step.as_str(),
                        detail: "Init before Fetch",
                    });
                }
                pending_fetch = Some(*state);
            }
            MonorepoNodeExecutionStep::MaterializeState { state } => {
                if !seen_init {
                    return Err(MonorepoNodeExecutionError::OutOfOrder {
                        step: step.as_str(),
                        detail: "Init before Materialize",
                    });
                }
                match pending_fetch {
                    None => return Err(MonorepoNodeExecutionError::MaterializeWithoutFetch),
                    Some(fetch) if fetch != *state => {
                        return Err(MonorepoNodeExecutionError::FetchMaterializeStateMismatch {
                            fetch,
                            materialize: *state,
                        });
                    }
                    Some(_) => {
                        pending_fetch = None;
                    }
                }
            }
            MonorepoNodeExecutionStep::RecordMapping => {
                if !seen_init {
                    return Err(MonorepoNodeExecutionError::OutOfOrder {
                        step: step.as_str(),
                        detail: "Init before RecordMapping",
                    });
                }
                if pending_fetch.is_some() {
                    return Err(MonorepoNodeExecutionError::FetchWithoutMaterialize);
                }
            }
        }
    }

    if !seen_validate {
        return Err(MonorepoNodeExecutionError::MissingStep {
            step: MonorepoNodeExecutionStep::ValidateDest.as_str(),
        });
    }
    if !seen_init {
        return Err(MonorepoNodeExecutionError::MissingStep {
            step: MonorepoNodeExecutionStep::InitRepo.as_str(),
        });
    }
    if pending_fetch.is_some() {
        return Err(MonorepoNodeExecutionError::FetchWithoutMaterialize);
    }

    Ok(())
}

/// Validate every selected node's step list in a monorepo execution plan.
pub fn validate_monorepo_execution(
    plan: &MonorepoExecutionPlan,
) -> Result<(), MonorepoNodeExecutionError> {
    for node_exec in &plan.nodes {
        validate_monorepo_node_execution(&node_exec.steps)?;
    }
    Ok(())
}

/// Pure progress label for one step inside a multi-node monorepo clone walk.
///
/// CLI owns TTY styling; this is unstyled display data only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MonorepoExecutionProgress {
    /// 0-based index into [`MonorepoExecutionPlan::nodes`].
    pub node_index: usize,
    /// Total selected nodes in the plan.
    pub total_nodes: usize,
    /// 1-based human node ordinal (`node_index + 1`, floored at 1 when total is 0).
    pub node_display: usize,
    /// Stable step id from [`MonorepoNodeExecutionStep::as_str`].
    pub step: &'static str,
}

impl MonorepoExecutionProgress {
    /// Compact unstyled label, e.g. `[1/3] init_repo`.
    pub fn label(&self) -> String {
        format!("[{}/{}] {}", self.node_display, self.total_nodes, self.step)
    }
}

/// Build pure display labels for a monorepo node step at `node_index` of `total`.
///
/// `node_index` is 0-based. `total` is the plan's selected node count.
pub fn monorepo_execution_progress(
    node_index: usize,
    total: usize,
    step: &MonorepoNodeExecutionStep,
) -> MonorepoExecutionProgress {
    MonorepoExecutionProgress {
        node_index,
        total_nodes: total,
        node_display: node_index.saturating_add(1),
        step: step.as_str(),
    }
}

/// One successfully planned placement for monorepo clone result assembly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MonorepoPlacedNodeSummary {
    pub spool_id: String,
    /// Destination path relative to the clone root. Root is `""`.
    pub rel_path: PathBuf,
    pub content_state: Option<StateId>,
    /// True when the node plan included fetch + materialize (had content).
    pub materialized_content: bool,
}

/// Aggregate placed/skipped summary for a monorepo clone result (pure).
///
/// Assembled from a validated execution plan after all selected nodes succeed.
/// Skipped edges are never fatal; they are reported here for text/JSON output.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MonorepoCloneResultSummary {
    pub placed_count: usize,
    pub skipped_count: usize,
    pub placed: Vec<MonorepoPlacedNodeSummary>,
    pub skipped: Vec<MonorepoSkippedChild>,
}

impl MonorepoCloneResultSummary {
    /// Unstyled headline, e.g. `Cloned monorepo org/root (2 spools placed).`
    pub fn headline(&self, root_path: &str) -> String {
        let unit = if self.placed_count == 1 {
            "spool"
        } else {
            "spools"
        };
        format!(
            "Cloned monorepo {root_path} ({} {unit} placed).",
            self.placed_count
        )
    }

    /// Unstyled skip section header when any edges were withheld; `None` if empty.
    pub fn skipped_header(&self) -> Option<String> {
        if self.skipped_count == 0 {
            None
        } else {
            Some(format!(
                "{} child spool(s) skipped (not part of your coherent slice):",
                self.skipped_count
            ))
        }
    }
}

/// Assemble placed/skipped summary from a monorepo execution plan (no I/O).
///
/// Call after every selected node has been materialized successfully. Counts
/// reflect plan size (success path), not partial progress mid-walk.
pub fn assemble_monorepo_clone_result_summary(
    plan: &MonorepoExecutionPlan,
) -> MonorepoCloneResultSummary {
    let placed: Vec<MonorepoPlacedNodeSummary> = plan
        .nodes
        .iter()
        .map(|node_exec| {
            let materialized_content = node_exec
                .steps
                .iter()
                .any(|step| matches!(step, MonorepoNodeExecutionStep::MaterializeState { .. }));
            MonorepoPlacedNodeSummary {
                spool_id: node_exec.node.spool_id.clone(),
                rel_path: node_exec.node.rel_path.clone(),
                content_state: node_exec.node.content_state,
                materialized_content,
            }
        })
        .collect();
    MonorepoCloneResultSummary {
        placed_count: placed.len(),
        skipped_count: plan.skipped.len(),
        placed,
        skipped: plan.skipped.clone(),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn base_clone_options(remote: &str, local: &str) -> ClonePlanOptions {
        ClonePlanOptions {
            remote: remote.to_string(),
            local: PathBuf::from(local),
            thread: None,
            depth: None,
            lazy: false,
            filter: None,
            recursive: false,
            insecure: false,
        }
    }

    #[test]
    fn absolute_path_joins_relative_against_cwd() {
        let cwd = Path::new("/work");
        assert_eq!(
            absolute_path(Path::new("dest"), cwd),
            PathBuf::from("/work/dest")
        );
        assert_eq!(
            absolute_path(Path::new("/abs/dest"), cwd),
            PathBuf::from("/abs/dest")
        );
    }

    #[test]
    fn resolve_clone_destination_uses_absolute_policy() {
        let cwd = Path::new("/tmp/repo");
        assert_eq!(
            resolve_clone_destination(Path::new("clone-here"), cwd),
            PathBuf::from("/tmp/repo/clone-here")
        );
    }

    #[test]
    fn validate_clone_destination_refuses_existing() {
        assert!(matches!(
            validate_clone_destination(Path::new("/tmp/x"), true),
            Err(ClonePlanError::DestinationExists { .. })
        ));
        assert!(validate_clone_destination(Path::new("/tmp/x"), false).is_ok());
    }

    #[test]
    fn normalize_clone_depth_drops_zero() {
        assert_eq!(normalize_clone_depth(None), None);
        assert_eq!(normalize_clone_depth(Some(0)), None);
        assert_eq!(normalize_clone_depth(Some(1)), Some(1));
        assert_eq!(normalize_clone_depth(Some(5)), Some(5));
    }

    #[test]
    fn looks_like_local_path_shapes() {
        assert!(looks_like_local_path("/abs/path"));
        assert!(looks_like_local_path("."));
        assert!(looks_like_local_path(".."));
        assert!(looks_like_local_path("./rel"));
        assert!(looks_like_local_path("../up"));
        assert!(looks_like_local_path("~/home"));
        assert!(!looks_like_local_path("host:8421/repo"));
        assert!(!looks_like_local_path("https://example.com/repo.git"));
    }

    #[test]
    fn looks_like_git_overlay_url_shapes() {
        assert!(looks_like_git_overlay_url("https://example.com/repo.git"));
        assert!(looks_like_git_overlay_url("git@github.com:org/repo.git"));
        assert!(looks_like_git_overlay_url("ssh://git@host/repo.git"));
        assert!(!looks_like_git_overlay_url("localhost:8421/acme/heddle"));
        assert!(!looks_like_git_overlay_url("/local/path"));
    }

    #[test]
    fn plan_clone_refuses_existing_destination() {
        let opts = base_clone_options("file:///src", "/dest");
        let err = plan_clone(
            &opts,
            &ClonePlanFacts {
                destination_exists: true,
                remote_source: CloneRemoteSource::Local {
                    path: PathBuf::from("/src"),
                    has_heddle: true,
                    is_git: false,
                },
            },
        )
        .unwrap_err();
        assert!(matches!(err, ClonePlanError::DestinationExists { .. }));
    }

    #[test]
    fn plan_clone_local_heddle_vs_git_overlay() {
        let opts = base_clone_options("file:///src", "/dest");
        let heddle = plan_clone(
            &opts,
            &ClonePlanFacts {
                destination_exists: false,
                remote_source: CloneRemoteSource::Local {
                    path: PathBuf::from("/src"),
                    has_heddle: true,
                    is_git: true,
                },
            },
        )
        .unwrap();
        assert!(matches!(heddle.mode, CloneMode::LocalHeddle { .. }));
        assert!(!heddle.security.requires_network_session);

        let git = plan_clone(
            &opts,
            &ClonePlanFacts {
                destination_exists: false,
                remote_source: CloneRemoteSource::Local {
                    path: PathBuf::from("/src"),
                    has_heddle: false,
                    is_git: true,
                },
            },
        )
        .unwrap();
        assert!(matches!(git.mode, CloneMode::LocalGitOverlay { .. }));
    }

    #[test]
    fn plan_clone_network_security_and_effective_lazy() {
        let mut opts = base_clone_options("heddle://host:1/repo", "/dest");
        opts.insecure = true;
        opts.lazy = false;
        opts.filter = Some("blob:none".into());
        opts.depth = Some(0);

        let plan = plan_clone(
            &opts,
            &ClonePlanFacts {
                destination_exists: false,
                remote_source: CloneRemoteSource::Network {
                    has_repo_path: true,
                },
            },
        )
        .unwrap();

        assert_eq!(plan.mode, CloneMode::NetworkHosted { recursive: false });
        assert!(plan.security.requires_network_session);
        assert!(plan.security.allow_insecure);
        assert!(plan.effective_lazy);
        assert_eq!(plan.depth, None);
    }

    #[test]
    fn plan_clone_monorepo_requires_hosted() {
        let mut opts = base_clone_options("/local/repo", "/dest");
        opts.recursive = true;
        let err = plan_clone(
            &opts,
            &ClonePlanFacts {
                destination_exists: false,
                remote_source: CloneRemoteSource::Local {
                    path: PathBuf::from("/local/repo"),
                    has_heddle: true,
                    is_git: false,
                },
            },
        )
        .unwrap_err();
        assert!(matches!(err, ClonePlanError::MonorepoRequiresHosted { .. }));

        let mut opts = base_clone_options("https://example.com/r.git", "/dest");
        opts.recursive = true;
        let err = plan_clone(
            &opts,
            &ClonePlanFacts {
                destination_exists: false,
                remote_source: CloneRemoteSource::Unparsed,
            },
        )
        .unwrap_err();
        assert!(matches!(err, ClonePlanError::MonorepoRequiresHosted { .. }));
    }

    #[test]
    fn plan_clone_unparsed_git_url_and_invalid() {
        let plan = plan_clone(
            &base_clone_options("https://example.com/r.git", "/dest"),
            &ClonePlanFacts {
                destination_exists: false,
                remote_source: CloneRemoteSource::Unparsed,
            },
        )
        .unwrap();
        assert_eq!(plan.mode, CloneMode::GitOverlayUrl);

        let err = plan_clone(
            &base_clone_options("not-a-remote", "/dest"),
            &ClonePlanFacts {
                destination_exists: false,
                remote_source: CloneRemoteSource::Unparsed,
            },
        )
        .unwrap_err();
        assert!(matches!(err, ClonePlanError::InvalidRemoteUrl { .. }));

        let err = plan_clone(
            &base_clone_options("./missing", "/dest"),
            &ClonePlanFacts {
                destination_exists: false,
                remote_source: CloneRemoteSource::Unparsed,
            },
        )
        .unwrap_err();
        assert!(matches!(
            err,
            ClonePlanError::RemoteLooksLikeMissingLocalPath { .. }
        ));
    }

    #[test]
    fn plan_clone_rejects_unsupported_mode_options() {
        let mut opts = base_clone_options("https://example.com/r.git", "/dest");
        opts.depth = Some(1);
        let err = plan_clone(
            &opts,
            &ClonePlanFacts {
                destination_exists: false,
                remote_source: CloneRemoteSource::Unparsed,
            },
        )
        .unwrap_err();
        assert!(matches!(
            err,
            ClonePlanError::UnsupportedOption {
                flag: UnsupportedCloneFlag::Depth,
                mode: "git-overlay",
                ..
            }
        ));

        let mut opts = base_clone_options("file:///src", "/dest");
        opts.lazy = true;
        let err = plan_clone(
            &opts,
            &ClonePlanFacts {
                destination_exists: false,
                remote_source: CloneRemoteSource::Local {
                    path: PathBuf::from("/src"),
                    has_heddle: true,
                    is_git: false,
                },
            },
        )
        .unwrap_err();
        assert!(matches!(
            err,
            ClonePlanError::UnsupportedOption {
                flag: UnsupportedCloneFlag::Lazy,
                mode: "local",
                ..
            }
        ));

        let mut opts = base_clone_options("heddle://h:1/r", "/dest");
        opts.recursive = true;
        opts.filter = Some("blob:none".into());
        let err = plan_clone(
            &opts,
            &ClonePlanFacts {
                destination_exists: false,
                remote_source: CloneRemoteSource::Network {
                    has_repo_path: true,
                },
            },
        )
        .unwrap_err();
        assert!(matches!(
            err,
            ClonePlanError::UnsupportedOption {
                flag: UnsupportedCloneFlag::Filter,
                mode: "monorepo",
                ..
            }
        ));
    }

    #[test]
    fn plan_adopt_path_resolution_and_conflict() {
        let cwd = PathBuf::from("/work");
        let plan = plan_adopt(&AdoptPlanOptions {
            path: None,
            repo_flag: None,
            cwd: cwd.clone(),
            refs: vec![],
        })
        .unwrap();
        assert_eq!(plan.start_path, cwd);
        assert!(plan.import_all_refs);

        let plan = plan_adopt(&AdoptPlanOptions {
            path: Some(PathBuf::from("repo")),
            repo_flag: None,
            cwd: PathBuf::from("/work"),
            refs: vec!["main".into()],
        })
        .unwrap();
        assert_eq!(plan.start_path, PathBuf::from("repo"));
        assert!(!plan.import_all_refs);

        let plan = plan_adopt(&AdoptPlanOptions {
            path: Some(PathBuf::from("repo")),
            repo_flag: Some(PathBuf::from("/work/repo")),
            cwd: PathBuf::from("/work"),
            refs: vec![],
        })
        .unwrap();
        assert_eq!(plan.start_path, PathBuf::from("repo"));

        let err = plan_adopt(&AdoptPlanOptions {
            path: Some(PathBuf::from("a")),
            repo_flag: Some(PathBuf::from("b")),
            cwd: PathBuf::from("/work"),
            refs: vec![],
        })
        .unwrap_err();
        assert!(matches!(err, AdoptPlanError::PathConflict { .. }));
    }

    #[test]
    fn assemble_security_only_for_network() {
        let local = assemble_clone_security_preflight(
            &CloneMode::LocalHeddle {
                remote_path: PathBuf::from("/s"),
            },
            true,
        );
        assert!(!local.allow_insecure);
        assert!(!local.requires_network_session);

        let net =
            assemble_clone_security_preflight(&CloneMode::NetworkHosted { recursive: false }, true);
        assert!(net.allow_insecure);
        assert!(net.requires_network_session);
    }

    // ---- monorepo pure planning ----

    fn cid(seed: u8) -> StateId {
        StateId::from_bytes([seed; 32])
    }

    fn leaf(spool_id: &str, content: u8) -> MonorepoNodeFacts {
        MonorepoNodeFacts {
            spool_id: spool_id.to_string(),
            content_state: Some(cid(content)),
            edges: vec![],
        }
    }

    fn selected_edge(mount: &str, child_id: &str, child: MonorepoNodeFacts) -> MonorepoEdgeFacts {
        MonorepoEdgeFacts {
            mount_name: mount.to_string(),
            child_spool_id: child_id.to_string(),
            child: Some(child),
            skip_reason: None,
        }
    }

    fn skipped_edge(
        mount: &str,
        child_id: &str,
        reason: MonorepoEdgeSkipReason,
    ) -> MonorepoEdgeFacts {
        MonorepoEdgeFacts {
            mount_name: mount.to_string(),
            child_spool_id: child_id.to_string(),
            child: None,
            skip_reason: Some(reason),
        }
    }

    /// root (c1)
    ///  ├─ libs/  -> child-a (c2)
    ///  │            └─ vendor/ -> grandchild (c3)
    ///  └─ secret/ -> child-b  [SKIPPED: unreadable]
    fn fixture_tree() -> MonorepoNodeFacts {
        let grandchild = leaf("acme/grandchild", 3);
        let child_a = MonorepoNodeFacts {
            spool_id: "acme/child-a".to_string(),
            content_state: Some(cid(2)),
            edges: vec![selected_edge("vendor", "acme/grandchild", grandchild)],
        };
        MonorepoNodeFacts {
            spool_id: "acme/root".to_string(),
            content_state: Some(cid(1)),
            edges: vec![
                selected_edge("libs", "acme/child-a", child_a),
                skipped_edge("secret", "acme/child-b", MonorepoEdgeSkipReason::Unreadable),
            ],
        }
    }

    #[test]
    fn plan_monorepo_places_nodes_at_mount_paths_in_preorder() {
        let plan = plan_monorepo_clone(&fixture_tree());

        assert_eq!(plan.nodes.len(), 3, "root + child-a + grandchild");

        assert_eq!(plan.nodes[0].spool_id, "acme/root");
        assert_eq!(plan.nodes[0].rel_path, PathBuf::new());
        assert_eq!(plan.nodes[0].content_state, Some(cid(1)));

        assert_eq!(plan.nodes[1].spool_id, "acme/child-a");
        assert_eq!(plan.nodes[1].rel_path, PathBuf::from("libs"));
        assert_eq!(plan.nodes[1].content_state, Some(cid(2)));

        assert_eq!(plan.nodes[2].spool_id, "acme/grandchild");
        assert_eq!(plan.nodes[2].rel_path, PathBuf::from("libs").join("vendor"));
        assert_eq!(plan.nodes[2].content_state, Some(cid(3)));
    }

    #[test]
    fn plan_monorepo_records_skipped_children_and_does_not_select_them() {
        let plan = plan_monorepo_clone(&fixture_tree());

        assert_eq!(plan.skipped.len(), 1);
        let sk = &plan.skipped[0];
        assert_eq!(sk.child_spool_id, "acme/child-b");
        assert_eq!(sk.mount_name, "secret");
        assert_eq!(sk.rel_path, PathBuf::from("secret"));
        assert_eq!(sk.reason, MonorepoEdgeSkipReason::Unreadable);
        assert_eq!(sk.reason_label(), "unreadable");

        assert!(
            plan.nodes.iter().all(|n| n.spool_id != "acme/child-b"),
            "skipped child must not appear as a materialize node"
        );
    }

    #[test]
    fn monorepo_node_dest_path_joins_root() {
        let plan = plan_monorepo_clone(&fixture_tree());
        let root = Path::new("/tmp/mono");

        assert_eq!(plan.nodes[0].dest_path(root), PathBuf::from("/tmp/mono"));
        assert_eq!(
            plan.nodes[1].dest_path(root),
            PathBuf::from("/tmp/mono/libs")
        );
        assert_eq!(
            plan.nodes[2].dest_path(root),
            PathBuf::from("/tmp/mono/libs/vendor")
        );
    }

    #[test]
    fn plan_monorepo_empty_content_still_walks_children() {
        let child = leaf("acme/child", 5);
        let root = MonorepoNodeFacts {
            spool_id: "acme/root".to_string(),
            content_state: None,
            edges: vec![selected_edge("sub", "acme/child", child)],
        };
        let plan = plan_monorepo_clone(&root);

        assert_eq!(plan.nodes.len(), 2);
        assert_eq!(plan.nodes[0].spool_id, "acme/root");
        assert_eq!(plan.nodes[0].content_state, None);
        assert_eq!(plan.nodes[1].spool_id, "acme/child");
        assert_eq!(plan.nodes[1].rel_path, PathBuf::from("sub"));
        assert_eq!(plan.nodes[1].content_state, Some(cid(5)));
    }

    #[test]
    fn plan_monorepo_missing_skip_reason_defaults_to_unspecified() {
        let root = MonorepoNodeFacts {
            spool_id: "root".to_string(),
            content_state: Some(cid(1)),
            edges: vec![MonorepoEdgeFacts {
                mount_name: "m".into(),
                child_spool_id: "child".into(),
                child: None,
                skip_reason: None,
            }],
        };
        let plan = plan_monorepo_clone(&root);
        assert_eq!(plan.skipped.len(), 1);
        assert_eq!(plan.skipped[0].reason, MonorepoEdgeSkipReason::Unspecified);
        assert_eq!(plan.skipped[0].reason_label(), "unspecified");
    }

    #[test]
    fn monorepo_rel_display_and_wire_skip() {
        assert_eq!(monorepo_rel_display(Path::new("")), ".");
        assert_eq!(monorepo_rel_display(Path::new("libs")), "libs");
        assert_eq!(
            MonorepoEdgeSkipReason::from_wire_i32(1),
            Some(MonorepoEdgeSkipReason::Unreadable)
        );
        assert_eq!(MonorepoEdgeSkipReason::from_wire_i32(99), None);
    }

    #[test]
    fn monorepo_edge_skip_labels_are_stable() {
        for (reason, label) in [
            (MonorepoEdgeSkipReason::Unreadable, "unreadable"),
            (MonorepoEdgeSkipReason::Cycle, "cycle"),
            (MonorepoEdgeSkipReason::DepthBounded, "depth-bounded"),
        ] {
            assert_eq!(reason.as_str(), label);
            let root = MonorepoNodeFacts {
                spool_id: "root".to_string(),
                content_state: Some(cid(1)),
                edges: vec![skipped_edge("m", "child", reason)],
            };
            let plan = plan_monorepo_clone(&root);
            assert_eq!(plan.skipped[0].reason_label(), label);
        }
    }

    #[test]
    fn validate_monorepo_clone_options_refuses_filter_lazy_depth() {
        assert!(validate_monorepo_clone_options(None, false, None).is_ok());

        assert!(matches!(
            validate_monorepo_clone_options(None, false, Some("blob:none")),
            Err(ClonePlanError::UnsupportedOption {
                flag: UnsupportedCloneFlag::Filter,
                mode: "monorepo",
                ..
            })
        ));
        assert!(matches!(
            validate_monorepo_clone_options(None, true, None),
            Err(ClonePlanError::UnsupportedOption {
                flag: UnsupportedCloneFlag::Lazy,
                mode: "monorepo",
                ..
            })
        ));
        assert!(matches!(
            validate_monorepo_clone_options(Some(1), false, None),
            Err(ClonePlanError::UnsupportedOption {
                flag: UnsupportedCloneFlag::Depth,
                mode: "monorepo",
                ..
            })
        ));
    }

    #[test]
    fn selected_edge_with_skip_reason_still_descends() {
        // Selection is driven by presence of child facts, not skip_reason.
        let child = leaf("acme/child", 2);
        let root = MonorepoNodeFacts {
            spool_id: "root".to_string(),
            content_state: Some(cid(1)),
            edges: vec![MonorepoEdgeFacts {
                mount_name: "sub".into(),
                child_spool_id: "acme/child".into(),
                child: Some(child),
                skip_reason: Some(MonorepoEdgeSkipReason::Unreadable),
            }],
        };
        let plan = plan_monorepo_clone(&root);
        assert_eq!(plan.nodes.len(), 2);
        assert!(plan.skipped.is_empty());
        assert_eq!(plan.nodes[1].spool_id, "acme/child");
    }

    // ---- monorepo per-node execution scaffolding ----

    #[test]
    fn plan_monorepo_node_steps_full_content_order() {
        let node = MonorepoNodePlan {
            spool_id: "acme/root".into(),
            content_state: Some(cid(1)),
            rel_path: PathBuf::new(),
        };
        let steps = plan_monorepo_node_steps(&node, &MonorepoNodeStepOptions::default());
        assert_eq!(
            steps
                .iter()
                .map(MonorepoNodeExecutionStep::as_str)
                .collect::<Vec<_>>(),
            [
                "validate_dest",
                "init_repo",
                "fetch_content",
                "materialize_state",
                "record_mapping",
            ]
        );
        assert_eq!(
            steps[2],
            MonorepoNodeExecutionStep::FetchContent { state: cid(1) }
        );
        assert_eq!(
            steps[3],
            MonorepoNodeExecutionStep::MaterializeState { state: cid(1) }
        );
    }

    #[test]
    fn plan_monorepo_node_steps_empty_content_skips_fetch_and_materialize() {
        let node = MonorepoNodePlan {
            spool_id: "acme/empty".into(),
            content_state: None,
            rel_path: PathBuf::from("libs"),
        };
        let steps = plan_monorepo_node_steps(&node, &MonorepoNodeStepOptions::default());
        assert_eq!(
            steps,
            vec![
                MonorepoNodeExecutionStep::ValidateDest,
                MonorepoNodeExecutionStep::InitRepo,
                MonorepoNodeExecutionStep::RecordMapping,
            ]
        );
    }

    #[test]
    fn plan_monorepo_node_steps_can_omit_record_mapping() {
        let node = MonorepoNodePlan {
            spool_id: "acme/root".into(),
            content_state: Some(cid(9)),
            rel_path: PathBuf::new(),
        };
        let steps = plan_monorepo_node_steps(
            &node,
            &MonorepoNodeStepOptions {
                record_mapping: false,
            },
        );
        assert_eq!(
            steps
                .iter()
                .map(MonorepoNodeExecutionStep::as_str)
                .collect::<Vec<_>>(),
            [
                "validate_dest",
                "init_repo",
                "fetch_content",
                "materialize_state",
            ]
        );
        assert!(
            !steps
                .iter()
                .any(|s| matches!(s, MonorepoNodeExecutionStep::RecordMapping))
        );
    }

    #[test]
    fn plan_monorepo_execution_preserves_preorder_and_skipped() {
        let clone_plan = plan_monorepo_clone(&fixture_tree());
        let exec = plan_monorepo_execution(&clone_plan, &MonorepoNodeStepOptions::default());

        assert_eq!(exec.node_count(), 3);
        assert_eq!(exec.nodes.len(), clone_plan.nodes.len());
        assert_eq!(exec.skipped, clone_plan.skipped);

        // Pre-order preserved: root → libs → libs/vendor
        assert_eq!(exec.nodes[0].node.spool_id, "acme/root");
        assert_eq!(exec.nodes[1].node.spool_id, "acme/child-a");
        assert_eq!(exec.nodes[2].node.spool_id, "acme/grandchild");
        assert_eq!(
            exec.nodes[2].node.rel_path,
            PathBuf::from("libs").join("vendor")
        );

        // Every content-bearing node gets the full five-step sequence.
        for node_exec in &exec.nodes {
            assert_eq!(
                node_exec
                    .steps
                    .iter()
                    .map(MonorepoNodeExecutionStep::as_str)
                    .collect::<Vec<_>>(),
                [
                    "validate_dest",
                    "init_repo",
                    "fetch_content",
                    "materialize_state",
                    "record_mapping",
                ]
            );
        }
    }

    #[test]
    fn plan_monorepo_execution_empty_root_still_emits_scaffold_steps() {
        let child = leaf("acme/child", 5);
        let root = MonorepoNodeFacts {
            spool_id: "acme/root".to_string(),
            content_state: None,
            edges: vec![selected_edge("sub", "acme/child", child)],
        };
        let clone_plan = plan_monorepo_clone(&root);
        let exec = plan_monorepo_execution(&clone_plan, &MonorepoNodeStepOptions::default());

        assert_eq!(exec.nodes[0].node.content_state, None);
        assert_eq!(
            exec.nodes[0].steps,
            vec![
                MonorepoNodeExecutionStep::ValidateDest,
                MonorepoNodeExecutionStep::InitRepo,
                MonorepoNodeExecutionStep::RecordMapping,
            ]
        );
        // Child with content still gets fetch + materialize after parent.
        assert!(
            exec.nodes[1]
                .steps
                .iter()
                .any(|s| matches!(s, MonorepoNodeExecutionStep::FetchContent { .. }))
        );
    }

    // ---- monorepo step validation / progress / result summary ----

    #[test]
    fn validate_monorepo_node_execution_accepts_planner_output() {
        let full = MonorepoNodePlan {
            spool_id: "acme/root".into(),
            content_state: Some(cid(1)),
            rel_path: PathBuf::new(),
        };
        let empty = MonorepoNodePlan {
            spool_id: "acme/empty".into(),
            content_state: None,
            rel_path: PathBuf::from("libs"),
        };
        assert!(
            validate_monorepo_node_execution(&plan_monorepo_node_steps(
                &full,
                &MonorepoNodeStepOptions::default()
            ))
            .is_ok()
        );
        assert!(
            validate_monorepo_node_execution(&plan_monorepo_node_steps(
                &empty,
                &MonorepoNodeStepOptions::default()
            ))
            .is_ok()
        );
        assert!(
            validate_monorepo_node_execution(&plan_monorepo_node_steps(
                &full,
                &MonorepoNodeStepOptions {
                    record_mapping: false
                }
            ))
            .is_ok()
        );
    }

    #[test]
    fn validate_monorepo_node_execution_rejects_empty_and_missing_scaffold() {
        assert_eq!(
            validate_monorepo_node_execution(&[]),
            Err(MonorepoNodeExecutionError::EmptySteps)
        );
        assert_eq!(
            validate_monorepo_node_execution(&[MonorepoNodeExecutionStep::InitRepo]),
            Err(MonorepoNodeExecutionError::OutOfOrder {
                step: "init_repo",
                detail: "InitRepo requires ValidateDest first",
            })
        );
        assert_eq!(
            validate_monorepo_node_execution(&[MonorepoNodeExecutionStep::ValidateDest]),
            Err(MonorepoNodeExecutionError::MissingStep { step: "init_repo" })
        );
    }

    #[test]
    fn validate_monorepo_node_execution_requires_init_before_fetch() {
        let steps = vec![
            MonorepoNodeExecutionStep::ValidateDest,
            MonorepoNodeExecutionStep::FetchContent { state: cid(1) },
            MonorepoNodeExecutionStep::MaterializeState { state: cid(1) },
        ];
        assert_eq!(
            validate_monorepo_node_execution(&steps),
            Err(MonorepoNodeExecutionError::OutOfOrder {
                step: "fetch_content",
                detail: "Init before Fetch",
            })
        );
    }

    #[test]
    fn validate_monorepo_node_execution_pairs_fetch_and_materialize() {
        let fetch_only = vec![
            MonorepoNodeExecutionStep::ValidateDest,
            MonorepoNodeExecutionStep::InitRepo,
            MonorepoNodeExecutionStep::FetchContent { state: cid(1) },
        ];
        assert_eq!(
            validate_monorepo_node_execution(&fetch_only),
            Err(MonorepoNodeExecutionError::FetchWithoutMaterialize)
        );

        let materialize_only = vec![
            MonorepoNodeExecutionStep::ValidateDest,
            MonorepoNodeExecutionStep::InitRepo,
            MonorepoNodeExecutionStep::MaterializeState { state: cid(1) },
        ];
        assert_eq!(
            validate_monorepo_node_execution(&materialize_only),
            Err(MonorepoNodeExecutionError::MaterializeWithoutFetch)
        );

        let mismatch = vec![
            MonorepoNodeExecutionStep::ValidateDest,
            MonorepoNodeExecutionStep::InitRepo,
            MonorepoNodeExecutionStep::FetchContent { state: cid(1) },
            MonorepoNodeExecutionStep::MaterializeState { state: cid(2) },
        ];
        assert_eq!(
            validate_monorepo_node_execution(&mismatch),
            Err(MonorepoNodeExecutionError::FetchMaterializeStateMismatch {
                fetch: cid(1),
                materialize: cid(2),
            })
        );
    }

    #[test]
    fn validate_monorepo_node_execution_rejects_duplicate_or_reordered_steps() {
        let dup = vec![
            MonorepoNodeExecutionStep::ValidateDest,
            MonorepoNodeExecutionStep::InitRepo,
            MonorepoNodeExecutionStep::InitRepo,
        ];
        assert!(matches!(
            validate_monorepo_node_execution(&dup),
            Err(MonorepoNodeExecutionError::OutOfOrder {
                step: "init_repo",
                ..
            })
        ));

        let reordered = vec![
            MonorepoNodeExecutionStep::InitRepo,
            MonorepoNodeExecutionStep::ValidateDest,
        ];
        assert!(matches!(
            validate_monorepo_node_execution(&reordered),
            Err(MonorepoNodeExecutionError::OutOfOrder { .. })
        ));
    }

    #[test]
    fn validate_monorepo_execution_accepts_full_plan() {
        let clone_plan = plan_monorepo_clone(&fixture_tree());
        let exec = plan_monorepo_execution(&clone_plan, &MonorepoNodeStepOptions::default());
        assert!(validate_monorepo_execution(&exec).is_ok());
    }

    #[test]
    fn monorepo_execution_progress_labels_are_stable() {
        let step = MonorepoNodeExecutionStep::InitRepo;
        let progress = monorepo_execution_progress(0, 3, &step);
        assert_eq!(progress.node_index, 0);
        assert_eq!(progress.total_nodes, 3);
        assert_eq!(progress.node_display, 1);
        assert_eq!(progress.step, "init_repo");
        assert_eq!(progress.label(), "[1/3] init_repo");

        let fetch = MonorepoNodeExecutionStep::FetchContent { state: cid(9) };
        let p2 = monorepo_execution_progress(2, 3, &fetch);
        assert_eq!(p2.label(), "[3/3] fetch_content");
    }

    #[test]
    fn assemble_monorepo_clone_result_summary_counts_placed_and_skipped() {
        let clone_plan = plan_monorepo_clone(&fixture_tree());
        let exec = plan_monorepo_execution(&clone_plan, &MonorepoNodeStepOptions::default());
        let summary = assemble_monorepo_clone_result_summary(&exec);

        assert_eq!(summary.placed_count, 3);
        assert_eq!(summary.skipped_count, 1);
        assert_eq!(summary.placed.len(), 3);
        assert_eq!(summary.skipped.len(), 1);
        assert_eq!(summary.placed[0].spool_id, "acme/root");
        assert!(summary.placed[0].materialized_content);
        assert_eq!(summary.skipped[0].child_spool_id, "acme/child-b");

        assert_eq!(
            summary.headline("acme/root"),
            "Cloned monorepo acme/root (3 spools placed)."
        );
        assert_eq!(
            summary.skipped_header().as_deref(),
            Some("1 child spool(s) skipped (not part of your coherent slice):")
        );

        // Singular headline.
        let single = MonorepoCloneResultSummary {
            placed_count: 1,
            skipped_count: 0,
            placed: vec![],
            skipped: vec![],
        };
        assert_eq!(
            single.headline("solo"),
            "Cloned monorepo solo (1 spool placed)."
        );
        assert!(single.skipped_header().is_none());
    }

    #[test]
    fn assemble_summary_marks_empty_content_nodes_not_materialized() {
        let root = MonorepoNodeFacts {
            spool_id: "acme/root".to_string(),
            content_state: None,
            edges: vec![],
        };
        let exec = plan_monorepo_execution(
            &plan_monorepo_clone(&root),
            &MonorepoNodeStepOptions::default(),
        );
        let summary = assemble_monorepo_clone_result_summary(&exec);
        assert_eq!(summary.placed_count, 1);
        assert!(!summary.placed[0].materialized_content);
        assert_eq!(summary.placed[0].content_state, None);
    }
}
