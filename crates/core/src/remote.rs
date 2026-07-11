// SPDX-License-Identifier: Apache-2.0
//! Remote domain helpers: list/show assembly and pure push/pull orchestration.
//!
//! - List/show: pure report types and default-resolution for `heddle remote
//!   list` / `heddle remote show`.
//! - Push/pull routing: capability → plan decisions (git-overlay mirror vs
//!   native fan-out, default thread selection).
//! - Transport result fields (CLI maps wire/protobuf → plain structs) →
//!   [`PushExecutionFacts`] / [`PullExecutionFacts`], multi-ref progress, and
//!   unstyled working/mirror/ref-list text. No tonic/gRPC types here.
//! - Typed outcomes, failure kinds (map to RecoveryAdvice kinds), multi-ref
//!   progress events, and unstyled human text assembly.
//! - CLI probes the repo, plans, executes network I/O, maps failures, and styles.
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
/// Destination track is `local_thread` when set, otherwise `remote_thread`.
/// Attached HEAD materializes only when that track equals the attached thread.
/// Detached HEAD materializes only when there is no `--local-thread` override.
pub fn pull_will_materialize(local_thread: Option<&str>, remote_thread: &str, head: &Head) -> bool {
    let track = local_thread.unwrap_or(remote_thread);
    match head {
        Head::Attached { thread } => thread == track,
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
    let will_materialize = pull_will_materialize(
        request.local_thread.as_deref(),
        &remote_thread,
        &request.head,
    );
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

// ---------------------------------------------------------------------------
// Typed push/pull failure kinds (pure; CLI maps to RecoveryAdvice)
// ---------------------------------------------------------------------------

/// Stable RecoveryAdvice `kind` strings shared by domain failures and CLI.
pub mod remote_advice_kind {
    pub const REMOTE_NOT_CONFIGURED: &str = "remote_not_configured";
    pub const REMOTE_TRANSPORT_MISMATCH: &str = "remote_transport_mismatch";
    pub const GIT_OVERLAY_THREAD_MISMATCH: &str = "git_overlay_thread_mismatch";
    pub const NAMED_THREAD_TIP_MISMATCH: &str = "named_thread_tip_mismatch";
    pub const REMOTE_PUSH_FAILED: &str = "remote_push_failed";
    pub const REMOTE_PULL_FAILED: &str = "remote_pull_failed";
    pub const LOCAL_LAZY_PULL_UNSUPPORTED: &str = "local_lazy_pull_unsupported";
}

/// Typed push failure. Pure facts only; CLI maps via [`PushFailure::advice_kind`]
/// and field accessors into [`RecoveryAdvice`](crate-external).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PushFailure {
    /// Preflight blocker from [`plan_push`].
    Preflight(RemotePreflightBlocker),
    /// heddle#837: named existing thread tip ≠ current checkout, without `--force`.
    NamedThreadTipMismatch {
        thread: String,
        tip_short: String,
        current_short: String,
    },
    /// Hosted/network push or multi-thread fan-out reported failure.
    RemoteFailed { track_name: String, error: String },
}

/// Typed pull failure. Pure facts only; CLI maps to RecoveryAdvice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PullFailure {
    /// Preflight blocker from [`plan_pull`].
    Preflight(RemotePreflightBlocker),
    /// `--lazy` is unsupported on local path remotes.
    LocalLazyUnsupported { source_path: String },
    /// Hosted/network pull reported failure.
    RemoteFailed {
        remote_thread: String,
        local_thread: Option<String>,
        error: String,
    },
}

impl PushFailure {
    /// RecoveryAdvice `kind` this failure should surface as.
    pub fn advice_kind(&self) -> &'static str {
        match self {
            Self::Preflight(RemotePreflightBlocker::MissingRemote) => {
                remote_advice_kind::REMOTE_NOT_CONFIGURED
            }
            Self::Preflight(RemotePreflightBlocker::TransportMismatch) => {
                remote_advice_kind::REMOTE_TRANSPORT_MISMATCH
            }
            Self::Preflight(RemotePreflightBlocker::GitOverlayThreadMismatch { .. }) => {
                remote_advice_kind::GIT_OVERLAY_THREAD_MISMATCH
            }
            Self::NamedThreadTipMismatch { .. } => remote_advice_kind::NAMED_THREAD_TIP_MISMATCH,
            Self::RemoteFailed { .. } => remote_advice_kind::REMOTE_PUSH_FAILED,
        }
    }

    /// Primary recovery command for this failure (unstyled).
    pub fn primary_command(&self) -> String {
        match self {
            Self::Preflight(RemotePreflightBlocker::MissingRemote) => {
                "heddle remote add <name> <url>".to_string()
            }
            Self::Preflight(RemotePreflightBlocker::TransportMismatch) => {
                "heddle clone <remote> <fresh-path>".to_string()
            }
            Self::Preflight(RemotePreflightBlocker::GitOverlayThreadMismatch {
                requested, ..
            }) => format!("heddle thread switch {requested} && heddle push"),
            Self::NamedThreadTipMismatch { thread, .. } => {
                format!("heddle thread switch {thread}")
            }
            Self::RemoteFailed { track_name, .. } => format!("heddle push {track_name}"),
        }
    }

    /// Operator-facing recovery hint (unstyled prose).
    pub fn recovery_hint(&self) -> String {
        match self {
            Self::Preflight(RemotePreflightBlocker::MissingRemote) => {
                "Add a remote with `heddle remote add <name> <url>`, inspect remotes with `heddle remote list`, or choose one with `heddle remote set-default <name>`. Ad-hoc targets are supported without configuration: `heddle push <remote>` accepts a remote name, URL, local path, or hosted address positionally.".to_string()
            }
            Self::Preflight(RemotePreflightBlocker::TransportMismatch) => {
                "Use a Heddle-native remote here, or clone/adopt that Git remote in a Git-overlay checkout.".to_string()
            }
            Self::Preflight(RemotePreflightBlocker::GitOverlayThreadMismatch {
                requested, ..
            }) => format!(
                "Switch to the requested thread with `heddle thread switch {requested} && heddle push`, or pass `--all-threads`."
            ),
            Self::NamedThreadTipMismatch { thread, .. } => format!(
                "Switch to that thread's checkout (`heddle thread switch {thread}`), or pass `--force` to push the current state under '{thread}'."
            ),
            Self::RemoteFailed { track_name, .. } => format!(
                "Inspect `heddle verify`, then retry with `heddle push {track_name}` after fixing the remote."
            ),
        }
    }
}

