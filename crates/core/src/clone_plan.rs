// SPDX-License-Identifier: Apache-2.0
//! Pure clone and adopt planning.
//!
//! Owns decision logic shared by `heddle clone` and `heddle adopt`:
//! - destination path validation and absolute-resolution policy
//! - remote mode selection (local path vs network hosted vs git-overlay URL)
//! - security preflight flag assembly (no network I/O)
//! - adopt start-path resolution and path-conflict policy
//!
//! Filesystem mutations, hosted RPC, git import, and recovery-advice
//! rendering stay CLI-owned. Callers gather cheap facts (path existence,
//! RemoteTarget parse result, git/.heddle probes), invoke these helpers,
//! then execute I/O from the plan.

use std::path::{Path, PathBuf};

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
}
