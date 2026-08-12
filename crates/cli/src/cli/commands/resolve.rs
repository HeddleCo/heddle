// SPDX-License-Identifier: Apache-2.0
//! Resolve command implementation.

use std::fs;

use anyhow::{Context, Result, anyhow};
use heddle_core::{
    ConflictRegionReport, ConflictResolutionReport, ResolveReport,
    contains_line_start_conflict_markers, path_is_active_conflict, unresolved_conflict_paths,
};
use objects::{
    object::{Attribution, StructuredConflict},
    store::ObjectStore,
};
use oplog::{ConflictResolutionMode, OpLogBackend, OpRecord};
use repo::{MergeState, Repository};

use super::{action_line::print_next_step, advice::RecoveryAdvice, snapshot::resolve_attribution};
use crate::{
    cli::{Cli, should_output_json},
    config::UserConfig,
};

#[allow(clippy::too_many_arguments)]
pub fn cmd_resolve(
    cli: &Cli,
    path: Option<String>,
    all: bool,
    list: bool,
    ours: bool,
    theirs: bool,
    force: bool,
    abort: bool,
) -> Result<()> {
    let repo = cli.open_repo()?;
    let merge_manager = repo.merge_state_manager();

    if abort {
        return cmd_resolve_abort(&repo, &merge_manager, cli);
    }

    if list {
        return cmd_resolve_list(&repo, &merge_manager, cli);
    }

    if all {
        return cmd_resolve_all(&repo, &merge_manager, cli, ours, theirs, force);
    }

    let Some(path) = path else {
        return Err(anyhow!(
            "Specify a file to resolve, or use --all, --list, or --abort"
        ));
    };

    cmd_resolve_file(&repo, &merge_manager, cli, &path, ours, theirs, force)
}

fn cmd_resolve_abort(
    repo: &Repository,
    merge_manager: &repo::MergeStateManager,
    cli: &Cli,
) -> Result<()> {
    abort_merge_state(repo, merge_manager)?;

    if should_output_json(cli, Some(repo.config())) {
        println!(
            "{}",
            serde_json::to_string(&ResolveReport {
                output_kind: "resolve".to_string(),
                message: Some("Merge aborted".to_string()),
                resolved: vec![],
                remaining: vec![],
                conflict_paths: vec![],
                conflicts: vec![],
                resolutions: vec![],
                continued: false,
                continuation_status: None,
                continuation_message: None,
                next_action: None,
                recommended_action: None,
            })?
        );
    } else {
        println!("Merge aborted");
    }

    Ok(())
}

pub(crate) fn abort_merge_state(
    repo: &Repository,
    merge_manager: &repo::MergeStateManager,
) -> Result<()> {
    let merge_state = load_merge_state_or_advice(merge_manager, "abort merge")?;
    // The 3-way merge that preceded this abort wrote a partial tree
    // (conflict markers) but did not move HEAD or the target thread
    // ref — both stay at `ours` throughout the conflicted-merge
    // window. The FF here is therefore a worktree reset to `ours`,
    // not a thread advance, so the recorded `FastForward`'s
    // `pre_target_id` and `post_target_id` are equal. Migrated as
    // part of the heddle#110 Rule-7 sweep for uniformity with the
    // other `fast_forward_attached` callers: a future merge variant
    // that *does* move HEAD before aborting (e.g. a partial-apply
    // shape) would then get correct undo semantics for free without
    // a second migration.
    super::ff_record::record_ff_advance_discard_local(repo, "<abort>", &merge_state.ours)?;
    merge_manager.abort()?;
    Ok(())
}

fn cmd_resolve_list(
    repo: &Repository,
    merge_manager: &repo::MergeStateManager,
    cli: &Cli,
) -> Result<()> {
    let merge_state = load_merge_state_or_advice(merge_manager, "list merge conflicts")?;
    let unresolved = unresolved_paths(&merge_state);
    let conflicts = structured_conflicts_for_paths(repo, &merge_state, &unresolved)?;

    if should_output_json(cli, Some(repo.config())) {
        println!(
            "{}",
            serde_json::to_string(&ResolveReport {
                output_kind: "resolve".to_string(),
                message: None,
                resolved: Vec::new(),
                remaining: unresolved.clone(),
                conflict_paths: unresolved.clone(),
                conflicts,
                resolutions: Vec::new(),
                continued: false,
                continuation_status: None,
                continuation_message: None,
                next_action: None,
                recommended_action: None,
            })?
        );
    } else if unresolved.is_empty() {
        println!("No unresolved conflicts");
    } else {
        for path in &unresolved {
            println!("{}", path);
            for conflict in conflicts.iter().filter(|conflict| &conflict.path == path) {
                print_conflict_region(conflict);
            }
        }
    }

    Ok(())
}