impl std::fmt::Display for PushFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Preflight(blocker) => write!(f, "{blocker}"),
            Self::NamedThreadTipMismatch {
                thread,
                tip_short,
                current_short,
            } => write!(
                f,
                "thread '{thread}' already exists at {tip_short} but the current checkout is {current_short}; refusing to overwrite it"
            ),
            Self::RemoteFailed { track_name, error } => {
                write!(f, "Push failed for {track_name}: {error}")
            }
        }
    }
}

impl std::error::Error for PushFailure {}

impl PullFailure {
    /// RecoveryAdvice `kind` this failure should surface as.
    pub fn advice_kind(&self) -> &'static str {
        match self {
            Self::Preflight(RemotePreflightBlocker::MissingRemote) => {
                remote_advice_kind::REMOTE_NOT_CONFIGURED
            }
            Self::Preflight(RemotePreflightBlocker::TransportMismatch) => {
                remote_advice_kind::REMOTE_TRANSPORT_MISMATCH
            }
            Self::Preflight(RemotePreflightBlocker::GitOverlayThreadMismatch { .. }) => {
                // Not raised by plan_pull today; reserved for shared kind map.
                remote_advice_kind::GIT_OVERLAY_THREAD_MISMATCH
            }
            Self::LocalLazyUnsupported { .. } => remote_advice_kind::LOCAL_LAZY_PULL_UNSUPPORTED,
            Self::RemoteFailed { .. } => remote_advice_kind::REMOTE_PULL_FAILED,
        }
    }

    /// Primary recovery command for this failure (unstyled).
    pub fn primary_command(&self) -> String {
        match self {
            Self::Preflight(RemotePreflightBlocker::MissingRemote) => {
                "heddle remote add <name> <url>".to_string()
            }
            Self::Preflight(RemotePreflightBlocker::TransportMismatch) => {
                "heddle clone <remote> <fresh-path>".to_string()
            }
            Self::Preflight(RemotePreflightBlocker::GitOverlayThreadMismatch {
                requested, ..
            }) => format!("heddle thread switch {requested}"),
            Self::LocalLazyUnsupported { source_path } => {
                format!("heddle pull {source_path}")
            }
            Self::RemoteFailed {
                remote_thread,
                local_thread,
                ..
            } => {
                if let Some(local) = local_thread {
                    format!("heddle pull {remote_thread} {local}")
                } else {
                    format!("heddle pull {remote_thread}")
                }
            }
        }
    }

    /// Operator-facing recovery hint (unstyled prose).
    pub fn recovery_hint(&self) -> String {
        match self {
            Self::Preflight(RemotePreflightBlocker::MissingRemote) => {
                "Add a remote with `heddle remote add <name> <url>`, inspect remotes with `heddle remote list`, or choose one with `heddle remote set-default <name>`. Ad-hoc targets are supported without configuration: `heddle pull <remote>` accepts a remote name, URL, local path, or hosted address positionally.".to_string()
            }
            Self::Preflight(RemotePreflightBlocker::TransportMismatch) => {
                "Use a Heddle-native remote here, or clone/adopt that Git remote in a Git-overlay checkout.".to_string()
            }
            Self::Preflight(RemotePreflightBlocker::GitOverlayThreadMismatch { .. }) => {
                "Switch to the attached thread, or omit an explicit mismatched thread name.".to_string()
            }
            Self::LocalLazyUnsupported { source_path } => format!(
                "Run `heddle pull {source_path}` without `--lazy`, or configure a hosted remote and retry lazy pull there."
            ),
            Self::RemoteFailed {
                remote_thread,
                local_thread,
                ..
            } => {
                let cmd = if let Some(local) = local_thread {
                    format!("heddle pull {remote_thread} {local}")
                } else {
                    format!("heddle pull {remote_thread}")
                };
                format!("Inspect `heddle verify`, then retry with `{cmd}` after fixing the remote.")
            }
        }
    }
}

impl std::fmt::Display for PullFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Preflight(blocker) => write!(f, "{blocker}"),
            Self::LocalLazyUnsupported { .. } => write!(
                f,
                "Refusing lazy pull from local remote: lazy materialization requires a hosted or network remote"
            ),
            Self::RemoteFailed {
                remote_thread,
                error,
                ..
            } => write!(f, "Pull failed from {remote_thread}: {error}"),
        }
    }
}

impl std::error::Error for PullFailure {}

impl RemotePreflightBlocker {
    /// RecoveryAdvice `kind` for this preflight blocker.
    pub fn advice_kind(&self) -> &'static str {
        match self {
            Self::MissingRemote => remote_advice_kind::REMOTE_NOT_CONFIGURED,
            Self::TransportMismatch => remote_advice_kind::REMOTE_TRANSPORT_MISMATCH,
            Self::GitOverlayThreadMismatch { .. } => {
                remote_advice_kind::GIT_OVERLAY_THREAD_MISMATCH
            }
        }
    }
}

/// Pure heddle#837 guard: refuse pushing the current checkout under an existing
/// named thread whose tip differs, unless `--force`.
///
/// `existing_tip_differs` is true only when the named thread exists **and** its
/// tip is not the current checkout state. Non-existent threads always allow
/// (push creates them on the remote).
pub fn refuse_named_thread_tip_overwrite(
    force: bool,
    named_thread: Option<&str>,
    existing_tip_differs: bool,
) -> bool {
    named_thread.is_some() && !force && existing_tip_differs
}

/// Build a [`PushFailure::NamedThreadTipMismatch`] for the heddle#837 refuse path.
pub fn named_thread_tip_mismatch_failure(
    thread: &str,
    tip_short: impl Into<String>,
    current_short: impl Into<String>,
) -> PushFailure {
    PushFailure::NamedThreadTipMismatch {
        thread: thread.to_string(),
        tip_short: tip_short.into(),
        current_short: current_short.into(),
    }
}

/// First multi-thread push failure as a typed [`PushFailure`], if any.
pub fn first_multi_thread_push_failure(failures: &[(String, String)]) -> Option<PushFailure> {
    failures
        .first()
        .map(|(name, err)| remote_push_failure(name, Some(err.as_str())))
}

/// Default message when a transport result omits or blanks its error string.
pub const UNKNOWN_TRANSPORT_ERROR: &str = "Unknown error";

