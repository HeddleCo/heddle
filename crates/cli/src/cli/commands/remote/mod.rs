// SPDX-License-Identifier: Apache-2.0
//! Remote operations (push, pull, remote management).

#[cfg(feature = "client")]
use std::net::SocketAddr;
use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, anyhow};
use gix::{bstr::ByteSlice, refs::transaction::PreviousValue};
use objects::{fs_atomic::write_file_atomic, object::ThreadName};
#[cfg(feature = "client")]
use proto::AuthToken;
use refs::Head;
use repo::{Repository, RepositoryCapability};
use serde::Serialize;

use super::{
    action_line::print_next,
    advice::RecoveryAdvice,
    command_catalog::{ActionFields, ActionTemplate},
    git_overlay_health::{RepositoryVerificationState, build_repository_verification_state},
    snapshot::ensure_current_state,
};
#[cfg(feature = "client")]
use crate::client::{ClientConfig, HostedGrpcClient};
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
pub(crate) use remote_ops::{
    resolve_default_remote_name, resolved_default_remote_name,
};

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
    bridge.push_with_scope_force(&remote_name, scope, force)?;
    let tracking_refresh = refresh_git_tracking_after_overlay_push(repo, &remote_name)?;
    let trust = build_repository_verification_state(repo);
    Ok((remote_name, scope, current_thread, tracking_refresh, trust))
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
    let repo = Repository::open(cli.repo.as_ref().unwrap_or(&std::env::current_dir()?))?;
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

    if repo.capability() == RepositoryCapability::GitOverlay && !repo.hosted_enabled() {
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
        if !all_threads
            && let Some(requested) = thread.as_deref()
        {
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
        let (remote_name, scope, current_thread, tracking_refresh, trust) =
            push_git_overlay_refs(&repo, remote.as_deref(), all_threads, force)?;
        if should_output_json(cli, Some(repo.config())) {
            let output = git_overlay_push_output(
                remote_name,
                scope,
                current_thread,
                tracking_refresh,
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

    let token = user_config.remote_token()?;
    #[cfg(feature = "client")]
    let mut token = token;
    let (target, server_key) =
        resolve_remote_with_key(&repo, remote.as_deref()).map_err(anyhow::Error::msg)?;

    // Fall back to the credential store if no token was provided via env/config.
    #[cfg(feature = "client")]
    let mut credential_proof_key: Option<String> = None;
    #[cfg(not(feature = "client"))]
    let credential_proof_key: Option<String> = None;
    #[cfg(feature = "client")]
    if token.is_none()
        && let Some(ref key) = server_key
        && let Ok(Some(cred)) = heddle_client::credentials::resolve_credential_for_server(key)
    {
        token = Some(AuthToken::new(cred.token, "credential-store"));
        credential_proof_key = cred.private_key_pem;
    }

    let network_client_config = if matches!(target, RemoteTarget::Network { .. }) {
        let mut config = user_config.heddle_client_config(token.clone())?;
        if let Some(ref key) = server_key {
            config = config.with_server_key(key.clone());
        }
        if let Some(ref pem) = credential_proof_key
            && config.auth_proof_key_pem.is_none()
        {
            config = config.with_auth_proof_key_pem(pem.clone());
        }
        Some(config)
    } else {
        None
    };

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
                    client_config: network_client_config
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
            let _ = (addr, repo_path, token, network_client_config);
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
    outcome: crate::bridge::GitResult<()>,
) {
    let json = should_output_json(cli, Some(repo.config()));
    match outcome {
        Ok(()) => {
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
    let git = match gix::discover(repo.root()) {
        Ok(git) => git,
        Err(_) => return Ok(None),
    };
    let Ok(head) = git.head_id() else {
        return Ok(None);
    };
    let head = head.detach();
    let Some(tracking_remote) = resolve_git_tracking_remote_name(repo, remote_name)? else {
        return Ok(None);
    };

    let upstream = git
        .find_reference(format!("refs/heads/{branch}").as_str())
        .ok()
        .and_then(|local| {
            local
                .remote_tracking_ref_name(gix::remote::Direction::Fetch)?
                .ok()
                .map(|name| name.as_ref().as_bstr().to_str_lossy().into_owned())
        })
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
        PreviousValue::Any,
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
    for name in git_remote_names(root)? {
        let git = match gix::discover(root) {
            Ok(git) => git,
            Err(_) => return Ok(None),
        };
        let Some(remote) = git
            .try_find_remote(name.as_bytes().as_bstr())
            .and_then(Result::ok)
        else {
            continue;
        };
        let Some(url) = remote.url(gix::remote::Direction::Push) else {
            continue;
        };
        if remote_urls_match(&url.to_string(), requested) {
            return Ok(Some(name));
        }
    }
    Ok(None)
}

fn git_remote_names(root: &Path) -> Result<Vec<String>> {
    let git = match gix::discover(root) {
        Ok(git) => git,
        Err(_) => return Ok(Vec::new()),
    };
    Ok(git
        .remote_names()
        .into_iter()
        .map(|name| name.to_str_lossy().into_owned())
        .filter(|name| !name.is_empty())
        .collect())
}

fn git_remote_ref_name_is_valid(_root: &Path, name: &str) -> Result<bool> {
    if name.trim().is_empty() {
        return Ok(false);
    }
    let refname = format!("refs/remotes/{name}/HEAD");
    Ok(gix::refs::FullName::try_from(refname.as_str()).is_ok())
}

fn write_git_overlay_branch_upstream(root: &Path, branch: &str, remote: &str) -> Result<()> {
    let config_path = git_overlay_config_path_for_write(root)
        .context("Git-overlay tracking refresh requires a writable Git config")?;
    let contents = fs::read_to_string(&config_path).unwrap_or_default();
    let mut contents = remove_git_config_named_section(&contents, "branch", branch);
    if !contents.ends_with('\n') && !contents.is_empty() {
        contents.push('\n');
    }
    contents.push_str(&format!(
        "[branch \"{}\"]\n\tremote = {}\n\tmerge = refs/heads/{}\n",
        escape_git_config_section(branch),
        quote_git_config_value(remote),
        escape_git_config_value(branch)
    ));
    write_file_atomic(&config_path, contents.as_bytes())?;
    Ok(())
}

fn write_git_overlay_remote(root: &Path, name: &str, url: &str) -> Result<()> {
    let config_path = git_overlay_config_path_for_write(root)
        .context("Git-overlay remote tracking requires a writable Git config")?;
    let contents = fs::read_to_string(&config_path).unwrap_or_default();
    let mut contents = remove_git_config_named_section(&contents, "remote", name);
    if !contents.ends_with('\n') && !contents.is_empty() {
        contents.push('\n');
    }
    contents.push_str(&format!(
        "[remote \"{}\"]\n\turl = {}\n\tfetch = +refs/heads/*:refs/remotes/{}/*\n",
        escape_git_config_section(name),
        quote_git_config_value(url),
        escape_git_config_value(name)
    ));
    write_file_atomic(&config_path, contents.as_bytes())?;
    Ok(())
}

fn git_overlay_config_path_for_write(root: &Path) -> Option<PathBuf> {
    let dot_git = root.join(".git");
    if dot_git.is_dir() {
        return Some(dot_git.join("config"));
    }
    let git_dir = pointed_git_dir(&dot_git)?;
    Some(common_git_dir(&git_dir).unwrap_or(git_dir).join("config"))
}

fn pointed_git_dir(dot_git: &Path) -> Option<PathBuf> {
    if dot_git.is_dir() {
        return Some(dot_git.to_path_buf());
    }
    let contents = fs::read_to_string(dot_git).ok()?;
    let target = contents.trim().strip_prefix("gitdir:")?.trim();
    let path = Path::new(target);
    Some(if path.is_absolute() {
        path.to_path_buf()
    } else {
        dot_git.parent()?.join(path)
    })
}

fn common_git_dir(git_dir: &Path) -> Option<PathBuf> {
    let contents = fs::read_to_string(git_dir.join("commondir")).ok()?;
    let target = contents.trim();
    let path = Path::new(target);
    Some(if path.is_absolute() {
        path.to_path_buf()
    } else {
        git_dir.join(path)
    })
}

fn remove_git_config_named_section(contents: &str, section: &str, subsection_name: &str) -> String {
    let mut rewritten = String::new();
    let mut skipping_section = false;
    for line in contents.lines() {
        if let Some(section_name) = parse_git_config_subsection_name(line, section) {
            skipping_section = section_name == subsection_name;
            if skipping_section {
                continue;
            }
        } else if line.trim_start().starts_with('[') && line.trim_end().ends_with(']') {
            skipping_section = false;
        }
        if !skipping_section {
            rewritten.push_str(line);
            rewritten.push('\n');
        }
    }
    if !rewritten.is_empty() && !rewritten.ends_with("\n\n") {
        rewritten.push('\n');
    }
    rewritten
}

fn parse_git_config_subsection_name(line: &str, section: &str) -> Option<String> {
    let trimmed = line.trim();
    let prefix = format!("[{section} \"");
    let inner = trimmed.strip_prefix(&prefix)?.strip_suffix("\"]")?;
    unescape_git_config_string(inner)
}

fn escape_git_config_section(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

fn escape_git_config_value(value: &str) -> String {
    let mut out = String::new();
    for ch in value.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            '\u{0008}' => out.push_str("\\b"),
            ch => out.push(ch),
        }
    }
    out
}

fn quote_git_config_value(value: &str) -> String {
    format!("\"{}\"", escape_git_config_value(value))
}

fn unescape_git_config_string(value: &str) -> Option<String> {
    let mut out = String::new();
    let mut chars = value.chars();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            out.push(ch);
            continue;
        }
        match chars.next()? {
            '\\' => out.push('\\'),
            '"' => out.push('"'),
            'n' => out.push('\n'),
            't' => out.push('\t'),
            'r' => out.push('\r'),
            'b' => out.push('\u{0008}'),
            escaped => out.push(escaped),
        }
    }
    Some(out)
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

    target_repo.refs().set_thread(&ThreadName::new(track_name), state_id)?;

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
    let repo_path = options
        .repo_path
        .context("network remotes must include a hosted repository path")?;

    let mut client = HostedGrpcClient::connect(options.addr, options.client_config).await?;
    client.auto_rotate_if_needed().await;

    if !should_output_json(options.cli, Some(repo.config())) {
        println!(
            "{} connected to {}",
            style::ok_marker(),
            style::dim(&options.addr.to_string())
        );
    }

    let result = client
        .push(
            repo,
            repo_path,
            *options.state_id,
            options.track_name,
            options.force,
        )
        .await?;

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
struct PushNetworkOptions<'a> {
    addr: SocketAddr,
    repo_path: Option<&'a str>,
    client_config: &'a ClientConfig,
    state_id: &'a objects::object::ChangeId,
    track_name: &'a str,
    force: bool,
    cli: &'a Cli,
}

#[cfg(test)]
mod git_overlay_config_atomic_tests {
    //! Crash-mid-write semantics for the Git-overlay `.git/config`
    //! writers. Both `write_git_overlay_remote` and
    //! `write_git_overlay_branch_upstream` now route through
    //! `objects::fs_atomic::write_file_atomic` — the same tmp-and-rename
    //! primitive `remote add`/`remove` use. The atomicity contract:
    //! after the helper returns, the file is the full new content; a
    //! prior interrupted write that left the file partially-written
    //! must not leak back into the result.
    use super::*;
    use tempfile::TempDir;

    fn init_dot_git(root: &Path) {
        fs::create_dir_all(root.join(".git")).unwrap();
    }

    #[test]
    fn write_git_overlay_remote_recovers_from_partial_prior_write() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();
        init_dot_git(root);
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
        init_dot_git(root);
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
        assert!(
            recovered.contains("merge = refs/heads/main"),
            "{recovered}"
        );
        assert!(
            !recovered.trim_end().ends_with("rem"),
            "partial bytes from prior crash leaked into result: {recovered}"
        );
    }
}
