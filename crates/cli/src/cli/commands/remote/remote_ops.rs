// SPDX-License-Identifier: Apache-2.0
//! Pull, remote management, and serve commands.

#[cfg(feature = "client")]
use std::net::SocketAddr;
use std::{collections::BTreeMap, fs, path::Path};

use anyhow::{Context, Result};
#[cfg(feature = "client")]
use heddle_client::grpc_hosted::PullMaterialization;
use objects::{
    fs_atomic::write_file_atomic,
    object::{ChangeId, Tree},
};
#[cfg(feature = "client")]
use proto::AuthToken;
use refs::Head;
use repo::{Repository, RepositoryCapability};
use serde::Serialize;

use super::super::{
    advice::RecoveryAdvice,
    git_overlay_health::{
        RepositoryVerificationState, build_plain_git_verification_probe,
        build_repository_verification_state,
    },
    worktree_safety::ensure_worktree_clean,
};
#[cfg(feature = "client")]
use crate::client::HostedGrpcClient;
use crate::{
    bridge::GitBridge,
    cli::{Cli, RemoteCommands, should_output_json, style},
    client::LocalSync,
    config::UserConfig,
    remote::{Remote, RemoteConfig, RemoteTarget, resolve_remote_with_key},
};

#[derive(Serialize)]
struct RemoteListOutput {
    output_kind: &'static str,
    remotes: Vec<RemoteInfoOutput>,
}

#[derive(Serialize)]
struct RemoteInfoOutput {
    #[serde(skip_serializing_if = "Option::is_none")]
    output_kind: Option<&'static str>,
    name: String,
    url: String,
    source: String,
    is_default: bool,
}