/// Normalize an optional transport error string for failure construction.
///
/// Empty/whitespace-only strings are treated as missing (same as `None`).
pub fn transport_error_message(error: Option<&str>) -> String {
    match error.map(str::trim).filter(|s| !s.is_empty()) {
        Some(s) => s.to_string(),
        None => UNKNOWN_TRANSPORT_ERROR.to_string(),
    }
}

/// Build [`PushFailure::RemoteFailed`] from a track name + optional transport error.
pub fn remote_push_failure(track_name: &str, error: Option<&str>) -> PushFailure {
    PushFailure::RemoteFailed {
        track_name: track_name.to_string(),
        error: transport_error_message(error),
    }
}

/// Build [`PullFailure::RemoteFailed`] from pull target + optional transport error.
pub fn remote_pull_failure(
    remote_thread: &str,
    local_thread: Option<&str>,
    error: Option<&str>,
) -> PullFailure {
    PullFailure::RemoteFailed {
        remote_thread: remote_thread.to_string(),
        local_thread: local_thread.map(str::to_string),
        error: transport_error_message(error),
    }
}

/// Thread names reported as failed in a multi-thread push fan-out (order preserved).
pub fn multi_thread_failed_names(failures: &[(String, String)]) -> Vec<String> {
    failures.iter().map(|(thread, _)| thread.clone()).collect()
}

/// Sorted list of refs/threads reported as successfully pushed (JSON contract).
///
/// Matches the sort applied inside [`build_push_outcome`] for
/// [`PushExecutionFacts::HeddleAllThreads`] so callers can preview `refs_written`
/// without assembling a full outcome.
pub fn multi_thread_reported_refs(pushed_threads: &[String]) -> Vec<String> {
    let mut refs = pushed_threads.to_vec();
    refs.sort();
    refs
}

/// Assemble multi-thread push execution facts: which refs landed vs failed.
///
/// Pure: no I/O. `pushed_threads` are the threads that landed (unsorted;
/// [`build_push_outcome`] sorts for JSON `refs_written`). `failures` are
/// `(thread, error)` pairs from the fan-out loop.
pub fn multi_thread_push_execution_facts(
    pushed_threads: Vec<String>,
    failures: &[(String, String)],
    objects: usize,
) -> PushExecutionFacts {
    PushExecutionFacts::HeddleAllThreads {
        pushed_threads,
        failed_threads: multi_thread_failed_names(failures),
        objects,
    }
}

// ---------------------------------------------------------------------------
// Transport result fields → execution facts (no gRPC / wire types in core)
// ---------------------------------------------------------------------------
//
// CLI maps protobuf / `wire::*Complete` / local transfer counts into these
// plain field structs, then calls the pure constructors below. Domain builds
// [`PushExecutionFacts`] / [`PullExecutionFacts`]; CLI never invents outcome
// JSON fields outside `build_*_outcome`.

/// Caller-mapped hosted push transport fields (no wire/protobuf types).
///
/// Map from transport `success` / `new_state` / `error` before invoking pure
/// parse helpers. State is already stringified by the caller (full or short).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostedPushResultFields {
    pub success: bool,
    pub new_state: Option<String>,
    pub error: Option<String>,
}

/// Caller-mapped hosted pull transport fields (no wire/protobuf types).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostedPullResultFields {
    pub success: bool,
    pub final_state: Option<String>,
    pub error: Option<String>,
}

/// Local path transfer counts/SHAs after a successful single-thread push/pull.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalTransferSummary {
    pub state: Option<String>,
    pub objects: Option<usize>,
}

/// Parsed hosted push: success state string or typed [`PushFailure`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostedPushResult {
    /// Transport reported success; `state` is the remote tip when present.
    Success { state: Option<String> },
    /// Transport reported failure (or blank error → [`UNKNOWN_TRANSPORT_ERROR`]).
    Failed(PushFailure),
}

/// Parsed hosted pull: final state string or typed [`PullFailure`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostedPullResult {
    /// Transport reported success; `final_state` is the tip when present.
    Success { final_state: Option<String> },
    /// Transport reported failure.
    Failed(PullFailure),
}

/// Parse hosted push fields into success state or [`PushFailure`].
///
/// Pure: no network I/O. Callers map wire/protobuf → [`HostedPushResultFields`]
/// first.
pub fn parse_hosted_push_result(
    track_name: &str,
    fields: &HostedPushResultFields,
) -> HostedPushResult {
    if fields.success {
        HostedPushResult::Success {
            state: fields.new_state.clone(),
        }
    } else {
        HostedPushResult::Failed(remote_push_failure(track_name, fields.error.as_deref()))
    }
}

/// Parse hosted pull fields into final state or [`PullFailure`].
pub fn parse_hosted_pull_result(
    remote_thread: &str,
    local_thread: Option<&str>,
    fields: &HostedPullResultFields,
) -> HostedPullResult {
    if fields.success {
        HostedPullResult::Success {
            final_state: fields.final_state.clone(),
        }
    } else {
        HostedPullResult::Failed(remote_pull_failure(
            remote_thread,
            local_thread,
            fields.error.as_deref(),
        ))
    }
}

/// Single-thread native push execution facts from state/object counts.
pub fn heddle_single_push_execution_facts(
    state: Option<String>,
    objects: Option<usize>,
) -> PushExecutionFacts {
    PushExecutionFacts::HeddleSingle { state, objects }
}

/// Map a local transfer summary into single-thread push execution facts.
pub fn heddle_single_push_execution_facts_from_local(
    summary: &LocalTransferSummary,
) -> PushExecutionFacts {
    heddle_single_push_execution_facts(summary.state.clone(), summary.objects)
}

/// Map hosted push success fields into single-thread execution facts.
///
/// Object counts are unknown on the hosted path (`None`). Caller must only
/// invoke this after [`parse_hosted_push_result`] reports success (or when
/// `fields.success` is already known true).
pub fn heddle_single_push_execution_facts_from_hosted(
    fields: &HostedPushResultFields,
) -> PushExecutionFacts {
    heddle_single_push_execution_facts(fields.new_state.clone(), None)
}

