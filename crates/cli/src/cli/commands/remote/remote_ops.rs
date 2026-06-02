// SPDX-License-Identifier: Apache-2.0
//! Pull, remote management, and serve commands.

use objects::store::ObjectStore;
#[cfg(feature = "client")]
use std::net::SocketAddr;
use std::{borrow::Cow, collections::BTreeMap, fs, path::Path};

use anyhow::{Context, Result};
use gix::bstr::{BStr, BString};
#[cfg(feature = "client")]
use heddle_client::grpc_hosted::PullMaterialization;
use objects::{
    fs_atomic::write_file_atomic,
    object::{ChangeId, ThreadName, Tree},
};
#[cfg(feature = "client")]
use proto::AuthToken;
use refs::Head;
use repo::{Repository, RepositoryCapability};
use serde::Serialize;

use super::super::{
    action_line::print_next,
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
    bridge::{GitBridge, git_core::GitPullOutcome},
    cli::{Cli, RemoteCommands, should_output_json, style},
    client::LocalSync,
    config::UserConfig,
    remote::{Remote, RemoteConfig, RemoteError, RemoteTarget, resolve_remote_with_key},
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
    #[allow(dead_code)]
    #[serde(skip_serializing)]
    #[serde(rename = "verification")]
    trust: RepositoryVerificationState,
}

#[derive(Serialize)]
struct PullOutput {
    output_kind: &'static str,
    action: &'static str,
    status: &'static str,
    success: bool,
    pulled: bool,
    changed: bool,
    transport: &'static str,
    remote: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    branch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    old_git_head: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    new_git_head: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    old_state: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    new_state: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    states_created: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    commits_seen: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    commits_seen_scope: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    materialized_checkout: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    changed_path_count: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    changed_paths: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    thread: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    state: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    objects: Option<usize>,
    #[allow(dead_code)]
    #[serde(skip_serializing)]
    #[serde(rename = "verification")]
    trust: RepositoryVerificationState,
}

struct GitOverlayPullOutputInput {
    remote: String,
    branch: Option<String>,
    old_git_head: Option<String>,
    new_git_head: Option<String>,
    old_state: Option<ChangeId>,
    new_state: Option<ChangeId>,
    changed_paths: Vec<String>,
    outcome: GitPullOutcome,
    trust: RepositoryVerificationState,
}

fn git_overlay_pull_output(input: GitOverlayPullOutputInput) -> PullOutput {
    PullOutput {
        output_kind: "pull",
        action: "pull",
        status: pull_status(input.outcome.changed),
        success: true,
        pulled: input.outcome.changed,
        changed: input.outcome.changed,
        transport: "git",
        remote: input.remote,
        branch: input.branch,
        old_git_head: input.old_git_head,
        new_git_head: input.new_git_head,
        old_state: input.old_state.map(|state| state.to_string()),
        new_state: input.new_state.map(|state| state.to_string()),
        states_created: Some(input.outcome.states_created),
        commits_seen: Some(input.outcome.commits_seen),
        commits_seen_scope: Some("branches_and_heddle_notes"),
        materialized_checkout: Some(input.outcome.materialized_checkout),
        changed_path_count: Some(input.changed_paths.len()),
        changed_paths: Some(input.changed_paths),
        thread: None,
        state: None,
        objects: None,
        trust: input.trust,
    }
}

fn heddle_pull_output(
    changed: bool,
    remote: String,
    thread: String,
    state: Option<String>,
    objects: Option<usize>,
    trust: RepositoryVerificationState,
) -> PullOutput {
    PullOutput {
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
        trust,
    }
}

