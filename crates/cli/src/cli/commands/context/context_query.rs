// SPDX-License-Identifier: Apache-2.0
//! Context query commands: get, list, history, check, suggest, audit.

use std::{collections::BTreeMap, path::Path};

use anyhow::Result;
use objects::{
    object::{AnnotationStatus, ContextTarget},
    store::{AgentRegistry, ContextQueryEntry},
};
use repo::{
    ContextSuggestionTier, Repository, ThreadManager,
    staleness::{self, StalenessStatus},
};
use serde::Serialize;

use super::{
    AnnotationHistoryOutput, AnnotationOutput, ContextGetOutput, RevisionOutput,
    filter_annotations, print_context_get, resolve_state, resolve_state_id, target_label,
};
use crate::cli::{Cli, commands::RecoveryAdvice, should_output_json};

#[derive(Serialize)]
struct SuggestionOutput {
    path: String,
    score: u32,
    tier: String,
    reasons: Vec<String>,
    recent_changes: u32,
    distinct_states: u32,
    distinct_agents: u32,
    has_context: bool,
    stale_annotations: u32,
}

pub async fn cmd_context_get(
    cli: &Cli,
    path: Option<String>,
    state: Option<String>,
    scope: Option<String>,
    tag: Option<String>,
    r#ref: Option<String>,
) -> Result<()> {
    let repo = Repository::open(cli.repo.as_ref().unwrap_or(&std::env::current_dir()?))?;
    let state_obj = resolve_state(&repo, r#ref.as_deref())?;
    let target = super::resolve_target(&repo, path, state)?;
    let Some(context_root) = &state_obj.context else {
        return print_context_get(cli, &target, Vec::new());
    };

    let blob = repo.get_context_blob(context_root, &target)?;
    let empty = objects::object::ContextBlob::new(vec![]);
    let blob_ref = blob.as_ref().unwrap_or(&empty);
    let annotations = filter_annotations(
        &blob_ref.annotations,
        scope.as_deref(),
        tag.as_deref(),
        false,
    )?;

    let _ = target
        .path()
        .map(|path| log_context_query_if_agent_session(&repo, path, scope.as_deref()));

    print_context_get(cli, &target, annotations)
}

pub async fn cmd_context_list(
    cli: &Cli,
    prefix: Option<String>,
    tag: Option<String>,
    r#ref: Option<String>,
    include_superseded: bool,
) -> Result<()> {
    let repo = Repository::open(cli.repo.as_ref().unwrap_or(&std::env::current_dir()?))?;
    let state_obj = resolve_state(&repo, r#ref.as_deref())?;
    let Some(context_root) = &state_obj.context else {
        if should_output_json(cli, None) {
            println!(
                "{}",
                serde_json::json!({"output_kind": "context_list", "items": []})
            );
        } else {
            println!("No context annotations.");
        }
        return Ok(());
    };

    let entries = repo.list_context_entries(context_root, prefix.as_deref().map(Path::new))?;

    if should_output_json(cli, None) {
        let items: Vec<ContextGetOutput> = entries
            .iter()
            .filter_map(|entry| {
                let annotations = filter_annotations(
                    &entry.blob.annotations,
                    None,
                    tag.as_deref(),
                    include_superseded,
                )
                .ok()?;
                if annotations.is_empty() {
                    return None;
                }
                let (target_kind, target_label) = target_label(&entry.target);
                Some(ContextGetOutput {
                    // `context_list` envelopes ContextGetOutput rows, so
                    // the inner `output_kind` field is suppressed via
                    // serialization to avoid leaking a misleading
                    // per-row discriminator. The outer envelope owns
                    // the discriminator.
                    output_kind: "context_get",
                    target_kind,
                    target: target_label,
                    annotations: annotations
                        .into_iter()
                        .map(AnnotationOutput::from_annotation)
                        .collect(),
                })
            })
            .collect();
        let envelope = serde_json::json!({
            "output_kind": "context_list",
            "items": items,
        });
        println!("{}", serde_json::to_string(&envelope)?);
    } else if entries.is_empty() {
        println!("No context annotations.");
    } else {
        for entry in &entries {
            let annotations = filter_annotations(
                &entry.blob.annotations,
                None,
                tag.as_deref(),
                include_superseded,
            )?;
            if annotations.is_empty() {
                continue;
            }
            let (kind, label) = target_label(&entry.target);
            println!(
                "  {} {} ({} annotation{})",
                kind,
                label,
                annotations.len(),
                if annotations.len() == 1 { "" } else { "s" }
            );
        }
    }

    Ok(())
}

pub async fn cmd_context_history(
    cli: &Cli,
    annotation_id: String,
    r#ref: Option<String>,
) -> Result<()> {
    let repo = Repository::open(cli.repo.as_ref().unwrap_or(&std::env::current_dir()?))?;
    let state_obj = resolve_state(&repo, r#ref.as_deref())?;
    let context_root = state_obj
        .context
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!(RecoveryAdvice::context_empty()))?;

    let (target, blob, index) = repo
        .find_annotation(context_root, &annotation_id)?
        .ok_or_else(|| anyhow::anyhow!(RecoveryAdvice::annotation_not_found(&annotation_id)))?;
    let annotation = &blob.annotations[index];
    let (target_kind, target_label) = target_label(&target);
    let output = AnnotationHistoryOutput {
        output_kind: "context_history",
        annotation_id: annotation.annotation_id.clone(),
        target_kind,
        target: target_label,
        scope: annotation.scope.to_string(),
        status: match annotation.status {
            AnnotationStatus::Active => "active".to_string(),
            AnnotationStatus::Superseded => "superseded".to_string(),
        },
        supersedes_annotation_id: annotation.supersedes_annotation_id.clone(),
        supersedes_rewrite_pct: annotation.supersedes_rewrite_pct,
        revisions: annotation
            .revisions
            .iter()
            .rev()
            .map(|revision| RevisionOutput {
                revision_id: revision.revision_id.clone(),
                kind: revision.kind.to_string(),
                content: revision.content.clone(),
                tags: revision.tags.clone(),
                attribution: revision.attribution.clone(),
                created_at: revision.created_at,
            })
            .collect(),
    };

    if should_output_json(cli, None) {
        println!("{}", serde_json::to_string(&output)?);
    } else {
        println!("{} {}", output.target_kind, output.target);
        println!("annotation: {}", output.annotation_id);
        println!("scope: {}", output.scope);
        println!("status: {}", output.status);
        for revision in &output.revisions {
            println!("--- [{}] {} ---", revision.kind, revision.revision_id);
            if !revision.tags.is_empty() {
                println!("tags: {}", revision.tags.join(", "));
            }
            println!("by: {}", revision.attribution);
            println!("{}", revision.content);
            println!();
        }
    }

    Ok(())
}

pub async fn cmd_context_check(
    cli: &Cli,
    path: Option<String>,
    state: Option<String>,
    tag: Option<String>,
    r#ref: Option<String>,
) -> Result<()> {
    let repo = Repository::open(cli.repo.as_ref().unwrap_or(&std::env::current_dir()?))?;
    let state_obj = resolve_state(&repo, r#ref.as_deref())?;
    let context_root = state_obj
        .context
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!(RecoveryAdvice::context_empty()))?;

    let target_filter = match (path, state) {
        (Some(path), None) => Some(ContextTarget::file(path)?),
        (None, Some(state)) => Some(ContextTarget::state(resolve_state_id(&repo, &state)?)),
        (None, None) => None,
        (Some(_), Some(_)) => {
            return Err(anyhow::anyhow!(RecoveryAdvice::invalid_usage(
                "context_target_conflict",
                "--path and --state are mutually exclusive",
                "Pass exactly one target: either `--path <path>` or `--state <state>`.",
                "heddle context list --path <path>",
            )));
        }
    };

    let entries = repo.list_context_entries(context_root, None)?;
    let filtered_entries: Vec<_> = entries
        .into_iter()
        .filter(|entry| {
            target_filter
                .as_ref()
                .is_none_or(|target| &entry.target == target)
        })
        .collect();

    if filtered_entries.is_empty() {
        if should_output_json(cli, None) {
            println!(
                "{}",
                serde_json::json!({
                    "output_kind": "context_check",
                    "annotations": 0,
                    "stale": 0,
                })
            );
        } else {
            println!("No annotations found.");
        }
        return Ok(());
    }

    let mut total = 0u32;
    let mut fresh = 0u32;
    let mut stale = 0u32;
    let mut unknown = 0u32;
    let mut issues: Vec<serde_json::Value> = Vec::new();

    for entry in &filtered_entries {
        for annotation in &entry.blob.annotations {
            if annotation.status == AnnotationStatus::Superseded {
                continue;
            }
            let Some(current) = annotation.current_revision() else {
                continue;
            };
            if let Some(ref tag_filter) = tag
                && !current.tags.iter().any(|candidate| candidate == tag_filter)
            {
                continue;
            }

            total += 1;
            let status = staleness::check_annotation_staleness(
                &repo,
                annotation,
                &entry.target,
                &state_obj,
            )?;
            match &status {
                StalenessStatus::Fresh => fresh += 1,
                StalenessStatus::Unknown => unknown += 1,
                StalenessStatus::SourceChanged { .. }
                | StalenessStatus::SymbolMissing { .. }
                | StalenessStatus::FileMissing => {
                    stale += 1;
                    let reason = match &status {
                        StalenessStatus::SourceChanged { .. } => "source_changed",
                        StalenessStatus::SymbolMissing { .. } => "symbol_missing",
                        StalenessStatus::FileMissing => "file_missing",
                        StalenessStatus::Unknown | StalenessStatus::Fresh => unreachable!(),
                    };
                    let (_, target_label) = target_label(&entry.target);
                    if should_output_json(cli, None) {
                        issues.push(serde_json::json!({
                            "target": target_label,
                            "scope": annotation.scope.to_string(),
                            "reason": reason,
                            "annotation_id": annotation.annotation_id,
                            "content": current.content.chars().take(80).collect::<String>(),
                        }));
                    } else {
                        println!("  ✗ {}  {}  {}", target_label, annotation.scope, reason,);
                    }
                }
            }
        }
    }

    if should_output_json(cli, None) {
        println!(
            "{}",
            serde_json::json!({
                "output_kind": "context_check",
                "annotations": total,
                "fresh": fresh,
                "stale": stale,
                "unknown": unknown,
                "issues": issues,
            })
        );
    } else {
        println!();
        println!(
            "{} annotation{} checked: {} fresh, {} stale, {} unknown",
            total,
            if total == 1 { "" } else { "s" },
            fresh,
            stale,
            unknown,
        );
        if stale == 0 {
            println!("All annotations are current.");
        }
    }

    Ok(())
}

pub async fn cmd_context_suggest(cli: &Cli, r#ref: Option<String>, limit: usize) -> Result<()> {
    let repo = Repository::open(cli.repo.as_ref().unwrap_or(&std::env::current_dir()?))?;
    let state_obj = resolve_state(&repo, r#ref.as_deref())?;
    let suggestions = repo.suggest_context_targets(&state_obj, limit)?;

    if should_output_json(cli, None) {
        let items: Vec<SuggestionOutput> = suggestions
            .into_iter()
            .map(|suggestion| SuggestionOutput {
                path: suggestion.path,
                score: suggestion.score,
                tier: match suggestion.tier {
                    ContextSuggestionTier::Medium => "medium".to_string(),
                    ContextSuggestionTier::High => "high".to_string(),
                },
                reasons: suggestion.reasons,
                recent_changes: suggestion.recent_changes,
                distinct_states: suggestion.distinct_states,
                distinct_agents: suggestion.distinct_agents,
                has_context: suggestion.has_context,
                stale_annotations: suggestion.stale_annotations,
            })
            .collect();
        let envelope = serde_json::json!({
            "output_kind": "context_suggest",
            "items": items,
        });
        println!("{}", serde_json::to_string(&envelope)?);
    } else if suggestions.is_empty() {
        println!("No low-noise context suggestions right now.");
    } else {
        for suggestion in suggestions {
            let tier = match suggestion.tier {
                ContextSuggestionTier::Medium => "may benefit",
                ContextSuggestionTier::High => "recommended",
            };
            println!("{}  {} ({})", suggestion.path, suggestion.score, tier);
            for reason in suggestion.reasons {
                println!("  - {reason}");
            }
        }
    }

    Ok(())
}

pub async fn cmd_context_audit(cli: &Cli, r#ref: Option<String>) -> Result<()> {
    let repo = Repository::open(cli.repo.as_ref().unwrap_or(&std::env::current_dir()?))?;
    let state_obj = resolve_state(&repo, r#ref.as_deref())?;
    let Some(context_root) = &state_obj.context else {
        if should_output_json(cli, None) {
            println!(
                "{}",
                serde_json::json!({
                    "output_kind": "context_audit",
                    "annotations": 0,
                    "superseded": 0,
                    "duplicates": 0,
                    "stale": 0,
                })
            );
        } else {
            println!("No context annotations.");
        }
        return Ok(());
    };

    let entries = repo.list_context_entries(context_root, None)?;
    let stale_map = staleness::check_context_staleness(&repo, &state_obj)?;
    let mut total = 0u32;
    let mut superseded = 0u32;
    let mut stale = 0u32;
    let mut signatures = BTreeMap::<(String, String, String), u32>::new();

    for entry in &entries {
        for annotation in &entry.blob.annotations {
            total += 1;
            if annotation.status == AnnotationStatus::Superseded {
                superseded += 1;
            }
            let Some(current) = annotation.current_revision() else {
                continue;
            };
            let key = match &entry.target {
                ContextTarget::File { path } => format!("{path}:{}", annotation.scope),
                ContextTarget::State { change_id } => {
                    format!(
                        "state:{}:{}",
                        change_id.to_string_full(),
                        annotation.annotation_id
                    )
                }
            };
            if stale_map
                .get(&key)
                .is_some_and(|status| !matches!(status, StalenessStatus::Fresh))
            {
                stale += 1;
            }
            let target_key = match &entry.target {
                ContextTarget::File { path } => path.clone(),
                ContextTarget::State { change_id } => change_id.to_string_full(),
            };
            *signatures
                .entry((
                    target_key,
                    annotation.scope.to_string(),
                    current.content.clone(),
                ))
                .or_default() += 1;
        }
    }

    let duplicates = signatures.values().filter(|count| **count > 1).count() as u32;

    if should_output_json(cli, None) {
        println!(
            "{}",
            serde_json::json!({
                "output_kind": "context_audit",
                "annotations": total,
                "superseded": superseded,
                "duplicates": duplicates,
                "stale": stale,
            })
        );
    } else {
        println!("annotations: {total}");
        println!("superseded: {superseded}");
        println!("duplicates: {duplicates}");
        println!("stale: {stale}");
    }

    Ok(())
}

/// Append a context query record to the active agent session's TOML, if any.
fn log_context_query_if_agent_session(
    repo: &Repository,
    path: &str,
    scope: Option<&str>,
) -> std::result::Result<(), ()> {
    let registry = AgentRegistry::new(repo.heddle_dir());
    let session = registry
        .find_active_by_path(repo.root())
        .map_err(|_| ())?
        .or_else(|| {
            let thread = ThreadManager::new(repo.heddle_dir())
                .find_by_execution_root(repo.root())
                .ok()
                .flatten()?;
            registry.list().ok()?.into_iter().find(|entry| {
                entry.status == objects::store::AgentStatus::Active
                    && entry.thread_id.as_deref() == Some(thread.id.as_str())
            })
        });

    if let Some(session) = session {
        let query = ContextQueryEntry {
            path: path.to_string(),
            scope: scope.map(str::to_string),
            queried_at: chrono::Utc::now(),
        };
        let _ = registry.log_context_query(&session.session_id, query);
    }

    Ok(())
}
