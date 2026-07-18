// SPDX-License-Identifier: Apache-2.0
//! Hosted context (annotation) sync bridge.
//!
//! Local context annotations live in per-state `Context` attachments
//! (`ContextBlob`s keyed by target). The hosted weft `RepositoryService` speaks
//! an id-keyed annotation model with the SAME `Annotation` shape, mutated
//! through the caller-authenticated `SetContext`/`ReviseContext`/
//! `SupersedeContext` RPCs and read back through `ListContext`/
//! `GetContextHistory`. This module bridges the two, mirroring
//! [`crate::client::discussion_sync`]:
//!
//! * **Push (write path):** after a successful `heddle push`, replay the
//!   annotations we authored to the server. #549 rejects `Context` attachments
//!   in the pack, so an annotation only reaches the server through these RPCs.
//! * **Pull/clone (read path):** after a successful clone/pull, `ListContext`
//!   the head and materialize any server annotation/revision we don't already
//!   hold into the local `Context` attachment so `context list` sees it.
//!
//! ## Identity is by the annotation's stable id — NOT ordinal counts
//!
//! Unlike discussion turns (whose only cross-side identity was a fragile
//! ordinal), a context annotation carries a globally-unique `annotation_id`.
//! Crucially, when the server ships an annotation back on a pull, the pack
//! carries the server's `Annotation` verbatim, so the **local** annotation id
//! IS the server id. That collapses the cross-side identity problem: a pulled
//! annotation and its server twin share one id, and the cross-author
//! identical-body hazard (two "lgtm" annotations from different authors) is
//! trivially safe — distinct server ids materialize as distinct local
//! annotations, so a body can never be misattributed.
//!
//! The mirror map (`.heddle/collaboration/hosted-context-mirror.json`) records,
//! per repo, `local_annotation_id ↔ server_annotation_id` plus a count of
//! revisions already synced (revisions are append-only and linear). A locally
//! authored annotation is created with a fresh uuid, so its local id differs
//! from the server id the RPC mints; the mirror is what maps the two so a
//! re-push revises instead of re-creating.
//!
//! ## Push is idempotent even with no mirror
//!
//! Before creating an annotation, push checks whether the server ALREADY holds
//! one with that id (the pulled/pack-delivered case, where local id == server
//! id). If so it links and revises rather than issuing a duplicate `SetContext`.
//! This makes a lost mirror self-heal instead of doubling annotations.
//!
//! ## Fail-closed self filter
//!
//! A fresh annotation is created on the server only when we can attribute it to
//! the local principal (`SetContext`/`SupersedeContext` stamp the server-side
//! attribution with our own hosted username). Revisions to an *already-hosted*
//! annotation are always forwarded — a revision is a local edit the server
//! re-attributes to the caller, exactly the collaborative-edit case.
//!
//! ## weft#638 / weft#640 limits (degrade gracefully, don't fix here)
//!
//! Each `SetContext`/`Revise`/`Supersede` advances the server head (context
//! lives per-state, no carry-forward), so a repo effectively tracks one state's
//! context; a no-HEAD repo is skipped. `SetContext` does not return the minted
//! id, so push discovers it by diffing `ListContext` before/after — precise for
//! the real multi-writer case but ambiguous between two identical annotations
//! authored in the same push (weft#640, same-user-identical edge). The mirror is
//! saved after every annotation and on the error path, collect-and-continue, so
//! one wedged annotation never aborts the rest or orphans a durable write.
//!
//! Scope: annotations only. `rm` is not mirrored (removal is local-only).

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

