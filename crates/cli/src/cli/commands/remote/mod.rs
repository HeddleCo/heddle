// SPDX-License-Identifier: Apache-2.0
//! Remote operations (push, pull, remote management).

#[cfg(feature = "client")]
use std::net::SocketAddr;
use std::path::Path;

use anyhow::{Context, Result, anyhow};
use heddle_core::{
    GitOverlayPushTracking, GitRemoteConfigured, LocalTransferSummary, PushFailure, PushOutcome,
    PushPath, PushPlan, PushPlanRequest, RemotePreflightBlocker, build_push_outcome,
    first_multi_thread_push_failure, format_mirror_failure_text, format_mirror_success_text,
    format_multi_ref_push_progress, format_push_outcome_text, format_pushing_to,
    git_overlay_push_execution_facts, heddle_single_push_execution_facts_from_local,
    multi_ref_push_begin, multi_ref_thread_failed, multi_ref_thread_succeeded_local,
    multi_thread_push_execution_facts, named_thread_tip_mismatch_failure, plan_push,
    refuse_named_thread_tip_overwrite, transport_error_message, uses_local_git_overlay_transport,
};
#[cfg(feature = "client")]
use heddle_core::{
    HostedPushPlan, HostedPushResult, HostedPushResultFields, all_threads_mirror_coverage_note,
    format_connected_to, format_remote_state_detail, heddle_single_push_execution_facts,
    multi_ref_progress_from_hosted_thread, parse_hosted_push_result, plan_hosted_push,
    remote_push_failure, uses_git_overlay_mirror_rpc,
};
use objects::object::ThreadName;
use refs::Head;
use repo::{Repository, RepositoryCapability};
use serde::Serialize;
use sley::{
    ConfigEdit, ConfigEditPlan, ConfigSectionEntry, FullName, RefPrecondition, RemoteConfigSet,
    Repository as SleyRepository,
};
#[cfg(feature = "client")]
use wire::ProtocolError;

use super::{
    action_line::print_next,
    advice::RecoveryAdvice,
    auto_capture::{AutoCaptureTrigger, auto_capture_command_boundary},
    command_catalog::{ActionFields, ActionTemplate},
    snapshot::ensure_current_state,
    verification_health::{RepositoryVerificationState, build_repository_verification_state},
};
#[cfg(feature = "client")]
use crate::cli::progress_render::{clear_line, progress_for};
#[cfg(feature = "client")]
use crate::client::HostedGrpcClient;
#[cfg(feature = "client")]
use crate::client::{HostedAuthMode, HostedSession};
#[cfg(feature = "client")]
use crate::remote::Remote;
use crate::{
    cli::{Cli, should_output_json, style},
    client::LocalSync,
    config::UserConfig,
    git_projection_engine::{
        GitProjection,
        git_core::{GitPushScope, set_reference},
    },
    remote::{RemoteConfig, RemoteTarget, resolve_remote_with_key},
};

mod remote_ops;

pub use remote_ops::{cmd_pull, cmd_remote};
pub(crate) use remote_ops::{resolve_default_remote_name, resolved_default_remote_name};

