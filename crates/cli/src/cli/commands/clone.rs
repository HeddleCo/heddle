// SPDX-License-Identifier: Apache-2.0
//! Clone command - clone from remote.

#[cfg(feature = "client")]
use std::time::{SystemTime, UNIX_EPOCH};
#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::{ffi::CString, os::unix::ffi::OsStrExt};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::{
        Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

#[cfg(feature = "client")]
use anyhow::Context;
use anyhow::{Result, anyhow};
/// `output_kind` value carried by the final `heddle clone --output json`
/// payload. Referenced by the command catalog and the catalog/runtime
/// invariant test to keep the runtime emission and the advertised
/// discriminator from drifting apart.
// The wire payload lives in cli-contract so the schema registry registers
// the real serialization type.
pub use heddle_cli_contract::cli::commands::wire::remote::CloneOutput;
use heddle_git_projection::git_core::{
    clone_url_to_bare, copy_local_repo_to_bare, open_repo, set_reference, write_head_symref,
};
use hosted_client::client::LocalSync;
#[cfg(feature = "client")]
use hosted_client::hosted_runtime::hosted::{
    HostedClient, HostedRefEntry, PullMaterialization, advertised_user_thread_id,
    persist_advertised_thread_identity_with_live_fallback,
};
use ingest::ImportOptions;
#[cfg(feature = "client")]
use objects::fs_atomic::CloneDurabilityBatch;
use objects::{
    Progress,
    error::{HeddleError, Result as HeddleResult},
    object::{Blob, ContentHash, ThreadName},
    store::ObjectStore,
    sync::LockExt,
};
use repo::{BlobHydrator, Repository, ThreadManager};
#[cfg(feature = "client")]
use repo::{RepositorySourceAuthority, clone_intent::CloneIntent};
#[cfg(feature = "client")]
use sley::plumbing::sley_worktree;
use sley::{
    ConfigEdit, ConfigEditPlan, ConfigEditScope, ConfigSectionEntry, GitObjectType,
    IndexWriteOptions, ObjectId, RefPrecondition, RemoteConfigSet, Repository as SleyRepository,
    plumbing::sley_core::redact_url_for_display,
    remote::{ProgressSink as SleyProgressSink, TransferProgress},
};
use verbs::{
    CloneMode, ClonePlanError, ClonePlanFacts, ClonePlanOptions, CloneRemoteSource,
    CloneThreadSelectError, UnsupportedCloneFlag, plan_clone, select_clone_checkout_thread,
    status::next_action::canonical_git_import_ref_command,
};
#[cfg(feature = "client")]
use verbs::{
    MonorepoCloneResultSummary, MonorepoEdgeFacts, MonorepoEdgeSkipReason, MonorepoNodeExecution,
    MonorepoNodeExecutionStep, MonorepoNodeFacts, MonorepoNodeStepOptions,
    assemble_monorepo_clone_json_report, assemble_monorepo_clone_result_summary,
    monorepo_execution_progress, monorepo_rel_display, plan_monorepo_clone,
    plan_monorepo_execution, validate_monorepo_clone_options, validate_monorepo_execution,
};

use super::{
    advice::RecoveryAdvice,
    import_progress::ImportProgress,
    next_action::{NextActionValidationContext, write_full_command_json},
    verification_health::{RepositoryVerificationState, build_repository_verification_state},
};
#[cfg(feature = "client")]
use crate::remote::credential_key_from_remote_url;
use crate::{
    cli::{
        Cli,
        progress_render::{TerminalSink, finish_line, format_transfer_bytes},
        should_output_json, style,
    },
    perf::{ProfileField, emit_profile},
    remote::{Remote, RemoteConfig, RemoteTarget},
};

pub const CLONE_OUTPUT_KIND: &str = "clone";

/// `output_kind` value carried by the *preliminary* JSON record emitted
/// by `clone_network` before the final clone payload. Hosted clones
/// emit two JSON objects on one invocation (connection envelope, then
/// the clone result), so the catalog advertises both discriminators.
pub const CLONE_CONNECTION_OUTPUT_KIND: &str = "clone_connection";

/// Pull/materialization options shared by local and network clone paths.
struct CloneOptions {
    thread: Option<String>,
    depth: Option<u32>,
    lazy: bool,
    filter: Option<String>,
    /// Allow cleartext to non-loopback hosts for this clone. Only read on
    /// the network clone paths, which are gated behind the `client`
    /// feature; a build without `client` never reads this field back out
    /// (`clone_local` explicitly discards it via `insecure: _`).
    #[cfg_attr(not(feature = "client"), allow(dead_code))]
    insecure: bool,
}

struct GitOverlayCloneOutputInput {
    remote: String,
    local: String,
    branch: String,
    commits_imported: usize,
    states_created: usize,
    trust: RepositoryVerificationState,
}

fn git_overlay_clone_output(input: GitOverlayCloneOutputInput) -> CloneOutput {
    CloneOutput {
        output_kind: CLONE_OUTPUT_KIND,
        action: "clone",
        status: "cloned",
        success: true,
        cloned: true,
        transport: "git",
        remote: input.remote,
        local: input.local,
        branch: Some(input.branch),
        repository_capability: Some("git-overlay"),
        commits_imported: Some(input.commits_imported),
        states_created: Some(input.states_created),
        objects: None,
        state: None,
        trust: Some(input.trust),
    }
}

fn heddle_clone_output(
    remote: String,
    local: String,
    branch: String,
    repository_capability: &'static str,
    objects: Option<usize>,
    state: Option<String>,
    trust: Option<RepositoryVerificationState>,
) -> CloneOutput {
    CloneOutput {
        output_kind: CLONE_OUTPUT_KIND,
        action: "clone",
        status: "cloned",
        success: true,
        cloned: true,
        transport: "heddle",
        remote,
        local,
        branch: Some(branch),
        repository_capability: Some(repository_capability),
        commits_imported: None,
        states_created: None,
        objects,
        state,
        trust,
    }
}

#[allow(clippy::too_many_arguments)]
pub async fn cmd_clone(
    cli: &Cli,
    remote: String,
    local: String,
    thread: Option<String>,
    depth: Option<u32>,
    lazy: bool,
    filter: Option<String>,
    recursive: bool,
    insecure: bool,
) -> Result<()> {
    let local_path = PathBuf::from(&local);

    // Cheap remote classification for pure planning (parse may resolve DNS
    // / check path existence; no clone FS body or hosted pull yet).
    let parse_result = RemoteTarget::parse(&remote);
    let remote_source = match &parse_result {
        Ok(RemoteTarget::Local(path)) => CloneRemoteSource::Local {
            path: path.clone(),
            has_heddle: path.join(".heddle").exists(),
            is_git: open_repo(path).is_ok(),
        },
        Ok(RemoteTarget::Network { repo_path, .. }) => CloneRemoteSource::Network {
            has_repo_path: repo_path.is_some(),
        },
        Err(_) => CloneRemoteSource::Unparsed,
    };

    let plan = plan_clone(
        &ClonePlanOptions {
            remote: remote.clone(),
            local: local_path.clone(),
            thread,
            depth,
            lazy,
            filter,
            recursive,
            insecure,
        },
        &ClonePlanFacts {
            destination_exists: local_path.exists(),
            remote_source,
        },
    )
    .map_err(clone_plan_error_to_anyhow)?;

    if insecure && plan.mode.is_git_overlay() {
        return Err(anyhow!(git_overlay_clone_insecure_advice()));
    }

    let options = CloneOptions {
        thread: plan.thread.clone(),
        depth: plan.depth,
        lazy: plan.lazy,
        filter: plan.filter.clone(),
        // Network paths honor the planned security preflight; local paths
        // ignore insecure (clone_local discards it). Recursive monorepo vs
        // single-repo is carried on `plan.mode`, not on CloneOptions.
        insecure: plan.security.allow_insecure,
    };

    #[cfg(feature = "client")]
    let server_key = credential_key_from_remote_url(&remote);

    match plan.mode {
        CloneMode::LocalHeddle { remote_path } => {
            clone_local(cli, &remote_path, &plan.destination, &options).await?;
        }
        CloneMode::LocalGitOverlay { remote_path } => {
            clone_git_overlay_path(cli, &remote_path, &plan.destination, &options)?;
        }
        CloneMode::GitOverlayUrl => {
            clone_git_overlay_url(cli, &remote, &plan.destination, &options)?;
        }
        CloneMode::NetworkHosted { recursive } => {
            let (authority, repo_path) = match parse_result {
                Ok(RemoteTarget::Network {
                    authority,
                    repo_path,
                }) => (authority, repo_path),
                _ => {
                    return Err(anyhow!(clone_invalid_remote_url_advice(&remote)));
                }
            };
            #[cfg(feature = "client")]
            {
                // Security preflight is already assembled on the plan; session
                // build + TLS validation still run inside network/monorepo
                // bodies before any destination mutation.
                let _ = &plan.security;
                if recursive {
                    clone_monorepo(
                        cli,
                        &authority,
                        repo_path.as_deref(),
                        &plan.destination,
                        &options,
                        server_key,
                        hosted_endpoint_spec(&remote),
                    )
                    .await?;
                } else {
                    clone_network(
                        cli,
                        &authority,
                        repo_path.as_deref(),
                        &plan.destination,
                        &options,
                        server_key,
                        hosted_endpoint_spec(&remote),
                    )
                    .await?;
                }
            }
            #[cfg(not(feature = "client"))]
            {
                let _ = (authority, repo_path, recursive, &plan.security);
                return Err(anyhow!(network_clone_unavailable_advice()));
            }
        }
    }

    Ok(())
}

fn clone_plan_error_to_anyhow(err: ClonePlanError) -> anyhow::Error {
    match err {
        ClonePlanError::DestinationExists { path } => {
            anyhow!(clone_destination_exists_advice(&path.display().to_string()))
        }
        ClonePlanError::MonorepoRequiresHosted { remote } => {
            anyhow!(monorepo_requires_hosted_remote_advice(&remote))
        }
        ClonePlanError::RemoteLooksLikeMissingLocalPath { remote } => {
            anyhow!(clone_remote_not_found_advice(Path::new(&remote)))
        }
        ClonePlanError::InvalidRemoteUrl { remote } => {
            anyhow!(clone_invalid_remote_url_advice(&remote))
        }
        ClonePlanError::UnsupportedOption { flag, mode, value } => match mode {
            "local" => {
                let detail = match flag {
                    UnsupportedCloneFlag::Filter => value.as_deref().unwrap_or(""),
                    UnsupportedCloneFlag::Lazy => "true",
                    UnsupportedCloneFlag::Depth => "",
                };
                anyhow!(local_clone_option_unsupported_advice(flag.as_str(), detail))
            }
            "monorepo" => anyhow!(RecoveryAdvice::safety_refusal(
                "monorepo_clone_option_unsupported",
                format!(
                    "{} is not supported with --recursive monorepo clones",
                    flag.as_str()
                ),
                format!(
                    "Run the monorepo clone without `{}`, or clone the individual spool with `{}` non-recursively.",
                    flag.as_str(),
                    flag.as_str()
                ),
                format!(
                    "`{}` changes single-spool pull semantics that don't compose across the anchored-state monorepo walk",
                    flag.as_str()
                ),
                "accepting the flag could leave nodes materialized under mismatched fetch semantics",
                "no destination directory or spool content was written",
                "heddle clone <hosted-spool> <path> --recursive",
                vec!["heddle clone <hosted-spool> <path> --recursive".to_string()],
            )),
            _ => anyhow!(unsupported_git_overlay_clone_option_advice(
                flag.as_str(),
                value.as_deref()
            )),
        },
    }
}

fn clone_invalid_remote_url_advice(remote: &str) -> RecoveryAdvice {
    RecoveryAdvice::safety_refusal(
        "clone_invalid_remote_url",
        format!("Invalid remote URL: {remote}"),
        "Use an existing local repository, a hosted Heddle remote, or a Git clone URL.",
        format!("remote '{remote}' could not be parsed as a supported Heddle or Git remote"),
        "clone cannot determine which transport or repository to read from",
        "no destination directory, repository metadata, refs, or worktree files were written",
        "heddle clone <remote> <path>",
        vec!["heddle clone <remote> <path>".to_string()],
    )
}

fn clone_destination_exists_advice(local: &str) -> RecoveryAdvice {
    RecoveryAdvice::safety_refusal(
        "clone_destination_exists",
        format!("Local path '{local}' already exists"),
        "Choose an empty destination path, or move the existing path aside before retrying `heddle clone`.",
        format!("destination path '{local}' already exists"),
        "clone would need to write repository metadata and worktree files into that destination",
        "existing destination path and current repository state were left unchanged",
        "heddle clone <remote> <new-path>",
        vec!["heddle clone <remote> <new-path>".to_string()],
    )
}

struct GitCloneProgress {
    progress: Progress,
    received_bytes: u64,
    received_objects: u64,
}

impl GitCloneProgress {
    fn new(cli: &Cli) -> Self {
        let progress = if should_output_json(cli, None) {
            Progress::null()
        } else {
            Progress::with_sink(Box::new(TerminalSink::new()))
        };
        progress.set_phase("streaming Git objects");
        Self {
            progress,
            received_bytes: 0,
            received_objects: 0,
        }
    }
}

impl SleyProgressSink for GitCloneProgress {
    fn transfer(&mut self, event: TransferProgress) {
        self.received_bytes = event.received_bytes;
        if let Some(total) = event.total_objects {
            self.progress.set_total(total as usize);
        }
        let received = event.received_objects.saturating_sub(self.received_objects);
        self.received_objects = event.received_objects;
        self.progress.inc(received as usize);
    }

    fn message(&mut self, message: &str) {
        let _ = message;
    }
}

struct FinishedGitOverlayClone {
    output_json: bool,
    remote: String,
    branch: String,
    commits_imported: usize,
    states_created: usize,
    ingest_ms: u128,
    state_store_write_ms: u128,
    trust: RepositoryVerificationState,
}

fn clone_git_overlay_url(
    cli: &Cli,
    url: &str,
    local_path: &Path,
    options: &CloneOptions,
) -> Result<()> {
    reject_unsupported_for_git_overlay(options)?;
    let staging = AtomicCloneDestination::new(local_path)?;
    let mut progress = GitCloneProgress::new(cli);
    let filter = options
        .filter
        .as_deref()
        .or_else(|| options.lazy.then_some("blob:none"));
    let mirror_copy_start = std::time::Instant::now();
    clone_url_to_bare(
        url,
        &staging.path().join(".git"),
        options.depth,
        filter,
        &mut progress,
    )
    .map_err(anyhow::Error::msg)?;
    let mirror_copy_ms = mirror_copy_start.elapsed().as_millis();
    finish_line(
        &progress.progress,
        &format!(
            "[done] streamed {} Git objects ({} received)",
            progress.received_objects,
            format_transfer_bytes(progress.received_bytes)
        ),
    );
    let finished = finish_git_overlay_clone(
        cli,
        staging.path(),
        options,
        url.to_string(),
        redact_url_for_display(url),
    )?;
    staging.publish()?;
    emit_git_overlay_clone_profile(mirror_copy_ms, &finished);
    render_finished_git_overlay_clone(local_path, finished)?;
    Ok(())
}

fn clone_git_overlay_path(
    cli: &Cli,
    remote_path: &Path,
    local_path: &Path,
    options: &CloneOptions,
) -> Result<()> {
    reject_unsupported_for_git_overlay(options)?;
    let staging = AtomicCloneDestination::new(local_path)?;
    SleyRepository::init(staging.path()).map_err(anyhow::Error::msg)?;
    let mirror_copy_start = std::time::Instant::now();
    copy_local_repo_to_bare(remote_path, &staging.path().join(".git"))
        .map_err(anyhow::Error::msg)?;
    let mirror_copy_ms = mirror_copy_start.elapsed().as_millis();
    let remote_label = fs::canonicalize(remote_path)
        .unwrap_or_else(|_| remote_path.to_path_buf())
        .display()
        .to_string();
    let finished = finish_git_overlay_clone(
        cli,
        staging.path(),
        options,
        remote_label.clone(),
        remote_label,
    )?;
    staging.publish()?;
    emit_git_overlay_clone_profile(mirror_copy_ms, &finished);
    render_finished_git_overlay_clone(local_path, finished)?;
    Ok(())
}

/// Reject `--depth` / `--lazy` / `--filter` for Git-overlay clones before
/// any filesystem or network work runs. Pure validation lives in
/// `verbs::validate_clone_mode_options`; this wrapper maps errors to
/// recovery advice for the git-overlay execution path and unit tests.
fn reject_unsupported_for_git_overlay(options: &CloneOptions) -> Result<()> {
    if options.insecure {
        return Err(anyhow!(git_overlay_clone_insecure_advice()));
    }
    verbs::validate_clone_mode_options(
        &CloneMode::GitOverlayUrl,
        options.depth,
        options.lazy,
        options.filter.as_deref(),
    )
    .map_err(clone_plan_error_to_anyhow)
}

fn git_overlay_clone_insecure_advice() -> RecoveryAdvice {
    RecoveryAdvice::safety_refusal(
        "git_overlay_clone_insecure_unsupported",
        "--insecure is not supported for Git-overlay clones",
        "Use a TLS-protected Git URL, or configure the remote's trust through the system certificate store.",
        "Sley does not expose a clone-scoped TLS verification override",
        "accepting the flag would imply a security setting that the Git transport did not apply",
        "no destination directory, repository metadata, refs, or worktree files were written",
        "heddle clone <git-url> <path>",
        vec!["heddle clone <git-url> <path>".to_string()],
    )
}

