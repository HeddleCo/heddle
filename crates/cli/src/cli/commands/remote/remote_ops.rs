// SPDX-License-Identifier: Apache-2.0
//! Pull, remote management, and serve commands.

#[cfg(feature = "client")]
use std::net::SocketAddr;

use anyhow::{Context, Result};
#[cfg(feature = "client")]
use heddle_client::grpc_hosted::{HostedAuthMode, PullMaterialization};
#[cfg(feature = "client")]
use heddle_core::{
    HostedPullResult, HostedPullResultFields, format_connected_to,
    heddle_pull_execution_facts_from_hosted, parse_hosted_pull_result, pull_tip_changed,
};
use heddle_core::{
    LocalTransferSummary, PullFailure, PullOutcome, PullPlan, PullPlanRequest, RemoteInfo,
    RemoteListReport, build_pull_outcome, format_pull_outcome_text, format_pulling_from,
    heddle_pull_execution_facts_from_local, is_native_transport_mismatch, list_remotes,
    local_pull_changed, plan_pull, pull_should_materialize, show_remote,
};
// Re-export under the historical crate-local names for sibling modules.
pub(crate) use heddle_core::{resolve_default_remote_name, resolved_default_remote_name};
use objects::object::ThreadName;
use refs::Head;
use repo::Repository;
use serde::Serialize;

