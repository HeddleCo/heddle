// SPDX-License-Identifier: Apache-2.0
//! Clone command - clone from remote.

use std::{fs, path::Path};

#[cfg(feature = "client")]
use anyhow::Context;
use anyhow::{Result, anyhow};
use refs::Head;
use repo::Repository;

#[cfg(feature = "client")]
use crate::remote::credential_key_from_remote_url;
use crate::{
    bridge::{
        GitBridge,
        git_core::{clone_url_to_bare, copy_local_repo_to_bare},
        git_import::{import_all, import_selected_refs},
    },
    cli::{Cli, should_output_json, style},
    client::LocalSync,
    remote::RemoteTarget,
};
#[cfg(feature = "client")]
use heddle_client::grpc_hosted::PullMaterialization;

/// Pull/materialization options shared by local and network clone paths.
struct CloneOptions {
    thread: Option<String>,
    depth: Option<u32>,
    lazy: bool,
    filter: Option<String>,
}

pub async fn cmd_clone(
    cli: &Cli,
    remote: String,
    local: String,
    thread: Option<String>,
    depth: Option<u32>,
    lazy: bool,
    filter: Option<String>,
) -> Result<()> {
    let local_path = Path::new(&local);
    let options = CloneOptions {
        thread,
        depth: depth.filter(|depth| *depth > 0),
        lazy,
        filter,
    };

    if local_path.exists() {
        return Err(anyhow!("Local path '{}' already exists", local));
    }

    // Parse the remote URL
    #[cfg(feature = "client")]
    let server_key = credential_key_from_remote_url(&remote);
    let target = match RemoteTarget::parse(&remote) {
        Ok(target) => target,
        Err(_) => {
            if let Ok(url) = gix::url::parse(remote.as_bytes().into()) {
                return clone_git_overlay_url(cli, &url, local_path, &options);
            }
            return Err(anyhow!("invalid remote url: {}", remote));
        }
    };

    match target {
        RemoteTarget::Local(remote_path) => {
            if !remote_path.join(".heddle").exists() && gix::open(&remote_path).is_ok() {
                return clone_git_overlay_path(cli, &remote_path, local_path, &options);
            }
            clone_local(cli, &remote_path, local_path, &options).await?;
        }
        RemoteTarget::Network { addr, repo_path } => {
            #[cfg(feature = "client")]
            clone_network(
                cli,
                addr,
                repo_path.as_deref(),
                local_path,
                &options,
                server_key,
            )
            .await?;
            #[cfg(not(feature = "client"))]
            let _ = (addr, repo_path);
            #[cfg(not(feature = "client"))]
            anyhow::bail!(
                "network clone support is not available in this build; enable the `client` feature"
            );
        }
    }

    Ok(())
}

fn clone_git_overlay_url(
    cli: &Cli,
    url: &gix::Url,
    local_path: &Path,
    options: &CloneOptions,
) -> Result<()> {
    fs::create_dir_all(local_path)?;
    clone_url_to_bare(
        url,
        &local_path.join(".git"),
        options.depth,
        git_overlay_filter_spec(options),
    )
    .map_err(anyhow::Error::msg)?;
    finish_git_overlay_clone(cli, local_path, options, url.to_string())
}

fn clone_git_overlay_path(
    cli: &Cli,
    remote_path: &Path,
    local_path: &Path,
    options: &CloneOptions,
) -> Result<()> {
    // The local-copy path (used when both source and dest are on the
    // same filesystem) bypasses the gix fetch builder entirely, so
    // `--depth` / `--filter` would have nothing to plug into. Reject
    // those explicitly here rather than silently dropping them.
    if let Some(filter) = options.filter.as_deref() {
        return Err(anyhow!(
            "--filter {} is not supported when cloning from a local Git path; use a file:// URL instead",
            filter
        ));
    }
    if options.lazy {
        return Err(anyhow!(
            "--lazy is not supported when cloning from a local Git path; use a file:// URL instead"
        ));
    }
    if options.depth.is_some() {
        return Err(anyhow!(
            "--depth is not supported when cloning from a local Git path; use a file:// URL instead"
        ));
    }
    fs::create_dir_all(local_path)?;
    gix::init(local_path).map_err(anyhow::Error::msg)?;
    copy_local_repo_to_bare(remote_path, &local_path.join(".git")).map_err(anyhow::Error::msg)?;
    finish_git_overlay_clone(cli, local_path, options, remote_path.display().to_string())
}

/// `--filter blob:none` is a synonym for `--lazy` on the Git-overlay
/// path too, mirroring the hosted/network mapping in `clone_network`.
/// Returns `None` if neither was requested.
fn git_overlay_filter_spec(options: &CloneOptions) -> Option<&str> {
    if let Some(filter) = options.filter.as_deref() {
        return Some(filter);
    }
    if options.lazy {
        return Some("blob:none");
    }
    None
}