fn unsupported_git_overlay_clone_option_advice(flag: &str, value: Option<&str>) -> RecoveryAdvice {
    let flag_with_value = value
        .map(|value| format!("{flag} {value}"))
        .unwrap_or_else(|| flag.to_string());
    let detail = match flag {
        "--depth" => "the import step walks ancestry past the shallow boundary",
        _ => "the import step requires all blobs locally",
    };
    RecoveryAdvice::safety_refusal(
        "git_overlay_clone_option_unsupported",
        format!("{flag_with_value} is not yet supported for Git-overlay clones; {detail}"),
        format!("Run a full Git-overlay clone without `{flag}` for now."),
        "Git-overlay import requires a complete local Git object graph",
        format!(
            "accepting `{flag}` now could leave a partially imported clone that Heddle cannot verify"
        ),
        "no clone directory, Git refs, or Heddle state were written",
        "heddle clone <remote> <path>",
        vec!["heddle clone <remote> <path>".to_string()],
    )
}

fn finish_git_overlay_clone(
    cli: &Cli,
    local_path: &Path,
    options: &CloneOptions,
    remote_label: String,
    remote_display: String,
) -> Result<FinishedGitOverlayClone> {
    configure_git_overlay_origin(local_path, &remote_label)?;
    let repo = Repository::init_git_overlay_sidecar(local_path)?.without_fsmonitor();
    let refs = options
        .thread
        .as_ref()
        .map(|thread| vec![thread.clone()])
        .unwrap_or_default();
    let scope = if refs.is_empty() {
        ingest::ImportScope::all()
    } else {
        ingest::ImportScope::refs(refs.clone())
    };
    let scope_label = if refs.is_empty() {
        "all branches and tags".to_string()
    } else {
        refs.join(", ")
    };
    let mut progress = ImportProgress::start(cli, &repo, &scope_label, &remote_display);
    heddle_git_projection::git_core::GitProjection::hydrate_checkout_heddle_notes_without_mirror(
        local_path,
    );
    progress.begin_commit_import();
    let mut on_commit = |event| progress.commit_tick(event);
    let ingest_start = std::time::Instant::now();
    let (stats, _map) = ingest::import_git_into_scoped_with_options_and_progress(
        local_path,
        local_path,
        ImportOptions {
            delta_search: repo.config().storage.delta_search.import,
            ..ImportOptions::default()
        },
        scope,
        Some(&mut on_commit),
    )
    .map_err(|err| {
        anyhow!(clone_git_overlay_import_failed_advice(
            options.thread.as_deref(),
            &remote_display,
            err.to_string()
        ))
    })?;
    let ingest_ms = ingest_start.elapsed().as_millis();
    progress.begin_ref_write();
    progress.finish();

    let track_name = select_clone_thread(
        &repo,
        options.thread.as_deref(),
        read_git_head_branch(&local_path.join(".git")).as_deref(),
        &remote_display,
    )?;
    let tn = ThreadName::new(&track_name);
    let state_id = repo.refs().get_thread(&tn)?.ok_or_else(|| {
        anyhow!(clone_git_overlay_branch_not_imported_advice(
            &track_name,
            &remote_display
        ))
    })?;
    checkout_clone_thread(&repo, &track_name, &state_id)?;
    write_git_head_branch(&local_path.join(".git"), &track_name)?;
    configure_git_overlay_origin_tracking(local_path, &track_name)?;
    verify_git_overlay_clone(&repo, local_path, &track_name, &state_id)?;

    let trust = build_repository_verification_state(&repo);
    Ok(FinishedGitOverlayClone {
        output_json: should_output_json(cli, Some(repo.config())),
        remote: remote_display,
        branch: track_name,
        commits_imported: stats.commits_imported,
        states_created: stats.states_created,
        ingest_ms,
        state_store_write_ms: stats.state_store_write_ms,
        trust,
    })
}

fn emit_git_overlay_clone_profile(mirror_copy_ms: u128, finished: &FinishedGitOverlayClone) {
    emit_profile(
        "git overlay clone phases",
        &[
            ProfileField::millis("sley_mirror_copy_ms", mirror_copy_ms),
            ProfileField::millis("heddle_ingest_ms", finished.ingest_ms),
            ProfileField::millis("state_store_write_ms", finished.state_store_write_ms),
        ],
    );
}

fn render_finished_git_overlay_clone(
    local_path: &Path,
    finished: FinishedGitOverlayClone,
) -> Result<()> {
    if finished.output_json {
        let output = git_overlay_clone_output(GitOverlayCloneOutputInput {
            remote: finished.remote,
            local: local_path.display().to_string(),
            branch: finished.branch,
            commits_imported: finished.commits_imported,
            states_created: finished.states_created,
            trust: finished.trust,
        });
        write_full_command_json(
            &output,
            NextActionValidationContext::without_repo(&["clone"]),
        )?;
    } else {
        let repo_name = clone_repo_name_from_label(&finished.remote);
        for line in
            format_clone_completion_lines(repo_name, finished.commits_imported, &finished.branch)
        {
            println!("{line}");
        }
    }
    Ok(())
}

fn configure_git_overlay_origin(local_path: &Path, remote_label: &str) -> Result<()> {
    let git_repo = SleyRepository::discover(local_path).map_err(anyhow::Error::msg)?;
    let core_plan = git_repo
        .plan_config_set("core.bare", "false", ConfigEditScope::Local)
        .map_err(anyhow::Error::msg)?
        .with_fsync(true);
    git_repo
        .apply_config_edit_plan(core_plan)
        .map_err(anyhow::Error::msg)?;

    let origin = RemoteConfigSet::new("origin")
        .with_url(remote_label)
        .with_fetch_refspec("+refs/heads/*:refs/remotes/origin/*");
    let remote_plan = git_repo
        .plan_remote_set(origin, ConfigEditScope::Local)
        .map_err(anyhow::Error::msg)?
        .with_fsync(true);
    git_repo
        .apply_config_edit_plan(remote_plan)
        .map_err(anyhow::Error::msg)?;
    Ok(())
}

fn configure_git_overlay_origin_tracking(local_path: &Path, branch: &str) -> Result<()> {
    let git_dir = local_path.join(".git");
    let git_repo = open_repo(&git_dir).map_err(|err| {
        anyhow!(clone_verification_failed_advice(
            format!("clone verification failed: cannot reopen Git checkout: {err}"),
            format!(
                "Git repository at '{}' could not be opened",
                git_dir.display()
            ),
            "clone cannot seed origin tracking until the selected Git branch is readable",
            "heddle status",
        ))
    })?;
    let branch_ref = format!("refs/heads/{branch}");
    let reference = git_repo.require_reference(&branch_ref).map_err(|err| {
        anyhow!(clone_verification_failed_advice(
            format!("clone verification failed: selected Git branch '{branch}' is missing: {err}"),
            format!("Git ref '{branch_ref}' is missing after Git-overlay clone"),
            "Git status would report upstream tracking for a branch whose local ref is absent",
            canonical_git_import_ref_command(branch),
        ))
    })?;
    let target = reference.peeled_oid(&git_repo).map_err(|err| {
        anyhow!(clone_verification_failed_advice(
            format!(
                "clone verification failed: selected Git branch '{branch}' is not readable: {err}"
            ),
            format!("Git ref '{branch_ref}' could not be peeled to a commit"),
            "Git status would report upstream tracking for an unreadable branch",
            canonical_git_import_ref_command(branch),
        ))
    })?
    .ok_or_else(|| {
        anyhow!(clone_verification_failed_advice(
            format!("clone verification failed: selected Git branch '{branch}' is unborn"),
            format!("Git ref '{branch_ref}' could not be peeled to a commit"),
            "Git status would report upstream tracking for an unreadable branch",
            canonical_git_import_ref_command(branch),
        ))
    })?;
    set_reference(
        &git_repo,
        &format!("refs/remotes/origin/{branch}"),
        target,
        RefPrecondition::Any,
        "heddle: seed origin remote-tracking branch after clone",
    )
    .map_err(|err| {
        anyhow!(clone_verification_failed_advice(
            format!("clone verification failed: cannot seed origin/{branch}: {err}"),
            format!("Git remote-tracking ref 'refs/remotes/origin/{branch}' could not be written"),
            "Git status would not show the cloned branch tracking origin",
            "heddle status",
        ))
    })?;
    write_git_overlay_branch_upstream(local_path, branch)?;
    Ok(())
}

fn write_git_overlay_branch_upstream(local_path: &Path, branch: &str) -> Result<()> {
    let git_repo = SleyRepository::discover(local_path).map_err(anyhow::Error::msg)?;
    let plan = ConfigEditPlan::new(git_repo.common_dir().join("config"))
        .with_operation(ConfigEdit::replace_section(
            "branch",
            Some(branch.to_string()),
            vec![
                ConfigSectionEntry::new("remote", "origin"),
                ConfigSectionEntry::new("merge", format!("refs/heads/{branch}")),
            ],
        ))
        .with_fsync(true);
    git_repo
        .apply_config_edit_plan(plan)
        .map_err(anyhow::Error::msg)?;
    Ok(())
}

fn verify_git_overlay_clone(
    repo: &Repository,
    local_path: &Path,
    track_name: &str,
    state_id: &objects::object::StateId,
) -> Result<()> {
    ensure_git_excludes_heddle(local_path)?;
    refresh_git_index_to_head(local_path)?;
    if let Some(status) = repo.git_overlay_worktree_status()?
        && !status.is_clean()
    {
        let dirty = clone_dirty_paths(&status).join(", ");
        return Err(anyhow!(clone_verification_failed_advice(
            format!("clone verification failed: Git worktree is not clean after checkout: {dirty}"),
            format!(
                "Git-overlay status reports dirty path(s) after clone checkout at {}: {dirty}",
                local_path.display(),
            ),
            "treating this clone as verified could hide checkout files that were not imported into Heddle",
            "heddle status",
        )));
    }

    let git_head = read_git_head_branch(&local_path.join(".git")).ok_or_else(|| {
        anyhow!(clone_verification_failed_advice(
            "clone verification failed: .git/HEAD is not attached to a branch",
            "Git HEAD is detached after clone verification",
            "Heddle cannot prove which Git branch should map to the imported thread",
            canonical_git_import_ref_command(track_name),
        ))
    })?;
    if git_head != track_name {
        return Err(anyhow!(clone_verification_failed_advice(
            format!(
                "clone verification failed: .git/HEAD points at '{git_head}', but Heddle attached '{track_name}'"
            ),
            format!("Git HEAD branch '{git_head}' does not match Heddle thread '{track_name}'"),
            "continuing would leave Git and Heddle attached to different active names",
            canonical_git_import_ref_command(&git_head),
        )));
    }

    match repo.current_lane()? {
        Some(current) if current == track_name => {}
        Some(current) => {
            return Err(anyhow!(clone_verification_failed_advice(
                format!(
                    "clone verification failed: Heddle active thread is '{current}', expected '{track_name}'"
                ),
                format!(
                    "Heddle active thread '{current}' does not match imported Git branch '{track_name}'"
                ),
                "continuing would report the clone as verified while Heddle is attached to the wrong thread",
                format!("heddle thread switch {track_name} --force"),
            )));
        }
        None => {
            return Err(anyhow!(clone_verification_failed_advice(
                "clone verification failed: Heddle HEAD is detached after clone",
                "Heddle HEAD is detached after clone verification",
                "continuing would report the clone as verified without an attached Heddle thread",
                format!("heddle thread switch {track_name} --force"),
            )));
        }
    }

    let imported = repo.refs().get_thread(&ThreadName::new(track_name))?;
    if imported.as_ref() != Some(state_id) {
        return Err(anyhow!(clone_verification_failed_advice(
            format!(
                "clone verification failed: Git branch '{track_name}' did not map to the imported Heddle state"
            ),
            format!("Git branch '{track_name}' does not map to imported Heddle state {state_id}"),
            "continuing would leave the Git/Heddle mapping unproven for this clone",
            canonical_git_import_ref_command(track_name),
        )));
    }

    Ok(())
}

fn refresh_git_index_to_head(local_path: &Path) -> Result<()> {
    let git = open_repo(local_path).map_err(|err| {
        anyhow!(clone_verification_failed_advice(
            format!("clone verification failed: cannot reopen Git checkout: {err}"),
            format!(
                "Git repository at '{}' could not be opened",
                local_path.display()
            ),
            "clone cannot refresh the Git index to match the selected branch",
            "heddle status",
        ))
    })?;
    let head = git.head().map_err(|err| {
        anyhow!(clone_verification_failed_advice(
            format!("clone verification failed: cannot read Git HEAD: {err}"),
            "Git HEAD could not be read during clone verification",
            "clone cannot refresh the Git index to match the selected branch",
            "heddle status",
        ))
    })?;
    let Some(head_oid) = head.oid else {
        return Ok(());
    };
    let commit = git.read_commit(&head_oid).map_err(|err| {
        anyhow!(clone_verification_failed_advice(
            format!("clone verification failed: cannot read Git HEAD tree: {err}"),
            "Git HEAD tree could not be read during clone verification",
            "clone cannot refresh the Git index to match the selected branch",
            "heddle status",
        ))
    })?;
    let mut index = git.index_from_tree(&commit.tree).map_err(|err| {
        anyhow!(clone_verification_failed_advice(
            format!("clone verification failed: cannot build Git index from HEAD tree: {err}"),
            "Git index could not be rebuilt from HEAD during clone verification",
            "clone cannot prove the Git index and selected branch agree",
            "heddle status",
        ))
    })?;
    index.upgrade_version_for_flags();
    git.write_index(
        &index,
        IndexWriteOptions {
            fsync: true,
            validate_checksum: true,
        },
    )
    .map_err(|err| {
        anyhow!(clone_verification_failed_advice(
            format!("clone verification failed: cannot write Git index: {err}"),
            "Git index could not be written during clone verification",
            "clone cannot prove the Git index and selected branch agree",
            "heddle status",
        ))
    })?;
    Ok(())
}

fn clone_dirty_paths(status: &objects::worktree::WorktreeStatus) -> Vec<String> {
    let mut paths = Vec::new();
    paths.extend(status.added.iter().map(|path| path.display().to_string()));
    paths.extend(
        status
            .modified
            .iter()
            .map(|path| path.display().to_string()),
    );
    paths.extend(status.deleted.iter().map(|path| path.display().to_string()));
    paths.sort();
    paths.dedup();
    paths
}

fn clone_verification_failed_advice(
    error: impl Into<String>,
    unsafe_condition: impl Into<String>,
    would_change: impl Into<String>,
    primary_command: impl Into<String>,
) -> RecoveryAdvice {
    let primary_command = primary_command.into();
    RecoveryAdvice::safety_refusal(
        "clone_verification_failed",
        error,
        format!("Repair the clone mapping, then rerun `{primary_command}`."),
        unsafe_condition,
        would_change,
        "the incomplete destination created by this clone attempt was removed",
        primary_command.clone(),
        vec![primary_command],
    )
}

fn clone_git_overlay_import_failed_advice(
    requested_ref: Option<&str>,
    remote_label: &str,
    cause: String,
) -> RecoveryAdvice {
    let requested = requested_ref
        .map(|name| format!(" for requested ref '{name}'"))
        .unwrap_or_default();
    let primary_command = requested_ref
        .map(|name| format!("heddle clone {remote_label} <path> --thread {name}"))
        .unwrap_or_else(|| format!("heddle clone {remote_label} <path>"));
    RecoveryAdvice::safety_refusal(
        "git_overlay_clone_import_failed",
        format!("Git-overlay clone import failed{requested}: {cause}"),
        "Retry with an existing commit-pointing branch or repair the source repository, then clone again.",
        format!("Git-overlay import failed{requested}: {cause}"),
        "clone cannot create a verified Git/Heddle mapping until the requested refs import cleanly",
        "the incomplete destination created by this clone attempt was removed",
        primary_command.clone(),
        vec![primary_command],
    )
}

fn clone_git_overlay_branch_not_imported_advice(
    track_name: &str,
    remote_label: &str,
) -> RecoveryAdvice {
    let primary_command = format!("heddle clone {remote_label} <path> --thread {track_name}");
    RecoveryAdvice::safety_refusal(
        "git_overlay_clone_branch_not_imported",
        format!("Git clone did not import branch '{track_name}'"),
        "Retry with an existing commit-pointing branch or repair the source repository, then clone again.",
        format!(
            "Git-overlay clone selected branch '{track_name}', but no Heddle thread was imported for it"
        ),
        "materializing this clone would attach Git and Heddle to an unverified or missing branch mapping",
        "the incomplete destination created by this clone attempt was removed",
        primary_command.clone(),
        vec![primary_command],
    )
}

