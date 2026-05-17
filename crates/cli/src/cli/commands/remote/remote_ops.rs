// SPDX-License-Identifier: Apache-2.0
//! Pull, remote management, and serve commands.

#[cfg(feature = "client")]
use std::net::SocketAddr;
use std::{collections::BTreeMap, fs, path::Path};

use anyhow::{Context, Result};
#[cfg(feature = "client")]
use proto::AuthToken;
use refs::Head;
use repo::{Repository, RepositoryCapability};

#[cfg(feature = "client")]
use crate::client::HostedGrpcClient;
use crate::{
    bridge::GitBridge,
    cli::{Cli, RemoteCommands, should_output_json, style},
    client::LocalSync,
    config::UserConfig,
    remote::{Remote, RemoteConfig, RemoteTarget, resolve_remote_with_key},
};
#[cfg(feature = "client")]
use heddle_client::grpc_hosted::PullMaterialization;

/// Execute pull command.
pub async fn cmd_pull(
    cli: &Cli,
    remote: Option<String>,
    thread: Option<String>,
    local_thread: Option<String>,
    lazy: bool,
) -> Result<()> {
    let repo = Repository::open(cli.repo.as_ref().unwrap_or(&std::env::current_dir()?))?;
    if repo.capability() == RepositoryCapability::GitOverlay && !repo.hosted_enabled() {
        let remote_name = remote.as_deref().unwrap_or("origin");
        let mut bridge = GitBridge::new(&repo);
        bridge.pull(remote_name)?;
        if should_output_json(cli, Some(repo.config())) {
            println!(
                "{{\"pulled\":true,\"transport\":\"git\",\"remote\":{:?}}}",
                remote_name
            );
        } else {
            println!(
                "{} pulled Git-overlay refs from {}",
                style::ok_marker(),
                style::bold(remote_name)
            );
        }
        return Ok(());
    }

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

    let remote_thread = thread.unwrap_or_else(|| "main".to_string());
    let local_thread_name = local_thread.as_deref();

    match target {
        RemoteTarget::Local(path) => {
            pull_local(&repo, &path, &remote_thread, local_thread_name, cli, lazy).await?;
        }
        RemoteTarget::Network { addr, repo_path } => {
            #[cfg(feature = "client")]
            pull_network(
                &repo,
                PullNetworkOptions {
                    addr,
                    repo_path: repo_path.as_deref(),
                    user_config: &user_config,
                    token,
                    server_key,
                    credential_proof_key,
                    remote_thread: &remote_thread,
                    local_thread: local_thread_name,
                    lazy,
                    cli,
                },
            )
            .await?;
            #[cfg(not(feature = "client"))]
            let _ = (addr, repo_path, token);
            #[cfg(not(feature = "client"))]
            anyhow::bail!(
                "network pull support is not available in this build; enable the `client` feature"
            );
        }
    }

    Ok(())
}

async fn pull_local(
    repo: &Repository,
    source_path: &std::path::Path,
    remote_thread: &str,
    local_thread: Option<&str>,
    cli: &Cli,
    lazy: bool,
) -> Result<()> {
    if lazy {
        anyhow::bail!("lazy pull is only supported for hosted/network remotes");
    }

    if should_output_json(cli, Some(repo.config())) {
        println!("{{\"status\":\"connected\",\"type\":\"local\"}}");
    } else {
        println!(
            "{} pulling from {}",
            style::working_marker(),
            style::dim(&format!("file://{}", source_path.display()))
        );
    }

    let source = LocalSync::open(source_path)?;

    let state_id = source
        .source()
        .refs()
        .get_thread(remote_thread)?
        .context(format!("Thread {} not found in source", remote_thread))?;

    let objects_copied = source.fetch_state(repo, &state_id)?;

    let track_to_update = local_thread.unwrap_or(remote_thread);

    // Capture the local thread's pre-pull tip *before* mutating it
    // (heddle#110). The materializing branch records an
    // `OpRecord::FastForwardV2` whose `pre_target_id` must be the
    // pre-pull state so undo restores both HEAD and the local thread
    // ref. Reading the ref after `set_thread` would return the
    // post-pull state and silently strand the thread on undo —
    // exactly the bug we're closing.
    let pre_target = repo.refs().get_thread(track_to_update)?;
    repo.refs().set_thread(track_to_update, &state_id)?;

    // Preserve attached-HEAD semantics only when the pull target is the
    // current checkout. Pulling a remote into a side thread must not move
    // the operator's active thread or overwrite its worktree.
    let should_materialize = match repo.head_ref()? {
        Head::Attached { thread } => thread == track_to_update,
        Head::Detached { .. } => local_thread.is_none(),
    };
    if should_materialize {
        // First-time pull of this thread has no pre_target_id; the
        // `set_thread` above effectively created the ref. Fall back to
        // recording the generic `Goto` inverse — there's no pre-FF tip
        // to restore on undo, only HEAD to rewind. Repeat pulls flow
        // through the `FastForwardV2` arm so undo restores the local
        // thread ref alongside HEAD.
        match pre_target {
            Some(pre) => {
                super::super::ff_record::record_ff_advance_explicit(
                    repo,
                    remote_thread,
                    &pre,
                    &state_id,
                )?;
            }
            None => {
                repo.fast_forward_attached(&state_id)?;
            }
        }
    }

    if should_output_json(cli, Some(repo.config())) {
        println!(
            "{{\"success\":true,\"state\":\"{}\",\"objects\":{}}}",
            state_id, objects_copied
        );
    } else {
        println!(
            "{} pulled {} from {} ({})",
            style::ok_marker(),
            style::change_id(&state_id.short().to_string()),
            style::bold(remote_thread),
            style::count(objects_copied, "object")
        );
    }

    Ok(())
}

