// SPDX-License-Identifier: Apache-2.0
//! Pull, remote management, and serve commands.

#[cfg(feature = "client")]
use std::net::SocketAddr;
use std::path::Path;

use anyhow::{Context, Result};
#[cfg(feature = "client")]
use heddle_client::grpc_hosted::{HostedAuthMode, PullMaterialization};
use heddle_core::{
    GitConfigContext, RemoteInfo, RemoteListReport, list_plain_git_remotes, list_remotes,
    merged_remote_items, show_plain_git_remote, show_remote,
};
// Re-export under the historical crate-local names for sibling modules.
pub(crate) use heddle_core::{resolve_default_remote_name, resolved_default_remote_name};
use objects::{
    object::{ChangeId, ThreadName, Tree},
    store::ObjectStore,
};
use refs::Head;
use repo::{Repository, RepositoryCapability};
use serde::Serialize;
use sley::{ConfigEdit, ConfigEditPlan, RemoteConfigSet, Repository as SleyRepository};

use super::super::{
    action_line::print_next,
    advice::RecoveryAdvice,
    verification_health::{
        RepositoryVerificationState, build_plain_git_verification_probe,
        build_repository_verification_state,
    },
    worktree_safety::ensure_worktree_clean,
};
#[cfg(feature = "client")]
use crate::client::HostedGrpcClient;
use crate::{
    cli::{Cli, RemoteCommands, should_output_json, style},
    client::LocalSync,
    config::UserConfig,
    git_projection_engine::{GitProjection, git_core::GitPullOutcome},
    remote::{Remote, RemoteConfig, RemoteError, RemoteTarget, resolve_remote_with_key},
};

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
    insecure: bool,
) -> Result<()> {
    let repo = cli.open_repo()?;
    if remote.is_none() && resolved_default_remote_name(&repo)?.is_none() {
        return Err(anyhow::anyhow!(RecoveryAdvice::remote_not_configured(
            "pull"
        )));
    }
    let pull_uses_hosted_network = super::push_target_is_hosted_network(&repo, remote.as_deref());
    if repo.capability() == RepositoryCapability::GitOverlay
        && !repo.hosted_enabled()
        && !pull_uses_hosted_network
    {
        ensure_worktree_clean(&repo, "pull")?;
        let remote_name = resolve_default_remote_name(&repo, remote.as_deref())?;
        let branch = repo.git_overlay_current_branch()?;
        let old_git_head = git_checkout_head_oid(repo.root());
        let old_state = repo.head()?;
        let mut bridge = GitProjection::new(&repo);
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
    #[cfg(not(feature = "client"))]
    let token = user_config.remote_token()?;
    #[cfg(feature = "client")]
    let (target, server_key) = resolve_remote_with_key(&repo, remote.as_deref())?;
    #[cfg(not(feature = "client"))]
    let (target, _server_key) = resolve_remote_with_key(&repo, remote.as_deref())?;

    let head_ref = repo.head_ref()?;
    let remote_thread = default_pull_thread_name(thread, repo.capability(), &head_ref);
    let local_thread_name = local_thread.as_deref();
    let should_materialize = match &head_ref {
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
                    server_key,
                    remote_thread: &remote_thread,
                    local_thread: local_thread_name,
                    lazy,
                    insecure: insecure
                        || cli_shared::remote_allows_insecure(&repo, remote.as_deref()),
                    cli,
                },
            )
            .await?;
            #[cfg(not(feature = "client"))]
            let _ = (addr, repo_path, token, insecure);
            #[cfg(not(feature = "client"))]
            anyhow::bail!(RecoveryAdvice::network_feature_unavailable("pull"));
        }
    }

    Ok(())
}