fn clone_git_overlay_no_branch_refs_advice(remote_label: &str) -> RecoveryAdvice {
    let primary_command = format!("heddle clone {remote_label} <path>");
    RecoveryAdvice::safety_refusal(
        "git_overlay_clone_no_branch_refs",
        "Git clone did not import any branch refs",
        "Clone from a repository with at least one commit-pointing branch, or pass `--thread <branch>` after creating one.",
        format!("Git-overlay import from '{remote_label}' produced no branch refs"),
        "clone cannot choose a verified active branch without an imported Git/Heddle mapping",
        "the incomplete destination created by this clone attempt was removed",
        primary_command.clone(),
        vec![primary_command],
    )
}

#[cfg(not(feature = "client"))]
fn network_clone_unavailable_advice() -> RecoveryAdvice {
    RecoveryAdvice::safety_refusal(
        "network_clone_unavailable",
        "Network clone support is not available in this build",
        "Use a build with the `client` feature enabled, or clone from a local path.",
        "this heddle binary was built without hosted/network clone support",
        "clone cannot contact hosted/network remotes without the client transport",
        "no destination directory, repository metadata, refs, or worktree files were written",
        "heddle clone <local-path> <path>",
        vec!["heddle clone <local-path> <path>".to_string()],
    )
}

fn ensure_git_excludes_heddle(local_path: &Path) -> Result<()> {
    Ok(Repository::ensure_git_overlay_local_excludes(local_path)?)
}

/// Best-effort repo-name extraction for the text-mode clone summary.
///
/// The remote label can be a HTTPS URL, an SSH spec
/// (`git@host:owner/repo.git`), a `file://` URL, or a plain filesystem
/// path. We do not try to fully parse any of these — we just want the
/// last path-like segment so the human-facing line can say "Cloned
/// ripgrep" instead of dumping the whole URL again next to where the
/// URL was already echoed by the dim-styled source label. If the input
/// has no usable segment, return it unchanged so the rendered summary
/// still carries something identifying.
fn clone_repo_name_from_label(label: &str) -> &str {
    // `:` is only an SSH/SCP host/path separator when the prefix has no
    // path separator (git's local-path rule) and isn't a Windows drive
    // (`C:\…` or `C:/…`). Splitting unconditionally truncated Windows
    // drive paths and any local path with a literal colon.
    let after_colon = match label.find(':') {
        Some(colon_pos) => {
            let prefix = &label[..colon_pos];
            let rest = &label[colon_pos + 1..];
            let is_windows_drive = prefix.len() == 1
                && prefix
                    .chars()
                    .next()
                    .is_some_and(|c| c.is_ascii_alphabetic())
                && (rest.starts_with('\\') || rest.starts_with('/'));
            let prefix_has_separator = prefix.contains('/') || prefix.contains('\\');
            if is_windows_drive || prefix_has_separator {
                label
            } else {
                rest
            }
        }
        None => label,
    };
    let is_sep = |c: char| c == '/' || c == '\\';
    let segment = after_colon
        .trim_end_matches(is_sep)
        .rsplit(is_sep)
        .find(|part| !part.is_empty())
        .unwrap_or(after_colon);
    segment.strip_suffix(".git").unwrap_or(segment)
}

/// Render the human-facing clone-completion summary as three lines.
///
/// The shape — repo name + commit count, current thread, next-step
/// hint — comes from heddle#161: the previous text mode printed a terse
/// `cloned <url> into <path>` / `imported: N Git commits` pair that
/// scanned like a JSON dump rather than guidance. Returning a `Vec<String>`
/// (one entry per output line) keeps the formatter unit-testable without
/// having to capture process stdout.
fn format_clone_completion_lines(
    repo_name: &str,
    commits_imported: usize,
    thread_name: &str,
) -> Vec<String> {
    vec![
        format!(
            "{} Cloned {} ({} imported).",
            style::ok_marker(),
            style::bold(repo_name),
            style::count(commits_imported, "commit"),
        ),
        format!(
            "  {}",
            style::field("current thread", &style::bold(thread_name))
        ),
        super::action_line::format_next_step_dim("heddle status", 2)
            .expect("static clone next action is non-empty"),
    ]
}

/// Pick which imported branch the clone should land on.
///
/// Same fail-closed rule as [`select_clone_checkout_thread`]: `--thread` must
/// be advertised, else advertised HEAD, else `main`, else the first short name.
fn select_clone_thread(
    repo: &Repository,
    requested: Option<&str>,
    advertised_head: Option<&str>,
    remote_label: &str,
) -> Result<String> {
    let threads = repo.refs().list_threads()?;
    select_clone_checkout_thread(
        requested,
        advertised_head,
        threads.iter().map(ThreadName::as_str),
    )
    .map_err(|err| match err {
        CloneThreadSelectError::RequestedNotAdvertised { requested } => {
            anyhow!(clone_git_overlay_branch_not_imported_advice(
                &requested,
                remote_label
            ))
        }
        CloneThreadSelectError::NoAdvertisedThreads => {
            anyhow!(clone_git_overlay_no_branch_refs_advice(remote_label))
        }
    })
}

/// Read `.git/HEAD` as a symbolic ref into `refs/heads/`, returning
/// the bare branch name. Returns `None` for detached HEAD, malformed
/// files, or symrefs outside `refs/heads/` — none of which can drive
/// thread selection.
fn read_git_head_branch(git_dir: &Path) -> Option<String> {
    let worktree = git_dir.parent().unwrap_or(git_dir);
    let repo = open_repo(worktree).ok()?;
    let head = repo.head_state().ok()?;
    let branch = head.branch_name()?;
    if branch.is_empty() {
        None
    } else {
        Some(branch.to_string())
    }
}

/// Pin `.git/HEAD` to `refs/heads/<branch>`. Called after clone so a
/// future `Repository::open` reads the same branch heddle attached to,
/// rather than the init-time default Sley wrote (typically `main`).
fn write_git_head_branch(git_dir: &Path, branch: &str) -> Result<()> {
    write_head_symref(git_dir, &format!("refs/heads/{branch}"))?;
    Ok(())
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
        insecure: _,
    } = options;
    let depth = *depth;
    if let Some(filter) = filter.as_deref() {
        return Err(anyhow!(local_clone_option_unsupported_advice(
            "--filter", filter
        )));
    }
    if *lazy {
        return Err(anyhow!(local_clone_option_unsupported_advice(
            "--lazy", "true"
        )));
    }

    if !remote_path.exists() {
        return Err(anyhow!(clone_remote_not_found_advice(remote_path)));
    }

    // Resolve the requested remote thread before creating the
    // destination. Missing-thread refusals should not leave behind a
    // half-initialized clone directory.
    let sync = LocalSync::open(remote_path)?;
    let remote_repo = sync.source();
    let advertised_head = advertised_clone_source_lane(remote_repo)?;
    let remote_threads = remote_repo.refs().list_threads()?;
    let track_name = select_clone_checkout_thread(
        thread.as_deref(),
        advertised_head.as_deref(),
        remote_threads.iter().map(ThreadName::as_str),
    )
    .map_err(|err| match err {
        CloneThreadSelectError::RequestedNotAdvertised { requested } => {
            anyhow!(clone_remote_thread_not_found_advice(
                &requested,
                remote_path
            ))
        }
        CloneThreadSelectError::NoAdvertisedThreads => anyhow!(
            clone_remote_thread_not_found_advice(thread.as_deref().unwrap_or("main"), remote_path)
        ),
    })?;
    let state_id = remote_repo
        .refs()
        .get_thread(&ThreadName::new(&track_name))?
        .ok_or_else(|| clone_remote_thread_not_found_advice(&track_name, remote_path))?;

    // Create and initialize the local repository only after all
    // preflight target selection has succeeded.
    fs::create_dir_all(local_path)?;
    let local_repo = Repository::init(local_path)?.without_fsmonitor();

    // Fetch the state and dependencies
    let mut objects_copied = if let Some(d) = depth {
        sync.fetch_state_with_depth(&local_repo, &state_id, d)?
    } else {
        sync.fetch_state(&local_repo, &state_id)?
    };
    if depth.is_none() {
        objects_copied += sync.fetch_markers(&local_repo)?;
    }

    // Materialize saved history only. The source worktree is not copied:
    // uncommitted files must not become contagious. Publish the selected
    // thread attached, never a detached lane.
    if let Err(error) = checkout_clone_thread(&local_repo, &track_name, &state_id) {
        let _ = fs::remove_dir_all(local_path);
        return Err(error);
    }
    if let Some(metadata) =
        ThreadManager::new(remote_repo.heddle_dir()).find_record_by_thread(&track_name)?
    {
        ThreadManager::new(local_repo.heddle_dir()).save_pulled_metadata(
            &track_name,
            &state_id,
            metadata,
        )?;
    }

    let origin_url = configure_local_clone_origin(&local_repo, remote_path)?;

    if should_output_json(cli, Some(local_repo.config())) {
        let output = heddle_clone_output(
            origin_url,
            local_path.display().to_string(),
            track_name.to_string(),
            local_repo.capability_label(),
            Some(objects_copied),
            Some(state_id.to_string()),
            Some(build_repository_verification_state(&local_repo)),
        );
        write_full_command_json(
            &output,
            NextActionValidationContext::without_repo(&["clone"]),
        )?;
    } else {
        let depth_info = depth.map(|d| format!(" (depth {})", d)).unwrap_or_default();
        println!(
            "{} cloned {} into {}{}",
            style::ok_marker(),
            style::dim(&origin_url),
            style::bold(&local_path.display().to_string()),
            style::dim(&depth_info)
        );
        println!(
            "  {}",
            style::field("current thread", &style::bold(&track_name))
        );
        println!(
            "  {}",
            style::field("copied", &style::count(objects_copied, "object"))
        );
    }

    Ok(())
}

fn configure_local_clone_origin(repo: &Repository, remote_path: &Path) -> Result<String> {
    let remote_path = fs::canonicalize(remote_path).unwrap_or_else(|_| remote_path.to_path_buf());
    let origin_url = format!("file://{}", remote_path.display());
    let mut cfg = RemoteConfig::open(repo).map_err(|err| {
        anyhow!(clone_default_remote_failed_advice(
            &origin_url,
            err.to_string()
        ))
    })?;
    cfg.add(
        "origin",
        Remote {
            url: origin_url.clone(),
            insecure: false,
        },
    )
    .map_err(|err| {
        anyhow!(clone_default_remote_failed_advice(
            &origin_url,
            err.to_string()
        ))
    })?;
    Ok(origin_url)
}

fn advertised_clone_source_lane(repo: &Repository) -> Result<Option<String>> {
    repo.current_lane()
        .map_err(|err| anyhow!(clone_source_head_unreadable_advice(&err.to_string())))
}

fn clone_source_head_unreadable_advice(cause: &str) -> RecoveryAdvice {
    RecoveryAdvice::safety_refusal(
        "clone_source_head_unreadable",
        format!("Cannot read the source repository HEAD: {cause}"),
        "Repair the source repository, then retry `heddle clone`.",
        format!("clone cannot choose a default thread because source current_lane failed: {cause}"),
        "falling through to main would attach the clone to the wrong lane",
        "destination path was left unchanged; no local clone repository was initialized",
        "heddle status",
        vec!["heddle status".to_string()],
    )
}

/// Materialize the selected tip without moving HEAD, then publish the thread
/// and an attached HEAD together. Never writes a detached lane.
fn checkout_clone_thread(
    repo: &Repository,
    track_name: &str,
    state_id: &objects::object::StateId,
) -> Result<()> {
    repo.restore_worktree_state_only(state_id, None)?;
    if !repo.worktree_matches_state(state_id)? {
        return Err(anyhow!(clone_checkout_not_attached_advice(track_name)));
    }
    publish_attached_clone_thread(repo, track_name, state_id)
}

fn publish_attached_clone_thread(
    repo: &Repository,
    track_name: &str,
    state_id: &objects::object::StateId,
) -> Result<()> {
    Ok(repo.publish_clone_checkout(&ThreadName::new(track_name), state_id)?)
}

fn clone_checkout_not_attached_advice(track_name: &str) -> RecoveryAdvice {
    RecoveryAdvice::safety_refusal(
        "clone_checkout_not_attached",
        format!("Clone worktree does not match thread '{track_name}'"),
        format!("Retry `heddle clone --thread {track_name}`."),
        format!("clone materialized files that do not match the selected thread '{track_name}'"),
        "refs and HEAD are not published until the worktree matches the selected tip",
        "destination objects may have been written; the selected thread was not published",
        format!("heddle clone --thread {track_name}"),
        vec![format!("heddle clone --thread {track_name}")],
    )
}

fn local_clone_option_unsupported_advice(option: &'static str, value: &str) -> RecoveryAdvice {
    let detail = if option == "--filter" {
        format!("{option} {value}")
    } else {
        option.to_string()
    };
    RecoveryAdvice::safety_refusal(
        "local_clone_option_unsupported",
        format!("{detail} is only supported for hosted/network remotes"),
        "Retry without lazy/filter options for local remotes, or use a hosted/network remote that supports lazy materialization.",
        format!("selected clone transport is local but {detail} requires hosted/network hydration"),
        "clone cannot create a lazy local checkout because the local transport does not provide on-demand object hydration",
        "destination path was left unchanged; no local clone repository was initialized",
        "heddle clone <remote> <path>",
        vec!["heddle clone <remote> <path>".to_string()],
    )
}

fn clone_default_remote_failed_advice(origin_url: &str, cause: String) -> RecoveryAdvice {
    RecoveryAdvice::safety_refusal(
        "clone_default_remote_failed",
        format!("Cloned state, but could not configure default remote 'origin': {cause}"),
        "Inspect the clone, then configure the remote with `heddle remote add origin <url>` if you want push/pull defaults.",
        format!("clone could not write default remote 'origin' for {origin_url}: {cause}"),
        "future push or pull commands would not know which remote to use by default",
        "objects, refs, and worktree files were already copied into the clone",
        "heddle remote add origin <url>",
        vec!["heddle remote add origin <url>".to_string()],
    )
}

fn clone_remote_not_found_advice(remote_path: &Path) -> RecoveryAdvice {
    RecoveryAdvice::safety_refusal(
        "clone_remote_not_found",
        format!(
            "Remote repository '{}' does not exist",
            remote_path.display()
        ),
        "Check the remote path or URL, then retry `heddle clone` with an existing repository.",
        format!(
            "remote repository '{}' does not exist or is not reachable as a local path",
            remote_path.display()
        ),
        "clone cannot read refs, objects, or worktree data from the requested source",
        "destination path was left unchanged; no local clone repository was initialized",
        "heddle clone <remote> <path>",
        vec!["heddle clone <remote> <path>".to_string()],
    )
}

fn clone_remote_thread_not_found_advice(track_name: &str, remote_path: &Path) -> RecoveryAdvice {
    RecoveryAdvice::safety_refusal(
        "clone_remote_thread_not_found",
        format!("Thread '{track_name}' not found in remote"),
        "Inspect the remote with `heddle thread list`, then retry `heddle clone --thread <thread>` with an existing thread.",
        format!(
            "remote '{}' has no Heddle thread named '{track_name}'",
            remote_path.display()
        ),
        "clone cannot choose a state to fetch or materialize until the remote thread resolves",
        "destination path was left unchanged; no local clone repository was initialized",
        "heddle thread list",
        vec!["heddle thread list".to_string()],
    )
}

/// Extract the `host:port` substring from a raw remote URL so the lazy
/// hydrator config can persist it as the descriptor-trust authority.
/// Keeping the hostname matters when the upstream service rotates IPs
/// (e.g. behind a load balancer): an IP baked into the marker at
/// clone time would pin to a stale IP and break later hydrate calls even
/// though the original URL still resolves. The hydrator re-resolves DNS
/// on every process start when given a hostname spec.
#[cfg(feature = "client")]
fn hosted_endpoint_spec(remote: &str) -> String {
    let trimmed = remote.strip_prefix("heddle://").unwrap_or(remote);
    // The address ends at the first slash that introduces a repo path.
    trimmed.split('/').next().unwrap_or(trimmed).to_string()
}

static CLONE_STAGING_SEQUENCE: AtomicU64 = AtomicU64::new(0);

struct AtomicCloneDestination {
    destination: PathBuf,
    staging: PathBuf,
    published: bool,
}

