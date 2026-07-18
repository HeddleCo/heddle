// SPDX-License-Identifier: Apache-2.0
//! Hosted context (annotation) sync bridge.
//!
//! Local context annotations live in per-state `Context` attachments
//! (`ContextBlob`s keyed by target). The hosted weft `RepositoryService` speaks
//! an id-keyed annotation model with the SAME `Annotation` shape, mutated
//! through the caller-authenticated `SetContext`/`ReviseContext`/
//! `SupersedeContext` RPCs and read back through `ListContext`/
//! `GetContextHistory`. This module bridges the two, mirroring
//! [`crate::client::discussion_sync`].
//!
//! ## Identity: annotation id AND per-revision id links
//!
//! A context annotation carries a globally-unique `annotation_id`, and each
//! revision a `revision_id`. When the server ships an annotation back on a pull,
//! the pack carries its `Annotation` verbatim, so a pulled annotation's local id
//! (and each pulled revision's id) IS the server id. A locally-authored
//! annotation/revision is minted with a fresh uuid, so its id differs from the
//! server id the RPC assigns. The mirror map records BOTH:
//! `local_annotation_id ↔ server_annotation_id` and, per annotation, a set of
//! `local_revision_id ↔ server_revision_id` links.
//!
//! A prefix COUNT of "revisions synced" is wrong: with two clones concurrently
//! revising one annotation, the linear server list interleaves their revisions,
//! so a count both drops one author's revision and duplicates the other's on the
//! next pull. Reconciliation is therefore by revision-id set difference in server
//! order, and pull rebuilds the local revision list to match server order so the
//! "current" revision agrees across clones.
//!
//! ## Reconciliation is author-aware, never body-alone
//!
//! Where an id link is missing (a lost/rebuilt mirror, or recovering a
//! server-minted id), an unlinked server revision is matched to an unlinked
//! local revision only under an explicit author rule — never body equality
//! alone, which would cross-link two different authors' identical bodies
//! (`"lgtm"`) and silently drop one:
//! * (i) a revision WE pushed — the local revision is self-authored (its
//!   attribution equals our local attribution) AND the server stamped it with
//!   our own hosted username (`"{username} <>"`); or
//! * (ii) a revision we previously PULLED — the local attribution + `created_at`
//!   exactly equal the server revision's. Minted-annotation-id recovery after
//!   `SetContext` uses the same author rule (weft returns only a count, not the
//!   id): it adopts the annotation at the target whose attribution is our hosted
//!   username, whose content matches, and whose id is not already linked.
//!
//! ## Idempotent, crash-safe create
//!
//! `SetContext` mints the id server-side and does not return it. Before the RPC
//! the mirror records a `pending_create_op` (write-ahead, persisted to disk) used
//! as the `client_operation_id`, so a retry after a crash replays (weft dedup)
//! instead of duplicating, and recovery re-adopts the already-created annotation
//! by author+content among ids NOT already linked — closing the "could not
//! recover minted id" wedge.
//!
//! ## Supersession chains through the mirror
//!
//! `supersedes_annotation_id` is a LOCAL id; it is resolved to the SERVER id via
//! the mirror before calling `SupersedeContext`, so superseding a previously
//! pushed annotation marks the right server annotation superseded (rather than
//! degrading to a plain create that leaves the old one Active on every clone).
//!
//! ## weft#638 limit (degrade gracefully, don't fix here)
//!
//! Each RPC advances the server head (context lives per-state); a no-HEAD repo is
//! skipped. The mirror is saved after every annotation and on the error path,
//! collect-and-continue, so one wedged annotation never aborts the rest or
//! orphans a durable write. Scope: annotations only (`rm` is local-only).

#![cfg(feature = "client")]

