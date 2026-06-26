// SPDX-License-Identifier: Apache-2.0
//! Remote operations (push, pull, remote management).

#[cfg(feature = "client")]
use std::net::SocketAddr;
use std::path::Path;

use anyhow::{Context, Result, anyhow};
use objects::object::ThreadName;
use refs::Head;
use repo::{Repository, RepositoryCapability};
use serde::Serialize;
use sley::{
    ConfigEdit, ConfigEditPlan, ConfigSectionEntry, FullName, RefPrecondition, RemoteConfigSet,
    Repository as SleyRepository,
};
#[cfg(feature = "client")]
use wire::ProtocolError;

use super::{
    action_line::print_next,
    advice::RecoveryAdvice,
    auto_capture::{AutoCaptureTrigger, auto_capture_command_boundary},
    command_catalog::{ActionFields, ActionTemplate},
    git_overlay_health::{RepositoryVerificationState, build_repository_verification_state},
    snapshot::ensure_current_state,
};
#[cfg(feature = "client")]
use crate::client::HostedGrpcClient;
#[cfg(feature = "client")]
use crate::client::{HostedAuthMode, HostedSession};
#[cfg(feature = "client")]
use crate::remote::Remote;
use crate::{
    bridge::{
        GitBridge,
        git_core::{GitPushScope, set_reference},
    },
    cli::{Cli, should_output_json, style},
    client::LocalSync,
    config::UserConfig,
    remote::{RemoteConfig, RemoteTarget, resolve_remote_with_key},
};

mod remote_ops;

pub use remote_ops::{cmd_pull, cmd_remote};
pub(crate) use remote_ops::{resolve_default_remote_name, resolved_default_remote_name};

#[allow(clippy::type_complexity)]
pub(crate) fn push_git_overlay_refs(
    repo: &Repository,
    remote: Option<&str>,
    all_threads: bool,
    force: bool,
) -> Result<(
    String,
    GitPushScope,
    Option<String>,
    Option<GitOverlayTrackingRefresh>,
    Vec<String>,
    super::git_overlay_health::RepositoryVerificationState,
)> {
    let remote_name = resolve_default_remote_name(repo, remote)?;
    let scope = if all_threads {
        GitPushScope::AllThreads
    } else {
        GitPushScope::CurrentThread
    };
    let current_thread = if matches!(scope, GitPushScope::CurrentThread) {
        match repo.head_ref()? {
            Head::Attached { thread } => Some(thread.to_string()),
            Head::Detached { .. } => None,
        }
    } else {
        None
    };
    let mut bridge = GitBridge::new(repo);
    let refs_written = bridge.push_with_scope_force(&remote_name, scope, force)?;
    let tracking_refresh = refresh_git_tracking_after_overlay_push(repo, &remote_name)?;
    let trust = build_repository_verification_state(repo);
    Ok((
        remote_name,
        scope,
        current_thread,
        tracking_refresh,
        refs_written,
        trust,
    ))
}