fn finish_git_overlay_clone(
    cli: &Cli,
    local_path: &Path,
    options: &CloneOptions,
    remote_label: String,
) -> Result<()> {
    write_git_overlay_origin(local_path, &remote_label)?;
    let repo = Repository::init(local_path)?;
    let mut bridge = GitBridge::new(&repo);
    let stats = if let Some(thread) = options.thread.as_ref() {
        import_selected_refs(
            &mut bridge,
            Some(&local_path.join(".git")),
            std::slice::from_ref(thread),
        )
        .map_err(anyhow::Error::msg)?
    } else {
        import_all(&mut bridge, Some(&local_path.join(".git"))).map_err(anyhow::Error::msg)?
    };

    let track_name = select_clone_thread(&repo, options.thread.as_deref())?;
    repo.refs().write_head(&Head::Attached {
        thread: track_name.clone(),
    })?;
    let state_id = repo
        .refs()
        .get_thread(&track_name)?
        .ok_or_else(|| anyhow!("Git clone did not import branch '{}'", track_name))?;
    repo.goto(&state_id)?;

    if should_output_json(cli, Some(repo.config())) {
        println!(
            "{{\"status\":\"cloned\",\"transport\":\"git\",\"remote\":{:?},\"local\":{:?},\"branch\":{:?},\"commits_imported\":{}}}",
            remote_label,
            local_path.display().to_string(),
            track_name,
            stats.commits_imported
        );
    } else {
        println!(
            "{} cloned {} into {}",
            style::ok_marker(),
            style::dim(&remote_label),
            style::bold(&local_path.display().to_string())
        );
        println!(
            "  {}",
            style::field(
                "imported",
                &format!(
                    "{}; checked out {}",
                    style::count(stats.commits_imported, "Git commit"),
                    style::bold(&track_name)
                )
            )
        );
    }
    Ok(())
}

fn write_git_overlay_origin(local_path: &Path, remote_label: &str) -> Result<()> {
    let config_path = local_path.join(".git").join("config");
    let mut contents = fs::read_to_string(&config_path).unwrap_or_default();
    if contents.contains("[remote \"origin\"]") {
        return Ok(());
    }
    if !contents.ends_with('\n') && !contents.is_empty() {
        contents.push('\n');
    }
    contents.push_str(&format!(
        "[remote \"origin\"]\n\turl = {remote_label}\n\tfetch = +refs/heads/*:refs/remotes/origin/*\n"
    ));
    fs::write(config_path, contents)?;
    Ok(())
}

fn select_clone_thread(repo: &Repository, requested: Option<&str>) -> Result<String> {
    if let Some(requested) = requested {
        return Ok(requested.to_string());
    }
    let threads = repo.refs().list_threads()?;
    if threads.iter().any(|thread| thread == "main") {
        return Ok("main".to_string());
    }
    threads
        .into_iter()
        .next()
        .ok_or_else(|| anyhow!("Git clone did not import any branch refs"))
}

async fn clone_local(
    cli: &Cli,
    remote_path: &Path,
    local_path: &Path,
    options: &CloneOptions,
) -> Result<()> {
    let CloneOptions {
        thread,
        depth,
        lazy,
        filter,
    } = options;
    let depth = *depth;
    if let Some(filter) = filter.as_deref() {
        return Err(anyhow!(
            "--filter {} is only supported for hosted/network remotes",
            filter
        ));
    }
    if *lazy {
        return Err(anyhow!(
            "lazy clone is only supported for hosted/network remotes"
        ));
    }

    if !remote_path.exists() {
        return Err(anyhow!(
            "Remote repository '{}' does not exist",
            remote_path.display()
        ));
    }

    // Create the local directory
    fs::create_dir_all(local_path)?;

    // Initialize the local repository
    let local_repo = Repository::init(local_path)?;

    // Open the remote and sync
    let sync = LocalSync::open(remote_path)?;
    let remote_repo = sync.source();

    // Get the thread to clone
    let track_name = thread.as_deref().unwrap_or("main");
    let state_id = remote_repo
        .refs()
        .get_thread(track_name)?
        .ok_or_else(|| anyhow!("Thread '{}' not found in remote", track_name))?;

    // Fetch the state and dependencies
    let objects_copied = if let Some(d) = depth {
        sync.fetch_state_with_depth(&local_repo, &state_id, d)?
    } else {
        sync.fetch_state(&local_repo, &state_id)?
    };

    // Set up the thread locally
    local_repo.refs().set_thread(track_name, &state_id)?;

    // Intentional raw `goto`: a fresh `Repository::init` writes HEAD as
    // `Attached { thread: "main" }`, but we may be cloning a non-"main"
    // thread (e.g. `develop`). `fast_forward_attached` here would
    // mis-advance the "main" thread ref instead of the cloned one. Clone
    // post-HEAD-attach is tracked separately.
    local_repo.goto(&state_id)?;

    // Copy worktree files from remote (for file:// protocol, we can do a direct copy)
    copy_worktree(remote_repo.root(), local_repo.root())?;

    if should_output_json(cli, Some(local_repo.config())) {
        println!(
            "{{\"status\": \"cloned\", \"remote\": \"file://{}\", \"local\": \"{}\", \"objects\": {}}}",
            remote_path.display(),
            local_path.display(),
            objects_copied
        );
    } else {
        let depth_info = depth.map(|d| format!(" (depth {})", d)).unwrap_or_default();
        println!(
            "{} cloned {} into {}{}",
            style::ok_marker(),
            style::dim(&format!("file://{}", remote_path.display())),
            style::bold(&local_path.display().to_string()),
            style::dim(&depth_info)
        );
        println!(
            "  {}",
            style::field("copied", &style::count(objects_copied, "object"))
        );
    }

    Ok(())
}

