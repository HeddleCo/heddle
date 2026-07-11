// SPDX-License-Identifier: Apache-2.0
//! Remote domain helpers: list/show assembly and pure push/pull routing.
//!
//! - List/show: pure report types and default-resolution for `heddle remote
//!   list` / `heddle remote show`.
//! - Push/pull routing: capability → plan decisions (git-overlay mirror vs
//!   native fan-out, default thread selection). CLI probes the repo, calls
//!   these pure helpers, then owns network I/O and rendering.
//!
//! Mutation (add/remove/set-default) and push/pull network bodies stay outside
//! this module.

use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Result, anyhow};
use cli_shared::remote::{RemoteConfig, RemoteTarget};
use refs::Head;
use repo::{Repository, RepositoryCapability};
use serde::Serialize;
use sley::{
    GitConfig, Repository as SleyRepository,
    plumbing::sley_config::{
        ConfigIncludeContext, ConfigOriginKind, ConfigScope, ConfigStack, ConfigStackEntry,
    },
};

/// Machine JSON for `heddle remote list`.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct RemoteListReport {
    pub output_kind: &'static str,
    pub remotes: Vec<RemoteInfo>,
}

/// One remote entry for list/show machine output.
///
/// Field names match the existing CLI JSON contract (`name`, `url`, `source`,
/// `is_default`). `output_kind` is `Some("remote_show")` for show, omitted on
/// list rows.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct RemoteInfo {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_kind: Option<&'static str>,
    pub name: String,
    pub url: String,
    pub source: String,
    pub is_default: bool,
}

impl RemoteListReport {
    pub fn empty() -> Self {
        Self {
            output_kind: "remote_list",
            remotes: Vec::new(),
        }
    }
}

/// List remotes for an opened Heddle repository (merged heddle + git-overlay).
pub fn list_remotes(repo: &Repository) -> Result<RemoteListReport> {
    let items = merged_remote_items(repo)?;
    let default = resolved_default_remote_name(repo)?;
    Ok(RemoteListReport {
        output_kind: "remote_list",
        remotes: items
            .into_iter()
            .map(|(name, (url, source))| {
                let is_default = default.as_deref() == Some(name.as_str());
                RemoteInfo {
                    output_kind: None,
                    name,
                    url,
                    source,
                    is_default,
                }
            })
            .collect(),
    })
}

/// List remotes from a plain-Git worktree root (no Heddle metadata required).
pub fn list_plain_git_remotes(root: &Path) -> RemoteListReport {
    let items = plain_git_remote_items(root);
    let default = default_remote_from_items(&items);
    RemoteListReport {
        output_kind: "remote_list",
        remotes: items
            .into_iter()
            .map(|(name, url)| {
                let is_default = default.as_deref() == Some(name.as_str());
                RemoteInfo {
                    output_kind: None,
                    name,
                    url,
                    source: "git".to_string(),
                    is_default,
                }
            })
            .collect(),
    }
}

/// Show a single remote in a Heddle repository. Returns `Ok(None)` when the
/// name is not present in the merged remote set.
pub fn show_remote(repo: &Repository, name: &str) -> Result<Option<RemoteInfo>> {
    let items = merged_remote_items(repo)?;
    let default = resolved_default_remote_name(repo)?;
    let Some((url, source)) = items.get(name).cloned() else {
        return Ok(None);
    };
    Ok(Some(RemoteInfo {
        output_kind: Some("remote_show"),
        name: name.to_string(),
        url,
        source,
        is_default: default.as_deref() == Some(name),
    }))
}

/// Show a single remote from a plain-Git worktree. Returns `None` when missing.
pub fn show_plain_git_remote(root: &Path, name: &str) -> Option<RemoteInfo> {
    let items = plain_git_remote_items(root);
    let default = default_remote_from_items(&items);
    let url = items.get(name)?.clone();
    Some(RemoteInfo {
        output_kind: Some("remote_show"),
        name: name.to_string(),
        url,
        source: "git".to_string(),
        is_default: default.as_deref() == Some(name),
    })
}

/// Resolve the remote name for push/pull when the user omitted it.
///
/// Falls back to `"origin"` when no configured default exists (legacy CLI
/// contract for explicit transport resolution).
pub fn resolve_default_remote_name(repo: &Repository, requested: Option<&str>) -> Result<String> {
    if let Some(requested) = requested {
        return Ok(requested.to_string());
    }
    if let Some(default) = RemoteConfig::open(repo)
        .map_err(anyhow::Error::new)?
        .default_name()
    {
        return Ok(default.to_string());
    }
    if repo.capability() == RepositoryCapability::GitOverlay
        && let Some(default) = git_overlay_default_remote_name(repo)
    {
        return Ok(default);
    }
    Ok("origin".to_string())
}

/// The configured default remote name, if any (no `"origin"` fallback).
pub fn resolved_default_remote_name(repo: &Repository) -> Result<Option<String>> {
    let cfg = RemoteConfig::open(repo).map_err(anyhow::Error::new)?;
    if let Some(default) = cfg.default_name() {
        return Ok(Some(default.to_string()));
    }
    if repo.capability() == RepositoryCapability::GitOverlay {
        return Ok(git_overlay_default_remote_name(repo));
    }
    Ok(None)
}

// ---------------------------------------------------------------------------
// Push / pull capability routing (pure; no network I/O)
// ---------------------------------------------------------------------------

/// Hosted/network push strategy for one push invocation.
///
/// Derived solely from [`RepositoryCapability`] and the `--all-threads` flag.
/// CLI applies the plan by calling the corresponding transport (mirror RPC,
/// per-thread native fan-out, or single native push).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostedPushPlan {
    /// Native path: one push RPC per pushable thread (heddle#838).
    NativePerThreadFanout,
    /// Git-overlay: single multi-ref git-mirror transfer (heddle#846).
    /// Covers every ref (= every thread) in one ship even when
    /// `--all-threads` was set.
    GitOverlayMirror,
    /// Native path: single-thread push RPC for the resolved track name.
    NativeSingleThread,
}

/// Whether a hosted `--all-threads` push collapses to a SINGLE mirror push
/// instead of the per-thread native fan-out.
///
/// True for git-overlay repos: the default mirror push (#846) already ships
/// every ref (= every thread) in one transfer, so looping per thread would
/// re-upload the identical pack T times. Native (non-overlay) repos keep the
/// #838 per-thread fan-out.
pub fn all_threads_uses_single_mirror_push(capability: RepositoryCapability) -> bool {
    capability == RepositoryCapability::GitOverlay
}

/// Plan the hosted/network push strategy for a capability + `--all-threads`.
pub fn plan_hosted_push(capability: RepositoryCapability, all_threads: bool) -> HostedPushPlan {
    if all_threads && !all_threads_uses_single_mirror_push(capability) {
        HostedPushPlan::NativePerThreadFanout
    } else if capability == RepositoryCapability::GitOverlay {
        HostedPushPlan::GitOverlayMirror
    } else {
        HostedPushPlan::NativeSingleThread
    }
}

/// Whether a single-thread network push should use the git-overlay mirror RPC
/// rather than the plain native push RPC.
pub fn uses_git_overlay_mirror_rpc(capability: RepositoryCapability) -> bool {
    capability == RepositoryCapability::GitOverlay
}

/// Whether push/pull should take the local git-overlay path (git refs /
/// git projection) rather than native heddle remote transport.
///
/// Eligible when the repo is git-overlay, hosted mode is off, and the
/// resolved target is not a hosted heddle network endpoint.
pub fn uses_local_git_overlay_transport(
    capability: RepositoryCapability,
    hosted_enabled: bool,
    uses_hosted_network: bool,
) -> bool {
    capability == RepositoryCapability::GitOverlay && !hosted_enabled && !uses_hosted_network
}

/// Default thread name for a push when the user omitted it.
///
/// Explicit request wins; otherwise the attached HEAD thread, else `"main"`
/// for detached HEAD.
pub fn default_push_thread_name(requested: Option<&str>, head: &Head) -> String {
    if let Some(requested) = requested {
        return requested.to_string();
    }
    match head {
        Head::Attached { thread } => thread.to_string(),
        Head::Detached { .. } => "main".to_string(),
    }
}

/// Default remote thread name for a pull when the user omitted it.
///
/// Explicit request wins. On git-overlay, pull tracks the attached HEAD
/// thread (Git branch). On native heddle, the historical default is `"main"`.
pub fn default_pull_thread_name(
    explicit_thread: Option<&str>,
    capability: RepositoryCapability,
    head: &Head,
) -> String {
    if let Some(thread) = explicit_thread {
        return thread.to_string();
    }

    if capability == RepositoryCapability::GitOverlay
        && let Head::Attached { thread } = head
    {
        return thread.to_string();
    }

    "main".to_string()
}