use std::{
    collections::{BTreeMap, HashSet},
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use api::heddle::api::v1alpha1::{
    AnnotationScope as ProtoScope, ContextAnnotation, ContextAnnotationKind, ContextAnnotationStatus,
    LineRange, SymbolScope, annotation_scope::Scope,
};
use objects::fs_atomic::write_file_atomic;
use objects::object::{
    Annotation, AnnotationKind, AnnotationRevision, AnnotationScope, AnnotationStatus, ContentHash,
    ContextBlob, ContextTarget, State, StateId,
};
use objects::store::ObjectStore;
use repo::Repository;
use serde::{Deserialize, Serialize};

use crate::cli::commands::context::{context_root_for_state, put_context_attachment};
use crate::client::HostedGrpcClient;

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

fn server_id_is_linked(mirror: &HostedContextMirror, repo_path: &str, server_id: &str) -> bool {
    mirror.repos.get(repo_path).is_some_and(|repo_mirror| {
        repo_mirror
            .annotations
            .iter()
            .any(|entry| entry.server_id == server_id)
    })
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

// =========================================================================
// scope / kind converters
// =========================================================================

fn scope_to_proto(scope: &AnnotationScope) -> ProtoScope {
    let inner = match scope {
        AnnotationScope::File => Scope::File(true),
        AnnotationScope::Symbol {
            name,
            resolved_lines,
        } => Scope::Symbol(SymbolScope {
            name: name.clone(),
            resolved_start: resolved_lines.map(|(start, _)| start),
            resolved_end: resolved_lines.map(|(_, end)| end),
        }),
        AnnotationScope::Lines(start, end) => Scope::Lines(LineRange {
            start: *start,
            end: *end,
        }),
    };
    ProtoScope { scope: Some(inner) }
}

fn scope_from_proto(scope: Option<&ProtoScope>) -> AnnotationScope {
    match scope.and_then(|s| s.scope.as_ref()) {
        Some(Scope::File(_)) | None => AnnotationScope::File,
        Some(Scope::Symbol(symbol)) => AnnotationScope::Symbol {
            name: symbol.name.clone(),
            resolved_lines: match (symbol.resolved_start, symbol.resolved_end) {
                (Some(start), Some(end)) => Some((start, end)),
                _ => None,
            },
        },
        Some(Scope::Lines(range)) => AnnotationScope::Lines(range.start, range.end),
    }
}

/// Scope identity ignoring `resolved_lines` (the server may resolve a symbol's
/// lines even when the local scope has none), for matching recovered annotations.
fn scope_ident(scope: &AnnotationScope) -> String {
    match scope {
        AnnotationScope::File => "file".to_string(),
        AnnotationScope::Symbol { name, .. } => format!("symbol:{name}"),
        AnnotationScope::Lines(start, end) => format!("lines:{start}-{end}"),
    }
}

fn kind_to_proto(kind: AnnotationKind) -> ContextAnnotationKind {
    match kind {
        AnnotationKind::Constraint => ContextAnnotationKind::Constraint,
        AnnotationKind::Invariant => ContextAnnotationKind::Invariant,
        AnnotationKind::Rationale => ContextAnnotationKind::Rationale,
    }
}

fn kind_from_proto(kind: i32) -> AnnotationKind {
    match ContextAnnotationKind::try_from(kind).unwrap_or(ContextAnnotationKind::Rationale) {
        ContextAnnotationKind::Constraint => AnnotationKind::Constraint,
        ContextAnnotationKind::Invariant => AnnotationKind::Invariant,
        ContextAnnotationKind::Rationale | ContextAnnotationKind::Unspecified => {
            AnnotationKind::Rationale
        }
    }
}

fn status_from_proto(status: i32) -> AnnotationStatus {
    match ContextAnnotationStatus::try_from(status).unwrap_or(ContextAnnotationStatus::Active) {
        ContextAnnotationStatus::Superseded => AnnotationStatus::Superseded,
        _ => AnnotationStatus::Active,
    }
}

fn target_operands(target: &ContextTarget) -> (String, Option<String>) {
    match target {
        ContextTarget::File { path } => (path.clone(), None),
        ContextTarget::State { state_id } => (String::new(), Some(state_id.to_string_full())),
    }
}

fn content_hash_from_bytes(bytes: &Option<Vec<u8>>) -> Option<ContentHash> {
    bytes
        .as_ref()
        .and_then(|raw| <[u8; 32]>::try_from(raw.as_slice()).ok())
        .map(ContentHash::from_bytes)
}

fn state_id_from_proto(id: Option<&api::heddle::api::v1alpha1::StateId>) -> Option<StateId> {
    id.and_then(|value| StateId::try_from_slice(&value.value).ok())
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
    client: &mut HostedGrpcClient,
    repo_path: &str,
) -> Result<usize> {
    let Some(head_id) = repo.head().context("resolve repository head")? else {
        return Ok(0);
    };
    let Some(head_state) = repo.store().get_state(&head_id).context("load head state")? else {
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

    let user_config = crate::config::UserConfig::load_default().unwrap_or_default();
    let self_local_attr = crate::cli::commands::snapshot::resolve_attribution(repo, &user_config)
        .ok()
        .map(|attribution| attribution.to_string());
    let username = client.authenticated_username();

    let server_ann_ids: HashSet<String> = list_server_annotations(client, repo_path)
        .await?
        .into_iter()
        .map(|annotation| annotation.id)
        .collect();

    let heddle_dir = repo.heddle_dir().to_path_buf();
    let mut mirror = load_mirror(&heddle_dir)?;
    let mut synced = 0usize;
    for entry in &entries {
        for annotation in &entry.blob.annotations {
            let result = push_one(
                client,
                repo_path,
                &heddle_dir,
                &entry.target,
                annotation,
                self_local_attr.as_deref(),
                username.as_deref(),
                &server_ann_ids,
                &mut mirror,
            )
            .await;
            save_mirror(&heddle_dir, &mirror)?;
            match result {
                Ok(true) => synced += 1,
                Ok(false) => {}
                Err(error) => {
                    eprintln!(
                        "{} hosted context {}: {error:#}",
                        crate::cli::style::warn_marker(),
                        annotation.annotation_id
                    );
                }
            }
        }
    }
    Ok(synced)
}

#[allow(clippy::too_many_arguments)]
async fn push_one(
    client: &mut HostedGrpcClient,
    repo_path: &str,
    heddle_dir: &Path,
    target: &ContextTarget,
    annotation: &Annotation,
    self_local_attr: Option<&str>,
    username: Option<&str>,
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
            eprintln!(
                "{} hosted context {}: not attributed to the local principal; left unpublished",
                crate::cli::style::warn_marker(),
                annotation.annotation_id
            );
            return Ok(false);
        }
        // Resolve the superseded LOCAL id → SERVER id through the mirror (P1-3).
        let superseded_server = annotation.supersedes_annotation_id.as_ref().and_then(|local| {
            server_id_for_local(mirror, repo_path, local)
                .or_else(|| server_ann_ids.contains(local).then(|| local.clone()))
        });

        // Write-ahead: persist a create nonce to DISK before the RPC so a
        // crash-retry replays with the same client_operation_id (P2-5).
        let op_id = {
            let entry = get_or_create_entry(mirror, repo_path, &annotation.annotation_id);
            if entry.pending_create_op.is_none() {
                entry.pending_create_op = Some(uuid::Uuid::new_v4().to_string());
            }
            entry.pending_create_op.clone().expect("just set")
        };
        save_mirror(heddle_dir, mirror)?;

        let (sid, first_server_rev) = create_on_server(
            client,
            repo_path,
            target,
            annotation,
            superseded_server.as_deref(),
            &op_id,
            username,
            mirror,
        )
        .await?;

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
        &server_id,
        annotation,
        self_local_attr,
        username,
        mirror,
    )
    .await?;

    Ok(created || pushed > 0)
}

/// Create a fresh annotation server-side, returning `(server_annotation_id,
/// first_server_revision_id)`.
#[allow(clippy::too_many_arguments)]
async fn create_on_server(
    client: &mut HostedGrpcClient,
    repo_path: &str,
    target: &ContextTarget,
    annotation: &Annotation,
    superseded_server: Option<&str>,
    op_id: &str,
    username: Option<&str>,
    mirror: &HostedContextMirror,
) -> Result<(String, String)> {
    let first = annotation
        .revisions
        .first()
        .context("annotation has no revisions")?;
    let (path, target_state_id) = target_operands(target);

    let server_id = if let Some(superseded) = superseded_server {
        let response = client
            .supersede_context(
                repo_path,
                superseded,
                if path.is_empty() {
                    None
                } else {
                    Some(path.as_str())
                },
                target_state_id.as_deref(),
                scope_to_proto(&annotation.scope),
                first.tags.clone(),
                &first.content,
                None,
                None,
                kind_to_proto(first.kind),
                op_id.to_string(),
            )
            .await
            .with_context(|| format!("supersede hosted annotation {superseded}"))?;
        response.new_annotation_id
    } else {
        client
            .set_context(
                repo_path,
                &path,
                target_state_id.as_deref(),
                scope_to_proto(&annotation.scope),
                kind_to_proto(first.kind),
                first.tags.clone(),
                &first.content,
                None,
                None,
                op_id.to_string(),
            )
            .await
            .with_context(|| format!("set hosted context for {}", annotation.annotation_id))?;

        // Recover the minted id author-aware (weft returns only a count): the
        // annotation at the target stamped with OUR hosted username, matching
        // content + scope, whose id is not already linked (re-link safe).
        let hosted = hosted_attribution(username);
        list_server_targets(client, repo_path)
            .await?
            .into_iter()
            .rev()
            .find(|(candidate_target, candidate)| {
                candidate_target == target
                    && Some(candidate.attribution.as_str()) == hosted.as_deref()
                    && candidate.content == first.content
                    && scope_ident(&scope_from_proto(candidate.scope.as_ref()))
                        == scope_ident(&annotation.scope)
                    && !server_id_is_linked(mirror, repo_path, &candidate.id)
            })
            .map(|(_, candidate)| candidate.id)
            .context("could not recover the minted annotation id (author + content)")?
    };

    let first_server_rev = first_server_revision_id(client, repo_path, &server_id).await?;
    Ok((server_id, first_server_rev))
}

/// Forward local revisions the server does not yet hold. Returns the count
/// actually pushed. Author-aware recovery links each minted server revision id.
async fn sync_revisions_push(
    client: &mut HostedGrpcClient,
    repo_path: &str,
    server_id: &str,
    annotation: &Annotation,
    self_local_attr: Option<&str>,
    username: Option<&str>,
    mirror: &mut HostedContextMirror,
) -> Result<usize> {
    let hosted = hosted_attribution(username);
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
        .map(|entry| entry.revision_links.iter().map(|l| l.local.clone()).collect())
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
        // revision not on the server is anomalous — skip rather than mis-author).
        if self_local_attr != Some(revision.attribution.as_str()) {
            eprintln!(
                "{} hosted context {}: unlinked revision not attributed to the local principal; left unpublished",
                crate::cli::style::warn_marker(),
                annotation.annotation_id
            );
            continue;
        }
        client
            .revise_context(
                repo_path,
                server_id,
                &revision.content,
                revision.tags.clone(),
                None,
                None,
                kind_to_proto(revision.kind),
                revise_op_id(repo_path, server_id, &revision.revision_id),
            )
            .await
            .with_context(|| format!("revise hosted annotation {server_id}"))?;

        // Recover the minted revision id: the newly-appended server revision
        // stamped with our hosted username + our content, not seen before.
        let refreshed = fetch_history(client, repo_path, server_id).await?;
        let minted = refreshed.iter().rev().find(|candidate| {
            !server_rev_ids.contains(&candidate.revision_id)
                && Some(candidate.attribution.as_str()) == hosted.as_deref()
                && candidate.content == revision.content
        });
        match minted {
            Some(candidate) => {
                add_revision_link(
                    mirror,
                    repo_path,
                    &annotation.annotation_id,
                    revision.revision_id.clone(),
                    candidate.revision_id.clone(),
                );
                server_rev_ids.insert(candidate.revision_id.clone());
                pushed += 1;
            }
            None => {
                eprintln!(
                    "{} hosted context {}: could not recover the minted revision id",
                    crate::cli::style::warn_marker(),
                    annotation.annotation_id
                );
            }
        }
    }
    Ok(pushed)
}