#[derive(Serialize)]
struct RemoteMutationOutput {
    output_kind: &'static str,
    status: &'static str,
    action: &'static str,
    name: String,
    url: Option<String>,
    default: Option<String>,
    message: String,
    #[serde(rename = "verification")]
    trust: RepositoryVerificationState,
}

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
        ensure_worktree_clean(&repo, "pull")?;
        let remote_name = resolve_default_remote_name(&repo, remote.as_deref())?;
        let branch = repo.git_overlay_current_branch()?;
        let old_git_head = git_checkout_head_oid(repo.root());
        let old_state = repo.head()?;
        let mut bridge = GitBridge::new(&repo);
        let outcome = bridge.pull(&remote_name)?;
        let new_git_head = git_checkout_head_oid(repo.root());
        let new_state = repo.head()?;
        let changed_paths =
            changed_paths_between_states(&repo, old_state.as_ref(), new_state.as_ref())?;
        let verification = build_repository_verification_state(&repo);
        let status = if outcome.changed {
            "updated"
        } else {
            "up_to_date"
        };
        if should_output_json(cli, Some(repo.config())) {
            println!(
                "{}",
                serde_json::json!({
                    "output_kind": "pull",
                    "status": status,
                    "success": true,
                    "pulled": outcome.changed,
                    "changed": outcome.changed,
                    "transport": "git",
                    "remote": remote_name,
                    "branch": branch,
                    "old_git_head": old_git_head,
                    "new_git_head": new_git_head,
                    "old_state": old_state.as_ref().map(ToString::to_string),
                    "new_state": new_state.as_ref().map(ToString::to_string),
                    "states_created": outcome.states_created,
                    "commits_seen": outcome.commits_seen,
                    "commits_seen_scope": "branches_and_heddle_notes",
                    "materialized_checkout": outcome.materialized_checkout,
                    "changed_path_count": changed_paths.len(),
                    "changed_paths": changed_paths,
                    "verification": verification,
                })
            );
        } else {
            if outcome.changed {
                println!(
                    "{} pulled from {}",
                    style::ok_marker(),
                    style::bold(&remote_name)
                );
            } else {
                println!(
                    "{} already up to date with {}",
                    style::ok_marker(),
                    style::bold(&remote_name)
                );
            }
            if let Some(branch) = &branch {
                if outcome.changed {
                    println!("Branch: {}", style::bold(branch));
                } else if let Some(head) = &new_git_head {
                    println!("Branch: {} at {}", style::bold(branch), short_oid(head));
                }
            }
            match (&old_git_head, &new_git_head) {
                (Some(old), Some(new)) if old != new => {
                    println!("Git: {} -> {}", short_oid(old), short_oid(new));
                }
                (Some(head), Some(_)) if outcome.changed => {
                    println!("Git: {}", short_oid(head));
                }
                _ => {}
            }
            println!(
                "Imported: {}",
                style::count(outcome.states_created, "new state")
            );
            println!(
                "Scanned: {} across branches + refs/notes/heddle",
                style::count(outcome.commits_seen, "Git commit object")
            );
            if outcome.materialized_checkout {
                println!("Worktree: materialized checkout");
            }
            if outcome.changed {
                println!("Changed paths: {}", changed_paths.len());
                for path in changed_paths.iter().take(8) {
                    println!("  - {path}");
                }
                if changed_paths.len() > 8 {
                    println!("  - ... {} more", changed_paths.len() - 8);
                }
            }
            if !verification.verified {
                println!("Workspace: {}", style::warn(&verification.status));
                if !verification.recommended_action.is_empty() {
                    println!("Next: {}", style::bold(&verification.recommended_action));
                }
            } else {
                println!("Workspace: verified");
            }
        }
        return Ok(());
    }

    super::preflight_native_remote_transport(&repo, remote.as_deref(), "pull")?;

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
    let should_materialize = match repo.head_ref()? {
        Head::Attached { thread } => local_thread_name.is_none_or(|local| local == thread),
        Head::Detached { .. } => local_thread_name.is_none(),
    };
    if should_materialize {
        ensure_worktree_clean(&repo, "pull")?;
    }

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
        return Err(anyhow::anyhow!(local_lazy_pull_advice(source_path)));
    }

    if !should_output_json(cli, Some(repo.config())) {
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
    let changed = pre_target.as_ref() != Some(&state_id) || objects_copied > 0;
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
            "{}",
            serde_json::json!({
                "output_kind": "pull",
                "status": if changed { "updated" } else { "up_to_date" },
                "success": true,
                "pulled": changed,
                "changed": changed,
                "transport": "heddle",
                "remote": source_path.display().to_string(),
                "thread": track_to_update,
                "state": state_id.to_string(),
                "objects": objects_copied,
                "verification": build_repository_verification_state(repo),
            })
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

fn git_checkout_head_oid(root: &Path) -> Option<String> {
    let git = gix::discover(root).ok()?;
    Some(git.head_id().ok()?.detach().to_string())
}

fn short_oid(oid: &str) -> String {
    oid.chars().take(12).collect()
}

fn changed_paths_between_states(
    repo: &Repository,
    old_state: Option<&ChangeId>,
    new_state: Option<&ChangeId>,
) -> Result<Vec<String>> {
    if old_state == new_state {
        return Ok(Vec::new());
    }
    let Some(new_state) = new_state else {
        return Ok(Vec::new());
    };
    let new_state = repo
        .store()
        .get_state(new_state)?
        .context("new pulled state was not found in Heddle storage")?;
    let old_tree = match old_state {
        Some(old_state) => repo
            .store()
            .get_state(old_state)?
            .map(|state| state.tree)
            .unwrap_or_else(|| Tree::new().hash()),
        None => Tree::new().hash(),
    };
    let mut paths = repo
        .diff_trees(&old_tree, &new_state.tree)?
        .iter()
        .map(|change| change.path.clone())
        .collect::<Vec<_>>();
    paths.sort();
    paths.dedup();
    Ok(paths)
}

fn local_lazy_pull_advice(source_path: &Path) -> RecoveryAdvice {
    let source = source_path.display().to_string();
    let pull_without_lazy = format!("heddle pull {source}");

    RecoveryAdvice::safety_refusal(
        "local_lazy_pull_unsupported",
        "Refusing lazy pull from local remote: lazy materialization requires a hosted or network remote",
        format!(
            "Run `{pull_without_lazy}` without `--lazy`, or configure a hosted remote and retry lazy pull there."
        ),
        format!("selected remote resolves to local path file://{source}"),
        "lazy pull would leave the worktree depending on on-demand object fetches that the local transport does not provide",
        "repository state was left unchanged",
        pull_without_lazy.clone(),
        vec![pull_without_lazy, "heddle remote list".to_string()],
    )
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

    if !should_output_json(options.cli, Some(repo.config())) {
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
        let changed = result.final_state.is_some();
        if should_output_json(options.cli, Some(repo.config())) {
            println!(
                "{}",
                serde_json::json!({
                    "output_kind": "pull",
                    "status": if changed { "updated" } else { "up_to_date" },
                    "success": true,
                    "pulled": changed,
                    "changed": changed,
                    "transport": "heddle",
                    "remote": options.remote_thread,
                    "thread": options.local_thread.unwrap_or(options.remote_thread),
                    "state": result
                        .final_state
                        .map(|s| s.to_string())
                        .unwrap_or_default(),
                    "verification": build_repository_verification_state(repo),
                })
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
        return Err(anyhow::anyhow!(network_pull_failed_advice(
            options.remote_thread,
            options.local_thread,
            &err,
        )));
    }

    Ok(())
}

#[cfg(feature = "client")]
fn network_pull_failed_advice(
    remote_thread: &str,
    local_thread: Option<&str>,
    error: &str,
) -> RecoveryAdvice {
    let primary_command = if let Some(local_thread) = local_thread {
        format!("heddle pull {remote_thread} {local_thread}")
    } else {
        format!("heddle pull {remote_thread}")
    };
    RecoveryAdvice::safety_refusal(
        "remote_pull_failed",
        format!("Pull failed from {remote_thread}: {error}"),
        format!(
            "Inspect `heddle verify`, then retry with `{primary_command}` after fixing the remote."
        ),
        format!("remote pull from {remote_thread} failed: {error}"),
        "the local thread was not confirmed updated from the remote",
        "local Heddle state, Git refs, and worktree files were left unchanged by the failed pull result",
        primary_command.clone(),
        vec![primary_command, "heddle verify".to_string()],
    )
}

/// Execute remote command.
pub fn cmd_remote(cli: &Cli, command: RemoteCommands) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let start = cli.repo.as_ref().unwrap_or(&cwd);
    match &command {
        RemoteCommands::List => {
            if let Some(probe) = build_plain_git_verification_probe(start)? {
                let items = plain_git_remote_items(&probe.root);
                let default = default_remote_from_items(&items);
                let output = RemoteListOutput {
                    output_kind: "remote_list",
                    remotes: items
                        .into_iter()
                        .map(|(name, url)| {
                            let is_default = default.as_deref() == Some(name.as_str());
                            RemoteInfoOutput {
                                output_kind: None,
                                name,
                                url,
                                source: "git".to_string(),
                                is_default,
                            }
                        })
                        .collect(),
                };
                render_remote_list(&output, should_output_json(cli, None))?;
                return Ok(());
            }
        }
        RemoteCommands::Show { name } => {
            if let Some(probe) = build_plain_git_verification_probe(start)? {
                let items = plain_git_remote_items(&probe.root);
                let default = default_remote_from_items(&items);
                let url = items
                    .get(name)
                    .cloned()
                    .ok_or_else(|| remote_not_found_advice(name))?;
                let output = RemoteInfoOutput {
                    output_kind: Some("remote_show"),
                    name: name.clone(),
                    url,
                    source: "git".to_string(),
                    is_default: default.as_deref() == Some(name.as_str()),
                };
                render_remote_info(&output, should_output_json(cli, None))?;
                return Ok(());
            }
        }
        RemoteCommands::Add { .. }
        | RemoteCommands::Remove { .. }
        | RemoteCommands::SetDefault { .. } => {}
    }

    let repo = Repository::open(start)?;

    match command {
        RemoteCommands::List => {
            let items = merged_remote_items(&repo)?;
            let default = resolved_default_remote_name(&repo)?;
            let output = RemoteListOutput {
                output_kind: "remote_list",
                remotes: items
                    .into_iter()
                    .map(|(name, (url, source))| {
                        let is_default = default.as_deref() == Some(name.as_str());
                        RemoteInfoOutput {
                            output_kind: None,
                            name,
                            url,
                            source,
                            is_default,
                        }
                    })
                    .collect(),
            };
            render_remote_list(&output, should_output_json(cli, Some(repo.config())))?;
            Ok(())
        }
        RemoteCommands::Add { name, url } => {
            super::preflight_native_remote_transport(&repo, Some(&url), "remote add")?;
            let git_overlay_default_before = (repo.capability()
                == RepositoryCapability::GitOverlay)
                .then(|| git_overlay_default_remote_name(&repo))
                .flatten();
            sync_git_overlay_remote_add(&repo, &name, &url)?;
            let mut cfg = RemoteConfig::open(&repo).map_err(anyhow::Error::msg)?;
            let default_was_empty = cfg.default_name().is_none();
            cfg.add(&name, Remote { url: url.clone() })
                .map_err(anyhow::Error::msg)?;
            if default_was_empty
                && git_overlay_default_before
                    .as_deref()
                    .is_some_and(|default| default != name)
            {
                cfg.clear_default().map_err(anyhow::Error::msg)?;
            }
            let default = resolved_default_remote_name(&repo)?;
            render_remote_mutation(
                RemoteMutationOutput {
                    output_kind: "remote_add",
                    status: "completed",
                    action: "remote_add",
                    name,
                    url: Some(url),
                    default,
                    message: "Added remote".to_string(),
                    trust: build_repository_verification_state(&repo),
                },
                should_output_json(cli, Some(repo.config())),
            )?;
            Ok(())
        }
        RemoteCommands::Remove { name } => {
            let mut cfg = RemoteConfig::open(&repo).map_err(anyhow::Error::msg)?;
            cfg.remove(&name).map_err(anyhow::Error::msg)?;
            render_remote_mutation(
                RemoteMutationOutput {
                    output_kind: "remote_remove",
                    status: "completed",
                    action: "remote_remove",
                    name,
                    url: None,
                    default: resolved_default_remote_name(&repo)?,
                    message: "Removed remote".to_string(),
                    trust: build_repository_verification_state(&repo),
                },
                should_output_json(cli, Some(repo.config())),
            )?;
            Ok(())
        }
        RemoteCommands::SetDefault { name } => {
            let mut cfg = RemoteConfig::open(&repo).map_err(anyhow::Error::msg)?;
            cfg.set_default(&name).map_err(anyhow::Error::msg)?;
            render_remote_mutation(
                RemoteMutationOutput {
                    output_kind: "remote_set_default",
                    status: "completed",
                    action: "remote_set_default",
                    name: name.clone(),
                    url: None,
                    default: Some(name),
                    message: "Set default remote".to_string(),
                    trust: build_repository_verification_state(&repo),
                },
                should_output_json(cli, Some(repo.config())),
            )?;
            Ok(())
        }
        RemoteCommands::Show { name } => {
            let items = merged_remote_items(&repo)?;
            let default = resolved_default_remote_name(&repo)?;
            let (url, source) = items
                .get(&name)
                .cloned()
                .ok_or_else(|| remote_not_found_advice(&name))?;
            let is_default = default.as_deref() == Some(name.as_str());
            let output = RemoteInfoOutput {
                output_kind: Some("remote_show"),
                name,
                url,
                source,
                is_default,
            };
            render_remote_info(&output, should_output_json(cli, Some(repo.config())))?;
            Ok(())
        }
    }
}

fn remote_not_found_advice(name: &str) -> RecoveryAdvice {
    RecoveryAdvice::safety_refusal(
        "remote_not_found",
        format!("Remote '{name}' not found"),
        "Inspect configured remotes with `heddle remote list`, or add one with `heddle remote add <name> <url>`.",
        format!("no configured Heddle or Git remote named '{name}' was found"),
        "the command cannot inspect, fetch, pull, or push a remote until the remote name is resolved",
        "remote configuration, refs, objects, and worktree files were left unchanged",
        "heddle remote list",
        vec![
            "heddle remote list".to_string(),
            "heddle remote add <name> <url>".to_string(),
        ],
    )
}

fn render_remote_mutation(output: RemoteMutationOutput, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string(&output)?);
    } else {
        println!(
            "{} {} {}",
            style::ok_marker(),
            output.message.to_lowercase(),
            style::bold(&output.name)
        );
        if !output.trust.recommended_action.is_empty() {
            println!("Next: {}", style::bold(&output.trust.recommended_action));
        }
    }
    Ok(())
}