impl AtomicCloneDestination {
    fn new(destination: &Path) -> Result<Self> {
        let parent = destination
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let name = destination
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("repository");
        let staging = loop {
            let sequence = CLONE_STAGING_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let candidate = parent.join(format!(
                ".{name}.heddle-clone-{}-{sequence}",
                std::process::id()
            ));
            match fs::create_dir(&candidate) {
                Ok(()) => break candidate,
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error.into()),
            }
        };
        Ok(Self {
            destination: destination.to_path_buf(),
            staging,
            published: false,
        })
    }

    fn path(&self) -> &Path {
        &self.staging
    }

    fn publish(mut self) -> Result<()> {
        rename_clone_noreplace(&self.staging, &self.destination)?;
        self.published = true;
        Ok(())
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn rename_clone_noreplace(source: &Path, destination: &Path) -> std::io::Result<()> {
    let source = CString::new(source.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
    let destination = CString::new(destination.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
    #[cfg(target_os = "linux")]
    // SAFETY: both CString pointers remain valid for the duration of this call.
    let result = unsafe {
        libc::renameat2(
            libc::AT_FDCWD,
            source.as_ptr(),
            libc::AT_FDCWD,
            destination.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    #[cfg(target_os = "macos")]
    // SAFETY: both CString pointers remain valid for the duration of this call.
    let result =
        unsafe { libc::renamex_np(source.as_ptr(), destination.as_ptr(), libc::RENAME_EXCL) };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn rename_clone_noreplace(source: &Path, destination: &Path) -> std::io::Result<()> {
    if destination.exists() {
        return Err(std::io::Error::from(std::io::ErrorKind::AlreadyExists));
    }
    fs::rename(source, destination)
}

impl Drop for AtomicCloneDestination {
    fn drop(&mut self) {
        if !self.published {
            let _ = fs::remove_dir_all(&self.staging);
        }
    }
}

#[cfg(feature = "client")]
struct CloneDestinationCleanup<'a> {
    path: &'a Path,
    armed: bool,
}

#[cfg(feature = "client")]
impl<'a> CloneDestinationCleanup<'a> {
    fn new(path: &'a Path) -> Self {
        Self {
            path,
            armed: !path.exists(),
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }

    fn arm(&mut self) {
        self.armed = true;
    }
}

#[cfg(feature = "client")]
impl Drop for CloneDestinationCleanup<'_> {
    fn drop(&mut self) {
        if self.armed {
            let _ = fs::remove_dir_all(self.path);
        }
    }
}

#[cfg(feature = "client")]
async fn clone_network(
    cli: &Cli,
    authority: &str,
    repo_path: Option<&str>,
    local_path: &Path,
    options: &CloneOptions,
    server_key: Option<String>,
    endpoint_spec: String,
) -> Result<()> {
    use hosted_client::client::{HostedAuthMode, HostedSession};

    use crate::config::UserConfig;

    let user_config = UserConfig::load_default()?;
    // On every network-connecting command, TLS/auth config validation
    // (`hosted_runtime_config`) must succeed before any irreversible
    // filesystem/repo mutation such as `create_dir_all`, `Repository::init`,
    // state writes, or ref publishes. A rejected security config must leave
    // no partial on-disk artifact.
    let session =
        HostedSession::build(&user_config, server_key, HostedAuthMode::CredentialFallback)?
            .with_allow_insecure(options.insecure);
    let repo_path = repo_path.context("network remotes must include a hosted repository path")?;

    let mut client = session.connect(authority).await?;
    let result = clone_network_connected(
        cli,
        authority,
        repo_path,
        local_path,
        options,
        endpoint_spec,
        &mut client,
    )
    .await;
    client.close().await;
    result
}

#[cfg(feature = "client")]
async fn clone_network_connected(
    cli: &Cli,
    authority: &str,
    repo_path: &str,
    local_path: &Path,
    options: &CloneOptions,
    endpoint_spec: String,
    client: &mut HostedClient,
) -> Result<()> {
    let CloneOptions {
        thread,
        depth,
        lazy,
        filter,
        insecure: _,
    } = options;
    let depth = *depth;
    // `--filter blob:none` is a synonym for `--lazy` on hosted/network
    // remotes; both produce a clone whose blob content is hydrated on demand.
    let lazy = *lazy || filter.is_some();
    let json_output = should_output_json(cli, None);

    if json_output {
        println!(
            "{}",
            serde_json::json!({
                "output_kind": CLONE_CONNECTION_OUTPUT_KIND,
                "status": "connected",
                "address": authority,
            })
        );
    } else {
        println!("Connected to {authority}");
    }

    let mut cleanup = CloneDestinationCleanup::new(local_path);
    // API 0.15 carries a pre-PullReady stream failure as a real terminal
    // failure instead of letting it decode as a server frame. Persist the
    // recovery authority once the hosted connection is established so that a
    // disconnect before clone refs arrive remains resumable. PullReady later
    // replaces this provisional record with the advertised HEAD before any
    // repository data is initialized.
    let provisional_intent = CloneIntent {
        origin: hosted_clone_origin_url(&endpoint_spec, repo_path),
        endpoint: endpoint_spec.clone(),
        repository: repo_path.to_string(),
        thread: thread.clone(),
        advertised_head: None,
        depth,
        lazy,
    };
    provisional_intent.create(local_path)?;
    cleanup.disarm();
    let mut durability = None;
    let materialization = if lazy {
        PullMaterialization::Lazy
    } else {
        PullMaterialization::Full
    };
    let mut advertised = None;
    let mut selected_track = None;
    let (mut result, local_repo) = client
        .clone_pull_with_depth_and_materialization(
            repo_path,
            thread.as_deref(),
            depth,
            materialization,
            |ready, refs| {
                let _ = ready;
                if refs.refs.is_empty() {
                    return Err(wire::ProtocolError::InvalidState(
                        "server does not advertise clone refs".to_string(),
                    ));
                }
                let mut intent = provisional_intent.clone();
                intent.advertised_head = refs.head_thread.clone();
                // An advertised-ref semantic refusal (for example an unknown
                // requested thread) is not a resumable transport interruption:
                // restore the original destination-cleanup contract until the
                // validated intent has replaced the provisional one.
                cleanup.arm();
                let (_, track_name) = create_hosted_clone_intent_after_thread_select(
                    local_path,
                    intent,
                    refs.refs
                        .iter()
                        .filter(|entry| entry.is_user_thread())
                        .map(|entry| entry.name.as_str()),
                )
                .map_err(|error| wire::ProtocolError::InvalidState(error.to_string()))?;
                // Failures after the durable intent are resumable and must remain on disk.
                cleanup.disarm();
                durability = Some(CloneDurabilityBatch::begin(local_path));
                let repo = initialize_hosted_clone_repository(local_path, &refs.refs, &track_name)?;
                selected_track = Some(track_name);
                advertised = Some(refs.clone());
                Ok(repo)
            },
        )
        .await?;
    let advertised = advertised.context("clone response is missing its refs")?;
    let remote_refs = advertised.refs;
    let track_name = selected_track.context("hosted clone did not select a thread")?;
    let git_overlay_clone = hosted_clone_thread_revision_address(&remote_refs, &track_name)
        .is_some_and(|address| address.starts_with("git:"));
    let origin_url = hosted_clone_origin_url(&endpoint_spec, repo_path);
    if git_overlay_clone {
        configure_git_overlay_origin(local_path, &origin_url)?;
    }
    if result.success {
        objects::fault_inject::maybe_panic_at("clone_after_fetch_before_verify");
        let mut final_state = result
            .final_state
            .context("hosted clone completed without a final state")?;
        if let Err(first_error) = verify_hosted_clone(&local_repo, final_state, depth, lazy) {
            local_repo.store().discard_corrupt_clone_packs()?;
            result = client
                .repair_clone_with_depth_and_materialization(
                    &local_repo,
                    repo_path,
                    &track_name,
                    depth,
                    materialization,
                )
                .await
                .map_err(|repair| {
                    anyhow!(
                        "clone verification failed ({first_error}); targeted repair failed ({repair})"
                    )
                })?;
            if !result.success {
                anyhow::bail!(
                    "clone verification failed ({first_error}); targeted repair was rejected: {}",
                    result.error.as_deref().unwrap_or("unknown remote error")
                );
            }
            final_state = result
                .final_state
                .context("targeted clone repair completed without a final state")?;
            verify_hosted_clone(&local_repo, final_state, depth, lazy)
                .context("clone remained incomplete after targeted remote repair")?;
        }
        client
            .fetch_advertised_synthetic_frontier_objects(
                &local_repo,
                repo_path,
                &remote_refs,
                depth,
                materialization,
            )
            .await?;

        let bootstrap =
            hosted_client::hosted_runtime::hosted::decode_pull_bootstrap(&result.checkpoint)
                .context("decode hosted clone bootstrap")?
                .ok_or_else(|| {
                    anyhow!(RecoveryAdvice::network_clone_failed(
                        "hosted response is missing required folded metadata",
                        local_path,
                    ))
                })?
                .resolve(&local_repo, Some(final_state))
                .context("resolve hosted clone bootstrap")?;

        if lazy {
            use repo::lazy_hydrator::LazyHydratorConfig;
            let cfg = LazyHydratorConfig::hosted(
                endpoint_spec.clone(),
                repo_path,
                &track_name,
                &track_name,
            );
            cfg.save(local_repo.heddle_dir())
                .context("failed to persist lazy-hydrator.toml")?;
        }
        configure_hosted_clone_origin(&local_repo, &endpoint_spec, repo_path)?;

        // Read path for hosted discussions (heddle discuss): materialize the
        // hosted CollaborationService discussions for the cloned head into the
        // local op-log so `discuss list` / `discuss show` see them. Best-effort:
        // a fetch hiccup warns rather than failing an otherwise-good clone.
        match hosted_client::client::discussion_sync::pull_discussions(
            &local_repo,
            client,
            repo_path,
            bootstrap.discussions.as_deref(),
            Some(final_state),
        )
        .await
        {
            Ok(_) => {}
            Err(error) => {
                eprintln!(
                    "{} discussion sync skipped: {error:#}",
                    style::warn_marker()
                );
            }
        }
        // Read path for hosted context annotations (heddle context): materialize
        // the hosted head's annotations into the local Context attachment so
        // `context list` sees them. Best-effort, mirroring discussions.
        match hosted_client::client::context_sync::pull_context(
            &local_repo,
            client,
            repo_path,
            bootstrap.context.as_deref(),
            Some(final_state),
        )
        .await
        {
            Ok(_) => {}
            Err(error) => {
                eprintln!("{} context sync skipped: {error:#}", style::warn_marker());
            }
        }

        // Ordering invariant: the reachable object closure is hash-complete;
        // one whole-filesystem barrier commits all direct clone data; only
        // then may refs/HEAD become visible; the intent is cleared last.
        let durability = durability
            .as_ref()
            .context("clone durability was not started")?;
        durability.commit()?;
        if durability.barrier_count() != 1 {
            return Err(HeddleError::InvalidObject(
                "clone durability commit executed more than once".to_string(),
            )
            .into());
        }
        persist_advertised_synthetic_refs(&local_repo, &remote_refs)?;
        client
            .publish_clone_markers(
                &local_repo,
                repo_path,
                &result.checkpoint,
                depth,
                materialization,
            )
            .await?;
        // Lazy clone: persist the hydrator metadata so future
        // `Repository::open` calls (in any process) can reconstruct
        // the on-read hydrator. Without this, lazy clones would only
        // hydrate inside the single `cmd_clone` process — every
        // subsequent `heddle <verb>` would surface MissingObject on
        // any blob read.
        if lazy {
            publish_attached_clone_thread(&local_repo, &track_name, &final_state)?;
        } else if git_overlay_clone {
            finish_hosted_git_overlay_checkout(&local_repo, &track_name)
                .context("failed to finish hosted Git-overlay checkout")?;
            configure_git_overlay_origin_tracking(local_path, &track_name)?;
            publish_attached_clone_thread(&local_repo, &track_name, &final_state)?;
        } else {
            checkout_clone_thread(&local_repo, &track_name, &final_state)
                .context("failed to materialize hosted clone worktree")?;
        }
        persist_hosted_clone_thread_identity(
            &local_repo,
            client,
            repo_path,
            &remote_refs,
            &track_name,
            &final_state,
        )
        .await?;
        CloneIntent::clear(local_path)?;
        if should_output_json(cli, Some(local_repo.config())) {
            let output = heddle_clone_output(
                origin_url.clone(),
                local_path.display().to_string(),
                track_name.clone(),
                local_repo.capability_label(),
                None,
                Some(final_state.to_string()),
                Some(build_repository_verification_state(&local_repo)),
            );
            write_full_command_json(
                &output,
                NextActionValidationContext::without_repo(&["clone"]),
            )?;
        } else {
            let depth_info = depth.map(|d| format!(" (depth {})", d)).unwrap_or_default();
            println!(
                "{} cloned {} into {}{}",
                style::ok_marker(),
                style::dim(&origin_url),
                style::bold(&local_path.display().to_string()),
                style::dim(&depth_info)
            );
            println!(
                "  {}",
                style::field("current thread", &style::bold(&track_name))
            );
            println!(
                "  {}",
                style::field("state", &style::state_id(&final_state.to_string()))
            );
        }
    } else {
        let err = result.error.unwrap_or_else(|| "Unknown error".to_string());
        return Err(anyhow!(RecoveryAdvice::network_clone_failed(
            &err, local_path
        )));
    }

    Ok(())
}

#[cfg(feature = "client")]
pub async fn recover_interrupted_clone(cli: &Cli, start: &Path) -> Result<bool> {
    use hosted_client::client::{HostedAuthMode, HostedSession};

    use crate::config::UserConfig;

    let Some(root) = repo::clone_intent::find_clone_intent_root(start) else {
        return Ok(false);
    };
    let intent = CloneIntent::load(&root)?
        .context("clone intent disappeared while recovery was starting")?;
    let target = RemoteTarget::parse(&intent.origin).map_err(anyhow::Error::msg)?;
    let RemoteTarget::Network { authority, .. } = target else {
        anyhow::bail!(
            "clone intent origin is not a hosted remote: {}",
            intent.origin
        );
    };
    let user_config = UserConfig::load_default()?;
    let server_key = credential_key_from_remote_url(&intent.origin);
    let session =
        HostedSession::build(&user_config, server_key, HostedAuthMode::CredentialFallback)?;
    let mut client = session.connect(&authority).await?;
    let recovered = recover_interrupted_clone_connected(cli, &root, &intent, &mut client).await;
    client.close().await;
    recovered?;
    Ok(true)
}

#[cfg(feature = "client")]
async fn recover_interrupted_clone_connected(
    _cli: &Cli,
    root: &Path,
    intent: &CloneIntent,
    client: &mut HostedClient,
) -> Result<()> {
    let remote_refs = client
        .list_refs_with_revision_addresses(&intent.repository)
        .await?;
    let track_name = select_recover_clone_thread(
        intent,
        remote_refs
            .iter()
            .filter(|entry| entry.is_user_thread())
            .map(|entry| entry.name.as_str()),
    )?;
    let git_overlay_clone = hosted_clone_thread_revision_address(&remote_refs, &track_name)
        .is_some_and(|address| address.starts_with("git:"));
    let durability = CloneDurabilityBatch::begin(root);
    let repo = initialize_hosted_clone_repository(root, &remote_refs, &track_name)?;
    let materialization = if intent.lazy {
        PullMaterialization::Lazy
    } else {
        PullMaterialization::Full
    };
    let mut result = client
        .repair_clone_with_depth_and_materialization(
            &repo,
            &intent.repository,
            &track_name,
            intent.depth,
            materialization,
        )
        .await?;
    if !result.success {
        anyhow::bail!(
            "clone repair failed: {}",
            result.error.as_deref().unwrap_or("unknown remote error")
        );
    }
    let mut final_state = result
        .final_state
        .context("clone repair completed without a final state")?;
    if verify_hosted_clone(&repo, final_state, intent.depth, intent.lazy).is_err() {
        repo.store().discard_corrupt_clone_packs()?;
        result = client
            .repair_clone_with_depth_and_materialization(
                &repo,
                &intent.repository,
                &track_name,
                intent.depth,
                materialization,
            )
            .await?;
        if !result.success {
            anyhow::bail!(
                "clone repair retry failed: {}",
                result.error.as_deref().unwrap_or("unknown remote error")
            );
        }
        final_state = result
            .final_state
            .context("clone repair retry completed without a final state")?;
        verify_hosted_clone(&repo, final_state, intent.depth, intent.lazy)?;
    }
    client
        .fetch_advertised_synthetic_frontier_objects(
            &repo,
            &intent.repository,
            &remote_refs,
            intent.depth,
            materialization,
        )
        .await?;

    if intent.lazy {
        use repo::lazy_hydrator::LazyHydratorConfig;
        LazyHydratorConfig::hosted(
            intent.endpoint.clone(),
            &intent.repository,
            &track_name,
            &track_name,
        )
        .save(repo.heddle_dir())?;
    }
    configure_hosted_clone_origin(&repo, &intent.endpoint, &intent.repository)?;
    let bootstrap =
        hosted_client::hosted_runtime::hosted::decode_pull_bootstrap(&result.checkpoint)?
            .ok_or_else(|| {
                anyhow!(RecoveryAdvice::network_clone_failed(
                    "hosted response is missing required folded metadata",
                    root,
                ))
            })?
            .resolve(&repo, Some(final_state))?;
    if let Err(error) = hosted_client::client::discussion_sync::pull_discussions(
        &repo,
        client,
        &intent.repository,
        bootstrap.discussions.as_deref(),
        Some(final_state),
    )
    .await
    {
        eprintln!(
            "{} discussion sync skipped during clone recovery: {error:#}",
            style::warn_marker()
        );
    }
    if let Err(error) = hosted_client::client::context_sync::pull_context(
        &repo,
        client,
        &intent.repository,
        bootstrap.context.as_deref(),
        Some(final_state),
    )
    .await
    {
        eprintln!(
            "{} context sync skipped during clone recovery: {error:#}",
            style::warn_marker()
        );
    }
    durability.commit()?;
    if durability.barrier_count() != 1 {
        return Err(HeddleError::InvalidObject(
            "clone recovery durability commit executed more than once".to_string(),
        )
        .into());
    }
    persist_advertised_synthetic_refs(&repo, &remote_refs)?;
    client
        .publish_clone_markers(
            &repo,
            &intent.repository,
            &result.checkpoint,
            intent.depth,
            materialization,
        )
        .await?;
    if intent.lazy {
        publish_attached_clone_thread(&repo, &track_name, &final_state)?;
    } else if git_overlay_clone {
        finish_hosted_git_overlay_checkout(&repo, &track_name)?;
        configure_git_overlay_origin(root, &intent.origin)?;
        configure_git_overlay_origin_tracking(root, &track_name)?;
        publish_attached_clone_thread(&repo, &track_name, &final_state)?;
    } else {
        checkout_clone_thread(&repo, &track_name, &final_state)?;
    }
    persist_hosted_clone_thread_identity(
        &repo,
        client,
        &intent.repository,
        &remote_refs,
        &track_name,
        &final_state,
    )
    .await?;
    CloneIntent::clear(root)?;
    Ok(())
}

#[cfg(feature = "client")]
fn verify_hosted_clone(
    repo: &Repository,
    final_state: objects::object::StateId,
    depth: Option<u32>,
    lazy: bool,
) -> Result<usize> {
    let options = wire::StateClosureOptions {
        depth,
        exclude_states: Vec::new(),
    };
    if lazy {
        wire::enumerate_state_closure_plan_with_options(repo.store(), final_state, options)
            .map(|objects| objects.len())
            .map_err(anyhow::Error::new)
    } else {
        wire::enumerate_state_closure_with_options(repo.store(), final_state, options)
            .map(|objects| objects.len())
            .map_err(anyhow::Error::new)
    }
}

/// Recursive MONOREPO clone (Spool epic P9, weft#358).
///
/// The headline user feature: `heddle clone <hosted-spool> --recursive`.
///
/// 1. Connect to the hosted server and `ResolveMonorepo(root_path)` — the
///    server returns the caller's coherent visible slice (per-child
///    visibility, cycle guard, depth bound).
/// 2. Map the transport tree into pure [`MonorepoNodeFacts`], then
///    [`plan_monorepo_clone`] selects children, anchors mount paths, and
///    orders per-node work (root first, pre-order) plus withheld edges.
/// 3. Expand each selected node into pure [`MonorepoNodeExecutionStep`]s via
///    [`plan_monorepo_execution`], validate ordering invariants, then execute
///    FS / hosted I/O per step (progress labels stay pure in core).
/// 4. Assemble placed/skipped summary and report — unreadable / cycle /
///    depth-bounded edges are surfaced, never fatal.
///
/// Pure planning, validation, progress labels, and result summary (steps 2–4
/// facts) live in `verbs::clone_plan` and are unit-tested there. This
/// function owns hosted RPC and per-node materialize I/O.
#[cfg(feature = "client")]
async fn clone_monorepo(
    cli: &Cli,
    authority: &str,
    repo_path: Option<&str>,
    local_path: &Path,
    options: &CloneOptions,
    server_key: Option<String>,
    endpoint_spec: String,
) -> Result<()> {
    use hosted_client::client::{HostedAuthMode, HostedSession};

    use crate::config::UserConfig;

    // Monorepo clone materializes each node at a resolved state; the shallow /
    // lazy / partial knobs don't compose with the multi-spool walk in this
    // first cut. Reject them up front so the user isn't surprised mid-walk.
    reject_unsupported_for_monorepo(options)?;

    let root_path =
        repo_path.context("monorepo clone requires a hosted root spool path in the remote")?;

    let user_config = UserConfig::load_default()?;
    // Security config validation must pass before any irreversible filesystem
    // mutation, exactly as `clone_network` does.
    let session =
        HostedSession::build(&user_config, server_key, HostedAuthMode::CredentialFallback)?
            .with_allow_insecure(options.insecure);

    let mut client = session.connect(authority).await?;
    let result = clone_monorepo_connected(
        cli,
        authority,
        root_path,
        local_path,
        endpoint_spec,
        &mut client,
    )
    .await;
    client.close().await;
    result
}

#[cfg(feature = "client")]
async fn clone_monorepo_connected(
    cli: &Cli,
    authority: &str,
    root_path: &str,
    local_path: &Path,
    endpoint_spec: String,
    client: &mut HostedClient,
) -> Result<()> {
    let json_output = should_output_json(cli, None);
    if json_output {
        println!(
            "{}",
            serde_json::json!({
                "output_kind": CLONE_CONNECTION_OUTPUT_KIND,
                "status": "connected",
                "address": authority,
            })
        );
    } else {
        println!("Connected to {authority}");
    }

    // Resolve the whole child tree into the caller's coherent visible slice,
    // then pure-plan placement, work order, and per-node steps (no FS yet).
    let resolved = client.resolve_monorepo(root_path, None).await?;
    let facts = monorepo_node_facts_from_resolved(&resolved);
    let clone_plan = plan_monorepo_clone(&facts).map_err(|err| anyhow!(err))?;
    let exec = plan_monorepo_execution(&clone_plan, &MonorepoNodeStepOptions::default());
    // Ordering invariants (Init before Fetch, paired fetch/materialize, …)
    // before any irreversible per-node I/O.
    validate_monorepo_execution(&exec).map_err(|err| anyhow!(err))?;

    // Guard the destination: remove it on any failure so a partial monorepo
    // isn't left behind (armed only if it didn't already exist).
    let mut cleanup = CloneDestinationCleanup::new(local_path);
    fs::create_dir_all(local_path)?;

    let total_nodes = exec.node_count();
    for (node_index, node_exec) in exec.nodes.iter().enumerate() {
        let dest = validate_monorepo_destination(local_path, &node_exec.node.rel_path)
            .with_context(|| {
                format!(
                    "refusing unsafe mount path for spool '{}'",
                    node_exec.node.spool_id
                )
            })?;
        execute_monorepo_node_steps(
            client,
            node_exec,
            &dest,
            &endpoint_spec,
            node_index,
            total_nodes,
        )
        .await
        .with_context(|| {
            format!(
                "failed to clone spool '{}' into {}",
                node_exec.node.spool_id,
                dest.display()
            )
        })?;
    }

    cleanup.disarm();

    let summary = assemble_monorepo_clone_result_summary(&exec);

    // Report the outcome, including every withheld child edge.
    if json_output {
        let output = monorepo_clone_output_json(local_path, &summary);
        write_full_command_json(
            &output,
            NextActionValidationContext::without_repo(&["clone"]),
        )?;
    } else {
        // Counts/copy from pure summary; CLI owns markers and bold root.
        let unit = if summary.placed_count == 1 {
            "spool"
        } else {
            "spools"
        };
        println!(
            "{} Cloned monorepo {} ({} {} placed).",
            style::ok_marker(),
            style::bold(root_path),
            summary.placed_count,
            unit,
        );
        for placed in &summary.placed {
            let rel = monorepo_rel_display(&placed.rel_path);
            println!("  {} <- {}", style::dim(&rel), placed.spool_id);
        }
        if let Some(header) = summary.skipped_header() {
            println!("  {header}");
            for sk in &summary.skipped {
                println!(
                    "    {} ({}) at {} — {}",
                    sk.mount_name,
                    sk.child_spool_id,
                    sk.rel_path.display(),
                    sk.reason_label(),
                );
            }
        }
    }
    Ok(())
}

#[cfg(feature = "client")]
fn validate_monorepo_destination(clone_root: &Path, rel_path: &Path) -> Result<PathBuf> {
    let root_metadata = fs::symlink_metadata(clone_root)?;
    if root_metadata.file_type().is_symlink() {
        anyhow::bail!(
            "monorepo clone root '{}' must not be a symlink",
            clone_root.display()
        );
    }
    let canonical_root = fs::canonicalize(clone_root)?;
    let mut checked = canonical_root.clone();

    for component in rel_path.components() {
        let std::path::Component::Normal(name) = component else {
            anyhow::bail!(
                "monorepo mount path '{}' contains an unsafe component",
                rel_path.display()
            );
        };
        checked.push(name);
        match fs::symlink_metadata(&checked) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                anyhow::bail!(
                    "monorepo mount path '{}' traverses symlink '{}'",
                    rel_path.display(),
                    checked.display()
                );
            }
            Ok(metadata) if !metadata.is_dir() => {
                anyhow::bail!(
                    "monorepo mount path '{}' traverses non-directory '{}'",
                    rel_path.display(),
                    checked.display()
                );
            }
            Ok(_) => {
                checked = fs::canonicalize(&checked)?;
                if !checked.starts_with(&canonical_root) {
                    anyhow::bail!(
                        "monorepo mount path '{}' resolves outside clone root '{}'",
                        rel_path.display(),
                        clone_root.display()
                    );
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }

    Ok(clone_root.join(rel_path))
}

/// Map a transport `MonorepoNode` tree into pure core facts (no I/O).
///
/// Parses content-state bytes into [`StateId`]; malformed/absent states map
/// to `None` (empty checkout), matching prior client planner policy.
#[cfg(feature = "client")]
fn monorepo_node_facts_from_resolved(
    node: &api::heddle::api::v1alpha1::MonorepoNode,
) -> MonorepoNodeFacts {
    use objects::object::StateId;

    let content_state = node
        .content_state
        .as_ref()
        .and_then(|state_id| StateId::try_from_slice(&state_id.value).ok());
    let edges = node
        .edges
        .iter()
        .map(|edge| {
            let skip_reason = edge.skipped.and_then(MonorepoEdgeSkipReason::from_wire_i32);
            MonorepoEdgeFacts {
                mount_name: edge.mount_name.clone(),
                child_spool_id: edge.child_spool_id.clone(),
                child: edge.subtree.as_ref().map(monorepo_node_facts_from_resolved),
                skip_reason,
            }
        })
        .collect();
    MonorepoNodeFacts {
        spool_id: node.spool_id.clone(),
        content_state,
        edges,
    }
}

/// JSON envelope for a monorepo clone from the pure result summary.
#[cfg(feature = "client")]
fn monorepo_clone_output_json(
    local_path: &Path,
    summary: &MonorepoCloneResultSummary,
) -> serde_json::Value {
    serde_json::to_value(assemble_monorepo_clone_json_report(local_path, summary))
        .expect("monorepo clone report serializes")
}

/// Execute pure per-node monorepo steps with hosted/FS I/O helpers.
///
/// Step order and gating come from [`plan_monorepo_node_steps`]; ordering is
/// pre-validated by [`validate_monorepo_execution`]. This only performs side
/// effects. Empty content plans omit fetch/materialize so the mount is an
/// initialized empty repo (layout stays coherent).
#[cfg(feature = "client")]
async fn execute_monorepo_node_steps(
    client: &mut HostedClient,
    node_exec: &MonorepoNodeExecution,
    dest: &Path,
    endpoint_spec: &str,
    node_index: usize,
    total_nodes: usize,
) -> Result<()> {
    let spool_id = node_exec.node.spool_id.as_str();
    // Repository is opened by InitRepo and reused by later steps.
    let mut repo: Option<Repository> = None;

    for step in &node_exec.steps {
        let progress = monorepo_execution_progress(node_index, total_nodes, step);
        match step {
            MonorepoNodeExecutionStep::ValidateDest => {
                // Children mount under the root, whose directory already exists;
                // create any intermediate mount directories and the dest itself.
                if let Some(parent) = dest.parent() {
                    fs::create_dir_all(parent)
                        .with_context(|| format!("failed at {}", progress.label()))?;
                }
                fs::create_dir_all(dest)
                    .with_context(|| format!("failed at {}", progress.label()))?;
            }
            MonorepoNodeExecutionStep::InitRepo => {
                repo = Some(
                    Repository::init(dest)
                        .with_context(|| format!("failed at {}", progress.label()))?
                        .without_fsmonitor(),
                );
            }
            MonorepoNodeExecutionStep::FetchContent { state } => {
                let repo = repo.as_ref().with_context(|| {
                    format!(
                        "monorepo FetchContent requires InitRepo first ({})",
                        progress.label()
                    )
                })?;
                // Fetch the exact resolved state's object closure. A
                // `target_state` pull is thread-agnostic on the server (see
                // `locally_complete_*`), so the anchored state — not a thread
                // tip — is what gets materialized.
                client
                    .fetch_state(repo, spool_id, "main", *state)
                    .await
                    .with_context(|| format!("failed at {}", progress.label()))?;
            }
            MonorepoNodeExecutionStep::MaterializeState { state } => {
                let repo = repo.as_ref().with_context(|| {
                    format!(
                        "monorepo MaterializeState requires InitRepo first ({})",
                        progress.label()
                    )
                })?;
                repo.goto_from_materialized_state(state, None)
                    .with_context(|| {
                        format!(
                            "failed to materialize monorepo node worktree ({})",
                            progress.label()
                        )
                    })?;
            }
            MonorepoNodeExecutionStep::RecordMapping => {
                let repo = repo.as_ref().with_context(|| {
                    format!(
                        "monorepo RecordMapping requires InitRepo first ({})",
                        progress.label()
                    )
                })?;
                // Seed origin so each placed spool tracks its own hosted upstream.
                configure_hosted_clone_origin(repo, endpoint_spec, spool_id)
                    .with_context(|| format!("failed at {}", progress.label()))?;
            }
        }
    }
    Ok(())
}

/// Reject `--depth`/`--lazy`/`--filter` for monorepo clones. Pure validation
/// lives in `verbs::validate_monorepo_clone_options`; this wrapper maps
/// errors for the monorepo execution path.
#[cfg(feature = "client")]
fn reject_unsupported_for_monorepo(options: &CloneOptions) -> Result<()> {
    validate_monorepo_clone_options(options.depth, options.lazy, options.filter.as_deref())
        .map_err(clone_plan_error_to_anyhow)
}

fn monorepo_requires_hosted_remote_advice(remote: &str) -> RecoveryAdvice {
    RecoveryAdvice::safety_refusal(
        "monorepo_requires_hosted_remote",
        format!("--recursive monorepo clone requires a hosted spool remote; '{remote}' is not one"),
        "Point `--recursive` at a hosted spool (e.g. `heddle://host/org/root`), or clone this remote without `--recursive`.",
        format!("remote '{remote}' does not resolve to a hosted spool that can carry a child tree"),
        "a monorepo clone must call ResolveMonorepo on a hosted spool to discover its children",
        "no destination directory, repository metadata, or worktree files were written",
        "heddle clone <hosted-spool> <path> --recursive",
        vec!["heddle clone <hosted-spool> <path> --recursive".to_string()],
    )
}

#[cfg(feature = "client")]
fn select_hosted_clone_thread<'a>(
    requested: Option<&str>,
    remote_threads: impl IntoIterator<Item = &'a str>,
    advertised_head: Option<&str>,
    remote_label: &str,
) -> Result<String> {
    select_clone_checkout_thread(requested, advertised_head, remote_threads).map_err(
        |err| match err {
            CloneThreadSelectError::RequestedNotAdvertised { requested } => {
                anyhow!(clone_hosted_thread_not_found_advice(
                    &requested,
                    remote_label
                ))
            }
            CloneThreadSelectError::NoAdvertisedThreads => {
                anyhow!(clone_git_overlay_no_branch_refs_advice(remote_label))
            }
        },
    )
}

/// Validate the advertised thread, then persist the recovery intent. Dest is
/// created only after selection succeeds.
#[cfg(feature = "client")]
fn create_hosted_clone_intent_after_thread_select<'a>(
    local_path: &Path,
    intent: CloneIntent,
    advertised_threads: impl IntoIterator<Item = &'a str>,
) -> Result<(CloneIntent, String)> {
    let track_name = select_hosted_clone_thread(
        intent.thread.as_deref(),
        advertised_threads,
        intent.advertised_head.as_deref(),
        &intent.repository,
    )?;
    intent.create(local_path)?;
    Ok((intent, track_name))
}

#[cfg(feature = "client")]
fn select_recover_clone_thread<'a>(
    intent: &CloneIntent,
    remote_threads: impl IntoIterator<Item = &'a str>,
) -> Result<String> {
    select_hosted_clone_thread(
        intent.thread.as_deref(),
        remote_threads,
        intent.advertised_head.as_deref(),
        &intent.repository,
    )
}

#[cfg(feature = "client")]
fn clone_hosted_thread_not_found_advice(track_name: &str, remote_label: &str) -> RecoveryAdvice {
    RecoveryAdvice::safety_refusal(
        "clone_remote_thread_not_found",
        format!("Thread '{track_name}' not found in remote"),
        "Inspect the remote with `heddle thread list`, then retry `heddle clone --thread <thread>` with an existing thread.",
        format!("remote '{remote_label}' has no Heddle thread named '{track_name}'"),
        "clone cannot choose a state to fetch or materialize until the remote thread resolves",
        "destination path was left unchanged; no local clone repository was initialized",
        "heddle thread list",
        vec!["heddle thread list".to_string()],
    )
}

#[cfg(feature = "client")]
async fn persist_hosted_clone_thread_identity(
    repo: &Repository,
    client: &mut HostedClient,
    repo_path: &str,
    remote_refs: &[HostedRefEntry],
    track_name: &str,
    final_state: &objects::object::StateId,
) -> Result<()> {
    if let Some(metadata) = client
        .try_get_thread_metadata(repo, repo_path, track_name, *final_state)
        .await?
    {
        ThreadManager::new(repo.heddle_dir()).save_pulled_metadata(
            track_name,
            final_state,
            metadata,
        )?;
        return Ok(());
    }
    // Prefer the advertised pull-path thread_id when present. Refresh live
    // ListRefs only when that advertisement omitted the stable id.
    let live_refs = if advertised_user_thread_id(remote_refs, track_name).is_none() {
        Some(client.list_refs_with_revision_addresses(repo_path).await?)
    } else {
        None
    };
    persist_advertised_thread_identity_with_live_fallback(
        repo,
        remote_refs,
        live_refs.as_deref(),
        track_name,
        final_state,
    )?;
    Ok(())
}

#[cfg(feature = "client")]
fn hosted_clone_thread_revision_address<'a>(
    remote_refs: &'a [HostedRefEntry],
    thread: &str,
) -> Option<&'a str> {
    remote_refs
        .iter()
        .find(|entry| entry.name == thread && entry.is_user_thread())
        .map(|entry| entry.revision_address.as_str())
}

#[cfg(feature = "client")]
fn initialize_hosted_clone_repository(
    root: &Path,
    remote_refs: &[HostedRefEntry],
    track_name: &str,
) -> std::result::Result<Repository, wire::ProtocolError> {
    let source_authority = if hosted_clone_thread_revision_address(remote_refs, track_name)
        .is_some_and(|address| address.starts_with("git:"))
    {
        RepositorySourceAuthority::GitOverlay
    } else {
        RepositorySourceAuthority::Native
    };
    fs::create_dir_all(root)
        .map_err(|error| wire::ProtocolError::InvalidState(error.to_string()))?;
    if source_authority == RepositorySourceAuthority::GitOverlay && !root.join(".git").exists() {
        SleyRepository::init(root)
            .map_err(|error| wire::ProtocolError::InvalidState(error.to_string()))?;
    }
    Repository::init_clone(root, source_authority)
        .map(Repository::without_fsmonitor)
        .map_err(wire::ProtocolError::from)
}

#[cfg(feature = "client")]
fn persist_advertised_synthetic_refs(
    repo: &Repository,
    remote_refs: &[HostedRefEntry],
) -> std::result::Result<(), wire::ProtocolError> {
    for entry in remote_refs {
        match entry.to_wire_entry().advertised() {
            Ok(wire::AdvertisedRef::SyntheticFrontier(name)) => {
                if !repo.store().has_state(&entry.state_id)? {
                    return Err(wire::ProtocolError::InvalidState(format!(
                        "synthetic frontier {} advertised {} but the target state is absent",
                        name.as_name(),
                        entry.state_id.to_string_full()
                    )));
                }
                repo.refs()
                    .set_synthetic_frontier(&name, &entry.state_id)
                    .map_err(wire::ProtocolError::from)?;
            }
            Ok(wire::AdvertisedRef::Thread(_) | wire::AdvertisedRef::Marker(_)) | Err(_) => {}
        }
    }
    Ok(())
}

#[cfg(feature = "client")]
fn finish_hosted_git_overlay_checkout(repo: &Repository, branch: &str) -> Result<()> {
    Repository::ensure_git_overlay_local_excludes(repo.root())?;
    let git_repo = SleyRepository::discover(repo.root()).map_err(anyhow::Error::msg)?;
    let config = git_repo.config_snapshot().map_err(anyhow::Error::msg)?;
    let checkout = sley_worktree::checkout_branch_filtered(
        repo.root(),
        git_repo.git_dir(),
        git_repo.object_format(),
        branch,
        hosted_clone_reflog_committer(),
        &config,
    )
    .map_err(anyhow::Error::msg)?;
    if checkout.oid.is_null() {
        let branch_ref = format!("refs/heads/{branch}");
        anyhow::bail!("hosted Git-overlay clone missing {branch_ref}");
    }
    sley_worktree::reset_index_and_worktree_to_commit(
        repo.root(),
        git_repo.git_dir(),
        git_repo.object_format(),
        &checkout.oid,
    )
    .map_err(anyhow::Error::msg)?;
    Ok(())
}

#[cfg(feature = "client")]
fn hosted_clone_reflog_committer() -> Vec<u8> {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0);
    format!("Heddle <heddle@local> {seconds} +0000").into_bytes()
}