fn cmd_resolve_all(
    repo: &Repository,
    merge_manager: &repo::MergeStateManager,
    cli: &Cli,
    ours: bool,
    theirs: bool,
    force: bool,
) -> Result<()> {
    let merge_state = load_merge_state_or_advice(merge_manager, "resolve merge conflicts")?;
    let unresolved = unresolved_paths(&merge_state);

    if unresolved.is_empty() {
        return Err(anyhow!(no_conflicts_to_resolve_advice()));
    }
    let resolver = resolve_attribution(repo, &UserConfig::load_default()?)?;
    let mode = manual_resolution_mode(ours, theirs);
    let conflicts = structured_conflicts_for_paths(repo, &merge_state, &unresolved)?;
    let mut resolutions = Vec::new();

    for path in &unresolved {
        resolve_file_with_version(repo, &merge_state, path, ours, theirs)?;
        ensure_resolved_file_has_no_conflict_markers(repo, path, ours || theirs, force)?;
        merge_manager.resolve(path)?;
        resolutions.extend(record_conflicts_resolved(
            repo, path, &conflicts, &resolver, mode,
        )?);
    }

    let remaining = merge_manager.unresolved()?;
    let continuation = continue_if_resolution_complete(repo, remaining.is_empty())?;
    let output = resolve_output(
        format!("Resolved {} conflict(s)", unresolved.len()),
        unresolved.clone(),
        remaining.clone(),
        unresolved.clone(),
        conflicts,
        resolutions,
        continuation,
    );

    if should_output_json(cli, Some(repo.config())) {
        println!("{}", serde_json::to_string(&output)?);
    } else {
        println!("{}", output.message.as_deref().unwrap_or_default());
        for path in &unresolved {
            println!("  {}", path);
        }
        for resolution in &output.resolutions {
            print_resolution(resolution);
        }
        if !remaining.is_empty() {
            println!("Remaining: {} conflict(s)", remaining.len());
        }
        print_continuation(&output);
    }

    Ok(())
}

fn cmd_resolve_file(
    repo: &Repository,
    merge_manager: &repo::MergeStateManager,
    cli: &Cli,
    path: &str,
    ours: bool,
    theirs: bool,
    force: bool,
) -> Result<()> {
    let merge_state = load_merge_state_or_advice(merge_manager, "resolve merge conflict")?;
    if !path_is_active_conflict(&merge_state.conflicts, path) {
        return Err(anyhow!(path_not_in_active_merge_advice(path)));
    }
    let resolver = resolve_attribution(repo, &UserConfig::load_default()?)?;
    let mode = manual_resolution_mode(ours, theirs);
    let conflict_paths = vec![path.to_string()];
    let conflicts = structured_conflicts_for_paths(repo, &merge_state, &conflict_paths)?;
    resolve_file_with_version(repo, &merge_state, path, ours, theirs)?;
    ensure_resolved_file_has_no_conflict_markers(repo, path, ours || theirs, force)?;
    merge_manager.resolve(path)?;
    let resolutions = record_conflicts_resolved(repo, path, &conflicts, &resolver, mode)?;

    let remaining = merge_manager.unresolved()?;
    let continuation = continue_if_resolution_complete(repo, remaining.is_empty())?;
    let output = resolve_output(
        format!("Resolved {}", path),
        vec![path.to_string()],
        remaining.clone(),
        conflict_paths,
        conflicts,
        resolutions,
        continuation,
    );

    if should_output_json(cli, Some(repo.config())) {
        println!("{}", serde_json::to_string(&output)?);
    } else {
        println!("{}", output.message.as_deref().unwrap_or_default());
        if !remaining.is_empty() {
            println!("{} conflict(s) remaining", remaining.len());
        }
        for resolution in &output.resolutions {
            print_resolution(resolution);
        }
        print_continuation(&output);
    }

    Ok(())
}

fn continue_if_resolution_complete(
    repo: &Repository,
    complete: bool,
) -> Result<Option<super::operator_core::OperatorCommandOutput>> {
    if complete {
        super::operator_core::continue_operator(repo).map(Some)
    } else {
        Ok(None)
    }
}

fn manual_resolution_mode(ours: bool, theirs: bool) -> ConflictResolutionMode {
    if ours {
        ConflictResolutionMode::Ours
    } else if theirs {
        ConflictResolutionMode::Theirs
    } else {
        ConflictResolutionMode::Edit
    }
}