/// Whether a git-overlay current-thread refs push may target `requested`.
///
/// Git-overlay refs push always ships the attached HEAD branch. When the user
/// names a different thread without `--all-threads`, callers should refuse.
/// `all_threads == true` or `requested == None` always allows.
pub fn git_overlay_current_thread_push_ok(
    all_threads: bool,
    requested: Option<&str>,
    attached: Option<&str>,
) -> bool {
    if all_threads {
        return true;
    }
    match requested {
        None => true,
        Some(name) => attached == Some(name),
    }
}

// ---------------------------------------------------------------------------
// Push / pull orchestration plans (pure; no network I/O)
// ---------------------------------------------------------------------------

/// Pure preflight refusals for push/pull orchestration.
///
/// Derived only from caller-supplied facts (flags, HEAD attachment, transport
/// classification). CLI maps these to recovery advice / user-facing errors;
/// dirty-worktree enforcement still runs via CLI `ensure_worktree_clean` when
/// the plan's `requires_clean_worktree` policy is true.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemotePreflightBlocker {
    /// No remote argument and no configured default remote.
    MissingRemote,
    /// Native heddle repo targeting a Git URL or local Git remote.
    TransportMismatch,
    /// Git-overlay current-thread refs push requested a non-attached thread.
    GitOverlayThreadMismatch {
        requested: String,
        attached: Option<String>,
    },
}

impl std::fmt::Display for RemotePreflightBlocker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingRemote => write!(f, "no remote configured"),
            Self::TransportMismatch => {
                write!(f, "remote transport does not match repository capability")
            }
            Self::GitOverlayThreadMismatch {
                requested,
                attached,
            } => {
                let attached_label = attached
                    .as_deref()
                    .map(|t| format!("'{t}'"))
                    .unwrap_or_else(|| "detached HEAD".to_string());
                write!(
                    f,
                    "git-overlay push targets the attached thread; requested '{requested}' but HEAD is {attached_label}"
                )
            }
        }
    }
}

impl std::error::Error for RemotePreflightBlocker {}

/// Caller-supplied facts for pure push planning (no repository I/O here).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushPlanRequest {
    pub capability: RepositoryCapability,
    pub hosted_enabled: bool,
    /// True when the resolved remote is a hosted heddle network endpoint.
    pub uses_hosted_network: bool,
    /// Explicit remote name/spec from the user; `None` means default.
    pub remote: Option<String>,
    /// Whether a configured default remote exists (when `remote` is `None`).
    pub has_default_remote: bool,
    /// Explicit thread from the user (`--thread` / positional).
    pub thread: Option<String>,
    pub all_threads: bool,
    pub force: bool,
    /// HEAD for default thread selection.
    pub head: Head,
    /// CLI-discovered: under local git-overlay transport, the remote is a
    /// native heddle local path (local-sync path rather than refs push).
    pub native_local_heddle_target: bool,
    /// Native capability + git remote classification (CLI `classify_remote_spec`).
    pub transport_mismatch: bool,
}

/// Execution path selected by [`plan_push`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PushPath {
    /// Local git-overlay refs push (`GitProjection` / current or all threads).
    LocalGitOverlayRefs { all_threads: bool },
    /// Local native heddle push to a path remote (under overlay eligibility).
    LocalNativeHeddle { all_threads: bool },
    /// Native heddle remote transport after `resolve_remote` (local path or network).
    NativeRemote {
        hosted: HostedPushPlan,
        /// Network single-thread path uses git-overlay mirror RPC.
        uses_mirror_rpc: bool,
        /// `--all-threads` should fan out per thread (native #838), not collapse
        /// to a single mirror ship.
        native_all_threads_fanout: bool,
    },
}

/// Pure push orchestration plan. CLI resolves remotes/state then executes I/O.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushPlan {
    /// Remote name resolution input (explicit or default still unresolved).
    pub remote: Option<String>,
    pub all_threads: bool,
    pub force: bool,
    /// Resolved track/thread name for single-thread pushes.
    pub track_name: String,
    /// True when taking the local git-overlay transport gate
    /// ([`uses_local_git_overlay_transport`]).
    pub uses_local_git_overlay: bool,
    /// Hosted strategy composed from capability + `--all-threads`.
    pub hosted: HostedPushPlan,
    /// Whether a network push should use the git-overlay mirror RPC.
    pub uses_git_overlay_mirror_rpc: bool,
    /// Convenience: native per-thread fan-out for `--all-threads`.
    pub native_all_threads_fanout: bool,
    pub path: PushPath,
}

/// Caller-supplied facts for pure pull planning.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullPlanRequest {
    pub capability: RepositoryCapability,
    pub hosted_enabled: bool,
    pub uses_hosted_network: bool,
    pub remote: Option<String>,
    pub has_default_remote: bool,
    /// Explicit remote thread to pull.
    pub thread: Option<String>,
    /// Optional local destination thread (`--local-thread`).
    pub local_thread: Option<String>,
    pub head: Head,
    pub transport_mismatch: bool,
    pub lazy: bool,
}

/// Pure pull orchestration plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullPlan {
    /// Remote name resolution input (explicit or default still unresolved).
    pub remote: Option<String>,
    /// Remote thread to fetch.
    pub remote_thread: String,
    /// Optional local destination thread name.
    pub local_thread: Option<String>,
    /// True when taking the local git-overlay pull path.
    pub uses_local_git_overlay: bool,
    /// Whether materialization would rewrite the current checkout.
    pub will_materialize: bool,
    /// Dirty-worktree policy: caller must refuse dirty trees when true.
    pub requires_clean_worktree: bool,
    pub lazy: bool,
}

/// Pure: missing remote when the user omitted it and no default is configured.
pub fn remote_missing_blocker(
    remote: Option<&str>,
    has_default_remote: bool,
) -> Option<RemotePreflightBlocker> {
    if remote.is_none() && !has_default_remote {
        Some(RemotePreflightBlocker::MissingRemote)
    } else {
        None
    }
}

/// Pure: native-repo git-transport mismatch, only when not on local overlay path.
pub fn transport_mismatch_blocker(
    uses_local_git_overlay: bool,
    transport_mismatch: bool,
) -> Option<RemotePreflightBlocker> {
    if !uses_local_git_overlay && transport_mismatch {
        Some(RemotePreflightBlocker::TransportMismatch)
    } else {
        None
    }
}

/// Pure: git-overlay refs push refuses a non-attached explicit thread.
pub fn git_overlay_thread_mismatch_blocker(
    all_threads: bool,
    requested: Option<&str>,
    attached: Option<&str>,
) -> Option<RemotePreflightBlocker> {
    if git_overlay_current_thread_push_ok(all_threads, requested, attached) {
        None
    } else {
        Some(RemotePreflightBlocker::GitOverlayThreadMismatch {
            requested: requested.unwrap_or("").to_string(),
            attached: attached.map(str::to_string),
        })
    }
}

/// Whether a pull would materialize into the current checkout.
///
/// Matches CLI: materialize when no `--local-thread` or when it equals the
/// attached HEAD thread (detached HEAD materializes only with no local override).
pub fn pull_will_materialize(local_thread: Option<&str>, head: &Head) -> bool {
    match head {
        Head::Attached { thread } => local_thread.is_none_or(|local| thread == local),
        Head::Detached { .. } => local_thread.is_none(),
    }
}

/// Dirty-worktree policy for pull: clean required on local git-overlay path or
/// when the pull will materialize the current checkout.
pub fn pull_requires_clean_worktree(uses_local_git_overlay: bool, will_materialize: bool) -> bool {
    uses_local_git_overlay || will_materialize
}

