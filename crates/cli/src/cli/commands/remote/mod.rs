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
#[cfg(feature = "client")]
use proto::AuthToken;
use refs::Head;
use repo::{Repository, RepositoryCapability};

use super::{
    advice::RecoveryAdvice,
    command_catalog::{recommended_action_argv, recommended_action_template},
    git_overlay_health::{RepositoryVerificationState, build_repository_verification_state},
    snapshot::ensure_current_state,
};
#[cfg(feature = "client")]
use crate::client::HostedGrpcClient;
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

pub(crate) fn push_git_overlay_refs(
    repo: &Repository,
    remote: Option<&str>,
    all_threads: bool,
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
            Head::Attached { thread } => Some(thread),
            Head::Detached { .. } => None,
        }
    } else {
        None
    };
    let mut bridge = GitBridge::new(repo);
    bridge.push_with_scope(&remote_name, scope)?;
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

/// Execute push command.
pub async fn cmd_push(
    cli: &Cli,
    remote: Option<String>,
    thread: Option<String>,
    state: Option<String>,
    force: bool,
    all_threads: bool,
) -> Result<()> {
    let repo = Repository::open(cli.repo.as_ref().unwrap_or(&std::env::current_dir()?))?;
    if remote.is_none() && resolved_default_remote_name(&repo)?.is_none() {
        return Err(anyhow!(remote_not_configured_advice("push")));
    }
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
                        &UserConfig::load_default().unwrap_or_default(),
                        Some("Bootstrap git-overlay before push".to_string()),
                    )?;
                }
                repo.resolve_state(&state_str)?.context("State not found")?
            } else {
                ensure_current_state(
                    &repo,
                    &UserConfig::load_default().unwrap_or_default(),
                    Some("Bootstrap git-overlay before push".to_string()),
                )?
            };
            let track_name = resolve_default_push_thread(&repo, thread.as_deref())?;
            push_local(&repo, &target_path, &state_id, &track_name, force, cli).await?;
            return Ok(());
        }
        let (remote_name, scope, current_thread, tracking_refresh, trust) =
            push_git_overlay_refs(&repo, remote.as_deref(), all_threads)?;
        if should_output_json(cli, Some(repo.config())) {
            let action = action_value(&trust);
            let configured_remote = tracking_refresh
                .as_ref()
                .and_then(|refresh| refresh.configured_remote.as_ref());
            println!(
                "{}",
                serde_json::json!({
                    "output_kind": "push",
                    "status": "pushed",
                    "success": true,
                    "pushed": true,
                    "changed": true,
                    "transport": "git",
                    "remote": remote_name,
                    "push_scope": match scope {
                        GitPushScope::CurrentThread => "current_thread",
                        GitPushScope::AllThreads => "all_threads",
                    },
                    "ref_scope": match scope {
                        GitPushScope::CurrentThread => "branch_and_heddle_notes",
                        GitPushScope::AllThreads => "all_threads_tags_and_heddle_notes",
                    },
                    "git_notes_ref": "refs/notes/heddle",
                    "git_notes_visibility_warning": "ordinary `git log --all` may show Heddle metadata commits from refs/notes/heddle",
                    "git_tracking_remote": tracking_refresh.as_ref().map(|refresh| refresh.remote_name.as_str()),
                    "git_remote_configured": configured_remote.map(|remote| serde_json::json!({
                        "name": remote.name.as_str(),
                        "url": remote.url.as_str(),
                    })),
                    "git_upstream_configured": tracking_refresh
                        .as_ref()
                        .and_then(|refresh| refresh.upstream_branch.as_ref())
                        .map(|branch| serde_json::json!({
                            "branch": branch,
                            "remote": tracking_refresh.as_ref().map(|refresh| refresh.remote_name.as_str()).unwrap_or("origin"),
                        })),
                    "tags_included": matches!(scope, GitPushScope::AllThreads),
                    "thread": current_thread,
                    "next_action": action,
                    "next_action_argv": action_argv_value(&trust),
                    "next_action_template": action_template_value(&trust),
                    "recommended_action": action_value(&trust),
                    "recommended_action_argv": action_argv_value(&trust),
                    "recommended_action_template": action_template_value(&trust),
                    "verification": trust,
                })
            );
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
                println!("Next: {}", style::bold(&trust.recommended_action));
            }
        }
        return Ok(());
    }

    preflight_native_remote_transport(&repo, remote.as_deref(), "push")?;

    // `pre_push` JSON-protocol hook. Veto via non-empty
    // `abort` aborts the push before any remote round-trip.
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

    let state_id = if let Some(state_str) = state {
        if matches!(state_str.as_str(), "HEAD" | "@") && repo.current_state()?.is_none() {
            ensure_current_state(
                &repo,
                &UserConfig::load_default().unwrap_or_default(),
                Some("Bootstrap git-overlay before push".to_string()),
            )?;
        }
        repo.resolve_state(&state_str)?.context("State not found")?
    } else {
        ensure_current_state(
            &repo,
            &UserConfig::load_default().unwrap_or_default(),
            Some("Bootstrap git-overlay before push".to_string()),
        )?
    };

    let user_config = UserConfig::load_default().unwrap_or_default();
    #[cfg(feature = "client")]
    let mut token = user_config.remote_token();
    #[cfg(not(feature = "client"))]
    let token = user_config.remote_token();
    #[cfg(feature = "client")]
    let (target, server_key) =
        resolve_remote_with_key(&repo, remote.as_deref()).map_err(anyhow::Error::msg)?;
    #[cfg(not(feature = "client"))]
    let (target, _server_key) =
        resolve_remote_with_key(&repo, remote.as_deref()).map_err(anyhow::Error::msg)?;

    // Fall back to the credential store if no token was provided via env/config.
    #[cfg(feature = "client")]
    let mut credential_proof_key: Option<String> = None;
    #[cfg(feature = "client")]
    if token.is_none()
        && let Some(ref key) = server_key
        && let Ok(Some(cred)) = heddle_client::credentials::resolve_credential_for_server(key)
    {
        token = Some(AuthToken::new(cred.token, "credential-store"));
        credential_proof_key = cred.private_key_pem;
    }

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
                    user_config: &user_config,
                    token,
                    server_key,
                    credential_proof_key,
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

    // `post_push` JSON-protocol hook. Best-effort; fires after
    // a successful push.
    let post_push_payload = serde_json::json!({
        "remote": remote.unwrap_or_default(),
    });
    if let Err(err) = hook_manager.run_with_payload(
        repo::Hook::PostPush,
        &hook_ctx,
        &post_push_payload,
        std::time::Duration::from_secs(5),
    ) {
        tracing::warn!(error = %err, "post_push hook error swallowed");
    }

    Ok(())
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
            remote_transport_mismatch_advice(action, remote_arg.unwrap_or("<default>"))
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