#[cfg(feature = "client")]
async fn clone_network(
    cli: &Cli,
    addr: std::net::SocketAddr,
    repo_path: Option<&str>,
    local_path: &Path,
    options: &CloneOptions,
    server_key: Option<String>,
) -> Result<()> {
    use crate::{client::HostedGrpcClient, config::UserConfig};

    let CloneOptions {
        thread,
        depth,
        lazy,
        filter,
    } = options;
    let depth = *depth;
    // `--filter blob:none` is a synonym for `--lazy` on hosted/network
    // remotes; both produce a clone whose blob content is hydrated on demand.
    let lazy = *lazy || filter.is_some();

    // Create the local directory
    fs::create_dir_all(local_path)?;

    // Initialize the local repository
    let local_repo = Repository::init(local_path)?;

    let user_config = UserConfig::load_default().unwrap_or_default();

    // Connect to remote
    let mut config = user_config.heddle_client_config(None);
    if let Some(key) = server_key {
        config = config.with_server_key(key);
    }
    let repo_path = repo_path.context("network remotes must include a hosted repository path")?;

    let mut client = HostedGrpcClient::connect(addr, &config).await?;
    client.auto_rotate_if_needed().await;

    if should_output_json(cli, Some(local_repo.config())) {
        println!("{{\"status\":\"connected\",\"address\":\"{}\"}}", addr);
    } else {
        println!("Connected to {}", addr);
    }

    let track_name = thread.as_deref().unwrap_or("main");
    let materialization = if lazy {
        PullMaterialization::Lazy
    } else {
        PullMaterialization::Full
    };
    let result = client
        .pull_with_depth_and_materialization(
            &local_repo,
            repo_path,
            track_name,
            Some(track_name),
            depth,
            materialization,
        )
        .await?;
    if result.success {
        if should_output_json(cli, Some(local_repo.config())) {
            println!(
                "{{\"status\": \"cloned\", \"remote\": \"{}\", \"local\": \"{}\", \"state\": \"{}\"}}",
                addr,
                local_path.display(),
                result
                    .final_state
                    .map(|s| s.to_string())
                    .unwrap_or_default()
            );
        } else {
            let depth_info = depth.map(|d| format!(" (depth {})", d)).unwrap_or_default();
            println!(
                "{} cloned {} into {}{}",
                style::ok_marker(),
                style::dim(&addr.to_string()),
                style::bold(&local_path.display().to_string()),
                style::dim(&depth_info)
            );
            if let Some(state) = result.final_state {
                println!(
                    "  {}",
                    style::field("state", &style::change_id(&state.to_string()))
                );
            }
        }
    } else {
        let err = result.error.unwrap_or_else(|| "Unknown error".to_string());
        return Err(anyhow!("Clone failed: {}", err));
    }

    Ok(())
}

fn copy_worktree(from: &Path, to: &Path) -> Result<()> {
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let path = entry.path();
        let file_name = entry.file_name();

        if file_name == ".heddle" || file_name == ".git" {
            continue;
        }

        let dest_path = to.join(&file_name);
        copy_entry(&path, &dest_path)?;
    }

    Ok(())
}

fn copy_dir_recursive(from: &Path, to: &Path) -> Result<()> {
    fs::create_dir_all(to)?;

    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let path = entry.path();
        let dest_path = to.join(entry.file_name());
        copy_entry(&path, &dest_path)?;
    }

    Ok(())
}

fn copy_entry(path: &Path, dest_path: &Path) -> Result<()> {
    if path.is_symlink() {
        let target = fs::read_link(path)?;
        #[cfg(unix)]
        std::os::unix::fs::symlink(&target, dest_path)?;
        #[cfg(not(unix))]
        return Err(anyhow!("Symlinks are not supported on this platform"));
    } else if path.is_dir() {
        copy_dir_recursive(path, dest_path)?;
    } else {
        fs::copy(path, dest_path)?;
    }
    Ok(())
}