#[derive(Debug, Clone)]
pub(crate) struct GitOverlayTrackingRefresh {
    remote_name: String,
    configured_remote: Option<GitOverlayConfiguredRemote>,
    upstream_branch: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct GitOverlayConfiguredRemote {
    name: String,
    url: String,
}

#[derive(Debug, Clone, Serialize)]
struct PushOutput {
    output_kind: &'static str,
    action: &'static str,
    status: &'static str,
    success: bool,
    pushed: bool,
    changed: bool,
    transport: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    remote: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    push_scope: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    ref_scope: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    git_notes_ref: Option<&'static str>,
    /// The full ref names this push actually wrote at the destination —
    /// `refs/heads/<thread>`, `refs/notes/heddle`, `refs/tags/<tag>` —
    /// sorted, empty for a no-op push. Present on the Git-overlay refs
    /// path (`transport: "git"`); omitted on the native Heddle transport,
    /// which writes Heddle thread refs, not Git refs. Verifiable with
    /// `git ls-remote <remote>`.
    #[serde(skip_serializing_if = "Option::is_none")]
    refs_written: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    git_notes_visibility_warning: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    git_tracking_remote: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    git_remote_configured: Option<GitRemoteConfiguredOutput>,
    #[serde(skip_serializing_if = "Option::is_none")]
    git_upstream_configured: Option<GitUpstreamConfiguredOutput>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tags_included: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    force: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    force_discard_warning: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    thread: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    state: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    objects: Option<usize>,
    next_action: Option<String>,
    next_action_template: Option<ActionTemplate>,
    recommended_action: Option<String>,
    recommended_action_template: Option<ActionTemplate>,
    #[allow(dead_code)]
    #[serde(skip_serializing)]
    #[serde(rename = "verification")]
    trust: RepositoryVerificationState,
}

#[derive(Debug, Clone, Serialize)]
struct GitRemoteConfiguredOutput {
    name: String,
    url: String,
}

#[derive(Debug, Clone, Serialize)]
struct GitUpstreamConfiguredOutput {
    branch: String,
    remote: String,
}

/// Execute push command.
///
/// `mirror` is an ad-hoc dual-push escape hatch (heddle#25): after the
/// primary push to the Heddle/git-overlay remote succeeds, also push to
/// the named git-bridge remote. Best-effort — mirror failure surfaces
/// as a warning and does NOT abort the primary push.
pub async fn cmd_push(
    cli: &Cli,
    remote: Option<String>,
    thread: Option<String>,
    state: Option<String>,
    force: bool,
    all_threads: bool,
    mirror: Option<String>,
) -> Result<()> {
    let repo = cli.open_repo()?;
    if remote.is_none() && resolved_default_remote_name(&repo)?.is_none() {
        return Err(anyhow!(RecoveryAdvice::remote_not_configured("push")));
    }
    if let Some(remote_name) = remote.as_deref() {
        ensure_remote_arg_resolves(&repo, remote_name)?;
    }

    // `pre_push` JSON-protocol hook fires before any push work, on every
    // path (git-overlay local target, git-overlay refs push, and native
    // remote). Veto via non-empty `abort` aborts the push before any
    // mutation or remote round-trip.
    let hook_manager = repo::HookManager::new(&repo);
    let hook_ctx = repo::HookContext::new(&repo);
    let pre_push_payload = serde_json::json!({
        "remote": remote.clone().unwrap_or_default(),
    });
    if let Ok(Some(resp)) = hook_manager.run_with_payload(
        repo::Hook::PrePush,
        &hook_ctx,
        &pre_push_payload,
        std::time::Duration::from_secs(5),
    ) && !resp.abort.is_empty()
    {
        return Err(anyhow!(RecoveryAdvice::hook_veto(
            "pre_push", "push", resp.abort
        )));
    }

    let user_config = UserConfig::load_default()?;
    if state.is_none() {
        auto_capture_command_boundary(cli, &repo, &user_config, AutoCaptureTrigger::Push)?;
    }

    let push_uses_hosted_network = push_target_is_hosted_network(&repo, remote.as_deref());

    if repo.capability() == RepositoryCapability::GitOverlay
        && !repo.hosted_enabled()
        && !push_uses_hosted_network
    {
        let default_remote_name = if remote.is_none() {
            resolved_default_remote_name(&repo)?
        } else {
            None
        };
        let remote_arg = remote.as_deref().or(default_remote_name.as_deref());
        if let Some(target_path) = native_heddle_local_push_target(&repo, remote_arg)? {
            let state_id = if let Some(state_str) = state {
                if matches!(state_str.as_str(), "HEAD" | "@") && repo.current_state()?.is_none() {
                    ensure_current_state(
                        &repo,
                        &user_config,
                        Some("Bootstrap git-overlay before push".to_string()),
                    )?;
                }
                repo.resolve_state(&state_str)?.context("State not found")?
            } else {
                ensure_current_state(
                    &repo,
                    &user_config,
                    Some("Bootstrap git-overlay before push".to_string()),
                )?
            };
            let track_name = resolve_default_push_thread(&repo, thread.as_deref())?;
            push_local(&repo, &target_path, &state_id, &track_name, force, cli).await?;
            // Ad-hoc dual-push parity (heddle#25): mirror runs on the
            // local-target overlay path too, best-effort.
            if let Some(mirror_remote) = mirror.as_deref() {
                let mut bridge = GitBridge::new(&repo);
                let outcome = bridge.push(mirror_remote);
                render_mirror_outcome(cli, &repo, mirror_remote, outcome);
            }
            run_post_push_hook(&hook_manager, &hook_ctx, remote.as_deref());
            return Ok(());
        }
        // The git-overlay refs path pushes whatever's attached to HEAD
        // when scope is CurrentThread. If the user named a different
        // thread explicitly (positional or `--thread`) we must NOT
        // silently push the wrong branch — refuse and tell them to
        // switch first or use `--all-threads`.
        if !all_threads && let Some(requested) = thread.as_deref() {
            let attached = match repo.head_ref()? {
                Head::Attached { thread } => Some(thread.to_string()),
                Head::Detached { .. } => None,
            };
            if attached.as_deref() != Some(requested) {
                let attached_label = attached
                    .as_deref()
                    .map(|t| format!("'{t}'"))
                    .unwrap_or_else(|| "detached HEAD".to_string());
                return Err(anyhow!(
                    "git-overlay push targets the attached thread; requested '{requested}' but HEAD is {attached_label}.\nNext: heddle thread switch {requested} && heddle push, or pass --all-threads"
                ));
            }
        }
        let (remote_name, scope, current_thread, tracking_refresh, refs_written, trust) =
            push_git_overlay_refs(&repo, remote.as_deref(), all_threads, force)?;
        if should_output_json(cli, Some(repo.config())) {
            let output = git_overlay_push_output(
                remote_name,
                scope,
                current_thread,
                tracking_refresh,
                refs_written,
                force,
                trust,
            );
            crate::cli::render::write_json_stdout(&output)?;
        } else {
            println!(
                "{} pushed {} to {} ({})",
                style::ok_marker(),
                match scope {
                    GitPushScope::CurrentThread => current_thread
                        .as_deref()
                        .map(|thread| format!("thread {}", style::bold(thread)))
                        .unwrap_or_else(|| "current thread".to_string()),
                    GitPushScope::AllThreads => "all threads".to_string(),
                },
                style::bold(&remote_name),
                match scope {
                    GitPushScope::CurrentThread => "branch + refs/notes/heddle; tags skipped",
                    GitPushScope::AllThreads => "all threads + Git tags + refs/notes/heddle",
                }
            );
            if force {
                println!(
                    "Force: remote refs may be moved back to match local Heddle state; remote commits not reachable from this checkout can be discarded."
                );
            }
            println!(
                "Git interop: published {}; ordinary `git log --all` may show Heddle metadata commits.",
                style::bold("refs/notes/heddle")
            );
            if let Some(refresh) = tracking_refresh.as_ref() {
                if let Some(configured) = &refresh.configured_remote {
                    println!(
                        "Git tracking: configured remote {} -> {} for future fetch/push.",
                        style::bold(&configured.name),
                        style::dim(&configured.url)
                    );
                }
                if let Some(branch) = &refresh.upstream_branch {
                    println!(
                        "Git tracking: branch {} tracks {}/{}.",
                        style::bold(branch),
                        style::bold(&refresh.remote_name),
                        branch
                    );
                }
            }
            println!(
                "Workspace: {}",
                if trust.verified {
                    style::accent("verified")
                } else {
                    style::warn(&trust.status)
                }
            );
            if !trust.recommended_action.is_empty() {
                print_next(&trust.recommended_action);
            }
        }
        // Ad-hoc dual-push parity for the git-overlay branch (heddle#25):
        // `--mirror` fires here too, best-effort, after the primary push.
        if let Some(mirror_remote) = mirror.as_deref() {
            let mut bridge = GitBridge::new(&repo);
            let outcome = bridge.push(mirror_remote);
            render_mirror_outcome(cli, &repo, mirror_remote, outcome);
        }
        run_post_push_hook(&hook_manager, &hook_ctx, remote.as_deref());
        return Ok(());
    }

    preflight_native_remote_transport(&repo, remote.as_deref(), "push")?;

    #[cfg(not(feature = "client"))]
    let token = user_config.remote_token()?;
    #[cfg(feature = "client")]
    let (target, server_key) =
        resolve_remote_with_key(&repo, remote.as_deref()).map_err(anyhow::Error::msg)?;
    #[cfg(not(feature = "client"))]
    let (target, _server_key) =
        resolve_remote_with_key(&repo, remote.as_deref()).map_err(anyhow::Error::msg)?;

    // Prevalidate auth/TLS config (including the credential-store fallback)
    // before any irreversible state mutation below; a rejected security
    // config must leave no partial state behind.
    #[cfg(feature = "client")]
    let network_session = if matches!(target, RemoteTarget::Network { .. }) {
        Some(HostedSession::build(
            &user_config,
            server_key,
            HostedAuthMode::CredentialFallback,
        )?)
    } else {
        None
    };
    // Builds without the `client` feature can't push over the network, but
    // must still fail closed on a bad TLS/auth config before bootstrapping
    // local state — matching the prevalidation the `client` build runs above.
    #[cfg(not(feature = "client"))]
    if matches!(target, RemoteTarget::Network { .. }) {
        user_config.heddle_client_config(token.clone())?;
    }

    let state_id = if let Some(state_str) = state {
        if matches!(state_str.as_str(), "HEAD" | "@") && repo.current_state()?.is_none() {
            ensure_current_state(
                &repo,
                &user_config,
                Some("Bootstrap git-overlay before push".to_string()),
            )?;
        }
        repo.resolve_state(&state_str)?.context("State not found")?
    } else {
        ensure_current_state(
            &repo,
            &user_config,
            Some("Bootstrap git-overlay before push".to_string()),
        )?
    };

    let track_name = resolve_default_push_thread(&repo, thread.as_deref())?;

    match target {
        RemoteTarget::Local(path) => {
            push_local(&repo, &path, &state_id, &track_name, force, cli).await?;
        }
        RemoteTarget::Network { addr, repo_path } => {
            #[cfg(feature = "client")]
            push_network(
                &repo,
                PushNetworkOptions {
                    addr,
                    repo_path: repo_path.as_deref(),
                    remote_arg: remote.as_deref(),
                    session: network_session
                        .as_ref()
                        .context("network client config was not prevalidated")?,
                    state_id: &state_id,
                    track_name: &track_name,
                    force,
                    cli,
                },
            )
            .await?;
            #[cfg(not(feature = "client"))]
            let _ = (addr, repo_path, token);
            #[cfg(not(feature = "client"))]
            anyhow::bail!(RecoveryAdvice::network_feature_unavailable("push"));
        }
    }

    // Ad-hoc dual-push (heddle#25): after the primary push, also push to
    // the named git-bridge mirror. Best-effort — mirror failure does not
    // abort the primary push.
    if let Some(mirror_remote) = mirror.as_deref() {
        let mut bridge = GitBridge::new(&repo);
        let outcome = bridge.push(mirror_remote);
        render_mirror_outcome(cli, &repo, mirror_remote, outcome);
    }

    run_post_push_hook(&hook_manager, &hook_ctx, remote.as_deref());

    Ok(())
}

/// `post_push` JSON-protocol hook. Best-effort; fires after a successful
/// push regardless of which transport path served it (git-overlay local,
/// git-overlay refs, or native). Errors are swallowed so a misbehaving
/// hook never masks a push that already succeeded.
fn run_post_push_hook(
    hook_manager: &repo::HookManager,
    hook_ctx: &repo::HookContext,
    remote: Option<&str>,
) {
    let payload = serde_json::json!({
        "remote": remote.unwrap_or_default(),
    });
    if let Err(err) = hook_manager.run_with_payload(
        repo::Hook::PostPush,
        hook_ctx,
        &payload,
        std::time::Duration::from_secs(5),
    ) {
        tracing::warn!(error = %err, "post_push hook error swallowed");
    }
}

/// Print the outcome of the ad-hoc mirror push (heddle#25). Mirror
/// failure is best-effort: surface as a warning, never bubble up.
/// Matches the JSON and text shapes the main branch shipped.
fn render_mirror_outcome(
    cli: &Cli,
    repo: &Repository,
    mirror_remote: &str,
    outcome: crate::bridge::GitResult<Vec<String>>,
) {
    let json = should_output_json(cli, Some(repo.config()));
    match outcome {
        Ok(_) => {
            if json {
                // Stderr (not stdout): the primary push already wrote
                // the documented single JSON object to stdout. Emitting
                // a second JSON object there would break the
                // `heddle push --output json` parse-as-one-object
                // contract for any caller using `--mirror`.
                let record = serde_json::json!({
                    "mirrored": true,
                    "remote": mirror_remote,
                });
                eprintln!("{}", record);
            } else {
                println!(
                    "{} mirrored to {}",
                    style::ok_marker(),
                    style::bold(mirror_remote)
                );
            }
        }
        Err(err) => {
            if json {
                let record = serde_json::json!({
                    "mirrored": false,
                    "remote": mirror_remote,
                    "error": err.to_string(),
                });
                eprintln!("{}", record);
            } else {
                eprintln!(
                    "{} mirror push to {} failed (primary push still succeeded): {}",
                    style::warn_marker(),
                    style::bold(mirror_remote),
                    err
                );
            }
        }
    }
}

fn git_overlay_push_output(
    remote_name: String,
    scope: GitPushScope,
    current_thread: Option<String>,
    tracking_refresh: Option<GitOverlayTrackingRefresh>,
    refs_written: Vec<String>,
    force: bool,
    trust: RepositoryVerificationState,
) -> PushOutput {
    let action = ActionFields::from_action(&trust.recommended_action);
    let tracking_remote = tracking_refresh
        .as_ref()
        .map(|refresh| refresh.remote_name.clone());
    let configured_remote = tracking_refresh
        .as_ref()
        .and_then(|refresh| refresh.configured_remote.as_ref())
        .map(|remote| GitRemoteConfiguredOutput {
            name: remote.name.clone(),
            url: remote.url.clone(),
        });
    let upstream_configured = tracking_refresh
        .as_ref()
        .and_then(|refresh| refresh.upstream_branch.as_ref())
        .map(|branch| GitUpstreamConfiguredOutput {
            branch: branch.clone(),
            remote: tracking_remote
                .clone()
                .unwrap_or_else(|| "origin".to_string()),
        });
    PushOutput {
        output_kind: "push",
        action: "push",
        status: "pushed",
        success: true,
        pushed: true,
        changed: true,
        transport: "git",
        remote: Some(remote_name),
        push_scope: Some(match scope {
            GitPushScope::CurrentThread => "current_thread",
            GitPushScope::AllThreads => "all_threads",
        }),
        ref_scope: Some(match scope {
            GitPushScope::CurrentThread => "branch_and_heddle_notes",
            GitPushScope::AllThreads => "all_threads_tags_and_heddle_notes",
        }),
        git_notes_ref: Some("refs/notes/heddle"),
        refs_written: Some(refs_written),
        git_notes_visibility_warning: Some(
            "ordinary `git log --all` may show Heddle metadata commits from refs/notes/heddle",
        ),
        git_tracking_remote: tracking_remote,
        git_remote_configured: configured_remote,
        git_upstream_configured: upstream_configured,
        tags_included: Some(matches!(scope, GitPushScope::AllThreads)),
        force: Some(force),
        force_discard_warning: force.then_some(
            "remote refs may be moved back to match local Heddle state; remote commits not reachable from this checkout can be discarded",
        ),
        thread: current_thread,
        state: None,
        objects: None,
        next_action: action.action.clone(),
        next_action_template: action.template.clone(),
        recommended_action: action.action,
        recommended_action_template: action.template,
        trust,
    }
}

fn heddle_push_output(
    state: Option<String>,
    objects: Option<usize>,
    trust: RepositoryVerificationState,
) -> PushOutput {
    let action = ActionFields::from_action(&trust.recommended_action);
    PushOutput {
        output_kind: "push",
        action: "push",
        status: "pushed",
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
        next_action: action.action.clone(),
        next_action_template: action.template.clone(),
        recommended_action: action.action,
        recommended_action_template: action.template,
        trust,
    }
}

fn ensure_remote_arg_resolves(repo: &Repository, remote_arg: &str) -> Result<()> {
    if remote_arg.trim().is_empty()
        || RemoteTarget::parse(remote_arg).is_ok()
        || looks_like_remote_location(remote_arg)
        || looks_like_git_remote_url(remote_arg)
    {
        return Ok(());
    }
    if RemoteConfig::open(repo)
        .map_err(anyhow::Error::msg)?
        .get(remote_arg)
        .is_ok()
    {
        return Ok(());
    }
    if repo.capability() == RepositoryCapability::GitOverlay
        && git_remote_names(repo.root())?
            .iter()
            .any(|name| name == remote_arg)
    {
        return Ok(());
    }
    Err(anyhow!(RecoveryAdvice::remote_not_found(remote_arg)))
}

pub(super) fn push_target_is_hosted_network(repo: &Repository, remote_arg: Option<&str>) -> bool {
    matches!(
        classify_remote_spec(repo, remote_arg),
        Some(RemoteTransportKind::NetworkHeddle)
    )
}

fn native_heddle_local_push_target(
    repo: &Repository,
    remote_arg: Option<&str>,
) -> Result<Option<std::path::PathBuf>> {
    let Some(remote_arg) = remote_arg else {
        return Ok(None);
    };
    let target = match resolve_remote_with_key(repo, Some(remote_arg)) {
        Ok((target, _)) => target,
        Err(_) => match RemoteTarget::parse(remote_arg) {
            Ok(target) => target,
            Err(_) => return Ok(None),
        },
    };
    let RemoteTarget::Local(path) = target else {
        return Ok(None);
    };
    if classify_remote_spec(repo, Some(path.to_string_lossy().as_ref())).is_some_and(|kind| {
        matches!(
            kind,
            RemoteTransportKind::LocalGit | RemoteTransportKind::GitUrl
        )
    }) {
        return Ok(None);
    }
    let Ok(target_repo) = Repository::open(&path) else {
        return Ok(None);
    };
    if target_repo.capability() == RepositoryCapability::GitOverlay {
        return Ok(None);
    }
    Ok(Some(target_repo.root().to_path_buf()))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RemoteTransportKind {
    LocalHeddle,
    LocalGit,
    LocalUnknown,
    NetworkHeddle,
    GitUrl,
    Unknown,
}

pub(super) fn preflight_native_remote_transport(
    repo: &Repository,
    remote_arg: Option<&str>,
    action: &str,
) -> Result<()> {
    if repo.capability() == RepositoryCapability::GitOverlay {
        return Ok(());
    }
    match classify_remote_spec(repo, remote_arg) {
        Some(RemoteTransportKind::LocalGit | RemoteTransportKind::GitUrl) => Err(anyhow!(
            RecoveryAdvice::remote_transport_mismatch(action, remote_arg.unwrap_or("<default>"))
        )),
        _ => Ok(()),
    }
}

pub(super) fn classify_remote_spec(
    repo: &Repository,
    remote_arg: Option<&str>,
) -> Option<RemoteTransportKind> {
    let spec = remote_spec_for_preflight(repo, remote_arg)?;
    if let Ok(target) = RemoteTarget::parse(&spec) {
        return Some(match target {
            RemoteTarget::Local(path) => {
                if let Ok(target_repo) = Repository::open(&path) {
                    if target_repo.capability() == RepositoryCapability::GitOverlay {
                        RemoteTransportKind::LocalGit
                    } else {
                        RemoteTransportKind::LocalHeddle
                    }
                } else if is_local_git_repository(&path) {
                    RemoteTransportKind::LocalGit
                } else {
                    RemoteTransportKind::LocalUnknown
                }
            }
            RemoteTarget::Network { .. } => RemoteTransportKind::NetworkHeddle,
        });
    }
    if looks_like_git_remote_url(&spec) {
        return Some(RemoteTransportKind::GitUrl);
    }
    Some(RemoteTransportKind::Unknown)
}

fn remote_spec_for_preflight(repo: &Repository, remote_arg: Option<&str>) -> Option<String> {
    let cfg = RemoteConfig::open(repo).ok();
    match remote_arg {
        Some(arg) if RemoteTarget::parse(arg).is_ok() || looks_like_remote_location(arg) => {
            Some(arg.to_string())
        }
        Some(arg) => cfg
            .as_ref()
            .and_then(|cfg| cfg.get(arg).ok())
            .map(|remote| remote.url)
            .or_else(|| Some(arg.to_string())),
        None => {
            let cfg = cfg?;
            cfg.default_name()
                .and_then(|name| cfg.get(name).ok())
                .map(|remote| remote.url)
        }
    }
}

pub(super) fn is_local_git_repository(path: &Path) -> bool {
    if path.join(".git").exists() {
        return true;
    }
    path.join("HEAD").is_file() && path.join("objects").is_dir() && path.join("refs").is_dir()
}

fn looks_like_git_remote_url(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    lower.starts_with("http://")
        || lower.starts_with("https://")
        || lower.starts_with("ssh://")
        || lower.starts_with("git://")
        || lower.ends_with(".git")
        || (value.contains('@') && value.contains(':'))
}

fn refresh_git_tracking_after_overlay_push(
    repo: &Repository,
    remote_name: &str,
) -> Result<Option<GitOverlayTrackingRefresh>> {
    if repo.capability() != RepositoryCapability::GitOverlay || !repo.root().join(".git").exists() {
        return Ok(None);
    }

    let branch = repo.git_overlay_current_branch()?.unwrap_or_default();
    if branch.is_empty() {
        return Ok(None);
    }
    let git = match SleyRepository::discover(repo.root()) {
        Ok(git) => git,
        Err(_) => return Ok(None),
    };
    let Some(head) = git.head().ok().and_then(|head| head.oid) else {
        return Ok(None);
    };
    let Some(tracking_remote) = resolve_git_tracking_remote_name(repo, remote_name)? else {
        return Ok(None);
    };

    let upstream = git_branch_upstream_from_config(&git, &branch)
        .and_then(|name| name.strip_prefix("refs/remotes/").map(str::to_string))
        .unwrap_or_else(|| format!("{}/{branch}", tracking_remote.name));

    let expected_prefix = format!("{}/", tracking_remote.name);
    if !upstream.starts_with(&expected_prefix) {
        return Ok(if tracking_remote.configured_remote.is_some() {
            Some(GitOverlayTrackingRefresh {
                remote_name: tracking_remote.name,
                configured_remote: tracking_remote.configured_remote,
                upstream_branch: None,
            })
        } else {
            None
        });
    }

    let full_ref = format!("refs/remotes/{upstream}");
    if let Err(error) = set_reference(
        &git,
        &full_ref,
        head,
        RefPrecondition::Any,
        &format!("heddle: push to {remote_name}"),
    ) {
        return Err(anyhow!(
            RecoveryAdvice::git_overlay_tracking_refresh_failed(
                remote_name,
                &full_ref,
                Some(error.to_string()),
            )
        ));
    }

    write_git_overlay_branch_upstream(repo.root(), &branch, &tracking_remote.name)?;

    Ok(Some(GitOverlayTrackingRefresh {
        remote_name: tracking_remote.name,
        configured_remote: tracking_remote.configured_remote,
        upstream_branch: Some(branch),
    }))
}

#[derive(Debug, Clone)]
struct GitTrackingRemoteResolution {
    name: String,
    configured_remote: Option<GitOverlayConfiguredRemote>,
}

fn resolve_git_tracking_remote_name(
    repo: &Repository,
    requested: &str,
) -> Result<Option<GitTrackingRemoteResolution>> {
    if let Some(name) = git_remote_name_for_url(repo.root(), requested)? {
        return Ok(Some(GitTrackingRemoteResolution {
            name,
            configured_remote: None,
        }));
    }
    if !looks_like_remote_location(requested)
        && git_remote_ref_name_is_valid(repo.root(), requested)?
    {
        return Ok(Some(GitTrackingRemoteResolution {
            name: requested.to_string(),
            configured_remote: None,
        }));
    }

    let remotes = git_remote_names(repo.root())?;
    if remotes.is_empty() && !requested.trim().is_empty() {
        write_git_overlay_remote(repo.root(), "origin", requested)
            .context("failed to configure Git remote for tracking")?;
        return Ok(Some(GitTrackingRemoteResolution {
            name: "origin".to_string(),
            configured_remote: Some(GitOverlayConfiguredRemote {
                name: "origin".to_string(),
                url: requested.to_string(),
            }),
        }));
    }
    // Only fall back to a sole configured remote when the requested
    // argument is not itself a remote-location shape. If the user
    // pushed to an explicit URL/path that did not match any
    // configured remote (otherwise `git_remote_name_for_url` would
    // have caught it above), silently retargeting the unrelated
    // sole remote (e.g. `origin`) would corrupt its tracking refs.
    if remotes.len() == 1 && !looks_like_remote_location(requested) {
        return Ok(Some(GitTrackingRemoteResolution {
            name: remotes[0].clone(),
            configured_remote: None,
        }));
    }
    if looks_like_remote_location(requested) {
        // Explicit URL/path that does not match any configured
        // remote — skip the tracking refresh rather than guessing.
        return Ok(None);
    }
    Ok(Some(GitTrackingRemoteResolution {
        name: requested.to_string(),
        configured_remote: None,
    }))
}

fn git_remote_name_for_url(root: &Path, requested: &str) -> Result<Option<String>> {
    let git = match SleyRepository::discover(root) {
        Ok(git) => git,
        Err(_) => return Ok(None),
    };
    for name in git_remote_names(root)? {
        let Some(url) = git_remote_push_url(&git, &name)? else {
            continue;
        };
        if remote_urls_match(&url, requested) {
            return Ok(Some(name));
        }
    }
    Ok(None)
}

fn git_remote_names(root: &Path) -> Result<Vec<String>> {
    let git = match SleyRepository::discover(root) {
        Ok(git) => git,
        Err(_) => return Ok(Vec::new()),
    };
    Ok(git
        .remote_names()?
        .into_iter()
        .filter(|name| !name.is_empty())
        .collect())
}

fn git_remote_ref_name_is_valid(_root: &Path, name: &str) -> Result<bool> {
    if name.trim().is_empty() {
        return Ok(false);
    }
    let refname = format!("refs/remotes/{name}/HEAD");
    Ok(FullName::try_from(refname.as_str()).is_ok())
}

fn git_branch_upstream_from_config(git: &SleyRepository, branch: &str) -> Option<String> {
    let config = git.config_snapshot().ok()?;
    let remote = config.get("branch", Some(branch), "remote")?;
    let merge = config.get("branch", Some(branch), "merge")?;
    let branch_name = merge.strip_prefix("refs/heads/")?;
    Some(format!("refs/remotes/{remote}/{branch_name}"))
}

fn git_remote_push_url(git: &SleyRepository, remote: &str) -> Result<Option<String>> {
    let config = git.config_snapshot()?;
    Ok(config
        .get("remote", Some(remote), "pushurl")
        .or_else(|| config.get("remote", Some(remote), "url"))
        .map(str::to_string))
}

fn write_git_overlay_branch_upstream(root: &Path, branch: &str, remote: &str) -> Result<()> {
    let git = SleyRepository::discover(root).map_err(anyhow::Error::msg)?;
    let plan = ConfigEditPlan::new(git.common_dir().join("config"))
        .with_operation(ConfigEdit::replace_section(
            "branch",
            Some(branch.to_string()),
            vec![
                ConfigSectionEntry::new("remote", remote),
                ConfigSectionEntry::new("merge", format!("refs/heads/{branch}")),
            ],
        ))
        .with_fsync(true);
    git.apply_config_edit_plan(plan)
        .map_err(anyhow::Error::msg)?;
    Ok(())
}

fn write_git_overlay_remote(root: &Path, name: &str, url: &str) -> Result<()> {
    let git = SleyRepository::discover(root).map_err(anyhow::Error::msg)?;
    let remote = RemoteConfigSet::new(name)
        .with_url(url)
        .with_fetch_refspec(format!("+refs/heads/*:refs/remotes/{name}/*"));
    let plan = ConfigEditPlan::new(git.common_dir().join("config"))
        .with_operation(ConfigEdit::replace_section(
            "remote",
            Some(remote.name),
            remote.entries,
        ))
        .with_fsync(true);
    git.apply_config_edit_plan(plan)
        .map_err(anyhow::Error::msg)?;
    Ok(())
}

fn remote_urls_match(left: &str, right: &str) -> bool {
    if left == right {
        return true;
    }
    let left_path = Path::new(left);
    let right_path = Path::new(right);
    match (left_path.canonicalize(), right_path.canonicalize()) {
        (Ok(left), Ok(right)) => left == right,
        _ => false,
    }
}

fn looks_like_remote_location(value: &str) -> bool {
    value.starts_with('/')
        || value.starts_with("./")
        || value.starts_with("../")
        || value.contains("://")
        || value.contains('\\')
}

fn resolve_default_push_thread(repo: &Repository, requested: Option<&str>) -> Result<String> {
    if let Some(requested) = requested {
        return Ok(requested.to_string());
    }

    match repo.head_ref()? {
        Head::Attached { thread } => Ok(thread.to_string()),
        Head::Detached { .. } => Ok("main".to_string()),
    }
}

async fn push_local(
    repo: &Repository,
    target_path: &std::path::Path,
    state_id: &objects::object::ChangeId,
    track_name: &str,
    _force: bool,
    cli: &Cli,
) -> Result<()> {
    if !should_output_json(cli, Some(repo.config())) {
        println!(
            "{} pushing to {}",
            style::working_marker(),
            style::dim(&format!("file://{}", target_path.display()))
        );
    }

    let target_repo = Repository::open(target_path)?;

    let sync = LocalSync::open(repo.root())?;
    let objects_copied = sync.fetch_state(&target_repo, state_id)?;

    target_repo
        .refs()
        .set_thread(&ThreadName::new(track_name), state_id)?;

    if should_output_json(cli, Some(repo.config())) {
        let trust = build_repository_verification_state(repo);
        let output = heddle_push_output(Some(state_id.to_string()), Some(objects_copied), trust);
        crate::cli::render::write_json_stdout(&output)?;
    } else {
        println!(
            "{} pushed {} to {} ({})",
            style::ok_marker(),
            style::change_id(&state_id.short().to_string()),
            style::bold(track_name),
            style::count(objects_copied, "object")
        );
    }

    Ok(())
}

#[cfg(feature = "client")]
async fn push_network(repo: &Repository, options: PushNetworkOptions<'_>) -> Result<()> {
    let mut client = options.session.connect(options.addr).await?;

    if !should_output_json(options.cli, Some(repo.config())) {
        println!(
            "{} connected to {}",
            style::ok_marker(),
            style::dim(&options.addr.to_string())
        );
    }

    let repo_path = match options.repo_path {
        Some(repo_path) => repo_path.to_string(),
        None => auto_provision_hosted_repo(repo, &mut client, &options).await?,
    };

    let result = if repo.capability() == RepositoryCapability::GitOverlay {
        client
            .push_git_overlay_checkpoint(
                repo,
                &repo_path,
                *options.state_id,
                options.track_name,
                options.force,
            )
            .await?
    } else {
        client
            .push(
                repo,
                &repo_path,
                *options.state_id,
                options.track_name,
                options.force,
            )
            .await?
    };

    if result.success {
        if should_output_json(options.cli, Some(repo.config())) {
            let trust = build_repository_verification_state(repo);
            let output = heddle_push_output(result.new_state.map(|s| s.to_string()), None, trust);
            crate::cli::render::write_json_stdout(&output)?;
        } else {
            println!(
                "{} pushed to {}",
                style::ok_marker(),
                style::bold(options.track_name)
            );
            if let Some(new_state) = result.new_state {
                println!(
                    "{}",
                    style::field("remote state", &style::change_id(&new_state.to_string()))
                );
            }
        }
    } else {
        let err = result.error.unwrap_or_else(|| "Unknown error".to_string());
        return Err(anyhow::anyhow!(RecoveryAdvice::remote_push_failed(
            options.track_name,
            &err
        )));
    }

    Ok(())
}

#[cfg(feature = "client")]
async fn auto_provision_hosted_repo(
    repo: &Repository,
    client: &mut HostedGrpcClient,
    options: &PushNetworkOptions<'_>,
) -> Result<String> {
    let namespace = client.get_current_user_namespace().await?;
    let slug = default_spool_slug_from_repo_root(repo.root())?;
    let derived_full_path = format!("{}/{}", namespace.full_path, slug);
    let provisioned_repo = match client.create_repository(&namespace.full_path, &slug).await {
        Ok(created) => AutoProvisionedHostedRepo::Created(created.full_path),
        Err(err) if auto_provision_create_already_exists(&err) => {
            AutoProvisionedHostedRepo::Existing(derived_full_path)
        }
        Err(err) => {
            return Err(anyhow!(RecoveryAdvice::remote_push_failed(
                options.track_name,
                &auto_provision_create_error_message(&slug, &err),
            )));
        }
    };

    let configured_remote = persist_auto_provisioned_remote(
        repo,
        options.remote_arg,
        options.addr,
        provisioned_repo.full_path(),
    )?;

    if !should_output_json(options.cli, Some(repo.config())) {
        let display_full_path =
            hosted_spool_display_path(&namespace, &slug, provisioned_repo.full_path());
        println!(
            "{} {} hosted spool {}",
            style::ok_marker(),
            provisioned_repo.status_verb(),
            style::bold(&display_full_path)
        );
        if let Some(remote_name) = configured_remote {
            println!(
                "{}",
                style::field(
                    "remote",
                    &format!(
                        "{} -> {}",
                        style::bold(&remote_name),
                        style::dim(&format!("heddle://{}/{}", options.addr, display_full_path))
                    )
                )
            );
        } else {
            print_next(&format!(
                "heddle remote add origin heddle://{}/{}",
                options.addr, display_full_path
            ));
        }
    }

    Ok(provisioned_repo.into_full_path())
}

#[cfg(feature = "client")]
enum AutoProvisionedHostedRepo {
    Created(String),
    Existing(String),
}

#[cfg(feature = "client")]
impl AutoProvisionedHostedRepo {
    fn full_path(&self) -> &str {
        match self {
            Self::Created(full_path) | Self::Existing(full_path) => full_path,
        }
    }

    fn into_full_path(self) -> String {
        match self {
            Self::Created(full_path) | Self::Existing(full_path) => full_path,
        }
    }

    fn status_verb(&self) -> &'static str {
        match self {
            Self::Created(_) => "created",
            Self::Existing(_) => "using",
        }
    }
}

#[cfg(feature = "client")]
fn auto_provision_create_already_exists(err: &ProtocolError) -> bool {
    match err {
        ProtocolError::AlreadyExists(_) => true,
        ProtocolError::Remote(message) => message_indicates_already_exists(message),
        _ => false,
    }
}

#[cfg(feature = "client")]
fn message_indicates_already_exists(message: &str) -> bool {
    message.to_ascii_lowercase().contains("already exists")
}

#[cfg(feature = "client")]
fn auto_provision_create_error_message(slug: &str, err: &ProtocolError) -> String {
    let error = match err {
        ProtocolError::Remote(_) | ProtocolError::LockError(_) => err.client_message(),
        _ => redact_internal_hosted_paths(&err.to_string()),
    };
    format!(
        "could not create hosted spool '{slug}': {error}. Pass a full hosted remote path or choose another local folder name"
    )
}

#[cfg(feature = "client")]
fn hosted_spool_display_path(
    namespace: &wire::HostedNamespaceInfo,
    slug: &str,
    full_path: &str,
) -> String {
    if hosted_path_contains_internal_user_namespace(full_path) && !namespace.slug.is_empty() {
        format!("{}/{}", namespace.slug, slug)
    } else {
        full_path.to_string()
    }
}

#[cfg(feature = "client")]
fn redact_internal_hosted_paths(message: &str) -> String {
    message
        .split_whitespace()
        .map(|part| {
            if hosted_path_contains_internal_user_namespace(part) {
                "[user namespace]"
            } else {
                part
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(feature = "client")]
fn hosted_path_contains_internal_user_namespace(value: &str) -> bool {
    value.contains("__users/")
}

#[cfg(feature = "client")]
fn persist_auto_provisioned_remote(
    repo: &Repository,
    remote_arg: Option<&str>,
    addr: SocketAddr,
    full_path: &str,
) -> Result<Option<String>> {
    let Some(remote_name) = auto_provision_remote_name(repo, remote_arg)? else {
        return Ok(None);
    };
    let mut cfg = RemoteConfig::open(repo).map_err(anyhow::Error::msg)?;
    cfg.add(
        &remote_name,
        Remote {
            url: format!("heddle://{addr}/{full_path}"),
        },
    )
    .map_err(anyhow::Error::msg)?;
    Ok(Some(remote_name))
}

#[cfg(feature = "client")]
fn auto_provision_remote_name(
    repo: &Repository,
    remote_arg: Option<&str>,
) -> Result<Option<String>> {
    let cfg = RemoteConfig::open(repo).map_err(anyhow::Error::msg)?;
    match remote_arg {
        Some(arg) if cfg.get(arg).is_ok() => Ok(Some(arg.to_string())),
        Some(arg) => {
            let is_direct_network_without_path = matches!(
                RemoteTarget::parse(arg),
                Ok(RemoteTarget::Network {
                    repo_path: None,
                    ..
                })
            );
            if is_direct_network_without_path && cfg.default_name().is_none() {
                Ok(Some("origin".to_string()))
            } else {
                Ok(None)
            }
        }
        None => Ok(Some(
            cfg.default_name()
                .map(str::to_string)
                .unwrap_or_else(|| "origin".to_string()),
        )),
    }
}

#[cfg(feature = "client")]
fn default_spool_slug_from_repo_root(root: &Path) -> Result<String> {
    let name = root
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    let slug = spool_slug_from_local_name(name);
    if slug.is_empty() {
        return Err(anyhow!(
            "could not derive a hosted spool name from {}; rename the directory or pass a full hosted remote path",
            root.display()
        ));
    }
    Ok(slug)
}

#[cfg(any(feature = "client", test))]
fn spool_slug_from_local_name(name: &str) -> String {
    let mut slug = String::new();
    let mut last_was_separator = false;
    for ch in name.chars().flat_map(char::to_lowercase) {
        if ch.is_ascii_lowercase() || ch.is_ascii_digit() {
            slug.push(ch);
            last_was_separator = false;
        } else if !slug.is_empty() && !last_was_separator {
            slug.push('-');
            last_was_separator = true;
        }
    }
    while slug.ends_with('-') {
        slug.pop();
    }
    slug
}

#[cfg(feature = "client")]
struct PushNetworkOptions<'a> {
    addr: SocketAddr,
    repo_path: Option<&'a str>,
    remote_arg: Option<&'a str>,
    session: &'a HostedSession,
    state_id: &'a objects::object::ChangeId,
    track_name: &'a str,
    force: bool,
    cli: &'a Cli,
}

#[cfg(test)]
mod git_overlay_config_atomic_tests {
    //! Crash-mid-write semantics for the Git-overlay `.git/config` writers.
    //! Both helpers route through Sley's config editor, so section parsing,
    //! quoting, locking, and atomic replacement stay Git-parity tested.
    use std::fs;

    use tempfile::TempDir;

    use super::*;

    fn init_git_repo(root: &Path) {
        SleyRepository::init(root).unwrap();
    }

    #[test]
    fn spool_slug_from_local_name_normalizes_folder_names() {
        assert_eq!(spool_slug_from_local_name("My Cool Repo"), "my-cool-repo");
        assert_eq!(spool_slug_from_local_name("Heddle_CLI.v2"), "heddle-cli-v2");
        assert_eq!(spool_slug_from_local_name("---"), "");
    }

    #[cfg(feature = "client")]
    #[test]
    fn auto_provision_remote_name_uses_existing_or_origin_remote() {
        let temp = TempDir::new().unwrap();
        let repo = Repository::init_default(temp.path()).unwrap();

        assert_eq!(
            auto_provision_remote_name(&repo, None).unwrap().as_deref(),
            Some("origin")
        );
        assert_eq!(
            auto_provision_remote_name(&repo, Some("127.0.0.1:8421"))
                .unwrap()
                .as_deref(),
            Some("origin")
        );

        let mut cfg = RemoteConfig::open(&repo).unwrap();
        cfg.add(
            "weft",
            Remote {
                url: "heddle://127.0.0.1:8421".to_string(),
            },
        )
        .unwrap();

        assert_eq!(
            auto_provision_remote_name(&repo, Some("weft"))
                .unwrap()
                .as_deref(),
            Some("weft")
        );
        assert_eq!(
            auto_provision_remote_name(&repo, Some("127.0.0.1:8421"))
                .unwrap()
                .as_deref(),
            None
        );
    }

    #[cfg(feature = "client")]
    #[test]
    fn auto_provision_reuses_create_repository_already_exists() {
        let typed_already_exists = ProtocolError::AlreadyExists("luke/demo-repo".to_string());
        assert!(auto_provision_create_already_exists(&typed_already_exists));

        let already_exists = ProtocolError::Remote("repository already exists".to_string());
        assert!(auto_provision_create_already_exists(&already_exists));

        let validation = ProtocolError::InvalidState("repository already exists".to_string());
        assert!(!auto_provision_create_already_exists(&validation));

        let permission = ProtocolError::AuthorizationFailed("missing grant".to_string());
        assert!(!auto_provision_create_already_exists(&permission));
    }

    #[cfg(feature = "client")]
    #[test]
    fn auto_provision_hides_internal_user_namespace_paths_in_cli_text() {
        let namespace = wire::HostedNamespaceInfo {
            namespace_id: "user-1".to_string(),
            kind: "user".to_string(),
            slug: "alice".to_string(),
            parent_id: None,
            display_name: None,
            full_path: "__users/user-1".to_string(),
        };

        assert_eq!(
            hosted_spool_display_path(&namespace, "demo-repo", "__users/user-1/demo-repo"),
            "alice/demo-repo"
        );

        let validation =
            ProtocolError::InvalidState("namespace __users/user-1 rejected".to_string());
        let validation_message = auto_provision_create_error_message("demo-repo", &validation);
        assert!(
            !validation_message.contains("__users/"),
            "{validation_message}"
        );
        assert!(validation_message.contains("demo-repo"));

        let remote =
            ProtocolError::Remote("repository __users/user-1/demo-repo failed".to_string());
        let remote_message = auto_provision_create_error_message("demo-repo", &remote);
        assert!(!remote_message.contains("__users/"), "{remote_message}");
        assert!(remote_message.contains("internal server error"));
    }

    #[test]
    fn write_git_overlay_remote_recovers_from_partial_prior_write() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();
        init_git_repo(root);
        let config = root.join(".git").join("config");

        // Establish a clean baseline so we know what "previous full
        // content" looks like.
        write_git_overlay_remote(root, "origin", "https://example.com/a.git").unwrap();
        assert!(
            fs::read_to_string(&config)
                .unwrap()
                .contains("https://example.com/a.git")
        );

        // Simulate a crash mid-write by truncating the file. A
        // non-atomic writer using `fs::write` could leave the config
        // in exactly this shape if the process died between the
        // `open(O_TRUNC)` and the final `write_all`.
        fs::write(&config, "[remote \"origin\"]\n\turl = htt").unwrap();

        // Re-invoke the helper. The atomic contract: the resulting
        // file is the full, well-formed new content — never a partial.
        write_git_overlay_remote(root, "origin", "https://example.com/b.git").unwrap();
        let recovered = fs::read_to_string(&config).unwrap();
        assert!(
            recovered.contains("[remote \"origin\"]"),
            "section header missing: {recovered}"
        );
        assert!(
            recovered.contains("https://example.com/b.git"),
            "new url missing: {recovered}"
        );
        assert!(
            recovered.contains("fetch = +refs/heads/*:refs/remotes/origin/*"),
            "fetch line missing: {recovered}"
        );
        assert!(
            !recovered.contains("url = htt\n") && !recovered.trim_end().ends_with("url = htt"),
            "partial bytes from prior crash leaked into result: {recovered}"
        );
    }

    #[test]
    fn write_git_overlay_branch_upstream_recovers_from_partial_prior_write() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();
        init_git_repo(root);
        let config = root.join(".git").join("config");

        // Baseline.
        write_git_overlay_branch_upstream(root, "main", "origin").unwrap();
        assert!(
            fs::read_to_string(&config)
                .unwrap()
                .contains("[branch \"main\"]")
        );

        // Crash-mid-write simulation: leave the file truncated mid-key.
        fs::write(&config, "[branch \"main\"]\n\trem").unwrap();

        // The atomic helper produces a fully-formed section regardless
        // of the prior partial state.
        write_git_overlay_branch_upstream(root, "main", "upstream").unwrap();
        let recovered = fs::read_to_string(&config).unwrap();
        assert!(recovered.contains("[branch \"main\"]"), "{recovered}");
        assert!(recovered.contains("upstream"), "{recovered}");
        assert!(recovered.contains("merge = refs/heads/main"), "{recovered}");
        assert!(
            !recovered.trim_end().ends_with("rem"),
            "partial bytes from prior crash leaked into result: {recovered}"
        );
    }
}