fn pull_status(changed: bool) -> &'static str {
    if changed { "updated" } else { "up_to_date" }
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
    if remote.is_none() && resolved_default_remote_name(&repo)?.is_none() {
        return Err(anyhow::anyhow!(RecoveryAdvice::remote_not_configured("pull")));
    }
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
        if should_output_json(cli, Some(repo.config())) {
            let output = git_overlay_pull_output(GitOverlayPullOutputInput {
                remote: remote_name,
                branch,
                old_git_head,
                new_git_head,
                old_state,
                new_state,
                changed_paths,
                outcome,
                trust: verification,
            });
            crate::cli::render::write_json_stdout(&output)?;
        } else {
            if outcome.changed {
                println!(
                    "{} pulled from {}",
                    style::ok_marker(),
                    style::bold(&remote_name)
                );
            } else {
                println!(
                    "{} already up to date with {}; repository verification checked below",
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
                    print_next(&verification.recommended_action);
                }
            } else {
                println!("Workspace: verified");
            }
        }
        return Ok(());
    }

    super::preflight_native_remote_transport(&repo, remote.as_deref(), "pull")?;

    let user_config = UserConfig::load_default()?;
    #[cfg(feature = "client")]
    let mut token = user_config.remote_token()?;
    #[cfg(not(feature = "client"))]
    let token = user_config.remote_token()?;
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
        Head::Attached { thread } => local_thread_name.is_none_or(|local| thread == local),
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
            anyhow::bail!(RecoveryAdvice::network_feature_unavailable("pull"));
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
        return Err(anyhow::anyhow!(
            RecoveryAdvice::local_lazy_pull_unsupported(source_path)
        ));
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
        .get_thread(&ThreadName::new(remote_thread))?
        .context(format!("Thread {} not found in source", remote_thread))?;

    let objects_copied = source.fetch_state(repo, &state_id)?;

    let track_to_update = local_thread.unwrap_or(remote_thread);
    let track_tn = ThreadName::new(track_to_update);

    // Capture the local thread's pre-pull tip *before* mutating it
    // (heddle#110). The materializing branch records an
    // `OpRecord::FastForwardV2` whose `pre_target_id` must be the
    // pre-pull state so undo restores both HEAD and the local thread
    // ref. Reading the ref after `set_thread` would return the
    // post-pull state and silently strand the thread on undo —
    // exactly the bug we're closing.
    let pre_target = repo.refs().get_thread(&track_tn)?;
    let changed = pre_target.as_ref() != Some(&state_id) || objects_copied > 0;
    repo.refs().set_thread(&track_tn, &state_id)?;

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
        let output = heddle_pull_output(
            changed,
            source_path.display().to_string(),
            track_to_update.to_string(),
            Some(state_id.to_string()),
            Some(objects_copied),
            build_repository_verification_state(repo),
        );
        crate::cli::render::write_json_stdout(&output)?;
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