use super::super::{
    action_line::print_next,
    advice::RecoveryAdvice,
    source_authority::SourceAuthorityDispatch,
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
    remote::{Remote, RemoteConfig, RemoteTarget, resolve_remote_with_key},
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

/// CLI machine envelope: domain [`PullOutcome`] plus skipped verification state.
#[derive(Serialize)]
struct PullOutput {
    #[serde(flatten)]
    outcome: PullOutcome,
    #[allow(dead_code)]
    #[serde(skip_serializing)]
    #[serde(rename = "verification")]
    trust: RepositoryVerificationState,
}

fn heddle_pull_output_from_local(
    plan: Option<&PullPlan>,
    changed: bool,
    remote: String,
    thread: String,
    summary: &LocalTransferSummary,
    trust: RepositoryVerificationState,
) -> PullOutput {
    let outcome = build_pull_outcome(
        plan,
        heddle_pull_execution_facts_from_local(changed, remote, thread, summary),
    );
    PullOutput { outcome, trust }
}

#[cfg(feature = "client")]
fn heddle_pull_output_from_hosted(
    plan: Option<&PullPlan>,
    changed: bool,
    remote: String,
    thread: String,
    fields: &HostedPullResultFields,
    trust: RepositoryVerificationState,
) -> PullOutput {
    let outcome = build_pull_outcome(
        plan,
        heddle_pull_execution_facts_from_hosted(changed, remote, thread, fields),
    );
    PullOutput { outcome, trust }
}

/// Map a typed [`PullFailure`] to RecoveryAdvice / anyhow for CLI exit.
fn map_pull_failure(failure: PullFailure) -> anyhow::Error {
    match failure {
        PullFailure::Preflight(blocker) => {
            super::map_remote_preflight_blocker(blocker, "pull", None)
        }
        PullFailure::LocalLazyUnsupported { source_path } => anyhow::anyhow!(
            RecoveryAdvice::local_lazy_pull_unsupported(std::path::Path::new(&source_path))
        ),
        PullFailure::RemoteFailed {
            remote_thread,
            local_thread,
            error,
        } => {
            #[cfg(feature = "client")]
            {
                anyhow::anyhow!(RecoveryAdvice::remote_pull_failed(
                    &remote_thread,
                    local_thread.as_deref(),
                    &error,
                ))
            }
            #[cfg(not(feature = "client"))]
            {
                let _ = local_thread;
                anyhow::anyhow!("Pull failed from {remote_thread}: {error}")
            }
        }
    }
}

/// Print unstyled domain pull text with CLI markers / emphasis.
#[cfg(feature = "client")]
fn render_pull_outcome_text(outcome: &PullOutcome, trust: &RepositoryVerificationState) {
    let text = format_pull_outcome_text(outcome, 8);
    if outcome.changed {
        if outcome.transport == "git" {
            println!(
                "{} pulled from {}",
                style::ok_marker(),
                style::bold(&outcome.remote)
            );
        } else if let (Some(state), Some(objects)) = (&outcome.state, outcome.objects) {
            let thread = outcome.thread.as_deref().unwrap_or("thread");
            println!(
                "{} pulled {} from {} ({})",
                style::ok_marker(),
                style::state_id(state),
                style::bold(thread),
                style::count(objects, "object")
            );
        } else {
            println!(
                "{} pulled from {}",
                style::ok_marker(),
                style::bold(outcome.thread.as_deref().unwrap_or(outcome.remote.as_str()))
            );
            for line in &text.detail_lines {
                if let Some(state) = line.strip_prefix("state: ") {
                    println!("{}", style::field("state", &style::state_id(state)));
                } else {
                    println!("{line}");
                }
            }
            // Workspace line for heddle hosted success is not historical; skip.
            return;
        }
    } else {
        println!(
            "{} already up to date with {}; repository verification checked below",
            style::ok_marker(),
            style::bold(&outcome.remote)
        );
    }

    if outcome.transport == "git" {
        for line in &text.detail_lines {
            if let Some(branch) = line.strip_prefix("Branch: ") {
                if let Some((name, rest)) = branch.split_once(" at ") {
                    println!("Branch: {} at {rest}", style::bold(name));
                } else {
                    println!("Branch: {}", style::bold(branch));
                }
            } else if let Some(rest) = line.strip_prefix("Imported: ") {
                // rest is "N new state(s)" — re-style via count when parseable
                println!("Imported: {rest}");
            } else if let Some(rest) = line.strip_prefix("Scanned: ") {
                println!("Scanned: {rest}");
            } else {
                println!("{line}");
            }
        }
        if !trust.verified {
            println!("Workspace: {}", style::warn(&trust.status));
            if !trust.recommended_action.is_empty() {
                print_next(&trust.recommended_action);
            }
        } else {
            println!("Workspace: verified");
        }
    }
}

/// Execute pull command.
///
/// Pure orchestration (`plan_pull`) runs first; network / git I/O bodies stay
/// here and consume plan fields (thread targets, clean-worktree policy, path).
pub async fn cmd_pull(
    cli: &Cli,
    remote: Option<String>,
    thread: Option<String>,
    local_thread: Option<String>,
    lazy: bool,
    insecure: bool,
) -> Result<()> {
    let repo = cli.open_repo()?;
    SourceAuthorityDispatch::for_repo(&repo)
        .require_pull(
            remote.as_deref(),
            thread.as_deref(),
            local_thread.as_deref(),
        )
        .map_err(anyhow::Error::new)?;
    let has_default_remote = resolved_default_remote_name(&repo)?.is_some();
    let pull_uses_hosted_network = super::push_target_is_hosted_network(&repo, remote.as_deref());
    // Match preflight_native_remote_transport: overlay capability never
    // treats a git URL as a native-transport mismatch.
    let remote_is_git_local_or_url = matches!(
        super::classify_remote_spec(&repo, remote.as_deref()),
        Some(super::RemoteTransportKind::LocalGit | super::RemoteTransportKind::GitUrl)
    );
    let transport_mismatch =
        is_native_transport_mismatch(repo.capability(), remote_is_git_local_or_url);
    let head = repo.head_ref()?;
    let plan = plan_pull(&PullPlanRequest {
        capability: repo.capability(),
        hosted_enabled: repo.hosted_enabled(),
        uses_hosted_network: pull_uses_hosted_network,
        remote: remote.clone(),
        has_default_remote,
        thread: thread.clone(),
        local_thread: local_thread.clone(),
        head,
        transport_mismatch,
        lazy,
    })
    .map_err(|blocker| super::map_remote_preflight_blocker(blocker, "pull", remote.as_deref()))?;

    // Transport mismatch already refused by plan_pull.

    let user_config = UserConfig::load_default()?;
    #[cfg(not(feature = "client"))]
    let token = user_config.remote_token()?;
    #[cfg(feature = "client")]
    let (target, server_key) = resolve_remote_with_key(&repo, plan.remote.as_deref())?;
    #[cfg(not(feature = "client"))]
    let (target, _server_key) = resolve_remote_with_key(&repo, plan.remote.as_deref())?;

    let remote_thread = plan.remote_thread.as_str();
    let local_thread_name = plan.local_thread.as_deref();
    if plan.requires_clean_worktree {
        ensure_worktree_clean(&repo, "pull")?;
    }

    match target {
        RemoteTarget::Local(path) => {
            pull_local(
                &repo,
                &path,
                remote_thread,
                local_thread_name,
                &plan,
                cli,
                plan.lazy,
            )
            .await?;
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
                    remote_thread,
                    local_thread: local_thread_name,
                    lazy: plan.lazy,
                    insecure: insecure
                        || cli_shared::remote_allows_insecure(&repo, plan.remote.as_deref()),
                    plan: &plan,
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

async fn pull_local(
    repo: &Repository,
    source_path: &std::path::Path,
    remote_thread: &str,
    local_thread: Option<&str>,
    plan: &PullPlan,
    cli: &Cli,
    lazy: bool,
) -> Result<()> {
    if lazy {
        return Err(map_pull_failure(PullFailure::LocalLazyUnsupported {
            source_path: source_path.display().to_string(),
        }));
    }

    let source_label = format!("file://{}", source_path.display());
    if !should_output_json(cli, Some(repo.config())) {
        let working = format_pulling_from(&source_label);
        if let Some(source) = working.strip_prefix("pulling from ") {
            println!(
                "{} pulling from {}",
                style::working_marker(),
                style::dim(source)
            );
        } else {
            println!("{} {working}", style::working_marker());
        }
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
    let pre_target_str = pre_target.as_ref().map(|s| s.to_string());
    let changed = local_pull_changed(
        pre_target_str.as_deref(),
        &state_id.to_string(),
        objects_copied,
    );

    // Materialize policy comes from plan_pull (pure); lazy is always false here.
    let head_ref = repo.head_ref()?;
    let should_materialize = pull_should_materialize(plan.will_materialize, plan.lazy);
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
        let summary = LocalTransferSummary {
            state: Some(state_id.to_string()),
            objects: Some(objects_copied),
        };
        let output = heddle_pull_output_from_local(
            Some(plan),
            changed,
            source_path.display().to_string(),
            track_to_update.to_string(),
            &summary,
            build_repository_verification_state(repo),
        );
        crate::cli::render::write_json_stdout(&output)?;
    } else {
        let summary = LocalTransferSummary {
            state: Some(state_id.short().to_string()),
            objects: Some(objects_copied),
        };
        let output = heddle_pull_output_from_local(
            Some(plan),
            changed,
            source_path.display().to_string(),
            remote_thread.to_string(),
            &summary,
            build_repository_verification_state(repo),
        );
        let text = format_pull_outcome_text(&output.outcome, 8);
        println!(
            "{} pulled {} from {} ({})",
            style::ok_marker(),
            style::state_id(&state_id.short().to_string()),
            style::bold(remote_thread),
            style::count(objects_copied, "object")
        );
        debug_assert!(
            text.headline.contains(remote_thread) || text.headline.contains("pulled"),
            "domain headline: {}",
            text.headline
        );
        // Domain detail lines (e.g. hosted state field when objects omitted).
        for line in &text.detail_lines {
            if let Some(state) = line.strip_prefix("state: ") {
                println!("{}", style::field("state", &style::state_id(state)));
            } else {
                println!("{line}");
            }
        }
    }

    Ok(())
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
        let line = format_connected_to(&options.addr.to_string());
        if let Some(addr) = line.strip_prefix("connected to ") {
            println!("{} connected to {}", style::ok_marker(), style::dim(addr));
        } else {
            println!("{} {line}", style::ok_marker());
        }
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

    // Keep typed StateId for ref/worktree I/O; map string fields for pure parse.
    let final_state_id = result.final_state;
    let fields = HostedPullResultFields {
        success: result.success,
        final_state: final_state_id.as_ref().map(|state| state.to_string()),
        error: result.error,
    };
    match parse_hosted_pull_result(options.remote_thread, options.local_thread, &fields) {
        HostedPullResult::Success { final_state } => {
            let track_to_update = options.local_thread.unwrap_or(options.remote_thread);
            let mut changed = false;
            if let Some(final_state_id) = final_state_id {
                let track_tn = ThreadName::new(track_to_update);
                let pre_target = repo.refs().get_thread(&track_tn)?;
                let pre_target_str = pre_target.as_ref().map(|s| s.to_string());
                changed = pull_tip_changed(pre_target_str.as_deref(), final_state.as_deref());
                if changed {
                    let head_ref = repo.head_ref()?;
                    let should_materialize =
                        pull_should_materialize(options.plan.will_materialize, options.lazy);
                    if should_materialize {
                        match (&head_ref, pre_target) {
                            (Head::Attached { .. }, Some(_)) => {
                                super::super::ff_record::record_ff_advance(
                                    repo,
                                    options.remote_thread,
                                    &final_state_id,
                                )?;
                            }
                            (Head::Attached { .. }, None) => {
                                repo.fast_forward_attached_from_materialized_state(
                                    &final_state_id,
                                    None,
                                )?;
                            }
                            (Head::Detached { .. }, _) => {
                                repo.goto(&final_state_id)?;
                                repo.refs().set_thread(&track_tn, &final_state_id)?;
                            }
                        }
                    } else {
                        repo.refs().set_thread(&track_tn, &final_state_id)?;
                    }
                }
            }
            // Facts reuse the same string-mapped transport fields.
            let facts_fields = HostedPullResultFields {
                success: true,
                final_state,
                error: None,
            };
            if should_output_json(options.cli, Some(repo.config())) {
                let output = heddle_pull_output_from_hosted(
                    Some(options.plan),
                    changed,
                    options.remote_thread.to_string(),
                    track_to_update.to_string(),
                    &facts_fields,
                    build_repository_verification_state(repo),
                );
                crate::cli::render::write_json_stdout(&output)?;
            } else {
                let output = heddle_pull_output_from_hosted(
                    Some(options.plan),
                    changed,
                    options.remote_thread.to_string(),
                    options.remote_thread.to_string(),
                    &facts_fields,
                    build_repository_verification_state(repo),
                );
                render_pull_outcome_text(&output.outcome, &output.trust);
            }
        }
        HostedPullResult::Failed(failure) => {
            return Err(map_pull_failure(failure));
        }
    }

    Ok(())
}

/// Execute remote command.
pub fn cmd_remote(cli: &Cli, command: RemoteCommands) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let start = cli.repo.as_ref().unwrap_or(&cwd);
    if let Some(probe) = build_plain_git_verification_probe(start)? {
        refuse_git_owned_remote(
            SourceAuthorityDispatch::git_overlay(),
            &command,
            probe.git_branch.as_deref(),
        )?;
    }

    let repo = Repository::open(start)?;
    refuse_git_owned_remote(
        SourceAuthorityDispatch::for_repo(&repo),
        &command,
        repo.current_lane()?.as_deref(),
    )?;

    match command {
        RemoteCommands::List => {
            let output = list_remotes(&repo)?;
            render_remote_list(&output, should_output_json(cli, Some(repo.config())))?;
            Ok(())
        }
        RemoteCommands::Add { name, url } => {
            super::preflight_native_remote_transport(&repo, Some(&url), "remote add")?;
            let mut cfg = RemoteConfig::open(&repo).map_err(anyhow::Error::new)?;
            cfg.add(
                &name,
                Remote {
                    url: url.clone(),
                    insecure: false,
                },
            )
            .map_err(anyhow::Error::new)?;
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
            let mut cfg = RemoteConfig::open(&repo).map_err(anyhow::Error::new)?;
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
            let mut cfg = RemoteConfig::open(&repo).map_err(anyhow::Error::new)?;
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

fn refuse_git_owned_remote(
    dispatch: SourceAuthorityDispatch,
    command: &RemoteCommands,
    branch: Option<&str>,
) -> Result<()> {
    if let RemoteCommands::SetDefault { name } = command {
        let push_default = super::super::command_catalog::checked_action_from_argv([
            "git",
            "config",
            "remote.pushDefault",
            name,
        ]);
        let mut recovery = vec![push_default.clone()];
        let pull_guidance = branch.map(|branch| {
            super::super::command_catalog::checked_action_from_argv(vec![
                "git".to_string(),
                "config".to_string(),
                format!("branch.{branch}.remote"),
                name.clone(),
            ])
        });
        if let Some(action) = &pull_guidance {
            recovery.push(action.clone());
        } else {
            recovery.push("git branch --show-current".to_string());
        }
        recovery.push("heddle adopt".to_string());
        if !dispatch.is_native() {
            return Err(anyhow::anyhow!(RecoveryAdvice::safety_refusal(
                "source_authority_direct_git",
                "`heddle remote set-default` is unavailable while Git owns source history",
                match pull_guidance {
                    Some(pull) => format!(
                        "Run `{push_default}` to configure Git push default and `{pull}` to configure this branch's pull remote. These are separate Git settings."
                    ),
                    None => format!(
                        "Run `{push_default}` to configure Git push default. Git pull remains unconfigured until you select a branch and set its branch.<name>.remote."
                    ),
                },
                "repository source authority is git-overlay",
                "Heddle has one default-remote concept, while Git separates push default from branch pull configuration",
                "Git config and Heddle metadata were left unchanged",
                push_default,
                recovery,
            )));
        }
        return Ok(());
    }

    let argv = match command {
        RemoteCommands::List => vec!["git".into(), "remote".into(), "-v".into()],
        RemoteCommands::Show { name } => {
            vec![
                "git".into(),
                "remote".into(),
                "get-url".into(),
                name.clone(),
            ]
        }
        RemoteCommands::Add { name, url } => vec![
            "git".into(),
            "remote".into(),
            "add".into(),
            name.clone(),
            url.clone(),
        ],
        RemoteCommands::Remove { name } => {
            vec!["git".into(), "remote".into(), "remove".into(), name.clone()]
        }
        RemoteCommands::SetDefault { .. } => unreachable!(),
    };
    dispatch
        .require_remote(argv, Vec::new())
        .map_err(anyhow::Error::new)
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
    /// Pure orchestration plan (outcome assembly + dirty-worktree policy).
    plan: &'a PullPlan,
    cli: &'a Cli,
}
