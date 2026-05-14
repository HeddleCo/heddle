// SPDX-License-Identifier: Apache-2.0
//! Remote operations (push, pull, remote management).

#[cfg(feature = "client")]
use std::net::SocketAddr;

use anyhow::{Context, Result};
#[cfg(feature = "client")]
use proto::AuthToken;
use refs::Head;
use repo::{Repository, RepositoryCapability};

use super::snapshot::ensure_current_state;
#[cfg(feature = "client")]
use crate::client::HostedGrpcClient;
use crate::{
    bridge::GitBridge,
    cli::{Cli, should_output_json, style},
    client::LocalSync,
    config::UserConfig,
    remote::{RemoteTarget, resolve_remote_with_key},
};

mod remote_ops;

pub use remote_ops::{cmd_pull, cmd_remote};

/// Execute push command.
pub async fn cmd_push(
    cli: &Cli,
    remote: Option<String>,
    thread: Option<String>,
    state: Option<String>,
    force: bool,
) -> Result<()> {
    let repo = Repository::open(cli.repo.as_ref().unwrap_or(&std::env::current_dir()?))?;
    if repo.capability() == RepositoryCapability::GitOverlay && !repo.hosted_enabled() {
        let remote_name = remote.as_deref().unwrap_or("origin");
        let mut bridge = GitBridge::new(&repo);
        bridge.push(remote_name)?;
        if should_output_json(cli, Some(repo.config())) {
            println!(
                "{{\"pushed\":true,\"transport\":\"git\",\"remote\":{:?}}}",
                remote_name
            );
        } else {
            println!(
                "{} pushed Git-overlay refs to {}",
                style::ok_marker(),
                style::bold(remote_name)
            );
        }
        return Ok(());
    }

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
        anyhow::bail!("pre_push hook vetoed: {}", resp.abort);
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
            anyhow::bail!(
                "network push support is not available in this build; enable the `client` feature"
            );
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
    if should_output_json(cli, Some(repo.config())) {
        println!("{{\"status\":\"connected\",\"type\":\"local\"}}");
    } else {
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
        println!(
            "{{\"success\":true,\"state\":\"{}\",\"objects\":{}}}",
            state_id, objects_copied
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

    if should_output_json(options.cli, Some(repo.config())) {
        println!("{{\"status\":\"connected\"}}");
    } else {
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
            println!(
                "{{\"success\":true,\"state\":\"{}\"}}",
                result.new_state.map(|s| s.to_string()).unwrap_or_default()
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
        return Err(anyhow::anyhow!("Push failed: {}", err));
    }

    Ok(())
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