#[cfg(feature = "client")]
async fn pull_network(repo: &Repository, options: PullNetworkOptions<'_>) -> Result<()> {
    let repo_path = options
        .repo_path
        .context("network remotes must include a hosted repository path")?;
    let mut config = options.user_config.heddle_client_config(options.token)?;
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
            let output = heddle_pull_output(
                changed,
                options.remote_thread.to_string(),
                options
                    .local_thread
                    .unwrap_or(options.remote_thread)
                    .to_string(),
                result.final_state.map(|state| state.to_string()),
                None,
                build_repository_verification_state(repo),
            );
            crate::cli::render::write_json_stdout(&output)?;
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
        return Err(anyhow::anyhow!(RecoveryAdvice::remote_pull_failed(
            options.remote_thread,
            options.local_thread,
            &err,
        )));
    }

    Ok(())
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
                    .ok_or_else(|| RecoveryAdvice::remote_not_found(name))?;
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
            if !merged_remote_items(&repo)?.contains_key(&name) {
                return Err(RecoveryAdvice::remote_not_found(&name).into());
            }
            // Remove the git-overlay side FIRST so its uneditable-include
            // refusal (raised before any file is touched) leaves the Heddle
            // config unmutated. Persisting the Heddle removal ahead of this
            // fallible step stranded the repo in partial state: the Heddle
            // remote gone, the Git remote still present.
            sync_git_overlay_remote_remove(&repo, &name)?;
            let mut cfg = RemoteConfig::open(&repo).map_err(anyhow::Error::msg)?;
            match cfg.remove(&name) {
                Ok(()) | Err(RemoteError::NotFound(_)) => {}
                Err(err) => return Err(anyhow::Error::msg(err)),
            }
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
            let items = merged_remote_items(&repo)?;
            let (url, _source) = items
                .get(&name)
                .cloned()
                .ok_or_else(|| RecoveryAdvice::remote_not_found(&name))?;
            let mut cfg = RemoteConfig::open(&repo).map_err(anyhow::Error::msg)?;
            // Git-overlay remotes added via `git remote add` only live in
            // `.git/config`. `merged_remote_items` surfaces them in
            // `remote list/show`, but `RemoteConfig::set_default` would
            // reject them as NotFound. Adopt the URL into
            // `.heddle/remotes.toml` first so `default_name()`-driven
            // readers (including `resolve_remote_with_key`) can resolve
            // it, then set the default explicitly.
            if cfg.get(&name).is_err() {
                cfg.add(&name, Remote { url })
                    .map_err(anyhow::Error::msg)?;
            }
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
                .ok_or_else(|| RecoveryAdvice::remote_not_found(&name))?;
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
            print_next(&output.trust.recommended_action);
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
    if repo.capability() == RepositoryCapability::GitOverlay
        && let Some(default) = git_overlay_default_remote_name(repo) {
            return Ok(default);
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

fn git_overlay_config_remotes(repo: &Repository) -> BTreeMap<String, String> {
    let Some(ctx) = GitConfigContext::discover(repo.root()) else {
        return BTreeMap::new();
    };
    let mut paths = ctx.layered_paths();
    paths.push(repo.heddle_dir().join("git").join("config"));
    ctx.remotes(paths)
}

/// The resolved Git directory layout for a repository, used to read remote
/// definitions from `.git/config` (and its layered companions) through
/// `gix_config`, which correctly handles quoting, inline comments, include
/// directives, and conditional `includeIf` directives.
struct GitConfigContext {
    git_dir: std::path::PathBuf,
    common_dir: std::path::PathBuf,
    branch: Option<gix::refs::FullName>,
}

impl GitConfigContext {
    fn discover(root: &Path) -> Option<Self> {
        let git = gix::discover(root).ok()?;
        Some(Self {
            git_dir: git.git_dir().to_path_buf(),
            common_dir: git.common_dir().to_path_buf(),
            branch: git.head_name().ok().flatten(),
        })
    }

    fn branch_ref(&self) -> Option<&gix::refs::FullNameRef> {
        self.branch.as_ref().map(AsRef::as_ref)
    }

    /// The standard repository config files, ordered highest-precedence first:
    /// the per-worktree `config.worktree` (only when `extensions.worktreeConfig`
    /// is enabled), then the git-dir `config`, then the shared common-dir
    /// `config` for linked worktrees.
    fn layered_paths(&self) -> Vec<std::path::PathBuf> {
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
            .and_then(|file| file.boolean("extensions.worktreeConfig"))
            .and_then(Result::ok)
            .unwrap_or(false)
    }

    /// The file a write to remote `name` must target so the next
    /// `remote list` read resolves the value we just wrote. The
    /// highest-precedence file that already defines the remote, resolved
    /// through `include.path`/`includeIf` indirection — not merely the
    /// top-level layer that *follows* the include, whose physical text has
    /// no `[remote]` section to edit. When no file defines the remote, the
    /// common config — git's default target for a brand-new remote.
    ///
    /// Errors when the defining file lies outside the repository's Git
    /// directory (reached via an include), so a reported-successful write is
    /// never a silent no-op against a file heddle won't edit.
    fn write_file_for(&self, name: &str) -> Result<std::path::PathBuf> {
        match self.defining_files_for(name).into_iter().next() {
            Some(path) => {
                if !self.owns_config_file(&path) {
                    anyhow::bail!(RecoveryAdvice::git_remote_in_included_config(name, &path));
                }
                Ok(path)
            }
            None => Ok(self.common_dir.join("config")),
        }
    }

    /// Every file that currently defines remote `name`, resolved through
    /// includes. A remove must clear all of them, otherwise a
    /// lower-precedence definition resurfaces — or a higher-precedence one
    /// keeps winning — on the next read, leaving the "successful" removal
    /// silently divergent. Errors when any defining file lies outside the
    /// repository's Git directory rather than no-op'ing against it.
    fn remove_files_for(&self, name: &str) -> Result<Vec<std::path::PathBuf>> {
        let files = self.defining_files_for(name);
        for path in &files {
            if !self.owns_config_file(path) {
                anyhow::bail!(RecoveryAdvice::git_remote_in_included_config(name, path));
            }
        }
        Ok(files)
    }

    /// The file(s) whose `[remote "<name>"]` section the reader resolves,
    /// following `include.path`/`includeIf`. Returned highest-precedence
    /// first, matching `remotes` read precedence (first-seen wins). The
    /// section metadata records the file each section physically lives in,
    /// so an include-defined remote resolves to the included file — the one
    /// a write must edit — not the including config.
    fn defining_files_for(&self, name: &str) -> Vec<std::path::PathBuf> {
        let Some(file) = self.load(self.layered_paths()) else {
            return Vec::new();
        };
        let Some(sections) = file.sections_by_name("remote") else {
            return Vec::new();
        };
        let mut files = Vec::new();
        for section in sections {
            let matches = section
                .header()
                .subsection_name()
                .map(|subsection| subsection.to_string());
            if matches.as_deref() != Some(name) {
                continue;
            }
            let Some(path) = section.meta().path.clone() else {
                continue;
            };
            if !files.contains(&path) {
                files.push(path);
            }
        }
        files
    }

    /// Whether heddle may rewrite `path`: only config files within the
    /// repository's own Git directory tree (git-dir / common-dir). A section
    /// pulled in from a file outside that tree via `include.path`/`includeIf`
    /// (e.g. a user-global config) is not ours to edit.
    fn owns_config_file(&self, path: &Path) -> bool {
        let target = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        [&self.git_dir, &self.common_dir].into_iter().any(|root| {
            let root = root.canonicalize().unwrap_or_else(|_| root.clone());
            target.starts_with(&root)
        })
    }

    fn remotes(&self, paths: Vec<std::path::PathBuf>) -> BTreeMap<String, String> {
        let mut remotes = BTreeMap::new();
        let Some(file) = self.load(paths) else {
            return remotes;
        };
        let Some(sections) = file.sections_by_name("remote") else {
            return remotes;
        };
        for section in sections {
            let Some(name) = section.header().subsection_name() else {
                continue;
            };
            let Some(url) = section.value("url") else {
                continue;
            };
            remotes
                .entry(name.to_string())
                .or_insert_with(|| url.to_string());
        }
        remotes
    }

    fn load(&self, paths: Vec<std::path::PathBuf>) -> Option<gix_config::File<'static>> {
        let options = gix_config::file::init::Options {
            includes: gix_config::file::includes::Options::follow(
                gix_config::path::interpolate::Context::default(),
                gix_config::file::includes::conditional::Context {
                    git_dir: Some(&self.git_dir),
                    branch_name: self.branch_ref(),
                },
            ),
            lossy: true,
            ignore_io_errors: true,
        };
        let mut metadata = paths
            .into_iter()
            .map(|path| gix_config::file::Metadata::from(gix_config::Source::Local).at(path));
        let mut buf = Vec::new();
        gix_config::File::from_paths_metadata_buf(&mut metadata, &mut buf, false, options)
            .ok()
            .flatten()
    }
}