#[cfg(feature = "client")]
fn configure_hosted_clone_origin(
    repo: &Repository,
    endpoint_spec: &str,
    repo_path: &str,
) -> Result<String> {
    let origin_url = hosted_clone_origin_url(endpoint_spec, repo_path);
    let mut cfg = RemoteConfig::open(repo).map_err(|err| {
        anyhow!(clone_default_remote_failed_advice(
            &origin_url,
            err.to_string()
        ))
    })?;
    cfg.add(
        "origin",
        Remote {
            url: origin_url.clone(),
            insecure: false,
        },
    )
    .map_err(|err| {
        anyhow!(clone_default_remote_failed_advice(
            &origin_url,
            err.to_string()
        ))
    })?;
    Ok(origin_url)
}

#[cfg(feature = "client")]
fn hosted_clone_origin_url(endpoint_spec: &str, repo_path: &str) -> String {
    format!("heddle://{endpoint_spec}/{repo_path}")
}

/// Read-time blob hydrator for **Git-overlay** lazy clones (issue #50).
///
/// Plugs into [`repo::Repository::set_blob_hydrator`]. When
/// [`Repository::require_blob`] hits a missing-blob marker — i.e. the
/// blake3-hashed blob is recorded in `.heddle/partial-fetch` but is
/// absent from the local object store — the read path delegates here.
/// This hydrator looks up the corresponding Git object id, fetches the
/// blob from the underlying sley repo when it is already present locally
/// and writes the bytes into the heddle store. Native promisor fetching
/// for absent Git blobs is not implemented yet; Heddle rejects public
/// Git-overlay lazy/filtered clones until that path can run without a
/// `git` executable.
///
/// ## Why a side-table?
///
/// `PartialFetchMetadata` records blake3 hashes only, but
/// `Repository::read_object` is keyed by Git OID. Git Projection
/// already computes blake3↔git mappings *for commits* (see
/// `SyncMapping` in `heddle-git-projection::git_core`); blob mappings are
/// constructed on-the-fly during import. We accept the same shape of
/// mapping here, populated by the caller (clone-time or test-time)
/// before [`Self::hydrate`] fires. Future work: persist a sidecar
/// blob mapping during import so a fresh `Repository::open` in a
/// separate process can rebuild this map without re-walking history.
pub struct GitOverlayBlobHydrator {
    git_repo_path: PathBuf,
    /// Pre-seeded blake3 → git OID mapping for missing blobs. Held
    /// behind `Mutex` so a long-lived `Arc<GitOverlayBlobHydrator>` is
    /// `Send + Sync` while still allowing the mapping to grow over
    /// time (e.g. if the import path is later extended to record new
    /// blobs as it walks).
    blob_oid_map: Mutex<std::collections::HashMap<ContentHash, ObjectId>>,
}