/// Git-overlay refs push execution facts (local `GitProjection` path).
pub fn git_overlay_push_execution_facts(
    remote_name: String,
    current_thread: Option<String>,
    refs_written: Vec<String>,
    tracking: Option<GitOverlayPushTracking>,
) -> PushExecutionFacts {
    PushExecutionFacts::GitOverlayRefs {
        remote_name,
        current_thread,
        refs_written,
        tracking,
    }
}

/// Native heddle pull execution facts.
pub fn heddle_pull_execution_facts(
    changed: bool,
    remote: String,
    thread: String,
    state: Option<String>,
    objects: Option<usize>,
) -> PullExecutionFacts {
    PullExecutionFacts::Heddle {
        changed,
        remote,
        thread,
        state,
        objects,
    }
}

/// Map hosted pull success fields + materialize change flag into pull facts.
///
/// Object counts are unknown on the hosted path (`None`).
pub fn heddle_pull_execution_facts_from_hosted(
    changed: bool,
    remote: String,
    thread: String,
    fields: &HostedPullResultFields,
) -> PullExecutionFacts {
    heddle_pull_execution_facts(changed, remote, thread, fields.final_state.clone(), None)
}

/// Map a local transfer summary into heddle pull execution facts.
pub fn heddle_pull_execution_facts_from_local(
    changed: bool,
    remote: String,
    thread: String,
    summary: &LocalTransferSummary,
) -> PullExecutionFacts {
    heddle_pull_execution_facts(
        changed,
        remote,
        thread,
        summary.state.clone(),
        summary.objects,
    )
}

/// Git-overlay pull / import execution facts.
#[allow(clippy::too_many_arguments)]
pub fn git_overlay_pull_execution_facts(
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
) -> PullExecutionFacts {
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
    }
}

/// Whether a pull tip moved: `final_state` differs from the pre-pull tip.
///
/// When `final_state` is missing, the tip is treated as unchanged (hosted
/// success-with-no-state is a no-op for ref advance).
pub fn pull_tip_changed(pre_target: Option<&str>, final_state: Option<&str>) -> bool {
    match final_state {
        Some(state) => pre_target != Some(state),
        None => false,
    }
}

/// Local-path pull change: tip moved **or** objects were copied.
pub fn local_pull_changed(
    pre_target: Option<&str>,
    final_state: &str,
    objects_copied: usize,
) -> bool {
    pre_target != Some(final_state) || objects_copied > 0
}

// ---------------------------------------------------------------------------
// Multi-ref push progress (pure event facts for --all-threads fan-out)
// ---------------------------------------------------------------------------

/// Progress facts for a multi-thread / multi-ref push fan-out (heddle#838).
///
/// CLI owns TTY rendering and styling; domain only names the pure events and
/// unstyled text lines. Live byte-upload progress remains on the transport
/// progress handle and is out of scope here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MultiRefPushProgress {
    /// Fan-out is about to begin.
    Begin {
        /// Display target (for example `file:///path` or a host address).
        target: String,
    },
    /// One thread landed successfully.
    ThreadSucceeded {
        thread: String,
        /// Short state id when known (local path push).
        state_short: Option<String>,
        /// Objects copied when known (local path push).
        objects: Option<usize>,
        /// Hosted remote state id when known.
        remote_state: Option<String>,
    },
    /// One thread failed; fan-out continues for remaining threads.
    ThreadFailed { thread: String, error: String },
}

/// Begin multi-ref fan-out progress for a display target.
pub fn multi_ref_push_begin(target: impl Into<String>) -> MultiRefPushProgress {
    MultiRefPushProgress::Begin {
        target: target.into(),
    }
}

/// Local-path thread success progress (state short + objects when known).
pub fn multi_ref_thread_succeeded_local(
    thread: impl Into<String>,
    state_short: Option<String>,
    objects: Option<usize>,
) -> MultiRefPushProgress {
    MultiRefPushProgress::ThreadSucceeded {
        thread: thread.into(),
        state_short,
        objects,
        remote_state: None,
    }
}

/// Hosted-path thread success progress (remote state when known).
pub fn multi_ref_thread_succeeded_hosted(
    thread: impl Into<String>,
    remote_state: Option<String>,
) -> MultiRefPushProgress {
    MultiRefPushProgress::ThreadSucceeded {
        thread: thread.into(),
        state_short: None,
        objects: None,
        remote_state,
    }
}

/// Thread failure progress with normalized transport error text.
pub fn multi_ref_thread_failed(
    thread: impl Into<String>,
    error: Option<&str>,
) -> MultiRefPushProgress {
    MultiRefPushProgress::ThreadFailed {
        thread: thread.into(),
        error: transport_error_message(error),
    }
}

/// Map hosted per-thread push fields into a multi-ref progress event.
///
/// Pure result-summary: success → [`MultiRefPushProgress::ThreadSucceeded`]
/// with `remote_state`; failure → [`MultiRefPushProgress::ThreadFailed`] with
/// normalized error text.
pub fn multi_ref_progress_from_hosted_thread(
    thread: &str,
    fields: &HostedPushResultFields,
) -> MultiRefPushProgress {
    if fields.success {
        multi_ref_thread_succeeded_hosted(thread, fields.new_state.clone())
    } else {
        multi_ref_thread_failed(thread, fields.error.as_deref())
    }
}

/// Unstyled human line for a multi-ref progress fact (no TTY markers).
pub fn format_multi_ref_push_progress(event: &MultiRefPushProgress) -> String {
    match event {
        MultiRefPushProgress::Begin { target } => {
            format!("pushing all threads to {target}")
        }
        MultiRefPushProgress::ThreadSucceeded {
            thread,
            state_short: Some(state),
            objects: Some(n),
            ..
        } => {
            let unit = if *n == 1 { "object" } else { "objects" };
            format!("pushed {state} to {thread} ({n} {unit})")
        }
        MultiRefPushProgress::ThreadSucceeded {
            thread,
            state_short: Some(state),
            objects: None,
            ..
        } => format!("pushed {state} to {thread}"),
        MultiRefPushProgress::ThreadSucceeded {
            thread,
            state_short: None,
            objects: Some(n),
            ..
        } => {
            let unit = if *n == 1 { "object" } else { "objects" };
            format!("pushed to {thread} ({n} {unit})")
        }
        MultiRefPushProgress::ThreadSucceeded {
            thread,
            remote_state: Some(state),
            ..
        } => format!("pushed to {thread} (remote state {state})"),
        MultiRefPushProgress::ThreadSucceeded { thread, .. } => {
            format!("pushed to {thread}")
        }
        MultiRefPushProgress::ThreadFailed { thread, error } => {
            format!("failed to push {thread}: {error}")
        }
    }
}

