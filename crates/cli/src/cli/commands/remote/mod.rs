// SPDX-License-Identifier: Apache-2.0
//! Remote operations (push, pull, remote management).

#[cfg(feature = "client")]
use std::net::SocketAddr;
use std::path::Path;

use anyhow::{Context, Result, anyhow};
use heddle_core::{
    GitOverlayPushTracking, GitRemoteConfigured, LocalTransferSummary, PushFailure, PushOutcome,
    PushPath, PushPlan, PushPlanRequest, RemotePreflightBlocker, build_push_outcome,
    first_multi_thread_push_failure, format_multi_ref_push_progress, format_push_outcome_text,
    format_pushing_to, git_overlay_push_execution_facts,
    heddle_single_push_execution_facts_from_local, is_native_transport_mismatch,
    looks_like_git_remote_url, looks_like_remote_location, multi_ref_push_begin,
    multi_ref_thread_failed, multi_ref_thread_succeeded_local, multi_thread_push_execution_facts,
    named_thread_tip_mismatch_failure, plan_push, refuse_named_thread_tip_overwrite,
    remote_urls_match, resolve_default_push_remote_name, transport_error_message,
};
#[cfg(feature = "client")]
use heddle_core::{
    HostedPushPlan, HostedPushResult, HostedPushResultFields, format_connected_to,
    format_remote_state_detail, heddle_single_push_execution_facts, hosted_spool_display_path,
    message_indicates_already_exists, multi_ref_progress_from_hosted_thread,
    parse_hosted_push_result, redact_internal_hosted_paths, remote_push_failure,
};
use heddle_git_projection::{
    credential::EmbeddingSafeCredentialProvider,
    git_core::{
        AuthoritativeGitPushOptions, GitPushScope, push_authoritative_git_refs, set_reference,
    },
};
use objects::object::ThreadName;
use repo::{Repository, RepositoryCapability};
use serde::Serialize;
use sley::{
    ConfigEdit, ConfigEditPlan, FullName, RefPrecondition, RemoteConfigSet,
    Repository as SleyRepository,
    remote::{PackGenerationProgress, ProgressSink as SleyProgressSink},
};
#[cfg(feature = "client")]
use weft_client_shim::CliContext as _;
#[cfg(feature = "client")]
use wire::ProtocolError;

#[cfg(feature = "client")]
use super::action_line::print_next;
use super::{
    advice::RecoveryAdvice,
    auto_capture::{AutoCaptureTrigger, auto_capture_command_boundary},
    command_catalog::{ActionFields, ActionTemplate},
    snapshot::ensure_current_state,
    verification_health::{RepositoryVerificationState, build_repository_verification_state},
};
#[cfg(feature = "client")]
use crate::cli::progress_render::clear_line;
#[cfg(feature = "client")]
use crate::client::HostedGrpcClient;
#[cfg(feature = "client")]
use crate::client::{HostedAuthMode, HostedSession};
#[cfg(feature = "client")]
use crate::remote::Remote;
use crate::{
    cli::{
        Cli,
        progress_render::{finish_line, progress_for},
        should_output_json, style,
    },
    client::LocalSync,
    config::UserConfig,
    remote::{RemoteConfig, RemoteTarget, resolve_remote_with_key},
};

mod remote_ops;

pub use remote_ops::{cmd_pull, cmd_remote};
pub(crate) use remote_ops::{resolve_default_remote_name, resolved_default_remote_name};

#[allow(clippy::type_complexity)]
/// CLI machine envelope: domain [`PushOutcome`] plus verification next-actions.
#[derive(Debug, Clone, Serialize)]
struct PushOutput {
    #[serde(flatten)]
    outcome: PushOutcome,
    next_action: Option<String>,
    next_action_template: Option<ActionTemplate>,
    recommended_action: Option<String>,
    recommended_action_template: Option<ActionTemplate>,
    #[serde(rename = "verification")]
    trust: RepositoryVerificationState,
}

fn push_output_from_outcome(
    outcome: PushOutcome,
    trust: RepositoryVerificationState,
) -> PushOutput {
    let action = ActionFields::from_action(&trust.recommended_action);
    PushOutput {
        outcome,
        next_action: action.action.clone(),
        next_action_template: action.template.clone(),
        recommended_action: action.action,
        recommended_action_template: action.template,
        trust,
    }
}

#[derive(Debug, Clone)]
struct GitOverlayTrackingRefresh {
    remote_name: String,
    configured_remote: Option<GitOverlayConfiguredRemote>,
    upstream_branch: Option<String>,
}

#[derive(Debug, Clone)]
struct GitOverlayConfiguredRemote {
    name: String,
    url: String,
}

struct GitPushProgress {
    progress: objects::Progress,
}

impl SleyProgressSink for GitPushProgress {
    fn pack_generation(&mut self, event: &PackGenerationProgress) {
        self.progress.set_phase("streaming Git objects");
        self.progress.set_total(event.total_objects);
        self.progress
            .inc(event.total_objects.saturating_sub(self.progress.done()));
    }

    fn message(&mut self, message: &str) {
        if !message.trim().is_empty() {
            self.progress.set_phase(message);
        }
    }
}

#[allow(clippy::type_complexity)]
fn push_git_overlay_refs(
    cli: &Cli,
    repo: &Repository,
    remote: Option<&str>,
    all_threads: bool,
    force: bool,
) -> Result<(
    String,
    Option<String>,
    Option<GitOverlayTrackingRefresh>,
    Vec<String>,
    RepositoryVerificationState,
)> {
    let remote_name = resolve_default_push_remote_name(repo, remote)?;
    let git = SleyRepository::discover(repo.root()).map_err(anyhow::Error::new)?;
    let current_thread = if all_threads {
        None
    } else {
        Some(
            repo.git_overlay_current_branch()?
                .filter(|branch| !branch.is_empty())
                .ok_or_else(|| {
                    anyhow!("cannot push a detached Git checkout without --all-threads")
                })?,
        )
    };
    let scope = if all_threads {
        GitPushScope::AllThreads
    } else {
        GitPushScope::CurrentThread
    };
    let mut credentials =
        EmbeddingSafeCredentialProvider::new(&git.config_snapshot().map_err(anyhow::Error::new)?);
    let progress = progress_for(cli, repo);
    let mut sley_progress = GitPushProgress {
        progress: progress.clone(),
    };
    let refs_written = push_authoritative_git_refs(
        &git,
        AuthoritativeGitPushOptions {
            heddle_dir: repo.heddle_dir(),
            remote: &remote_name,
            scope,
            current_branch: current_thread.as_deref(),
            force,
        },
        &mut credentials,
        &mut sley_progress,
    )
    .map_err(|error| {
        if RecoveryAdvice::from_git_projection_error(&error).is_some() {
            anyhow::Error::new(error)
        } else {
            map_push_failure(PushFailure::RemoteFailed {
                track_name: remote_name.clone(),
                error: error.to_string(),
            })
        }
    })?;
    finish_line(&progress, "[done] pushed Git refs");
    let tracking = refresh_git_tracking_after_overlay_push(repo, &remote_name)?;
    let trust = build_repository_verification_state(repo);
    Ok((remote_name, current_thread, tracking, refs_written, trust))
}

