// SPDX-License-Identifier: Apache-2.0
//! Marker commands.

use anyhow::{Result, anyhow};
use objects::{object::MarkerName, store::LocalObjectStore};
use oplog::LocalOpLogRecorder;
use repo::Repository;
use serde::Serialize;

use super::{advice::RecoveryAdvice, snapshot::ensure_current_state};
use crate::{
    cli::{Cli, ThreadMarkerCommands, should_output_json},
    config::UserConfig,
};

#[derive(Serialize)]
struct MarkerListOutput {
    output_kind: &'static str,
    markers: Vec<MarkerEntry>,
}

#[derive(Serialize)]
struct MarkerEntry {
    name: String,
    /// Short change-id of the state the marker points at.
    change_id: String,
}

#[derive(Serialize)]
struct MarkerOpOutput {
    output_kind: &'static str,
    name: String,
    /// Short change-id of the state the marker pointed at after the op.
    /// `None` for ops that delete the marker.
    change_id: Option<String>,
    message: String,
}

#[derive(Serialize)]
struct MarkerBulkDeleteOutput {
    output_kind: &'static str,
    deleted: Vec<MarkerEntry>,
    count: usize,
    message: String,
}

pub fn cmd_thread_marker(
    cli: &Cli,
    repo: &Repository,
    command: ThreadMarkerCommands,
) -> Result<()> {
    match command {
        ThreadMarkerCommands::List { filter } => cmd_marker_list(cli, repo, filter),
        ThreadMarkerCommands::Create { name, .. } => cmd_marker_create(cli, repo, name),
        ThreadMarkerCommands::Delete { name, prefix } => match (name, prefix) {
            (Some(name), None) => cmd_marker_delete(cli, repo, name),
            (None, Some(prefix)) => cmd_marker_delete_prefix(cli, repo, prefix),
            // Clap enforces required_unless_present + conflicts_with, so
            // these branches are unreachable in practice. Guard defensively
            // in case the constraint is ever relaxed.
            (Some(_), Some(_)) => Err(anyhow!(marker_delete_selector_conflict_advice())),
            (None, None) => Err(anyhow!(marker_delete_selector_required_advice())),
        },
        ThreadMarkerCommands::Show { name } => cmd_marker_show(cli, repo, name),
    }
}

fn cmd_marker_list(cli: &Cli, repo: &Repository, filter: Option<String>) -> Result<()> {
    let markers = repo.refs().list_markers()?;

    let entries: Vec<MarkerEntry> = markers
        .iter()
        .filter(|name| match filter.as_deref() {
            // Prefix match (not a glob). An empty filter is treated as
            // "no filter" rather than "match every marker" — passing
            // `--filter ""` is almost always an unintended shell
            // expansion accident, and the no-op behavior is the
            // friendliest interpretation. (The symmetric `marker
            // delete --prefix ""` rejects the empty string for safety,
            // because the consequence there is destructive.)
            Some(prefix) if !prefix.is_empty() => name.starts_with(prefix),
            _ => true,
        })
        .filter_map(|name| {
            let state = repo.refs().get_marker(name).ok()??;
            Some(MarkerEntry {
                name: name.to_string(),
                change_id: state.short(),
            })
        })
        .collect();

    let output = MarkerListOutput {
        output_kind: "thread_marker_list",
        markers: entries,
    };

    if should_output_json(cli, Some(repo.config())) {
        println!("{}", serde_json::to_string(&output)?);
    } else {
        for entry in &output.markers {
            println!("{} -> {}", entry.name, entry.change_id);
        }
        if output.markers.is_empty() {
            println!("No markers");
        }
    }

    Ok(())
}

fn cmd_marker_create(cli: &Cli, repo: &Repository, name: String) -> Result<()> {
    let current = ensure_current_state(
        repo,
        &UserConfig::load_default().unwrap_or_default(),
        Some(format!(
            "Bootstrap git-overlay before creating marker {}",
            name
        )),
    )?;

    let mn = MarkerName::new(&name);
    repo.refs().create_marker(&mn, &current)?;
    repo.oplog().record_marker_create(&mn, &current)?;

    let output = MarkerOpOutput {
        output_kind: "thread_marker_create",
        name: name.clone(),
        change_id: Some(current.short()),
        message: format!("Created marker '{}' at {}", name, current.short()),
    };

    if should_output_json(cli, Some(repo.config())) {
        println!("{}", serde_json::to_string(&output)?);
    } else {
        println!("{}", output.message);
    }

    Ok(())
}

fn cmd_marker_delete(cli: &Cli, repo: &Repository, name: String) -> Result<()> {
    let mn = MarkerName::new(&name);
    let state = repo
        .refs()
        .delete_marker(&mn)?
        .ok_or_else(|| anyhow!("Marker not found: {}", name))?;

    repo.oplog().record_marker_delete(&mn, &state)?;

    let output = MarkerOpOutput {
        output_kind: "thread_marker_delete",
        name: name.clone(),
        change_id: None,
        message: format!("Deleted marker '{}'", name),
    };

    if should_output_json(cli, Some(repo.config())) {
        println!("{}", serde_json::to_string(&output)?);
    } else {
        println!("{}", output.message);
    }

    Ok(())
}

