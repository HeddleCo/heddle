// SPDX-License-Identifier: Apache-2.0
//! Fetch command - download objects and refs from remote.

#[cfg(feature = "client")]
use std::collections::HashSet;

#[cfg(feature = "client")]
use anyhow::Context;
use anyhow::Result;
#[cfg(feature = "client")]
use objects::object::ChangeId;
use objects::object::{MarkerName, ThreadName};
#[cfg(feature = "client")]
use objects::store::ObjectStore;
use repo::{Repository, RepositoryCapability};
use serde::Serialize;

use super::{
    advice::RecoveryAdvice, verification_health::build_repository_verification_state,
    remote::resolved_default_remote_name,
};
#[cfg(feature = "client")]
use crate::client::{HostedAuthMode, HostedGrpcClient};
#[cfg(feature = "client")]
use crate::config::UserConfig;
use crate::{
    bridge::GitBridge,
    cli::{Cli, should_output_json, style},
    client::LocalSync,
    remote::{RemoteConfig, RemoteTarget, resolve_remote_with_key},
};

#[derive(Serialize)]
struct FetchOutput {
    output_kind: &'static str,
    remote: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    ref_scope: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tags_included: Option<bool>,
    refs_fetched: usize,
    objects_fetched: usize,
    #[allow(dead_code)]
    #[serde(skip_serializing)]
    #[serde(rename = "verification")]
    trust: super::verification_health::RepositoryVerificationState,
}

pub async fn cmd_fetch(cli: &Cli, remote: Option<String>, all: bool) -> Result<()> {
    let repo = cli.open_repo()?;

    // A git-overlay repo (not a hosted-native repo) fetches through the
    // git-overlay exporter *except* for remotes that resolve to a hosted
    // `heddle://` network endpoint. Those must route through the native
    // hosted-sync path (`fetch_network`), the same way `pull`/`clone` do —
    // the overlay exporter cannot speak the `heddle://` scheme and hard-errors
    // on it. `--all` may mix git and hosted remotes, so classify each remote
    // by its own scheme rather than gating the whole batch on one guard.
    if repo.capability() == RepositoryCapability::GitOverlay && !repo.hosted_enabled() {
        let remotes = if all {
            let configured = all_configured_remotes(&repo)?;
            if configured.is_empty() {
                vec!["origin".to_string()]
            } else {
                configured
            }
        } else {
            let selected = if let Some(remote) = remote.as_ref() {
                remote.clone()
            } else if let Some(default) = resolved_default_remote_name(&repo)? {
                default
            } else {
                return Err(RecoveryAdvice::remote_name_required_for_fetch().into());
            };
            vec![selected]
        };

        // Peel off hosted-network remotes; the rest fetch via the overlay
        // exporter. A repo with a mixed set gets each remote routed by scheme.
        let (hosted_remotes, overlay_remotes): (Vec<String>, Vec<String>) =
            remotes.into_iter().partition(|name| {
                super::remote::push_target_is_hosted_network(&repo, Some(name.as_str()))
            });

        for remote_name in &overlay_remotes {
            let mut bridge = GitBridge::new(&repo);
            bridge.fetch(remote_name)?;
        }

        // If every remote was hosted (or a mix), the hosted ones fall through
        // to the shared network path below. Only short-circuit here when there
        // is nothing hosted left to fetch.
        if hosted_remotes.is_empty() {
            if should_output_json(cli, Some(repo.config())) {
                println!(
                    "{}",
                    serde_json::to_string(&FetchOutput {
                        output_kind: "fetch",
                        remote: if all {
                            "all".to_string()
                        } else {
                            overlay_remotes
                                .first()
                                .cloned()
                                .unwrap_or_else(|| "origin".to_string())
                        },
                        ref_scope: Some("branches_and_heddle_notes"),
                        tags_included: Some(false),
                        refs_fetched: overlay_remotes.len(),
                        objects_fetched: 0,
                        trust: build_repository_verification_state(&repo),
                    })?
                );
            } else {
                println!(
                    "{} fetched branches + refs/notes/heddle from {} (tags skipped)",
                    style::ok_marker(),
                    if all {
                        style::bold("all remotes")
                    } else {
                        style::bold(&overlay_remotes.join(", "))
                    }
                );
            }
            return Ok(());
        }

        // Hosted remotes remain: route them through the network path below.
        // (Any overlay remotes were already fetched above.)
        return fetch_via_network(cli, &repo, hosted_remotes, all).await;
    }

    let remotes = if all {
        all_configured_remotes(&repo)?
    } else if let Some(remote) = remote.as_ref() {
        vec![remote.clone()]
    } else if let Some(default) = resolved_default_remote_name(&repo)? {
        vec![default]
    } else {
        return Err(RecoveryAdvice::remote_name_required_for_fetch().into());
    };

    fetch_via_network(cli, &repo, remotes, all).await
}