#[allow(clippy::type_complexity)]
pub(crate) fn push_git_overlay_refs(
    repo: &Repository,
    remote: Option<&str>,
    all_threads: bool,
    force: bool,
) -> Result<(
    String,
    GitPushScope,
    Option<String>,
    Option<GitOverlayTrackingRefresh>,
    Vec<String>,
    super::verification_health::RepositoryVerificationState,
)> {
    let remote_name = resolve_default_remote_name(repo, remote)?;
    let scope = if all_threads {
        GitPushScope::AllThreads
    } else {
        GitPushScope::CurrentThread
    };
    let current_thread = if matches!(scope, GitPushScope::CurrentThread) {
        match repo.head_ref()? {
            Head::Attached { thread } => Some(thread.to_string()),
            Head::Detached { .. } => None,
        }
    } else {
        None
    };
    let mut bridge = GitProjection::new(repo);
    let refs_written = bridge.push_with_scope_force(&remote_name, scope, force)?;
    let tracking_refresh = refresh_git_tracking_after_overlay_push(repo, &remote_name)?;
    let trust = build_repository_verification_state(repo);
    Ok((
        remote_name,
        scope,
        current_thread,
        tracking_refresh,
        refs_written,
        trust,
    ))
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

/// CLI machine envelope: domain [`PushOutcome`] plus verification next-actions.
#[derive(Debug, Clone, Serialize)]
struct PushOutput {
    #[serde(flatten)]
    outcome: PushOutcome,
    next_action: Option<String>,
    next_action_template: Option<ActionTemplate>,
    recommended_action: Option<String>,
    recommended_action_template: Option<ActionTemplate>,
    #[allow(dead_code)]
    #[serde(skip_serializing)]
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

/// Execute push command.
///
/// Pure orchestration (`plan_push`) runs first; network / git I/O bodies stay
/// in this module and consume plan fields.
///
/// `mirror` is an ad-hoc dual-push escape hatch (heddle#25): after the
/// primary push to the Heddle/git-overlay remote succeeds, also push to
/// the named Git remote. Best-effort — mirror failure surfaces
/// as a warning and does NOT abort the primary push.
#[allow(clippy::too_many_arguments)]
pub async fn cmd_push(
    cli: &Cli,
    remote: Option<String>,
    thread: Option<String>,
    state: Option<String>,
    force: bool,
    all_threads: bool,
    mirror: Option<String>,
    insecure: bool,
) -> Result<()> {
    let repo = cli.open_repo()?;
    if let Some(remote_name) = remote.as_deref() {
        ensure_remote_arg_resolves(&repo, remote_name)?;
    }

    let has_default_remote = resolved_default_remote_name(&repo)?.is_some();
    let push_uses_hosted_network = push_target_is_hosted_network(&repo, remote.as_deref());
    let uses_local_overlay = uses_local_git_overlay_transport(
        repo.capability(),
        repo.hosted_enabled(),
        push_uses_hosted_network,
    );
    // Discover native-heddle local target under local overlay (I/O fact for plan).
    let native_local_path = if uses_local_overlay {
        let default_remote_name = if remote.is_none() {
            resolved_default_remote_name(&repo)?
        } else {
            None
        };
        let remote_arg = remote.as_deref().or(default_remote_name.as_deref());
        native_heddle_local_push_target(&repo, remote_arg)?
    } else {
        None
    };
    // Match preflight_native_remote_transport: overlay capability never
    // treats a git URL as a native-transport mismatch.
    let transport_mismatch = repo.capability() != RepositoryCapability::GitOverlay
        && matches!(
            classify_remote_spec(&repo, remote.as_deref()),
            Some(RemoteTransportKind::LocalGit | RemoteTransportKind::GitUrl)
        );
    let head = repo.head_ref()?;
    let plan = plan_push(&PushPlanRequest {
        capability: repo.capability(),
        hosted_enabled: repo.hosted_enabled(),
        uses_hosted_network: push_uses_hosted_network,
        remote: remote.clone(),
        has_default_remote,
        thread: thread.clone(),
        all_threads,
        force,
        head,
        native_local_heddle_target: native_local_path.is_some(),
        transport_mismatch,
    })
    .map_err(|blocker| map_remote_preflight_blocker(blocker, "push", remote.as_deref()))?;

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
        PushPath::LocalNativeHeddle {
            all_threads: path_all_threads,
        } => {
            let target_path = native_local_path.expect("plan selected local native heddle path");
            if *path_all_threads {
                push_local_all_threads(&repo, &target_path, &plan, cli).await?;
            } else {
                let state_id = resolve_push_state_id(
                    &repo,
                    &user_config,
                    state,
                    thread.as_deref(),
                    plan.force,
                )?;
                push_local(&repo, &target_path, &state_id, &plan.track_name, &plan, cli).await?;
            }
            // Ad-hoc dual-push parity (heddle#25): mirror runs on the
            // local-target overlay path too, best-effort.
            if let Some(mirror_remote) = mirror.as_deref() {
                let mut bridge = GitProjection::new(&repo);
                let outcome = bridge.push(mirror_remote);
                render_mirror_outcome(cli, &repo, mirror_remote, outcome);
            }
            run_post_push_hook(&hook_manager, &hook_ctx, remote.as_deref());
            return Ok(());
        }
        PushPath::LocalGitOverlayRefs {
            all_threads: path_all_threads,
        } => {
            // Thread mismatch already refused by plan_push.
            let (remote_name, _scope, current_thread, tracking_refresh, refs_written, trust) =
                push_git_overlay_refs(&repo, remote.as_deref(), *path_all_threads, plan.force)?;
            let output = git_overlay_push_output(
                &plan,
                remote_name,
                current_thread,
                tracking_refresh,
                refs_written,
                trust,
            );
            if should_output_json(cli, Some(repo.config())) {
                crate::cli::render::write_json_stdout(&output)?;
            } else {
                render_push_outcome_text(&output.outcome, None, &output.trust);
            }
            // Ad-hoc dual-push parity for the git-overlay branch (heddle#25):
            // `--mirror` fires here too, best-effort, after the primary push.
            if let Some(mirror_remote) = mirror.as_deref() {
                let mut bridge = GitProjection::new(&repo);
                let outcome = bridge.push(mirror_remote);
                render_mirror_outcome(cli, &repo, mirror_remote, outcome);
            }
            run_post_push_hook(&hook_manager, &hook_ctx, remote.as_deref());
            return Ok(());
        }
        PushPath::NativeRemote { .. } => {
            // Transport mismatch already refused by plan_push.
        }
    }

    #[cfg(not(feature = "client"))]
    let token = user_config.remote_token()?;
    #[cfg(feature = "client")]
    let (target, server_key) = resolve_remote_with_key(&repo, remote.as_deref())?;
    #[cfg(not(feature = "client"))]
    let (target, _server_key) = resolve_remote_with_key(&repo, remote.as_deref())?;

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
                    all_threads: plan.all_threads,
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

    // Ad-hoc dual-push (heddle#25): after the primary push, also push to
    // the named Git remote mirror. Best-effort — mirror failure does not
    // abort the primary push.
    if let Some(mirror_remote) = mirror.as_deref() {
        let mut bridge = GitProjection::new(&repo);
        let outcome = bridge.push(mirror_remote);
        render_mirror_outcome(cli, &repo, mirror_remote, outcome);
    }

    run_post_push_hook(&hook_manager, &hook_ctx, remote.as_deref());

    Ok(())
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

/// Print unstyled domain push text with CLI markers / emphasis.
fn render_push_outcome_text(
    outcome: &PushOutcome,
    track_name: Option<&str>,
    trust: &RepositoryVerificationState,
) {
    let text = format_push_outcome_text(outcome, track_name);
    // Restyle headline pieces lightly: keep domain wording, add ok marker.
    println!("{} {}", style::ok_marker(), text.headline);
    for line in &text.detail_lines {
        if let Some(rest) = line.strip_prefix("Force: ") {
            println!("Force: {rest}");
        } else if let Some(rest) = line.strip_prefix("Git interop: published ") {
            // Domain line is "Git interop: published refs/notes/heddle; …"
            if let Some((notes, tail)) = rest.split_once(';') {
                println!(
                    "Git interop: published {};{}",
                    style::bold(notes.trim()),
                    tail
                );
            } else {
                println!("{line}");
            }
        } else if let Some(rest) = line.strip_prefix("Git tracking: configured remote ") {
            // "name -> url for future…"
            if let Some((name, after)) = rest.split_once(" -> ") {
                if let Some((url, tail)) = after.split_once(" for future") {
                    println!(
                        "Git tracking: configured remote {} -> {} for future{tail}",
                        style::bold(name),
                        style::dim(url)
                    );
                } else {
                    println!("{line}");
                }
            } else {
                println!("{line}");
            }
        } else if let Some(rest) = line.strip_prefix("Git tracking: branch ") {
            // "branch tracks remote/branch."
            if let Some((branch, after)) = rest.split_once(" tracks ") {
                if let Some((remote, branch2)) = after.trim_end_matches('.').split_once('/') {
                    println!(
                        "Git tracking: branch {} tracks {}/{branch2}.",
                        style::bold(branch),
                        style::bold(remote),
                    );
                } else {
                    println!("{line}");
                }
            } else {
                println!("{line}");
            }
        } else {
            println!("{line}");
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
        print_next(&trust.recommended_action);
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

/// Print the outcome of the ad-hoc mirror push (heddle#25). Mirror
/// failure is best-effort: surface as a warning, never bubble up.
/// Matches the JSON and text shapes the main branch shipped.
fn render_mirror_outcome(
    cli: &Cli,
    repo: &Repository,
    mirror_remote: &str,
    outcome: crate::git_projection_engine::GitProjectionResult<Vec<String>>,
) {
    let json = should_output_json(cli, Some(repo.config()));
    match outcome {
        Ok(_) => {
            if json {
                // Stderr (not stdout): the primary push already wrote
                // the documented single JSON object to stdout. Emitting
                // a second JSON object there would break the
                // `heddle push --output json` parse-as-one-object
                // contract for any caller using `--mirror`.
                let record = serde_json::json!({
                    "mirrored": true,
                    "remote": mirror_remote,
                });
                eprintln!("{}", record);
            } else {
                // Domain owns unstyled wording; CLI adds marker + bold remote.
                let line = format_mirror_success_text(mirror_remote);
                if let Some(remote) = line.strip_prefix("mirrored to ") {
                    println!("{} mirrored to {}", style::ok_marker(), style::bold(remote));
                } else {
                    println!("{} {line}", style::ok_marker());
                }
            }
        }
        Err(err) => {
            if json {
                let record = serde_json::json!({
                    "mirrored": false,
                    "remote": mirror_remote,
                    "error": err.to_string(),
                });
                eprintln!("{}", record);
            } else {
                let line = format_mirror_failure_text(mirror_remote, &err.to_string());
                // Restyle the remote name while keeping domain wording.
                if let Some(rest) = line.strip_prefix("mirror push to ") {
                    if let Some((remote, tail)) = rest.split_once(" failed ") {
                        eprintln!(
                            "{} mirror push to {} failed {tail}",
                            style::warn_marker(),
                            style::bold(remote),
                        );
                    } else {
                        eprintln!("{} {line}", style::warn_marker());
                    }
                } else {
                    eprintln!("{} {line}", style::warn_marker());
                }
            }
        }
    }
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

pub(super) fn push_target_is_hosted_network(repo: &Repository, remote_arg: Option<&str>) -> bool {
    matches!(
        classify_remote_spec(repo, remote_arg),
        Some(RemoteTransportKind::NetworkHeddle)
    )
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
            RecoveryAdvice::remote_transport_mismatch(action, remote_arg.unwrap_or("<default>"))
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
    let git = match SleyRepository::discover(repo.root()) {
        Ok(git) => git,
        Err(_) => return Ok(None),
    };
    let Some(head) = git.head().ok().and_then(|head| head.oid) else {
        return Ok(None);
    };
    let Some(tracking_remote) = resolve_git_tracking_remote_name(repo, remote_name)? else {
        return Ok(None);
    };

    let upstream = git_branch_upstream_from_config(&git, &branch)
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
        RefPrecondition::Any,
        &format!("heddle: push to {remote_name}"),
    ) {
        return Err(anyhow!(
            RecoveryAdvice::git_overlay_tracking_refresh_failed(
                remote_name,
                &full_ref,
                Some(error.to_string()),
            )
        ));
    }

    write_git_overlay_branch_upstream(repo.root(), &branch, &tracking_remote.name)?;

    Ok(Some(GitOverlayTrackingRefresh {
        remote_name: tracking_remote.name,
        configured_remote: tracking_remote.configured_remote,
        upstream_branch: Some(branch),
    }))
}

#[derive(Debug, Clone)]
struct GitTrackingRemoteResolution {
    name: String,
    configured_remote: Option<GitOverlayConfiguredRemote>,
}

fn resolve_git_tracking_remote_name(
    repo: &Repository,
    requested: &str,
) -> Result<Option<GitTrackingRemoteResolution>> {
    if let Some(name) = git_remote_name_for_url(repo.root(), requested)? {
        return Ok(Some(GitTrackingRemoteResolution {
            name,
            configured_remote: None,
        }));
    }
    if !looks_like_remote_location(requested)
        && git_remote_ref_name_is_valid(repo.root(), requested)?
    {
        return Ok(Some(GitTrackingRemoteResolution {
            name: requested.to_string(),
            configured_remote: None,
        }));
    }

    let remotes = git_remote_names(repo.root())?;
    if remotes.is_empty() && !requested.trim().is_empty() {
        write_git_overlay_remote(repo.root(), "origin", requested)
            .context("failed to configure Git remote for tracking")?;
        return Ok(Some(GitTrackingRemoteResolution {
            name: "origin".to_string(),
            configured_remote: Some(GitOverlayConfiguredRemote {
                name: "origin".to_string(),
                url: requested.to_string(),
            }),
        }));
    }
    // Only fall back to a sole configured remote when the requested
    // argument is not itself a remote-location shape. If the user
    // pushed to an explicit URL/path that did not match any
    // configured remote (otherwise `git_remote_name_for_url` would
    // have caught it above), silently retargeting the unrelated
    // sole remote (e.g. `origin`) would corrupt its tracking refs.
    if remotes.len() == 1 && !looks_like_remote_location(requested) {
        return Ok(Some(GitTrackingRemoteResolution {
            name: remotes[0].clone(),
            configured_remote: None,
        }));
    }
    if looks_like_remote_location(requested) {
        // Explicit URL/path that does not match any configured
        // remote — skip the tracking refresh rather than guessing.
        return Ok(None);
    }
    Ok(Some(GitTrackingRemoteResolution {
        name: requested.to_string(),
        configured_remote: None,
    }))
}

fn git_remote_name_for_url(root: &Path, requested: &str) -> Result<Option<String>> {
    let git = match SleyRepository::discover(root) {
        Ok(git) => git,
        Err(_) => return Ok(None),
    };
    for name in git_remote_names(root)? {
        let Some(url) = git_remote_push_url(&git, &name)? else {
            continue;
        };
        if remote_urls_match(&url, requested) {
            return Ok(Some(name));
        }
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

fn git_remote_ref_name_is_valid(_root: &Path, name: &str) -> Result<bool> {
    if name.trim().is_empty() {
        return Ok(false);
    }
    let refname = format!("refs/remotes/{name}/HEAD");
    Ok(FullName::try_from(refname.as_str()).is_ok())
}

fn git_branch_upstream_from_config(git: &SleyRepository, branch: &str) -> Option<String> {
    let config = git.config_snapshot().ok()?;
    let remote = config.get("branch", Some(branch), "remote")?;
    let merge = config.get("branch", Some(branch), "merge")?;
    let branch_name = merge.strip_prefix("refs/heads/")?;
    Some(format!("refs/remotes/{remote}/{branch_name}"))
}

fn git_remote_push_url(git: &SleyRepository, remote: &str) -> Result<Option<String>> {
    let config = git.config_snapshot()?;
    Ok(config
        .get("remote", Some(remote), "pushurl")
        .or_else(|| config.get("remote", Some(remote), "url"))
        .map(str::to_string))
}

fn write_git_overlay_branch_upstream(root: &Path, branch: &str, remote: &str) -> Result<()> {
    let git = SleyRepository::discover(root).map_err(anyhow::Error::new)?;
    let plan = ConfigEditPlan::new(git.common_dir().join("config"))
        .with_operation(ConfigEdit::replace_section(
            "branch",
            Some(branch.to_string()),
            vec![
                ConfigSectionEntry::new("remote", remote),
                ConfigSectionEntry::new("merge", format!("refs/heads/{branch}")),
            ],
        ))
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
) -> Result<objects::object::ChangeId> {
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
    state_id: &objects::object::ChangeId,
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
            style::change_id(&state_id.short().to_string()),
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
    state: objects::object::ChangeId,
}

/// Enumerate the threads `--all-threads` should push on the native/hosted
/// path (heddle#838): every heddle-managed thread, with remote-tracking
/// names filtered out exactly as the Git exporter does
/// ([`git_export::is_remote_tracking_thread_name`]). Each thread's state is
/// resolved from its own tip (composes with the heddle#837 fix). Sorted by
/// name for deterministic output. Threads whose ref cannot be resolved to a
/// state are skipped (they carry no pushable state).
fn pushable_threads_for_all(repo: &Repository) -> Result<Vec<PushableThread>> {
    let remote_names = crate::git_projection_engine::git_export::git_remote_names(repo);
    let mut threads: Vec<PushableThread> = Vec::new();
    for thread in repo.refs().list_threads()? {
        let name = thread.to_string();
        if crate::git_projection_engine::git_export::is_remote_tracking_thread_name(
            &name,
            &remote_names,
        ) {
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
                        style::change_id(&thread.state.short().to_string()),
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
    if matches!(
        plan_hosted_push(repo.capability(), options.all_threads),
        HostedPushPlan::NativePerThreadFanout
    ) {
        return push_network_all_threads(repo, &mut client, &repo_path, &options).await;
    }

    // Git-overlay repos DEFAULT to the Git-backed projection path (#846): ship the
    // git format (one multi-root pack + all refs) straight through weft's git
    // lane with no native conversion. Native heddle conversion stays opt-in via
    // `heddle adopt`, after which the repo is no longer GitOverlay and takes the
    // plain native push below. `progress` drives the live push line on a TTY.
    //
    // In `--all-threads` mode `state_id` is `None` (the fan-out resolves each
    // thread's tip); the projection push nominates the current checkout state as
    // its advisory `local_state` — every ref ships regardless.
    let progress = progress_for(options.cli, repo);
    let state_id = match options.state_id {
        Some(state_id) => *state_id,
        None => {
            // --all-threads Git-backed projection+hosted: projection ships all refs, so the
            // nominated state is advisory. Use the current checkout state.
            let user_config = UserConfig::load_default()?;
            ensure_current_state(
                repo,
                &user_config,
                Some("Bootstrap git-overlay before push".to_string()),
            )?
        }
    };
    let result = push_network_one_thread(
        repo,
        &mut client,
        &repo_path,
        &state_id,
        options.track_name,
        options.force,
        &progress,
    )
    .await?;
    // Clear the live progress line so the result message starts clean on a TTY.
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
                if let Some(note) = all_threads_mirror_coverage_note(options.all_threads) {
                    // Single Git Projection push covers every ref/thread.
                    println!("{}", style::dim(note));
                }
                if let Some(new_state) = state {
                    let detail = format_remote_state_detail(&new_state);
                    if let Some(state_val) = detail.strip_prefix("remote state: ") {
                        println!(
                            "{}",
                            style::field("remote state", &style::change_id(state_val))
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

/// Push a single thread's state over the hosted transport, routing through the
/// git-overlay checkpoint RPC or the plain push RPC per repo capability.
#[cfg(feature = "client")]
async fn push_network_one_thread(
    repo: &Repository,
    client: &mut HostedGrpcClient,
    repo_path: &str,
    state_id: &objects::object::ChangeId,
    track_name: &str,
    force: bool,
    progress: &objects::Progress,
) -> Result<wire::PushComplete> {
    let result = if uses_git_overlay_mirror_rpc(repo.capability()) {
        // Default (heddle#846): push ALL git-overlay refs in one multi-ref
        // git-mirror transfer through weft's git lane, with live progress.
        // Native heddle conversion stays opt-in via `heddle adopt`.
        client
            .push_git_overlay_mirror(repo, repo_path, *state_id, track_name, force, progress)
            .await?
    } else {
        client
            .push(repo, repo_path, *state_id, track_name, force)
            .await?
    };
    Ok(result)
}

/// `--all-threads` fan-out over the NATIVE hosted transport (heddle#838): the
/// native push RPC is single-thread, so loop once per pushable thread (each at
/// its own tip — composes with the heddle#837 fix). Not atomic; every thread is
/// attempted, per-thread results reported, and any failure exits non-zero.
///
/// This path is git-overlay-free by construction: `push_network` short-circuits
/// git-overlay `--all-threads` to a single mirror push (which already ships
/// every ref) before ever reaching here. The native `push` RPC does not drive
/// live progress, so there is no transient progress line to clear between the
/// per-thread `println!`s (a `null` handle is passed to satisfy the shared
/// helper signature).
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

    // Native push does not render a live progress line; pass a null handle.
    let progress = objects::Progress::null();
    for thread in &threads {
        let outcome = push_network_one_thread(
            repo,
            client,
            repo_path,
            &thread.state,
            &thread.name,
            options.force,
            &progress,
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
                                        style::field("remote state", &style::change_id(state_val))
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
            hosted_spool_display_path(&namespace, &slug, provisioned_repo.full_path());
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
fn message_indicates_already_exists(message: &str) -> bool {
    message.to_ascii_lowercase().contains("already exists")
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
fn hosted_spool_display_path(
    namespace: &wire::HostedNamespaceInfo,
    slug: &str,
    full_path: &str,
) -> String {
    if hosted_path_contains_internal_user_namespace(full_path) && !namespace.slug.is_empty() {
        format!("{}/{}", namespace.slug, slug)
    } else {
        full_path.to_string()
    }
}

#[cfg(feature = "client")]
fn redact_internal_hosted_paths(message: &str) -> String {
    message
        .split_whitespace()
        .map(|part| {
            if hosted_path_contains_internal_user_namespace(part) {
                "[user namespace]"
            } else {
                part
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(feature = "client")]
fn hosted_path_contains_internal_user_namespace(value: &str) -> bool {
    value.contains("__users/")
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
    /// The single resolved state for a one-thread push (heddle#837). `None`
    /// when `all_threads` is set — the fan-out resolves each thread's tip
    /// itself (heddle#838).
    state_id: Option<&'a objects::object::ChangeId>,
    track_name: &'a str,
    force: bool,
    /// heddle#838: fan out over every pushable thread instead of the single
    /// `track_name`/`state_id`.
    all_threads: bool,
    /// Pure orchestration plan (outcome assembly + routing policy).
    plan: &'a PushPlan,
    cli: &'a Cli,
}

#[cfg(test)]
mod git_overlay_config_atomic_tests {
    //! Crash-mid-write semantics for the Git-overlay `.git/config` writers.
    //! Both helpers route through Sley's config editor, so section parsing,
    //! quoting, locking, and atomic replacement stay Git-parity tested.
    use std::fs;

    use tempfile::TempDir;

    use super::*;

    fn init_git_repo(root: &Path) {
        SleyRepository::init(root).unwrap();
    }

    #[test]
    fn spool_slug_from_local_name_normalizes_folder_names() {
        assert_eq!(spool_slug_from_local_name("My Cool Repo"), "my-cool-repo");
        assert_eq!(spool_slug_from_local_name("Heddle_CLI.v2"), "heddle-cli-v2");
        assert_eq!(spool_slug_from_local_name("---"), "");
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
            hosted_spool_display_path(&namespace, "demo-repo", "__users/user-1/demo-repo"),
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

    #[test]
    fn write_git_overlay_remote_recovers_from_partial_prior_write() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();
        init_git_repo(root);
        let config = root.join(".git").join("config");

        // Establish a clean baseline so we know what "previous full
        // content" looks like.
        write_git_overlay_remote(root, "origin", "https://example.com/a.git").unwrap();
        assert!(
            fs::read_to_string(&config)
                .unwrap()
                .contains("https://example.com/a.git")
        );

        // Simulate a crash mid-write by truncating the file. A
        // non-atomic writer using `fs::write` could leave the config
        // in exactly this shape if the process died between the
        // `open(O_TRUNC)` and the final `write_all`.
        fs::write(&config, "[remote \"origin\"]\n\turl = htt").unwrap();

        // Re-invoke the helper. The atomic contract: the resulting
        // file is the full, well-formed new content — never a partial.
        write_git_overlay_remote(root, "origin", "https://example.com/b.git").unwrap();
        let recovered = fs::read_to_string(&config).unwrap();
        assert!(
            recovered.contains("[remote \"origin\"]"),
            "section header missing: {recovered}"
        );
        assert!(
            recovered.contains("https://example.com/b.git"),
            "new url missing: {recovered}"
        );
        assert!(
            recovered.contains("fetch = +refs/heads/*:refs/remotes/origin/*"),
            "fetch line missing: {recovered}"
        );
        assert!(
            !recovered.contains("url = htt\n") && !recovered.trim_end().ends_with("url = htt"),
            "partial bytes from prior crash leaked into result: {recovered}"
        );
    }

    #[test]
    fn write_git_overlay_branch_upstream_recovers_from_partial_prior_write() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();
        init_git_repo(root);
        let config = root.join(".git").join("config");

        // Baseline.
        write_git_overlay_branch_upstream(root, "main", "origin").unwrap();
        assert!(
            fs::read_to_string(&config)
                .unwrap()
                .contains("[branch \"main\"]")
        );

        // Crash-mid-write simulation: leave the file truncated mid-key.
        fs::write(&config, "[branch \"main\"]\n\trem").unwrap();

        // The atomic helper produces a fully-formed section regardless
        // of the prior partial state.
        write_git_overlay_branch_upstream(root, "main", "upstream").unwrap();
        let recovered = fs::read_to_string(&config).unwrap();
        assert!(recovered.contains("[branch \"main\"]"), "{recovered}");
        assert!(recovered.contains("upstream"), "{recovered}");
        assert!(recovered.contains("merge = refs/heads/main"), "{recovered}");
        assert!(
            !recovered.trim_end().ends_with("rem"),
            "partial bytes from prior crash leaked into result: {recovered}"
        );
    }
}