fn cmd_marker_delete_prefix(cli: &Cli, repo: &Repository, prefix: String) -> Result<()> {
    if prefix.is_empty() {
        return Err(anyhow!(marker_delete_empty_prefix_advice()));
    }

    let all = repo.refs().list_markers()?;
    let matches: Vec<MarkerName> = all
        .into_iter()
        .filter(|name| name.starts_with(&prefix))
        .collect();

    let mut deleted: Vec<MarkerEntry> = Vec::with_capacity(matches.len());
    for name in &matches {
        // Skip if it disappeared between list and delete (concurrent delete).
        if let Some(state) = repo.refs().delete_marker(name)? {
            repo.oplog().record_marker_delete(name, &state)?;
            deleted.push(MarkerEntry {
                name: name.to_string(),
                change_id: state.short(),
            });
        }
    }

    let count = deleted.len();
    let message = match count {
        0 => format!("No markers matched prefix '{}'", prefix),
        1 => format!("Deleted 1 marker matching prefix '{}'", prefix),
        n => format!("Deleted {} markers matching prefix '{}'", n, prefix),
    };

    let output = MarkerBulkDeleteOutput {
        output_kind: "thread_marker_delete",
        deleted,
        count,
        message,
    };

    if should_output_json(cli, Some(repo.config())) {
        println!("{}", serde_json::to_string(&output)?);
    } else {
        println!("{}", output.message);
        for entry in &output.deleted {
            println!("  {} -> {}", entry.name, entry.change_id);
        }
    }

    Ok(())
}

fn marker_delete_empty_prefix_advice() -> RecoveryAdvice {
    RecoveryAdvice::safety_refusal(
        "marker_delete_empty_prefix",
        "Refusing to delete markers: --prefix must be non-empty",
        "Inspect markers with `heddle thread marker list`, then rerun with a non-empty `--prefix`.",
        "an empty marker prefix matches every marker",
        "`heddle thread marker delete --prefix \"\"` would delete every marker ref",
        "no marker refs were deleted",
        "heddle thread marker list",
        vec![
            "heddle thread marker list".to_string(),
            "heddle thread marker delete --prefix <prefix>".to_string(),
        ],
    )
}

fn marker_delete_selector_conflict_advice() -> RecoveryAdvice {
    RecoveryAdvice::safety_refusal(
        "marker_delete_selector_conflict",
        "marker delete cannot combine <NAME> with --prefix",
        "Choose exactly one selector: delete one marker with `heddle thread marker delete <NAME>`, or delete a named group with `heddle thread marker delete --prefix <prefix>`.",
        "both an exact marker name and a prefix selector were supplied",
        "deleting with two selector modes would make the target marker set ambiguous",
        "no marker refs, repository objects, metadata, or worktree files were changed",
        "heddle thread marker list",
        vec![
            "heddle thread marker list".to_string(),
            "heddle thread marker delete <NAME>".to_string(),
            "heddle thread marker delete --prefix <prefix>".to_string(),
        ],
    )
}

fn marker_delete_selector_required_advice() -> RecoveryAdvice {
    RecoveryAdvice::safety_refusal(
        "marker_delete_selector_required",
        "marker delete requires <NAME> or --prefix <prefix>",
        "Inspect markers with `heddle thread marker list`, then delete one marker with `heddle thread marker delete <NAME>` or a named group with `heddle thread marker delete --prefix <prefix>`.",
        "no marker name or prefix selector was supplied",
        "deleting without a selector would have to guess which marker refs should be removed",
        "no marker refs, repository objects, metadata, or worktree files were changed",
        "heddle thread marker list",
        vec![
            "heddle thread marker list".to_string(),
            "heddle thread marker delete <NAME>".to_string(),
            "heddle thread marker delete --prefix <prefix>".to_string(),
        ],
    )
}

fn cmd_marker_show(cli: &Cli, repo: &Repository, name: String) -> Result<()> {
    let state_id = repo
        .refs()
        .get_marker(&MarkerName::new(&name))?
        .ok_or_else(|| anyhow!("Marker not found: {}", name))?;

    let state = repo
        .store()
        .get_state(&state_id)?
        .ok_or_else(|| anyhow!("State not found for marker: {}", name))?;

    let output = MarkerOpOutput {
        output_kind: "thread_marker_show",
        name: name.clone(),
        change_id: Some(state_id.short()),
        message: format!("Marker '{}' -> {}", name, state_id.short()),
    };

    if should_output_json(cli, Some(repo.config())) {
        println!("{}", serde_json::to_string(&output)?);
    } else {
        println!("Marker: {}", name);
        println!("State: {}", state_id.short());
        if let Some(intent) = &state.intent {
            println!("Intent: {}", intent);
        }
        println!("Created: {}", state.created_at.format("%Y-%m-%d %H:%M:%S"));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marker_delete_selector_advices_are_typed() {
        let conflict = marker_delete_selector_conflict_advice();
        assert_eq!(conflict.kind, "marker_delete_selector_conflict");
        assert_eq!(conflict.primary_command, "heddle thread marker list");
        assert!(conflict.primary_hint().contains("exactly one selector"));
        assert!(conflict.unsafe_condition.contains("exact marker name"));
        assert!(conflict.would_change.contains("ambiguous"));
        assert!(conflict.preserved.contains("no marker refs"));

        let required = marker_delete_selector_required_advice();
        assert_eq!(required.kind, "marker_delete_selector_required");
        assert_eq!(required.primary_command, "heddle thread marker list");
        assert!(
            required
                .primary_hint()
                .contains("heddle thread marker list")
        );
        assert!(required.unsafe_condition.contains("no marker name"));
        assert!(required.would_change.contains("guess"));
        assert!(required.preserved.contains("no marker refs"));
    }
}