fn sync_git_overlay_remote_add(repo: &Repository, name: &str, url: &str) -> Result<()> {
    if repo.capability() != RepositoryCapability::GitOverlay {
        return Ok(());
    }
    validate_git_overlay_remote_name(name)?;
    let ctx = GitConfigContext::discover(repo.root())
        .context("Git-overlay remote add requires a writable Git config")?;
    upsert_git_remote_config(&ctx.write_file_for(name)?, name, url)
}

fn sync_git_overlay_remote_remove(repo: &Repository, name: &str) -> Result<()> {
    if repo.capability() != RepositoryCapability::GitOverlay {
        return Ok(());
    }
    let Some(ctx) = GitConfigContext::discover(repo.root()) else {
        return Ok(());
    };
    for config_path in ctx.remove_files_for(name)? {
        remove_git_remote_config(&config_path, name)?;
    }
    Ok(())
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
        anyhow::bail!(RecoveryAdvice::git_remote_name_invalid(name));
    }
    Ok(())
}

/// Add or replace the `[remote "<name>"]` section in a single physical config
/// file via `gix_config`'s structured editing, so the writer resolves the same
/// section the reader does regardless of the surface header form (quoted
/// `[remote "x"]`, legacy dotted `[remote.x]`, or comment-suffixed). Every
/// existing definition of the remote is dropped before a fresh canonical
/// section is appended, so an upsert replaces rather than appends a duplicate
/// that the first-seen (stale) section would win over on the next read.
fn upsert_git_remote_config(config_path: &Path, name: &str, url: &str) -> Result<()> {
    if let Some(parent) = config_path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut file = load_config_file_for_edit(config_path)?;
    remove_remote_sections(&mut file, name);
    let mut section = file
        .new_section("remote", Some(Cow::Owned(BString::from(name))))
        .with_context(|| format!("invalid git remote section name '{name}'"))?;
    section.push(git_config_key("url")?, Some(BStr::new(url)));
    let fetch = format!("+refs/heads/*:refs/remotes/{name}/*");
    section.push(git_config_key("fetch")?, Some(BStr::new(fetch.as_str())));
    let serialized = file.to_bstring();
    write_file_atomic(config_path, &serialized)?;
    Ok(())
}