/// Plan a push from pure inputs. Composes existing routing helpers.
pub fn plan_push(request: &PushPlanRequest) -> Result<PushPlan, RemotePreflightBlocker> {
    if let Some(blocker) =
        remote_missing_blocker(request.remote.as_deref(), request.has_default_remote)
    {
        return Err(blocker);
    }

    let uses_local = uses_local_git_overlay_transport(
        request.capability,
        request.hosted_enabled,
        request.uses_hosted_network,
    );
    let track_name = default_push_thread_name(request.thread.as_deref(), &request.head);
    let hosted = plan_hosted_push(request.capability, request.all_threads);
    let uses_mirror = uses_git_overlay_mirror_rpc(request.capability);
    let native_fanout = matches!(hosted, HostedPushPlan::NativePerThreadFanout);

    if uses_local {
        if request.native_local_heddle_target {
            return Ok(PushPlan {
                remote: request.remote.clone(),
                all_threads: request.all_threads,
                force: request.force,
                track_name,
                uses_local_git_overlay: true,
                hosted,
                uses_git_overlay_mirror_rpc: uses_mirror,
                native_all_threads_fanout: native_fanout,
                path: PushPath::LocalNativeHeddle {
                    all_threads: request.all_threads,
                },
            });
        }

        let attached = match &request.head {
            Head::Attached { thread } => Some(thread.as_str()),
            Head::Detached { .. } => None,
        };
        if let Some(blocker) = git_overlay_thread_mismatch_blocker(
            request.all_threads,
            request.thread.as_deref(),
            attached,
        ) {
            return Err(blocker);
        }

        return Ok(PushPlan {
            remote: request.remote.clone(),
            all_threads: request.all_threads,
            force: request.force,
            track_name,
            uses_local_git_overlay: true,
            hosted,
            uses_git_overlay_mirror_rpc: uses_mirror,
            native_all_threads_fanout: native_fanout,
            path: PushPath::LocalGitOverlayRefs {
                all_threads: request.all_threads,
            },
        });
    }

    if let Some(blocker) = transport_mismatch_blocker(false, request.transport_mismatch) {
        return Err(blocker);
    }

    Ok(PushPlan {
        remote: request.remote.clone(),
        all_threads: request.all_threads,
        force: request.force,
        track_name,
        uses_local_git_overlay: false,
        hosted,
        uses_git_overlay_mirror_rpc: uses_mirror,
        native_all_threads_fanout: native_fanout,
        path: PushPath::NativeRemote {
            hosted,
            uses_mirror_rpc: uses_mirror,
            native_all_threads_fanout: native_fanout,
        },
    })
}

/// Plan a pull from pure inputs. Composes transport + thread + dirty policy.
pub fn plan_pull(request: &PullPlanRequest) -> Result<PullPlan, RemotePreflightBlocker> {
    if let Some(blocker) =
        remote_missing_blocker(request.remote.as_deref(), request.has_default_remote)
    {
        return Err(blocker);
    }

    let uses_local = uses_local_git_overlay_transport(
        request.capability,
        request.hosted_enabled,
        request.uses_hosted_network,
    );

    if let Some(blocker) = transport_mismatch_blocker(uses_local, request.transport_mismatch) {
        return Err(blocker);
    }

    let remote_thread =
        default_pull_thread_name(request.thread.as_deref(), request.capability, &request.head);
    let will_materialize = pull_will_materialize(request.local_thread.as_deref(), &request.head);
    let requires_clean = pull_requires_clean_worktree(uses_local, will_materialize);

    Ok(PullPlan {
        remote: request.remote.clone(),
        remote_thread,
        local_thread: request.local_thread.clone(),
        uses_local_git_overlay: uses_local,
        will_materialize,
        requires_clean_worktree: requires_clean,
        lazy: request.lazy,
    })
}

// ---------------------------------------------------------------------------
// Push / pull typed outcomes (pure; assembled from plan + execution facts)
// ---------------------------------------------------------------------------

/// Stable notes ref published on the git-overlay refs push path.
pub const GIT_NOTES_REF: &str = "refs/notes/heddle";

/// Warning that ordinary `git log --all` may surface Heddle notes commits.
pub const GIT_NOTES_VISIBILITY_WARNING: &str =
    "ordinary `git log --all` may show Heddle metadata commits from refs/notes/heddle";

/// Warning when a forced git-overlay push may discard remote-only history.
pub const FORCE_DISCARD_WARNING: &str = "remote refs may be moved back to match local Heddle state; remote commits not reachable from this checkout can be discarded";

/// Scope label for commits scanned during a git-overlay pull import.
pub const COMMITS_SEEN_SCOPE: &str = "branches_and_heddle_notes";

/// Git remote name/url pair for machine JSON (`git_remote_configured`).
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct GitRemoteConfigured {
    pub name: String,
    pub url: String,
}

/// Upstream branch binding for machine JSON (`git_upstream_configured`).
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct GitUpstreamConfigured {
    pub branch: String,
    pub remote: String,
}

/// Tracking refresh facts after a git-overlay refs push (CLI-discovered).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitOverlayPushTracking {
    pub remote_name: String,
    pub configured_remote: Option<GitRemoteConfigured>,
    pub upstream_branch: Option<String>,
}

/// Machine JSON body for a successful (or partial) push.
///
/// Field names match the CLI `heddle push --output json` contract. CLI may
/// flatten this and attach verification-derived `next_action*` fields.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct PushOutcome {
    pub output_kind: &'static str,
    pub action: &'static str,
    pub status: &'static str,
    pub success: bool,
    pub pushed: bool,
    pub changed: bool,
    pub transport: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remote: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub push_scope: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ref_scope: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub git_notes_ref: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refs_written: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub git_notes_visibility_warning: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub git_tracking_remote: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub git_remote_configured: Option<GitRemoteConfigured>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub git_upstream_configured: Option<GitUpstreamConfigured>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tags_included: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub force: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub force_discard_warning: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thread: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub objects: Option<usize>,
}

/// Machine JSON body for a successful pull.
///
/// Field names match the CLI `heddle pull --output json` contract.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct PullOutcome {
    pub output_kind: &'static str,
    pub action: &'static str,
    pub status: &'static str,
    pub success: bool,
    pub pulled: bool,
    pub changed: bool,
    pub transport: &'static str,
    pub remote: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old_git_head: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub new_git_head: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old_state: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub new_state: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub states_created: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub commits_seen: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub commits_seen_scope: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub materialized_checkout: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub changed_path_count: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub changed_paths: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thread: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub objects: Option<usize>,
}

/// Post-transport facts for assembling a [`PushOutcome`] (no network I/O).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PushExecutionFacts {
    /// Local git-overlay refs push (`GitProjection` path).
    GitOverlayRefs {
        remote_name: String,
        current_thread: Option<String>,
        refs_written: Vec<String>,
        tracking: Option<GitOverlayPushTracking>,
    },
    /// Native single-thread push (local path or network).
    HeddleSingle {
        state: Option<String>,
        objects: Option<usize>,
    },
    /// Native `--all-threads` fan-out (heddle#838).
    HeddleAllThreads {
        /// Thread names that landed (unsorted; builder sorts for JSON).
        pushed_threads: Vec<String>,
        /// Thread names that failed (presence drives `status: "partial"`).
        failed_threads: Vec<String>,
        objects: usize,
    },
}

/// Post-transport facts for assembling a [`PullOutcome`] (no network I/O).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PullExecutionFacts {
    /// Local git-overlay pull / import path.
    GitOverlay {
        remote: String,
        branch: Option<String>,
        old_git_head: Option<String>,
        new_git_head: Option<String>,
        old_state: Option<String>,
        new_state: Option<String>,
        changed: bool,
        states_created: usize,
        commits_seen: usize,
        materialized_checkout: bool,
        changed_paths: Vec<String>,
    },
    /// Native heddle pull (local path or network).
    Heddle {
        changed: bool,
        remote: String,
        thread: String,
        state: Option<String>,
        objects: Option<usize>,
    },
}

/// `push_scope` machine label for all-threads vs current-thread.
pub fn push_scope_label(all_threads: bool) -> &'static str {
    if all_threads {
        "all_threads"
    } else {
        "current_thread"
    }
}

/// `ref_scope` machine label for git-overlay refs push.
pub fn git_overlay_ref_scope(all_threads: bool) -> &'static str {
    if all_threads {
        "all_threads_tags_and_heddle_notes"
    } else {
        "branch_and_heddle_notes"
    }
}

/// Machine `status` for push: full success vs partial multi-thread failure.
pub fn push_status(ok: bool) -> &'static str {
    if ok { "pushed" } else { "partial" }
}

/// Machine `status` for pull: updated vs already up to date.
pub fn pull_status(changed: bool) -> &'static str {
    if changed { "updated" } else { "up_to_date" }
}

