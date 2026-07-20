// SPDX-License-Identifier: Apache-2.0
//! Pull, remote management, and serve commands.

#[cfg(feature = "client")]
use std::net::SocketAddr;
use std::path::Path;

use anyhow::{Context, Result};
#[cfg(feature = "client")]
use heddle_client::grpc_hosted::PullMaterialization;
#[cfg(feature = "client")]
use heddle_core::{
    HostedPullResult, HostedPullResultFields, format_connected_to,
    heddle_pull_execution_facts_from_hosted, parse_hosted_pull_result, pull_tip_changed,
};
use heddle_core::{
    LocalTransferSummary, PullFailure, PullOutcome, PullPlan, PullPlanRequest, RemoteInfo,
    RemoteListReport, build_pull_outcome, format_pull_outcome_text, format_pulling_from,
    git_overlay_pull_execution_facts, heddle_pull_execution_facts_from_local,
    is_native_transport_mismatch, list_plain_git_remotes, list_remotes, local_pull_changed,
    plan_pull, pull_should_materialize, show_plain_git_remote, show_remote,
};
// Re-export under the historical crate-local names for sibling modules.
pub(crate) use heddle_core::{resolve_default_remote_name, resolved_default_remote_name};
use heddle_git_projection::credential::EmbeddingSafeCredentialProvider;
use objects::{
    object::{StateId, ThreadName, Tree},
    store::ObjectStore,
};
use refs::Head;
use repo::{Repository, RepositoryCapability};
use serde::Serialize;
use sley::{
    ConfigEdit, ConfigEditPlan, ConfigEditScope, HeadUpdateOptions, RefChange, ReferenceTarget,
    RemoteConfigRefusal, RemoteConfigRemove, RemoteConfigSet, Repository as SleyRepository,
    remote::{
        FetchOptions, PackGenerationProgress, ProgressSink as SleyProgressSink, TransferProgress,
    },
};

use super::super::{
    action_line::print_next,
    advice::RecoveryAdvice,
    import_progress::ImportProgress,
    verification_health::{
        RepositoryVerificationState, build_plain_git_verification_probe,
        build_repository_verification_state,
    },
    worktree_safety::ensure_worktree_clean,
};
#[cfg(feature = "client")]
use crate::client::HostedGrpcClient;
use crate::{
    cli::{
        Cli, RemoteCommands,
        progress_render::{finish_line, format_transfer_bytes, progress_for},
        should_output_json, style,
    },
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
    #[serde(rename = "verification")]
    trust: RepositoryVerificationState,
}