/// Remove every `[remote "<name>"]` section from a single physical config file
/// via `gix_config`, matching whatever header form the reader resolves the
/// remote through. No-ops (no write) when the file is absent or defines no such
/// remote.
fn remove_git_remote_config(config_path: &Path, name: &str) -> Result<()> {
    if !config_path.exists() {
        return Ok(());
    }
    let mut file = load_config_file_for_edit(config_path)?;
    if !remove_remote_sections(&mut file, name) {
        return Ok(());
    }
    let serialized = file.to_bstring();
    write_file_atomic(config_path, &serialized)?;
    Ok(())
}

/// Drop every `[remote "<name>"]` section from `file`, returning whether any
/// was removed. `gix_config` keys sections by parsed name + subsection, so this
/// matches the remote regardless of its surface header syntax. `remove_section`
/// removes only the last match, so loop until none remain.
fn remove_remote_sections(file: &mut gix_config::File<'static>, name: &str) -> bool {
    let mut removed = false;
    while file
        .remove_section("remote", Some(BStr::new(name)))
        .is_some()
    {
        removed = true;
    }
    removed
}

/// Load a single physical config file for in-place editing. Includes are NOT
/// followed: the caller already resolved the physical file that defines the
/// remote (see `defining_files_for`), and a write must round-trip that file
/// alone rather than inline the content of any files it includes. A missing
/// file yields an empty document so a brand-new remote can be appended.
fn load_config_file_for_edit(config_path: &Path) -> Result<gix_config::File<'static>> {
    if !config_path.exists() {
        return Ok(gix_config::File::default());
    }
    gix_config::File::from_path_no_includes(config_path.to_path_buf(), gix_config::Source::Local)
        .with_context(|| format!("reading git config at {}", config_path.display()))
}

