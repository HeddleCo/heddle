// SPDX-License-Identifier: Apache-2.0
//! Context mutation commands: set, edit, supersede, rm.

use anyhow::Result;
use chrono::Utc;
use objects::{
    lock::RepositoryLockExt,
    object::{Annotation, AnnotationStatus, ContextBlob},
};
use repo::{Repository, compute_rewrite_pct};

use super::{
    apply_new_state, build_context_state, compute_source_hash, parse_kind, parse_scope,
    read_annotation_content, resolve_scope_at_target, resolve_state, resolve_target, target_label,
};
use crate::{
    cli::{
        Cli,
        commands::{RecoveryAdvice, snapshot::resolve_attribution},
        should_output_json,
    },
    config::UserConfig,
};

/// Set a context annotation on a file path or state target.
#[allow(clippy::too_many_arguments)]
pub async fn cmd_context_set(
    cli: &Cli,
    path: Option<String>,
    state: Option<String>,
    scope: Option<String>,
    kind: String,
    tags: Vec<String>,
    message: Option<String>,
    file: Option<std::path::PathBuf>,
) -> Result<()> {
    let repo = Repository::open(cli.repo.as_ref().unwrap_or(&std::env::current_dir()?))?;
    let target = resolve_target(&repo, path, state)?;
    let scope = parse_scope(scope.as_deref())?;
    target.validate_scope(&scope)?;
    let kind = parse_kind(Some(&kind))?;
    let content = read_annotation_content(message, file)?;

    let _lock = repo.locker().write().map_err(|e| anyhow::anyhow!("{e}"))?;
    let head_state = resolve_state(&repo, None)?;
    // Eagerly resolve symbol scopes against the worktree so the annotation
    // carries `resolved_lines` from the moment of creation. Without this, the
    // staleness check returns SymbolMissing on the very first read and the
    // chip never renders.
    let scope = resolve_scope_at_target(&repo, &target, scope)?;
    let source_hash = compute_source_hash(&repo, &target, &scope);
    let user_config = UserConfig::load_default()?;
    let attribution = resolve_attribution(&repo, &user_config)?;
    let annotation = Annotation::new(
        scope,
        kind,
        content,
        tags,
        attribution.to_string(),
        Utc::now().timestamp(),
        source_hash,
        Some(head_state.change_id),
    );

    let mut blob = match &head_state.context {
        Some(root) => repo
            .get_context_blob(root, &target)?
            .unwrap_or_else(|| ContextBlob::new(vec![])),
        None => ContextBlob::new(vec![]),
    };
    blob.annotations.push(annotation);
    let new_context_root = repo.set_context_blob(head_state.context.as_ref(), &target, &blob)?;
    let (_, label) = target_label(&target);
    let new_state = build_context_state(
        &repo,
        &head_state,
        Some(new_context_root),
        format!("context: annotate {label}"),
    )?;
    apply_new_state(&repo, &new_state)?;

    if should_output_json(cli, None) {
        println!(
            "{}",
            serde_json::json!({
                "target": label,
                "annotations": blob.annotations.len(),
                "state": new_state.change_id.short(),
            })
        );
    } else {
        println!(
            "Annotated {} ({} active annotation{})",
            label,
            blob.annotations
                .iter()
                .filter(|annotation| annotation.status == AnnotationStatus::Active)
                .count(),
            if blob.annotations.len() == 1 { "" } else { "s" }
        );
    }

    Ok(())
}

