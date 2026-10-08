// SPDX-License-Identifier: Apache-2.0
//! Resolve command implementation.

use std::{collections::HashMap, fs};

use anyhow::{Context, Result, anyhow};
use objects::{
    HeddleError,
    object::{Attribution, ConflictSide, StateId, StructuredConflict},
    store::ObjectStore,
};
use oplog::{ConflictResolutionMode, OpLogBackend, OpRecord};
#[cfg(feature = "client")]
use repo::thread_replication::source_heads::DefaultHeadRule;
use repo::{MergeState, Repository};
use verbs::{
    ConflictRegionReport, ConflictResolutionReport, ResolveReport,
    contains_line_start_conflict_markers, path_is_active_conflict,
    source_heads::{
        MergeSourceHeadOutcome, SourceHeadResolutionMode, SourceHeadResolutionReport,
        SourceHeadsReport, merge_source_head, pick_source_head, select_source_head,
        source_heads_report,
    },
    unresolved_conflict_paths,
};

use super::{
    action_line::print_next_step,
    advice::RecoveryAdvice,
    next_action::{NextActionValidationContext, normalized_action, write_full_command_json},
    snapshot::resolve_attribution,
};
use crate::{
    cli::{Cli, ResolveArgs, should_output_json},
    config::UserConfig,
};