fn record_conflicts_resolved(
    repo: &Repository,
    path: &str,
    conflicts: &[ConflictRegionReport],
    resolver: &Attribution,
    mode: ConflictResolutionMode,
) -> Result<Vec<ConflictResolutionReport>> {
    let conflict_ids: Vec<String> = conflicts
        .iter()
        .filter(|conflict| conflict.path == path)
        .map(|conflict| conflict.id.clone())
        .collect();
    let conflict_ids = if conflict_ids.is_empty() {
        vec![path.to_string()]
    } else {
        conflict_ids
    };
    repo.oplog().record_batch_scoped(
        conflict_ids
            .iter()
            .map(|conflict_id| OpRecord::conflict_resolved(conflict_id, resolver.clone(), mode))
            .collect(),
        Some(&repo.op_scope()),
    )?;
    Ok(conflict_ids
        .into_iter()
        .map(|conflict_id| ConflictResolutionReport::new(conflict_id, path, resolver, mode))
        .collect())
}

fn structured_conflicts_for_paths(
    repo: &Repository,
    merge_state: &MergeState,
    paths: &[String],
) -> Result<Vec<ConflictRegionReport>> {
    let Some(payload_id) = merge_state.structured_conflicts else {
        return Ok(Vec::new());
    };
    let blob = repo.require_blob(&payload_id)?;
    if blob.hash() != payload_id {
        return Err(anyhow!(
            "structured conflict payload {} failed BLAKE3 verification",
            payload_id
        ));
    }
    let payload = StructuredConflict::decode(blob.content())?;
    for conflict in &payload.conflicts {
        verify_conflict_side(repo, &conflict.base)?;
        verify_conflict_side(repo, &conflict.ours)?;
        verify_conflict_side(repo, &conflict.theirs)?;
    }
    Ok(payload
        .conflicts
        .iter()
        .filter(|conflict| paths.contains(&conflict.path))
        .map(Into::into)
        .collect())
}

fn verify_conflict_side(repo: &Repository, side: &objects::object::ConflictSide) -> Result<()> {
    let bytes = match side.blob_id {
        Some(blob_id) => repo.require_blob(&blob_id)?.content().to_vec(),
        None => Vec::new(),
    };
    side.verify_blob(&bytes)?;
    Ok(())
}

fn print_conflict_region(conflict: &ConflictRegionReport) {
    let symbol = conflict
        .symbol
        .as_deref()
        .map(|symbol| format!(" ({symbol})"))
        .unwrap_or_default();
    println!(
        "  {}{} lines {}..{}",
        conflict.id, symbol, conflict.merged_range.start_line, conflict.merged_range.end_line
    );
    print_conflict_side("base", &conflict.base);
    print_conflict_side("ours", &conflict.ours);
    print_conflict_side("theirs", &conflict.theirs);
}

fn print_conflict_side(label: &str, side: &heddle_core::ConflictSideReport) {
    println!(
        "    {label}: state {} blob {} lines {}..{} hunk {}",
        side.source_state,
        side.blob_id.as_deref().unwrap_or("<absent>"),
        side.range.start_line,
        side.range.end_line,
        side.hunk_hash
    );
}

fn print_resolution(resolution: &ConflictResolutionReport) {
    let actor = resolution
        .resolver
        .agent
        .as_ref()
        .map(|agent| format!("{}/{}", agent.provider, agent.model))
        .unwrap_or_else(|| resolution.resolver.principal.name.clone());
    println!(
        "  {}: {} by {}",
        resolution.conflict_id, resolution.mode, actor
    );
}

fn resolve_output(
    message: String,
    resolved: Vec<String>,
    remaining: Vec<String>,
    conflict_paths: Vec<String>,
    conflicts: Vec<ConflictRegionReport>,
    resolutions: Vec<ConflictResolutionReport>,
    continuation: Option<super::operator_core::OperatorCommandOutput>,
) -> ResolveReport {
    let continued = continuation.is_some();
    let continuation_status = continuation.as_ref().map(|output| output.status.clone());
    let continuation_message = continuation.as_ref().map(|output| output.message.clone());
    let next_action = continuation
        .as_ref()
        .and_then(|output| output.next_action.clone());
    let recommended_action = continuation
        .as_ref()
        .and_then(|output| output.recommended_action.clone());
    let message = if continued {
        format!("{message}; completed merge")
    } else {
        message
    };
    ResolveReport {
        output_kind: "resolve".to_string(),
        message: Some(message),
        resolved,
        remaining,
        conflict_paths,
        conflicts,
        resolutions,
        continued,
        continuation_status,
        continuation_message,
        next_action,
        recommended_action,
    }
}

fn print_continuation(output: &ResolveReport) {
    if let Some(message) = output.continuation_message.as_deref() {
        println!("{message}");
    }
    if let Some(action) = output
        .recommended_action
        .as_deref()
        .or(output.next_action.as_deref())
    {
        print_next_step(action);
    }
}