#[cfg(feature = "client")]
async fn pull_network(repo: &Repository, options: PullNetworkOptions<'_>) -> Result<()> {
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
        .pull_with_depth_and_materialization(
            repo,
            repo_path,
            options.remote_thread,
            options.local_thread,
            None,
            if options.lazy {
                PullMaterialization::Lazy
            } else {
                PullMaterialization::Full
            },
        )
        .await?;

    if result.success {
        if should_output_json(options.cli, Some(repo.config())) {
            println!(
                "{{\"success\":true,\"state\":\"{}\"}}",
                result
                    .final_state
                    .map(|s| s.to_string())
                    .unwrap_or_default()
            );
        } else {
            println!(
                "{} pulled from {}",
                style::ok_marker(),
                style::bold(options.remote_thread)
            );
            if let Some(final_state) = result.final_state {
                println!(
                    "{}",
                    style::field("state", &style::change_id(&final_state.to_string()))
                );
            }
        }
    } else {
        let err = result.error.unwrap_or_else(|| "Unknown error".to_string());
        return Err(anyhow::anyhow!("Pull failed: {}", err));
    }

    Ok(())
}

/// Execute remote command.
pub fn cmd_remote(cli: &Cli, command: RemoteCommands) -> Result<()> {
    let repo = Repository::open(cli.repo.as_ref().unwrap_or(&std::env::current_dir()?))?;

    match command {
        RemoteCommands::List => {
            let cfg = RemoteConfig::open(&repo).map_err(anyhow::Error::msg)?;
            let mut items: BTreeMap<String, String> = cfg
                .list()
                .into_iter()
                .map(|(name, remote)| (name, remote.url))
                .collect();
            if repo.capability() == RepositoryCapability::GitOverlay {
                for (name, url) in git_overlay_config_remotes(&repo) {
                    items.entry(name).or_insert(url);
                }
            }
            if items.is_empty() {
                println!("{}", style::dim("No remotes configured"));
                println!("{}", style::field("next", "heddle remote add <name> <url>"));
                return Ok(());
            }
            println!("{}", style::section("Remotes"));
            for (name, url) in items {
                println!("  {} {}", style::bold(&name), style::dim(&url));
            }
            Ok(())
        }
        RemoteCommands::Add { name, url } => {
            let mut cfg = RemoteConfig::open(&repo).map_err(anyhow::Error::msg)?;
            cfg.add(&name, Remote { url }).map_err(anyhow::Error::msg)?;
            println!("{} added remote {}", style::ok_marker(), style::bold(&name));
            Ok(())
        }
        RemoteCommands::Remove { name } => {
            let mut cfg = RemoteConfig::open(&repo).map_err(anyhow::Error::msg)?;
            cfg.remove(&name).map_err(anyhow::Error::msg)?;
            println!(
                "{} removed remote {}",
                style::ok_marker(),
                style::bold(&name)
            );
            Ok(())
        }
        RemoteCommands::Show { name } => {
            let cfg = RemoteConfig::open(&repo).map_err(anyhow::Error::msg)?;
            let remote = cfg.get(&name).map_err(anyhow::Error::msg)?;
            println!("{}", style::section("Remote"));
            println!("  {}", style::field("name", &style::bold(&name)));
            println!("  {}", style::field("url", &style::dim(&remote.url)));
            Ok(())
        }
    }
}

fn git_overlay_config_remotes(repo: &Repository) -> BTreeMap<String, String> {
    let mut remotes = BTreeMap::new();
    for config_path in git_overlay_config_paths(repo) {
        parse_git_config_remotes(&config_path, &mut remotes);
    }
    remotes
}

fn git_overlay_config_paths(repo: &Repository) -> Vec<std::path::PathBuf> {
    let mut paths = Vec::new();
    paths.push(repo.root().join(".git").join("config"));
    if let Some(git_dir) = pointed_git_dir(&repo.root().join(".git")) {
        paths.push(git_dir.join("config"));
        if let Some(common_dir) = common_git_dir(&git_dir) {
            paths.push(common_dir.join("config"));
        }
    }
    paths.push(repo.heddle_dir().join("git").join("config"));
    paths
}

fn pointed_git_dir(dot_git: &Path) -> Option<std::path::PathBuf> {
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

fn common_git_dir(git_dir: &Path) -> Option<std::path::PathBuf> {
    let contents = fs::read_to_string(git_dir.join("commondir")).ok()?;
    let target = contents.trim();
    let path = Path::new(target);
    Some(if path.is_absolute() {
        path.to_path_buf()
    } else {
        git_dir.join(path)
    })
}

fn parse_git_config_remotes(path: &Path, remotes: &mut BTreeMap<String, String>) {
    let Ok(contents) = fs::read_to_string(path) else {
        return;
    };
    let mut current_remote: Option<String> = None;
    for raw in contents.lines() {
        let line = raw.trim();
        if line.starts_with('[') && line.ends_with(']') {
            current_remote = line
                .strip_prefix("[remote \"")
                .and_then(|rest| rest.strip_suffix("\"]"))
                .map(str::to_string);
            continue;
        }
        let Some(name) = current_remote.as_ref() else {
            continue;
        };
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if key.trim() == "url" {
            remotes
                .entry(name.clone())
                .or_insert_with(|| value.trim().to_string());
        }
    }
}

#[cfg(feature = "client")]
struct PullNetworkOptions<'a> {
    addr: SocketAddr,
    repo_path: Option<&'a str>,
    user_config: &'a UserConfig,
    token: Option<AuthToken>,
    server_key: Option<String>,
    credential_proof_key: Option<String>,
    remote_thread: &'a str,
    local_thread: Option<&'a str>,
    lazy: bool,
    cli: &'a Cli,
}