impl GitOverlayBlobHydrator {
    pub fn new(git_repo_path: PathBuf) -> Self {
        Self {
            git_repo_path,
            blob_oid_map: Mutex::new(std::collections::HashMap::new()),
        }
    }

    /// Pre-seed the blake3 → git OID mapping. Called by the importer
    /// (or by tests) as missing blobs are discovered.
    pub fn record_blob_oid(&self, hash: ContentHash, oid: ObjectId) {
        self.blob_oid_map.lock_or_poisoned().insert(hash, oid);
    }
}

impl BlobHydrator for GitOverlayBlobHydrator {
    fn hydrate(&self, repo: &Repository, hash: &ContentHash) -> HeddleResult<()> {
        let oid = self
            .blob_oid_map
            .lock_or_poisoned()
            .get(hash)
            .copied()
            .ok_or_else(|| {
                HeddleError::Config(format!(
                    "Git-overlay hydrator has no Git OID mapping for blake3 {}; \
                     the importer must call `record_blob_oid` for every missing blob \
                     before reads can be served lazily",
                    hash.to_hex()
                ))
            })?;

        let bytes = self.read_blob_bytes(oid)?;
        let heddle_blob = Blob::new(bytes);
        // Sanity-check the upstream gave us bytes that match the
        // blake3 we were asked for — protects against an oid mapping
        // corruption silently delivering the wrong content.
        let computed = heddle_blob.hash();
        if computed != *hash {
            return Err(HeddleError::Corruption {
                expected: *hash,
                found: computed,
            });
        }
        repo.store().put_blob(&heddle_blob)?;
        Ok(())
    }
}

impl GitOverlayBlobHydrator {
    fn read_blob_bytes(&self, oid: ObjectId) -> HeddleResult<Vec<u8>> {
        let object = open_repo(&self.git_repo_path)
            .map_err(|err| HeddleError::Io(std::io::Error::other(err.to_string())))?
            .read_object(&oid)
            .map_err(|err| {
                HeddleError::Io(std::io::Error::other(format!(
                    "Git object {oid} could not be read from {}; native Git-overlay lazy hydration is not implemented yet. Re-run a full clone/import without --lazy or --filter so Heddle has a complete local object graph. Cause: {err}",
                    self.git_repo_path.display()
                )))
            })?;
        if object.object_type == GitObjectType::Blob {
            return Ok(object.body.clone());
        }

        Err(HeddleError::Config(format!(
            "Git object {oid} in {} is not a blob; native Git-overlay lazy hydration is not implemented yet. Re-run a full clone/import without --lazy or --filter so Heddle has a complete local object graph.",
            self.git_repo_path.display()
        )))
    }
}