/// Comma-separated ref/thread list for multi-thread push reporting.
///
/// Order is preserved (caller sorts via [`multi_thread_reported_refs`] when
/// the JSON `refs_written` order is required).
pub fn format_ref_list(refs: &[String]) -> String {
    refs.join(", ")
}

/// Unstyled detail line for landed multi-thread refs (`refs: a, b`), sorted.
///
/// Returns `None` when no threads landed (partial fan-out with zero success).
pub fn format_multi_thread_refs_detail(pushed_threads: &[String]) -> Option<String> {
    if pushed_threads.is_empty() {
        return None;
    }
    let sorted = multi_thread_reported_refs(pushed_threads);
    Some(format!("refs: {}", format_ref_list(&sorted)))
}

// ---------------------------------------------------------------------------
// Unstyled working / mirror lines (CLI adds markers + style)
// ---------------------------------------------------------------------------

/// Unstyled "pushing to …" working line.
pub fn format_pushing_to(target: &str) -> String {
    format!("pushing to {target}")
}

/// Unstyled "pulling from …" working line.
pub fn format_pulling_from(source: &str) -> String {
    format!("pulling from {source}")
}

/// Unstyled "connected to …" line after a network session opens.
pub fn format_connected_to(addr: &str) -> String {
    format!("connected to {addr}")
}

/// Unstyled remote-state detail field (`remote state: {state}`).
pub fn format_remote_state_detail(state: &str) -> String {
    format!("remote state: {state}")
}

/// Unstyled mirror success line (heddle#25 ad-hoc dual-push).
pub fn format_mirror_success_text(remote: &str) -> String {
    format!("mirrored to {remote}")
}

/// Unstyled mirror failure line (primary push still succeeded).
pub fn format_mirror_failure_text(remote: &str, error: &str) -> String {
    format!("mirror push to {remote} failed (primary push still succeeded): {error}")
}

// ---------------------------------------------------------------------------
// Human text assembly from outcomes (pure; CLI adds style markers)
// ---------------------------------------------------------------------------

/// Unstyled human text derived from a [`PushOutcome`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushOutcomeText {
    /// Primary success / partial line.
    pub headline: String,
    /// Follow-on detail lines (force warning, notes visibility, tracking).
    pub detail_lines: Vec<String>,
}

/// Unstyled human text derived from a [`PullOutcome`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullOutcomeText {
    /// Primary success / up-to-date line.
    pub headline: String,
    /// Follow-on detail lines (branch, import stats, changed paths, …).
    pub detail_lines: Vec<String>,
}

/// Git-overlay scope description for text mode (matches historical CLI copy).
pub fn git_overlay_push_scope_description(all_threads: bool) -> &'static str {
    if all_threads {
        "all threads + Git tags + refs/notes/heddle"
    } else {
        "branch + refs/notes/heddle; tags skipped"
    }
}

/// Unstyled note when a single git-mirror transfer covers `--all-threads`
/// (heddle#846 collapse: every ref ships in one pack, not a per-thread loop).
pub const ALL_THREADS_MIRROR_COVERS_NOTE: &str =
    "Git Projection push covers all threads (every ref shipped in one transfer)";

/// Pure force / all-threads display policy for network text mode after a
/// successful single-shot push (mirror or native single-thread).
///
/// Returns the unstyled all-threads coverage note when the CLI took the
/// collapsed mirror path with `--all-threads`. Force discard warnings for
/// git-overlay refs push remain on [`format_push_outcome_text`] via
/// [`FORCE_DISCARD_WARNING`].
pub fn all_threads_mirror_coverage_note(all_threads: bool) -> Option<&'static str> {
    all_threads.then_some(ALL_THREADS_MIRROR_COVERS_NOTE)
}

/// Assemble unstyled human text from a push outcome.
///
/// `track_name` fills the heddle single-thread headline when the outcome does
/// not carry a thread field (JSON contract keeps that field optional).
pub fn format_push_outcome_text(
    outcome: &PushOutcome,
    track_name: Option<&str>,
) -> PushOutcomeText {
    let headline = match outcome.transport {
        "git" => {
            let remote = outcome.remote.as_deref().unwrap_or("remote");
            let all_threads = outcome.push_scope == Some("all_threads");
            let subject = if all_threads {
                "all threads".to_string()
            } else {
                outcome
                    .thread
                    .as_deref()
                    .map(|t| format!("thread {t}"))
                    .unwrap_or_else(|| "current thread".to_string())
            };
            format!(
                "pushed {subject} to {remote} ({})",
                git_overlay_push_scope_description(all_threads)
            )
        }
        "heddle" if outcome.push_scope == Some("all_threads") => summarize_push_outcome(outcome),
        "heddle" => {
            let track = track_name.or(outcome.thread.as_deref()).unwrap_or("thread");
            match (&outcome.state, outcome.objects) {
                (Some(state), Some(objects)) => {
                    let unit = if objects == 1 { "object" } else { "objects" };
                    format!("pushed {state} to {track} ({objects} {unit})")
                }
                (Some(state), None) => format!("pushed to {track} (state {state})"),
                (None, Some(objects)) => {
                    let unit = if objects == 1 { "object" } else { "objects" };
                    format!("pushed to {track} ({objects} {unit})")
                }
                (None, None) => format!("pushed to {track}"),
            }
        }
        _ => summarize_push_outcome(outcome),
    };

    let mut detail_lines = Vec::new();
    if let Some(warning) = outcome.force_discard_warning {
        detail_lines.push(format!("Force: {warning}."));
    }
    if outcome.git_notes_ref.is_some() {
        detail_lines.push(format!(
            "Git interop: published {GIT_NOTES_REF}; ordinary `git log --all` may show Heddle metadata commits."
        ));
    }
    if let Some(configured) = &outcome.git_remote_configured {
        detail_lines.push(format!(
            "Git tracking: configured remote {} -> {} for future fetch/push.",
            configured.name, configured.url
        ));
    }
    if let Some(upstream) = &outcome.git_upstream_configured {
        detail_lines.push(format!(
            "Git tracking: branch {} tracks {}/{}.",
            upstream.branch, upstream.remote, upstream.branch
        ));
    }

    PushOutcomeText {
        headline,
        detail_lines,
    }
}