fn default_pull_thread_name(
    explicit_thread: Option<String>,
    capability: RepositoryCapability,
    head_ref: &Head,
) -> String {
    if let Some(thread) = explicit_thread {
        return thread;
    }

    if capability == RepositoryCapability::GitOverlay
        && let Head::Attached { thread } = head_ref
    {
        return thread.to_string();
    }

    "main".to_string()
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

    let pre_target = repo.refs().get_thread(&track_tn)?;
    let changed = pre_target.as_ref() != Some(&state_id) || objects_copied > 0;

    // Preserve attached-HEAD semantics only when the pull target is the
    // current checkout. Pulling a remote into a side thread must not move
    // the operator's active thread or overwrite its worktree.
    let head_ref = repo.head_ref()?;
    let should_materialize = match &head_ref {
        Head::Attached { thread } => thread == track_to_update,
        Head::Detached { .. } => local_thread.is_none(),
    };
    if should_materialize {
        // A dirty-refusal must NEVER leave a ref advanced without its
        // corresponding worktree materialization. Run the refuse-able
        // apply before publishing `track_tn`; `fast_forward_attached*`
        // publishes the attached current thread only after the worktree
        // apply succeeds, and the detached arm publishes `track_tn`
        // explicitly below after the same guard has passed.
        match (&head_ref, pre_target) {
            (Head::Attached { .. }, Some(_)) => {
                super::super::ff_record::record_ff_advance(repo, remote_thread, &state_id)?;
            }
            (Head::Attached { .. }, None) => {
                repo.fast_forward_attached_from_materialized_state(&state_id, None)?;
            }
            (Head::Detached { .. }, _) => {
                repo.goto(&state_id)?;
                repo.refs().set_thread(&track_tn, &state_id)?;
            }
        }
    } else {
        repo.refs().set_thread(&track_tn, &state_id)?;
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
    let git = SleyRepository::discover(root).ok()?;
    git.head().ok()?.oid.map(|oid| oid.to_string())
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
    let mut client = HostedGrpcClient::open_session_with_insecure(
        options.addr,
        options.user_config,
        options.server_key,
        HostedAuthMode::CredentialFallback,
        options.insecure,
    )
    .await?
    .with_human_signature_callback(crate::client::cli_human_signature_callback());

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
        let track_to_update = options.local_thread.unwrap_or(options.remote_thread);
        let mut changed = false;
        if let Some(final_state) = result.final_state {
            let track_tn = ThreadName::new(track_to_update);
            let pre_target = repo.refs().get_thread(&track_tn)?;
            changed = pre_target.as_ref() != Some(&final_state);
            if changed {
                let head_ref = repo.head_ref()?;
                let should_materialize = !options.lazy
                    && match &head_ref {
                        Head::Attached { thread } => thread == track_to_update,
                        Head::Detached { .. } => options.local_thread.is_none(),
                    };
                if should_materialize {
                    match (&head_ref, pre_target) {
                        (Head::Attached { .. }, Some(_)) => {
                            super::super::ff_record::record_ff_advance(
                                repo,
                                options.remote_thread,
                                &final_state,
                            )?;
                        }
                        (Head::Attached { .. }, None) => {
                            repo.fast_forward_attached_from_materialized_state(&final_state, None)?;
                        }
                        (Head::Detached { .. }, _) => {
                            repo.goto(&final_state)?;
                            repo.refs().set_thread(&track_tn, &final_state)?;
                        }
                    }
                } else {
                    repo.refs().set_thread(&track_tn, &final_state)?;
                }
            }
        }
        if should_output_json(options.cli, Some(repo.config())) {
            let output = heddle_pull_output(
                changed,
                options.remote_thread.to_string(),
                track_to_update.to_string(),
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
                let output = list_plain_git_remotes(&probe.root);
                render_remote_list(&output, should_output_json(cli, None))?;
                return Ok(());
            }
        }
        RemoteCommands::Show { name } => {
            if let Some(probe) = build_plain_git_verification_probe(start)? {
                let output = show_plain_git_remote(&probe.root, name)
                    .ok_or_else(|| RecoveryAdvice::remote_not_found(name))?;
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
            let output = list_remotes(&repo)?;
            render_remote_list(&output, should_output_json(cli, Some(repo.config())))?;
            Ok(())
        }
        RemoteCommands::Add { name, url } => {
            super::preflight_native_remote_transport(&repo, Some(&url), "remote add")?;
            // When heddle has no default yet, core's resolved default falls
            // through to git-overlay rules (upstream / origin / sole remote),
            // matching the previous private `git_overlay_default_remote_name`.
            let git_overlay_default_before = (repo.capability()
                == RepositoryCapability::GitOverlay)
                .then(|| resolved_default_remote_name(&repo).ok().flatten())
                .flatten();
            sync_git_overlay_remote_add(&repo, &name, &url)?;
            let mut cfg = RemoteConfig::open(&repo).map_err(anyhow::Error::new)?;
            let default_was_empty = cfg.default_name().is_none();
            cfg.add(
                &name,
                Remote {
                    url: url.clone(),
                    insecure: false,
                },
            )
            .map_err(anyhow::Error::new)?;
            if default_was_empty
                && git_overlay_default_before
                    .as_deref()
                    .is_some_and(|default| default != name)
            {
                cfg.clear_default().map_err(anyhow::Error::new)?;
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
            let mut cfg = RemoteConfig::open(&repo).map_err(anyhow::Error::new)?;
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
            let mut cfg = RemoteConfig::open(&repo).map_err(anyhow::Error::new)?;
            // Git-overlay remotes added via `git remote add` only live in
            // `.git/config`. `merged_remote_items` surfaces them in
            // `remote list/show`, but `RemoteConfig::set_default` would
            // reject them as NotFound. Adopt the URL into
            // `.heddle/remotes.toml` first so `default_name()`-driven
            // readers (including `resolve_remote_with_key`) can resolve
            // it, then set the default explicitly.
            if cfg.get(&name).is_err() {
                cfg.add(
                    &name,
                    Remote {
                        url,
                        insecure: false,
                    },
                )
                .map_err(anyhow::Error::new)?;
            }
            cfg.set_default(&name).map_err(anyhow::Error::new)?;
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
            let output = show_remote(&repo, &name)?
                .ok_or_else(|| RecoveryAdvice::remote_not_found(&name))?;
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

fn render_remote_list(output: &RemoteListReport, json: bool) -> Result<()> {
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

fn render_remote_info(output: &RemoteInfo, json: bool) -> Result<()> {
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

fn map_included_config_error(err: heddle_core::IncludedGitRemoteConfigError) -> anyhow::Error {
    RecoveryAdvice::git_remote_in_included_config(&err.name, &err.path).into()
}

fn sync_git_overlay_remote_add(repo: &Repository, name: &str, url: &str) -> Result<()> {
    if repo.capability() != RepositoryCapability::GitOverlay {
        return Ok(());
    }
    validate_git_overlay_remote_name(name)?;
    let ctx = GitConfigContext::discover(repo.root())
        .context("Git-overlay remote add requires a writable Git config")?;
    let config_path = ctx
        .write_file_for(name)
        .map_err(map_included_config_error)?;
    upsert_git_remote_config(repo.root(), &config_path, name, url)
}

fn sync_git_overlay_remote_remove(repo: &Repository, name: &str) -> Result<()> {
    if repo.capability() != RepositoryCapability::GitOverlay {
        return Ok(());
    }
    let Some(ctx) = GitConfigContext::discover(repo.root()) else {
        return Ok(());
    };
    for config_path in ctx
        .remove_files_for(name)
        .map_err(map_included_config_error)?
    {
        remove_git_remote_config(repo.root(), &config_path, name)?;
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
/// file. Every existing definition of the remote in that file is dropped before
/// a fresh canonical section is appended, so an upsert replaces rather than
/// appends a duplicate that the first-seen section would win over on the next
/// read.
fn upsert_git_remote_config(root: &Path, config_path: &Path, name: &str, url: &str) -> Result<()> {
    let git = SleyRepository::discover(root).map_err(anyhow::Error::new)?;
    let remote = RemoteConfigSet::new(name)
        .with_url(url)
        .with_fetch_refspec(format!("+refs/heads/*:refs/remotes/{name}/*"));
    let plan = ConfigEditPlan::new(config_path).with_operation(ConfigEdit::replace_section(
        "remote",
        Some(remote.name),
        remote.entries,
    ));
    git.apply_config_edit_plan(plan)
        .map_err(anyhow::Error::new)?;
    Ok(())
}

/// Remove every `[remote "<name>"]` section from a single physical config file
/// that uses the normal quoted subsection form. No-ops when the file is absent
/// or defines no such remote.
fn remove_git_remote_config(root: &Path, config_path: &Path, name: &str) -> Result<()> {
    let git = SleyRepository::discover(root).map_err(anyhow::Error::new)?;
    let plan = ConfigEditPlan::new(config_path)
        .with_operation(ConfigEdit::remove_section("remote", Some(name.to_string())));
    git.apply_config_edit_plan(plan)
        .map_err(anyhow::Error::new)?;
    Ok(())
}

#[cfg(feature = "client")]
struct PullNetworkOptions<'a> {
    addr: SocketAddr,
    repo_path: Option<&'a str>,
    user_config: &'a UserConfig,
    server_key: Option<String>,
    remote_thread: &'a str,
    local_thread: Option<&'a str>,
    lazy: bool,
    insecure: bool,
    cli: &'a Cli,
}

#[cfg(test)]
mod tests {
    use std::fs;

    use heddle_core::plain_git_remote_items;

    use super::*;

    fn init_git(root: &Path) {
        SleyRepository::init(root).expect("init git repo");
    }

    #[test]
    fn default_pull_thread_uses_current_git_overlay_thread() {
        let head = Head::Attached {
            thread: ThreadName::new("master"),
        };

        assert_eq!(
            default_pull_thread_name(None, RepositoryCapability::GitOverlay, &head),
            "master"
        );
    }

    #[test]
    fn default_pull_thread_keeps_native_main_default() {
        let head = Head::Attached {
            thread: ThreadName::new("feature"),
        };

        assert_eq!(
            default_pull_thread_name(None, RepositoryCapability::NativeHeddle, &head),
            "main"
        );
    }

    #[test]
    fn default_pull_thread_honors_explicit_thread() {
        let head = Head::Attached {
            thread: ThreadName::new("master"),
        };

        assert_eq!(
            default_pull_thread_name(
                Some("release".to_string()),
                RepositoryCapability::GitOverlay,
                &head,
            ),
            "release"
        );
    }

    // Pure git-config read coverage lives in heddle_core::remote. These tests
    // keep mutation/write-target invariants for the CLI git-overlay sync path.

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
            remove_git_remote_config(tmp.path(), &path, "origin").unwrap();
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
            tmp.path(),
            &ctx.write_file_for("origin").unwrap(),
            "origin",
            "https://example.com/new",
        )
        .unwrap();

        // The upsert must hit the per-worktree layer (where the remote
        // lives and wins on read); writing to common would leave the
        // stale per-worktree url winning, a silent read/write divergence.
        assert_eq!(
            plain_git_remote_items(tmp.path())
                .get("origin")
                .map(String::as_str),
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
            remove_git_remote_config(tmp.path(), &path, "upstream").unwrap();
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
        upsert_git_remote_config(tmp.path(), &target, "origin", "https://example.com/new").unwrap();

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
        assert_eq!(
            ctx.write_file_for("origin").unwrap(),
            git_dir.join("config")
        );
        upsert_git_remote_config(
            tmp.path(),
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

    #[test]
    fn remove_clears_comment_suffixed_remote_header() {
        let tmp = tempfile::TempDir::new().unwrap();
        init_git(tmp.path());
        let git_dir = tmp.path().join(".git");
        // A valid Git header Sley accepts but the hand-rolled writer didn't:
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
            remove_git_remote_config(tmp.path(), &path, "origin").unwrap();
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
        // The legacy dotted subsection form, equally valid to Sley.
        fs::write(
            git_dir.join("config"),
            "[remote.origin]\n\turl = https://example.com/repo\n",
        )
        .unwrap();

        assert!(plain_git_remote_items(tmp.path()).contains_key("origin"));

        let ctx = GitConfigContext::discover(tmp.path()).unwrap();
        for path in ctx.remove_files_for("origin").unwrap() {
            remove_git_remote_config(tmp.path(), &path, "origin").unwrap();
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
            tmp.path(),
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
            tmp.path(),
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