/// Register the `"git-overlay"` factory in the global lazy-hydrator
/// registry. Call once at process startup (from `main()`) so a
/// `Repository::open` on a lazy-cloned repo can reconstruct the
/// hydrator without re-running `cmd_clone`.
///
/// Note: the rebuilt hydrator's `blob_oid_map` starts empty, since the
/// blake3 → git-OID map is populated only by the importer (currently
/// in-process only). Cross-process git-overlay lazy reads are not yet
/// fully wired — `--lazy` for git-overlay clones is rejected at the
/// flag-validation surface (see `reject_unsupported_for_git_overlay`),
/// so this factory is registered for symmetry and forward-compat with
/// follow-up work that persists the OID map sidecar. Until then the
/// hydrator returns the descriptive `"no Git OID mapping"` error if a
/// missing blob is requested.
pub fn register_git_overlay_factory() {
    use std::{path::Path as StdPath, sync::Arc as StdArc};

    use repo::lazy_hydrator::{
        BlobHydratorFactory, HydratorSection, KIND_GIT_OVERLAY, register_factory,
    };

    let factory: BlobHydratorFactory = StdArc::new(
        |root: &StdPath, _section: &HydratorSection| -> HeddleResult<StdArc<dyn BlobHydrator>> {
            let bare = root.join(".git");
            Ok(StdArc::new(GitOverlayBlobHydrator::new(bare)))
        },
    );
    register_factory(KIND_GIT_OVERLAY, factory);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn heddle_clone_output_uses_native_repository_capability() {
        let temp = tempfile::TempDir::new().expect("temp");
        let repo = Repository::init(temp.path()).expect("init native repo");

        let output = heddle_clone_output(
            "file:///tmp/native".to_string(),
            temp.path().display().to_string(),
            "main".to_string(),
            repo.capability_label(),
            None,
            None,
            None,
        );

        assert_eq!(repo.capability_label(), "native-heddle");
        assert_eq!(output.repository_capability, Some("native-heddle"));
    }

    #[cfg(feature = "client")]
    #[test]
    fn hosted_clone_thread_selection_prefers_main() {
        let selected = select_hosted_clone_thread(None, ["master", "main"], None, "owner/repo")
            .expect("thread selected");

        assert_eq!(selected, "main");
    }

    #[cfg(feature = "client")]
    #[test]
    fn hosted_clone_thread_selection_uses_only_advertised_master() {
        let selected = select_hosted_clone_thread(None, ["master"], None, "owner/repo")
            .expect("thread selected");

        assert_eq!(selected, "master");
    }

    #[cfg(feature = "client")]
    #[test]
    fn hosted_clone_thread_selection_honors_requested_thread() {
        let selected =
            select_hosted_clone_thread(Some("feature"), ["main", "feature"], None, "owner/repo")
                .expect("thread selected");

        assert_eq!(selected, "feature");
    }

    #[cfg(feature = "client")]
    #[test]
    fn hosted_clone_thread_selection_uses_advertised_head() {
        let selected = select_hosted_clone_thread(
            None,
            ["alpha", "main", "trunk"],
            Some("trunk"),
            "owner/repo",
        )
        .expect("thread selected");

        assert_eq!(selected, "trunk");
    }

    #[cfg(feature = "client")]
    #[test]
    fn hosted_clone_thread_selection_refuses_unknown_requested_thread() {
        let err = select_hosted_clone_thread(Some("missing"), ["main"], None, "owner/repo")
            .expect_err("missing thread must fail closed");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("missing"),
            "unknown --thread must name the missing thread: {msg}"
        );
    }

    #[cfg(feature = "client")]
    #[test]
    fn hosted_clone_initialization_adopts_advertised_git_lane() {
        let temp = tempfile::TempDir::new().expect("temp");
        let root = temp.path().join("clone");
        CloneIntent {
            origin: "heddle://127.0.0.1:8421/owner/repo".to_string(),
            endpoint: "127.0.0.1:8421".to_string(),
            repository: "owner/repo".to_string(),
            thread: Some("main".to_string()),
            advertised_head: Some("main".to_string()),
            depth: None,
            lazy: false,
        }
        .create(&root)
        .expect("create clone intent");
        let remote_refs = vec![HostedRefEntry::from_advertised(
            "main".to_string(),
            objects::object::StateId::from_bytes([1; 32]),
            true,
            "git:5b2471720c93ee30e5764a19f3d3b3ae9ec9712a".to_string(),
            Some("thread-main".to_string()),
        )];

        let repo = initialize_hosted_clone_repository(&root, &remote_refs, "main")
            .expect("initialize Git-lane clone");

        assert_eq!(
            repo.source_authority(),
            RepositorySourceAuthority::GitOverlay
        );
        assert!(root.join(".git").is_dir());
    }

    #[cfg(feature = "client")]
    #[test]
    fn hosted_clone_initialization_keeps_native_lane_native() {
        let temp = tempfile::TempDir::new().expect("temp");
        let root = temp.path().join("clone");
        CloneIntent {
            origin: "heddle://127.0.0.1:8421/owner/repo".to_string(),
            endpoint: "127.0.0.1:8421".to_string(),
            repository: "owner/repo".to_string(),
            thread: Some("main".to_string()),
            advertised_head: Some("main".to_string()),
            depth: None,
            lazy: false,
        }
        .create(&root)
        .expect("create clone intent");
        let remote_refs = vec![HostedRefEntry::from_advertised(
            "main".to_string(),
            objects::object::StateId::from_bytes([2; 32]),
            true,
            "heddle:0123456789abcdef".to_string(),
            Some("thread-main".to_string()),
        )];

        let repo = initialize_hosted_clone_repository(&root, &remote_refs, "main")
            .expect("initialize native clone");

        assert_eq!(repo.source_authority(), RepositorySourceAuthority::Native);
        assert!(!root.join(".git").exists());
    }

    #[cfg(feature = "client")]
    #[test]
    fn hosted_clone_persists_advertised_thread_stable_id() {
        let temp = tempfile::TempDir::new().expect("temp");
        let repo = Repository::init_default(temp.path()).expect("init");
        std::fs::write(temp.path().join("tracked.txt"), b"cloned\n").unwrap();
        let state = repo
            .snapshot(Some("seed".into()), None)
            .expect("snapshot")
            .state_id;
        let minted = ThreadManager::new(repo.heddle_dir())
            .find_record_by_thread("main")
            .unwrap()
            .expect("init persists main");
        let remote_refs = vec![HostedRefEntry::from_advertised(
            "main".to_string(),
            state,
            true,
            format!("heddle:{}", state.to_string_full()),
            Some("hosted-stable-main".to_string()),
        )];

        persist_advertised_thread_identity_with_live_fallback(
            &repo,
            &remote_refs,
            None,
            "main",
            &state,
        )
        .expect("persist advertised identity");

        let persisted = ThreadManager::new(repo.heddle_dir())
            .find_record_by_thread("main")
            .unwrap()
            .expect("clone must persist a main record");
        assert_eq!(persisted.id, "hosted-stable-main");
        assert_ne!(persisted.id, minted.id);
        assert_ne!(persisted.id, "main");
    }

    #[cfg(feature = "client")]
    #[test]
    fn hosted_clone_refreshes_list_refs_when_folded_refs_omit_thread_id() {
        let temp = tempfile::TempDir::new().expect("temp");
        let repo = Repository::init_default(temp.path()).expect("init");
        std::fs::write(temp.path().join("tracked.txt"), b"cloned\n").unwrap();
        let state = repo
            .snapshot(Some("seed".into()), None)
            .expect("snapshot")
            .state_id;
        let minted = ThreadManager::new(repo.heddle_dir())
            .find_record_by_thread("main")
            .unwrap()
            .expect("init persists main");
        let folded_refs = vec![HostedRefEntry::from_advertised(
            "main".to_string(),
            state,
            true,
            format!("heddle:{}", state.to_string_full()),
            None,
        )];
        let live_refs = vec![HostedRefEntry::from_advertised(
            "main".to_string(),
            state,
            true,
            format!("heddle:{}", state.to_string_full()),
            Some("hosted-stable-main".to_string()),
        )];

        assert!(advertised_user_thread_id(&folded_refs, "main").is_none());
        persist_advertised_thread_identity_with_live_fallback(
            &repo,
            &folded_refs,
            Some(&live_refs),
            "main",
            &state,
        )
        .expect("persist from live ListRefs when the fold omitted thread_id");

        let persisted = ThreadManager::new(repo.heddle_dir())
            .find_record_by_thread("main")
            .unwrap()
            .expect("clone must persist a main record");
        assert_eq!(persisted.id, "hosted-stable-main");
        assert_ne!(persisted.id, minted.id);
        assert_ne!(persisted.id, "main");
    }

    #[cfg(feature = "client")]
    #[test]
    fn hosted_clone_persists_synthetic_frontier_and_does_not_treat_it_as_a_thread() {
        let temp = tempfile::TempDir::new().expect("temp");
        let root = temp.path().join("clone");
        CloneIntent {
            origin: "heddle://127.0.0.1:8421/owner/repo".to_string(),
            endpoint: "127.0.0.1:8421".to_string(),
            repository: "owner/repo".to_string(),
            thread: Some("main".to_string()),
            advertised_head: Some("main".to_string()),
            depth: None,
            lazy: false,
        }
        .create(&root)
        .expect("create clone intent");
        let change = objects::object::ChangeId::from_bytes([9; 16]);
        let frontier = objects::object::SyntheticFrontierName::new("main", change).unwrap();
        let frontier_state = objects::object::StateId::from_bytes([3; 32]);
        let remote_refs = vec![
            HostedRefEntry::from_advertised(
                "main".to_string(),
                objects::object::StateId::from_bytes([2; 32]),
                true,
                "heddle:main".to_string(),
                Some("thread-main".to_string()),
            ),
            HostedRefEntry::from_advertised(
                frontier.as_name(),
                frontier_state,
                true,
                "heddle:frontier".to_string(),
                None,
            ),
        ];

        assert!(!remote_refs[1].is_user_thread());
        assert!(!remote_refs[1].is_marker());
        let selected = select_hosted_clone_thread(
            None,
            remote_refs
                .iter()
                .filter(|entry| entry.is_user_thread())
                .map(|entry| entry.name.as_str()),
            Some("main"),
            "owner/repo",
        )
        .expect("thread selected");
        assert_eq!(selected, "main");

        let repo = initialize_hosted_clone_repository(&root, &remote_refs, "main")
            .expect("initialize clone with synthetic root");
        assert_eq!(
            repo.refs()
                .get_synthetic_frontier(&frontier)
                .expect("read synthetic"),
            None,
            "initialize must not publish a synthetic ref before its objects exist"
        );
        persist_advertised_synthetic_refs(&repo, &remote_refs)
            .expect_err("persist must fail closed when the advertised state is absent");

        std::fs::write(root.join("README"), "frontier\n").expect("seed worktree");
        let snapshot = repo
            .snapshot(Some("frontier".to_string()), None)
            .expect("seed state");
        let remote_refs = vec![
            HostedRefEntry::from_advertised(
                "main".to_string(),
                snapshot.state_id,
                true,
                "heddle:main".to_string(),
                Some("thread-main".to_string()),
            ),
            HostedRefEntry::from_advertised(
                frontier.as_name(),
                snapshot.state_id,
                true,
                "heddle:frontier".to_string(),
                None,
            ),
        ];
        persist_advertised_synthetic_refs(&repo, &remote_refs)
            .expect("persist after objects exist");
        assert!(
            repo.store()
                .has_state(&snapshot.state_id)
                .expect("has_state"),
            "clone must store the synthetic target state, not only the ref"
        );
        assert_eq!(
            repo.refs()
                .get_synthetic_frontier(&frontier)
                .expect("read synthetic"),
            Some(snapshot.state_id)
        );
        assert!(
            repo.refs()
                .get_thread(&objects::object::ThreadName::new(frontier.as_name()))
                .is_err(),
            "a synthetic root must not be readable as a ThreadName"
        );
    }

    // weft#633: the hosted ListRefs response for a git-overlay repo carries
    // companion entries keyed by the full Git ref name (`refs/heads/trunk`)
    // alongside the real short-name thread. Those must never be selected as the
    // clone track target — `refs/heads/trunk` sorts before `trunk`, so without
    // the filter default selection would pick the companion and clone would
    // fail on a bogus track name.
    #[cfg(feature = "client")]
    #[test]
    fn hosted_clone_thread_selection_ignores_git_ref_companions() {
        let selected = select_hosted_clone_thread(
            None,
            ["refs/heads/trunk", "trunk", "refs/heads/main", "main"],
            None,
            "owner/repo",
        )
        .expect("thread selected");
        assert_eq!(
            selected, "main",
            "companion refs/heads/* entries are ignored"
        );

        let non_main =
            select_hosted_clone_thread(None, ["refs/heads/trunk", "trunk"], None, "owner/repo")
                .expect("thread selected");
        assert_eq!(
            non_main, "trunk",
            "a non-main default resolves to the short thread, not its companion"
        );
    }

    #[cfg(feature = "client")]
    #[test]
    fn hosted_endpoint_spec_preserves_hostname_with_port() {
        // The lazy-hydrator marker must carry the original hostname so
        // the hydrator can re-resolve DNS on every process start. If we
        // accidentally persist a resolved IP, hosts behind a rotating-IP
        // load balancer break on the next process restart.
        assert_eq!(
            hosted_endpoint_spec("example.heddle.cloud:443"),
            "example.heddle.cloud:443",
        );
    }

    #[cfg(feature = "client")]
    #[test]
    fn hosted_endpoint_spec_strips_scheme_prefix() {
        assert_eq!(
            hosted_endpoint_spec("heddle://example.heddle.cloud:443"),
            "example.heddle.cloud:443",
        );
    }

    #[cfg(feature = "client")]
    #[test]
    fn hosted_endpoint_spec_strips_repo_path_suffix() {
        assert_eq!(
            hosted_endpoint_spec("example.heddle.cloud:443/org/acme/repo"),
            "example.heddle.cloud:443",
        );
        assert_eq!(
            hosted_endpoint_spec("heddle://example.heddle.cloud:443/org/acme/repo"),
            "example.heddle.cloud:443",
        );
    }

    #[cfg(feature = "client")]
    #[test]
    fn hosted_clone_origin_is_persisted_as_default_remote() {
        let temp = tempfile::TempDir::new().expect("temp");
        let repo = Repository::init_default(temp.path()).expect("init repo");

        let origin = configure_hosted_clone_origin(&repo, "weft.local:8421", "smoke-cli/project")
            .expect("configure hosted origin");

        assert_eq!(origin, "heddle://weft.local:8421/smoke-cli/project");
        let cfg = RemoteConfig::open(&repo).expect("open remotes");
        assert_eq!(cfg.default_name(), Some("origin"));
        assert_eq!(
            cfg.get("origin").expect("origin remote").url,
            "heddle://weft.local:8421/smoke-cli/project"
        );
    }

    #[cfg(feature = "client")]
    #[test]
    fn monorepo_destination_preserves_safe_nested_mounts() {
        let temp = tempfile::TempDir::new().expect("temp");
        let root = temp.path().join("clone");
        std::fs::create_dir_all(root.join("libs")).expect("create clone root");

        let destination = validate_monorepo_destination(&root, Path::new("libs/vendor"))
            .expect("safe nested mount");

        assert_eq!(destination, root.join("libs/vendor"));
    }

    #[cfg(all(feature = "client", unix))]
    #[test]
    fn monorepo_destination_rejects_symlinked_mount_ancestor() {
        let temp = tempfile::TempDir::new().expect("temp");
        let root = temp.path().join("clone");
        let outside = temp.path().join("outside");
        std::fs::create_dir_all(&root).expect("create clone root");
        std::fs::create_dir_all(&outside).expect("create outside");
        std::os::unix::fs::symlink(&outside, root.join("libs")).expect("create mount symlink");

        let error = validate_monorepo_destination(&root, Path::new("libs/vendor"))
            .expect_err("symlinked mount must fail");

        assert!(error.to_string().contains("traverses symlink"));
        assert!(!outside.join("vendor").exists());
    }

    #[test]
    fn atomic_clone_destination_removes_unpublished_staging() {
        let temp = tempfile::TempDir::new().expect("temp");
        let destination = temp.path().join("partial-clone");
        let staging;

        {
            let clone = AtomicCloneDestination::new(&destination).expect("create staging");
            staging = clone.path().to_path_buf();
            std::fs::write(clone.path().join("partial"), b"partial").expect("write staging");
        }

        assert!(!destination.exists());
        assert!(!staging.exists());
    }

    #[test]
    fn atomic_clone_destination_publishes_only_complete_staging() {
        let temp = tempfile::TempDir::new().expect("temp");
        let destination = temp.path().join("successful-clone");
        let clone = AtomicCloneDestination::new(&destination).expect("create staging");
        std::fs::write(clone.path().join("complete"), b"complete").expect("write staging");

        assert!(!destination.exists());
        clone.publish().expect("publish clone");

        assert_eq!(
            std::fs::read(destination.join("complete")).expect("read published file"),
            b"complete"
        );
    }

    #[test]
    fn atomic_clone_destination_never_replaces_a_late_destination() {
        let temp = tempfile::TempDir::new().expect("temp");
        let destination = temp.path().join("contended-clone");
        let clone = AtomicCloneDestination::new(&destination).expect("create staging");
        std::fs::write(clone.path().join("clone"), b"clone").expect("write staging");
        std::fs::create_dir(&destination).expect("create contending destination");
        std::fs::write(destination.join("owner"), b"owner").expect("write owner marker");

        clone
            .publish()
            .expect_err("publication must not replace a destination that appeared mid-clone");

        assert_eq!(
            std::fs::read(destination.join("owner")).expect("read owner marker"),
            b"owner"
        );
        assert!(!destination.join("clone").exists());
    }

    #[test]
    fn git_overlay_insecure_refusal_precedes_destination_staging() {
        let options = CloneOptions {
            thread: None,
            depth: None,
            lazy: false,
            filter: None,
            insecure: true,
        };

        let error = reject_unsupported_for_git_overlay(&options)
            .expect_err("Git-overlay --insecure must fail closed");
        assert!(error.to_string().contains("--insecure is not supported"));
    }

    #[test]
    fn git_clone_progress_tracks_sley_transfer_events() {
        let progress = Progress::null();
        progress.set_phase("streaming Git objects");
        let mut clone_progress = GitCloneProgress {
            progress: progress.clone(),
            received_bytes: 0,
            received_objects: 0,
        };

        clone_progress.transfer(TransferProgress {
            received_bytes: 1024,
            received_objects: 3,
            total_objects: Some(8),
            indexed_deltas: 0,
        });
        clone_progress.transfer(TransferProgress {
            received_bytes: 4096,
            received_objects: 5,
            total_objects: Some(8),
            indexed_deltas: 1,
        });

        assert_eq!(clone_progress.received_objects, 5);
        assert_eq!(clone_progress.received_bytes, 4096);
        assert_eq!(progress.done(), 5);
        assert_eq!(progress.total(), 8);
        assert_eq!(progress.phase(), "streaming Git objects");
        clone_progress.message("remote: counting objects");
        assert_eq!(progress.phase(), "streaming Git objects");
    }

    #[test]
    fn transfer_byte_formatter_uses_binary_units() {
        assert_eq!(format_transfer_bytes(42), "42 B");
        assert_eq!(format_transfer_bytes(1536), "1.5 KiB");
        assert_eq!(format_transfer_bytes(3 * 1024 * 1024), "3.0 MiB");
    }

    /// Standalone helpers to exercise [`GitOverlayBlobHydrator`]'s
    /// error and fallback branches that the kernel/hermetic end-to-end
    /// test (in `tests/lazy_blob_hydration_kernel.rs`) doesn't reach.
    /// Each test sets up the smallest possible bare Git repo it needs;
    /// none of them hit the network.
    mod git_overlay_hydrator {
        use objects::object::ContentHash;
        use repo::{BlobHydrator, Repository};
        use tempfile::TempDir;

        use super::*;

        /// Build a fresh empty bare Git repo and a fresh `Repository`,
        /// returning `(temp, bare_path, repo)` for use in a single test.
        fn fixtures() -> (TempDir, std::path::PathBuf, Repository) {
            let temp = TempDir::new().expect("temp");
            let bare = temp.path().join("source.git");
            SleyRepository::init_bare(&bare).expect("init bare git repo");
            let heddle_root = temp.path().join("heddle");
            std::fs::create_dir_all(&heddle_root).expect("mkdir heddle");
            let repo =
                Repository::init_default(&heddle_root).expect("init heddle repo for hydrator");
            (temp, bare, repo)
        }

        /// Write a single blob into the bare repo and return its OID.
        fn write_local_blob(bare: &std::path::Path, payload: &[u8]) -> ObjectId {
            let git = SleyRepository::open(bare).expect("open bare");
            git.write_blob(payload).expect("write blob")
        }

        #[test]
        fn hydrate_errors_descriptively_when_blob_oid_mapping_is_missing() {
            let (_temp, bare, repo) = fixtures();
            let hydrator = GitOverlayBlobHydrator::new(bare);
            let blake3 = objects::object::Blob::new(b"unknown".to_vec()).hash();

            let err = hydrator
                .hydrate(&repo, &blake3)
                .expect_err("missing mapping must be an error");
            let msg = err.to_string();
            assert!(
                msg.contains("no Git OID mapping"),
                "error message must explain why the mapping is missing: {msg}"
            );
            assert!(
                msg.contains(&blake3.to_hex()),
                "error message must name the blake3 the caller asked for: {msg}"
            );
        }

        #[test]
        fn hydrate_rejects_corrupted_mapping_via_blake3_check() {
            // Mapping points at an OID whose bytes don't match the
            // requested blake3 — the hydrator must NOT silently
            // deliver the wrong content. (Defends against a stale or
            // mis-imported sidecar mapping.)
            let (_temp, bare, repo) = fixtures();
            let real_bytes = b"genuine content".to_vec();
            let oid = write_local_blob(&bare, &real_bytes);

            let lying_blake3 = objects::object::Blob::new(b"different content".to_vec()).hash();
            let hydrator = GitOverlayBlobHydrator::new(bare);
            hydrator.record_blob_oid(lying_blake3, oid);

            let err = hydrator
                .hydrate(&repo, &lying_blake3)
                .expect_err("corrupted mapping must be rejected");
            assert!(
                matches!(err, objects::error::HeddleError::Corruption { .. }),
                "expected Corruption, got: {err:?}"
            );
        }

        #[test]
        fn read_blob_bytes_local_first_path_succeeds() {
            // Direct test of the local-first branch in
            // `read_blob_bytes` — independent of the trait hydrate
            // wrapper so the branch is reachable even if the trait
            // surface evolves.
            let (_temp, bare, _repo) = fixtures();
            let payload = b"local first".to_vec();
            let oid = write_local_blob(&bare, &payload);

            let hydrator = GitOverlayBlobHydrator::new(bare);
            let bytes = hydrator
                .read_blob_bytes(oid)
                .expect("local-first lookup must succeed");
            assert_eq!(bytes, payload);
        }

        #[test]
        fn read_blob_bytes_missing_blob_reports_native_lazy_boundary() {
            // No blob in the bare repo for this OID. Heddle must not
            // shell out to `git cat-file`; the error should name the
            // missing OID and the native lazy-hydration boundary.
            let (_temp, bare, _repo) = fixtures();
            let absent_oid = ObjectId::null(sley::ObjectFormat::Sha1);
            let hydrator = GitOverlayBlobHydrator::new(bare.clone());

            let err = hydrator
                .read_blob_bytes(absent_oid)
                .expect_err("missing blob + no promisor must fail");
            let msg = err.to_string();
            assert!(
                msg.contains("native Git-overlay lazy hydration is not implemented yet"),
                "error must name the native unsupported boundary: {msg}"
            );
            assert!(
                msg.contains(&absent_oid.to_string()),
                "error must include the OID we asked for: {msg}"
            );
            assert!(
                msg.contains(&bare.display().to_string()),
                "error must include the bare-repo path: {msg}"
            );
        }

        #[test]
        fn record_blob_oid_is_last_write_wins_for_a_given_blake3() {
            // The importer may revisit a blake3 (e.g. when an
            // ancestry walk hits the same blob via two trees);
            // `record_blob_oid` is documented as a side-table insert,
            // not a checked-insert, so the second write is the value
            // any subsequent hydrate sees. Pin that behaviour so
            // future tightening to checked-insert doesn't silently
            // change semantics under existing callers.
            let (_temp, bare, _repo) = fixtures();
            let bytes_a = b"first".to_vec();
            let bytes_b = b"second".to_vec();
            let oid_a = write_local_blob(&bare, &bytes_a);
            let oid_b = write_local_blob(&bare, &bytes_b);
            // Two different blob bodies, but we deliberately pin both
            // OIDs to the SAME blake3 (the blake3 of bytes_b) so the
            // hydrate call ends up reading whichever OID is currently
            // recorded for that blake3 — that's what the test is about.
            let blake3 =
                ContentHash::from_hex(&objects::object::Blob::new(bytes_b.clone()).hash().to_hex())
                    .unwrap();

            let hydrator = GitOverlayBlobHydrator::new(bare.clone());
            hydrator.record_blob_oid(blake3, oid_a);
            hydrator.record_blob_oid(blake3, oid_b);

            // The current stored mapping is oid_b → so read_blob_bytes
            // should return bytes_b.
            let bytes = hydrator.read_blob_bytes(oid_b).expect("read");
            assert_eq!(bytes, bytes_b);
            // Independent sanity check via the original oid_a path.
            let bytes_a_read = hydrator.read_blob_bytes(oid_a).expect("read a");
            assert_eq!(bytes_a_read, bytes_a);
        }
    }

    // ── Pure helper / advice coverage (coverage recovery for dead-code
    //    sweep PR). These surfaces are genuinely reachable; they were just
    //    never driven by unit tests, so the gate saw them as dead weight.
    #[test]
    fn clone_repo_name_from_label_strips_git_suffix_and_path_segments() {
        assert_eq!(clone_repo_name_from_label("acme/widgets.git"), "widgets");
        assert_eq!(clone_repo_name_from_label("acme/widgets/"), "widgets");
        assert_eq!(clone_repo_name_from_label("widgets"), "widgets");
        assert_eq!(
            clone_repo_name_from_label("git@github.com:acme/widgets.git"),
            "widgets"
        );
        assert_eq!(
            clone_repo_name_from_label("ssh://git@host/acme/widgets.git"),
            "widgets"
        );
        // Windows drive paths must not be split on the colon.
        assert_eq!(clone_repo_name_from_label(r"C:\src\widgets.git"), "widgets");
        assert_eq!(clone_repo_name_from_label("C:/src/widgets.git"), "widgets");
        // Local paths with a colon in a non-host position keep the full tail.
        assert_eq!(
            clone_repo_name_from_label("/tmp/weird:name/repo.git"),
            "repo"
        );
        assert_eq!(clone_repo_name_from_label("///"), "///");
    }

    #[test]
    fn format_clone_completion_lines_has_three_guidance_lines() {
        let lines = format_clone_completion_lines("widgets", 12, "main");
        assert_eq!(lines.len(), 3);
        assert!(
            lines[0].contains("widgets") && lines[0].contains("12"),
            "first line should name the repo and commit count: {}",
            lines[0]
        );
        assert!(
            lines[1].contains("main"),
            "second line should name the current thread: {}",
            lines[1]
        );
        assert!(
            lines[2].contains("heddle status"),
            "third line should hint the next step: {}",
            lines[2]
        );
    }

    #[test]
    fn clone_dirty_paths_sorts_and_dedups_status_entries() {
        use std::path::PathBuf;

        use objects::worktree::WorktreeStatus;

        let status = WorktreeStatus {
            modified: vec![PathBuf::from("b.txt"), PathBuf::from("a.txt")],
            added: vec![PathBuf::from("a.txt"), PathBuf::from("c.txt")],
            deleted: vec![PathBuf::from("z.txt")],
        };
        assert_eq!(
            clone_dirty_paths(&status),
            vec![
                "a.txt".to_string(),
                "b.txt".to_string(),
                "c.txt".to_string(),
                "z.txt".to_string(),
            ]
        );
        assert!(
            clone_dirty_paths(&WorktreeStatus {
                modified: vec![],
                added: vec![],
                deleted: vec![],
            })
            .is_empty()
        );
    }

    #[test]
    fn clone_advice_builders_carry_stable_kinds_and_primary_commands() {
        let advice = clone_invalid_remote_url_advice("not a url");
        assert_eq!(advice.kind, "clone_invalid_remote_url");
        assert!(advice.error.contains("not a url"));
        assert_eq!(advice.primary_command, "heddle clone <remote> <path>");

        let advice = clone_destination_exists_advice("/tmp/exists");
        assert_eq!(advice.kind, "clone_destination_exists");
        assert!(advice.error.contains("/tmp/exists"));

        let advice = git_overlay_clone_insecure_advice();
        assert_eq!(advice.kind, "git_overlay_clone_insecure_unsupported");
        assert!(advice.error.contains("--insecure"));

        let depth = unsupported_git_overlay_clone_option_advice("--depth", Some("1"));
        assert_eq!(depth.kind, "git_overlay_clone_option_unsupported");
        assert!(depth.error.contains("--depth 1"));
        assert!(depth.error.contains("shallow boundary"));

        let lazy = unsupported_git_overlay_clone_option_advice("--lazy", None);
        assert!(lazy.error.contains("--lazy"));
        assert!(lazy.error.contains("all blobs locally"));

        let advice = clone_verification_failed_advice(
            "index dirty",
            "worktree drifted",
            "would overwrite",
            "heddle status",
        );
        assert_eq!(advice.kind, "clone_verification_failed");
        assert_eq!(advice.primary_command, "heddle status");
        assert!(advice.hint.contains("heddle status"));

        let advice = clone_git_overlay_import_failed_advice(
            Some("feature"),
            "file:///tmp/src",
            "import blew up".into(),
        );
        assert_eq!(advice.kind, "git_overlay_clone_import_failed");
        assert!(advice.error.contains("import blew up"));
        assert!(advice.error.contains("feature"));

        let advice = clone_git_overlay_import_failed_advice(None, "file:///tmp/src", "boom".into());
        assert_eq!(advice.kind, "git_overlay_clone_import_failed");
        assert!(!advice.error.contains("requested ref"));

        let advice = clone_git_overlay_branch_not_imported_advice("feature", "file:///tmp/src");
        assert_eq!(advice.kind, "git_overlay_clone_branch_not_imported");
        assert!(advice.error.contains("feature"));

        let advice = clone_git_overlay_no_branch_refs_advice("file:///tmp/src");
        assert_eq!(advice.kind, "git_overlay_clone_no_branch_refs");
        assert!(advice.unsafe_condition.contains("file:///tmp/src"));

        let filter = local_clone_option_unsupported_advice("--filter", "blob:none");
        assert_eq!(filter.kind, "local_clone_option_unsupported");
        assert!(filter.error.contains("--filter blob:none"));
        let lazy = local_clone_option_unsupported_advice("--lazy", "");
        assert!(lazy.error.contains("--lazy"));

        let advice = clone_default_remote_failed_advice("heddle://host/repo", "disk full".into());
        assert_eq!(advice.kind, "clone_default_remote_failed");
        assert!(advice.error.contains("disk full"));
        assert_eq!(advice.primary_command, "heddle remote add origin <url>");

        let remote = std::path::Path::new("/tmp/missing-remote");
        let advice = clone_remote_not_found_advice(remote);
        assert_eq!(advice.kind, "clone_remote_not_found");
        assert!(advice.error.contains("missing-remote"));

        let advice = clone_remote_thread_not_found_advice("feature/x", remote);
        assert_eq!(advice.kind, "clone_remote_thread_not_found");
        assert!(advice.error.contains("feature/x"));
        assert_eq!(advice.primary_command, "heddle thread list");

        let advice = clone_checkout_not_attached_advice("main");
        assert_eq!(advice.kind, "clone_checkout_not_attached");
        assert!(advice.error.contains("main"));
        assert_eq!(advice.primary_command, "heddle clone --thread main");

        let advice = clone_source_head_unreadable_advice("corrupt HEAD");
        assert_eq!(advice.kind, "clone_source_head_unreadable");
        assert!(advice.error.contains("corrupt HEAD"));
        assert_eq!(advice.primary_command, "heddle status");

        let advice = monorepo_requires_hosted_remote_advice("file:///tmp/x");
        assert_eq!(advice.kind, "monorepo_requires_hosted_remote");
        assert!(advice.error.contains("file:///tmp/x"));
    }

    #[test]
    fn reject_unsupported_for_git_overlay_blocks_insecure_and_lazy() {
        let insecure = CloneOptions {
            thread: None,
            depth: None,
            lazy: false,
            filter: None,
            insecure: true,
        };
        let err = reject_unsupported_for_git_overlay(&insecure).unwrap_err();
        assert!(
            err.to_string().contains("insecure") || format!("{err:#}").contains("insecure"),
            "insecure should be refused: {err}"
        );

        let lazy = CloneOptions {
            thread: None,
            depth: None,
            lazy: true,
            filter: None,
            insecure: false,
        };
        let err = reject_unsupported_for_git_overlay(&lazy).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("lazy")
                || msg.contains("option")
                || msg.contains("filter")
                || msg.contains("depth")
                || msg.contains("supported"),
            "lazy should be refused for git-overlay: {msg}"
        );

        let ok = CloneOptions {
            thread: None,
            depth: None,
            lazy: false,
            filter: None,
            insecure: false,
        };
        reject_unsupported_for_git_overlay(&ok).expect("plain options ok");
    }

    #[test]
    fn reject_unsupported_for_monorepo_blocks_depth_lazy_filter() {
        let depth = CloneOptions {
            thread: None,
            depth: Some(1),
            lazy: false,
            filter: None,
            insecure: false,
        };
        assert!(reject_unsupported_for_monorepo(&depth).is_err());

        let lazy = CloneOptions {
            thread: None,
            depth: None,
            lazy: true,
            filter: None,
            insecure: false,
        };
        assert!(reject_unsupported_for_monorepo(&lazy).is_err());

        let filter = CloneOptions {
            thread: None,
            depth: None,
            lazy: false,
            filter: Some("blob:none".into()),
            insecure: false,
        };
        assert!(reject_unsupported_for_monorepo(&filter).is_err());

        let ok = CloneOptions {
            thread: None,
            depth: None,
            lazy: false,
            filter: None,
            insecure: false,
        };
        reject_unsupported_for_monorepo(&ok).expect("plain monorepo options ok");
    }

    #[test]
    fn atomic_clone_destination_stages_then_publishes() {
        let temp = tempfile::TempDir::new().expect("temp");
        let dest = temp.path().join("repo");
        let atomic = AtomicCloneDestination::new(&dest).expect("stage");
        assert!(atomic.path().exists());
        assert_ne!(atomic.path(), dest.as_path());
        // Write a marker into staging so publish has something to move.
        std::fs::write(atomic.path().join("marker"), b"ok").unwrap();
        atomic.publish().expect("publish");
        assert!(dest.join("marker").is_file());
        assert_eq!(std::fs::read_to_string(dest.join("marker")).unwrap(), "ok");
    }

    #[test]
    fn atomic_clone_destination_cleans_unpublished_staging_on_drop() {
        let temp = tempfile::TempDir::new().expect("temp");
        let dest = temp.path().join("repo");
        let staging = {
            let atomic = AtomicCloneDestination::new(&dest).expect("stage");
            let staging = atomic.path().to_path_buf();
            std::fs::write(staging.join("leftover"), b"x").unwrap();
            assert!(staging.exists());
            staging
            // drop without publish
        };
        assert!(
            !staging.exists(),
            "unpublished staging must be removed on drop"
        );
        assert!(!dest.exists());
    }

    #[test]
    fn select_clone_thread_priority_order() {
        use objects::object::ThreadName;

        let temp = tempfile::TempDir::new().expect("temp");
        let repo = Repository::init_default(temp.path()).expect("init");
        // Snapshot once so we have a real state tip to point threads at.
        std::fs::write(temp.path().join("seed.txt"), b"seed").unwrap();
        let state = repo
            .snapshot(Some("seed".into()), None)
            .expect("snapshot seed state");
        let state_id = state.state_id;
        for name in ["alpha", "main", "zeta"] {
            repo.set_thread_recorded(&ThreadName::from(name), &state_id)
                .expect("create thread tip");
        }

        assert_eq!(
            select_clone_thread(&repo, Some("alpha"), Some("zeta"), "remote").unwrap(),
            "alpha"
        );
        // Advertised HEAD wins over alphabetical when present.
        assert_eq!(
            select_clone_thread(&repo, None, Some("zeta"), "remote").unwrap(),
            "zeta"
        );
        // Prefer main when no hint.
        assert_eq!(
            select_clone_thread(&repo, None, None, "remote").unwrap(),
            "main"
        );
        let err = select_clone_thread(&repo, Some("missing"), Some("alpha"), "remote")
            .expect_err("unknown --thread must fail closed");
        assert!(
            err.to_string().contains("missing"),
            "unknown thread must be named: {err:#}"
        );

        // A second repo still has main from init/snapshot; selection stays on main.
        let temp2 = tempfile::TempDir::new().expect("temp2");
        let repo2 = Repository::init_default(temp2.path()).expect("init2");
        std::fs::write(temp2.path().join("seed.txt"), b"seed").unwrap();
        let state2 = repo2.snapshot(Some("seed".into()), None).expect("snapshot");
        for name in ["zeta", "alpha"] {
            repo2
                .set_thread_recorded(&ThreadName::from(name), &state2.state_id)
                .expect("thread tip");
        }
        assert_eq!(
            select_clone_thread(&repo2, None, None, "remote").unwrap(),
            "main"
        );
    }

    #[test]
    fn hosted_clone_origin_url_joins_endpoint_and_repo_path() {
        assert_eq!(
            hosted_clone_origin_url("weft.local:8421", "acme/widgets"),
            "heddle://weft.local:8421/acme/widgets"
        );
    }

    #[test]
    fn validate_monorepo_destination_rejects_escape_and_accepts_nested() {
        let temp = tempfile::TempDir::new().expect("temp");
        let root = temp.path().join("clone-root");
        std::fs::create_dir_all(&root).unwrap();

        let nested = validate_monorepo_destination(&root, Path::new("services/api")).unwrap();
        assert_eq!(nested, root.join("services/api"));

        let escape = validate_monorepo_destination(&root, Path::new("../outside"));
        assert!(escape.is_err(), "parent escape must be refused");

        let abs = validate_monorepo_destination(&root, Path::new("/etc/passwd"));
        assert!(abs.is_err(), "absolute path must be refused");
    }

    #[test]
    fn advertised_clone_source_lane_fails_closed_on_unreadable_head() {
        let temp = tempfile::TempDir::new().expect("temp");
        let repo = Repository::init_default(temp.path()).expect("init");
        std::fs::write(repo.heddle_dir().join("HEAD"), b"not-a-head\n").expect("corrupt HEAD");
        let err = advertised_clone_source_lane(&repo)
            .expect_err("unreadable source HEAD must fail closed");
        assert!(
            format!("{err:#}").contains("HEAD") || format!("{err:#}").contains("head"),
            "refusal must mention HEAD: {err:#}"
        );
    }

    #[test]
    fn checkout_clone_thread_attaches_without_recording_goto() {
        use objects::object::ThreadName;
        use oplog::{OpLogBackend, OpRecord};
        use refs::Head;

        let temp = tempfile::TempDir::new().expect("temp");
        let repo = Repository::init_default(temp.path()).expect("init");
        std::fs::write(temp.path().join("README.md"), b"captured\n").unwrap();
        let state = repo
            .snapshot(Some("seed".into()), None)
            .expect("snapshot")
            .state_id;
        repo.set_thread_recorded(&ThreadName::from("feature"), &state)
            .expect("feature thread");

        checkout_clone_thread(&repo, "feature", &state).expect("checkout");

        assert_eq!(
            repo.current_lane().expect("current lane").as_deref(),
            Some("feature")
        );
        assert!(
            matches!(
                repo.head_ref().expect("HEAD"),
                Head::Attached { thread } if thread.as_str() == "feature"
            ),
            "HEAD must be attached, not detached"
        );
        let recorded_goto = repo
            .oplog()
            .recent(200)
            .expect("oplog")
            .into_iter()
            .any(|entry| matches!(entry.operation, OpRecord::Goto { .. }));
        assert!(
            !recorded_goto,
            "clone checkout must not record a Goto that republishes detached HEAD"
        );
    }

    #[cfg(feature = "client")]
    #[test]
    fn hosted_unknown_thread_does_not_create_destination() {
        let temp = tempfile::TempDir::new().expect("temp");
        let dest = temp.path().join("clone");
        let intent = CloneIntent {
            origin: "heddle://127.0.0.1:8421/owner/repo".to_string(),
            endpoint: "127.0.0.1:8421".to_string(),
            repository: "owner/repo".to_string(),
            thread: Some("missing".to_string()),
            advertised_head: Some("main".to_string()),
            depth: None,
            lazy: false,
        };
        let err = create_hosted_clone_intent_after_thread_select(&dest, intent, ["main"])
            .expect_err("unknown --thread must fail closed");
        assert!(
            format!("{err:#}").contains("missing"),
            "refusal must name the missing thread: {err:#}"
        );
        assert!(
            !dest.exists(),
            "unknown --thread must not create the destination"
        );
        assert!(
            !CloneIntent::path(&dest).exists(),
            "unknown --thread must not persist a clone intent"
        );
    }

    #[cfg(feature = "client")]
    #[test]
    fn recover_keeps_non_main_advertised_default() {
        let intent = CloneIntent {
            origin: "heddle://127.0.0.1:8421/owner/repo".to_string(),
            endpoint: "127.0.0.1:8421".to_string(),
            repository: "owner/repo".to_string(),
            thread: None,
            advertised_head: Some("trunk".to_string()),
            depth: None,
            lazy: false,
        };
        let selected = select_recover_clone_thread(&intent, ["alpha", "main", "trunk"])
            .expect("recover must keep the advertised default");
        assert_eq!(
            selected, "trunk",
            "no-flag recover must not fall through to main"
        );
    }
}