fn ensure_resolved_file_has_no_conflict_markers(
    repo: &Repository,
    path: &str,
    selected_side: bool,
    force: bool,
) -> Result<()> {
    if selected_side || force {
        return Ok(());
    }
    let full_path = repo.root().join(path);
    let content = fs::read(&full_path)
        .with_context(|| format!("read resolved conflict candidate {}", full_path.display()))?;
    if contains_line_start_conflict_markers(&content) {
        return Err(anyhow!(conflict_markers_still_present_advice(path)));
    }
    Ok(())
}

fn resolve_file_with_version(
    repo: &Repository,
    merge_state: &MergeState,
    path: &str,
    ours: bool,
    theirs: bool,
) -> Result<()> {
    if !ours && !theirs {
        return Ok(());
    }

    let full_path = repo.root().join(path);

    if ours {
        let our_state = repo
            .store()
            .get_state(&merge_state.ours)?
            .ok_or_else(|| anyhow!("Our state not found"))?;
        let our_tree = repo.require_tree(&our_state.tree)?;

        if let Some(entry) = our_tree.get(path) {
            let Some(hash) = entry.leaf_content_hash() else {
                return Ok(());
            };
            let blob = repo.require_blob(&hash)?;
            fs::write(&full_path, blob.content())?;
        }
    } else if theirs {
        let their_state = repo
            .store()
            .get_state(&merge_state.theirs)?
            .ok_or_else(|| anyhow!("Their state not found"))?;
        let their_tree = repo.require_tree(&their_state.tree)?;

        if let Some(entry) = their_tree.get(path) {
            let Some(hash) = entry.leaf_content_hash() else {
                return Ok(());
            };
            let blob = repo.require_blob(&hash)?;
            fs::write(&full_path, blob.content())?;
        }
    }

    Ok(())
}

fn load_merge_state_or_advice(
    merge_manager: &repo::MergeStateManager,
    action: &'static str,
) -> Result<MergeState> {
    merge_manager
        .load()?
        .ok_or_else(|| anyhow!(no_merge_in_progress_advice(action)))
}

fn unresolved_paths(merge_state: &MergeState) -> Vec<String> {
    unresolved_conflict_paths(&merge_state.conflicts, &merge_state.resolved)
}

fn no_merge_in_progress_advice(action: &'static str) -> RecoveryAdvice {
    RecoveryAdvice::safety_refusal(
        "no_merge_in_progress",
        "No merge in progress",
        "Inspect the current operation state with `heddle status`.",
        "the repository has no persisted Heddle merge state",
        format!("{action} would need to read or update conflict state for an active merge"),
        "repository state was left unchanged",
        "heddle status",
        vec!["heddle status".to_string()],
    )
}

fn no_conflicts_to_resolve_advice() -> RecoveryAdvice {
    RecoveryAdvice::safety_refusal(
        "no_conflicts_to_resolve",
        "No conflicts to resolve",
        "Inspect the current conflict set with `heddle resolve --list`.",
        "the active merge has no unresolved conflict paths",
        "resolve --all would not update any files or merge state",
        "repository state was left unchanged",
        "heddle resolve --list",
        vec!["heddle resolve --list".to_string()],
    )
}

fn path_not_in_active_merge_advice(path: &str) -> RecoveryAdvice {
    RecoveryAdvice::safety_refusal(
        "conflict_path_not_found",
        format!("No active merge conflict is registered for {path}"),
        "Inspect unresolved conflicts with `heddle resolve --list`.",
        format!("{path} is not in the active merge conflict set"),
        "marking an unregistered path resolved would make the merge state disagree with the worktree",
        "repository state was left unchanged",
        "heddle resolve --list",
        vec!["heddle resolve --list".to_string()],
    )
}

fn conflict_markers_still_present_advice(path: &str) -> RecoveryAdvice {
    RecoveryAdvice::safety_refusal(
        "conflict_markers_still_present",
        format!("Refusing to mark {path} resolved while conflict markers remain"),
        format!(
            "Edit {path} to remove `<<<<<<<`, `=======`, and `>>>>>>>`, then rerun `heddle resolve {path}`. Use `--ours`, `--theirs`, or `--force` only when intentional."
        ),
        format!("{path} still contains conflict marker lines"),
        "continuing the merge would capture unresolved marker text as the resolved file content",
        "the merge state, refs, objects, and worktree files were left unchanged",
        "heddle resolve --list".to_string(),
        vec![
            "heddle resolve --list".to_string(),
            format!("heddle resolve {path}"),
            format!("heddle resolve {path} --force"),
        ],
    )
}