/// Assemble a push outcome from the orchestration plan and post-I/O facts.
///
/// Pure: no repository or network access. `plan` supplies force / all-threads
/// policy; `facts` supply refs written, object counts, and partial failures.
pub fn build_push_outcome(plan: &PushPlan, facts: PushExecutionFacts) -> PushOutcome {
    match facts {
        PushExecutionFacts::GitOverlayRefs {
            remote_name,
            current_thread,
            refs_written,
            tracking,
        } => {
            let all_threads = plan.all_threads;
            let force = plan.force;
            let tracking_remote = tracking.as_ref().map(|t| t.remote_name.clone());
            let configured_remote = tracking.as_ref().and_then(|t| t.configured_remote.clone());
            let upstream_configured = tracking.as_ref().and_then(|t| {
                t.upstream_branch
                    .as_ref()
                    .map(|branch| GitUpstreamConfigured {
                        branch: branch.clone(),
                        remote: tracking_remote
                            .clone()
                            .unwrap_or_else(|| "origin".to_string()),
                    })
            });
            PushOutcome {
                output_kind: "push",
                action: "push",
                status: push_status(true),
                success: true,
                pushed: true,
                changed: true,
                transport: "git",
                remote: Some(remote_name),
                push_scope: Some(push_scope_label(all_threads)),
                ref_scope: Some(git_overlay_ref_scope(all_threads)),
                git_notes_ref: Some(GIT_NOTES_REF),
                refs_written: Some(refs_written),
                git_notes_visibility_warning: Some(GIT_NOTES_VISIBILITY_WARNING),
                git_tracking_remote: tracking_remote,
                git_remote_configured: configured_remote,
                git_upstream_configured: upstream_configured,
                tags_included: Some(all_threads),
                force: Some(force),
                force_discard_warning: force.then_some(FORCE_DISCARD_WARNING),
                thread: current_thread,
                state: None,
                objects: None,
            }
        }
        PushExecutionFacts::HeddleSingle { state, objects } => PushOutcome {
            output_kind: "push",
            action: "push",
            status: push_status(true),
            success: true,
            pushed: true,
            changed: true,
            transport: "heddle",
            remote: None,
            push_scope: None,
            ref_scope: None,
            git_notes_ref: None,
            refs_written: None,
            git_notes_visibility_warning: None,
            git_tracking_remote: None,
            git_remote_configured: None,
            git_upstream_configured: None,
            tags_included: None,
            force: None,
            force_discard_warning: None,
            thread: None,
            state,
            objects,
        },
        PushExecutionFacts::HeddleAllThreads {
            mut pushed_threads,
            failed_threads,
            objects,
        } => {
            let ok = failed_threads.is_empty();
            pushed_threads.sort();
            PushOutcome {
                output_kind: "push",
                action: "push",
                status: push_status(ok),
                success: ok,
                pushed: ok,
                changed: true,
                transport: "heddle",
                remote: None,
                push_scope: Some(push_scope_label(true)),
                ref_scope: None,
                git_notes_ref: None,
                refs_written: Some(pushed_threads),
                git_notes_visibility_warning: None,
                git_tracking_remote: None,
                git_remote_configured: None,
                git_upstream_configured: None,
                tags_included: None,
                force: None,
                force_discard_warning: None,
                thread: None,
                state: None,
                objects: Some(objects),
            }
        }
    }
}

/// Assemble a pull outcome from post-I/O facts (and optional plan context).
///
/// `plan` is currently unused for field selection but reserved so callers can
/// pass the orchestration plan without a second signature later. Pure: no I/O.
pub fn build_pull_outcome(_plan: Option<&PullPlan>, facts: PullExecutionFacts) -> PullOutcome {
    match facts {
        PullExecutionFacts::GitOverlay {
            remote,
            branch,
            old_git_head,
            new_git_head,
            old_state,
            new_state,
            changed,
            states_created,
            commits_seen,
            materialized_checkout,
            changed_paths,
        } => {
            let path_count = changed_paths.len();
            PullOutcome {
                output_kind: "pull",
                action: "pull",
                status: pull_status(changed),
                success: true,
                pulled: changed,
                changed,
                transport: "git",
                remote,
                branch,
                old_git_head,
                new_git_head,
                old_state,
                new_state,
                states_created: Some(states_created),
                commits_seen: Some(commits_seen),
                commits_seen_scope: Some(COMMITS_SEEN_SCOPE),
                materialized_checkout: Some(materialized_checkout),
                changed_path_count: Some(path_count),
                changed_paths: Some(changed_paths),
                thread: None,
                state: None,
                objects: None,
            }
        }
        PullExecutionFacts::Heddle {
            changed,
            remote,
            thread,
            state,
            objects,
        } => PullOutcome {
            output_kind: "pull",
            action: "pull",
            status: pull_status(changed),
            success: true,
            pulled: changed,
            changed,
            transport: "heddle",
            remote,
            branch: None,
            old_git_head: None,
            new_git_head: None,
            old_state: None,
            new_state: None,
            states_created: None,
            commits_seen: None,
            commits_seen_scope: None,
            materialized_checkout: None,
            changed_path_count: None,
            changed_paths: None,
            thread: Some(thread),
            state,
            objects,
        },
    }
}

/// Short human-readable summary of a push outcome (for logs / text shells).
pub fn summarize_push_outcome(outcome: &PushOutcome) -> String {
    let remote = outcome.remote.as_deref().unwrap_or("remote");
    match outcome.transport {
        "git" => {
            let scope = outcome.push_scope.unwrap_or("current_thread");
            let refs = outcome.refs_written.as_ref().map(|r| r.len()).unwrap_or(0);
            if outcome.force == Some(true) {
                format!("force-pushed {scope} ({refs} refs) to {remote}")
            } else {
                format!("pushed {scope} ({refs} refs) to {remote}")
            }
        }
        "heddle" if outcome.push_scope == Some("all_threads") => {
            let n = outcome.refs_written.as_ref().map(|r| r.len()).unwrap_or(0);
            if outcome.success {
                format!("pushed {n} threads")
            } else {
                format!("partial push: {n} threads landed")
            }
        }
        "heddle" => match (&outcome.state, outcome.objects) {
            (Some(state), Some(objects)) => {
                format!("pushed state {state} ({objects} objects)")
            }
            (Some(state), None) => format!("pushed state {state}"),
            (None, Some(objects)) => format!("pushed ({objects} objects)"),
            (None, None) => "pushed".to_string(),
        },
        other => format!("pushed via {other}"),
    }
}

/// Short human-readable summary of a pull outcome (for logs / text shells).
pub fn summarize_pull_outcome(outcome: &PullOutcome) -> String {
    if !outcome.changed {
        return format!("already up to date with {}", outcome.remote);
    }
    match outcome.transport {
        "git" => {
            let paths = outcome.changed_path_count.unwrap_or(0);
            let states = outcome.states_created.unwrap_or(0);
            format!(
                "pulled from {} ({states} new states, {paths} changed paths)",
                outcome.remote
            )
        }
        "heddle" => {
            let thread = outcome.thread.as_deref().unwrap_or("thread");
            match (&outcome.state, outcome.objects) {
                (Some(state), Some(objects)) => {
                    format!("pulled {thread} -> {state} ({objects} objects)")
                }
                (Some(state), None) => format!("pulled {thread} -> {state}"),
                (None, Some(objects)) => format!("pulled {thread} ({objects} objects)"),
                (None, None) => format!("pulled {thread} from {}", outcome.remote),
            }
        }
        other => format!("pulled via {other} from {}", outcome.remote),
    }
}

/// Merged remote map: name → (url, source label).
///
/// Heddle remotes from `.heddle/remotes.toml` win; git-overlay entries fill
/// gaps. Used by list/show assembly and by mutation commands that need the
/// same visibility set.
pub fn merged_remote_items(repo: &Repository) -> Result<BTreeMap<String, (String, String)>> {
    let cfg = RemoteConfig::open(repo).map_err(anyhow::Error::new)?;
    let git_overlay_remotes = if repo.capability() == RepositoryCapability::GitOverlay {
        git_overlay_config_remotes(repo)
    } else {
        BTreeMap::new()
    };
    let mut items: BTreeMap<String, (String, String)> = cfg
        .list()
        .into_iter()
        .map(|(name, remote)| {
            let source = configured_remote_source(repo, &remote.url);
            (name, (remote.url, source.to_string()))
        })
        .collect();
    if repo.capability() == RepositoryCapability::GitOverlay {
        for (name, url) in git_overlay_remotes {
            items
                .entry(name)
                .or_insert_with(|| (url, "git-overlay".to_string()));
        }
    }
    Ok(items)
}

/// Remotes visible from plain-Git config layers under `root`.
pub fn plain_git_remote_items(root: &Path) -> BTreeMap<String, String> {
    let Some(ctx) = GitConfigContext::discover(root) else {
        return BTreeMap::new();
    };
    ctx.remotes(ctx.layered_paths())
}