use crate::client::HostedGrpcClient;
use crate::cli::commands::context::{context_root_for_state, put_context_attachment};

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

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ContextMirrorEntry {
    /// Local `annotation_id`.
    local_id: String,
    /// Server-assigned `annotation_id` (== `local_id` for pulled annotations).
    server_id: String,
    /// Count of revisions known present on BOTH sides. Revisions are linear and
    /// append-only, so a prefix count is a faithful link.
    #[serde(default)]
    synced_revisions: usize,
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

// --- scope / kind converters (local <-> proto) ---

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

/// `(path, target_state_id)` operands for the RPCs, from a local target.
fn target_operands(target: &ContextTarget) -> (String, Option<String>) {
    match target {
        ContextTarget::File { path } => (path.clone(), None),
        ContextTarget::State { state_id } => (String::new(), Some(state_id.to_string_full())),
    }
}

fn target_from_annotated_file(path: &str) -> Option<ContextTarget> {
    ContextTarget::file(path).ok()
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

// =========================================================================
// Push
// =========================================================================

/// Publish local annotations we authored to the hosted `RepositoryService`.
/// Saves the mirror after each annotation and continues past a per-annotation
/// failure (warn-and-skip).
pub async fn push_context(
    repo: &Repository,
    client: &mut HostedGrpcClient,
    repo_path: &str,
) -> Result<usize> {
    let Some(head_id) = repo.head().context("resolve repository head")? else {
        // weft#638: no HEAD → no state to attach annotations against.
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
    let self_attribution =
        crate::cli::commands::snapshot::resolve_attribution(repo, &user_config)
            .ok()
            .map(|attribution| attribution.to_string());

    let mut mirror = load_mirror(repo.heddle_dir())?;
    // `server_counts`: id → revision_count the server currently holds (from the
    // initial ListContext). Lets the adopt path (a pulled annotation whose local
    // id == server id) forward only the revisions the server is actually missing,
    // even with no mirror row. `known_ids` additionally folds in mirror server
    // ids so a `SetContext`'s freshly-minted id is spotted as the one NOT already
    // known.
    let mut server_counts: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();
    for annotation in list_server_annotations(client, repo_path).await? {
        server_counts.insert(annotation.id, annotation.revision_count as usize);
    }
    let mut known_ids: HashSet<String> = server_counts.keys().cloned().collect();
    if let Some(repo_mirror) = mirror.repos.get(repo_path) {
        for entry in &repo_mirror.annotations {
            known_ids.insert(entry.server_id.clone());
        }
    }

    let mut synced = 0usize;
    for entry in &entries {
        for annotation in &entry.blob.annotations {
            let result = push_one(
                client,
                repo_path,
                &entry.target,
                annotation,
                self_attribution.as_deref(),
                &mut mirror,
                &server_counts,
                &mut known_ids,
            )
            .await;
            save_mirror(repo.heddle_dir(), &mirror)?;
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
    target: &ContextTarget,
    annotation: &Annotation,
    self_attribution: Option<&str>,
    mirror: &mut HostedContextMirror,
    server_counts: &std::collections::HashMap<String, usize>,
    known_ids: &mut HashSet<String>,
) -> Result<bool> {
    let repo_mirror = mirror.repos.entry(repo_path.to_string()).or_default();
    let linked = repo_mirror
        .annotations
        .iter()
        .position(|entry| entry.local_id == annotation.annotation_id);

    // Resolve the server id, how many revisions are already synced, and whether
    // this call created a new server annotation.
    let (server_id, mut synced_revisions, created) = match linked {
        Some(index) => (
            repo_mirror.annotations[index].server_id.clone(),
            repo_mirror.annotations[index].synced_revisions,
            false,
        ),
        None if server_counts.contains_key(&annotation.annotation_id) => {
            // Pulled / pack-delivered: local id IS the server id. Adopt it
            // without a duplicate create (idempotent even with no mirror row).
            // Sync from the server's actual revision count so any revision we
            // added locally after the pull is still forwarded.
            let synced = server_counts[&annotation.annotation_id];
            repo_mirror.annotations.push(ContextMirrorEntry {
                local_id: annotation.annotation_id.clone(),
                server_id: annotation.annotation_id.clone(),
                synced_revisions: synced,
            });
            (annotation.annotation_id.clone(), synced, false)
        }
        None => {
            // Genuinely new annotation → create it. Fail-closed self filter.
            let first = annotation
                .revisions
                .first()
                .context("annotation has no revisions")?;
            let is_self = self_attribution.is_some_and(|me| me == first.attribution);
            if !is_self {
                eprintln!(
                    "{} hosted context {}: not attributed to the local principal; left unpublished",
                    crate::cli::style::warn_marker(),
                    annotation.annotation_id
                );
                return Ok(false);
            }
            let server_id =
                create_on_server(client, repo_path, target, annotation, known_ids).await?;
            known_ids.insert(server_id.clone());
            let repo_mirror = mirror.repos.entry(repo_path.to_string()).or_default();
            repo_mirror.annotations.push(ContextMirrorEntry {
                local_id: annotation.annotation_id.clone(),
                server_id: server_id.clone(),
                synced_revisions: 1,
            });
            (server_id, 1, true)
        }
    };

    // Forward any revisions the server does not yet hold (linear append).
    // "Pushed" = we created the annotation OR forwarded at least one revision;
    // adopting an already-hosted annotation with nothing new is not a push.
    let mut pushed_any = created;
    while synced_revisions < annotation.revisions.len() {
        let revision = &annotation.revisions[synced_revisions];
        client
            .revise_context(
                repo_path,
                &server_id,
                &revision.content,
                revision.tags.clone(),
                None,
                None,
                kind_to_proto(revision.kind),
                revise_op_id(repo_path, &server_id, &revision.revision_id),
            )
            .await
            .with_context(|| format!("revise hosted annotation {server_id}"))?;
        synced_revisions += 1;
        pushed_any = true;
    }

    // Persist the (possibly advanced) revision count.
    if let Some(entry) = mirror
        .repos
        .get_mut(repo_path)
        .and_then(|repo_mirror| {
            repo_mirror
                .annotations
                .iter_mut()
                .find(|entry| entry.local_id == annotation.annotation_id)
        })
    {
        entry.synced_revisions = synced_revisions;
    }

    Ok(pushed_any)
}

/// Create a fresh annotation on the server and return its minted id.
///
/// `SetContext`/`SupersedeContext` do not both return the id, so a plain
/// `SetContext` is followed by a before/after `ListContext` diff to recover it
/// (`SupersedeContext` returns the new id directly).
async fn create_on_server(
    client: &mut HostedGrpcClient,
    repo_path: &str,
    target: &ContextTarget,
    annotation: &Annotation,
    known_server_ids: &HashSet<String>,
) -> Result<String> {
    let first = annotation
        .revisions
        .first()
        .context("annotation has no revisions")?;
    let (path, target_state_id) = target_operands(target);

    // Supersession maps to `SupersedeContext` when the superseded annotation is
    // already on the server (linked or pulled). Otherwise it degrades to a
    // plain create (the chain link is lost server-side but the content lands).
    if let Some(superseded_local) = &annotation.supersedes_annotation_id
        && known_server_ids.contains(superseded_local)
    {
        let response = client
            .supersede_context(
                repo_path,
                superseded_local,
                if path.is_empty() { None } else { Some(path.as_str()) },
                target_state_id.as_deref(),
                scope_to_proto(&annotation.scope),
                first.tags.clone(),
                &first.content,
                None,
                None,
                kind_to_proto(first.kind),
                supersede_op_id(repo_path, superseded_local, &annotation.annotation_id),
            )
            .await
            .with_context(|| format!("supersede hosted annotation {superseded_local}"))?;
        return Ok(response.new_annotation_id);
    }

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
            set_op_id(repo_path, &annotation.annotation_id),
        )
        .await
        .with_context(|| format!("set hosted context for {}", annotation.annotation_id))?;

    // Recover the minted id: the one annotation now present at the target that
    // we did not already know about.
    let after = list_server_annotations(client, repo_path).await?;
    after
        .into_iter()
        .rev()
        .find(|candidate| {
            !known_server_ids.contains(&candidate.id) && candidate.content == first.content
        })
        .map(|candidate| candidate.id)
        .context("could not recover the minted annotation id after SetContext")
}

// =========================================================================
// Pull
// =========================================================================

/// Fetch hosted annotations for the repository head and materialize any
/// annotation/revision we do not already hold. Saves the mirror after each
/// annotation and continues past a per-annotation failure.
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

    let mut mirror = load_mirror(repo.heddle_dir())?;
    let mut changed = 0usize;
    for (target, annotation) in server {
        let result = pull_one(repo, client, repo_path, &head_state, &target, &annotation).await;
        // Record the link regardless (local id == server id for pulled annots),
        // so a later push adopts it instead of re-creating.
        record_pull_link(&mut mirror, repo_path, &annotation);
        save_mirror(repo.heddle_dir(), &mirror)?;
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

fn record_pull_link(
    mirror: &mut HostedContextMirror,
    repo_path: &str,
    annotation: &ContextAnnotation,
) {
    let repo_mirror = mirror.repos.entry(repo_path.to_string()).or_default();
    let synced = annotation.revision_count as usize;
    match repo_mirror
        .annotations
        .iter_mut()
        .find(|entry| entry.server_id == annotation.id)
    {
        Some(entry) => entry.synced_revisions = entry.synced_revisions.max(synced),
        None => repo_mirror.annotations.push(ContextMirrorEntry {
            local_id: annotation.id.clone(),
            server_id: annotation.id.clone(),
            synced_revisions: synced,
        }),
    }
}

async fn pull_one(
    repo: &Repository,
    client: &mut HostedGrpcClient,
    repo_path: &str,
    head_state: &State,
    target: &ContextTarget,
    server: &ContextAnnotation,
) -> Result<bool> {
    // The pack may have already delivered this annotation (local id == server
    // id). Load the current local blob at the target and reconcile by id.
    let context_root = context_root_for_state(repo, head_state)?;
    let mut blob = match &context_root {
        Some(root) => repo
            .get_context_blob(root, target)?
            .unwrap_or_else(|| ContextBlob::new(vec![])),
        None => ContextBlob::new(vec![]),
    };

    let existing = blob
        .annotations
        .iter()
        .position(|annotation| annotation.annotation_id == server.id);

    match existing {
        Some(index) => {
            // Already present (pack-delivered or previously pulled). Append only
            // the revisions the server has beyond what we hold.
            if server.revision_count as usize <= blob.annotations[index].revisions.len() {
                return Ok(false);
            }
            let history = fetch_history(client, repo_path, &server.id).await?;
            let local = &mut blob.annotations[index];
            let start = local.revisions.len();
            for revision in history.into_iter().skip(start) {
                local.revisions.push(revision);
            }
            local.status = status_from_proto(server.status);
        }
        None => {
            // New annotation → materialize the full history, preserving the
            // server annotation id and per-revision authorship/timestamps.
            let revisions = fetch_history(client, repo_path, &server.id).await?;
            blob.annotations.push(materialize_annotation(server, revisions));
        }
    }

    let new_root = repo.set_context_blob(context_root.as_ref(), target, &blob)?;
    put_context_attachment(repo, head_state, Some(new_root))?;
    Ok(true)
}

/// Build a local `Annotation` from a server annotation + its (oldest-first)
/// revisions, preserving the server id.
fn materialize_annotation(server: &ContextAnnotation, revisions: Vec<AnnotationRevision>) -> Annotation {
    Annotation {
        annotation_id: server.id.clone(),
        scope: scope_from_proto(server.scope.as_ref()),
        status: status_from_proto(server.status),
        revisions,
        supersedes_annotation_id: server.supersedes_annotation_id.clone(),
        supersedes_rewrite_pct: server.supersedes_rewrite_pct,
        visibility: objects::object::VisibilityTier::default(),
        resolved_from_discussion: None,
    }
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

// =========================================================================
// Shared server enumeration
// =========================================================================

/// Flat list of every server annotation (both file and state targets).
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

/// Every server annotation paired with the local `ContextTarget` it anchors to.
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
        let Some(target) = target_from_annotated_file(&file.path) else {
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

// --- derived, deterministic client-operation-ids (idempotent retry) ---

const OP_NAMESPACE: uuid::Uuid = uuid::Uuid::from_u128(0x6865_6464_6c65_6374_785f_7379_6e63_0001);

fn set_op_id(repo_path: &str, local_id: &str) -> String {
    uuid::Uuid::new_v5(&OP_NAMESPACE, format!("set:{repo_path}:{local_id}").as_bytes()).to_string()
}

fn revise_op_id(repo_path: &str, server_id: &str, revision_id: &str) -> String {
    uuid::Uuid::new_v5(
        &OP_NAMESPACE,
        format!("revise:{repo_path}:{server_id}:{revision_id}").as_bytes(),
    )
    .to_string()
}

fn supersede_op_id(repo_path: &str, superseded_id: &str, local_id: &str) -> String {
    uuid::Uuid::new_v5(
        &OP_NAMESPACE,
        format!("supersede:{repo_path}:{superseded_id}:{local_id}").as_bytes(),
    )
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

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

    // A pulled annotation carries the SERVER id as its local id, so a later push
    // must adopt it (idempotent) rather than issue a duplicate create.
    #[test]
    fn pull_link_uses_server_id_as_local_id() {
        let mut mirror = HostedContextMirror::default();
        let annotation = ContextAnnotation {
            id: "srv-1".to_string(),
            revision_count: 2,
            ..Default::default()
        };
        record_pull_link(&mut mirror, "ns/repo", &annotation);
        let entry = &mirror.repos["ns/repo"].annotations[0];
        assert_eq!(entry.local_id, "srv-1");
        assert_eq!(entry.server_id, "srv-1");
        assert_eq!(entry.synced_revisions, 2);
    }

    // Distinct server ids for identical bodies from different authors must
    // materialize as distinct annotations (no cross-author misattribution).
    #[test]
    fn identical_bodies_distinct_ids_materialize_separately() {
        let rev = |who: &str| AnnotationRevision {
            revision_id: format!("r-{who}"),
            kind: AnnotationKind::Rationale,
            content: "lgtm".to_string(),
            tags: vec![],
            attribution: format!("{who} <>"),
            created_at: 1,
            source_hash: None,
            created_at_state: None,
        };
        let a = materialize_annotation(
            &ContextAnnotation {
                id: "srv-a".to_string(),
                revision_count: 1,
                ..Default::default()
            },
            vec![rev("alice")],
        );
        let b = materialize_annotation(
            &ContextAnnotation {
                id: "srv-b".to_string(),
                revision_count: 1,
                ..Default::default()
            },
            vec![rev("bob")],
        );
        assert_ne!(a.annotation_id, b.annotation_id);
        assert_eq!(a.revisions[0].attribution, "alice <>");
        assert_eq!(b.revisions[0].attribution, "bob <>");
    }
}