fn render_remote_list(output: &RemoteListOutput, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string(output)?);
    } else if output.remotes.is_empty() {
        println!("{}", style::dim("No remotes configured"));
        println!("{}", style::field("next", "heddle remote add <name> <url>"));
    } else {
        println!("{}", style::section("Remotes"));
        for item in &output.remotes {
            println!(
                "  {} {} {}",
                style::bold(&item.name),
                style::dim(&item.url),
                style::dim(&format!(
                    "({}{})",
                    item.source,
                    if item.is_default { ", default" } else { "" }
                ))
            );
        }
    }
    Ok(())
}

fn render_remote_info(output: &RemoteInfoOutput, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string(output)?);
    } else {
        println!("{}", style::section("Remote"));
        println!("  {}", style::field("name", &style::bold(&output.name)));
        println!("  {}", style::field("url", &style::dim(&output.url)));
        println!("  {}", style::field("source", &style::dim(&output.source)));
        println!(
            "  {}",
            style::field("default", if output.is_default { "yes" } else { "no" })
        );
    }
    Ok(())
}

pub(crate) fn resolve_default_remote_name(
    repo: &Repository,
    requested: Option<&str>,
) -> Result<String> {
    if let Some(requested) = requested {
        return Ok(requested.to_string());
    }
    if let Some(default) = RemoteConfig::open(repo)
        .map_err(anyhow::Error::msg)?
        .default_name()
    {
        return Ok(default.to_string());
    }
    if repo.capability() == RepositoryCapability::GitOverlay {
        if let Some(default) = git_overlay_default_remote_name(repo) {
            return Ok(default);
        }
    }
    Ok("origin".to_string())
}