fn git_overlay_push_output(
    plan: &PushPlan,
    remote_name: String,
    current_thread: Option<String>,
    tracking_refresh: Option<GitOverlayTrackingRefresh>,
    refs_written: Vec<String>,
    trust: RepositoryVerificationState,
) -> PushOutput {
    let tracking = tracking_refresh.map(|refresh| GitOverlayPushTracking {
        remote_name: refresh.remote_name,
        configured_remote: refresh.configured_remote.map(|remote| GitRemoteConfigured {
            name: remote.name,
            url: remote.url,
        }),
        upstream_branch: refresh.upstream_branch,
    });
    let outcome = build_push_outcome(
        plan,
        git_overlay_push_execution_facts(remote_name, current_thread, refs_written, tracking),
    );
    push_output_from_outcome(outcome, trust)
}

/// Execute push command.
///
/// Pure orchestration (`plan_push`) runs first; network / git I/O bodies stay
/// in this module and consume plan fields.
///
#[allow(clippy::too_many_arguments)]
pub async fn cmd_push(
    cli: &Cli,
    remote: Option<String>,
    thread: Option<String>,
    state: Option<String>,
    force: bool,
    all_threads: bool,
    insecure: bool,
) -> Result<()> {
    let repo = cli.open_repo()?;
    if repo.capability() == RepositoryCapability::GitOverlay && state.is_some() {
        return Err(git_overlay_push_state_advice());
    }
    if let Some(remote_name) = remote.as_deref() {
        ensure_remote_arg_resolves(&repo, remote_name)?;
    }

    let has_default_remote = if repo.capability() == RepositoryCapability::GitOverlay {
        resolve_default_push_remote_name(&repo, None).is_ok()
    } else {
        resolved_default_remote_name(&repo)?.is_some()
    };
    let push_uses_hosted_network = push_target_is_hosted_network(&repo, remote.as_deref());
    // Match preflight_native_remote_transport: overlay capability never
    // treats a git URL as a native-transport mismatch.
    let remote_transport = classify_push_remote_spec(&repo, remote.as_deref());
    let remote_is_git_local_or_url = matches!(
        remote_transport,
        Some(RemoteTransportKind::LocalGit | RemoteTransportKind::GitUrl)
    );
    let transport_mismatch =
        is_native_transport_mismatch(repo.capability(), remote_is_git_local_or_url);
    let head = repo.head_ref()?;
    let plan = plan_push(&PushPlanRequest {
        capability: repo.capability(),
        uses_hosted_network: push_uses_hosted_network,
        remote: remote.clone(),
        has_default_remote,
        thread: thread.clone(),
        all_threads,
        force,
        head,
        native_local_heddle_target: matches!(
            remote_transport,
            Some(RemoteTransportKind::LocalHeddle)
        ),
        transport_mismatch,
    })
    .map_err(|blocker| map_remote_preflight_blocker(blocker, "push", remote.as_deref()))?;
    if insecure && matches!(plan.path, PushPath::LocalGitOverlayRefs { .. }) {
        return Err(git_overlay_push_insecure_advice());
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
    if state.is_none() {
        auto_capture_command_boundary(cli, &repo, &user_config, AutoCaptureTrigger::Push)?;
    }

    match &plan.path {
        PushPath::LocalNativeHeddle { .. } | PushPath::NativeRemote { .. } => {
            // Transport mismatch already refused by plan_push.
        }
        PushPath::LocalGitOverlayRefs {
            all_threads: path_all_threads,
        } => {
            let (remote_name, current_thread, tracking, refs_written, trust) =
                push_git_overlay_refs(
                    cli,
                    &repo,
                    remote.as_deref(),
                    *path_all_threads,
                    plan.force,
                )?;
            let output = git_overlay_push_output(
                &plan,
                remote_name,
                current_thread,
                tracking,
                refs_written,
                trust,
            );
            if should_output_json(cli, Some(repo.config())) {
                crate::cli::render::write_json_stdout(&output)?;
            } else {
                let text = format_push_outcome_text(&output.outcome, None);
                println!("{} {}", style::ok_marker(), text.headline);
                for line in &text.detail_lines {
                    println!("{line}");
                }
            }
            run_post_push_hook(&hook_manager, &hook_ctx, remote.as_deref());
            return Ok(());
        }
    }

    #[cfg(not(feature = "client"))]
    let token = user_config.remote_token()?;
    #[cfg(feature = "client")]
    let (target, server_key) = resolve_push_target_with_key(&repo, remote.as_deref())?;
    #[cfg(not(feature = "client"))]
    let (target, _server_key) = resolve_push_target_with_key(&repo, remote.as_deref())?;

    // Prevalidate auth/TLS config (including the credential-store fallback)
    // before any irreversible state mutation below; a rejected security
    // config must leave no partial state behind.
    #[cfg(feature = "client")]
    let network_session = if matches!(target, RemoteTarget::Network { .. }) {
        let allow_insecure =
            insecure || cli_shared::remote_allows_insecure(&repo, remote.as_deref());
        Some(
            HostedSession::build(&user_config, server_key, HostedAuthMode::CredentialFallback)?
                .with_allow_insecure(allow_insecure),
        )
    } else {
        None
    };
    // Builds without the `client` feature can't push over the network, but
    // must still fail closed on a bad TLS/auth config before bootstrapping
    // local state — matching the prevalidation the `client` build runs above.
    #[cfg(not(feature = "client"))]
    if matches!(target, RemoteTarget::Network { .. }) {
        user_config.heddle_client_config(token.clone())?;
    }

    // `--all-threads` fans out over every pushable thread (heddle#838);
    // otherwise push the current checkout state, guarding against overwriting
    // a mismatched existing named thread (heddle#837). `--all-threads`
    // supersedes an explicit single-thread `--state`/`[THREAD]` — it pushes
    // everything.
    let single_state_id = if plan.all_threads {
        None
    } else {
        Some(resolve_push_state_id(
            &repo,
            &user_config,
            state,
            thread.as_deref(),
            plan.force,
        )?)
    };

    let track_name = plan.track_name.as_str();

    match target {
        RemoteTarget::Local(path) => {
            if plan.all_threads {
                push_local_all_threads(&repo, &path, &plan, cli).await?;
            } else {
                let state_id = single_state_id
                    .as_ref()
                    .expect("single-thread push resolves a state");
                push_local(&repo, &path, state_id, track_name, &plan, cli).await?;
            }
        }
        RemoteTarget::Network { addr, repo_path } => {
            #[cfg(feature = "client")]
            push_network(
                &repo,
                PushNetworkOptions {
                    addr,
                    repo_path: repo_path.as_deref(),
                    remote_arg: remote.as_deref(),
                    session: network_session
                        .as_ref()
                        .context("network client config was not prevalidated")?,
                    state_id: single_state_id.as_ref(),
                    track_name,
                    force: plan.force,
                    plan: &plan,
                    cli,
                },
            )
            .await?;
            #[cfg(not(feature = "client"))]
            let _ = (addr, repo_path, token, single_state_id, insecure);
            #[cfg(not(feature = "client"))]
            anyhow::bail!(RecoveryAdvice::network_feature_unavailable("push"));
        }
    }

    run_post_push_hook(&hook_manager, &hook_ctx, remote.as_deref());

    Ok(())
}

fn git_overlay_push_state_advice() -> anyhow::Error {
    RecoveryAdvice::safety_refusal(
        "git_overlay_push_state_unsupported",
        "Git Overlay push cannot use --state",
        "Push the current Git branch, or adopt the repository before pushing a Heddle state.",
        "Git Overlay publishes Git refs rather than Heddle state identifiers",
        "accepting --state would imply a state-selection behavior the Git transport cannot honor",
        "no hook ran and repository, remote, index, and worktree state were left unchanged",
        "heddle push",
        vec!["heddle push".to_string(), "heddle adopt".to_string()],
    )
    .into()
}

fn git_overlay_push_insecure_advice() -> anyhow::Error {
    RecoveryAdvice::safety_refusal(
        "git_overlay_push_insecure_unsupported",
        "Git Overlay push cannot use --insecure",
        "Configure trusted transport credentials and retry without --insecure.",
        "Sley's Git transport does not expose a per-operation insecure TLS policy",
        "silently ignoring the flag would misrepresent the transport security policy",
        "no hook ran, no connection was opened, and repository, remote, index, and worktree state were left unchanged",
        "heddle push",
        vec!["heddle push".to_string()],
    )
    .into()
}

pub(super) fn map_remote_preflight_blocker(
    blocker: RemotePreflightBlocker,
    action: &str,
    remote_arg: Option<&str>,
) -> anyhow::Error {
    match blocker {
        RemotePreflightBlocker::MissingRemote => {
            anyhow!(RecoveryAdvice::remote_not_configured(action))
        }
        RemotePreflightBlocker::TransportMismatch => anyhow!(
            RecoveryAdvice::remote_transport_mismatch(action, remote_arg.unwrap_or("<default>"))
        ),
        RemotePreflightBlocker::GitOverlayThreadMismatch {
            requested,
            attached,
        } => {
            let failure =
                PushFailure::Preflight(RemotePreflightBlocker::GitOverlayThreadMismatch {
                    requested: requested.clone(),
                    attached: attached.clone(),
                });
            let attached_label = attached
                .as_deref()
                .map(|t| format!("'{t}'"))
                .unwrap_or_else(|| "detached HEAD".to_string());
            anyhow!(RecoveryAdvice::safety_refusal(
                failure.advice_kind(),
                format!(
                    "git-overlay push targets the attached thread; requested '{requested}' but HEAD is {attached_label}"
                ),
                failure.recovery_hint(),
                format!(
                    "requested thread '{requested}' is not the attached git-overlay HEAD thread"
                ),
                "pushing a non-attached git-overlay thread would ship the wrong branch ref",
                "repository state, refs, remote configuration, and worktree files were left unchanged",
                failure.primary_command(),
                vec![
                    failure.primary_command(),
                    "heddle push --all-threads".to_string(),
                ],
            ))
        }
    }
}

/// Map a typed [`PushFailure`] to RecoveryAdvice / anyhow for CLI exit.
pub(super) fn map_push_failure(failure: PushFailure) -> anyhow::Error {
    match failure {
        PushFailure::Preflight(blocker) => map_remote_preflight_blocker(blocker, "push", None),
        PushFailure::NamedThreadTipMismatch {
            thread,
            tip_short,
            current_short,
        } => {
            let failure = named_thread_tip_mismatch_failure(&thread, &tip_short, &current_short);
            anyhow!(RecoveryAdvice::safety_refusal(
                failure.advice_kind(),
                failure.to_string(),
                failure.recovery_hint(),
                format!(
                    "named thread '{thread}' tip {tip_short} differs from current checkout {current_short}"
                ),
                format!(
                    "pushing the current checkout under '{thread}' would overwrite a mismatched thread tip"
                ),
                "repository state, refs, remote configuration, and worktree files were left unchanged",
                failure.primary_command(),
                vec![
                    failure.primary_command(),
                    format!("heddle push --force {thread}"),
                ],
            ))
        }
        PushFailure::RemoteFailed { track_name, error } => {
            #[cfg(feature = "client")]
            {
                anyhow!(RecoveryAdvice::remote_push_failed(&track_name, &error))
            }
            #[cfg(not(feature = "client"))]
            {
                anyhow!("failed to push thread '{track_name}': {error}")
            }
        }
    }
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

#[cfg(feature = "client")]
fn heddle_push_output(
    plan: &PushPlan,
    state: Option<String>,
    objects: Option<usize>,
    trust: RepositoryVerificationState,
) -> PushOutput {
    let outcome = build_push_outcome(plan, heddle_single_push_execution_facts(state, objects));
    push_output_from_outcome(outcome, trust)
}

fn heddle_push_output_from_local(
    plan: &PushPlan,
    summary: &LocalTransferSummary,
    trust: RepositoryVerificationState,
) -> PushOutput {
    let outcome = build_push_outcome(plan, heddle_single_push_execution_facts_from_local(summary));
    push_output_from_outcome(outcome, trust)
}

/// JSON output for a native `--all-threads` push (heddle#838). `refs_written`
/// lists exactly the thread names that were pushed (the issue's explicit ask),
/// sorted; `success`/`pushed` are false if any thread failed. `push_scope`
/// mirrors the git-overlay path's `"all_threads"`.
fn heddle_all_threads_push_output(
    plan: &PushPlan,
    pushed: Vec<String>,
    failures: &[(String, String)],
    objects: usize,
    trust: RepositoryVerificationState,
) -> PushOutput {
    let outcome = build_push_outcome(
        plan,
        multi_thread_push_execution_facts(pushed, failures, objects),
    );
    push_output_from_outcome(outcome, trust)
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
        .map_err(anyhow::Error::new)?
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

fn refresh_git_tracking_after_overlay_push(
    repo: &Repository,
    requested: &str,
) -> Result<Option<GitOverlayTrackingRefresh>> {
    let Some(branch) = repo
        .git_overlay_current_branch()?
        .filter(|branch| !branch.is_empty())
    else {
        return Ok(None);
    };
    let git = SleyRepository::discover(repo.root()).map_err(anyhow::Error::new)?;
    let Some(head) = git.head().ok().and_then(|head| head.oid) else {
        return Ok(None);
    };
    let Some(tracking_remote) = resolve_git_tracking_remote(repo, requested)? else {
        return Ok(None);
    };
    let remote_ref = format!("refs/remotes/{}/{branch}", tracking_remote.name);
    set_reference(
        &git,
        &remote_ref,
        head,
        RefPrecondition::Any,
        &format!("heddle: push to {requested}"),
    )
    .map_err(|error| {
        anyhow!(RecoveryAdvice::safety_refusal(
            "git_overlay_tracking_refresh_failed",
            format!("Push succeeded, but {remote_ref} could not be refreshed: {error}"),
            "Run `heddle verify`, then refresh tracking before the next push.",
            format!("Sley could not publish the local tracking ref: {error}"),
            "continuing could report stale upstream distance",
            "the remote push completed; source history and worktree files were not rewritten",
            "heddle verify",
            vec!["heddle verify".to_string()],
        ))
    })?;
    write_git_overlay_branch_upstream(repo.root(), &branch, &tracking_remote.name)?;
    Ok(Some(GitOverlayTrackingRefresh {
        remote_name: tracking_remote.name,
        configured_remote: tracking_remote.configured_remote,
        upstream_branch: Some(branch),
    }))
}

struct GitTrackingRemoteResolution {
    name: String,
    configured_remote: Option<GitOverlayConfiguredRemote>,
}

fn resolve_git_tracking_remote(
    repo: &Repository,
    requested: &str,
) -> Result<Option<GitTrackingRemoteResolution>> {
    let git = SleyRepository::discover(repo.root()).map_err(anyhow::Error::new)?;
    for name in git_remote_names(repo.root())? {
        let config = git.config_snapshot().map_err(anyhow::Error::new)?;
        let url = config
            .get("remote", Some(&name), "pushurl")
            .or_else(|| config.get("remote", Some(&name), "url"));
        if url.is_some_and(|url| remote_urls_match(url, requested)) {
            return Ok(Some(GitTrackingRemoteResolution {
                name,
                configured_remote: None,
            }));
        }
    }
    if !looks_like_remote_location(requested)
        && FullName::try_from(format!("refs/remotes/{requested}/HEAD").as_str()).is_ok()
    {
        return Ok(Some(GitTrackingRemoteResolution {
            name: requested.to_string(),
            configured_remote: None,
        }));
    }
    if git_remote_names(repo.root())?.is_empty() && !requested.trim().is_empty() {
        write_git_overlay_remote(repo.root(), "origin", requested)?;
        return Ok(Some(GitTrackingRemoteResolution {
            name: "origin".to_string(),
            configured_remote: Some(GitOverlayConfiguredRemote {
                name: "origin".to_string(),
                url: requested.to_string(),
            }),
        }));
    }
    Ok(None)
}

fn git_remote_names(root: &Path) -> Result<Vec<String>> {
    let git = match SleyRepository::discover(root) {
        Ok(git) => git,
        Err(_) => return Ok(Vec::new()),
    };
    Ok(git
        .remote_names()?
        .into_iter()
        .filter(|name| !name.is_empty())
        .collect())
}

fn write_git_overlay_branch_upstream(root: &Path, branch: &str, remote: &str) -> Result<()> {
    let git = SleyRepository::discover(root).map_err(anyhow::Error::new)?;
    let branch_remote = format!("branch.{branch}.remote");
    let branch_merge = format!("branch.{branch}.merge");
    let plan = ConfigEditPlan::new(git.common_dir().join("config"))
        .with_operation(ConfigEdit::set(&branch_remote, remote)?)
        .with_operation(ConfigEdit::set(
            &branch_merge,
            format!("refs/heads/{branch}"),
        )?)
        .with_fsync(true);
    git.apply_config_edit_plan(plan)
        .map_err(anyhow::Error::new)?;
    Ok(())
}

fn write_git_overlay_remote(root: &Path, name: &str, url: &str) -> Result<()> {
    let git = SleyRepository::discover(root).map_err(anyhow::Error::new)?;
    let remote = RemoteConfigSet::new(name)
        .with_url(url)
        .with_fetch_refspec(format!("+refs/heads/*:refs/remotes/{name}/*"));
    let plan = ConfigEditPlan::new(git.common_dir().join("config"))
        .with_operation(ConfigEdit::replace_section(
            "remote",
            Some(remote.name),
            remote.entries,
        ))
        .with_fsync(true);
    git.apply_config_edit_plan(plan)
        .map_err(anyhow::Error::new)?;
    Ok(())
}

pub(super) fn push_target_is_hosted_network(repo: &Repository, remote_arg: Option<&str>) -> bool {
    matches!(
        classify_push_remote_spec(repo, remote_arg),
        Some(RemoteTransportKind::NetworkHeddle)
    )
}

pub(super) fn pull_target_is_hosted_network(repo: &Repository, remote_arg: Option<&str>) -> bool {
    matches!(
        classify_pull_remote_spec(repo, remote_arg),
        Some(RemoteTransportKind::NetworkHeddle)
    )
}

fn resolve_push_target_with_key(
    repo: &Repository,
    remote_arg: Option<&str>,
) -> Result<(RemoteTarget, Option<String>)> {
    if repo.capability() != RepositoryCapability::GitOverlay {
        return resolve_remote_with_key(repo, remote_arg).map_err(anyhow::Error::msg);
    }
    let remote = resolve_default_push_remote_name(repo, remote_arg)?;
    let git = SleyRepository::discover(repo.root()).map_err(anyhow::Error::new)?;
    let config = git.config_snapshot().map_err(anyhow::Error::new)?;
    let spec = config
        .get("remote", Some(&remote), "pushurl")
        .or_else(|| config.get("remote", Some(&remote), "url"))
        .unwrap_or(remote.as_str());
    let target = RemoteTarget::parse(spec).map_err(anyhow::Error::msg)?;
    let key = cli_shared::remote::credential_key_from_remote_url(spec);
    Ok((target, key))
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
    match classify_push_remote_spec(repo, remote_arg) {
        Some(RemoteTransportKind::LocalGit | RemoteTransportKind::GitUrl) => Err(anyhow!(
            RecoveryAdvice::remote_transport_mismatch(action, remote_arg.unwrap_or("<default>"))
        )),
        _ => Ok(()),
    }
}

fn classify_push_remote_spec(
    repo: &Repository,
    remote_arg: Option<&str>,
) -> Option<RemoteTransportKind> {
    classify_remote_spec(repo, remote_arg, RemoteAccess::Push)
}

pub(super) fn classify_pull_remote_spec(
    repo: &Repository,
    remote_arg: Option<&str>,
) -> Option<RemoteTransportKind> {
    classify_remote_spec(repo, remote_arg, RemoteAccess::Fetch)
}

#[derive(Debug, Clone, Copy)]
enum RemoteAccess {
    Fetch,
    Push,
}

fn classify_remote_spec(
    repo: &Repository,
    remote_arg: Option<&str>,
    access: RemoteAccess,
) -> Option<RemoteTransportKind> {
    let spec = remote_spec_for_transport(repo, remote_arg, access)?;
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

fn remote_spec_for_transport(
    repo: &Repository,
    remote_arg: Option<&str>,
    access: RemoteAccess,
) -> Option<String> {
    if repo.capability() == RepositoryCapability::GitOverlay {
        let git = SleyRepository::discover(repo.root()).ok()?;
        let config = git.config_snapshot().ok()?;
        let name = match remote_arg {
            Some(arg) if RemoteTarget::parse(arg).is_ok() || looks_like_remote_location(arg) => {
                return Some(arg.to_string());
            }
            Some(arg) => arg.to_string(),
            None => match access {
                RemoteAccess::Fetch => resolve_default_remote_name(repo, None).ok()?,
                RemoteAccess::Push => resolve_default_push_remote_name(repo, None).ok()?,
            },
        };
        let configured = match access {
            RemoteAccess::Fetch => config.get("remote", Some(&name), "url"),
            RemoteAccess::Push => config
                .get("remote", Some(&name), "pushurl")
                .or_else(|| config.get("remote", Some(&name), "url")),
        };
        return configured.map(str::to_string).or(Some(name));
    }
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

/// Resolve the state a `push` should upload for a single-thread push
/// (heddle#837).
///
/// A push always ships the CURRENT checkout state — it never resolves the
/// named thread's own tip. The heddle#837 fix is a data-integrity *guard*
/// against silently overwriting a mismatched existing thread, not a change to
/// which state gets pushed.
///
/// Precedence:
/// 1. `--state <spec>` explicit → resolve that spec (unchanged behavior); no
///    thread guard applies (the user asked for a specific state).
/// 2. No `--state` → the current checkout state, bootstrapping git-overlay if
///    needed ([`ensure_current_state`]). When a thread was named explicitly
///    (positional `[THREAD]` or `--thread`):
///    - the thread does not exist locally → allow (this is `push` creating the
///      thread on the remote from the current state);
///    - the thread exists and its tip == the current state → allow (they match);
///    - the thread exists and its tip != the current state → REFUSE unless
///      `--force`, so we never push the current checkout's state under a
///      DIFFERENT existing thread's ref by accident.
///
/// Used by every native/hosted/local single-thread push arm so they share the
/// same guard. The git-overlay refs path keeps its own refuse-guard (it pushes
/// Git branches, not resolved states); `--all-threads` bypasses this entirely
/// (each thread is pushed at its own tip).
fn resolve_push_state_id(
    repo: &Repository,
    user_config: &UserConfig,
    state: Option<String>,
    thread: Option<&str>,
    force: bool,
) -> Result<objects::object::StateId> {
    if let Some(state_str) = state {
        if matches!(state_str.as_str(), "HEAD" | "@") && repo.current_state()?.is_none() {
            ensure_current_state(
                repo,
                user_config,
                Some("Bootstrap git-overlay before push".to_string()),
            )?;
        }
        return repo.resolve_state(&state_str)?.context("State not found");
    }

    let current = ensure_current_state(
        repo,
        user_config,
        Some("Bootstrap git-overlay before push".to_string()),
    )?;

    // heddle#837 guard: pure refuse decision; I/O only gathers tip/current facts.
    // A non-existent named thread falls through (push creates it on the remote).
    if let Some(thread_name) = thread {
        let tip = repo.refs().get_thread(&ThreadName::new(thread_name))?;
        let existing_tip_differs = tip.as_ref().is_some_and(|tip| tip != &current);
        if refuse_named_thread_tip_overwrite(force, Some(thread_name), existing_tip_differs) {
            let tip = tip.expect("existing_tip_differs implies tip present");
            return Err(map_push_failure(named_thread_tip_mismatch_failure(
                thread_name,
                tip.short().to_string(),
                current.short().to_string(),
            )));
        }
    }

    Ok(current)
}

async fn push_local(
    repo: &Repository,
    target_path: &std::path::Path,
    state_id: &objects::object::StateId,
    track_name: &str,
    plan: &PushPlan,
    cli: &Cli,
) -> Result<()> {
    let target_label = format!("file://{}", target_path.display());
    if !should_output_json(cli, Some(repo.config())) {
        let working = format_pushing_to(&target_label);
        if let Some(target) = working.strip_prefix("pushing to ") {
            println!(
                "{} pushing to {}",
                style::working_marker(),
                style::dim(target)
            );
        } else {
            println!("{} {working}", style::working_marker());
        }
    }

    let target_repo = Repository::open(target_path)?;

    let sync = LocalSync::open(repo.root())?;
    let objects_copied = sync.fetch_state(&target_repo, state_id)?;

    target_repo
        .refs()
        .set_thread(&ThreadName::new(track_name), state_id)?;

    if should_output_json(cli, Some(repo.config())) {
        let trust = build_repository_verification_state(repo);
        let summary = LocalTransferSummary {
            state: Some(state_id.to_string()),
            objects: Some(objects_copied),
        };
        let output = heddle_push_output_from_local(plan, &summary, trust);
        crate::cli::render::write_json_stdout(&output)?;
    } else {
        // Domain owns unstyled text; CLI styles the primary line + detail lines.
        let trust = build_repository_verification_state(repo);
        let summary = LocalTransferSummary {
            state: Some(state_id.short().to_string()),
            objects: Some(objects_copied),
        };
        let output = heddle_push_output_from_local(plan, &summary, trust);
        let text = format_push_outcome_text(&output.outcome, Some(track_name));
        println!(
            "{} pushed {} to {} ({})",
            style::ok_marker(),
            style::state_id(&state_id.short().to_string()),
            style::bold(track_name),
            style::count(objects_copied, "object")
        );
        debug_assert!(
            text.headline.contains(track_name),
            "domain headline should name the track: {}",
            text.headline
        );
        for line in &text.detail_lines {
            println!("{line}");
        }
    }

    Ok(())
}

/// A pushable Heddle thread paired with its tip state (heddle#838).
struct PushableThread {
    name: String,
    state: objects::object::StateId,
}

/// Enumerate the threads `--all-threads` should push on the native/hosted
/// path (heddle#838): every heddle-managed thread, with remote-tracking
/// names filtered out exactly as the Git exporter does
/// ([`git_export::is_remote_tracking_thread_name`]). Each thread's state is
/// resolved from its own tip (composes with the heddle#837 fix). Sorted by
/// name for deterministic output. Threads whose ref cannot be resolved to a
/// state are skipped (they carry no pushable state).
fn pushable_threads_for_all(repo: &Repository) -> Result<Vec<PushableThread>> {
    let remote_names = heddle_git_projection::git_export::git_remote_names(repo);
    let mut threads: Vec<PushableThread> = Vec::new();
    for thread in repo.refs().list_threads()? {
        let name = thread.to_string();
        if heddle_git_projection::git_export::is_remote_tracking_thread_name(&name, &remote_names) {
            continue;
        }
        if let Some(state) = repo.refs().get_thread(&thread)? {
            threads.push(PushableThread { name, state });
        }
    }
    threads.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(threads)
}

/// `--all-threads` fan-out for a native Heddle local target (heddle#838):
/// push every pushable thread's tip under its own ref. Not atomic — a
/// mid-loop failure leaves earlier threads pushed; every thread is attempted
/// and any failure makes the whole command exit non-zero, with per-thread
/// results reported.
async fn push_local_all_threads(
    repo: &Repository,
    target_path: &std::path::Path,
    plan: &PushPlan,
    cli: &Cli,
) -> Result<()> {
    let json = should_output_json(cli, Some(repo.config()));
    let target_label = format!("file://{}", target_path.display());
    if !json {
        let begin = multi_ref_push_begin(target_label);
        // Domain line is unstyled; keep working marker + dim target styling.
        let line = format_multi_ref_push_progress(&begin);
        if let Some(target) = line.strip_prefix("pushing all threads to ") {
            println!(
                "{} pushing all threads to {}",
                style::working_marker(),
                style::dim(target)
            );
        } else {
            println!("{} {line}", style::working_marker());
        }
    }

    let target_repo = Repository::open(target_path)?;
    let sync = LocalSync::open(repo.root())?;
    let threads = pushable_threads_for_all(repo)?;

    let mut pushed: Vec<String> = Vec::new();
    let mut failures: Vec<(String, String)> = Vec::new();
    let mut total_objects: usize = 0;

    for thread in &threads {
        let push_one = || -> Result<usize> {
            let copied = sync.fetch_state(&target_repo, &thread.state)?;
            target_repo
                .refs()
                .set_thread(&ThreadName::new(&thread.name), &thread.state)?;
            Ok(copied)
        };
        match push_one() {
            Ok(copied) => {
                total_objects += copied;
                pushed.push(thread.name.clone());
                if !json {
                    let event = multi_ref_thread_succeeded_local(
                        thread.name.clone(),
                        Some(thread.state.short().to_string()),
                        Some(copied),
                    );
                    let domain_line = format_multi_ref_push_progress(&event);
                    println!(
                        "{} pushed {} to {} ({})",
                        style::ok_marker(),
                        style::state_id(&thread.state.short().to_string()),
                        style::bold(&thread.name),
                        style::count(copied, "object")
                    );
                    debug_assert_eq!(
                        domain_line,
                        format!(
                            "pushed {} to {} ({})",
                            thread.state.short(),
                            thread.name,
                            if copied == 1 {
                                "1 object".to_string()
                            } else {
                                format!("{copied} objects")
                            }
                        ),
                        "styled local multi-thread success must match domain progress line"
                    );
                }
            }
            Err(err) => {
                let error = transport_error_message(Some(&err.to_string()));
                failures.push((thread.name.clone(), error.clone()));
                if !json {
                    let event = multi_ref_thread_failed(thread.name.clone(), Some(&error));
                    eprintln!(
                        "{} {}",
                        style::warn_marker(),
                        format_multi_ref_push_progress(&event)
                    );
                }
            }
        }
    }

    if json {
        let trust = build_repository_verification_state(repo);
        let output = heddle_all_threads_push_output(plan, pushed, &failures, total_objects, trust);
        crate::cli::render::write_json_stdout(&output)?;
    }

    if let Some(failure) = first_multi_thread_push_failure(&failures) {
        return Err(map_push_failure(failure));
    }
    Ok(())
}

#[cfg(feature = "client")]
async fn push_network(repo: &Repository, options: PushNetworkOptions<'_>) -> Result<()> {
    let mut client = options
        .session
        .connect(options.addr)
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

    let repo_path = match options.repo_path {
        Some(repo_path) => repo_path.to_string(),
        None => auto_provision_hosted_repo(repo, &mut client, &options).await?,
    };

    // --all-threads (heddle#838) on the NATIVE hosted path fans out one push
    // per pushable thread. The Git-backed projection path (the #846 default)
    // already ships EVERY ref in one transfer, so a projection push IS an
    // all-threads push — routing it through the per-thread loop would rebuild
    // and re-upload the identical full pack once per thread and print a
    // misleading "pushed to <thread>" line each time. Short-circuit it to a
    // single projection push below; only the non-Git-backed path loops.
    // Plan is pure (capability + flag); network bodies stay here.
    if matches!(options.plan.hosted, HostedPushPlan::NativePerThreadFanout) {
        return push_network_all_threads(repo, &mut client, &repo_path, &options).await;
    }

    let progress = progress_for(options.cli, repo);
    let state_id = match options.state_id {
        Some(state_id) => *state_id,
        None if matches!(options.plan.hosted, HostedPushPlan::GitOverlayMirror) => {
            ensure_current_state(
                repo,
                &UserConfig::load_default()?,
                Some("Bootstrap git-overlay before push".to_string()),
            )?
        }
        None => anyhow::bail!("single-thread native push requires a resolved state"),
    };
    let result = push_network_one_thread(
        repo,
        &mut client,
        &repo_path,
        &state_id,
        options.track_name,
        options.force,
        &progress,
        options.cli.operation_id_wire(),
    )
    .await?;
    clear_line(&progress);

    // CLI maps wire/protobuf transport fields → pure domain fields; core
    // parses success/failure and builds execution facts / outcome.
    let fields = HostedPushResultFields {
        success: result.success,
        new_state: result.new_state.map(|s| s.to_string()),
        error: result.error,
    };
    match parse_hosted_push_result(options.track_name, &fields) {
        HostedPushResult::Success { state } => {
            if should_output_json(options.cli, Some(repo.config())) {
                let trust = build_repository_verification_state(repo);
                let output = heddle_push_output(options.plan, state, None, trust);
                crate::cli::render::write_json_stdout(&output)?;
            } else {
                let trust = build_repository_verification_state(repo);
                let output = heddle_push_output(options.plan, state.clone(), None, trust);
                let text = format_push_outcome_text(&output.outcome, Some(options.track_name));
                println!(
                    "{} pushed to {}",
                    style::ok_marker(),
                    style::bold(options.track_name)
                );
                debug_assert!(
                    text.headline.contains(options.track_name) || text.headline.contains("pushed"),
                    "domain headline: {}",
                    text.headline
                );
                for line in &text.detail_lines {
                    println!("{line}");
                }
                if let Some(new_state) = state {
                    let detail = format_remote_state_detail(&new_state);
                    if let Some(state_val) = detail.strip_prefix("remote state: ") {
                        println!(
                            "{}",
                            style::field("remote state", &style::state_id(state_val))
                        );
                    } else {
                        println!("{detail}");
                    }
                }
            }
        }
        HostedPushResult::Failed(failure) => {
            return Err(map_push_failure(failure));
        }
    }

    Ok(())
}

/// Push one Heddle state or one authoritative Git mirror over hosted transport.
#[cfg(feature = "client")]
async fn push_network_one_thread(
    repo: &Repository,
    client: &mut HostedGrpcClient,
    repo_path: &str,
    state_id: &objects::object::StateId,
    track_name: &str,
    force: bool,
    progress: &objects::Progress,
    client_operation_id: String,
) -> Result<wire::PushComplete> {
    if repo.capability() == RepositoryCapability::GitOverlay {
        Ok(client
            .push_git_overlay_mirror(
                repo,
                repo_path,
                *state_id,
                track_name,
                force,
                progress,
                client_operation_id,
            )
            .await?)
    } else {
        Ok(client
            .push(
                repo,
                repo_path,
                *state_id,
                track_name,
                force,
                client_operation_id,
            )
            .await?)
    }
}

/// `--all-threads` fan-out over the NATIVE hosted transport (heddle#838): the
/// native push RPC is single-thread, so loop once per pushable thread (each at
/// its own tip — composes with the heddle#837 fix). Not atomic; every thread is
/// attempted, per-thread results reported, and any failure exits non-zero.
///
#[cfg(feature = "client")]
async fn push_network_all_threads(
    repo: &Repository,
    client: &mut HostedGrpcClient,
    repo_path: &str,
    options: &PushNetworkOptions<'_>,
) -> Result<()> {
    let json = should_output_json(options.cli, Some(repo.config()));
    let threads = pushable_threads_for_all(repo)?;

    let mut pushed: Vec<String> = Vec::new();
    let mut failures: Vec<(String, String)> = Vec::new();
    let progress = objects::Progress::null();

    for thread in &threads {
        let root_operation_id = options.cli.operation_id_wire();
        let thread_operation_id = if root_operation_id.is_empty() {
            String::new()
        } else {
            // Each thread needs a DISTINCT client_operation_id so weft/daemon dedup
            // per-thread instead of collapsing them — but the id must parse as a
            // strict UUID (a composite "{uuid}:push:{thread}" string is rejected as
            // InvalidArgument). Derive a deterministic, retry-stable per-thread
            // UUIDv5 from the root op-id (namespace) and the thread name.
            let namespace = uuid::Uuid::parse_str(&root_operation_id)
                .unwrap_or(uuid::Uuid::NAMESPACE_OID);
            uuid::Uuid::new_v5(&namespace, thread.name.as_bytes()).to_string()
        };
        let outcome = push_network_one_thread(
            repo,
            client,
            repo_path,
            &thread.state,
            &thread.name,
            options.force,
            &progress,
            thread_operation_id,
        )
        .await;
        match outcome {
            Ok(result) => {
                let fields = HostedPushResultFields {
                    success: result.success,
                    new_state: result.new_state.map(|s| s.to_string()),
                    error: result.error,
                };
                match parse_hosted_push_result(&thread.name, &fields) {
                    HostedPushResult::Success { state } => {
                        pushed.push(thread.name.clone());
                        if !json {
                            let event =
                                multi_ref_progress_from_hosted_thread(&thread.name, &fields);
                            let line = format_multi_ref_push_progress(&event);
                            // Prefer historical two-line shape when a remote state is present;
                            // domain still owns the single-line progress contract for asserts.
                            println!(
                                "{} pushed to {}",
                                style::ok_marker(),
                                style::bold(&thread.name)
                            );
                            if let Some(new_state) = state {
                                let detail = format_remote_state_detail(&new_state);
                                if let Some(state_val) = detail.strip_prefix("remote state: ") {
                                    println!(
                                        "{}",
                                        style::field("remote state", &style::state_id(state_val))
                                    );
                                } else {
                                    println!("{detail}");
                                }
                            }
                            debug_assert!(
                                line.contains(&thread.name),
                                "progress line should name thread: {line}"
                            );
                        }
                    }
                    HostedPushResult::Failed(failure) => {
                        let err = match failure {
                            PushFailure::RemoteFailed { error, .. } => error,
                            other => other.to_string(),
                        };
                        failures.push((thread.name.clone(), err));
                        if !json {
                            let event =
                                multi_ref_progress_from_hosted_thread(&thread.name, &fields);
                            eprintln!(
                                "{} {}",
                                style::warn_marker(),
                                format_multi_ref_push_progress(&event)
                            );
                        }
                    }
                }
            }
            Err(err) => {
                let error = transport_error_message(Some(&err.to_string()));
                failures.push((thread.name.clone(), error.clone()));
                if !json {
                    let event = multi_ref_thread_failed(thread.name.clone(), Some(&error));
                    eprintln!(
                        "{} {}",
                        style::warn_marker(),
                        format_multi_ref_push_progress(&event)
                    );
                }
            }
        }
    }

    if json {
        let trust = build_repository_verification_state(repo);
        let output = heddle_all_threads_push_output(options.plan, pushed, &failures, 0, trust);
        crate::cli::render::write_json_stdout(&output)?;
    }

    if let Some(failure) = first_multi_thread_push_failure(&failures) {
        return Err(map_push_failure(failure));
    }
    Ok(())
}

#[cfg(feature = "client")]
async fn auto_provision_hosted_repo(
    repo: &Repository,
    client: &mut HostedGrpcClient,
    options: &PushNetworkOptions<'_>,
) -> Result<String> {
    let namespace = client.get_current_user_namespace().await?;
    let slug = default_spool_slug_from_repo_root(repo.root())?;
    let derived_full_path = format!("{}/{}", namespace.full_path, slug);
    let provisioned_repo = match client.create_repository(&namespace.full_path, &slug).await {
        Ok(created) => AutoProvisionedHostedRepo::Created(created.full_path),
        Err(err) if auto_provision_create_already_exists(&err) => {
            AutoProvisionedHostedRepo::Existing(derived_full_path)
        }
        Err(err) => {
            return Err(map_push_failure(remote_push_failure(
                options.track_name,
                Some(&auto_provision_create_error_message(&slug, &err)),
            )));
        }
    };

    let configured_remote = persist_auto_provisioned_remote(
        repo,
        options.remote_arg,
        options.addr,
        provisioned_repo.full_path(),
    )?;

    if !should_output_json(options.cli, Some(repo.config())) {
        let display_full_path =
            hosted_spool_display_path(&namespace.slug, &slug, provisioned_repo.full_path());
        println!(
            "{} {} hosted spool {}",
            style::ok_marker(),
            provisioned_repo.status_verb(),
            style::bold(&display_full_path)
        );
        if let Some(remote_name) = configured_remote {
            println!(
                "{}",
                style::field(
                    "remote",
                    &format!(
                        "{} -> {}",
                        style::bold(&remote_name),
                        style::dim(&format!("heddle://{}/{}", options.addr, display_full_path))
                    )
                )
            );
        } else {
            print_next(&format!(
                "heddle remote add origin heddle://{}/{}",
                options.addr, display_full_path
            ));
        }
    }

    Ok(provisioned_repo.into_full_path())
}

#[cfg(feature = "client")]
enum AutoProvisionedHostedRepo {
    Created(String),
    Existing(String),
}

#[cfg(feature = "client")]
impl AutoProvisionedHostedRepo {
    fn full_path(&self) -> &str {
        match self {
            Self::Created(full_path) | Self::Existing(full_path) => full_path,
        }
    }

    fn into_full_path(self) -> String {
        match self {
            Self::Created(full_path) | Self::Existing(full_path) => full_path,
        }
    }

    fn status_verb(&self) -> &'static str {
        match self {
            Self::Created(_) => "created",
            Self::Existing(_) => "using",
        }
    }
}

#[cfg(feature = "client")]
fn auto_provision_create_already_exists(err: &ProtocolError) -> bool {
    match err {
        ProtocolError::AlreadyExists(_) => true,
        ProtocolError::Remote(message) => message_indicates_already_exists(message),
        _ => false,
    }
}

#[cfg(feature = "client")]
fn auto_provision_create_error_message(slug: &str, err: &ProtocolError) -> String {
    let error = match err {
        ProtocolError::Remote(_) | ProtocolError::LockError(_) => err.client_message(),
        _ => redact_internal_hosted_paths(&err.to_string()),
    };
    format!(
        "could not create hosted spool '{slug}': {error}. Pass a full hosted remote path or choose another local folder name"
    )
}

#[cfg(feature = "client")]
fn persist_auto_provisioned_remote(
    repo: &Repository,
    remote_arg: Option<&str>,
    addr: SocketAddr,
    full_path: &str,
) -> Result<Option<String>> {
    let Some(remote_name) = auto_provision_remote_name(repo, remote_arg)? else {
        return Ok(None);
    };
    let mut cfg = RemoteConfig::open(repo).map_err(anyhow::Error::new)?;
    cfg.add(
        &remote_name,
        Remote {
            url: format!("heddle://{addr}/{full_path}"),
            insecure: false,
        },
    )
    .map_err(anyhow::Error::new)?;
    Ok(Some(remote_name))
}

#[cfg(feature = "client")]
fn auto_provision_remote_name(
    repo: &Repository,
    remote_arg: Option<&str>,
) -> Result<Option<String>> {
    let cfg = RemoteConfig::open(repo).map_err(anyhow::Error::new)?;
    match remote_arg {
        Some(arg) if cfg.get(arg).is_ok() => Ok(Some(arg.to_string())),
        Some(arg) => {
            let is_direct_network_without_path = matches!(
                RemoteTarget::parse(arg),
                Ok(RemoteTarget::Network {
                    repo_path: None,
                    ..
                })
            );
            if is_direct_network_without_path && cfg.default_name().is_none() {
                Ok(Some("origin".to_string()))
            } else {
                Ok(None)
            }
        }
        None => Ok(Some(
            cfg.default_name()
                .map(str::to_string)
                .unwrap_or_else(|| "origin".to_string()),
        )),
    }
}

#[cfg(feature = "client")]
fn default_spool_slug_from_repo_root(root: &Path) -> Result<String> {
    let name = root
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    let slug = spool_slug_from_local_name(name);
    if slug.is_empty() {
        return Err(anyhow!(
            "could not derive a hosted spool name from {}; rename the directory or pass a full hosted remote path",
            root.display()
        ));
    }
    Ok(slug)
}

#[cfg(any(feature = "client", test))]
fn spool_slug_from_local_name(name: &str) -> String {
    let mut slug = String::new();
    let mut last_was_separator = false;
    for ch in name.chars().flat_map(char::to_lowercase) {
        if ch.is_ascii_lowercase() || ch.is_ascii_digit() {
            slug.push(ch);
            last_was_separator = false;
        } else if !slug.is_empty() && !last_was_separator {
            slug.push('-');
            last_was_separator = true;
        }
    }
    while slug.ends_with('-') {
        slug.pop();
    }
    slug
}

#[cfg(feature = "client")]
struct PushNetworkOptions<'a> {
    addr: SocketAddr,
    repo_path: Option<&'a str>,
    remote_arg: Option<&'a str>,
    session: &'a HostedSession,
    /// The single resolved state for a one-thread push. Fan-out plans resolve
    /// each thread's tip themselves.
    state_id: Option<&'a objects::object::StateId>,
    track_name: &'a str,
    force: bool,
    /// Pure orchestration plan (outcome assembly + routing policy).
    plan: &'a PushPlan,
    cli: &'a Cli,
}

#[cfg(test)]
mod tests {
    #[cfg(feature = "client")]
    use tempfile::TempDir;

    use super::*;

    #[test]
    fn spool_slug_from_local_name_normalizes_folder_names() {
        assert_eq!(spool_slug_from_local_name("My Cool Repo"), "my-cool-repo");
        assert_eq!(spool_slug_from_local_name("Heddle_CLI.v2"), "heddle-cli-v2");
        assert_eq!(spool_slug_from_local_name("---"), "");
    }

    #[test]
    fn overlay_heddle_remote_resolves_to_hosted_mirror_transport() {
        let temp = tempfile::TempDir::new().unwrap();
        let git = SleyRepository::init(temp.path()).unwrap();
        let repo = Repository::init_git_overlay_sidecar(temp.path()).unwrap();
        let plan = ConfigEditPlan::new(git.common_dir().join("config"))
            .with_operation(
                ConfigEdit::set("remote.origin.url", "heddle://127.0.0.1:8421/acme/widget")
                    .unwrap(),
            )
            .with_operation(ConfigEdit::set("branch.main.remote", "origin").unwrap())
            .with_operation(ConfigEdit::set("branch.main.merge", "refs/heads/main").unwrap());
        git.apply_config_edit_plan(plan).unwrap();

        assert!(push_target_is_hosted_network(&repo, None));
        let (target, key) = resolve_push_target_with_key(&repo, None).unwrap();
        assert!(matches!(
            target,
            RemoteTarget::Network {
                repo_path: Some(ref path),
                ..
            } if path == "acme/widget"
        ));
        assert_eq!(key.as_deref(), Some("127.0.0.1:8421"));
    }

    // Capability / push-routing pure decisions live in heddle_core::remote.

    #[cfg(feature = "client")]
    #[test]
    fn auto_provision_remote_name_uses_existing_or_origin_remote() {
        let temp = TempDir::new().unwrap();
        let repo = Repository::init_default(temp.path()).unwrap();

        assert_eq!(
            auto_provision_remote_name(&repo, None).unwrap().as_deref(),
            Some("origin")
        );
        assert_eq!(
            auto_provision_remote_name(&repo, Some("127.0.0.1:8421"))
                .unwrap()
                .as_deref(),
            Some("origin")
        );

        let mut cfg = RemoteConfig::open(&repo).unwrap();
        cfg.add(
            "weft",
            Remote {
                url: "heddle://127.0.0.1:8421".to_string(),
                insecure: false,
            },
        )
        .unwrap();

        assert_eq!(
            auto_provision_remote_name(&repo, Some("weft"))
                .unwrap()
                .as_deref(),
            Some("weft")
        );
        assert_eq!(
            auto_provision_remote_name(&repo, Some("127.0.0.1:8421"))
                .unwrap()
                .as_deref(),
            None
        );
    }

    #[cfg(feature = "client")]
    #[test]
    fn auto_provision_reuses_create_repository_already_exists() {
        let typed_already_exists = ProtocolError::AlreadyExists("luke/demo-repo".to_string());
        assert!(auto_provision_create_already_exists(&typed_already_exists));

        let already_exists = ProtocolError::Remote("repository already exists".to_string());
        assert!(auto_provision_create_already_exists(&already_exists));

        let validation = ProtocolError::InvalidState("repository already exists".to_string());
        assert!(!auto_provision_create_already_exists(&validation));

        let permission = ProtocolError::AuthorizationFailed("missing grant".to_string());
        assert!(!auto_provision_create_already_exists(&permission));
    }

    #[cfg(feature = "client")]
    #[test]
    fn auto_provision_hides_internal_user_namespace_paths_in_cli_text() {
        let namespace = wire::HostedNamespaceInfo {
            namespace_id: "user-1".to_string(),
            kind: "user".to_string(),
            slug: "alice".to_string(),
            parent_id: None,
            display_name: None,
            full_path: "__users/user-1".to_string(),
        };

        assert_eq!(
            hosted_spool_display_path(&namespace.slug, "demo-repo", "__users/user-1/demo-repo"),
            "alice/demo-repo"
        );

        let validation =
            ProtocolError::InvalidState("namespace __users/user-1 rejected".to_string());
        let validation_message = auto_provision_create_error_message("demo-repo", &validation);
        assert!(
            !validation_message.contains("__users/"),
            "{validation_message}"
        );
        assert!(validation_message.contains("demo-repo"));

        let remote =
            ProtocolError::Remote("repository __users/user-1/demo-repo failed".to_string());
        let remote_message = auto_provision_create_error_message("demo-repo", &remote);
        assert!(!remote_message.contains("__users/"), "{remote_message}");
        assert!(remote_message.contains("internal server error"));
    }
}