/// Oldest server revision id for an annotation (the create revision).
async fn first_server_revision_id(
    client: &mut HostedGrpcClient,
    repo_path: &str,
    server_id: &str,
) -> Result<String> {
    let revisions = fetch_history(client, repo_path, server_id).await?;
    revisions
        .into_iter()
        .next()
        .map(|revision| revision.revision_id)
        .context("hosted annotation has no revisions")
}

// =========================================================================
// Pull
// =========================================================================

/// Fetch hosted annotations for the head and reconcile them into the local
/// `Context` attachment, rebuilding revision order to match the server.
pub async fn pull_context(
    repo: &Repository,
    client: &mut HostedGrpcClient,
    repo_path: &str,
) -> Result<usize> {
    let Some(head_id) = repo.head().context("resolve repository head")? else {
        return Ok(0);
    };
    let Some(head_state) = repo.store().get_state(&head_id).context("load head state")? else {
        return Ok(0);
    };

    let server = list_server_targets(client, repo_path).await?;
    if server.is_empty() {
        return Ok(0);
    }

    let user_config = crate::config::UserConfig::load_default().unwrap_or_default();
    let self_local_attr = crate::cli::commands::snapshot::resolve_attribution(repo, &user_config)
        .ok()
        .map(|attribution| attribution.to_string());
    let username = client.authenticated_username();

    let heddle_dir = repo.heddle_dir().to_path_buf();
    let mut mirror = load_mirror(&heddle_dir)?;
    let mut changed = 0usize;
    for (target, annotation) in server {
        let result = pull_one(
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
                eprintln!(
                    "{} hosted context {}: {error:#}",
                    crate::cli::style::warn_marker(),
                    annotation.id
                );
            }
        }
    }
    Ok(changed)
}