fn remote_transport_mismatch_advice(action: &str, remote: &str) -> RecoveryAdvice {
    RecoveryAdvice::safety_refusal(
        "remote_transport_mismatch",
        format!(
            "Refusing to {action}: remote '{remote}' is a Git remote, not a Heddle-native remote"
        ),
        "Use a Heddle-native remote here, or clone/adopt that Git remote in a Git-overlay checkout.",
        format!("remote '{remote}' resolves to Git storage"),
        format!(
            "{action} would route a Git repository through Heddle-native sync and fail after setup work"
        ),
        "remote configuration, Heddle refs, Git refs, and worktree files were left unchanged",
        "heddle clone <remote> <fresh-path>",
        vec![
            "heddle clone <remote> <fresh-path>".to_string(),
            "heddle remote add <name> <url>".to_string(),
        ],
    )
}

fn remote_not_configured_advice(action: &str) -> RecoveryAdvice {
    RecoveryAdvice::safety_refusal(
        "remote_not_configured",
        format!("No default remote is configured for {action}"),
        "Add a remote with `heddle remote add <name> <url>`, inspect remotes with `heddle remote list`, or choose one with `heddle remote set-default <name>`.",
        "the command did not receive a remote argument and no default remote is configured",
        format!(
            "{action} needs a concrete remote target before it can move remote refs or transfer objects"
        ),
        "repository state, refs, remote configuration, and worktree files were left unchanged",
        "heddle remote add <name> <url>",
        vec![
            "heddle remote add <name> <url>".to_string(),
            "heddle remote list".to_string(),
            "heddle remote set-default <name>".to_string(),
        ],
    )
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
    let tracking_remote = resolve_git_tracking_remote_name(repo, remote_name)?;

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
        return Err(anyhow!(git_tracking_refresh_failed_advice(
            remote_name,
            &full_ref,
            Some(error.to_string()),
        )));
    }

    write_git_overlay_branch_upstream(repo.root(), &branch, &tracking_remote.name)?;

    Ok(Some(GitOverlayTrackingRefresh {
        remote_name: tracking_remote.name,
        configured_remote: tracking_remote.configured_remote,
        upstream_branch: Some(branch),
    }))
}