/// CLI machine envelope: domain [`PullOutcome`] plus repository verification.
#[derive(Serialize)]
struct PullOutput {
    #[serde(flatten)]
    outcome: PullOutcome,
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
    let default_remote = resolved_default_remote_name(&repo)?;
    let has_default_remote = default_remote.is_some();
    let configured_remote_name = match remote.as_deref() {
        Some(name) => RemoteConfig::open(&repo)
            .ok()
            .and_then(|config| config.get(name).ok())
            .map(|_| name.to_string()),
        None => default_remote,
    };
    let pull_uses_hosted_network = super::pull_target_is_hosted_network(&repo, remote.as_deref());
    // Match preflight_native_remote_transport: overlay capability never
    // treats a git URL as a native-transport mismatch.
    let remote_is_git_local_or_url = matches!(
        super::classify_pull_remote_spec(&repo, remote.as_deref()),
        Some(super::RemoteTransportKind::LocalGit | super::RemoteTransportKind::GitUrl)
    );
    let transport_mismatch =
        is_native_transport_mismatch(repo.capability(), remote_is_git_local_or_url);
    let head = repo.head_ref()?;
    let plan = plan_pull(&PullPlanRequest {
        capability: repo.capability(),
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

    let remote_thread = plan.remote_thread.as_str();
    let local_thread_name = plan.local_thread.as_deref();
    if plan.requires_clean_worktree {
        ensure_worktree_clean(&repo, "pull")?;
    }
    if plan.uses_local_git_overlay {
        return pull_git_overlay(&repo, &plan, thread.as_deref(), insecure, cli);
    }

    let user_config = UserConfig::load_default()?;
    #[cfg(feature = "client")]
    let (target, server_key) = resolve_remote_with_key(&repo, plan.remote.as_deref())?;
    #[cfg(not(feature = "client"))]
    let (target, _server_key) = resolve_remote_with_key(&repo, plan.remote.as_deref())?;

    match target {
        RemoteTarget::Local(path) => {
            pull_local(
                &repo,
                &path,
                remote_thread,
                local_thread_name,
                configured_remote_name.as_deref(),
                &plan,
                cli,
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
            let _ = (addr, repo_path, insecure);
            #[cfg(not(feature = "client"))]
            anyhow::bail!(RecoveryAdvice::network_feature_unavailable("pull"));
        }
    }

    Ok(())
}

struct GitPullProgress {
    progress: objects::Progress,
    received_bytes: u64,
    received_objects: u64,
}

impl SleyProgressSink for GitPullProgress {
    fn transfer(&mut self, event: TransferProgress) {
        self.received_bytes = event.received_bytes;
        if let Some(total) = event.total_objects {
            self.progress.set_total(total as usize);
        }
        let received = event.received_objects.saturating_sub(self.received_objects);
        self.received_objects = event.received_objects;
        self.progress.inc(received as usize);
    }

    fn pack_generation(&mut self, event: &PackGenerationProgress) {
        let _ = event;
    }

    fn message(&mut self, message: &str) {
        let _ = message;
    }
}

fn pull_git_overlay(
    repo: &Repository,
    plan: &PullPlan,
    requested_thread: Option<&str>,
    insecure: bool,
    cli: &Cli,
) -> Result<()> {
    if plan.lazy {
        return Err(git_pull_lazy_advice());
    }
    if insecure {
        return Err(git_pull_insecure_advice());
    }

    let remote_name = resolve_default_remote_name(repo, plan.remote.as_deref())?;
    let git = SleyRepository::discover(repo.root()).map_err(anyhow::Error::new)?;
    if !git.remote_names()?.iter().any(|name| name == &remote_name) {
        return Err(git_pull_unconfigured_remote_advice(&remote_name));
    }
    let current_branch = repo.git_overlay_current_branch()?;
    let local_branch = plan
        .local_thread
        .as_deref()
        .or(current_branch.as_deref())
        .context("cannot pull into a detached Git checkout without --local-thread")?;
    let git_config = git.config_snapshot().map_err(anyhow::Error::new)?;
    let remote_branch = match (
        requested_thread,
        git_config.get("branch", Some(local_branch), "merge"),
    ) {
        (Some(requested), _) => requested.to_string(),
        (None, Some(merge_ref)) => merge_ref
            .strip_prefix("refs/heads/")
            .with_context(|| {
                format!(
                    "branch.{local_branch}.merge must name a branch under refs/heads/, not {merge_ref}"
                )
            })?
            .to_string(),
        (None, None) => plan.remote_thread.clone(),
    };
    let local_ref = format!("refs/heads/{local_branch}");
    let remote_ref = format!("refs/heads/{remote_branch}");
    let old_oid = git
        .find_reference(&local_ref)
        .map_err(anyhow::Error::new)?
        .and_then(|reference| reference.peeled_oid(&git).ok().flatten());
    let old_state = repo.refs().get_thread(&ThreadName::new(local_branch))?;

    let progress = progress_for(cli, repo);
    progress.set_phase("streaming Git objects");
    let mut sley_progress = GitPullProgress {
        progress: progress.clone(),
        received_bytes: 0,
        received_objects: 0,
    };
    let mut credentials = EmbeddingSafeCredentialProvider::new(&git_config);
    let fetch_refspecs = vec![remote_ref.clone(), "+refs/notes/*:refs/notes/*".to_string()];
    let outcome = git
        .fetch(
            &remote_name,
            &fetch_refspecs,
            git_pull_fetch_options(&remote_branch),
            &mut credentials,
            &mut sley_progress,
        )
        .map_err(|error| git_pull_fetch_advice(&remote_name, &remote_branch, &error))?;
    finish_line(
        &progress,
        &format!(
            "[done] streamed {} Git objects ({} received)",
            sley_progress.received_objects,
            format_transfer_bytes(sley_progress.received_bytes)
        ),
    );

    let new_oid = outcome
        .ref_updates
        .iter()
        .find(|update| update.src == remote_ref)
        .map(|update| update.oid)
        .with_context(|| format!("Remote branch {remote_branch} was not found"))?;
    if let Some(old_oid) = old_oid
        && old_oid != new_oid
        && !git
            .rev_graph()
            .is_ancestor(old_oid, new_oid)
            .map_err(anyhow::Error::new)?
    {
        return Err(git_pull_diverged_advice(
            local_branch,
            &remote_name,
            &remote_branch,
        ));
    }

    let staging_ref = publish_git_pull_tracking_ref(&git, &remote_name, &remote_branch, new_oid)?;

    let mut import_progress = ImportProgress::start(
        cli,
        repo,
        &format!("{remote_name}/{remote_branch}"),
        &remote_name,
    );
    import_progress.begin_commit_import();
    let mut on_import = |event| import_progress.commit_tick(event);
    let (stats, mapping) = ingest::import_git_into_scoped_with_options_and_progress(
        repo.root(),
        repo.root(),
        ingest::ImportOptions::default(),
        ingest::ImportScope::refs(vec![staging_ref.clone()]),
        Some(&mut on_import),
    )
    .map_err(|error| git_pull_import_advice(&remote_name, &remote_branch, &error))?;
    import_progress.begin_ref_write();
    import_progress.finish();
    let new_state = mapping.get_commit(&new_oid.to_string()).ok_or_else(|| {
        git_pull_import_advice(
            &remote_name,
            &remote_branch,
            &"the fetched commit was not mapped",
        )
    })?;

    let changed = old_oid != Some(new_oid);
    let materialized = current_branch.as_deref() == Some(local_branch) && changed;
    if changed {
        publish_git_pull_branch(
            repo,
            &git,
            &git_config,
            &local_ref,
            old_oid,
            new_oid,
            materialized,
        )?;
    }
    if old_state.as_ref() != Some(&new_state)
        && let Err(error) = repo
            .refs()
            .set_thread(&ThreadName::new(local_branch), &new_state)
    {
        let rollback = changed.then(|| {
            rollback_git_pull_branch(
                repo,
                &git,
                &git_config,
                &local_ref,
                old_oid,
                new_oid,
                materialized,
            )
        });
        return Err(git_pull_metadata_publish_advice(
            local_branch,
            changed,
            &error,
            rollback.as_ref().and_then(|result| result.as_ref().err()),
        ));
    }
    let changed_paths = changed_paths_between_states(repo, old_state.as_ref(), Some(&new_state))?;
    let output = PullOutput {
        outcome: build_pull_outcome(
            Some(plan),
            git_overlay_pull_execution_facts(
                remote_name,
                Some(local_branch.to_string()),
                old_oid.map(|oid| oid.to_string()),
                Some(new_oid.to_string()),
                old_state.map(|state| state.to_string()),
                Some(new_state.to_string()),
                changed,
                stats.states_created,
                stats.commits_imported,
                materialized,
                changed_paths,
            ),
        ),
        trust: build_repository_verification_state(repo),
    };
    if should_output_json(cli, Some(repo.config())) {
        crate::cli::render::write_json_stdout(&output)?;
    } else {
        render_pull_outcome_text(&output.outcome, &output.trust);
    }
    Ok(())
}

fn git_pull_fetch_options(remote_thread: &str) -> FetchOptions {
    FetchOptions {
        quiet: true,
        progress: None,
        auto_follow_tags: false,
        fetch_all_tags: false,
        prune: false,
        prune_tags: false,
        dry_run: false,
        force: false,
        append: false,
        write_fetch_head: true,
        tag_option_explicit: true,
        prune_option_explicit: true,
        prune_tags_option_explicit: true,
        refmap: Some(Vec::new()),
        depth: None,
        merge_srcs: vec![format!("refs/heads/{remote_thread}")],
        filter: None,
        filter_auto: false,
        refetch: false,
        cloning: false,
        record_promisor_refs: false,
        update_shallow: false,
        reject_shallow: false,
        deepen_relative: false,
        update_head_ok: false,
        deepen_since: None,
        deepen_not: Vec::new(),
        ssh_options: None,
        upload_pack_command: None,
        atomic: true,
        negotiation_restrict: None,
        negotiation_include: None,
    }
}

fn publish_git_pull_tracking_ref(
    git: &SleyRepository,
    remote_name: &str,
    remote_branch: &str,
    new_oid: sley::ObjectId,
) -> Result<String> {
    let tracking_ref = format!("refs/remotes/{remote_name}/{remote_branch}");
    let old_tracking = git.references().read_ref(&tracking_ref)?;
    let mut tracking = RefChange::new(tracking_ref.as_str(), ReferenceTarget::Direct(new_oid))?;
    tracking.expected = old_tracking;
    git.apply_ref_changes(&[tracking])
        .map_err(anyhow::Error::new)?;
    Ok(tracking_ref)
}

fn publish_git_pull_branch(
    repo: &Repository,
    git: &SleyRepository,
    config: &sley::GitConfig,
    local_ref: &str,
    old_oid: Option<sley::ObjectId>,
    new_oid: sley::ObjectId,
    materialized: bool,
) -> Result<()> {
    if materialized {
        sley::plumbing::sley_worktree::checkout_detached_filtered(
            repo.root(),
            git.git_dir(),
            git.object_format(),
            &new_oid,
            b"Heddle <heddle@localhost> 0 +0000".to_vec(),
            b"heddle pull: prepare fast-forward".to_vec(),
            config,
        )
        .map_err(|error| git_pull_checkout_advice(local_ref, &error))?;
    }

    let mut branch = RefChange::new(local_ref, ReferenceTarget::Direct(new_oid))?;
    branch.expected = old_oid.map(ReferenceTarget::Direct);
    let mut changes = vec![branch];
    if materialized {
        let mut head = RefChange::new("HEAD", ReferenceTarget::Symbolic(local_ref.to_string()))?;
        head.expected = Some(ReferenceTarget::Direct(new_oid));
        changes.push(head);
    }
    if let Err(error) = git.apply_ref_changes(&changes) {
        let rollback =
            rollback_git_pull_branch(repo, git, config, local_ref, old_oid, new_oid, materialized);
        return Err(git_pull_publish_advice(
            local_ref,
            &error,
            rollback.err().as_ref(),
        ));
    }
    Ok(())
}

fn rollback_git_pull_branch(
    repo: &Repository,
    git: &SleyRepository,
    config: &sley::GitConfig,
    local_ref: &str,
    old_oid: Option<sley::ObjectId>,
    new_oid: sley::ObjectId,
    materialized: bool,
) -> Result<()> {
    let old_oid = old_oid.context("the previous branch was unborn")?;
    if materialized {
        sley::plumbing::sley_worktree::checkout_detached_filtered(
            repo.root(),
            git.git_dir(),
            git.object_format(),
            &old_oid,
            b"Heddle <heddle@localhost> 0 +0000".to_vec(),
            b"heddle pull: roll back failed fast-forward".to_vec(),
            config,
        )?;
    }

    let current = git.references().read_ref(local_ref)?;
    let mut branch = RefChange::new(local_ref, ReferenceTarget::Direct(old_oid))?;
    branch.expected = current.or(Some(ReferenceTarget::Direct(new_oid)));
    git.apply_ref_changes(&[branch])?;
    if materialized {
        git.set_head_symref(
            local_ref,
            HeadUpdateOptions::new()
                .expect_current(ReferenceTarget::Direct(old_oid))
                .reflog("heddle pull: reattach after rollback"),
        )?;
    }
    Ok(())
}

fn git_pull_fetch_advice(
    remote: &str,
    remote_branch: &str,
    error: &impl std::fmt::Display,
) -> anyhow::Error {
    let retry = format!("heddle pull {remote} {remote_branch}");
    RecoveryAdvice::safety_refusal(
        "git_overlay_pull_fetch_failed",
        format!("Could not fetch {remote}/{remote_branch}: {error}"),
        format!("Fix the remote or credentials, then retry `{retry}`."),
        format!("Sley could not complete the Git fetch: {error}"),
        "publishing the fetched Git branch could leave an incomplete object graph",
        "the local branch, Heddle thread, index, and worktree were not advanced",
        retry.clone(),
        vec![retry, "heddle verify".to_string()],
    )
    .into()
}

fn git_pull_lazy_advice() -> anyhow::Error {
    RecoveryAdvice::safety_refusal(
        "git_overlay_pull_lazy_unsupported",
        "Git Overlay pull cannot use --lazy",
        "Pull the complete Git history, or adopt the repository before using native lazy transfer.",
        "the Git Overlay importer requires a complete commit and tree closure",
        "a partial fetch could publish a branch whose Heddle mapping cannot be completed",
        "Git refs, Heddle metadata, the index, and worktree were left unchanged",
        "heddle pull",
        vec!["heddle pull".to_string(), "heddle adopt".to_string()],
    )
    .into()
}

fn git_pull_insecure_advice() -> anyhow::Error {
    RecoveryAdvice::safety_refusal(
        "git_overlay_pull_insecure_unsupported",
        "Git Overlay pull cannot use --insecure",
        "Configure trusted transport credentials and retry without --insecure.",
        "Sley's Git transport does not expose a per-operation insecure TLS policy",
        "silently ignoring the flag would misrepresent the transport security policy",
        "no connection was opened and Git refs, Heddle metadata, the index, and worktree were left unchanged",
        "heddle pull",
        vec!["heddle pull".to_string()],
    )
    .into()
}

fn git_pull_unconfigured_remote_advice(remote: &str) -> anyhow::Error {
    let configure = format!("heddle remote add <name> {remote}");
    RecoveryAdvice::safety_refusal(
        "git_overlay_pull_requires_configured_remote",
        format!("Git Overlay pull requires a configured remote; '{remote}' is not configured"),
        format!("Configure the URL first with `{configure}`, then pull by name."),
        "an unconfigured URL has no durable remote-tracking ref",
        "importing through an internal staging ref would leak transport plumbing into repository history",
        "fetched objects may be cached, but the local branch, Heddle thread, index, and worktree were not advanced",
        configure.clone(),
        vec![configure],
    )
    .into()
}

fn git_pull_diverged_advice(
    local_branch: &str,
    remote: &str,
    remote_branch: &str,
) -> anyhow::Error {
    RecoveryAdvice::safety_refusal(
        "git_overlay_pull_diverged",
        format!("Cannot fast-forward {local_branch} from {remote}/{remote_branch}"),
        "Inspect both histories and reconcile them explicitly before pulling again.",
        "the remote tip is not a descendant of the local branch tip",
        "advancing the branch would discard or merge divergent Git history",
        "the local branch, Heddle thread, index, and worktree remain at their prior tip",
        "heddle status",
        vec!["heddle status".to_string(), "heddle log".to_string()],
    )
    .into()
}

fn git_pull_import_advice(
    remote: &str,
    remote_branch: &str,
    error: &impl std::fmt::Display,
) -> anyhow::Error {
    let retry = format!("heddle pull {remote} {remote_branch}");
    RecoveryAdvice::safety_refusal(
        "git_overlay_pull_import_failed",
        format!("Fetched {remote}/{remote_branch}, but could not prepare Heddle metadata: {error}"),
        format!("The local branch was not advanced. Fix the import error, then retry `{retry}`."),
        format!("Heddle could not map the fetched commit: {error}"),
        "advancing Git before its Heddle state exists would split repository authority",
        "the local branch, index, and worktree remain at their prior tip; the remote-tracking ref records the fetched tip",
        retry.clone(),
        vec![retry, "heddle verify".to_string()],
    )
    .into()
}

fn git_pull_publish_advice(
    local_ref: &str,
    error: &impl std::fmt::Display,
    rollback_error: Option<&anyhow::Error>,
) -> anyhow::Error {
    let (hint, preserved) = match rollback_error {
        Some(rollback) => (
            format!(
                "Publication failed and rollback also failed: {rollback}. Run `heddle verify` before continuing."
            ),
            "the fetched objects and prepared Heddle mapping are durable; the checkout may be detached at the fetched tip",
        ),
        None => (
            "The branch publication was rolled back. Inspect `heddle verify`, then retry the pull."
                .to_string(),
            "the local branch, index, and worktree were restored; fetched objects and remote tracking remain available",
        ),
    };
    RecoveryAdvice::safety_refusal(
        "git_overlay_pull_publish_failed",
        format!("Could not publish the fast-forward to {local_ref}: {error}"),
        hint,
        format!("the checked ref transaction failed: {error}"),
        "continuing without reconciliation could leave Git and Heddle pointers at different tips",
        preserved,
        "heddle verify",
        vec!["heddle verify".to_string(), "heddle pull".to_string()],
    )
    .into()
}

fn git_pull_checkout_advice(local_ref: &str, error: &impl std::fmt::Display) -> anyhow::Error {
    RecoveryAdvice::safety_refusal(
        "git_overlay_pull_checkout_failed",
        format!("Could not prepare the worktree for {local_ref}: {error}"),
        "Run `heddle verify` before retrying the pull.",
        format!("Sley could not complete the detached checkout: {error}"),
        "publishing the branch after an incomplete checkout would split refs from the worktree",
        "the local branch and Heddle thread were not advanced; fetched objects and remote tracking remain available",
        "heddle verify",
        vec!["heddle verify".to_string(), "heddle pull".to_string()],
    )
    .into()
}

fn git_pull_metadata_publish_advice(
    local_branch: &str,
    git_changed: bool,
    error: &impl std::fmt::Display,
    rollback_error: Option<&anyhow::Error>,
) -> anyhow::Error {
    if !git_changed {
        return RecoveryAdvice::safety_refusal(
            "git_overlay_pull_metadata_publish_failed",
            format!("Could not publish the Heddle mapping for {local_branch}: {error}"),
            "The Git branch is already at the fetched tip. Run `heddle verify`, then retry the pull.",
            format!("the Heddle thread update failed: {error}"),
            "continuing without the mapping would leave Heddle metadata behind Git",
            "Git refs, the index, and worktree were left unchanged",
            "heddle verify",
            vec!["heddle verify".to_string(), "heddle pull".to_string()],
        )
        .into();
    }
    let mut advice =
        git_pull_publish_advice(&format!("refs/heads/{local_branch}"), error, rollback_error);
    if let Some(recovery) = advice.downcast_mut::<RecoveryAdvice>() {
        recovery.kind = "git_overlay_pull_metadata_publish_failed";
        recovery.error =
            format!("Git advanced, but Heddle could not publish {local_branch}: {error}");
    }
    advice
}

fn changed_paths_between_states(
    repo: &Repository,
    old_state: Option<&StateId>,
    new_state: Option<&StateId>,
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
    paths.sort_unstable();
    paths.dedup();
    Ok(paths)
}

async fn pull_local(
    repo: &Repository,
    source_path: &std::path::Path,
    remote_thread: &str,
    local_thread: Option<&str>,
    configured_remote_name: Option<&str>,
    plan: &PullPlan,
    cli: &Cli,
) -> Result<()> {
    if plan.lazy {
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

    let objects_copied = source.fetch_state(repo, &state_id)? + source.fetch_markers(repo)?;

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
    if let Some(remote_name) = configured_remote_name {
        repo.refs()
            .set_remote_thread(remote_name, &ThreadName::new(remote_thread), &state_id)?;
    }

    let remote_label = configured_remote_name
        .map(str::to_string)
        .unwrap_or_else(|| source_path.display().to_string());

    if should_output_json(cli, Some(repo.config())) {
        let summary = LocalTransferSummary {
            state: Some(state_id.to_string()),
            objects: Some(objects_copied),
        };
        let output = heddle_pull_output_from_local(
            Some(plan),
            changed,
            remote_label.clone(),
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
            remote_label,
            remote_thread.to_string(),
            &summary,
            build_repository_verification_state(repo),
        );
        let text = format_pull_outcome_text(&output.outcome, 8);
        println!("{} {}", style::ok_marker(), text.headline);
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
            // Read path for hosted discussions (heddle discuss): materialize any
            // new hosted CollaborationService discussions/turns for the pulled
            // head into the local op-log. Best-effort — a fetch hiccup warns
            // rather than failing the pull.
            match crate::client::discussion_sync::pull_discussions(repo, &mut client, repo_path)
                .await
            {
                Ok(count)
                    if count > 0 && !should_output_json(options.cli, Some(repo.config())) =>
                {
                    println!(
                        "{} synced {count} discussion(s) from {}",
                        crate::cli::style::ok_marker(),
                        crate::cli::style::dim(repo_path)
                    );
                }
                Ok(_) => {}
                Err(error) => {
                    eprintln!(
                        "{} discussion sync skipped: {error:#}",
                        crate::cli::style::warn_marker()
                    );
                }
            }
            // Read path for hosted context annotations — same seam as discussions.
            match crate::client::context_sync::pull_context(repo, &mut client, repo_path).await {
                Ok(count)
                    if count > 0 && !should_output_json(options.cli, Some(repo.config())) =>
                {
                    println!(
                        "{} synced {count} annotation(s) from {}",
                        crate::cli::style::ok_marker(),
                        crate::cli::style::dim(repo_path)
                    );
                }
                Ok(_) => {}
                Err(error) => {
                    eprintln!(
                        "{} context sync skipped: {error:#}",
                        crate::cli::style::warn_marker()
                    );
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
    match &command {
        RemoteCommands::List => {
            if let Some(probe) = build_plain_git_verification_probe(start)? {
                render_remote_list(
                    &list_plain_git_remotes(&probe.root),
                    should_output_json(cli, None),
                )?;
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
    if repo.capability() == RepositoryCapability::GitOverlay {
        return cmd_git_overlay_remote(cli, &repo, command);
    }

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

fn cmd_git_overlay_remote(cli: &Cli, repo: &Repository, command: RemoteCommands) -> Result<()> {
    let git = SleyRepository::discover(repo.root()).map_err(anyhow::Error::new)?;
    let json = should_output_json(cli, Some(repo.config()));
    match command {
        RemoteCommands::List => render_remote_list(&list_git_overlay_remotes(repo, &git)?, json),
        RemoteCommands::Show { name } => {
            let output = show_git_overlay_remote(repo, &git, &name)?
                .ok_or_else(|| RecoveryAdvice::remote_not_found(&name))?;
            render_remote_info(&output, json)
        }
        RemoteCommands::Add { name, url } => {
            if git.remote_config_with_sources()?.get(&name).is_some() {
                anyhow::bail!("Remote '{name}' already exists");
            }
            let set = RemoteConfigSet::new(&name)
                .with_url(&url)
                .with_fetch_refspec(format!("+refs/heads/*:refs/remotes/{name}/*"));
            let plan = git
                .plan_remote_set(set, ConfigEditScope::Local)?
                .with_fsync(true);
            git.apply_config_edit_plan(plan)?;
            render_remote_mutation(
                RemoteMutationOutput {
                    output_kind: "remote_add",
                    status: "completed",
                    action: "remote_add",
                    name,
                    url: Some(url),
                    default: resolved_default_remote_name(repo)?,
                    message: "Added remote".to_string(),
                    trust: build_repository_verification_state(repo),
                },
                json,
            )
        }
        RemoteCommands::Remove { name } => {
            remove_git_overlay_remote(&git, &name)?;
            render_remote_mutation(
                RemoteMutationOutput {
                    output_kind: "remote_remove",
                    status: "completed",
                    action: "remote_remove",
                    name,
                    url: None,
                    default: resolved_default_remote_name(repo)?,
                    message: "Removed remote".to_string(),
                    trust: build_repository_verification_state(repo),
                },
                json,
            )
        }
        RemoteCommands::SetDefault { name } => {
            if git.remote_config_with_sources()?.get(&name).is_none() {
                return Err(RecoveryAdvice::remote_not_found(&name).into());
            }
            let branch = repo
                .git_overlay_current_branch()?
                .context("cannot set the Git Overlay default from a detached checkout")?;
            set_git_overlay_default(&git, &branch, &name)?;
            render_remote_mutation(
                RemoteMutationOutput {
                    output_kind: "remote_set_default",
                    status: "completed",
                    action: "remote_set_default",
                    name: name.clone(),
                    url: None,
                    default: Some(name),
                    message: "Set default remote".to_string(),
                    trust: build_repository_verification_state(repo),
                },
                json,
            )
        }
    }
}

fn list_git_overlay_remotes(repo: &Repository, git: &SleyRepository) -> Result<RemoteListReport> {
    let default = resolved_default_remote_name(repo)?;
    let snapshot = git.remote_config_with_sources()?;
    let mut remotes = snapshot
        .remotes
        .into_iter()
        .map(|remote| {
            let url = remote
                .urls()
                .into_iter()
                .next()
                .or_else(|| remote.push_urls().into_iter().next())
                .unwrap_or_default()
                .to_string();
            RemoteInfo {
                output_kind: None,
                is_default: default.as_deref() == Some(remote.name.as_str()),
                name: remote.name,
                url,
                source: "git".to_string(),
            }
        })
        .collect::<Vec<_>>();
    remotes.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(RemoteListReport {
        output_kind: "remote_list",
        remotes,
    })
}

fn show_git_overlay_remote(
    repo: &Repository,
    git: &SleyRepository,
    name: &str,
) -> Result<Option<RemoteInfo>> {
    let default = resolved_default_remote_name(repo)?;
    let snapshot = git.remote_config_with_sources()?;
    Ok(snapshot.get(name).map(|remote| RemoteInfo {
        output_kind: Some("remote_show"),
        name: remote.name.clone(),
        url: remote
            .urls()
            .into_iter()
            .next()
            .or_else(|| remote.push_urls().into_iter().next())
            .unwrap_or_default()
            .to_string(),
        source: "git".to_string(),
        is_default: default.as_deref() == Some(name),
    }))
}

fn remove_git_overlay_remote(git: &SleyRepository, name: &str) -> Result<()> {
    let snapshot = git.remote_config_with_sources()?;
    let remote = snapshot
        .get(name)
        .ok_or_else(|| RecoveryAdvice::remote_not_found(name))?;
    let mut paths = Vec::new();
    for source in &remote.sources {
        if let Some(RemoteConfigRefusal::ExternalInclude { path }) = &source.refusal {
            return Err(git_remote_external_config_advice(name, path));
        }
        if let Some(path) = &source.target_path {
            if !git_owns_config_path(git, path) {
                return Err(git_remote_external_config_advice(name, path));
            }
            if paths.contains(path) {
                continue;
            }
            paths.push(path.clone());
        }
    }
    if paths.is_empty() {
        anyhow::bail!("Remote '{name}' is not defined in an editable repository config");
    }
    if paths.len() != 1 {
        anyhow::bail!(
            "Remote '{name}' is defined in multiple Git config files; refusing a non-atomic removal"
        );
    }
    let plan = git
        .plan_remote_remove(
            RemoteConfigRemove::new(name),
            ConfigEditScope::Path(paths.remove(0)),
        )?
        .with_fsync(true);
    git.apply_config_edit_plan(plan)?;
    Ok(())
}

fn git_owns_config_path(git: &SleyRepository, path: &Path) -> bool {
    let target = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    [git.git_dir(), git.common_dir()].into_iter().any(|root| {
        let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
        target.starts_with(root)
    })
}

fn git_remote_external_config_advice(name: &str, path: &Path) -> anyhow::Error {
    RecoveryAdvice::safety_refusal(
        "git_remote_in_included_config",
        format!(
            "Remote '{name}' is defined outside this repository: {}",
            path.display()
        ),
        "Move the remote into this repository's Git config, then retry.",
        "the remote is owned by an external Git config file",
        "editing it would change configuration shared with other repositories",
        "the repository Git config and Heddle metadata were left unchanged",
        "heddle remote show <name>",
        vec!["heddle remote show <name>".to_string()],
    )
    .into()
}

fn set_git_overlay_default(git: &SleyRepository, branch: &str, name: &str) -> Result<()> {
    let branch_remote = format!("branch.{branch}.remote");
    let branch_merge = format!("branch.{branch}.merge");
    let merge_target = git
        .config_snapshot()?
        .get("branch", Some(branch), "merge")
        .map(str::to_string)
        .unwrap_or_else(|| format!("refs/heads/{branch}"));
    let mut plan = ConfigEditPlan::new(git.common_dir().join("config"))
        .with_operation(ConfigEdit::set("remote.pushDefault", name)?)
        .with_fsync(true);
    plan = plan
        .with_operation(ConfigEdit::set(&branch_remote, name)?)
        .with_operation(ConfigEdit::set(&branch_merge, merge_target)?);
    git.apply_config_edit_plan(plan).map_err(anyhow::Error::new)
}

fn render_remote_mutation(output: RemoteMutationOutput, json: bool) -> Result<()> {
    if json {
        crate::cli::render::write_json_stdout(&output)?;
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
        crate::cli::render::write_json_stdout(output)?;
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
        crate::cli::render::write_json_stdout(output)?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn git_pull_progress_keeps_transfer_phase_and_exact_counts() {
        let progress = objects::Progress::null();
        progress.set_phase("streaming Git objects");
        let mut pull = GitPullProgress {
            progress: progress.clone(),
            received_bytes: 0,
            received_objects: 0,
        };

        pull.transfer(TransferProgress {
            received_bytes: 1024,
            received_objects: 3,
            total_objects: Some(8),
            indexed_deltas: 0,
        });
        pull.transfer(TransferProgress {
            received_bytes: 4096,
            received_objects: 5,
            total_objects: Some(8),
            indexed_deltas: 1,
        });
        pull.message("remote: counting objects");

        assert_eq!(pull.received_bytes, 4096);
        assert_eq!(pull.received_objects, 5);
        assert_eq!(progress.done(), 5);
        assert_eq!(progress.total(), 8);
        assert_eq!(progress.phase(), "streaming Git objects");
    }

    #[test]
    fn transfer_byte_formatter_uses_binary_units() {
        assert_eq!(format_transfer_bytes(42), "42 B");
        assert_eq!(format_transfer_bytes(1536), "1.5 KiB");
        assert_eq!(format_transfer_bytes(3 * 1024 * 1024), "3.0 MiB");
    }
}
