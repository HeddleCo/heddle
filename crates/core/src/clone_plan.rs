// SPDX-License-Identifier: Apache-2.0
//! Pure clone and adopt planning.
//!
//! Owns decision logic shared by `heddle clone` and `heddle adopt`:
//! - destination path validation and absolute-resolution policy
//! - remote mode selection (local path vs network hosted vs git-overlay URL)
//! - security preflight flag assembly (no network I/O)
//! - adopt start-path resolution and path-conflict policy
//! - monorepo recursive clone: child selection, path anchoring, work order
//!
//! Filesystem mutations, hosted RPC, git import, and recovery-advice
//! rendering stay CLI-owned. Callers gather cheap facts (path existence,
//! RemoteTarget parse result, git/.heddle probes, resolved monorepo trees),
//! invoke these helpers, then execute I/O from the plan.

use std::path::{Path, PathBuf};

use objects::object::ChangeId;

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
    pub content_state: Option<ChangeId>,
    pub edges: Vec<MonorepoEdgeFacts>,
}

/// One per-node materialize step in monorepo work order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MonorepoNodePlan {
    pub spool_id: String,
    pub content_state: Option<ChangeId>,
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

    fn cid(seed: u8) -> ChangeId {
        ChangeId::from_bytes([seed; 16])
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
}