fn git_tracking_refresh_failed_advice(
    remote_name: &str,
    full_ref: &str,
    cause: Option<String>,
) -> RecoveryAdvice {
    let fetch_command = format!("heddle fetch {remote_name}");
    let error = match cause {
        Some(cause) => format!(
            "Pushed to {remote_name}, but could not refresh local tracking ref {full_ref}: {cause}"
        ),
        None => {
            format!("Pushed to {remote_name}, but could not refresh local tracking ref {full_ref}")
        }
    };
    RecoveryAdvice::safety_refusal(
        "git_overlay_tracking_refresh_failed",
        error,
        format!(
            "Run `{fetch_command}` if `heddle verify` still reports remote drift after the push."
        ),
        format!("remote push completed, but local Git tracking ref {full_ref} was not updated"),
        format!(
            "updating {full_ref} would record the pushed HEAD as the local tracking view of {remote_name}"
        ),
        "the remote push completed; the failed tracking-ref refresh did not make additional local tracking changes",
        fetch_command.clone(),
        vec![fetch_command, "heddle verify".to_string()],
    )
}

#[derive(Debug, Clone)]
struct GitTrackingRemoteResolution {
    name: String,
    configured_remote: Option<GitOverlayConfiguredRemote>,
}

fn resolve_git_tracking_remote_name(
    repo: &Repository,
    requested: &str,
) -> Result<GitTrackingRemoteResolution> {
    if let Some(name) = git_remote_name_for_url(repo.root(), requested)? {
        return Ok(GitTrackingRemoteResolution {
            name,
            configured_remote: None,
        });
    }
    if !looks_like_remote_location(requested)
        && git_remote_ref_name_is_valid(repo.root(), requested)?
    {
        return Ok(GitTrackingRemoteResolution {
            name: requested.to_string(),
            configured_remote: None,
        });
    }

    let remotes = git_remote_names(repo.root())?;
    if remotes.is_empty() && !requested.trim().is_empty() {
        write_git_overlay_remote(repo.root(), "origin", requested)
            .context("failed to configure Git remote for tracking")?;
        return Ok(GitTrackingRemoteResolution {
            name: "origin".to_string(),
            configured_remote: Some(GitOverlayConfiguredRemote {
                name: "origin".to_string(),
                url: requested.to_string(),
            }),
        });
    }
    if remotes.len() == 1 {
        return Ok(GitTrackingRemoteResolution {
            name: remotes[0].clone(),
            configured_remote: None,
        });
    }
    Ok(GitTrackingRemoteResolution {
        name: requested.to_string(),
        configured_remote: None,
    })
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
    fs::write(config_path, contents)?;
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
    fs::write(config_path, contents)?;
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
        Head::Attached { thread } => Ok(thread),
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

    target_repo.refs().set_thread(track_name, state_id)?;

    if should_output_json(cli, Some(repo.config())) {
        let trust = build_repository_verification_state(repo);
        let action = action_value(&trust);
        println!(
            "{}",
            serde_json::json!({
                "output_kind": "push",
                "status": "pushed",
                "success": true,
                "pushed": true,
                "changed": true,
                "transport": "heddle",
                "state": state_id.to_string(),
                "objects": objects_copied,
                "next_action": action,
                "next_action_argv": action_argv_value(&trust),
                "next_action_template": action_template_value(&trust),
                "recommended_action": action_value(&trust),
                "recommended_action_argv": action_argv_value(&trust),
                "recommended_action_template": action_template_value(&trust),
                "verification": trust,
            })
        );
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

    let mut config = options.user_config.heddle_client_config(options.token);
    if let Some(key) = options.server_key {
        config = config.with_server_key(key);
    }
    if let Some(pem) = options.credential_proof_key
        && config.auth_proof_key_pem.is_none()
    {
        config = config.with_auth_proof_key_pem(pem);
    }
    let mut client = HostedGrpcClient::connect(options.addr, &config).await?;
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
            let action = action_value(&trust);
            println!(
                "{}",
                serde_json::json!({
                    "output_kind": "push",
                    "status": "pushed",
                    "success": true,
                    "pushed": true,
                    "changed": true,
                    "transport": "heddle",
                    "state": result.new_state.map(|s| s.to_string()).unwrap_or_default(),
                    "next_action": action,
                    "next_action_argv": action_argv_value(&trust),
                    "next_action_template": action_template_value(&trust),
                    "recommended_action": action_value(&trust),
                    "recommended_action_argv": action_argv_value(&trust),
                    "recommended_action_template": action_template_value(&trust),
                    "verification": trust,
                })
            );
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
        return Err(anyhow::anyhow!(network_push_failed_advice(
            options.track_name,
            &err
        )));
    }

    Ok(())
}