/// Assemble unstyled human text from a pull outcome.
///
/// Path lists are truncated to `max_paths` entries with an overflow line.
pub fn format_pull_outcome_text(outcome: &PullOutcome, max_paths: usize) -> PullOutcomeText {
    let headline = if !outcome.changed {
        format!(
            "already up to date with {}; repository verification checked below",
            outcome.remote
        )
    } else if outcome.transport == "git" {
        format!("pulled from {}", outcome.remote)
    } else if let (Some(state), Some(objects)) = (&outcome.state, outcome.objects) {
        let unit = if objects == 1 { "object" } else { "objects" };
        let thread = outcome.thread.as_deref().unwrap_or("thread");
        format!("pulled {state} from {thread} ({objects} {unit})")
    } else if outcome.transport == "heddle" {
        format!(
            "pulled from {}",
            outcome.thread.as_deref().unwrap_or(outcome.remote.as_str())
        )
    } else {
        summarize_pull_outcome(outcome)
    };

    let mut detail_lines = Vec::new();
    if outcome.transport == "git" {
        if let Some(branch) = &outcome.branch {
            if outcome.changed {
                detail_lines.push(format!("Branch: {branch}"));
            } else if let Some(head) = &outcome.new_git_head {
                let short: String = head.chars().take(12).collect();
                detail_lines.push(format!("Branch: {branch} at {short}"));
            }
        }
        match (&outcome.old_git_head, &outcome.new_git_head) {
            (Some(old), Some(new)) if old != new => {
                let old_s: String = old.chars().take(12).collect();
                let new_s: String = new.chars().take(12).collect();
                detail_lines.push(format!("Git: {old_s} -> {new_s}"));
            }
            (Some(head), Some(_)) if outcome.changed => {
                let short: String = head.chars().take(12).collect();
                detail_lines.push(format!("Git: {short}"));
            }
            _ => {}
        }
        if let Some(states) = outcome.states_created {
            let unit = if states == 1 {
                "new state"
            } else {
                "new states"
            };
            detail_lines.push(format!("Imported: {states} {unit}"));
        }
        if let Some(commits) = outcome.commits_seen {
            let unit = if commits == 1 {
                "Git commit object"
            } else {
                "Git commit objects"
            };
            detail_lines.push(format!(
                "Scanned: {commits} {unit} across branches + refs/notes/heddle"
            ));
        }
        if outcome.materialized_checkout == Some(true) {
            detail_lines.push("Worktree: materialized checkout".to_string());
        }
        if outcome.changed
            && let Some(paths) = &outcome.changed_paths
        {
            detail_lines.push(format!("Changed paths: {}", paths.len()));
            for path in paths.iter().take(max_paths) {
                detail_lines.push(format!("  - {path}"));
            }
            if paths.len() > max_paths {
                detail_lines.push(format!("  - ... {} more", paths.len() - max_paths));
            }
        }
    } else if outcome.changed
        && let Some(state) = &outcome.state
        && outcome.objects.is_none()
    {
        // Hosted pull: print state as a field line (CLI styles separately when needed).
        detail_lines.push(format!("state: {state}"));
    }

    PullOutcomeText {
        headline,
        detail_lines,
    }
}