fn default_remote_from_items(items: &BTreeMap<String, String>) -> Option<String> {
    if items.contains_key("origin") {
        Some("origin".to_string())
    } else if items.len() == 1 {
        items.keys().next().cloned()
    } else {
        None
    }
}

fn git_overlay_default_remote_name(repo: &Repository) -> Option<String> {
    let git_remotes = git_overlay_config_remotes(repo);
    if let Some(upstream_remote) = git_upstream_remote_name(repo) {
        return Some(upstream_remote);
    }
    if git_remotes.contains_key("origin") {
        return Some("origin".to_string());
    }
    if git_remotes.len() == 1 {
        return git_remotes.keys().next().cloned();
    }
    None
}

fn git_upstream_remote_name(repo: &Repository) -> Option<String> {
    let branch = repo.git_overlay_current_branch().ok().flatten()?;
    let git = SleyRepository::discover(repo.root()).ok()?;
    git.config_snapshot()
        .ok()?
        .get("branch", Some(&branch), "remote")
        .map(str::to_string)
        .filter(|remote| !remote.is_empty())
}

fn git_overlay_config_remotes(repo: &Repository) -> BTreeMap<String, String> {
    let Some(ctx) = GitConfigContext::discover(repo.root()) else {
        return BTreeMap::new();
    };
    let mut paths = ctx.layered_paths();
    paths.push(repo.heddle_dir().join("git").join("config"));
    ctx.remotes(paths)
}

fn configured_remote_source(repo: &Repository, url: &str) -> &'static str {
    if repo.capability() == RepositoryCapability::GitOverlay
        && local_remote_path(url).is_some_and(|path| is_local_git_repository(&path))
    {
        "git-overlay"
    } else {
        "heddle"
    }
}

fn local_remote_path(url: &str) -> Option<PathBuf> {
    match RemoteTarget::parse(url).ok()? {
        RemoteTarget::Local(path) => Some(path),
        RemoteTarget::Network { .. } => None,
    }
}

fn is_local_git_repository(path: &Path) -> bool {
    if path.join(".git").exists() {
        return true;
    }
    path.join("HEAD").is_file() && path.join("objects").is_dir() && path.join("refs").is_dir()
}

/// Error when a remote write would touch config outside the repo Git tree.
#[derive(Debug, Clone, thiserror::Error)]
#[error("Remote '{name}' is defined in an included Git config that heddle won't edit: {path}")]
pub struct IncludedGitRemoteConfigError {
    pub name: String,
    pub path: PathBuf,
}

impl IncludedGitRemoteConfigError {
    fn new(name: impl Into<String>, path: impl Into<PathBuf>) -> Self {
        Self {
            name: name.into(),
            path: path.into(),
        }
    }
}

/// The resolved Git directory layout for a repository, used to read remote
/// definitions from `.git/config` and its layered companions.
#[derive(Debug, Clone)]
pub struct GitConfigContext {
    git_dir: PathBuf,
    common_dir: PathBuf,
    branch: Option<String>,
}

impl GitConfigContext {
    pub fn discover(root: &Path) -> Option<Self> {
        let git = SleyRepository::discover(root).ok()?;
        Some(Self {
            git_dir: git.git_dir().to_path_buf(),
            common_dir: git.common_dir().to_path_buf(),
            branch: git
                .head()
                .ok()
                .and_then(|head| head.symbolic_target.map(|name| name.to_string()))
                .and_then(|name| name.strip_prefix("refs/heads/").map(str::to_string)),
        })
    }

    pub fn common_dir(&self) -> &Path {
        &self.common_dir
    }

    /// The standard repository config files, ordered highest-precedence first:
    /// the per-worktree `config.worktree` (only when `extensions.worktreeConfig`
    /// is enabled), then the git-dir `config`, then the shared common-dir
    /// `config` for linked worktrees.
    pub fn layered_paths(&self) -> Vec<PathBuf> {
        let mut paths = Vec::new();
        if self.worktree_config_enabled() {
            paths.push(self.git_dir.join("config.worktree"));
        }
        paths.push(self.git_dir.join("config"));
        if self.common_dir != self.git_dir {
            paths.push(self.common_dir.join("config"));
        }
        paths
    }

    fn worktree_config_enabled(&self) -> bool {
        let mut paths = vec![self.git_dir.join("config")];
        if self.common_dir != self.git_dir {
            paths.push(self.common_dir.join("config"));
        }
        self.load(paths)
            .and_then(|config| config.get_bool("extensions", None, "worktreeConfig"))
            .unwrap_or(false)
    }

    /// The file a write to remote `name` must target so the next
    /// `remote list` read resolves the value we just wrote.
    pub fn write_file_for(
        &self,
        name: &str,
    ) -> std::result::Result<PathBuf, IncludedGitRemoteConfigError> {
        match self.defining_files_for(name).into_iter().next() {
            Some(path) => {
                if !self.owns_config_file(&path) {
                    return Err(IncludedGitRemoteConfigError::new(name, path));
                }
                Ok(path)
            }
            None => Ok(self.common_dir.join("config")),
        }
    }

    /// Every file that currently defines remote `name`, resolved through
    /// includes. A remove must clear all of them.
    pub fn remove_files_for(
        &self,
        name: &str,
    ) -> std::result::Result<Vec<PathBuf>, IncludedGitRemoteConfigError> {
        let files = self.defining_files_for(name);
        for path in &files {
            if !self.owns_config_file(path) {
                return Err(IncludedGitRemoteConfigError::new(name, path.clone()));
            }
        }
        Ok(files)
    }

    /// The file(s) whose `[remote "<name>"]` section the reader resolves,
    /// following `include.path`/`includeIf`. Returned highest-precedence first.
    pub fn defining_files_for(&self, name: &str) -> Vec<PathBuf> {
        let mut files = Vec::new();
        let Some(stack) = self.config_stack() else {
            return files;
        };
        for entry in stack.entries.iter().rev() {
            if entry.section.eq_ignore_ascii_case("remote")
                && entry.subsection.as_deref() == Some(name)
                && let Some(path) = config_entry_origin_path(entry)
                && !files.contains(&path)
            {
                files.push(path);
            }
        }
        files
    }

    /// Whether heddle may rewrite `path`: only config files within the
    /// repository's own Git directory tree (git-dir / common-dir).
    pub fn owns_config_file(&self, path: &Path) -> bool {
        let target = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        [&self.git_dir, &self.common_dir].into_iter().any(|root| {
            let root = root.canonicalize().unwrap_or_else(|_| root.clone());
            target.starts_with(&root)
        })
    }

    pub fn remotes(&self, paths: Vec<PathBuf>) -> BTreeMap<String, String> {
        let mut remotes = BTreeMap::new();
        for path in paths {
            let Some(config) = self.load_one(&path, true) else {
                continue;
            };
            for section in &config.sections {
                if !section.name.eq_ignore_ascii_case("remote") {
                    continue;
                }
                let Some(name) = section.subsection.as_deref() else {
                    continue;
                };
                let Some(url) = config_section_value(section, "url") else {
                    continue;
                };
                remotes
                    .entry(name.to_string())
                    .or_insert_with(|| url.to_string());
            }
        }
        remotes
    }

    fn load(&self, paths: Vec<PathBuf>) -> Option<GitConfig> {
        let mut merged = GitConfig::default();
        for path in paths.into_iter().rev() {
            let Some(config) = self.load_one(&path, true) else {
                continue;
            };
            merged.sections.extend(config.sections);
        }
        Some(merged)
    }

    fn config_stack(&self) -> Option<ConfigStack> {
        let context = ConfigIncludeContext {
            git_dir: Some(self.git_dir.clone()),
            current_branch: self.branch.clone(),
        };
        let mut stack = ConfigStack::new();
        for path in self.layered_paths().into_iter().rev() {
            let scope = if path
                .file_name()
                .is_some_and(|name| name == "config.worktree")
            {
                ConfigScope::Worktree
            } else {
                ConfigScope::Local
            };
            stack.push_file(&path, scope, true, &context).ok()?;
        }
        Some(stack)
    }

    fn load_one(&self, path: &Path, follow_includes: bool) -> Option<GitConfig> {
        let bytes = fs::read(path).ok()?;
        let config = GitConfig::parse(&bytes).ok()?;
        if !follow_includes {
            return Some(config);
        }
        let base = path.parent().unwrap_or_else(|| Path::new("."));
        config
            .resolve_includes(
                base,
                &ConfigIncludeContext {
                    git_dir: Some(self.git_dir.clone()),
                    current_branch: self.branch.clone(),
                },
            )
            .ok()
    }
}