#[cfg(feature = "client")]
fn network_push_failed_advice(track_name: &str, error: &str) -> RecoveryAdvice {
    let primary_command = format!("heddle push {track_name}");
    RecoveryAdvice::safety_refusal(
        "remote_push_failed",
        format!("Push failed for {track_name}: {error}"),
        format!(
            "Inspect `heddle verify`, then retry with `{primary_command}` after fixing the remote."
        ),
        format!("remote push to {track_name} failed: {error}"),
        "the remote branch was not confirmed updated",
        "local Heddle state, Git refs, and worktree files were left unchanged by the failed push result",
        primary_command.clone(),
        vec![primary_command, "heddle verify".to_string()],
    )
}

fn action_value(trust: &RepositoryVerificationState) -> serde_json::Value {
    if trust.recommended_action.trim().is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::Value::String(trust.recommended_action.clone())
    }
}

fn action_argv_value(trust: &RepositoryVerificationState) -> serde_json::Value {
    trust
        .recommended_action
        .trim()
        .is_empty()
        .then_some(serde_json::Value::Null)
        .unwrap_or_else(|| {
            recommended_action_argv(&trust.recommended_action)
                .ok()
                .flatten()
                .map(serde_json::Value::from)
                .unwrap_or(serde_json::Value::Null)
        })
}

fn action_template_value(trust: &RepositoryVerificationState) -> serde_json::Value {
    recommended_action_template(&trust.recommended_action)
        .and_then(|template| serde_json::to_value(template).ok())
        .unwrap_or(serde_json::Value::Null)
}

#[cfg(feature = "client")]
struct PushNetworkOptions<'a> {
    addr: SocketAddr,
    repo_path: Option<&'a str>,
    user_config: &'a UserConfig,
    token: Option<AuthToken>,
    server_key: Option<String>,
    credential_proof_key: Option<String>,
    state_id: &'a objects::object::ChangeId,
    track_name: &'a str,
    force: bool,
    cli: &'a Cli,
}