/// Whether a network pull should materialize the checkout after fetch.
///
/// Combines plan materialize policy with lazy mode (lazy never materializes).
pub fn pull_should_materialize(will_materialize: bool, lazy: bool) -> bool {
    will_materialize && !lazy
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
        // local_thread None → destination is remote_thread
        assert!(pull_will_materialize(None, "feature", &attached));
        assert!(!pull_will_materialize(None, "main", &attached));
        assert!(pull_will_materialize(Some("feature"), "main", &attached));
        assert!(!pull_will_materialize(Some("other"), "feature", &attached));
        assert!(pull_will_materialize(None, "main", &detached));
        assert!(!pull_will_materialize(Some("feature"), "main", &detached));
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
        // no explicit thread → native default remote_thread is "main" ≠ attached
        let plan = plan_pull(&req).unwrap();
        assert!(!plan.uses_local_git_overlay);
        assert!(!plan.will_materialize);
        assert!(!plan.requires_clean_worktree);
        assert_eq!(plan.remote_thread, "main");

        // Explicit remote thread matching attached HEAD materializes.
        req.thread = Some("feature".into());
        let plan = plan_pull(&req).unwrap();
        assert!(plan.will_materialize);
        assert!(plan.requires_clean_worktree);

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

    // --- Typed failures, multi-ref progress, outcome text ---

    #[test]
    fn push_failure_advice_kinds_map_to_recovery_kinds() {
        assert_eq!(
            PushFailure::Preflight(RemotePreflightBlocker::MissingRemote).advice_kind(),
            remote_advice_kind::REMOTE_NOT_CONFIGURED
        );
        assert_eq!(
            PushFailure::Preflight(RemotePreflightBlocker::TransportMismatch).advice_kind(),
            remote_advice_kind::REMOTE_TRANSPORT_MISMATCH
        );
        assert_eq!(
            PushFailure::Preflight(RemotePreflightBlocker::GitOverlayThreadMismatch {
                requested: "feature".into(),
                attached: Some("main".into()),
            })
            .advice_kind(),
            remote_advice_kind::GIT_OVERLAY_THREAD_MISMATCH
        );
        assert_eq!(
            named_thread_tip_mismatch_failure("feat", "aaa", "bbb").advice_kind(),
            remote_advice_kind::NAMED_THREAD_TIP_MISMATCH
        );
        assert_eq!(
            PushFailure::RemoteFailed {
                track_name: "main".into(),
                error: "boom".into(),
            }
            .advice_kind(),
            remote_advice_kind::REMOTE_PUSH_FAILED
        );
    }

    #[test]
    fn pull_failure_advice_kinds_map_to_recovery_kinds() {
        assert_eq!(
            PullFailure::LocalLazyUnsupported {
                source_path: "/tmp/src".into(),
            }
            .advice_kind(),
            remote_advice_kind::LOCAL_LAZY_PULL_UNSUPPORTED
        );
        assert_eq!(
            PullFailure::RemoteFailed {
                remote_thread: "main".into(),
                local_thread: None,
                error: "no".into(),
            }
            .advice_kind(),
            remote_advice_kind::REMOTE_PULL_FAILED
        );
    }

    #[test]
    fn named_thread_tip_overwrite_guard_table() {
        // (force, named, tip_differs) → refuse
        let cases = [
            (false, Some("feat"), true, true),
            (true, Some("feat"), true, false),
            (false, Some("feat"), false, false),
            (false, None, true, false),
            (false, None, false, false),
        ];
        for (force, named, differs, refuse) in cases {
            assert_eq!(
                refuse_named_thread_tip_overwrite(force, named, differs),
                refuse,
                "force={force} named={named:?} differs={differs}"
            );
        }
    }

    #[test]
    fn first_multi_thread_push_failure_picks_first() {
        assert!(first_multi_thread_push_failure(&[]).is_none());
        let failure = first_multi_thread_push_failure(&[
            ("a".into(), "e1".into()),
            ("b".into(), "e2".into()),
        ])
        .unwrap();
        assert_eq!(
            failure,
            PushFailure::RemoteFailed {
                track_name: "a".into(),
                error: "e1".into(),
            }
        );
    }

    #[test]
    fn transport_error_message_defaults_and_trims() {
        assert_eq!(transport_error_message(None), UNKNOWN_TRANSPORT_ERROR);
        assert_eq!(transport_error_message(Some("")), UNKNOWN_TRANSPORT_ERROR);
        assert_eq!(
            transport_error_message(Some("   ")),
            UNKNOWN_TRANSPORT_ERROR
        );
        assert_eq!(transport_error_message(Some(" boom ")), "boom");
    }

    #[test]
    fn remote_push_and_pull_failure_from_transport_errors() {
        assert_eq!(
            remote_push_failure("main", None),
            PushFailure::RemoteFailed {
                track_name: "main".into(),
                error: UNKNOWN_TRANSPORT_ERROR.into(),
            }
        );
        assert_eq!(
            remote_push_failure("feat", Some("refused")),
            PushFailure::RemoteFailed {
                track_name: "feat".into(),
                error: "refused".into(),
            }
        );
        assert_eq!(
            remote_pull_failure("main", Some("local"), None),
            PullFailure::RemoteFailed {
                remote_thread: "main".into(),
                local_thread: Some("local".into()),
                error: UNKNOWN_TRANSPORT_ERROR.into(),
            }
        );
        assert_eq!(
            remote_pull_failure("main", None, Some("gone")),
            PullFailure::RemoteFailed {
                remote_thread: "main".into(),
                local_thread: None,
                error: "gone".into(),
            }
        );
    }

    #[test]
    fn multi_thread_reported_refs_and_execution_facts() {
        let failures = [("b".into(), "e".into()), ("c".into(), "e2".into())];
        assert_eq!(
            multi_thread_failed_names(&failures),
            vec!["b".to_string(), "c".to_string()]
        );
        assert_eq!(
            multi_thread_reported_refs(&["z".into(), "a".into()]),
            vec!["a".to_string(), "z".to_string()]
        );
        let facts = multi_thread_push_execution_facts(vec!["z".into(), "a".into()], &failures, 3);
        assert_eq!(
            facts,
            PushExecutionFacts::HeddleAllThreads {
                pushed_threads: vec!["z".into(), "a".into()],
                failed_threads: vec!["b".into(), "c".into()],
                objects: 3,
            }
        );
        let mut req = base_push_request();
        req.all_threads = true;
        let plan = plan_push(&req).unwrap();
        let outcome = build_push_outcome(&plan, facts);
        assert_eq!(
            outcome.refs_written.as_deref(),
            Some(["a".to_string(), "z".to_string()].as_slice())
        );
        assert_eq!(outcome.status, "partial");
    }

    #[test]
    fn all_threads_mirror_coverage_note_policy() {
        assert_eq!(
            all_threads_mirror_coverage_note(true),
            Some(ALL_THREADS_MIRROR_COVERS_NOTE)
        );
        assert_eq!(all_threads_mirror_coverage_note(false), None);
    }

    #[test]
    fn hosted_push_result_parse_and_execution_facts() {
        let ok = HostedPushResultFields {
            success: true,
            new_state: Some("s1".into()),
            error: None,
        };
        assert_eq!(
            parse_hosted_push_result("main", &ok),
            HostedPushResult::Success {
                state: Some("s1".into())
            }
        );
        assert_eq!(
            heddle_single_push_execution_facts_from_hosted(&ok),
            PushExecutionFacts::HeddleSingle {
                state: Some("s1".into()),
                objects: None,
            }
        );
        let fail = HostedPushResultFields {
            success: false,
            new_state: None,
            error: Some(" refused ".into()),
        };
        assert_eq!(
            parse_hosted_push_result("feat", &fail),
            HostedPushResult::Failed(PushFailure::RemoteFailed {
                track_name: "feat".into(),
                error: "refused".into(),
            })
        );
        let local = LocalTransferSummary {
            state: Some("abc".into()),
            objects: Some(3),
        };
        assert_eq!(
            heddle_single_push_execution_facts_from_local(&local),
            PushExecutionFacts::HeddleSingle {
                state: Some("abc".into()),
                objects: Some(3),
            }
        );
    }

    #[test]
    fn hosted_pull_result_parse_and_execution_facts() {
        let ok = HostedPullResultFields {
            success: true,
            final_state: Some("s9".into()),
            error: None,
        };
        assert_eq!(
            parse_hosted_pull_result("main", Some("local"), &ok),
            HostedPullResult::Success {
                final_state: Some("s9".into())
            }
        );
        assert_eq!(
            heddle_pull_execution_facts_from_hosted(true, "origin".into(), "main".into(), &ok),
            PullExecutionFacts::Heddle {
                changed: true,
                remote: "origin".into(),
                thread: "main".into(),
                state: Some("s9".into()),
                objects: None,
            }
        );
        let fail = HostedPullResultFields {
            success: false,
            final_state: None,
            error: None,
        };
        assert_eq!(
            parse_hosted_pull_result("main", None, &fail),
            HostedPullResult::Failed(PullFailure::RemoteFailed {
                remote_thread: "main".into(),
                local_thread: None,
                error: UNKNOWN_TRANSPORT_ERROR.into(),
            })
        );
        assert!(pull_tip_changed(Some("a"), Some("b")));
        assert!(!pull_tip_changed(Some("a"), Some("a")));
        assert!(!pull_tip_changed(Some("a"), None));
        assert!(local_pull_changed(Some("a"), "a", 1));
        assert!(!local_pull_changed(Some("a"), "a", 0));
    }

    #[test]
    fn multi_ref_progress_constructors_and_ref_list() {
        assert_eq!(
            multi_ref_push_begin("file:///tmp/r"),
            MultiRefPushProgress::Begin {
                target: "file:///tmp/r".into(),
            }
        );
        let local = multi_ref_thread_succeeded_local("main", Some("abc".into()), Some(2));
        assert_eq!(
            format_multi_ref_push_progress(&local),
            "pushed abc to main (2 objects)"
        );
        let hosted_fields = HostedPushResultFields {
            success: true,
            new_state: Some("s1".into()),
            error: None,
        };
        assert_eq!(
            multi_ref_progress_from_hosted_thread("feat", &hosted_fields),
            multi_ref_thread_succeeded_hosted("feat", Some("s1".into()))
        );
        let fail_fields = HostedPushResultFields {
            success: false,
            new_state: None,
            error: Some("boom".into()),
        };
        assert_eq!(
            format_multi_ref_push_progress(&multi_ref_progress_from_hosted_thread(
                "x",
                &fail_fields
            )),
            "failed to push x: boom"
        );
        assert_eq!(
            format_ref_list(&["b".into(), "a".into()]),
            "b, a".to_string()
        );
        assert_eq!(
            format_multi_thread_refs_detail(&["z".into(), "a".into()]).as_deref(),
            Some("refs: a, z")
        );
        assert!(format_multi_thread_refs_detail(&[]).is_none());
    }

    #[test]
    fn working_and_mirror_text_helpers() {
        assert_eq!(format_pushing_to("file:///r"), "pushing to file:///r");
        assert_eq!(format_pulling_from("file:///s"), "pulling from file:///s");
        assert_eq!(
            format_connected_to("127.0.0.1:1"),
            "connected to 127.0.0.1:1"
        );
        assert_eq!(format_remote_state_detail("s1"), "remote state: s1");
        assert_eq!(format_mirror_success_text("origin"), "mirrored to origin");
        assert!(format_mirror_failure_text("m", "e").contains("mirror push to m failed"));
    }

    #[test]
    fn multi_ref_push_progress_formatting() {
        assert_eq!(
            format_multi_ref_push_progress(&MultiRefPushProgress::Begin {
                target: "file:///tmp/r".into(),
            }),
            "pushing all threads to file:///tmp/r"
        );
        assert_eq!(
            format_multi_ref_push_progress(&MultiRefPushProgress::ThreadSucceeded {
                thread: "main".into(),
                state_short: Some("abc".into()),
                objects: Some(1),
                remote_state: None,
            }),
            "pushed abc to main (1 object)"
        );
        assert_eq!(
            format_multi_ref_push_progress(&MultiRefPushProgress::ThreadSucceeded {
                thread: "main".into(),
                state_short: Some("abc".into()),
                objects: Some(2),
                remote_state: None,
            }),
            "pushed abc to main (2 objects)"
        );
        assert_eq!(
            format_multi_ref_push_progress(&MultiRefPushProgress::ThreadSucceeded {
                thread: "feat".into(),
                state_short: None,
                objects: None,
                remote_state: Some("s1".into()),
            }),
            "pushed to feat (remote state s1)"
        );
        assert_eq!(
            format_multi_ref_push_progress(&MultiRefPushProgress::ThreadFailed {
                thread: "x".into(),
                error: "nope".into(),
            }),
            "failed to push x: nope"
        );
    }

    #[test]
    fn format_push_outcome_text_git_overlay_details() {
        let mut req = base_push_request();
        req.capability = RepositoryCapability::GitOverlay;
        req.force = true;
        let plan = plan_push(&req).unwrap();
        let outcome = build_push_outcome(
            &plan,
            PushExecutionFacts::GitOverlayRefs {
                remote_name: "origin".into(),
                current_thread: Some("main".into()),
                refs_written: vec!["refs/heads/main".into()],
                tracking: Some(GitOverlayPushTracking {
                    remote_name: "origin".into(),
                    configured_remote: Some(GitRemoteConfigured {
                        name: "origin".into(),
                        url: "https://example.com/r.git".into(),
                    }),
                    upstream_branch: Some("main".into()),
                }),
            },
        );
        let text = format_push_outcome_text(&outcome, None);
        assert!(
            text.headline.contains("pushed thread main to origin"),
            "{}",
            text.headline
        );
        assert!(
            text.detail_lines.iter().any(|l| l.starts_with("Force:")),
            "{:?}",
            text.detail_lines
        );
        assert!(
            text.detail_lines
                .iter()
                .any(|l| l.contains("refs/notes/heddle")),
            "{:?}",
            text.detail_lines
        );
        assert!(
            text.detail_lines
                .iter()
                .any(|l| l.contains("tracks origin/main")),
            "{:?}",
            text.detail_lines
        );
    }

    #[test]
    fn format_pull_outcome_text_up_to_date_and_paths() {
        let plan = plan_pull(&base_pull_request()).unwrap();
        let up = build_pull_outcome(
            Some(&plan),
            PullExecutionFacts::Heddle {
                changed: false,
                remote: "origin".into(),
                thread: "main".into(),
                state: None,
                objects: None,
            },
        );
        let text = format_pull_outcome_text(&up, 8);
        assert!(text.headline.contains("already up to date with origin"));

        let git = build_pull_outcome(
            Some(&plan),
            PullExecutionFacts::GitOverlay {
                remote: "origin".into(),
                branch: Some("main".into()),
                old_git_head: None,
                new_git_head: None,
                old_state: None,
                new_state: None,
                changed: true,
                states_created: 1,
                commits_seen: 3,
                materialized_checkout: false,
                changed_paths: vec!["a".into(), "b".into(), "c".into()],
            },
        );
        let text = format_pull_outcome_text(&git, 2);
        assert_eq!(text.headline, "pulled from origin");
        assert!(text.detail_lines.iter().any(|l| l == "Changed paths: 3"));
        assert!(text.detail_lines.iter().any(|l| l == "  - ... 1 more"));
    }

    #[test]
    fn pull_should_materialize_respects_lazy() {
        assert!(pull_should_materialize(true, false));
        assert!(!pull_should_materialize(true, true));
        assert!(!pull_should_materialize(false, false));
        assert!(!pull_should_materialize(false, true));
    }
}