fn config_entry_origin_path(entry: &ConfigStackEntry) -> Option<PathBuf> {
    (entry.origin.kind == ConfigOriginKind::File).then(|| PathBuf::from(&entry.origin.name))
}

fn config_section_value<'a>(
    section: &'a sley::plumbing::sley_config::ConfigSection,
    key: &str,
) -> Option<&'a str> {
    section
        .entries
        .iter()
        .rev()
        .find(|entry| entry.key.eq_ignore_ascii_case(key))
        .and_then(|entry| entry.value.as_deref())
}

/// Map a core included-config error into a plain `anyhow` so CLI call sites
/// can attach recovery advice without depending on render types here.
pub fn included_config_error(err: IncludedGitRemoteConfigError) -> anyhow::Error {
    anyhow!(err)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn init_git(root: &Path) {
        SleyRepository::init(root).expect("init git repo");
    }

    #[test]
    fn parses_quoted_url_with_equals_and_strips_quotes() {
        let tmp = tempfile::TempDir::new().unwrap();
        init_git(tmp.path());
        fs::write(
            tmp.path().join(".git").join("config"),
            "[remote \"origin\"]\n\turl = \"https://example.com/repo?ref=main&a=b\"\n",
        )
        .unwrap();

        let remotes = plain_git_remote_items(tmp.path());

        assert_eq!(
            remotes.get("origin").map(String::as_str),
            Some("https://example.com/repo?ref=main&a=b"),
        );
    }

    #[test]
    fn strips_inline_comments_from_url() {
        let tmp = tempfile::TempDir::new().unwrap();
        init_git(tmp.path());
        fs::write(
            tmp.path().join(".git").join("config"),
            "[remote \"origin\"]\n\turl = https://example.com/repo ; trailing comment\n",
        )
        .unwrap();

        let remotes = plain_git_remote_items(tmp.path());

        assert_eq!(
            remotes.get("origin").map(String::as_str),
            Some("https://example.com/repo"),
        );
    }

    #[test]
    fn follows_include_directives() {
        let tmp = tempfile::TempDir::new().unwrap();
        init_git(tmp.path());
        let git_dir = tmp.path().join(".git");
        fs::write(
            git_dir.join("extra.config"),
            "[remote \"upstream\"]\n\turl = https://example.com/upstream\n",
        )
        .unwrap();
        fs::write(git_dir.join("config"), "[include]\n\tpath = extra.config\n").unwrap();

        let remotes = plain_git_remote_items(tmp.path());

        assert_eq!(
            remotes.get("upstream").map(String::as_str),
            Some("https://example.com/upstream"),
        );
    }

    #[test]
    fn worktree_config_overrides_local_when_extension_enabled() {
        let tmp = tempfile::TempDir::new().unwrap();
        init_git(tmp.path());
        let git_dir = tmp.path().join(".git");
        fs::write(
            git_dir.join("config"),
            "[extensions]\n\tworktreeConfig = true\n\
             [remote \"origin\"]\n\turl = https://example.com/local\n",
        )
        .unwrap();
        fs::write(
            git_dir.join("config.worktree"),
            "[remote \"origin\"]\n\turl = https://example.com/worktree\n",
        )
        .unwrap();

        let remotes = plain_git_remote_items(tmp.path());

        assert_eq!(
            remotes.get("origin").map(String::as_str),
            Some("https://example.com/worktree"),
        );
    }

    #[test]
    fn ignores_worktree_config_when_extension_disabled() {
        let tmp = tempfile::TempDir::new().unwrap();
        init_git(tmp.path());
        let git_dir = tmp.path().join(".git");
        fs::write(
            git_dir.join("config"),
            "[remote \"origin\"]\n\turl = https://example.com/local\n",
        )
        .unwrap();
        fs::write(
            git_dir.join("config.worktree"),
            "[remote \"origin\"]\n\turl = https://example.com/worktree\n",
        )
        .unwrap();

        let remotes = plain_git_remote_items(tmp.path());

        assert_eq!(
            remotes.get("origin").map(String::as_str),
            Some("https://example.com/local"),
        );
    }

    #[test]
    fn list_plain_git_marks_origin_default() {
        let tmp = tempfile::TempDir::new().unwrap();
        init_git(tmp.path());
        fs::write(
            tmp.path().join(".git").join("config"),
            "[remote \"origin\"]\n\turl = https://example.com/repo\n\
             [remote \"upstream\"]\n\turl = https://example.com/up\n",
        )
        .unwrap();

        let report = list_plain_git_remotes(tmp.path());
        assert_eq!(report.output_kind, "remote_list");
        assert_eq!(report.remotes.len(), 2);
        let origin = report.remotes.iter().find(|r| r.name == "origin").unwrap();
        assert!(origin.is_default);
        assert_eq!(origin.source, "git");
        let upstream = report
            .remotes
            .iter()
            .find(|r| r.name == "upstream")
            .unwrap();
        assert!(!upstream.is_default);
    }

    #[test]
    fn write_file_for_rejects_external_include() {
        let tmp = tempfile::TempDir::new().unwrap();
        init_git(tmp.path());
        let git_dir = tmp.path().join(".git");
        let external = tmp.path().join("external.config");
        fs::write(
            &external,
            "[remote \"origin\"]\n\turl = https://example.com/external\n",
        )
        .unwrap();
        fs::write(
            git_dir.join("config"),
            format!("[include]\n\tpath = {}\n", external.display()),
        )
        .unwrap();

        let ctx = GitConfigContext::discover(tmp.path()).unwrap();
        assert!(ctx.write_file_for("origin").is_err());
        assert!(ctx.remove_files_for("origin").is_err());
    }

    #[test]
    fn defining_files_follow_include_path() {
        let tmp = tempfile::TempDir::new().unwrap();
        init_git(tmp.path());
        let git_dir = tmp.path().join(".git");
        fs::write(
            git_dir.join("extra.config"),
            "[remote \"origin\"]\n\turl = https://example.com/old\n",
        )
        .unwrap();
        fs::write(git_dir.join("config"), "[include]\n\tpath = extra.config\n").unwrap();

        let ctx = GitConfigContext::discover(tmp.path()).unwrap();
        let target = ctx.write_file_for("origin").unwrap();
        assert_eq!(target, git_dir.join("extra.config"));
    }

    // --- Push / pull capability routing ---

    #[test]
    fn git_overlay_all_threads_hosted_push_is_single_mirror() {
        assert!(
            all_threads_uses_single_mirror_push(RepositoryCapability::GitOverlay),
            "git-overlay --all-threads must collapse to one mirror push",
        );
        assert!(
            !all_threads_uses_single_mirror_push(RepositoryCapability::NativeHeddle),
            "native --all-threads must keep the per-thread fan-out (#838)",
        );
    }

    #[test]
    fn plan_hosted_push_routes_by_capability_and_all_threads() {
        assert_eq!(
            plan_hosted_push(RepositoryCapability::NativeHeddle, true),
            HostedPushPlan::NativePerThreadFanout,
        );
        assert_eq!(
            plan_hosted_push(RepositoryCapability::GitOverlay, true),
            HostedPushPlan::GitOverlayMirror,
        );
        assert_eq!(
            plan_hosted_push(RepositoryCapability::GitOverlay, false),
            HostedPushPlan::GitOverlayMirror,
        );
        assert_eq!(
            plan_hosted_push(RepositoryCapability::NativeHeddle, false),
            HostedPushPlan::NativeSingleThread,
        );
    }

    #[test]
    fn uses_git_overlay_mirror_rpc_only_for_overlay() {
        assert!(uses_git_overlay_mirror_rpc(
            RepositoryCapability::GitOverlay
        ));
        assert!(!uses_git_overlay_mirror_rpc(
            RepositoryCapability::NativeHeddle
        ));
    }

    #[test]
    fn uses_local_git_overlay_transport_requires_overlay_and_no_hosted() {
        assert!(uses_local_git_overlay_transport(
            RepositoryCapability::GitOverlay,
            false,
            false,
        ));
        assert!(!uses_local_git_overlay_transport(
            RepositoryCapability::GitOverlay,
            true,
            false,
        ));
        assert!(!uses_local_git_overlay_transport(
            RepositoryCapability::GitOverlay,
            false,
            true,
        ));
        assert!(!uses_local_git_overlay_transport(
            RepositoryCapability::NativeHeddle,
            false,
            false,
        ));
    }

    #[test]
    fn default_push_thread_prefers_explicit_then_attached_then_main() {
        let attached = Head::Attached {
            thread: objects::object::ThreadName::new("feature"),
        };
        let detached = Head::Detached {
            state: objects::object::ChangeId::generate(),
        };

        assert_eq!(
            default_push_thread_name(Some("release"), &attached),
            "release"
        );
        assert_eq!(default_push_thread_name(None, &attached), "feature");
        assert_eq!(default_push_thread_name(None, &detached), "main");
    }

    #[test]
    fn default_pull_thread_uses_current_git_overlay_thread() {
        let head = Head::Attached {
            thread: objects::object::ThreadName::new("master"),
        };
        assert_eq!(
            default_pull_thread_name(None, RepositoryCapability::GitOverlay, &head),
            "master"
        );
    }

    #[test]
    fn default_pull_thread_keeps_native_main_default() {
        let head = Head::Attached {
            thread: objects::object::ThreadName::new("feature"),
        };
        assert_eq!(
            default_pull_thread_name(None, RepositoryCapability::NativeHeddle, &head),
            "main"
        );
    }

    #[test]
    fn default_pull_thread_honors_explicit_thread() {
        let head = Head::Attached {
            thread: objects::object::ThreadName::new("master"),
        };
        assert_eq!(
            default_pull_thread_name(Some("release"), RepositoryCapability::GitOverlay, &head),
            "release"
        );
    }

    #[test]
    fn git_overlay_current_thread_push_refuses_mismatched_thread() {
        assert!(git_overlay_current_thread_push_ok(
            false,
            None,
            Some("main")
        ));
        assert!(git_overlay_current_thread_push_ok(
            false,
            Some("main"),
            Some("main")
        ));
        assert!(!git_overlay_current_thread_push_ok(
            false,
            Some("feature"),
            Some("main")
        ));
        assert!(!git_overlay_current_thread_push_ok(
            false,
            Some("feature"),
            None
        ));
        assert!(git_overlay_current_thread_push_ok(
            true,
            Some("feature"),
            Some("main")
        ));
    }

    // --- Push / pull orchestration plan selection tables ---

    fn attached_head(name: &str) -> Head {
        Head::Attached {
            thread: objects::object::ThreadName::new(name),
        }
    }

    fn detached_head() -> Head {
        Head::Detached {
            state: objects::object::ChangeId::generate(),
        }
    }

    fn base_push_request() -> PushPlanRequest {
        PushPlanRequest {
            capability: RepositoryCapability::NativeHeddle,
            hosted_enabled: false,
            uses_hosted_network: false,
            remote: Some("origin".to_string()),
            has_default_remote: true,
            thread: None,
            all_threads: false,
            force: false,
            head: attached_head("main"),
            native_local_heddle_target: false,
            transport_mismatch: false,
        }
    }

    fn base_pull_request() -> PullPlanRequest {
        PullPlanRequest {
            capability: RepositoryCapability::NativeHeddle,
            hosted_enabled: false,
            uses_hosted_network: false,
            remote: Some("origin".to_string()),
            has_default_remote: true,
            thread: None,
            local_thread: None,
            head: attached_head("main"),
            transport_mismatch: false,
            lazy: false,
        }
    }

    #[test]
    fn remote_missing_blocker_table() {
        assert_eq!(
            remote_missing_blocker(None, false),
            Some(RemotePreflightBlocker::MissingRemote)
        );
        assert_eq!(remote_missing_blocker(None, true), None);
        assert_eq!(remote_missing_blocker(Some("origin"), false), None);
        assert_eq!(remote_missing_blocker(Some("origin"), true), None);
    }

    #[test]
    fn transport_mismatch_blocker_table() {
        assert_eq!(
            transport_mismatch_blocker(false, true),
            Some(RemotePreflightBlocker::TransportMismatch)
        );
        assert_eq!(transport_mismatch_blocker(true, true), None);
        assert_eq!(transport_mismatch_blocker(false, false), None);
        assert_eq!(transport_mismatch_blocker(true, false), None);
    }

    #[test]
    fn pull_clean_worktree_policy_table() {
        // (uses_local_overlay, will_materialize) → requires_clean
        let cases = [
            (true, true, true),
            (true, false, true),
            (false, true, true),
            (false, false, false),
        ];
        for (overlay, materialize, expected) in cases {
            assert_eq!(
                pull_requires_clean_worktree(overlay, materialize),
                expected,
                "overlay={overlay} materialize={materialize}"
            );
        }
    }

    #[test]
    fn pull_will_materialize_table() {
        let attached = attached_head("feature");
        let detached = detached_head();
        assert!(pull_will_materialize(None, &attached));
        assert!(pull_will_materialize(Some("feature"), &attached));
        assert!(!pull_will_materialize(Some("other"), &attached));
        assert!(pull_will_materialize(None, &detached));
        assert!(!pull_will_materialize(Some("feature"), &detached));
    }

    #[test]
    fn plan_push_missing_remote() {
        let mut req = base_push_request();
        req.remote = None;
        req.has_default_remote = false;
        assert_eq!(plan_push(&req), Err(RemotePreflightBlocker::MissingRemote));
    }

    #[test]
    fn plan_push_transport_mismatch_on_native_path() {
        let mut req = base_push_request();
        req.transport_mismatch = true;
        assert_eq!(
            plan_push(&req),
            Err(RemotePreflightBlocker::TransportMismatch)
        );
    }

    #[test]
    fn plan_push_ignores_transport_mismatch_on_local_overlay() {
        let mut req = base_push_request();
        req.capability = RepositoryCapability::GitOverlay;
        req.transport_mismatch = true;
        let plan = plan_push(&req).expect("overlay path skips mismatch");
        assert!(plan.uses_local_git_overlay);
        assert!(matches!(plan.path, PushPath::LocalGitOverlayRefs { .. }));
    }

    #[test]
    fn plan_push_git_overlay_thread_mismatch() {
        let mut req = base_push_request();
        req.capability = RepositoryCapability::GitOverlay;
        req.thread = Some("feature".to_string());
        req.head = attached_head("main");
        assert_eq!(
            plan_push(&req),
            Err(RemotePreflightBlocker::GitOverlayThreadMismatch {
                requested: "feature".to_string(),
                attached: Some("main".to_string()),
            })
        );
    }

    #[test]
    fn plan_push_native_local_heddle_skips_thread_mismatch() {
        let mut req = base_push_request();
        req.capability = RepositoryCapability::GitOverlay;
        req.thread = Some("feature".to_string());
        req.head = attached_head("main");
        req.native_local_heddle_target = true;
        let plan = plan_push(&req).expect("native local skips overlay thread gate");
        assert!(matches!(
            plan.path,
            PushPath::LocalNativeHeddle { all_threads: false }
        ));
        assert_eq!(plan.track_name, "feature");
    }

    #[test]
    fn plan_push_hosted_and_fanout_selection_table() {
        // (capability, all_threads) → path fields
        let cases = [
            (
                RepositoryCapability::NativeHeddle,
                true,
                HostedPushPlan::NativePerThreadFanout,
                true,
                false,
            ),
            (
                RepositoryCapability::GitOverlay,
                true,
                HostedPushPlan::GitOverlayMirror,
                false,
                true,
            ),
            (
                RepositoryCapability::GitOverlay,
                false,
                HostedPushPlan::GitOverlayMirror,
                false,
                true,
            ),
            (
                RepositoryCapability::NativeHeddle,
                false,
                HostedPushPlan::NativeSingleThread,
                false,
                false,
            ),
        ];
        for (capability, all_threads, hosted, fanout, mirror) in cases {
            let mut req = base_push_request();
            req.capability = capability;
            req.all_threads = all_threads;
            // Force native remote path (hosted network disables local overlay).
            req.uses_hosted_network = capability == RepositoryCapability::GitOverlay;
            req.hosted_enabled = capability == RepositoryCapability::GitOverlay;
            let plan = plan_push(&req).expect("plan");
            assert_eq!(plan.hosted, hosted, "capability={capability:?}");
            assert_eq!(plan.native_all_threads_fanout, fanout);
            assert_eq!(plan.uses_git_overlay_mirror_rpc, mirror);
            assert!(matches!(
                plan.path,
                PushPath::NativeRemote {
                    hosted: h,
                    uses_mirror_rpc: m,
                    native_all_threads_fanout: f,
                } if h == hosted && m == mirror && f == fanout
            ));
        }
    }

    #[test]
    fn plan_push_local_overlay_refs_path() {
        let mut req = base_push_request();
        req.capability = RepositoryCapability::GitOverlay;
        req.all_threads = true;
        let plan = plan_push(&req).unwrap();
        assert!(plan.uses_local_git_overlay);
        assert_eq!(
            plan.path,
            PushPath::LocalGitOverlayRefs { all_threads: true }
        );
        assert_eq!(plan.track_name, "main");
    }

    #[test]
    fn plan_push_track_name_from_head() {
        let mut req = base_push_request();
        req.remote = Some("origin".into());
        req.head = attached_head("feature");
        let plan = plan_push(&req).unwrap();
        assert_eq!(plan.track_name, "feature");

        req.thread = Some("release".into());
        let plan = plan_push(&req).unwrap();
        assert_eq!(plan.track_name, "release");
    }

    #[test]
    fn plan_pull_missing_remote() {
        let mut req = base_pull_request();
        req.remote = None;
        req.has_default_remote = false;
        assert_eq!(plan_pull(&req), Err(RemotePreflightBlocker::MissingRemote));
    }

    #[test]
    fn plan_pull_transport_mismatch() {
        let mut req = base_pull_request();
        req.transport_mismatch = true;
        assert_eq!(
            plan_pull(&req),
            Err(RemotePreflightBlocker::TransportMismatch)
        );
    }

    #[test]
    fn plan_pull_local_overlay_requires_clean() {
        let mut req = base_pull_request();
        req.capability = RepositoryCapability::GitOverlay;
        req.local_thread = Some("other".into());
        let plan = plan_pull(&req).unwrap();
        assert!(plan.uses_local_git_overlay);
        // will_materialize is false (local_thread != attached), but overlay still requires clean
        assert!(!plan.will_materialize);
        assert!(plan.requires_clean_worktree);
        assert_eq!(plan.remote_thread, "main");
    }

    #[test]
    fn plan_pull_native_materialize_policy() {
        let mut req = base_pull_request();
        req.head = attached_head("feature");
        // no explicit thread → native default remote_thread is "main"
        let plan = plan_pull(&req).unwrap();
        assert!(!plan.uses_local_git_overlay);
        assert!(plan.will_materialize);
        assert!(plan.requires_clean_worktree);
        assert_eq!(plan.remote_thread, "main");

        req.local_thread = Some("scratch".into());
        let plan = plan_pull(&req).unwrap();
        assert!(!plan.will_materialize);
        assert!(!plan.requires_clean_worktree);
    }

    #[test]
    fn plan_pull_thread_defaults_table() {
        let attached = attached_head("master");
        // git-overlay uses attached HEAD
        let mut req = base_pull_request();
        req.capability = RepositoryCapability::GitOverlay;
        req.head = attached.clone();
        let plan = plan_pull(&req).unwrap();
        assert_eq!(plan.remote_thread, "master");

        req.thread = Some("release".into());
        let plan = plan_pull(&req).unwrap();
        assert_eq!(plan.remote_thread, "release");

        // native keeps historical main default
        req.capability = RepositoryCapability::NativeHeddle;
        req.thread = None;
        req.head = attached_head("feature");
        let plan = plan_pull(&req).unwrap();
        assert_eq!(plan.remote_thread, "main");
    }

    // --- Push / pull outcome assembly ---

    #[test]
    fn build_git_overlay_push_outcome_matches_success_json_fields() {
        let mut req = base_push_request();
        req.capability = RepositoryCapability::GitOverlay;
        req.force = true;
        req.all_threads = false;
        let plan = plan_push(&req).unwrap();
        let outcome = build_push_outcome(
            &plan,
            PushExecutionFacts::GitOverlayRefs {
                remote_name: "origin".into(),
                current_thread: Some("main".into()),
                refs_written: vec!["refs/heads/main".into(), "refs/notes/heddle".into()],
                tracking: Some(GitOverlayPushTracking {
                    remote_name: "origin".into(),
                    configured_remote: Some(GitRemoteConfigured {
                        name: "origin".into(),
                        url: "https://example.com/repo.git".into(),
                    }),
                    upstream_branch: Some("main".into()),
                }),
            },
        );
        assert_eq!(outcome.output_kind, "push");
        assert_eq!(outcome.transport, "git");
        assert_eq!(outcome.status, "pushed");
        assert!(outcome.success && outcome.pushed && outcome.changed);
        assert_eq!(outcome.push_scope, Some("current_thread"));
        assert_eq!(outcome.ref_scope, Some("branch_and_heddle_notes"));
        assert_eq!(outcome.git_notes_ref, Some(GIT_NOTES_REF));
        assert_eq!(outcome.force, Some(true));
        assert_eq!(outcome.force_discard_warning, Some(FORCE_DISCARD_WARNING));
        assert_eq!(outcome.tags_included, Some(false));
        assert_eq!(outcome.thread.as_deref(), Some("main"));
        assert_eq!(
            outcome.git_upstream_configured,
            Some(GitUpstreamConfigured {
                branch: "main".into(),
                remote: "origin".into(),
            })
        );
        let summary = summarize_push_outcome(&outcome);
        assert!(summary.contains("force-pushed"), "{summary}");
        assert!(summary.contains("2 refs"), "{summary}");
    }

    #[test]
    fn build_heddle_all_threads_push_outcome_partial_and_sorts_refs() {
        let mut req = base_push_request();
        req.all_threads = true;
        let plan = plan_push(&req).unwrap();
        let outcome = build_push_outcome(
            &plan,
            PushExecutionFacts::HeddleAllThreads {
                pushed_threads: vec!["z".into(), "a".into()],
                failed_threads: vec!["b".into()],
                objects: 4,
            },
        );
        assert_eq!(outcome.status, "partial");
        assert!(!outcome.success);
        assert!(!outcome.pushed);
        assert_eq!(outcome.push_scope, Some("all_threads"));
        assert_eq!(
            outcome.refs_written.as_deref(),
            Some(["a".to_string(), "z".to_string()].as_slice())
        );
        assert_eq!(outcome.objects, Some(4));
        let summary = summarize_push_outcome(&outcome);
        assert!(summary.contains("partial"), "{summary}");
    }

    #[test]
    fn build_heddle_single_push_outcome() {
        let plan = plan_push(&base_push_request()).unwrap();
        let outcome = build_push_outcome(
            &plan,
            PushExecutionFacts::HeddleSingle {
                state: Some("abc123".into()),
                objects: Some(7),
            },
        );
        assert_eq!(outcome.transport, "heddle");
        assert_eq!(outcome.state.as_deref(), Some("abc123"));
        assert_eq!(outcome.objects, Some(7));
        assert!(outcome.refs_written.is_none());
        assert!(summarize_push_outcome(&outcome).contains("abc123"));
    }

    #[test]
    fn build_git_overlay_and_heddle_pull_outcomes() {
        let plan = plan_pull(&base_pull_request()).unwrap();
        let git = build_pull_outcome(
            Some(&plan),
            PullExecutionFacts::GitOverlay {
                remote: "origin".into(),
                branch: Some("main".into()),
                old_git_head: Some("old".into()),
                new_git_head: Some("new".into()),
                old_state: Some("s0".into()),
                new_state: Some("s1".into()),
                changed: true,
                states_created: 2,
                commits_seen: 5,
                materialized_checkout: true,
                changed_paths: vec!["a.rs".into(), "b.rs".into()],
            },
        );
        assert_eq!(git.status, "updated");
        assert_eq!(git.transport, "git");
        assert_eq!(git.changed_path_count, Some(2));
        assert_eq!(git.commits_seen_scope, Some(COMMITS_SEEN_SCOPE));
        assert!(git.pulled && git.changed);
        assert!(summarize_pull_outcome(&git).contains("2 changed paths"));

        let heddle = build_pull_outcome(
            Some(&plan),
            PullExecutionFacts::Heddle {
                changed: false,
                remote: "/tmp/src".into(),
                thread: "main".into(),
                state: Some("s1".into()),
                objects: Some(0),
            },
        );
        assert_eq!(heddle.status, "up_to_date");
        assert!(!heddle.pulled);
        assert_eq!(heddle.thread.as_deref(), Some("main"));
        assert!(summarize_pull_outcome(&heddle).contains("up to date"));
    }

    #[test]
    fn push_and_pull_status_helpers() {
        assert_eq!(push_status(true), "pushed");
        assert_eq!(push_status(false), "partial");
        assert_eq!(pull_status(true), "updated");
        assert_eq!(pull_status(false), "up_to_date");
        assert_eq!(push_scope_label(true), "all_threads");
        assert_eq!(push_scope_label(false), "current_thread");
        assert_eq!(
            git_overlay_ref_scope(true),
            "all_threads_tags_and_heddle_notes"
        );
        assert_eq!(git_overlay_ref_scope(false), "branch_and_heddle_notes");
    }
}