/// Enumerate every configured remote for `fetch --all`, unioning the Heddle
/// remote aliases (`.heddle/remotes.toml`) with the refs backend's
/// remote-tracking namespaces. `refs().list_remotes()` alone only surfaces
/// remotes that already have tracking refs, so a freshly-added remote that has
/// never been fetched — e.g. a hosted `heddle://` remote — was silently dropped
/// from `--all` (#839). Ordering is deterministic: Heddle-config remotes first
/// (in config order), then any tracking-only remotes not already listed.
fn all_configured_remotes(repo: &Repository) -> Result<Vec<String>> {
    let mut names = Vec::new();
    let mut seen = std::collections::HashSet::new();

    if let Ok(cfg) = RemoteConfig::open(repo) {
        for (name, _) in cfg.list() {
            if seen.insert(name.clone()) {
                names.push(name);
            }
        }
    }

    for name in repo.refs().list_remotes()? {
        if seen.insert(name.clone()) {
            names.push(name);
        }
    }

    Ok(names)
}

/// Fetch each remote through the resolve → `RemoteTarget` routing (local heddle
/// sync or hosted `fetch_network`). This is the shared tail for hosted-native
/// repos and for git-overlay repos whose remote(s) are hosted `heddle://`
/// endpoints.
async fn fetch_via_network(
    cli: &Cli,
    repo: &Repository,
    remotes: Vec<String>,
    all: bool,
) -> Result<()> {
    let mut total_refs = 0;
    let mut total_objects = 0;
    #[cfg(feature = "client")]
    let user_config = UserConfig::load_default()?;

    for remote_name in &remotes {
        #[cfg(feature = "client")]
        let (target, server_key) = resolve_remote_with_key(repo, Some(remote_name.as_str()))
            .map_err(anyhow::Error::msg)?;
        #[cfg(not(feature = "client"))]
        let (target, _server_key) = resolve_remote_with_key(repo, Some(remote_name.as_str()))
            .map_err(anyhow::Error::msg)?;

        match target {
            RemoteTarget::Local(path) => {
                let (refs, objects) = fetch_local(repo, &path, remote_name, cli).await?;
                total_refs += refs;
                total_objects += objects;
            }
            RemoteTarget::Network { addr, repo_path } => {
                #[cfg(feature = "client")]
                {
                    let (refs, objects) = match fetch_network(
                        repo,
                        FetchNetworkOptions {
                            addr,
                            repo_path: repo_path.as_deref(),
                            user_config: &user_config,
                            server_key,
                            remote_name,
                            cli,
                        },
                    )
                    .await
                    {
                        Ok(result) => result,
                        Err(err) => {
                            return Err(augment_missing_blob_error(repo, err));
                        }
                    };
                    total_refs += refs;
                    total_objects += objects;
                }
                #[cfg(not(feature = "client"))]
                {
                    let _ = (addr, repo_path);
                    anyhow::bail!(RecoveryAdvice::network_feature_unavailable("fetch"));
                }
            }
        }
    }

    if should_output_json(cli, Some(repo.config())) {
        println!(
            "{}",
            serde_json::to_string(&FetchOutput {
                output_kind: "fetch",
                remote: if all {
                    "all".to_string()
                } else {
                    remotes
                        .first()
                        .cloned()
                        .unwrap_or_else(|| "origin".to_string())
                },
                ref_scope: None,
                tags_included: None,
                refs_fetched: total_refs,
                objects_fetched: total_objects,
                trust: build_repository_verification_state(repo),
            })?
        );
    } else {
        println!(
            "{} fetched {} and {}",
            style::ok_marker(),
            style::count(total_refs, "ref"),
            style::count(total_objects, "object")
        );
    }

    Ok(())
}