pub fn cmd_resolve(cli: &Cli, args: ResolveArgs) -> Result<()> {
    let ResolveArgs {
        path,
        all,
        list,
        ours,
        theirs,
        force,
        heads,
        pick,
        merge,
    } = args;
    let repo = cli.open_repo()?;
    if heads {
        return cmd_resolve_heads(&repo, cli);
    }
    if let Some(selector) = pick {
        return cmd_resolve_source_head(&repo, cli, &selector, SourceHeadResolutionMode::Pick);
    }
    if let Some(selector) = merge {
        return cmd_resolve_source_head(&repo, cli, &selector, SourceHeadResolutionMode::Merge);
    }
    let merge_manager = repo.merge_state_manager();

    if list {
        return cmd_resolve_list(&repo, &merge_manager, cli);
    }

    if all {
        return cmd_resolve_all(&repo, &merge_manager, cli, ours, theirs, force);
    }

    let Some(path) = path else {
        return Err(anyhow!(missing_resolve_target_advice()));
    };

    cmd_resolve_file(&repo, &merge_manager, cli, &path, ours, theirs, force)
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
                source_heads: None,
                head_resolution: None,
            })?
        );
    } else if unresolved.is_empty() {
        println!("No unresolved conflicts");
    } else {
        for path in &unresolved {
            println!("{}", path);
            for conflict in conflicts.iter().filter(|conflict| &conflict.path == path) {
                render_conflict_region(conflict);
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
    let continuation = continue_if_resolution_complete(repo, remaining.is_empty(), &resolver)?;
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
        write_full_command_json(
            &output,
            NextActionValidationContext::without_repo(&["resolve"]),
        )?;
    } else {
        println!("{}", output.message.as_deref().unwrap_or_default());
        for path in &unresolved {
            println!("  {}", path);
        }
        for resolution in &output.resolutions {
            render_resolution(resolution);
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
    let continuation = continue_if_resolution_complete(repo, remaining.is_empty(), &resolver)?;
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
        write_full_command_json(
            &output,
            NextActionValidationContext::without_repo(&["resolve"]),
        )?;
    } else {
        println!("{}", output.message.as_deref().unwrap_or_default());
        if !remaining.is_empty() {
            println!("{} conflict(s) remaining", remaining.len());
        }
        for resolution in &output.resolutions {
            render_resolution(resolution);
        }
        print_continuation(&output);
    }

    Ok(())
}

fn continue_if_resolution_complete(
    repo: &Repository,
    complete: bool,
    resolver: &Attribution,
) -> Result<Option<super::operator_core::OperatorCommandOutput>> {
    if complete {
        super::operator_core::continue_operator(repo, Some(resolver)).map(Some)
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
    let mut attributions = HashMap::new();
    payload
        .conflicts
        .iter()
        .filter(|conflict| paths.contains(&conflict.path))
        .map(|conflict| {
            Ok(ConflictRegionReport::new(
                conflict,
                &side_attribution(repo, &conflict.base, &mut attributions)?,
                &side_attribution(repo, &conflict.ours, &mut attributions)?,
                &side_attribution(repo, &conflict.theirs, &mut attributions)?,
            ))
        })
        .collect::<Result<Vec<_>>>()
}

fn side_attribution(
    repo: &Repository,
    side: &ConflictSide,
    attributions: &mut HashMap<StateId, Attribution>,
) -> Result<Attribution> {
    if let Some(attribution) = attributions.get(&side.source_state) {
        return Ok(attribution.clone());
    }
    let attribution = repo
        .store()
        .get_state(&side.source_state)?
        .map(|state| state.attribution)
        .ok_or(HeddleError::StateNotFound(side.source_state))?;
    attributions.insert(side.source_state, attribution.clone());
    Ok(attribution)
}

fn verify_conflict_side(repo: &Repository, side: &objects::object::ConflictSide) -> Result<()> {
    let bytes = match side.blob_id {
        Some(blob_id) => repo.require_blob(&blob_id)?.content().to_vec(),
        None => Vec::new(),
    };
    side.verify_blob(&bytes)?;
    Ok(())
}

fn render_conflict_region(conflict: &ConflictRegionReport) {
    let symbol = conflict
        .symbol
        .as_deref()
        .map(|symbol| format!(" ({symbol})"))
        .unwrap_or_default();
    println!(
        "  {}{} lines {}..{}",
        conflict.id, symbol, conflict.merged_range.start_line, conflict.merged_range.end_line
    );
    render_conflict_side("base", &conflict.base);
    render_conflict_side("ours", &conflict.ours);
    render_conflict_side("theirs", &conflict.theirs);
}

fn render_conflict_side(label: &str, side: &verbs::ConflictSideReport) {
    let actor = side
        .producer
        .agent
        .as_ref()
        .map(|agent| format!("{}/{}", agent.provider, agent.model))
        .unwrap_or_else(|| side.producer.principal.name.clone());
    println!(
        "    {label}: state {} by {} (claimed) blob {} lines {}..{} hunk {}",
        side.source_state,
        actor,
        side.blob_id.as_deref().unwrap_or("<absent>"),
        side.range.start_line,
        side.range.end_line,
        side.hunk_hash
    );
}

fn render_resolution(resolution: &ConflictResolutionReport) {
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
        source_heads: None,
        head_resolution: None,
    }
}

/// `resolve --heads`: every concurrent source head of the current Thread,
/// with its claimed producer and the exact pick / merge commands.
fn cmd_resolve_heads(repo: &Repository, cli: &Cli) -> Result<()> {
    let thread = verbs::source_heads::attached_thread(repo)?;
    let heads = source_heads_report(repo, &thread, None)?;
    let message = match &heads {
        Some(heads) => format!(
            "Thread '{thread}' has {} concurrent source heads",
            heads.heads.len()
        ),
        None => format!("Thread '{thread}' has no alternative source heads"),
    };
    if should_output_json(cli, Some(repo.config())) {
        let mut output = source_head_output(message);
        output.source_heads = heads;
        return write_full_command_json(
            &output,
            NextActionValidationContext::without_repo(&["resolve"]),
        );
    }
    println!("{message}");
    if let Some(heads) = &heads {
        render_source_heads(heads);
    }
    Ok(())
}

/// Clone and pull: say which head is checked out, why, and what remains.
#[cfg(feature = "client")]
pub(crate) fn render_source_heads_summary(heads: &SourceHeadsReport) {
    let rule = heads
        .selected_by
        .as_deref()
        .and_then(DefaultHeadRule::parse)
        .map(DefaultHeadRule::describe)
        .unwrap_or("the local Thread tip");
    println!(
        "  thread '{}' has {} concurrent source heads; checked out {} ({rule})",
        heads.thread,
        heads.heads.len(),
        heads.current.as_deref().unwrap_or("none"),
    );
    render_source_heads(heads);
    print_next_step(verbs::source_heads::SOURCE_HEADS_ACTION);
}

/// Human rendering shared by `resolve --heads`, clone and pull.
pub(crate) fn render_source_heads(heads: &SourceHeadsReport) {
    for head in &heads.heads {
        let actor = head
            .producer
            .agent
            .as_ref()
            .map(|agent| format!("{}/{}", agent.provider, agent.model))
            .unwrap_or_else(|| head.producer.principal.name.clone());
        let current = if head.current { " (checked out)" } else { "" };
        let intent = head
            .intent
            .as_deref()
            .map(|intent| format!(" \"{intent}\""))
            .unwrap_or_default();
        println!("  {}{current} by {actor} (claimed){intent}", head.state);
        println!("    pick:  {}", head.pick_action);
        if let Some(merge) = &head.merge_action {
            println!("    merge: {merge}");
        }
    }
}

fn source_head_output(message: String) -> ResolveReport {
    ResolveReport {
        output_kind: "resolve".to_string(),
        message: Some(message),
        resolved: Vec::new(),
        remaining: Vec::new(),
        conflict_paths: Vec::new(),
        conflicts: Vec::new(),
        resolutions: Vec::new(),
        continued: false,
        continuation_status: None,
        continuation_message: None,
        next_action: None,
        recommended_action: None,
        source_heads: None,
        head_resolution: None,
    }
}

/// `resolve --pick` / `resolve --merge`: retire concurrent source heads with
/// one attributed capture.
fn cmd_resolve_source_head(
    repo: &Repository,
    cli: &Cli,
    selector: &str,
    mode: SourceHeadResolutionMode,
) -> Result<()> {
    let selection = select_source_head(repo, selector)?;
    let attribution = resolve_attribution(repo, &UserConfig::load_default()?)?;
    let selected = selection.selected.to_string_full();
    let (state, conflicts) = match mode {
        SourceHeadResolutionMode::Pick => (
            Some(pick_source_head(repo, &selection, attribution)?),
            Vec::new(),
        ),
        SourceHeadResolutionMode::Merge => {
            match merge_source_head(repo, &selection, attribution)? {
                MergeSourceHeadOutcome::Merged(state) => (Some(state), Vec::new()),
                MergeSourceHeadOutcome::Conflicted(paths) => (None, paths),
            }
        }
    };
    let parents = match state {
        Some(state) => repo
            .store()
            .get_state(&state)?
            .ok_or(HeddleError::StateNotFound(state))?
            .parents
            .iter()
            .map(StateId::to_string_full)
            .collect(),
        None => Vec::new(),
    };
    let verb = match mode {
        SourceHeadResolutionMode::Pick => "Picked",
        SourceHeadResolutionMode::Merge => "Merged",
    };
    let message = match state {
        Some(state) => format!(
            "{verb} source head {} on thread '{}' as {}",
            selection.selected.short(),
            selection.thread,
            state.short()
        ),
        None => format!(
            "Merging source head {} on thread '{}' stopped on {} conflict(s)",
            selection.selected.short(),
            selection.thread,
            conflicts.len()
        ),
    };
    let remaining = source_heads_report(repo, &selection.thread, None)?;
    // Publishing the resolution collapses the hosted heads too.
    let next_action = state.and(normalized_action(publish_action(repo)));
    let structured = if conflicts.is_empty() {
        Vec::new()
    } else {
        match repo.merge_state_manager().load()? {
            Some(merge_state) => structured_conflicts_for_paths(repo, &merge_state, &conflicts)?,
            None => Vec::new(),
        }
    };
    if should_output_json(cli, Some(repo.config())) {
        let mut output = source_head_output(message);
        output.remaining = conflicts.clone();
        output.conflict_paths = conflicts;
        output.conflicts = structured;
        output.recommended_action = next_action.clone();
        output.next_action = next_action;
        output.source_heads = remaining;
        output.head_resolution = Some(SourceHeadResolutionReport {
            mode,
            thread: selection.thread.clone(),
            selected,
            state: state.map(|state| state.to_string_full()),
            parents,
        });
        return write_full_command_json(
            &output,
            NextActionValidationContext::without_repo(&["resolve"]),
        );
    }
    println!("{message}");
    for conflict in &structured {
        render_conflict_region(conflict);
    }
    if !conflicts.is_empty() {
        for path in &conflicts {
            println!("  {path}");
        }
        println!(
            "Resolve each path with `heddle resolve <path>`; the merge completes when none remain."
        );
    }
    if let Some(remaining) = &remaining {
        println!(
            "{} other source head(s) remain on thread '{}':",
            remaining.heads.len().saturating_sub(1),
            selection.thread
        );
        render_source_heads(remaining);
    }
    if let Some(action) = next_action.as_deref() {
        print_next_step(action);
    }
    Ok(())
}

fn publish_action(repo: &Repository) -> String {
    if verbs::status::default_remote_name(repo).is_some() {
        "heddle push".to_string()
    } else {
        "heddle status".to_string()
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

fn missing_resolve_target_advice() -> RecoveryAdvice {
    RecoveryAdvice::safety_refusal(
        "resolve_target_required",
        "Specify a file to resolve, or use --all or --list",
        "Inspect unresolved conflicts with `heddle resolve --list`.",
        "no conflict path or bulk/list mode was selected",
        "resolve cannot choose a conflict path on the operator's behalf",
        "repository state and worktree files were left unchanged",
        "heddle resolve --list",
        vec!["heddle resolve --list".to_string()],
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
