// SPDX-License-Identifier: Apache-2.0
//! Clone command - clone from remote.

#[cfg(feature = "client")]
use std::time::{SystemTime, UNIX_EPOCH};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::Mutex,
};

#[cfg(feature = "client")]
use anyhow::Context;
use anyhow::{Result, anyhow};
#[cfg(feature = "client")]
use heddle_client::grpc_hosted::{HostedRefEntry, PullMaterialization};
use heddle_core::{
    CloneMode, ClonePlanError, ClonePlanFacts, ClonePlanOptions, CloneRemoteSource,
    UnsupportedCloneFlag, looks_like_git_overlay_url, plan_clone,
};
#[cfg(feature = "client")]
use heddle_core::{
    MonorepoCloneResultSummary, MonorepoEdgeFacts, MonorepoEdgeSkipReason, MonorepoNodeExecution,
    MonorepoNodeExecutionStep, MonorepoNodeFacts, MonorepoNodeStepOptions,
    assemble_monorepo_clone_json_report, assemble_monorepo_clone_result_summary,
    monorepo_execution_progress, monorepo_rel_display, plan_monorepo_clone,
    plan_monorepo_execution, validate_monorepo_clone_options, validate_monorepo_execution,
};
use objects::{
    error::{HeddleError, Result as HeddleResult},
    object::{Blob, ContentHash, ThreadName},
    store::ObjectStore,
    sync::LockExt,
};
use refs::Head;
use repo::{BlobHydrator, Repository};
use serde::Serialize;
#[cfg(feature = "client")]
use sley::plumbing::sley_worktree;
use sley::{GitObjectType, ObjectId};
#[cfg(any(feature = "client", test))]
use sley::Repository as SleyRepository;

use super::{
    advice::RecoveryAdvice,
    verification_health::{RepositoryVerificationState, build_repository_verification_state},
};
#[cfg(feature = "client")]
use crate::remote::credential_key_from_remote_url;
use crate::{
    cli::{Cli, should_output_json, style},
    client::LocalSync,
    git_projection_engine::git_core::open_repo,
    remote::{Remote, RemoteConfig, RemoteTarget},
};

/// `output_kind` value carried by the final `heddle clone --output json`
/// payload. Referenced by the command catalog and the catalog/runtime
/// invariant test to keep the runtime emission and the advertised
/// discriminator from drifting apart.
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

#[derive(Serialize)]
struct CloneOutput {
    output_kind: &'static str,
    action: &'static str,
    status: &'static str,
    success: bool,
    cloned: bool,
    transport: &'static str,
    remote: String,
    local: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    branch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    repository_capability: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    commits_imported: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    states_created: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    objects: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    state: Option<String>,
    #[serde(rename = "verification")]
    #[serde(skip_serializing_if = "Option::is_none")]
    trust: Option<RepositoryVerificationState>,
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