async fn fetch_local(
    repo: &Repository,
    source_path: &std::path::Path,
    remote_name: &str,
    cli: &Cli,
) -> Result<(usize, usize)> {
    if !should_output_json(cli, Some(repo.config())) {
        println!(
            "{} fetching from {} {}",
            style::working_marker(),
            style::bold(remote_name),
            style::dim(&format!("file://{}", source_path.display()))
        );
    }

    let source = LocalSync::open(source_path)?;
    let mut refs_fetched = 0;
    let mut objects_fetched = 0;

    // Fetch all threads from source. `fetch_state` is invoked
    // unconditionally — not just when the state is missing locally —
    // so `LocalSync` can sweep the tree for redaction sidecars the
    // peer declared after we last synced. The internal walk is cheap
    // when no objects need copying, but it must still run for
    // `accept_wire_redactions` to fire.
    for (track_name, change_id) in source.list_threads()? {
        let count = source.fetch_state(repo, &change_id)?;
        objects_fetched += count;
        repo.refs()
            .set_remote_thread(remote_name, &ThreadName::new(&track_name), &change_id)?;
        refs_fetched += 1;
    }

    // Fetch all markers from source (copy locally, not as remote refs)
    for (marker_name, change_id) in source.list_markers()? {
        let count = source.fetch_state(repo, &change_id)?;
        objects_fetched += count;
        // Create local marker if it doesn't exist
        let mn = MarkerName::new(&marker_name);
        if repo.refs().get_marker(&mn)?.is_none() {
            repo.refs().create_marker(&mn, &change_id)?;
        }
    }

    Ok((refs_fetched, objects_fetched))
}

#[cfg(feature = "client")]
struct FetchNetworkOptions<'a> {
    addr: std::net::SocketAddr,
    repo_path: Option<&'a str>,
    user_config: &'a UserConfig,
    server_key: Option<String>,
    remote_name: &'a str,
    cli: &'a Cli,
}

#[cfg(feature = "client")]
async fn fetch_network(
    repo: &Repository,
    options: FetchNetworkOptions<'_>,
) -> Result<(usize, usize)> {
    let repo_path = options
        .repo_path
        .context("network remotes must include a hosted repository path")?;

    let mut client = HostedGrpcClient::open_session(
        options.addr,
        options.user_config,
        options.server_key,
        // fetch hits the PoP-gated RepoSync transport (like push/pull/clone),
        // so it needs the credential store's proof key, not a token-only
        // session. CredentialFallback is identical to ConfigToken when an env
        // token is set and adds the proof-key fallback otherwise.
        HostedAuthMode::CredentialFallback,
    )
    .await?
    .with_human_signature_callback(crate::client::cli_human_signature_callback());

    if !should_output_json(options.cli, Some(repo.config())) {
        println!(
            "{} connected to {} {}",
            style::ok_marker(),
            style::bold(options.remote_name),
            style::dim(&options.addr.to_string())
        );
    }

    // List remote refs
    let remote_refs = client.list_refs(repo_path).await?;

    let mut refs_to_update = Vec::new();
    let mut markers_to_create = Vec::new();
    let mut fetched_states = HashSet::new();
    let mut objects_fetched = 0;

    for ref_entry in &remote_refs {
        if fetched_states.insert(ref_entry.change_id)
            && !repo.store().has_state(&ref_entry.change_id)?
        {
            objects_fetched += fetch_remote_state(
                &mut client,
                repo,
                repo_path,
                &ref_entry.name,
                ref_entry.change_id,
            )
            .await?;
        }

        if ref_entry.is_thread {
            refs_to_update.push((ref_entry.name.clone(), ref_entry.change_id));
        } else {
            markers_to_create.push((ref_entry.name.clone(), ref_entry.change_id));
        }
    }

    // Update remote refs
    let refs_fetched = refs_to_update.len();
    for (track_name, change_id) in refs_to_update {
        repo.refs().set_remote_thread(
            options.remote_name,
            &ThreadName::new(&track_name),
            &change_id,
        )?;
    }

    for (marker_name, change_id) in markers_to_create {
        let mn = MarkerName::new(&marker_name);
        if repo.refs().get_marker(&mn)?.is_none() {
            repo.refs().create_marker(&mn, &change_id)?;
        }
    }
    Ok((refs_fetched, objects_fetched))
}

#[cfg(feature = "client")]
async fn fetch_remote_state(
    client: &mut HostedGrpcClient,
    repo: &Repository,
    repo_path: &str,
    remote_name: &str,
    state_id: ChangeId,
) -> Result<usize> {
    client
        .fetch_state(repo, repo_path, remote_name, state_id)
        .await
        .map_err(anyhow::Error::new)
}

#[cfg(feature = "client")]
fn augment_missing_blob_error(repo: &Repository, err: anyhow::Error) -> anyhow::Error {
    let Ok(missing) = repo.missing_blobs() else {
        return err;
    };
    if missing.is_empty() {
        return err;
    }

    let missing = missing
        .into_iter()
        .map(|hash| hash.short())
        .collect::<Vec<_>>()
        .join(", ");
    anyhow::anyhow!("{err}; missing blobs: {missing}")
}