fn git_config_key(key: &'static str) -> Result<gix_config::parse::section::ValueName<'static>> {
    gix_config::parse::section::ValueName::try_from(key)
        .with_context(|| format!("invalid git config key '{key}'"))
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

#[cfg(test)]
mod tests {
    use super::*;

    fn init_git(root: &Path) {
        gix::init(root).expect("init git repo");
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
        fs::write(
            git_dir.join("config"),
            "[include]\n\tpath = extra.config\n",
        )
        .unwrap();

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
    fn remove_clears_worktree_layer_when_extension_enabled() {
        let tmp = tempfile::TempDir::new().unwrap();
        init_git(tmp.path());
        let git_dir = tmp.path().join(".git");
        fs::write(
            git_dir.join("config"),
            "[extensions]\n\tworktreeConfig = true\n\
             [remote \"origin\"]\n\turl = https://example.com/common\n",
        )
        .unwrap();
        fs::write(
            git_dir.join("config.worktree"),
            "[remote \"origin\"]\n\turl = https://example.com/worktree\n",
        )
        .unwrap();

        let ctx = GitConfigContext::discover(tmp.path()).unwrap();
        for path in ctx.remove_files_for("origin").unwrap() {
            remove_git_remote_config(&path, "origin").unwrap();
        }

        // The visible (per-worktree) remote must be gone after a remove;
        // a common-only removal would leave it winning on the next read.
        assert!(!plain_git_remote_items(tmp.path()).contains_key("origin"));
    }

    #[test]
    fn add_targets_worktree_layer_so_next_read_reflects_it() {
        let tmp = tempfile::TempDir::new().unwrap();
        init_git(tmp.path());
        let git_dir = tmp.path().join(".git");
        fs::write(
            git_dir.join("config"),
            "[extensions]\n\tworktreeConfig = true\n",
        )
        .unwrap();
        fs::write(
            git_dir.join("config.worktree"),
            "[remote \"origin\"]\n\turl = https://example.com/old\n",
        )
        .unwrap();

        let ctx = GitConfigContext::discover(tmp.path()).unwrap();
        upsert_git_remote_config(
            &ctx.write_file_for("origin").unwrap(),
            "origin",
            "https://example.com/new",
        )
        .unwrap();

        // The upsert must hit the per-worktree layer (where the remote
        // lives and wins on read); writing to common would leave the
        // stale per-worktree url winning, a silent read/write divergence.
        assert_eq!(
            plain_git_remote_items(tmp.path()).get("origin").map(String::as_str),
            Some("https://example.com/new"),
        );
    }

    #[test]
    fn remove_clears_remote_defined_via_include_path() {
        let tmp = tempfile::TempDir::new().unwrap();
        init_git(tmp.path());
        let git_dir = tmp.path().join(".git");
        fs::write(
            git_dir.join("extra.config"),
            "[remote \"upstream\"]\n\turl = https://example.com/upstream\n",
        )
        .unwrap();
        fs::write(git_dir.join("config"), "[include]\n\tpath = extra.config\n").unwrap();

        // The reader follows the include, so the remote is visible...
        assert!(plain_git_remote_items(tmp.path()).contains_key("upstream"));

        let ctx = GitConfigContext::discover(tmp.path()).unwrap();
        for path in ctx.remove_files_for("upstream").unwrap() {
            remove_git_remote_config(&path, "upstream").unwrap();
        }

        // ...and a remove must clear the section from the *included* file
        // it actually lives in, not no-op against the including config.
        assert!(!plain_git_remote_items(tmp.path()).contains_key("upstream"));
    }

    #[test]
    fn write_to_included_remote_targets_the_defining_file() {
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
        upsert_git_remote_config(&target, "origin", "https://example.com/new").unwrap();

        assert_eq!(
            plain_git_remote_items(tmp.path())
                .get("origin")
                .map(String::as_str),
            Some("https://example.com/new"),
        );
    }

    #[test]
    fn write_to_remote_in_external_include_errors_rather_than_no_ops() {
        let tmp = tempfile::TempDir::new().unwrap();
        init_git(tmp.path());
        let git_dir = tmp.path().join(".git");
        // An included config that lives *outside* the repository's Git tree.
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
    fn add_new_remote_targets_common_layer() {
        let tmp = tempfile::TempDir::new().unwrap();
        init_git(tmp.path());
        let git_dir = tmp.path().join(".git");
        fs::write(
            git_dir.join("config"),
            "[extensions]\n\tworktreeConfig = true\n",
        )
        .unwrap();

        let ctx = GitConfigContext::discover(tmp.path()).unwrap();
        // A brand-new remote (no layer defines it yet) follows git's
        // default: the common config.
        assert_eq!(ctx.write_file_for("origin").unwrap(), git_dir.join("config"));
        upsert_git_remote_config(
            &ctx.write_file_for("origin").unwrap(),
            "origin",
            "https://example.com/new",
        )
        .unwrap();
        assert_eq!(
            plain_git_remote_items(tmp.path()).get("origin").map(String::as_str),
            Some("https://example.com/new"),
        );
    }

    #[test]
    fn remove_clears_comment_suffixed_remote_header() {
        let tmp = tempfile::TempDir::new().unwrap();
        init_git(tmp.path());
        let git_dir = tmp.path().join(".git");
        // A valid Git header gix accepts but the hand-rolled writer didn't:
        // an inline comment trails the `[remote "origin"]` header.
        fs::write(
            git_dir.join("config"),
            "[remote \"origin\"] # primary mirror\n\turl = https://example.com/repo\n",
        )
        .unwrap();

        // The reader resolves it, so it shows up in `remote list`...
        assert!(plain_git_remote_items(tmp.path()).contains_key("origin"));

        let ctx = GitConfigContext::discover(tmp.path()).unwrap();
        for path in ctx.remove_files_for("origin").unwrap() {
            remove_git_remote_config(&path, "origin").unwrap();
        }

        // ...so a remove must actually clear it, not silently no-op against a
        // header form the writer can't parse.
        assert!(!plain_git_remote_items(tmp.path()).contains_key("origin"));
    }

    #[test]
    fn remove_clears_dotted_remote_header() {
        let tmp = tempfile::TempDir::new().unwrap();
        init_git(tmp.path());
        let git_dir = tmp.path().join(".git");
        // The legacy dotted subsection form, equally valid to gix.
        fs::write(
            git_dir.join("config"),
            "[remote.origin]\n\turl = https://example.com/repo\n",
        )
        .unwrap();

        assert!(plain_git_remote_items(tmp.path()).contains_key("origin"));

        let ctx = GitConfigContext::discover(tmp.path()).unwrap();
        for path in ctx.remove_files_for("origin").unwrap() {
            remove_git_remote_config(&path, "origin").unwrap();
        }

        assert!(!plain_git_remote_items(tmp.path()).contains_key("origin"));
    }

    #[test]
    fn upsert_replaces_comment_suffixed_remote_header_without_duplicating() {
        let tmp = tempfile::TempDir::new().unwrap();
        init_git(tmp.path());
        let git_dir = tmp.path().join(".git");
        fs::write(
            git_dir.join("config"),
            "[remote \"origin\"] # primary mirror\n\turl = https://example.com/old\n",
        )
        .unwrap();

        let ctx = GitConfigContext::discover(tmp.path()).unwrap();
        upsert_git_remote_config(
            &ctx.write_file_for("origin").unwrap(),
            "origin",
            "https://example.com/new",
        )
        .unwrap();

        // The upsert must update the existing section, not append a second
        // `[remote "origin"]` the first-seen (stale) section wins over on read.
        assert_eq!(
            plain_git_remote_items(tmp.path())
                .get("origin")
                .map(String::as_str),
            Some("https://example.com/new"),
        );
    }

    #[test]
    fn upsert_replaces_dotted_remote_header() {
        let tmp = tempfile::TempDir::new().unwrap();
        init_git(tmp.path());
        let git_dir = tmp.path().join(".git");
        fs::write(
            git_dir.join("config"),
            "[remote.origin]\n\turl = https://example.com/old\n",
        )
        .unwrap();

        let ctx = GitConfigContext::discover(tmp.path()).unwrap();
        upsert_git_remote_config(
            &ctx.write_file_for("origin").unwrap(),
            "origin",
            "https://example.com/new",
        )
        .unwrap();

        assert_eq!(
            plain_git_remote_items(tmp.path())
                .get("origin")
                .map(String::as_str),
            Some("https://example.com/new"),
        );
    }
}