    let direct_git_source = matches!(
        &remote_source,
        CloneRemoteSource::Local {
            has_heddle: false,
            is_git: true,
            ..
        }
    ) || looks_like_git_overlay_url(&remote);
    if direct_git_source {
        return Err(anyhow!(direct_git_clone_advice(
            &remote,
            &local,
            thread.as_deref(),
            depth,
            lazy,
            filter.as_deref(),
            recursive,
            insecure,
        )));
    }

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
        CloneMode::LocalGitOverlay { .. } | CloneMode::GitOverlayUrl => {
            unreachable!("Git clone sources are refused before Heddle clone planning")
        }
        CloneMode::NetworkHosted { recursive } => {
            let (addr, repo_path) = match parse_result {
                Ok(RemoteTarget::Network { addr, repo_path }) => (addr, repo_path),
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
                        addr,
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
                        addr,
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
                let _ = (addr, repo_path, recursive, &plan.security);
                return Err(anyhow!(network_clone_unavailable_advice()));
            }
        }
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn direct_git_clone_advice(
    remote: &str,
    local: &str,
    branch: Option<&str>,
    depth: Option<u32>,
    lazy: bool,
    filter: Option<&str>,
    recursive: bool,
    insecure: bool,
) -> RecoveryAdvice {
    let mut argv = vec!["git".to_string()];
    if insecure {
        argv.extend(["-c".into(), "http.sslVerify=false".into()]);
    }
    argv.push("clone".into());
    if recursive {
        argv.push("--recurse-submodules".into());
    }
    if lazy {
        argv.push("--filter=blob:none".into());
    }
    if let Some(filter) = filter {
        argv.extend(["--filter".into(), filter.into()]);
    }
    if let Some(depth) = depth.filter(|depth| *depth > 0) {
        argv.extend(["--depth".into(), depth.to_string()]);
    }
    if let Some(branch) = branch {
        argv.extend(["--branch".into(), branch.into()]);
    }
    argv.extend([remote.into(), local.into()]);
    let direct_git = super::command_catalog::checked_action_from_argv(argv);
    RecoveryAdvice::safety_refusal(
        "source_authority_direct_git",
        "`heddle clone` accepts Heddle repositories only",
        format!("Run `{direct_git}` directly for this Git repository."),
        "the clone source is a Git repository without Heddle-native history",
        "routing Git-owned source history through Heddle would make clone authority ambiguous",
        "the destination directory and repository metadata were left unchanged",
        direct_git.clone(),
        vec![direct_git],
    )
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
            _ => unreachable!("Git sources are refused before clone planning"),
        },
    }
}