pub(crate) fn resolved_default_remote_name(repo: &Repository) -> Result<Option<String>> {
    let cfg = RemoteConfig::open(repo).map_err(anyhow::Error::msg)?;
    if let Some(default) = cfg.default_name() {
        return Ok(Some(default.to_string()));
    }
    if repo.capability() == RepositoryCapability::GitOverlay {
        return Ok(git_overlay_default_remote_name(repo));
    }
    Ok(None)
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
    let git = gix::discover(repo.root()).ok()?;
    let local = git
        .find_reference(format!("refs/heads/{branch}").as_str())
        .ok()?;
    local
        .remote_name(gix::remote::Direction::Fetch)
        .and_then(|name| name.as_symbol().map(str::to_string))
        .filter(|remote| !remote.is_empty())
}

fn merged_remote_items(repo: &Repository) -> Result<BTreeMap<String, (String, String)>> {
    let cfg = RemoteConfig::open(repo).map_err(anyhow::Error::msg)?;
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

fn configured_remote_source(repo: &Repository, url: &str) -> &'static str {
    if repo.capability() == RepositoryCapability::GitOverlay
        && local_remote_path(url).is_some_and(|path| is_local_git_repository(&path))
    {
        "git-overlay"
    } else {
        "heddle"
    }
}

fn local_remote_path(url: &str) -> Option<std::path::PathBuf> {
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

fn plain_git_remote_items(root: &Path) -> BTreeMap<String, String> {
    let mut remotes = BTreeMap::new();
    for config_path in plain_git_config_paths(root) {
        parse_git_config_remotes(&config_path, &mut remotes);
    }
    remotes
}

fn plain_git_config_paths(root: &Path) -> Vec<std::path::PathBuf> {
    let mut paths = Vec::new();
    let dot_git = root.join(".git");
    paths.push(dot_git.join("config"));
    if let Some(git_dir) = pointed_git_dir(&dot_git) {
        paths.push(git_dir.join("config"));
        if let Some(common_dir) = common_git_dir(&git_dir) {
            paths.push(common_dir.join("config"));
        }
    }
    paths
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

fn sync_git_overlay_remote_add(repo: &Repository, name: &str, url: &str) -> Result<()> {
    if repo.capability() != RepositoryCapability::GitOverlay {
        return Ok(());
    }
    validate_git_overlay_remote_name(name)?;
    let config_path = git_overlay_config_path_for_write(repo)
        .context("Git-overlay remote add requires a writable Git config")?;
    upsert_git_remote_config(&config_path, name, url)
}

fn git_overlay_config_path_for_write(repo: &Repository) -> Option<std::path::PathBuf> {
    let dot_git = repo.root().join(".git");
    if dot_git.is_dir() {
        return Some(dot_git.join("config"));
    }
    let git_dir = pointed_git_dir(&dot_git)?;
    Some(common_git_dir(&git_dir).unwrap_or(git_dir).join("config"))
}

fn validate_git_overlay_remote_name(name: &str) -> Result<()> {
    if name.trim().is_empty()
        || name.starts_with('-')
        || name.bytes().any(|byte| byte < 0x20 || byte == 0x7f)
        || name
            .chars()
            .any(|ch| matches!(ch, ' ' | '~' | '^' | ':' | '?' | '*' | '[' | '\\'))
        || name.contains("..")
        || name.contains("//")
        || name.starts_with('/')
        || name.ends_with('/')
        || name.starts_with('.')
        || name.ends_with(".lock")
    {
        anyhow::bail!("invalid Git remote name for Git-overlay repository: {name}");
    }
    Ok(())
}

fn upsert_git_remote_config(config_path: &Path, name: &str, url: &str) -> Result<()> {
    if let Some(parent) = config_path.parent() {
        fs::create_dir_all(parent)?;
    }
    let original = fs::read_to_string(config_path).unwrap_or_default();
    let mut rewritten = String::new();
    let mut skipping_remote = false;
    for line in original.lines() {
        if let Some(section_name) = parse_git_remote_section_name(line) {
            skipping_remote = section_name == name;
            if skipping_remote {
                continue;
            }
        } else if line.trim_start().starts_with('[') && line.trim_end().ends_with(']') {
            skipping_remote = false;
        }
        if !skipping_remote {
            rewritten.push_str(line);
            rewritten.push('\n');
        }
    }
    if !rewritten.is_empty() && !rewritten.ends_with("\n\n") {
        rewritten.push('\n');
    }
    rewritten.push_str(&format!(
        "[remote \"{}\"]\n\turl = {}\n\tfetch = {}\n",
        git_config_quoted_section(name),
        git_config_quoted_value(url),
        git_config_quoted_value(&format!("+refs/heads/*:refs/remotes/{name}/*"))
    ));
    write_file_atomic(config_path, rewritten.as_bytes())?;
    Ok(())
}

fn parse_git_remote_section_name(line: &str) -> Option<String> {
    let trimmed = line.trim();
    let inner = trimmed.strip_prefix("[remote \"")?.strip_suffix("\"]")?;
    unescape_git_config_string(inner)
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

fn git_config_quoted_section(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

fn git_config_quoted_value(value: &str) -> String {
    let mut quoted = String::from("\"");
    for ch in value.chars() {
        match ch {
            '\\' => quoted.push_str("\\\\"),
            '"' => quoted.push_str("\\\""),
            '\n' => quoted.push_str("\\n"),
            '\t' => quoted.push_str("\\t"),
            '\r' => quoted.push_str("\\r"),
            '\u{0008}' => quoted.push_str("\\b"),
            ch => quoted.push(ch),
        }
    }
    quoted.push('"');
    quoted
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