#[allow(clippy::too_many_arguments)]
async fn pull_one(
    repo: &Repository,
    client: &mut HostedGrpcClient,
    repo_path: &str,
    head_state: &State,
    target: &ContextTarget,
    server: &ContextAnnotation,
    self_local_attr: Option<&str>,
    username: Option<&str>,
    mirror: &mut HostedContextMirror,
) -> Result<bool> {
    let local_id =
        local_id_for_server(mirror, repo_path, &server.id).unwrap_or_else(|| server.id.clone());
    let server_revs = fetch_history(client, repo_path, &server.id).await?;

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
        .map(|index| mirror.repos[repo_path].annotations[index].revision_links.clone())
        .unwrap_or_default();

    let (new_revisions, new_links) = reconcile_revisions_pull(
        &existing_revisions,
        &server_revs,
        &existing_links,
        self_local_attr,
        username,
    );

    {
        let entry = get_or_create_entry(mirror, repo_path, &local_id);
        entry.server_id = server.id.clone();
        entry.revision_links = new_links;
    }

    let new_status = status_from_proto(server.status);
    let changed = match existing_index {
        Some(index) => {
            let annotation = &mut blob.annotations[index];
            let differs = annotation.revisions != new_revisions
                || annotation.status != new_status
                || annotation.supersedes_annotation_id != server.supersedes_annotation_id
                || annotation.supersedes_rewrite_pct != server.supersedes_rewrite_pct;
            annotation.revisions = new_revisions;
            annotation.status = new_status;
            annotation.supersedes_annotation_id = server.supersedes_annotation_id.clone();
            annotation.supersedes_rewrite_pct = server.supersedes_rewrite_pct;
            differs
        }
        None => {
            blob.annotations.push(Annotation {
                annotation_id: local_id.clone(),
                scope: scope_from_proto(server.scope.as_ref()),
                status: new_status,
                revisions: new_revisions,
                supersedes_annotation_id: server.supersedes_annotation_id.clone(),
                supersedes_rewrite_pct: server.supersedes_rewrite_pct,
                visibility: objects::object::VisibilityTier::default(),
                resolved_from_discussion: None,
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
        if let Some(local_rev_id) = link_by_server.get(server_rev.revision_id.as_str()) {
            if let Some(local) = existing
                .iter()
                .find(|rev| rev.revision_id == *local_rev_id && !consumed.contains(&rev.revision_id))
            {
                consumed.insert(local.revision_id.clone());
                new_revisions.push(local.clone());
                new_links.push(RevisionLink {
                    local: local.revision_id.clone(),
                    server: server_rev.revision_id.clone(),
                });
                continue;
            }
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
    client: &mut HostedGrpcClient,
    repo_path: &str,
) -> Result<Vec<ContextAnnotation>> {
    Ok(list_server_targets(client, repo_path)
        .await?
        .into_iter()
        .map(|(_, annotation)| annotation)
        .collect())
}

async fn list_server_targets(
    client: &mut HostedGrpcClient,
    repo_path: &str,
) -> Result<Vec<(ContextTarget, ContextAnnotation)>> {
    let response = client
        .list_context(repo_path, None, None, None)
        .await
        .context("list hosted context")?;
    let mut out = Vec::new();
    for file in response.files {
        let Ok(target) = ContextTarget::file(&file.path) else {
            continue;
        };
        for annotation in file.annotations {
            out.push((target.clone(), annotation));
        }
    }
    for state in response.states {
        let Some(state_id) = state_id_from_proto(state.state_id.as_ref()) else {
            continue;
        };
        let target = ContextTarget::state(state_id);
        for annotation in state.annotations {
            out.push((target.clone(), annotation));
        }
    }
    Ok(out)
}

/// `GetContextHistory` returns revisions newest-first; local storage is
/// oldest-first, so reverse.
async fn fetch_history(
    client: &mut HostedGrpcClient,
    repo_path: &str,
    annotation_id: &str,
) -> Result<Vec<AnnotationRevision>> {
    let history = client
        .get_context_history(repo_path, None, annotation_id)
        .await
        .with_context(|| format!("fetch hosted annotation history {annotation_id}"))?;
    let mut revisions: Vec<AnnotationRevision> = history
        .revisions
        .into_iter()
        .map(|revision| AnnotationRevision {
            revision_id: revision.revision_id,
            kind: kind_from_proto(revision.kind),
            content: revision.content,
            tags: revision.tags,
            attribution: revision.attribution,
            created_at: revision.created_at.map(|ts| ts.seconds).unwrap_or(0),
            source_hash: content_hash_from_bytes(&revision.source_hash),
            created_at_state: state_id_from_proto(revision.created_at_state.as_ref()),
        })
        .collect();
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

#[cfg(test)]
mod tests {
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

    #[test]
    fn scope_round_trips_through_proto() {
        for scope in [
            AnnotationScope::File,
            AnnotationScope::Symbol {
                name: "run".to_string(),
                resolved_lines: Some((3, 9)),
            },
            AnnotationScope::Lines(10, 20),
        ] {
            assert_eq!(scope_from_proto(Some(&scope_to_proto(&scope))), scope);
        }
    }

    #[test]
    fn kind_round_trips_through_proto() {
        for kind in [
            AnnotationKind::Constraint,
            AnnotationKind::Invariant,
            AnnotationKind::Rationale,
        ] {
            assert_eq!(kind_from_proto(kind_to_proto(kind) as i32), kind);
        }
    }

    #[test]
    fn scope_ident_ignores_resolved_lines() {
        let a = AnnotationScope::Symbol {
            name: "greet".to_string(),
            resolved_lines: Some((1, 2)),
        };
        let b = AnnotationScope::Symbol {
            name: "greet".to_string(),
            resolved_lines: None,
        };
        assert_eq!(scope_ident(&a), scope_ident(&b));
    }

    // P1-2: two clones concurrently revise one annotation. Alice already holds
    // r1 (pulled) and her own r3 (linked to server sA3); Bob's r2 arrives in the
    // middle. Pull must yield all three in SERVER order, keep Alice's local id
    // for sA3, materialize Bob's, and neither drop nor duplicate anything.
    #[test]
    fn pull_two_author_revisions_no_loss_no_dup_server_order() {
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
        let ids: Vec<&str> = new_revisions.iter().map(|r| r.revision_id.as_str()).collect();
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
        let existing = vec![rev("rLocal", "Alice <a@x>", "lgtm", 5)];
        let server_revs = vec![rev("sBob", "bob <>", "lgtm", 9)];
        let (new_revisions, _links) = reconcile_revisions_pull(
            &existing,
            &server_revs,
            &[],
            Some("Alice <a@x>"),
            Some("alice"),
        );
        let ids: HashSet<&str> = new_revisions.iter().map(|r| r.revision_id.as_str()).collect();
        assert_eq!(new_revisions.len(), 2, "both must survive, none collapsed");
        assert!(ids.contains("sBob"), "Bob's revision materialized distinctly");
        assert!(ids.contains("rLocal"), "Alice's local revision preserved");
    }

    // A revision WE pushed comes back stamped with our hosted username → links
    // (rule i), so pull does not duplicate it.
    #[test]
    fn pull_relinks_our_pushed_revision_after_lost_mirror() {
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
        get_or_create_entry(&mut mirror, "ns/repo", "in-flight").pending_create_op = Some("op".into());
        assert_eq!(server_id_for_local(&mirror, "ns/repo", "in-flight"), None);
    }

    // Minted-id recovery must not adopt a server id that is already linked to a
    // different local annotation (the re-link-safe / concurrent-writer guard).
    #[test]
    fn server_id_is_linked_detects_prior_links() {
        let mut mirror = HostedContextMirror::default();
        get_or_create_entry(&mut mirror, "ns/repo", "local-a").server_id = "srv-1".into();
        assert!(server_id_is_linked(&mirror, "ns/repo", "srv-1"));
        assert!(!server_id_is_linked(&mirror, "ns/repo", "srv-2"));
    }
}