fn clone_invalid_remote_url_advice(remote: &str) -> RecoveryAdvice {
    RecoveryAdvice::safety_refusal(
        "clone_invalid_remote_url",
        format!("Invalid remote URL: {remote}"),
        "Use an existing Heddle repository path or a hosted Heddle remote.",
        format!("remote '{remote}' could not be parsed as a supported Heddle remote"),
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
    let track_name = thread.as_deref().unwrap_or("main");
    let tn = ThreadName::new(track_name);
    let state_id = remote_repo
        .refs()
        .get_thread(&tn)?
        .ok_or_else(|| clone_remote_thread_not_found_advice(track_name, remote_path))?;

    // Create and initialize the local repository only after all
    // preflight target selection has succeeded.
    fs::create_dir_all(local_path)?;
    let local_repo = Repository::init(local_path)?;

    // Fetch the state and dependencies
    let objects_copied = if let Some(d) = depth {
        sync.fetch_state_with_depth(&local_repo, &state_id, d)?
    } else {
        sync.fetch_state(&local_repo, &state_id)?
    };

    // Materialize from a fresh clone baseline before publishing the local
    // thread ref. Otherwise HEAD can resolve to the target first and make
    // the empty worktree look like deleted target files.
    local_repo.goto_from_materialized_state(&state_id, None)?;
    // Set up the thread locally after materialization so the dirty-worktree
    // guard does not mistake an empty fresh clone for deleted target files.
    local_repo.refs().set_thread(&tn, &state_id)?;
    local_repo.refs().write_head(&Head::Attached {
        thread: ThreadName::new(track_name),
    })?;

    // Copy worktree files from ordinary local remotes. A Heddle repo may
    // also live inside a bare Git directory used as a local remote; that
    // directory has no project worktree, only Git administrative files
    // such as HEAD/config/hooks/objects/refs. The fetched Heddle state
    // above has already materialized the real project files, so copying
    // the bare Git root would pollute the clone.
    if !looks_like_bare_git_admin_root(remote_repo.root()) {
        copy_worktree(remote_repo.root(), local_repo.root())?;
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
        crate::cli::render::write_json_stdout(&output)?;
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

fn looks_like_bare_git_admin_root(path: &Path) -> bool {
    !path.join(".git").exists()
        && path.join("HEAD").is_file()
        && path.join("objects").is_dir()
        && path.join("refs").is_dir()
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
/// hydrator config can persist it instead of the post-DNS `SocketAddr`.
/// Keeping the hostname matters when the upstream service rotates IPs
/// (e.g. behind a load balancer): a SocketAddr baked into the marker at
/// clone time would pin to a stale IP and break later hydrate calls even
/// though the original URL still resolves. The hydrator re-resolves DNS
/// on every process start when given a hostname spec.
#[cfg(feature = "client")]
fn hosted_endpoint_spec(remote: &str) -> String {
    let trimmed = remote.strip_prefix("heddle://").unwrap_or(remote);
    // The address ends at the first slash that introduces a repo path.
    trimmed.split('/').next().unwrap_or(trimmed).to_string()
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
    addr: std::net::SocketAddr,
    repo_path: Option<&str>,
    local_path: &Path,
    options: &CloneOptions,
    server_key: Option<String>,
    endpoint_spec: String,
) -> Result<()> {
    use crate::{
        client::{HostedAuthMode, HostedSession},
        config::UserConfig,
    };

    let CloneOptions {
        thread,
        depth,
        lazy,
        filter,
        insecure,
    } = options;
    let depth = *depth;
    // `--filter blob:none` is a synonym for `--lazy` on hosted/network
    // remotes; both produce a clone whose blob content is hydrated on demand.
    let lazy = *lazy || filter.is_some();

    let user_config = UserConfig::load_default()?;
    // On every network-connecting command, TLS/auth config validation
    // (`heddle_client_config`) must succeed before any irreversible
    // filesystem/repo mutation such as `create_dir_all`, `Repository::init`,
    // state writes, or ref publishes. A rejected security config must leave
    // no partial on-disk artifact.
    let session =
        HostedSession::build(&user_config, server_key, HostedAuthMode::CredentialFallback)?
            .with_allow_insecure(*insecure);
    let repo_path = repo_path.context("network remotes must include a hosted repository path")?;

    let json_output = should_output_json(cli, None);
    let mut client = session.connect(addr).await?;

    if json_output {
        println!(
            "{}",
            serde_json::json!({
                "output_kind": CLONE_CONNECTION_OUTPUT_KIND,
                "status": "connected",
                "address": addr.to_string(),
            })
        );
    } else {
        println!("Connected to {}", addr);
    }

    let remote_refs = client
        .list_refs_with_revision_addresses(repo_path)
        .await?
        .into_iter()
        .filter(|entry| entry.is_thread)
        .collect::<Vec<_>>();
    let track_name = select_hosted_clone_thread(
        thread.as_deref(),
        remote_refs.iter().map(|entry| entry.name.as_str()),
        repo_path,
    )?;
    let git_overlay_clone = hosted_clone_thread_revision_address(&remote_refs, &track_name)
        .is_some_and(|address| address.starts_with("git:"));

    let mut cleanup = CloneDestinationCleanup::new(local_path);

    // Create the local directory only after TLS/auth prevalidation and remote
    // ref discovery. Git-backed hosted refs need `.git` present before
    // Repository::init so the destination is opened as Git-overlay and accepts
    // the Git lane transfers during pull.
    fs::create_dir_all(local_path)?;
    if git_overlay_clone {
        SleyRepository::init(local_path).map_err(anyhow::Error::msg)?;
    }

    // Initialize the local repository only after TLS/auth prevalidation.
    let local_repo = Repository::init(local_path)?;
    let origin_url = hosted_clone_origin_url(&endpoint_spec, repo_path);
    if git_overlay_clone {
        configure_git_overlay_origin(local_path, &origin_url)?;
    }
    let materialization = if lazy {
        PullMaterialization::Lazy
    } else {
        PullMaterialization::Full
    };
    let result = client
        .pull_with_depth_and_materialization(
            &local_repo,
            repo_path,
            &track_name,
            Some(&track_name),
            depth,
            materialization,
        )
        .await?;
    if result.success {
        let final_state = result.final_state;
        // Lazy clone: persist the hydrator metadata so future
        // `Repository::open` calls (in any process) can reconstruct
        // the on-read hydrator. Without this, lazy clones would only
        // hydrate inside the single `cmd_clone` process — every
        // subsequent `heddle <verb>` would surface MissingObject on
        // any blob read.
        if lazy {
            use repo::lazy_hydrator::LazyHydratorConfig;
            // Persist the original `host:port` spec (not `addr.to_string()`,
            // which is a resolved IP). The hydrator re-resolves DNS on
            // every process start so a future LB rotation doesn't pin us
            // to a stale IP.
            let cfg = LazyHydratorConfig::hosted(
                endpoint_spec.clone(),
                repo_path,
                &track_name,
                &track_name,
            );
            cfg.save(local_repo.heddle_dir())
                .context("failed to persist lazy-hydrator.toml")?;
        } else if git_overlay_clone {
            finish_hosted_git_overlay_checkout(&local_repo, &track_name)
                .context("failed to finish hosted Git-overlay checkout")?;
            configure_git_overlay_origin_tracking(local_path, &track_name)?;
        } else if let Some(state) = final_state {
            local_repo
                .goto_from_materialized_state(&state, None)
                .context("failed to materialize hosted clone worktree")?;
        }
        configure_hosted_clone_origin(&local_repo, &endpoint_spec, repo_path)?;
        if should_output_json(cli, Some(local_repo.config())) {
            let output = heddle_clone_output(
                origin_url.clone(),
                local_path.display().to_string(),
                track_name.clone(),
                local_repo.capability_label(),
                None,
                final_state.map(|state| state.to_string()),
                Some(build_repository_verification_state(&local_repo)),
            );
            crate::cli::render::write_json_stdout(&output)?;
        } else {
            let depth_info = depth.map(|d| format!(" (depth {})", d)).unwrap_or_default();
            println!(
                "{} cloned {} into {}{}",
                style::ok_marker(),
                style::dim(&origin_url),
                style::bold(&local_path.display().to_string()),
                style::dim(&depth_info)
            );
            if let Some(state) = final_state {
                println!(
                    "  {}",
                    style::field("state", &style::state_id(&state.to_string()))
                );
            }
        }
    } else {
        let err = result.error.unwrap_or_else(|| "Unknown error".to_string());
        return Err(anyhow!(RecoveryAdvice::network_clone_failed(
            &err, local_path
        )));
    }

    cleanup.disarm();
    Ok(())
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
/// facts) live in `heddle_core::clone_plan` and are unit-tested there. This
/// function owns hosted RPC and per-node materialize I/O.
#[cfg(feature = "client")]
async fn clone_monorepo(
    cli: &Cli,
    addr: std::net::SocketAddr,
    repo_path: Option<&str>,
    local_path: &Path,
    options: &CloneOptions,
    server_key: Option<String>,
    endpoint_spec: String,
) -> Result<()> {
    use crate::{
        client::{HostedAuthMode, HostedSession},
        config::UserConfig,
    };

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

    let json_output = should_output_json(cli, None);
    let mut client = session.connect(addr).await?;
    if json_output {
        println!(
            "{}",
            serde_json::json!({
                "output_kind": CLONE_CONNECTION_OUTPUT_KIND,
                "status": "connected",
                "address": addr.to_string(),
            })
        );
    } else {
        println!("Connected to {}", addr);
    }

    // Resolve the whole child tree into the caller's coherent visible slice,
    // then pure-plan placement, work order, and per-node steps (no FS yet).
    let resolved = client.resolve_monorepo(root_path, None).await?;
    let facts = monorepo_node_facts_from_resolved(&resolved);
    let clone_plan = plan_monorepo_clone(&facts);
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
        let dest = node_exec.node.dest_path(local_path);
        execute_monorepo_node_steps(
            &mut client,
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
        crate::cli::render::write_json_stdout(&output)?;
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

/// Map a transport `MonorepoNode` tree into pure core facts (no I/O).
///
/// Parses content-state bytes into [`StateId`]; malformed/absent states map
/// to `None` (empty checkout), matching prior client planner policy.
#[cfg(feature = "client")]
fn monorepo_node_facts_from_resolved(node: &grpc::heddle::v1::MonorepoNode) -> MonorepoNodeFacts {
    use objects::object::StateId;

    let content_state = node
        .content_state
        .as_deref()
        .and_then(|bytes| StateId::try_from_slice(bytes).ok());
    let edges = node
        .edges
        .iter()
        .map(|edge| {
            let skip_reason = edge.skipped.and_then(MonorepoEdgeSkipReason::from_wire_i32);
            MonorepoEdgeFacts {
                mount_name: edge.mount_name.clone(),
                child_spool_id: edge.child_spool_id.clone(),
                child: edge
                    .subtree
                    .as_ref()
                    .map(|subtree| monorepo_node_facts_from_resolved(subtree)),
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
    client: &mut heddle_client::grpc_hosted::HostedGrpcClient,
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
                        .with_context(|| format!("failed at {}", progress.label()))?,
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
/// lives in `heddle_core::validate_monorepo_clone_options`; this wrapper maps
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
    remote_label: &str,
) -> Result<String> {
    if let Some(requested) = requested {
        return Ok(requested.to_string());
    }

    let mut threads = remote_threads
        .into_iter()
        .map(str::to_string)
        .collect::<Vec<_>>();
    threads.sort();
    threads.dedup();
    if threads.iter().any(|thread| thread == "main") {
        return Ok("main".to_string());
    }
    threads
        .into_iter()
        .next()
        .ok_or_else(|| anyhow!(clone_git_overlay_no_branch_refs_advice(remote_label)))
}

#[cfg(feature = "client")]
fn hosted_clone_thread_revision_address<'a>(
    remote_refs: &'a [HostedRefEntry],
    thread: &str,
) -> Option<&'a str> {
    remote_refs
        .iter()
        .find(|entry| entry.name == thread && entry.is_thread)
        .map(|entry| entry.revision_address.as_str())
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
        return Err(anyhow!(clone_symlink_unsupported_advice(path, dest_path)));
    } else if path.is_dir() {
        copy_dir_recursive(path, dest_path)?;
    } else {
        fs::copy(path, dest_path)?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn clone_symlink_unsupported_advice(path: &Path, dest_path: &Path) -> RecoveryAdvice {
    RecoveryAdvice::safety_refusal(
        "clone_symlink_unsupported",
        "Symlinks are not supported on this platform",
        "Retry on a platform with symlink support, or remove the symlink from the source before cloning.",
        format!(
            "source path '{}' is a symlink but this platform cannot create symlinks",
            path.display()
        ),
        format!(
            "clone would need to create symlink '{}' to preserve the worktree exactly",
            dest_path.display()
        ),
        "the clone operation stopped before replacing the unsupported symlink with different file contents",
        "heddle clone <remote> <path>",
        vec!["heddle clone <remote> <path>".to_string()],
    )
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
/// `Repository::read_object` is keyed by Git OID. The bridge
/// already computes blake3↔git mappings *for commits* (see
/// `SyncMapping` in `git_projection_engine/git_core.rs`); blob mappings are
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

    fn opts(depth: Option<u32>, lazy: bool, filter: Option<&str>) -> CloneOptions {
        CloneOptions {
            thread: None,
            depth,
            lazy,
            filter: filter.map(str::to_string),
            insecure: false,
        }
    }

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

    #[test]
    fn heddle_clone_output_uses_git_overlay_repository_capability() {
        let temp = tempfile::TempDir::new().expect("temp");
        SleyRepository::init(temp.path()).expect("init git repo");
        let repo = Repository::init(temp.path()).expect("init git-overlay repo");

        let output = heddle_clone_output(
            "heddle://weft.local:8421/org/repo".to_string(),
            temp.path().display().to_string(),
            "main".to_string(),
            repo.capability_label(),
            None,
            None,
            None,
        );

        assert_eq!(repo.capability_label(), "git-overlay");
        assert_eq!(output.repository_capability, Some("git-overlay"));
    }

    #[test]
    fn reject_unsupported_passes_when_no_flags_set() {
        assert!(reject_unsupported_for_git_overlay(&opts(None, false, None)).is_ok());
    }

    #[cfg(feature = "client")]
    #[test]
    fn hosted_clone_thread_selection_prefers_main() {
        let selected = select_hosted_clone_thread(None, ["master", "main"], "owner/repo")
            .expect("thread selected");

        assert_eq!(selected, "main");
    }

    #[cfg(feature = "client")]
    #[test]
    fn hosted_clone_thread_selection_uses_only_advertised_master() {
        let selected =
            select_hosted_clone_thread(None, ["master"], "owner/repo").expect("thread selected");

        assert_eq!(selected, "master");
    }

    #[cfg(feature = "client")]
    #[test]
    fn hosted_clone_thread_selection_honors_requested_thread() {
        let selected =
            select_hosted_clone_thread(Some("feature"), ["main", "master"], "owner/repo")
                .expect("thread selected");

        assert_eq!(selected, "feature");
    }

    // ---- clone text-mode summary helpers (heddle#161) ----

    #[test]
    fn clone_repo_name_strips_https_url_and_dot_git() {
        assert_eq!(
            clone_repo_name_from_label("https://github.com/BurntSushi/ripgrep.git"),
            "ripgrep"
        );
        assert_eq!(
            clone_repo_name_from_label("https://github.com/BurntSushi/ripgrep"),
            "ripgrep"
        );
    }

    #[test]
    fn clone_repo_name_strips_ssh_url_and_dot_git() {
        assert_eq!(
            clone_repo_name_from_label("git@github.com:owner/repo.git"),
            "repo"
        );
    }

    #[test]
    fn clone_repo_name_extracts_last_filesystem_segment() {
        assert_eq!(clone_repo_name_from_label("/home/user/foo"), "foo");
        assert_eq!(
            clone_repo_name_from_label("file:///tmp/projects/bar/"),
            "bar"
        );
    }

    #[test]
    fn clone_repo_name_falls_back_to_label_when_no_segment() {
        // Empty or pathologic input: don't panic, return the input as-is
        // so the rendered summary still carries *something* identifying.
        assert_eq!(clone_repo_name_from_label(""), "");
        assert_eq!(clone_repo_name_from_label("///"), "///");
    }

    #[test]
    fn clone_repo_name_handles_windows_drive_paths() {
        // Windows drive prefixes (`C:\…` and `C:/…`) are not SSH/SCP
        // shorthand — earlier versions unconditionally split on `:` and
        // dropped the drive letter, producing `\src\ripgrep` instead of
        // `ripgrep`.
        assert_eq!(clone_repo_name_from_label("C:\\src\\ripgrep"), "ripgrep");
        assert_eq!(clone_repo_name_from_label("C:/src/ripgrep"), "ripgrep");
        assert_eq!(
            clone_repo_name_from_label("D:\\workspaces\\heddle.git"),
            "heddle"
        );
    }

    #[test]
    fn clone_repo_name_handles_local_paths_with_colon() {
        // Git treats `host:path` as SCP shorthand only when the prefix
        // contains no path separator. `/tmp/foo:bar/repo` and
        // `./foo:bar/repo.git` are valid local paths whose basename
        // must not be shadowed by the colon.
        assert_eq!(clone_repo_name_from_label("/tmp/foo:bar/repo.git"), "repo");
        assert_eq!(clone_repo_name_from_label("./foo:bar/repo.git"), "repo");
    }

    #[test]
    fn format_clone_completion_text_names_repo_and_count_and_thread_and_next_command() {
        // Style helpers are no-ops when color is uninitialized (test
        // default), so substring assertions work on the raw text. Keeps
        // the assertions independent of ANSI escape sequences.
        let lines = format_clone_completion_lines("ripgrep", 2249, "master");
        let joined = lines.join("\n");
        assert!(
            joined.contains("ripgrep"),
            "summary must name the repo: {joined}"
        );
        assert!(
            joined.contains("2249"),
            "summary must include the commit count: {joined}"
        );
        assert!(
            joined.contains("commit"),
            "summary must use the word 'commit': {joined}"
        );
        assert!(
            joined.contains("master"),
            "summary must name the current thread: {joined}"
        );
        assert!(
            joined.to_lowercase().contains("heddle status"),
            "summary must suggest `heddle status` as the next step: {joined}"
        );
    }

    #[test]
    fn format_clone_completion_singularizes_one_commit() {
        // Avoid "1 commits" — the style::count helper already singularizes,
        // but pin it here so a future formatter refactor doesn't regress.
        let lines = format_clone_completion_lines("tiny", 1, "main");
        let joined = lines.join("\n");
        assert!(
            joined.contains("1 commit ")
                || joined.contains("1 commit\n")
                || joined.ends_with("1 commit"),
            "one commit must not pluralize: {joined}"
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
    fn clone_destination_cleanup_removes_armed_destination() {
        let temp = tempfile::TempDir::new().expect("temp");
        let path = temp.path().join("partial-clone");

        {
            let _cleanup = CloneDestinationCleanup::new(&path);
            std::fs::create_dir_all(&path).expect("create partial destination");
        }

        assert!(
            !path.exists(),
            "armed clone cleanup must remove the partial destination"
        );
    }

    #[cfg(feature = "client")]
    #[test]
    fn clone_destination_cleanup_disarm_preserves_destination() {
        let temp = tempfile::TempDir::new().expect("temp");
        let path = temp.path().join("successful-clone");

        {
            let mut cleanup = CloneDestinationCleanup::new(&path);
            std::fs::create_dir_all(&path).expect("create successful destination");
            cleanup.disarm();
        }

        assert!(
            path.exists(),
            "disarmed clone cleanup must preserve the successful destination"
        );
    }

    #[test]
    fn reject_unsupported_rejects_filter() {
        let err = reject_unsupported_for_git_overlay(&opts(None, false, Some("blob:none")))
            .expect_err("filter must be rejected");
        let advice = err
            .chain()
            .find_map(|cause| cause.downcast_ref::<RecoveryAdvice>())
            .expect("filter refusal should use typed recovery advice");
        assert_eq!(advice.kind, "git_overlay_clone_option_unsupported");
        assert_eq!(advice.primary_command, "heddle clone <remote> <path>");
        assert!(advice.preserved.contains("no clone directory"));
        let msg = err.to_string();
        assert!(
            msg.contains("--filter"),
            "message must name --filter: {msg}"
        );
        assert!(
            msg.contains("not yet supported"),
            "message must say not yet supported: {msg}"
        );
    }

    #[test]
    fn reject_unsupported_rejects_lazy() {
        let err = reject_unsupported_for_git_overlay(&opts(None, true, None))
            .expect_err("lazy must be rejected");
        let advice = err
            .chain()
            .find_map(|cause| cause.downcast_ref::<RecoveryAdvice>())
            .expect("lazy refusal should use typed recovery advice");
        assert_eq!(advice.kind, "git_overlay_clone_option_unsupported");
        assert!(advice.primary_hint().contains("full Git-overlay clone"));
        let msg = err.to_string();
        assert!(msg.contains("--lazy"), "message must name --lazy: {msg}");
        assert!(
            msg.contains("not yet supported"),
            "message must say not yet supported: {msg}"
        );
    }

    #[test]
    fn reject_unsupported_rejects_depth() {
        let err = reject_unsupported_for_git_overlay(&opts(Some(1), false, None))
            .expect_err("depth must be rejected");
        let advice = err
            .chain()
            .find_map(|cause| cause.downcast_ref::<RecoveryAdvice>())
            .expect("depth refusal should use typed recovery advice");
        assert_eq!(advice.kind, "git_overlay_clone_option_unsupported");
        assert!(advice.error.contains("shallow boundary"));
        let msg = err.to_string();
        assert!(msg.contains("--depth"), "message must name --depth: {msg}");
        assert!(
            msg.contains("not yet supported"),
            "message must say not yet supported: {msg}"
        );
    }

    #[test]
    fn clone_verification_failures_use_typed_recovery_advice() {
        let advice = clone_verification_failed_advice(
            "clone verification failed: Heddle active thread is 'dev', expected 'main'",
            "Heddle active thread 'dev' does not match imported Git branch 'main'",
            "continuing would report the clone as verified while Heddle is attached to the wrong thread",
            "heddle thread switch main --force",
        );

        assert_eq!(advice.kind, "clone_verification_failed");
        assert!(
            advice
                .error
                .contains("clone verification failed: Heddle active thread")
        );
        assert_eq!(advice.primary_command, "heddle thread switch main --force");
        assert_eq!(
            advice.recovery_commands,
            vec!["heddle thread switch main --force"]
        );
        assert!(advice.preserved.contains("left for inspection"));
        assert!(advice.primary_hint().contains("Repair the clone mapping"));
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
}