pub async fn cmd_context_edit(
    cli: &Cli,
    annotation_id: String,
    kind: Option<String>,
    tags: Vec<String>,
    message: Option<String>,
    file: Option<std::path::PathBuf>,
) -> Result<()> {
    let repo = Repository::open(cli.repo.as_ref().unwrap_or(&std::env::current_dir()?))?;
    let content = read_annotation_content(message, file)?;
    let _lock = repo.locker().write().map_err(|e| anyhow::anyhow!("{e}"))?;
    let head_state = resolve_state(&repo, None)?;
    let context_root = head_state
        .context
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("No context annotations in this repository"))?;

    let (target, mut blob, index) = repo
        .find_annotation(context_root, &annotation_id)?
        .ok_or_else(|| anyhow::anyhow!("Annotation not found: {annotation_id}"))?;

    let annotation = blob
        .annotations
        .get_mut(index)
        .ok_or_else(|| anyhow::anyhow!("Annotation index out of range"))?;
    let current = annotation.current_revision().cloned().unwrap();
    let next_kind = match kind.as_deref() {
        Some(kind) => parse_kind(Some(kind))?,
        None => current.kind,
    };
    let next_tags = if tags.is_empty() {
        current.tags.clone()
    } else {
        tags
    };
    annotation.scope = resolve_scope_at_target(&repo, &target, annotation.scope.clone())?;
    let source_hash = compute_source_hash(&repo, &target, &annotation.scope);
    let user_config = UserConfig::load_default()?;
    let attribution = resolve_attribution(&repo, &user_config)?;
    annotation.revise(
        next_kind,
        content,
        next_tags,
        attribution.to_string(),
        Utc::now().timestamp(),
        source_hash,
        Some(head_state.change_id),
    );
    let revision_count = annotation.revisions.len();
    let _ = annotation;

    let new_context_root = repo.set_context_blob(Some(context_root), &target, &blob)?;
    let (_, label) = target_label(&target);
    let new_state = build_context_state(
        &repo,
        &head_state,
        Some(new_context_root),
        format!("context: revise {label}"),
    )?;
    apply_new_state(&repo, &new_state)?;

    if should_output_json(cli, None) {
        println!(
            "{}",
            serde_json::json!({
                "annotation_id": annotation_id,
                "state": new_state.change_id.short(),
                "revision_count": revision_count,
            })
        );
    } else {
        println!("Revised annotation {}", annotation_id);
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub async fn cmd_context_supersede(
    cli: &Cli,
    annotation_id: String,
    path: Option<String>,
    state: Option<String>,
    scope: Option<String>,
    kind: String,
    tags: Vec<String>,
    message: Option<String>,
    file: Option<std::path::PathBuf>,
) -> Result<()> {
    let repo = Repository::open(cli.repo.as_ref().unwrap_or(&std::env::current_dir()?))?;
    let content = read_annotation_content(message, file)?;
    let _lock = repo.locker().write().map_err(|e| anyhow::anyhow!("{e}"))?;
    let head_state = resolve_state(&repo, None)?;
    let context_root = head_state
        .context
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("No context annotations in this repository"))?;

    let (original_target, mut original_blob, index) = repo
        .find_annotation(context_root, &annotation_id)?
        .ok_or_else(|| anyhow::anyhow!("Annotation not found: {annotation_id}"))?;
    let original_annotation = original_blob.annotations[index].clone();
    let original_revision = original_annotation.current_revision().cloned().unwrap();

    let target = match (path, state) {
        (None, None) => original_target.clone(),
        (path, state) => resolve_target(&repo, path, state)?,
    };
    let replacement_scope = match scope.as_deref() {
        Some(scope) => parse_scope(Some(scope))?,
        None => original_annotation.scope.clone(),
    };
    target.validate_scope(&replacement_scope)?;
    let replacement_scope = resolve_scope_at_target(&repo, &target, replacement_scope)?;
    let kind = parse_kind(Some(&kind))?;
    let source_hash = compute_source_hash(&repo, &target, &replacement_scope);
    let rewrite_pct = compute_rewrite_pct(&original_revision.content, &content);
    let user_config = UserConfig::load_default()?;
    let attribution = resolve_attribution(&repo, &user_config)?;
    let mut replacement = Annotation::new(
        replacement_scope,
        kind,
        content,
        tags,
        attribution.to_string(),
        Utc::now().timestamp(),
        source_hash,
        Some(head_state.change_id),
    );
    replacement.supersedes_annotation_id = Some(annotation_id.clone());
    replacement.supersedes_rewrite_pct = Some(rewrite_pct);

    original_blob.annotations[index].mark_superseded();
    let mut next_root =
        repo.set_context_blob(Some(context_root), &original_target, &original_blob)?;

    let mut replacement_blob = if target == original_target {
        original_blob
    } else {
        repo.get_context_blob(&next_root, &target)?
            .unwrap_or_else(|| ContextBlob::new(vec![]))
    };
    replacement_blob.annotations.push(replacement);
    next_root = repo.set_context_blob(Some(&next_root), &target, &replacement_blob)?;

    let (_, label) = target_label(&target);
    let new_state = build_context_state(
        &repo,
        &head_state,
        Some(next_root),
        format!("context: supersede {label}"),
    )?;
    apply_new_state(&repo, &new_state)?;

    if should_output_json(cli, None) {
        println!(
            "{}",
            serde_json::json!({
                "annotation_id": annotation_id,
                "replacement_target": label,
                "rewrite_pct": rewrite_pct,
                "state": new_state.change_id.short(),
            })
        );
    } else {
        println!(
            "Superseded annotation {} with a {}% rewrite",
            annotation_id, rewrite_pct
        );
    }

    Ok(())
}

pub async fn cmd_context_rm(
    cli: &Cli,
    path: Option<String>,
    state: Option<String>,
    scope: Option<String>,
    all: bool,
) -> Result<()> {
    let repo = Repository::open(cli.repo.as_ref().unwrap_or(&std::env::current_dir()?))?;
    let target = resolve_target(&repo, path, state)?;

    let _lock = repo.locker().write().map_err(|e| anyhow::anyhow!("{e}"))?;
    let head_state = resolve_state(&repo, None)?;
    let Some(context_root) = &head_state.context else {
        return Err(anyhow::anyhow!(RecoveryAdvice::invalid_usage(
            "context_remove_empty",
            "No context annotations to remove",
            "Inspect context with `heddle context list`, then remove an existing annotation scope.",
            "heddle context list",
        )));
    };
    if !all && scope.is_none() {
        return Err(anyhow::anyhow!(RecoveryAdvice::invalid_usage(
            "context_remove_scope_required",
            "Specify --scope to remove specific annotations, or --all to remove all",
            "Pass `--scope <scope>` to remove one scope, or `--all` to remove all annotations at the target.",
            "heddle context rm --path <path> --scope file",
        )));
    }
    let scope_filter = if all {
        None
    } else {
        Some(parse_scope(scope.as_deref())?)
    };

    let new_context_root =
        repo.remove_context_at_target(context_root, &target, scope_filter.as_ref())?;
    let (_, label) = target_label(&target);
    let new_state = build_context_state(
        &repo,
        &head_state,
        new_context_root,
        format!("context: remove annotation from {label}"),
    )?;
    apply_new_state(&repo, &new_state)?;

    if should_output_json(cli, None) {
        println!(
            "{}",
            serde_json::json!({
                "target": label,
                "removed": true,
                "state": new_state.change_id.short(),
            })
        );
    } else {
        println!("Removed annotations from {label}");
    }

    Ok(())
}
