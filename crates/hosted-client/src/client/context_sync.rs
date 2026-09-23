// SPDX-License-Identifier: Apache-2.0
//! Hosted replication for native context operations.
//!
//! Local annotations remain the authoring view. Push converts each authored
//! revision once into the typed native context record, including its scope,
//! kind, attribution, source hash, creation state, and occurrence time. The
//! exact signed bytes and expected causal version are stored before delivery,
//! so retries cannot reconstruct or restamp the evidence. Pull verifies native
//! signed records and materializes the annotation view from those originals.
//!
//! An observed hosted projection without the corresponding signed operation is
//! reported as incomplete rather than presented as authored provenance.

#![cfg(feature = "client")]

use std::{
    collections::{BTreeMap, HashSet},
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use api::heddle::api::v1alpha2::SignedRecord;
use objects::{
    fs_atomic::write_file_atomic,
    object::{
        Annotation, AnnotationRevision, AnnotationScope, CollaborationAnchor,
        CollaborationRevision, CollaborationSourceAnchor, ContextBlob, ContextProvenance,
        ContextTarget, State, StateId,
    },
    store::ObjectStore,
};
use prost::Message;
use repo::Repository;
use serde::{Deserialize, Serialize};

use crate::{
    attachments::{context_root_for_state, put_context_attachment},
    client::HostedClient,
};

// =========================================================================
// Mirror map
// =========================================================================

#[derive(Debug, Default, Serialize, Deserialize)]
struct HostedContextMirror {
    #[serde(default)]
    repos: BTreeMap<String, RepoContextMirror>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct RepoContextMirror {
    #[serde(default)]
    annotations: Vec<ContextMirrorEntry>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct ContextMirrorEntry {
    /// Local `annotation_id`.
    local_id: String,
    /// Server `annotation_id`. Empty while a create is in flight.
    #[serde(default)]
    server_id: String,
    /// `local_revision_id ↔ server_revision_id` links.
    #[serde(default)]
    revision_links: Vec<RevisionLink>,
    /// Write-ahead: the `client_operation_id` a create RPC is (or was) issued
    /// with, so a crash-retry replays rather than duplicating.
    #[serde(default)]
    pending_create_op: Option<String>,
    /// Exact native records prepared before delivery, keyed by the local
    /// revision identity. Retries decode and resend these bytes unchanged.
    #[serde(default)]
    native_operations: Vec<PreparedContextOperation>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct PreparedContextOperation {
    local_revision_id: String,
    signed_record: Vec<u8>,
    expected_version: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct RevisionLink {
    local: String,
    server: String,
}

fn mirror_path(heddle_dir: &Path) -> PathBuf {
    heddle_dir
        .join("collaboration")
        .join("hosted-context-mirror.json")
}

fn load_mirror(heddle_dir: &Path) -> Result<HostedContextMirror> {
    match fs::read(mirror_path(heddle_dir)) {
        Ok(bytes) => serde_json::from_slice(&bytes).context("decode hosted context mirror map"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Ok(HostedContextMirror::default())
        }
        Err(error) => Err(error).context("read hosted context mirror map"),
    }
}

fn save_mirror(heddle_dir: &Path, mirror: &HostedContextMirror) -> Result<()> {
    let path = mirror_path(heddle_dir);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).context("create collaboration dir")?;
    }
    let bytes = serde_json::to_vec_pretty(mirror).context("encode hosted context mirror map")?;
    write_file_atomic(&path, &bytes).context("write hosted context mirror map")?;
    Ok(())
}

// --- mirror accessors ---

fn entry_index(mirror: &HostedContextMirror, repo_path: &str, local_id: &str) -> Option<usize> {
    mirror
        .repos
        .get(repo_path)?
        .annotations
        .iter()
        .position(|entry| entry.local_id == local_id)
}

fn get_or_create_entry<'a>(
    mirror: &'a mut HostedContextMirror,
    repo_path: &str,
    local_id: &str,
) -> &'a mut ContextMirrorEntry {
    let repo_mirror = mirror.repos.entry(repo_path.to_string()).or_default();
    if let Some(index) = repo_mirror
        .annotations
        .iter()
        .position(|entry| entry.local_id == local_id)
    {
        return &mut repo_mirror.annotations[index];
    }
    repo_mirror.annotations.push(ContextMirrorEntry {
        local_id: local_id.to_string(),
        ..Default::default()
    });
    repo_mirror.annotations.last_mut().expect("just pushed")
}

fn server_id_for_local(
    mirror: &HostedContextMirror,
    repo_path: &str,
    local_id: &str,
) -> Option<String> {
    let entry = mirror
        .repos
        .get(repo_path)?
        .annotations
        .iter()
        .find(|entry| entry.local_id == local_id)?;
    (!entry.server_id.is_empty()).then(|| entry.server_id.clone())
}

fn local_id_for_server(
    mirror: &HostedContextMirror,
    repo_path: &str,
    server_id: &str,
) -> Option<String> {
    mirror
        .repos
        .get(repo_path)?
        .annotations
        .iter()
        .find(|entry| entry.server_id == server_id)
        .map(|entry| entry.local_id.clone())
}

fn add_revision_link(
    mirror: &mut HostedContextMirror,
    repo_path: &str,
    local_id: &str,
    local_rev: String,
    server_rev: String,
) {
    let entry = get_or_create_entry(mirror, repo_path, local_id);
    if !entry
        .revision_links
        .iter()
        .any(|link| link.local == local_rev && link.server == server_rev)
    {
        entry.revision_links.push(RevisionLink {
            local: local_rev,
            server: server_rev,
        });
    }
}

fn native_context_anchor(
    target: &ContextTarget,
    scope: &AnnotationScope,
    revision: &AnnotationRevision,
) -> Result<CollaborationAnchor> {
    match target {
        ContextTarget::State { state_id } => Ok(CollaborationAnchor::State {
            state_id: *state_id,
        }),
        ContextTarget::File { path } => {
            let state_id = revision.created_at_state.ok_or_else(|| {
                anyhow::anyhow!(
                    "context revision {} has no exact creation state; native replication is incomplete",
                    revision.revision_id
                )
            })?;
            match scope {
                AnnotationScope::File => Ok(CollaborationAnchor::Path {
                    state_id,
                    path: path.clone(),
                }),
                AnnotationScope::Symbol {
                    name,
                    resolved_lines,
                } => Ok(CollaborationAnchor::Source {
                    source: CollaborationSourceAnchor {
                        revision: CollaborationRevision::State { state_id },
                        path: path.clone(),
                        symbol_id: name.clone(),
                        start_line: resolved_lines.map(|(start, _)| start),
                        end_line: resolved_lines.map(|(_, end)| end),
                        target: None,
                    },
                }),
                AnnotationScope::Lines(start, end) => Ok(CollaborationAnchor::Source {
                    source: CollaborationSourceAnchor {
                        revision: CollaborationRevision::State { state_id },
                        path: path.clone(),
                        symbol_id: String::new(),
                        start_line: Some(*start),
                        end_line: Some(*end),
                        target: None,
                    },
                }),
            }
        }
    }
}

fn native_context_provenance(revision: &AnnotationRevision) -> ContextProvenance {
    ContextProvenance {
        revision_id: revision.revision_id.clone(),
        kind: revision.kind,
        attribution: revision.attribution.clone(),
        source_hash: revision.source_hash,
        created_at_state: revision.created_at_state,
    }
}

#[allow(clippy::too_many_arguments)]
async fn publish_native_context_revision(
    client: &mut HostedClient,
    repo_path: &str,
    thread_ref: &str,
    heddle_dir: &Path,
    mirror: &mut HostedContextMirror,
    local_annotation_id: &str,
    remote_annotation_id: &str,
    revision: &AnnotationRevision,
    anchor: CollaborationAnchor,
    supersedes: Option<uuid::Uuid>,
    client_operation_id: String,
) -> Result<()> {
    let prepared = mirror
        .repos
        .get(repo_path)
        .and_then(|repository| {
            repository
                .annotations
                .iter()
                .find(|entry| entry.local_id == local_annotation_id)
        })
        .and_then(|entry| {
            entry
                .native_operations
                .iter()
                .find(|operation| operation.local_revision_id == revision.revision_id)
        })
        .cloned();
    let (signed, expected_version) = if let Some(prepared) = prepared {
        (
            SignedRecord::decode(prepared.signed_record.as_slice())
                .context("decode prepared native context operation")?,
            prepared.expected_version,
        )
    } else {
        let (signed, expected_version) = client
            .prepare_native_context_record(
                repo_path,
                Some(thread_ref),
                remote_annotation_id,
                anchor,
                &revision.content,
                revision.tags.iter().cloned().map(Into::into).collect(),
                native_context_provenance(revision),
                revision.created_at.saturating_mul(1000),
                supersedes,
            )
            .await?;
        get_or_create_entry(mirror, repo_path, local_annotation_id)
            .native_operations
            .push(PreparedContextOperation {
                local_revision_id: revision.revision_id.clone(),
                signed_record: signed.encode_to_vec(),
                expected_version: expected_version.clone(),
            });
        save_mirror(heddle_dir, mirror)?;
        (signed, expected_version)
    };
    client
        .publish_native_context_record(signed, expected_version, client_operation_id)
        .await?;
    Ok(())
}

/// The attribution string weft stamps on our own hosted writes:
/// `Principal::new(username, "")` renders as `"{username} <>"`.
fn hosted_attribution(username: Option<&str>) -> Option<String> {
    username.map(|name| format!("{name} <>"))
}

// =========================================================================
// Push
// =========================================================================

/// Publish local annotations we authored to the hosted `RepositoryService`.
pub async fn push_context(
    repo: &Repository,
    client: &mut HostedClient,
    repo_path: &str,
    thread_ref: &str,
) -> Result<usize> {
    let Some(head_id) = repo.head().context("resolve repository head")? else {
        return Ok(0);
    };
    let Some(head_state) = repo
        .store()
        .get_state(&head_id)
        .context("load head state")?
    else {
        return Ok(0);
    };
    let Some(context_root) = context_root_for_state(repo, &head_state)? else {
        return Ok(0);
    };
    let entries = repo
        .list_context_entries(&context_root, None)
        .context("enumerate local context annotations")?;
    if entries.is_empty() {
        return Ok(0);
    }

    let user_config = config::UserConfig::load_default().unwrap_or_default();
    let self_local_attr = crate::attribution::resolve_attribution(repo, &user_config)
        .ok()
        .map(|attribution| attribution.to_string());
    let server_ann_ids: HashSet<String> = list_server_annotations(client, repo_path, Some(head_id))
        .await?
        .into_iter()
        .map(|annotation| annotation.annotation_id)
        .collect();

    let heddle_dir = repo.heddle_dir().to_path_buf();
    let mut mirror = load_mirror(&heddle_dir)?;
    let mut synced = 0usize;
    let mut incomplete = Vec::new();
    for entry in &entries {
        for annotation in &entry.blob.annotations {
            let result = push_one(
                client,
                repo_path,
                thread_ref,
                &heddle_dir,
                &entry.target,
                annotation,
                self_local_attr.as_deref(),
                &server_ann_ids,
                &mut mirror,
            )
            .await;
            save_mirror(&heddle_dir, &mirror)?;
            match result {
                Ok(true) => synced += 1,
                Ok(false) => {}
                Err(error) => {
                    client.warn(
                        "hosted_context_sync_failed",
                        format!("hosted context {}: {error:#}", annotation.annotation_id),
                    );
                    incomplete.push(format!("{}: {error:#}", annotation.annotation_id));
                }
            }
        }
    }
    if !incomplete.is_empty() {
        anyhow::bail!(
            "hosted context replication incomplete: {}",
            incomplete.join("; ")
        );
    }
    Ok(synced)
}

#[allow(clippy::too_many_arguments)]
async fn push_one(
    client: &mut HostedClient,
    repo_path: &str,
    thread_ref: &str,
    heddle_dir: &Path,
    target: &ContextTarget,
    annotation: &Annotation,
    self_local_attr: Option<&str>,
    server_ann_ids: &HashSet<String>,
    mirror: &mut HostedContextMirror,
) -> Result<bool> {
    // ---- 1. Resolve the server annotation id. ----
    let mut created = false;
    let server_id = if let Some(sid) =
        server_id_for_local(mirror, repo_path, &annotation.annotation_id)
    {
        sid
    } else if server_ann_ids.contains(&annotation.annotation_id) {
        // Pulled / pack-delivered: local id IS the server id. Adopt it.
        let entry = get_or_create_entry(mirror, repo_path, &annotation.annotation_id);
        entry.server_id = annotation.annotation_id.clone();
        annotation.annotation_id.clone()
    } else {
        // Genuinely new → create. Fail-closed self filter.
        let first = annotation
            .revisions
            .first()
            .context("annotation has no revisions")?;
        let is_self = self_local_attr.is_some_and(|me| me == first.attribution);
        if !is_self {
            anyhow::bail!(
                "context {} is not attributed to the local principal and has no retained native signed operation; replication is incomplete",
                annotation.annotation_id
            );
        }
        // Resolve the superseded LOCAL id → SERVER id through the mirror (P1-3).
        let superseded_server = annotation
            .supersedes_annotation_id
            .as_ref()
            .and_then(|local| {
                server_id_for_local(mirror, repo_path, local)
                    .or_else(|| server_ann_ids.contains(local).then(|| local.clone()))
            });

        // Write-ahead: persist a create nonce to DISK before the RPC so a
        // crash-retry replays with the same client_operation_id (P2-5).
        let op_id = {
            let entry = get_or_create_entry(mirror, repo_path, &annotation.annotation_id);
            entry
                .pending_create_op
                .get_or_insert_with(|| {
                    create_op_id(repo_path, &annotation.annotation_id, &first.revision_id)
                })
                .clone()
        };
        save_mirror(heddle_dir, mirror)?;

        let (sid, first_server_rev) = create_on_server(
            client,
            repo_path,
            thread_ref,
            heddle_dir,
            mirror,
            target,
            annotation,
            superseded_server.as_deref(),
            &op_id,
        )
        .await?;

        // Applied (or Dedup Conflict treated as replay) always clears the
        // nonce. Leaving it pending after success replays the same id with a
        // new signed body and weft Dedup Conflicts.
        {
            let entry = get_or_create_entry(mirror, repo_path, &annotation.annotation_id);
            entry.server_id = sid.clone();
            entry.pending_create_op = None;
        }
        add_revision_link(
            mirror,
            repo_path,
            &annotation.annotation_id,
            annotation.revisions[0].revision_id.clone(),
            first_server_rev,
        );
        created = true;
        sid
    };

    // ---- 2. Sync revisions (linear, id-linked, author-aware recovery). ----
    let pushed = sync_revisions_push(
        client,
        repo_path,
        thread_ref,
        heddle_dir,
        &server_id,
        target,
        annotation,
        self_local_attr,
        mirror,
    )
    .await?;

    Ok(created || pushed > 0)
}

/// Create a fresh annotation server-side, returning `(server_annotation_id,
/// first_server_revision_id)`.
#[allow(clippy::too_many_arguments)]
async fn create_on_server(
    client: &mut HostedClient,
    repo_path: &str,
    thread_ref: &str,
    heddle_dir: &Path,
    mirror: &mut HostedContextMirror,
    target: &ContextTarget,
    annotation: &Annotation,
    superseded_server: Option<&str>,
    op_id: &str,
) -> Result<(String, String)> {
    let first = annotation
        .revisions
        .first()
        .context("annotation has no revisions")?;
    let anchor = native_context_anchor(target, &annotation.scope, first)?;
    let supersedes = superseded_server
        .map(|value| {
            uuid::Uuid::parse_str(value.trim_start_matches("ann-")).with_context(|| {
                format!("superseded context id {value} is not a UUID; replication is incomplete")
            })
        })
        .transpose()?;
    let put = publish_native_context_revision(
        client,
        repo_path,
        thread_ref,
        heddle_dir,
        mirror,
        &annotation.annotation_id,
        &annotation.annotation_id,
        first,
        anchor,
        supersedes,
        op_id.to_string(),
    )
    .await
    .map(|()| annotation.annotation_id.clone());

    match put {
        Ok(_) => {}
        Err(error) if is_operation_id_conflict(&error) => {}
        Err(error) => {
            return Err(error).with_context(|| {
                if let Some(superseded) = superseded_server {
                    format!("supersede hosted annotation {superseded}")
                } else {
                    format!("set hosted context for {}", annotation.annotation_id)
                }
            });
        }
    }

    Ok((annotation.annotation_id.clone(), first.revision_id.clone()))
}

fn is_operation_id_conflict(error: &impl std::fmt::Display) -> bool {
    let text = error.to_string().to_ascii_lowercase();
    text.contains("names another command")
        || text.contains("names a different command")
        || text.contains("reused with a different request")
        || text.contains("operation_id_reused")
}

/// Forward local revisions the server does not yet hold. Returns the count
/// actually pushed. Author-aware recovery links each minted server revision id.
#[allow(clippy::too_many_arguments)]
async fn sync_revisions_push(
    client: &mut HostedClient,
    repo_path: &str,
    thread_ref: &str,
    heddle_dir: &Path,
    server_id: &str,
    target: &ContextTarget,
    annotation: &Annotation,
    self_local_attr: Option<&str>,
    mirror: &mut HostedContextMirror,
) -> Result<usize> {
    let mut server_rev_ids: HashSet<String> = fetch_history(client, repo_path, server_id)
        .await?
        .into_iter()
        .map(|rev| rev.revision_id)
        .collect();
    let linked_local: HashSet<String> = mirror
        .repos
        .get(repo_path)
        .and_then(|m| {
            m.annotations
                .iter()
                .find(|e| e.local_id == annotation.annotation_id)
        })
        .map(|entry| {
            entry
                .revision_links
                .iter()
                .map(|l| l.local.clone())
                .collect()
        })
        .unwrap_or_default();

    let mut pushed = 0usize;
    for revision in &annotation.revisions {
        if linked_local.contains(&revision.revision_id) {
            continue;
        }
        if server_rev_ids.contains(&revision.revision_id) {
            // Pulled / pack-delivered: local revision id IS the server id.
            add_revision_link(
                mirror,
                repo_path,
                &annotation.annotation_id,
                revision.revision_id.clone(),
                revision.revision_id.clone(),
            );
            continue;
        }
        // A local-only revision. Only forward our own (a foreign unlinked
        // revision not on the server has no native original we can replay).
        if self_local_attr != Some(revision.attribution.as_str()) {
            anyhow::bail!(
                "context {} revision {} is not attributed to the local principal and has no retained native signed operation; replication is incomplete",
                annotation.annotation_id,
                revision.revision_id
            );
        }
        let anchor = native_context_anchor(target, &annotation.scope, revision)?;
        let supersedes = annotation
            .supersedes_annotation_id
            .as_deref()
            .map(|value| uuid::Uuid::parse_str(value.trim_start_matches("ann-")))
            .transpose()
            .context("superseded context id is not a UUID")?;
        publish_native_context_revision(
            client,
            repo_path,
            thread_ref,
            heddle_dir,
            mirror,
            &annotation.annotation_id,
            server_id,
            revision,
            anchor,
            supersedes,
            revise_op_id(repo_path, server_id, &revision.revision_id),
        )
        .await
        .with_context(|| format!("revise hosted annotation {server_id}"))?;

        add_revision_link(
            mirror,
            repo_path,
            &annotation.annotation_id,
            revision.revision_id.clone(),
            revision.revision_id.clone(),
        );
        server_rev_ids.insert(revision.revision_id.clone());
        pushed += 1;
    }
    Ok(pushed)
}

// =========================================================================
// Pull
// =========================================================================

/// Fetch hosted annotations for `against` (or repository HEAD) and reconcile
/// them into the local `Context` attachment, rebuilding revision order to
/// match the server.
///
/// `against` is the pulled/cloned tip. Clone publishes HEAD only after this
/// call, and `heddle pull feature --local-thread feature` leaves HEAD on the
/// current checkout, so the fallback must not re-read HEAD.
pub async fn pull_context(
    repo: &Repository,
    client: &mut HostedClient,
    repo_path: &str,
    against: Option<StateId>,
) -> Result<usize> {
    let Some(head_id) = context_sync_state(repo, against)? else {
        return Ok(0);
    };
    let Some(head_state) = repo
        .store()
        .get_state(&head_id)
        .context("load head state")?
    else {
        return Ok(0);
    };

    let user_config = config::UserConfig::load_default().unwrap_or_default();
    let self_local_attr = crate::attribution::resolve_attribution(repo, &user_config)
        .ok()
        .map(|attribution| attribution.to_string());
    let username = client.authenticated_username();

    let heddle_dir = repo.heddle_dir().to_path_buf();
    let mut mirror = load_mirror(&heddle_dir)?;
    let mut changed = 0usize;
    let originals = client
        .list_context_operations(repo_path)
        .await
        .context("observe native context operations")?;
    for signed in originals {
        let verified = thread_api::collaboration::verify(&signed)
            .map_err(|error| anyhow::anyhow!(error.to_string()))?;
        let objects::object::thread_replication::ThreadOperationBody::Context(bytes) =
            verified.body
        else {
            continue;
        };
        let context = objects::object::ContextRevision::decode(&bytes)
            .map_err(|error| anyhow::anyhow!(error.to_string()))?;
        let provenance = context.provenance.as_ref().ok_or_else(|| {
            anyhow::anyhow!(
                "context {} has no authored provenance; replication is incomplete",
                context.id
            )
        })?;
        let entry = get_or_create_entry(&mut mirror, repo_path, &context.id.to_string());
        if !entry
            .native_operations
            .iter()
            .any(|operation| operation.local_revision_id == provenance.revision_id)
        {
            entry.native_operations.push(PreparedContextOperation {
                local_revision_id: provenance.revision_id.clone(),
                signed_record: signed.encode_to_vec(),
                expected_version: Vec::new(),
            });
        }
    }
    save_mirror(&heddle_dir, &mirror)?;

    let server = list_server_targets(client, repo_path, Some(head_id)).await?;
    for (target, annotation) in server {
        let result = pull_one_rpc(
            repo,
            client,
            repo_path,
            &head_state,
            &target,
            &annotation,
            self_local_attr.as_deref(),
            username.as_deref(),
            &mut mirror,
        )
        .await;
        save_mirror(&heddle_dir, &mirror)?;
        match result {
            Ok(true) => changed += 1,
            Ok(false) => {}
            Err(error) => {
                client.warn(
                    "hosted_context_sync_failed",
                    format!("hosted context {}: {error:#}", annotation.annotation_id),
                );
            }
        }
    }
    Ok(changed)
}

/// Prefer the pulled/cloned tip because clone has not published HEAD yet and a
/// local-thread pull may target a thread other than the current checkout.
fn context_sync_state(repo: &Repository, against: Option<StateId>) -> Result<Option<StateId>> {
    match against {
        Some(state) => Ok(Some(state)),
        None => repo.head().context("resolve repository head"),
    }
}

#[allow(clippy::too_many_arguments)]
async fn pull_one_rpc(
    repo: &Repository,
    client: &mut HostedClient,
    repo_path: &str,
    head_state: &State,
    target: &ContextTarget,
    server: &Annotation,
    self_local_attr: Option<&str>,
    username: Option<&str>,
    mirror: &mut HostedContextMirror,
) -> Result<bool> {
    let mut annotation = server.clone();
    let server_revs = fetch_history(client, repo_path, &server.annotation_id).await?;
    if !server_revs.is_empty() {
        annotation.revisions = server_revs;
    }
    pull_one_annotation(
        repo,
        repo_path,
        head_state,
        target,
        &annotation,
        self_local_attr,
        username,
        mirror,
    )
}

#[allow(clippy::too_many_arguments)]
fn pull_one_annotation(
    repo: &Repository,
    repo_path: &str,
    head_state: &State,
    target: &ContextTarget,
    server: &Annotation,
    self_local_attr: Option<&str>,
    username: Option<&str>,
    mirror: &mut HostedContextMirror,
) -> Result<bool> {
    let local_id = local_id_for_server(mirror, repo_path, &server.annotation_id)
        .unwrap_or_else(|| server.annotation_id.clone());

    let context_root = context_root_for_state(repo, head_state)?;
    let mut blob = match &context_root {
        Some(root) => repo
            .get_context_blob(root, target)?
            .unwrap_or_else(|| ContextBlob::new(vec![])),
        None => ContextBlob::new(vec![]),
    };

    let existing_index = blob
        .annotations
        .iter()
        .position(|annotation| annotation.annotation_id == local_id);

    let existing_revisions: Vec<AnnotationRevision> = existing_index
        .map(|index| blob.annotations[index].revisions.clone())
        .unwrap_or_default();
    let existing_links: Vec<RevisionLink> = entry_index(mirror, repo_path, &local_id)
        .map(|index| {
            mirror.repos[repo_path].annotations[index]
                .revision_links
                .clone()
        })
        .unwrap_or_default();

    let (new_revisions, new_links) = reconcile_revisions_pull(
        &existing_revisions,
        &server.revisions,
        &existing_links,
        self_local_attr,
        username,
    );

    {
        let entry = get_or_create_entry(mirror, repo_path, &local_id);
        entry.server_id = server.annotation_id.clone();
        entry.revision_links = new_links;
    }

    let changed = match existing_index {
        Some(index) => {
            let annotation = &mut blob.annotations[index];
            let differs = annotation.revisions != new_revisions
                || annotation.status != server.status
                || annotation.supersedes_annotation_id != server.supersedes_annotation_id
                || annotation.supersedes_rewrite_pct != server.supersedes_rewrite_pct;
            annotation.revisions = new_revisions;
            annotation.status = server.status;
            annotation.supersedes_annotation_id = server.supersedes_annotation_id.clone();
            annotation.supersedes_rewrite_pct = server.supersedes_rewrite_pct;
            differs
        }
        None => {
            blob.annotations.push(Annotation {
                annotation_id: local_id.clone(),
                scope: server.scope.clone(),
                status: server.status,
                revisions: new_revisions,
                supersedes_annotation_id: server.supersedes_annotation_id.clone(),
                supersedes_rewrite_pct: server.supersedes_rewrite_pct,
                visibility: server.visibility.clone(),
                resolved_from_discussion: server.resolved_from_discussion.clone(),
                anchor_status: server.anchor_status.clone(),
            });
            true
        }
    };

    if !changed {
        return Ok(false);
    }
    let new_root = repo.set_context_blob(context_root.as_ref(), target, &blob)?;
    if context_root != Some(new_root) {
        put_context_attachment(repo, head_state, Some(new_root))?;
    }
    Ok(true)
}

/// Rebuild the local revision list to match the server order, linking each
/// server revision to a local revision (existing link, id equality, or
/// author-aware reconcile) or materializing a new one. Purely-local revisions
/// not yet on the server are preserved at the tail.
fn reconcile_revisions_pull(
    existing: &[AnnotationRevision],
    server_revs: &[AnnotationRevision],
    existing_links: &[RevisionLink],
    self_local_attr: Option<&str>,
    username: Option<&str>,
) -> (Vec<AnnotationRevision>, Vec<RevisionLink>) {
    let hosted = hosted_attribution(username);
    let linked_local: HashSet<&str> = existing_links.iter().map(|l| l.local.as_str()).collect();
    let mut link_by_server: std::collections::HashMap<&str, &str> =
        std::collections::HashMap::new();
    for link in existing_links {
        link_by_server.insert(link.server.as_str(), link.local.as_str());
    }

    let mut consumed: HashSet<String> = HashSet::new();
    let mut new_revisions: Vec<AnnotationRevision> = Vec::new();
    let mut new_links: Vec<RevisionLink> = Vec::new();

    for server_rev in server_revs {
        // (a) existing link by server id.
        if let Some(local_rev_id) = link_by_server.get(server_rev.revision_id.as_str())
            && let Some(local) = existing.iter().find(|rev| {
                rev.revision_id == *local_rev_id && !consumed.contains(&rev.revision_id)
            })
        {
            consumed.insert(local.revision_id.clone());
            new_revisions.push(local.clone());
            new_links.push(RevisionLink {
                local: local.revision_id.clone(),
                server: server_rev.revision_id.clone(),
            });
            continue;
        }
        // (b) id equality (pack-delivered: local rev id == server rev id).
        if let Some(local) = existing.iter().find(|rev| {
            rev.revision_id == server_rev.revision_id && !consumed.contains(&rev.revision_id)
        }) {
            consumed.insert(local.revision_id.clone());
            new_revisions.push(local.clone());
            new_links.push(RevisionLink {
                local: local.revision_id.clone(),
                server: server_rev.revision_id.clone(),
            });
            continue;
        }
        // (c) author-aware reconcile against unlinked, unconsumed local revisions.
        let candidate = existing.iter().find(|rev| {
            !linked_local.contains(rev.revision_id.as_str())
                && !consumed.contains(&rev.revision_id)
                && rev.content == server_rev.content
                && reconcile_ok(rev, server_rev, hosted.as_deref(), self_local_attr)
        });
        if let Some(local) = candidate {
            consumed.insert(local.revision_id.clone());
            new_revisions.push(local.clone());
            new_links.push(RevisionLink {
                local: local.revision_id.clone(),
                server: server_rev.revision_id.clone(),
            });
            continue;
        }
        // (d) materialize a new local revision preserving the server id.
        consumed.insert(server_rev.revision_id.clone());
        new_revisions.push(server_rev.clone());
        new_links.push(RevisionLink {
            local: server_rev.revision_id.clone(),
            server: server_rev.revision_id.clone(),
        });
    }

    // Preserve purely-local revisions not yet on the server (unpushed edits), in
    // their original order, at the tail.
    for revision in existing {
        if !consumed.contains(&revision.revision_id)
            && !new_revisions
                .iter()
                .any(|rev| rev.revision_id == revision.revision_id)
        {
            new_revisions.push(revision.clone());
        }
    }

    (new_revisions, new_links)
}

/// Author rule for reconciling an unlinked local revision with a server one —
/// body equality is a precondition the caller already checked; this decides
/// authorship. Never links on body alone.
fn reconcile_ok(
    local: &AnnotationRevision,
    server: &AnnotationRevision,
    hosted: Option<&str>,
    self_local_attr: Option<&str>,
) -> bool {
    // (i) A revision WE pushed: locally self-authored AND server-stamped with our
    // hosted username.
    let pushed_by_us = self_local_attr == Some(local.attribution.as_str())
        && hosted == Some(server.attribution.as_str());
    // (ii) A revision we previously PULLED: local copied the server attribution +
    // timestamp verbatim.
    let pulled_before =
        local.attribution == server.attribution && local.created_at == server.created_at;
    pushed_by_us || pulled_before
}

// =========================================================================
// Server enumeration
// =========================================================================

async fn list_server_annotations(
    client: &mut HostedClient,
    repo_path: &str,
    fallback_state: Option<StateId>,
) -> Result<Vec<Annotation>> {
    Ok(list_server_targets(client, repo_path, fallback_state)
        .await?
        .into_iter()
        .map(|(_, annotation)| annotation)
        .collect())
}

async fn list_server_targets(
    client: &mut HostedClient,
    repo_path: &str,
    fallback_state: Option<StateId>,
) -> Result<Vec<(ContextTarget, Annotation)>> {
    let annotations = client
        .list_context(repo_path, None, None, None)
        .await
        .context("list hosted context")?;
    Ok(annotations
        .into_iter()
        .filter_map(|(target, annotation)| {
            target
                .or_else(|| fallback_state.map(ContextTarget::state))
                .map(|target| (target, annotation))
        })
        .collect())
}

/// `GetContextHistory` returns revisions newest-first; local storage is
/// oldest-first, so reverse.
async fn fetch_history(
    client: &mut HostedClient,
    repo_path: &str,
    annotation_id: &str,
) -> Result<Vec<AnnotationRevision>> {
    let history = client
        .get_context_history(repo_path, None, annotation_id)
        .await
        .with_context(|| format!("fetch hosted annotation history {annotation_id}"))?;
    let mut revisions = history;
    revisions.reverse();
    Ok(revisions)
}

// --- deterministic client-operation-ids (idempotent retry) ---

const OP_NAMESPACE: uuid::Uuid = uuid::Uuid::from_u128(0x6865_6464_6c65_6374_785f_7379_6e63_0001);

fn revise_op_id(repo_path: &str, server_id: &str, revision_id: &str) -> String {
    uuid::Uuid::new_v5(
        &OP_NAMESPACE,
        format!("revise:{repo_path}:{server_id}:{revision_id}").as_bytes(),
    )
    .to_string()
}

/// Deterministic client-operation-id for the initial `create_on_server` call,
/// derived from `(annotation id, first revision id)` so a crash-retry of the
/// same create replays with the same id and dedupes instead of duplicating.
fn create_op_id(repo_path: &str, annotation_id: &str, revision_id: &str) -> String {
    uuid::Uuid::new_v5(
        &OP_NAMESPACE,
        format!("create:{repo_path}:{annotation_id}:{revision_id}").as_bytes(),
    )
    .to_string()
}

#[cfg(test)]
mod tests {
    use objects::object::{AnnotationKind, Attribution, ContentHash, Principal};
    use tempfile::TempDir;

    use super::*;

    fn rev(id: &str, attr: &str, content: &str, created_at: i64) -> AnnotationRevision {
        AnnotationRevision {
            revision_id: id.to_string(),
            kind: AnnotationKind::Rationale,
            content: content.to_string(),
            tags: vec![],
            attribution: attr.to_string(),
            created_at,
            source_hash: None,
            created_at_state: None,
        }
    }

    // P1-2: two clones concurrently revise one annotation. Alice already holds
    // r1 (pulled) and her own r3 (linked to server sA3); Bob's r2 arrives in the
    // middle. Pull must yield all three in SERVER order, keep Alice's local id
    // for sA3, materialize Bob's, and neither drop nor duplicate anything.
    #[test]
    fn pull_two_author_revisions_no_loss_no_dup_server_order() {
        let _process_env_guard = crate::test_process_env::shared_blocking();
        let existing = vec![
            rev("sA1", "alice <>", "v1", 1), // pulled earlier (local id == server id)
            rev("rA3-local", "Alice <a@x>", "v3", 30), // Alice's own, linked to sA3
        ];
        let existing_links = vec![
            RevisionLink {
                local: "sA1".into(),
                server: "sA1".into(),
            },
            RevisionLink {
                local: "rA3-local".into(),
                server: "sA3".into(),
            },
        ];
        let server_revs = vec![
            rev("sA1", "alice <>", "v1", 1),
            rev("sB2", "bob <>", "v2-bob", 20), // Bob's revision, not yet local
            rev("sA3", "alice <>", "v3", 30),   // Alice's own, came back stamped
        ];
        let (new_revisions, new_links) = reconcile_revisions_pull(
            &existing,
            &server_revs,
            &existing_links,
            Some("Alice <a@x>"),
            Some("alice"),
        );
        let ids: Vec<&str> = new_revisions
            .iter()
            .map(|r| r.revision_id.as_str())
            .collect();
        // Server order, Alice's local id preserved for sA3, Bob materialized.
        assert_eq!(ids, vec!["sA1", "sB2", "rA3-local"]);
        let contents: Vec<&str> = new_revisions.iter().map(|r| r.content.as_str()).collect();
        assert_eq!(contents, vec!["v1", "v2-bob", "v3"]);
        // No duplicates.
        let unique: HashSet<&str> = ids.iter().copied().collect();
        assert_eq!(unique.len(), 3);
        // Every server revision is linked.
        assert_eq!(new_links.len(), 3);
    }

    // P2-4 hazard at the revision layer: identical body from two DIFFERENT
    // authors must not cross-link. Alice's own local, unpushed "lgtm" must NOT be
    // consumed by Bob's server "lgtm"; Bob's materializes distinctly and Alice's
    // survives at the tail (so a later push still publishes it).
    #[test]
    fn pull_rejects_cross_author_identical_body() {
        let _process_env_guard = crate::test_process_env::shared_blocking();
        let existing = vec![rev("rLocal", "Alice <a@x>", "lgtm", 5)];
        let server_revs = vec![rev("sBob", "bob <>", "lgtm", 9)];
        let (new_revisions, _links) = reconcile_revisions_pull(
            &existing,
            &server_revs,
            &[],
            Some("Alice <a@x>"),
            Some("alice"),
        );
        let ids: HashSet<&str> = new_revisions
            .iter()
            .map(|r| r.revision_id.as_str())
            .collect();
        assert_eq!(new_revisions.len(), 2, "both must survive, none collapsed");
        assert!(
            ids.contains("sBob"),
            "Bob's revision materialized distinctly"
        );
        assert!(ids.contains("rLocal"), "Alice's local revision preserved");
    }

    // A revision WE pushed comes back stamped with our hosted username → links
    // (rule i), so pull does not duplicate it.
    #[test]
    fn pull_relinks_our_pushed_revision_after_lost_mirror() {
        let _process_env_guard = crate::test_process_env::shared_blocking();
        let existing = vec![rev("rMine", "Alice <a@x>", "ship it", 40)];
        let server_revs = vec![rev("sMine", "alice <>", "ship it", 40)];
        let (new_revisions, links) = reconcile_revisions_pull(
            &existing,
            &server_revs,
            &[], // mirror lost
            Some("Alice <a@x>"),
            Some("alice"),
        );
        assert_eq!(new_revisions.len(), 1, "no duplicate of our own revision");
        assert_eq!(new_revisions[0].revision_id, "rMine", "local id preserved");
        assert_eq!(links[0].local, "rMine");
        assert_eq!(links[0].server, "sMine");
    }

    #[test]
    fn reconcile_ok_rules() {
        let _process_env_guard = crate::test_process_env::shared_blocking();
        // `hosted` is the server-stamped self attribution ("{username} <>"), as
        // `reconcile_revisions_pull` passes it.
        let hosted = Some("alice <>");
        let me = Some("Alice <a@x>");
        // (i) pushed by us: self-authored local + hosted-stamped server.
        assert!(reconcile_ok(
            &rev("l", "Alice <a@x>", "x", 1),
            &rev("s", "alice <>", "x", 999),
            hosted,
            me,
        ));
        // (i) fails when the server author is a DIFFERENT hosted user.
        assert!(!reconcile_ok(
            &rev("l", "Alice <a@x>", "x", 1),
            &rev("s", "bob <>", "x", 1),
            hosted,
            me,
        ));
        // (ii) pulled before: attribution + timestamp copied verbatim.
        assert!(reconcile_ok(
            &rev("l", "bob <>", "x", 7),
            &rev("s", "bob <>", "x", 7),
            hosted,
            me,
        ));
        // (ii) fails on a timestamp mismatch.
        assert!(!reconcile_ok(
            &rev("l", "bob <>", "x", 7),
            &rev("s", "bob <>", "x", 8),
            hosted,
            me,
        ));
    }

    // P1-3: supersedes_annotation_id is a LOCAL id and must resolve to the SERVER
    // id through the mirror (both the pushed case, local != server, and the
    // pulled case, local == server).
    #[test]
    fn supersede_resolves_local_to_server_through_mirror() {
        let _process_env_guard = crate::test_process_env::shared_blocking();
        let mut mirror = HostedContextMirror::default();
        // Pushed annotation: local uuid differs from the server id.
        get_or_create_entry(&mut mirror, "ns/repo", "local-old").server_id = "srv-old".into();
        // Pulled annotation: local id == server id.
        get_or_create_entry(&mut mirror, "ns/repo", "srv-pulled").server_id = "srv-pulled".into();

        assert_eq!(
            server_id_for_local(&mirror, "ns/repo", "local-old"),
            Some("srv-old".to_string()),
        );
        assert_eq!(
            server_id_for_local(&mirror, "ns/repo", "srv-pulled"),
            Some("srv-pulled".to_string()),
        );
        // An in-flight create (empty server id) does not resolve.
        get_or_create_entry(&mut mirror, "ns/repo", "in-flight").pending_create_op =
            Some("op".into());
        assert_eq!(server_id_for_local(&mirror, "ns/repo", "in-flight"), None);
    }

    #[test]
    fn context_without_creation_state_is_an_explicit_incomplete_result() {
        let _process_env_guard = crate::test_process_env::shared_blocking();
        let revision = AnnotationRevision {
            revision_id: "revision-without-state".into(),
            kind: AnnotationKind::Constraint,
            content: "preserve exact source".into(),
            tags: Vec::new(),
            attribution: "Original Author <author@test>".into(),
            created_at: 1_700_000_000,
            source_hash: None,
            created_at_state: None,
        };
        let error = native_context_anchor(
            &ContextTarget::file("lib.rs").unwrap(),
            &AnnotationScope::File,
            &revision,
        )
        .unwrap_err();
        assert!(
            format!("{error:#}").contains("native replication is incomplete"),
            "missing exact source state must not be synthesized: {error:#}"
        );
    }

    #[tokio::test]
    async fn clone_empty_bootstrap_rejects_projection_without_signed_operation() {
        let _process_env_guard = crate::test_process_env::shared().await;
        use crate::hosted_runtime::hosted::test_server::{ContextFixture, start_with_context};

        let temp = TempDir::new().unwrap();
        let repo = Repository::init(temp.path()).unwrap();
        let tree_id = repo
            .store()
            .put_tree(&objects::object::Tree::new())
            .unwrap();
        let state = State::new_snapshot(
            tree_id,
            Vec::new(),
            Attribution::human(Principal::new("Test", "test@example.com")),
        );
        repo.store().put_state(&state).unwrap();
        let against = state.id();
        assert_eq!(
            repo.head().unwrap(),
            None,
            "clone has not published HEAD yet"
        );

        let annotation_id = "server-context-from-observe".to_string();
        let fixture = ContextFixture {
            records: vec![api::heddle::api::v1alpha2::ContextRecord {
                r#ref: Some(api::heddle::api::v1alpha2::RecordRef {
                    id: annotation_id,
                    ..Default::default()
                }),
                content: "preserve this contract".to_string(),
                principal_id: "reviewer".to_string(),
                ..Default::default()
            }],
            ..ContextFixture::default()
        };
        let (mut client, server, fixture) = start_with_context(fixture).await;

        let error = pull_context(&repo, &mut client, "acme/widgets", Some(against))
            .await
            .unwrap_err();
        let observed = *fixture.list_requests.lock().unwrap();
        assert!(observed >= 1, "native pull must ObserveCollaboration");
        assert!(
            format!("{error:#}").contains("omitted their signed operations"),
            "projection-only context must be an explicit incomplete result: {error:#}"
        );

        client.close().await;
        server.await.unwrap();
    }

    fn seed_local_context_annotation(content: &str) -> (TempDir, Repository, Annotation) {
        let temp = TempDir::new().unwrap();
        let repo = crate::with_test_principal(Repository::init(temp.path()).unwrap()).unwrap();
        let tree_id = repo
            .store()
            .put_tree(&objects::object::Tree::new())
            .unwrap();
        let state = State::new_snapshot(
            tree_id,
            Vec::new(),
            Attribution::human(Principal::new("Test", "test@example.com")),
        );
        repo.store().put_state(&state).unwrap();
        repo.goto(&state.id()).unwrap();
        let head_state = repo.store().get_state(&state.id()).unwrap().unwrap();
        let target = ContextTarget::file("lib.rs").unwrap();
        let user_config = config::UserConfig::load_default().unwrap_or_default();
        let self_attribution = crate::attribution::resolve_attribution(&repo, &user_config)
            .unwrap()
            .to_string();
        let annotation = Annotation::new(
            AnnotationScope::File,
            AnnotationKind::Constraint,
            content.to_string(),
            vec![],
            self_attribution,
            1_700_000_000,
            None,
            Some(state.id()),
        );
        let root = repo
            .set_context_blob(None, &target, &ContextBlob::new(vec![annotation.clone()]))
            .unwrap();
        put_context_attachment(&repo, &head_state, Some(root)).unwrap();
        (temp, repo, annotation)
    }

    #[tokio::test]
    async fn push_context_clears_create_nonce_after_applied_when_history_is_empty() {
        let _process_env_guard = crate::test_process_env::shared().await;
        let (_temp, repo, annotation) = seed_local_context_annotation("do not remove");
        let (client, server) = crate::hosted_runtime::hosted::test_server::start().await;
        let warnings = std::sync::Arc::new(objects::CollectingWarnings::default());
        let mut client = client.with_warning_sink(warnings.clone());

        let pushed = push_context(&repo, &mut client, "acme/widgets", "main")
            .await
            .unwrap();
        assert_eq!(
            pushed, 1,
            "Applied PutContext must adopt even when Observe history is empty"
        );
        let mirror = load_mirror(repo.heddle_dir()).unwrap();
        let entry = &mirror.repos["acme/widgets"].annotations[0];
        assert!(
            entry.pending_create_op.is_none(),
            "pending nonce must clear after Applied so a retry cannot Dedup Conflict"
        );
        assert_eq!(entry.server_id, annotation.annotation_id);
        assert!(
            warnings.warnings().is_empty(),
            "empty history after Applied is not a push failure: {:?}",
            warnings.warnings()
        );

        // Second push with no new collab must not replay PutContext.
        assert_eq!(
            push_context(&repo, &mut client, "acme/widgets", "main")
                .await
                .unwrap(),
            0
        );
        assert!(warnings.warnings().is_empty());

        client.close().await;
        server.await.unwrap();
    }

    #[tokio::test]
    async fn native_push_pull_preserves_all_context_anchors_provenance_and_edit_scope() {
        let _process_env_guard = crate::test_process_env::shared().await;
        fn assert_canonical_anchor(authored: &CollaborationAnchor, signed: &SignedRecord) {
            let operation = thread_api::collaboration::verify(signed).unwrap();
            let objects::object::thread_replication::ThreadOperationBody::Context(bytes) =
                operation.body
            else {
                panic!("context")
            };
            let context = objects::object::ContextRevision::decode(&bytes).unwrap();
            match (authored, &context.anchor) {
                (CollaborationAnchor::Repository, CollaborationAnchor::Repository) => {}
                (
                    CollaborationAnchor::Path { state_id, path },
                    CollaborationAnchor::Source { source },
                ) => {
                    assert_eq!(
                        source.revision,
                        CollaborationRevision::State {
                            state_id: *state_id
                        }
                    );
                    assert_eq!(&source.path, path);
                    assert!(source.symbol_id.is_empty());
                    assert_eq!((source.start_line, source.end_line), (None, None));
                }
                (
                    CollaborationAnchor::Symbol {
                        state_id,
                        path,
                        symbol,
                    },
                    CollaborationAnchor::Source { source },
                ) => {
                    assert_eq!(
                        source.revision,
                        CollaborationRevision::State {
                            state_id: *state_id
                        }
                    );
                    assert_eq!(&source.path, path);
                    assert_eq!(&source.symbol_id, symbol);
                }
                (
                    CollaborationAnchor::Source { source: authored },
                    CollaborationAnchor::Source { source: actual },
                ) => assert_eq!(actual, authored),
                (authored, actual) => panic!("native anchor lost {authored:?}: {actual:?}"),
            }
        }

        let source_dir = TempDir::new().unwrap();
        let source =
            crate::with_test_principal(Repository::init_default(source_dir.path()).unwrap())
                .unwrap();
        std::fs::write(source_dir.path().join("lib.rs"), "pub fn run() {}\n").unwrap();
        let state = source
            .snapshot_with_attribution(
                Some("seed".into()),
                None,
                Attribution::human(Principal::new("Original Author", "author@test")),
            )
            .unwrap()
            .id();
        let anchors = vec![
            (
                CollaborationAnchor::Path {
                    state_id: state,
                    path: "lib.rs".into(),
                },
                AnnotationScope::File,
            ),
            (
                CollaborationAnchor::Source {
                    source: CollaborationSourceAnchor {
                        revision: CollaborationRevision::State { state_id: state },
                        path: "lib.rs".into(),
                        symbol_id: String::new(),
                        start_line: Some(1),
                        end_line: Some(1),
                        target: None,
                    },
                },
                AnnotationScope::Lines(1, 1),
            ),
            (
                CollaborationAnchor::Symbol {
                    state_id: state,
                    path: "lib.rs".into(),
                    symbol: "run".into(),
                },
                AnnotationScope::Symbol {
                    name: "run".into(),
                    resolved_lines: None,
                },
            ),
            (CollaborationAnchor::Repository, AnnotationScope::File),
        ];
        let (mut client, server) = crate::hosted_runtime::hosted::test_server::start().await;
        let mut expected = Vec::new();
        let mut published_signed = Vec::new();
        for (index, (anchor, scope)) in anchors.into_iter().enumerate() {
            let annotation_id = uuid::Uuid::now_v7().to_string();
            let revision_id = uuid::Uuid::now_v7().to_string();
            let kind = match index {
                0 => AnnotationKind::Constraint,
                1 => AnnotationKind::Invariant,
                _ => AnnotationKind::Rationale,
            };
            let provenance = ContextProvenance {
                revision_id: revision_id.clone(),
                kind,
                attribution: "Original Author <author@test> (via codex/model)".into(),
                source_hash: Some(ContentHash::from_bytes([index as u8 + 1; 32])),
                created_at_state: Some(state),
            };
            let occurred_at_ms = (1_700_000_000 + index as i64) * 1000;
            let (signed, expected_version) = client
                .prepare_native_context_record(
                    "acme/widgets",
                    Some("feature/provenance"),
                    &annotation_id,
                    anchor.clone(),
                    &format!("context {index}"),
                    vec![format!("tag-{index}").into()],
                    provenance.clone(),
                    occurred_at_ms,
                    None,
                )
                .await
                .unwrap();
            assert_canonical_anchor(&anchor, &signed);
            published_signed.push(signed.encode_to_vec());
            client
                .publish_native_context_record(
                    signed,
                    expected_version,
                    uuid::Uuid::now_v7().to_string(),
                )
                .await
                .unwrap();
            expected.push((
                annotation_id.clone(),
                revision_id,
                scope,
                kind,
                occurred_at_ms / 1000,
                provenance,
            ));

            if index == 0 {
                let edit_revision_id = uuid::Uuid::now_v7().to_string();
                let mut edit_provenance = expected[0].5.clone();
                edit_provenance.revision_id = edit_revision_id.clone();
                let (signed, expected_version) = client
                    .prepare_native_context_record(
                        "acme/widgets",
                        Some("feature/provenance"),
                        &annotation_id,
                        anchor.clone(),
                        "context edited",
                        vec!["tag-edited".into()],
                        edit_provenance,
                        occurred_at_ms + 1000,
                        None,
                    )
                    .await
                    .unwrap();
                assert_canonical_anchor(&anchor, &signed);
                published_signed.push(signed.encode_to_vec());
                client
                    .publish_native_context_record(
                        signed,
                        expected_version,
                        uuid::Uuid::now_v7().to_string(),
                    )
                    .await
                    .unwrap();
            }
        }

        let destination_dir = TempDir::new().unwrap();
        let destination =
            crate::with_test_principal(Repository::init_default(destination_dir.path()).unwrap())
                .unwrap();
        let destination_state = destination.head().unwrap().unwrap();
        assert_eq!(
            pull_context(
                &destination,
                &mut client,
                "acme/widgets",
                Some(destination_state),
            )
            .await
            .unwrap(),
            4
        );
        let head = destination
            .store()
            .get_state(&destination_state)
            .unwrap()
            .unwrap();
        let root = context_root_for_state(&destination, &head)
            .unwrap()
            .unwrap();
        let entries = destination.list_context_entries(&root, None).unwrap();
        let annotations: std::collections::HashMap<_, _> = entries
            .into_iter()
            .flat_map(|entry| entry.blob.annotations)
            .map(|annotation| (annotation.annotation_id.clone(), annotation))
            .collect();
        let edited_id = expected[0].0.clone();
        for (id, revision_id, scope, kind, created_at, provenance) in expected {
            let annotation = annotations.get(&id).unwrap();
            assert_eq!(annotation.scope, scope);
            let first = annotation.revisions.first().unwrap();
            assert_eq!(first.revision_id, revision_id);
            assert_eq!(first.kind, kind);
            assert_eq!(first.attribution, provenance.attribution);
            assert_eq!(first.source_hash, provenance.source_hash);
            assert_eq!(first.created_at_state, provenance.created_at_state);
            assert_eq!(first.created_at, created_at);
            if id == edited_id {
                assert_eq!(annotation.revisions.len(), 2);
                assert_eq!(
                    annotation.current_revision().unwrap().content,
                    "context edited"
                );
                assert_eq!(annotation.scope, AnnotationScope::File);
            }
        }
        let mirror = load_mirror(destination.heddle_dir()).unwrap();
        let stored: Vec<_> = mirror.repos["acme/widgets"]
            .annotations
            .iter()
            .flat_map(|entry| entry.native_operations.iter())
            .collect();
        assert_eq!(
            stored.len(),
            5,
            "four opens plus one edit stay signed locally"
        );
        let mut pulled_signed: Vec<Vec<u8>> = stored
            .iter()
            .map(|operation| operation.signed_record.clone())
            .collect();
        pulled_signed.sort();
        published_signed.sort();
        assert_eq!(
            pulled_signed, published_signed,
            "pull must retain the byte-identical context signed originals"
        );
        for prepared in stored {
            let signed = SignedRecord::decode(prepared.signed_record.as_slice()).unwrap();
            let verified = thread_api::collaboration::verify(&signed).unwrap();
            let objects::object::thread_replication::ThreadOperationBody::Context(bytes) =
                verified.body
            else {
                panic!("context")
            };
            let context = objects::object::ContextRevision::decode(&bytes).unwrap();
            assert!(context.metadata.scope.thread.is_some());
            assert!(!context.metadata.actor.principal_id.is_nil());
        }

        client.close().await;
        server.await.unwrap();
    }

    #[test]
    fn operation_id_conflict_matches_weft_dedup_wording() {
        let _process_env_guard = crate::test_process_env::shared_blocking();
        assert!(is_operation_id_conflict(
            &wire::ProtocolError::InvalidState("operation ID names another command".into())
        ));
        assert!(is_operation_id_conflict(
            &wire::ProtocolError::InvalidState("operation ID names a different command".into())
        ));
        assert!(is_operation_id_conflict(
            &wire::ProtocolError::RemoteFailure {
                code: wire::RemoteFailureCode::FailedPrecondition,
                message: "client_operation_id reused with a different request body".into(),
                details: Vec::new(),
            }
        ));
        assert!(!is_operation_id_conflict(
            &wire::ProtocolError::InvalidState("hosted annotation has no revisions".into())
        ));
    }
}
