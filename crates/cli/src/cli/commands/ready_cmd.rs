// SPDX-License-Identifier: Apache-2.0
//! Ready command implementation.

use anyhow::Result;
use chrono::Utc;
use objects::object::Tree;
use repo::{
    GitOverlayImportHint, GitRemoteTrackingStatus, Repository, RepositoryOperationStatus,
    ThreadState,
};
use serde::Serialize;

use super::{
    action_line::print_next,
    advice::RecoveryAdvice,
    git_overlay_health::{
        RepositoryVerificationState, build_plain_git_verification_probe,
        build_repository_verification_state, override_trust_recommended_action,
    },
    merge::{ThreadPreviewReport, build_thread_preview_report},
    next_action::{
        NextActionInput, NextActionValidationContext, effective_next_action, non_empty_action,
        normalized_action, write_command_json,
    },
    operator_core::{
        OperatorAction, OperatorCommandOutput, VerificationClaimPolicy,
        exit_if_blocked_operator_status,
    },
    snapshot::{SnapshotAgentOverrides, create_snapshot, ensure_current_state},
    thread::contextual_thread_action,
    thread_cmd::{current_thread, load_thread, thread_manager, thread_not_found_advice},
    thread_landing::land_local_command,
};
use crate::{
    cli::{Cli, ReadyArgs, output_is_compact, should_output_json, style, worktree_status_options},
    config::UserConfig,
};

#[derive(Serialize)]
struct ReadyOutput {
    #[serde(flatten)]
    operator: OperatorCommandOutput,
    captured: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    captured_state: Option<String>,
    thread_state: String,
    #[serde(skip_serializing)]
    #[serde(rename = "verification")]
    trust: RepositoryVerificationState,
    report: ThreadPreviewReport,
}

pub async fn cmd_ready(cli: &Cli, args: ReadyArgs) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let start = cli.repo.as_ref().unwrap_or(&cwd);
    if let Some(probe) = build_plain_git_verification_probe(start)? {
        let output = trust_blocked_ready_output(args.thread.as_deref(), probe.trust);
        write_ready_output_without_repo(cli, &output)?;
        return Ok(());
    }

    let repo = Repository::open(start)?;
    let user_config = UserConfig::load_default().unwrap_or_default();
    let initial_trust = build_repository_verification_state(&repo);
    if ready_verification_preflight_blocks(&initial_trust) {
        let output = trust_blocked_ready_output(args.thread.as_deref(), initial_trust);
        write_ready_output(cli, &repo, &output)?;
        return Ok(());
    }
    let had_current_state = repo.current_state()?.is_some();
    let status_options = worktree_status_options(Some(repo.config()));
    let bootstrap_dirty = if !had_current_state {
        worktree_dirty(&repo, &status_options)?
    } else {
        false
    };
    let mut captured_state = None;
    if !had_current_state && bootstrap_dirty && args.message.is_none() {
        let dirty_paths = worktree_dirty_paths(&repo, &status_options)?;
        let output = missing_ready_capture_intent_output(
            &repo,
            args.thread.as_deref(),
            dirty_paths,
            initial_trust,
        )?;
        write_ready_output(cli, &repo, &output)?;
        return Ok(());
    }
    if !had_current_state {
        let bootstrap_state = ensure_current_state(
            &repo,
            &user_config,
            args.message
                .clone()
                .or_else(|| Some("Bootstrap git-overlay readiness state".to_string())),
        )?;
        if bootstrap_dirty {
            captured_state = Some(bootstrap_state.short());
        }
    }
    let manager = thread_manager(&repo);
    let mut thread = match args.thread.clone() {
        Some(thread_id) => load_thread(&repo, &thread_id)?,
        None => current_thread(&repo)?.ok_or_else(|| {
            anyhow::anyhow!(RecoveryAdvice::no_current_thread(
                "ready",
                Some("--thread"),
                "heddle ready --thread <name>",
            ))
        })?,
    };

    let preflight_trust = build_repository_verification_state(&repo);
    if ready_verification_preflight_blocks(&preflight_trust) {
        let mut report = build_thread_preview_report(&repo, &mut thread, true)?;
        report.thread_state = "blocked".to_string();
        report.freshness = "not_checked".to_string();
        report.merge_relation = "blocked".to_string();
        report.thread_health = "blocked".to_string();
        if report.blockers.is_empty() {
            report
                .blockers
                .push("repository verification needs import".to_string());
        }
        let recommended_action = preflight_trust.recommended_action.clone();
        let trust_blockers = preflight_trust
            .checks
            .iter()
            .filter(|check| !check.clean)
            .map(|check| format!("{}: {}", check.name, check.summary))
            .collect::<Vec<_>>();
        let message = format!(
            "Thread '{}' cannot run readiness checks until repository verification is restored: {}",
            thread.id, preflight_trust.summary
        );
        let output = ReadyOutput {
            operator: OperatorCommandOutput {
                status: "blocked".to_string(),
                action: OperatorAction::Ready,
                message: message.clone(),
                blockers: trust_blockers,
                warnings: Vec::new(),
                next_action: Some(recommended_action.clone()),
                recommended_action: Some(recommended_action.clone()),
            },
            captured: false,
            captured_state: None,
            thread_state: "blocked".to_string(),
            trust: preflight_trust,
            report,
        };
        write_ready_output(cli, &repo, &output)?;
        return Ok(());
    }

    let mut captured = !had_current_state && bootstrap_dirty;
    let dirty = worktree_dirty(&repo, &status_options)?;
    if dirty {
        if args.message.is_none() {
            let dirty_paths = worktree_dirty_paths(&repo, &status_options)?;
            let output = missing_ready_capture_intent_output(
                &repo,
                Some(&thread.id),
                dirty_paths,
                preflight_trust,
            )?;
            write_ready_output(cli, &repo, &output)?;
            return Ok(());
        }
        let snapshot = create_snapshot(
            &repo,
            &user_config,
            args.message.clone(),
            args.confidence,
            SnapshotAgentOverrides {
                provider: None,
                model: None,
                session: None,
                segment: None,
                policy: None,
                no_policy: false,
                no_agent: false,
            },
        )?;
        captured_state = Some(snapshot.change_id);
        thread = manager
            .load(&thread.id)?
            .or_else(|| current_thread(&repo).ok().flatten())
            .ok_or_else(|| {
                anyhow::anyhow!(thread_not_found_advice(&thread.id, "ready after capture"))
            })?;
        captured = true;
    }

    let mut report = build_thread_preview_report(&repo, &mut thread, true)?;
    let has_integration_target = report.merge_relation != "no_target";
    if has_integration_target {
        let policy_blockers = super::workflow::auto_land_policy_blockers(&repo, &thread);
        if !policy_blockers.is_empty() {
            for blocker in policy_blockers {
                if !report.blockers.contains(&blocker) {
                    report.blockers.push(blocker);
                }
            }
            let recovery_scope = super::workflow::recovery_scope_checkout(&thread, repo.root());
            if let Some(action) = super::workflow::integration_blocker_recommended_action(
                &report.blockers,
                recovery_scope.as_deref(),
            ) {
                report.recommended_action = action;
                report.refresh_recommended_action_metadata();
            }
            report.thread_health = "blocked".to_string();
        }
    }
    if !has_integration_target && report.conflict_count == 0 && report.blockers.is_empty() {
        report.recommended_action.clear();
        report.refresh_recommended_action_metadata();
        report.thread_health = "clean".to_string();
    }
    let already_ready = has_integration_target
        && !captured
        && thread.state == ThreadState::Ready
        && report.conflict_count == 0
        && report.blockers.is_empty();

    let ready_without_target =
        !has_integration_target && report.conflict_count == 0 && report.blockers.is_empty();

    if !already_ready && has_integration_target {
        thread.state = if report.conflict_count == 0 && report.blockers.is_empty() {
            ThreadState::Ready
        } else {
            ThreadState::Blocked
        };
        thread.updated_at = Utc::now();
        manager.save(&thread)?;
        report.thread_state = thread.state.to_string();
    }
    if has_integration_target
        && thread.state == ThreadState::Ready
        && report.conflict_count == 0
        && report.blockers.is_empty()
    {
        report.thread_health = "ready".to_string();
        report.recommended_action = land_local_command(&thread.id);
        report.refresh_recommended_action_metadata();
    }

    let message = if already_ready {
        format!("Thread '{}' is already ready", thread.id)
    } else if ready_without_target {
        format!(
            "Thread '{}' is clean; no integration target is configured",
            thread.id
        )
    } else if thread.state == ThreadState::Ready {
        format!("Thread '{}' is ready to integrate", thread.id)
    } else {
        format!("Thread '{}' is blocked", thread.id)
    };
    let operation = repo.operation_status()?;
    let remote_tracking = repo.git_remote_tracking_status()?;
    let import_hint = repo.git_overlay_import_hint()?;
    let mut trust = build_repository_verification_state(&repo);
    let report_recommended_action = ready_report_recommended_action(&report);
    let recommended_action = ready_scoped_next_action(
        operation.as_ref(),
        remote_tracking.as_ref(),
        import_hint.as_ref(),
        report_recommended_action.as_deref(),
    );
    let recommended_action = contextual_thread_action(
        &repo,
        &thread.id,
        thread.target_thread.as_deref(),
        &recommended_action,
    );
    let report_action_selected = report_recommended_action
        .as_deref()
        .map(|action| {
            contextual_thread_action(&repo, &thread.id, thread.target_thread.as_deref(), action)
        })
        .is_some_and(|action| action == recommended_action);
    if report_action_selected
        && !recommended_action.is_empty()
        && report.recommended_action != recommended_action
    {
        report.recommended_action = recommended_action.clone();
        report.refresh_recommended_action_metadata();
    }
    let recommended_action_value = normalized_action(recommended_action.clone());

    let status = if thread.state == ThreadState::Ready || !has_integration_target {
        "completed"
    } else {
        "blocked"
    };
    let mut operator = OperatorCommandOutput {
        status: status.to_string(),
        action: OperatorAction::Ready,
        message: message.clone(),
        blockers: report.blockers.clone(),
        warnings: Vec::new(),
        next_action: recommended_action_value.clone(),
        recommended_action: recommended_action_value,
    };
    operator.block_success_claim_if_verification_blocked(
        &trust,
        format!("Thread '{}' readiness", thread.id),
        VerificationClaimPolicy::strict().allow_matching_workflow_action(),
    );
    if !matches!(operator.status.as_str(), "blocked" | "failed")
        && !recommended_action.is_empty()
        && trust.recommended_action != recommended_action
    {
        override_trust_recommended_action(&mut trust, recommended_action.clone());
    }
    let output = ReadyOutput {
        operator,
        captured,
        captured_state,
        thread_state: thread.state.to_string(),
        trust,
        report,
    };

    write_ready_output(cli, &repo, &output)?;

    Ok(())
}

fn ready_scoped_next_action(
    operation: Option<&RepositoryOperationStatus>,
    remote_tracking: Option<&GitRemoteTrackingStatus>,
    import_hint: Option<&GitOverlayImportHint>,
    thread_action: Option<&str>,
) -> String {
    effective_next_action(
        NextActionInput::default(operation, remote_tracking, import_hint, thread_action).ready(),
    )
}

fn ready_verification_preflight_blocks(trust: &RepositoryVerificationState) -> bool {
    matches!(
        trust.status.as_str(),
        "needs_init" | "needs_import" | "needs_reconcile" | "git_branch_advanced"
    )
}

fn write_ready_output(cli: &Cli, repo: &Repository, output: &ReadyOutput) -> Result<()> {
    write_ready_output_inner(
        output,
        should_output_json(cli, Some(repo.config())),
        output_is_compact(cli),
        NextActionValidationContext::new(&["ready"], repo.capability()),
    )
}

fn write_ready_output_without_repo(cli: &Cli, output: &ReadyOutput) -> Result<()> {
    write_ready_output_inner(
        output,
        should_output_json(cli, None),
        output_is_compact(cli),
        NextActionValidationContext::without_repo(&["ready"]),
    )
}

impl super::compact::CompactProjection for ReadyOutput {
    fn compact(&self) -> super::compact::CompactOutput {
        let mut compact = self.operator.compact();
        compact.changed_paths = Some(self.report.changed_paths.clone());
        compact.changed_path_count = Some(self.report.changed_path_count);
        compact.conflicts = Some(self.report.conflicts.clone());
        compact.conflict_count = Some(self.report.conflict_count);
        compact
    }
}

fn write_ready_output_inner(
    output: &ReadyOutput,
    json: bool,
    compact: bool,
    context: NextActionValidationContext<'_>,
) -> Result<()> {
    if json {
        write_command_json(output, compact, context)?;
    } else {
        let missing_intent = ready_blocked_by_missing_intent(output);
        if !missing_intent {
            let marker = if output.operator.status == "completed" {
                style::ok_marker()
            } else {
                style::warn_marker()
            };
            println!("{marker} {}", output.operator.message);
            if output.captured {
                match output.captured_state.as_deref() {
                    Some(state) => println!(
                        "  {}",
                        style::field("captured", &format!("state {}", style::change_id(state)))
                    ),
                    None => println!("  {}", style::field("captured", "yes")),
                }
            }
        }
        if !output.trust.verified && !missing_intent {
            write_trust_blocked_setup(output.operator.recommended_action.as_deref());
        } else {
            write_preview_report(
                &output.report,
                output.operator.recommended_action.as_deref(),
            );
        }
    }
    exit_if_blocked_operator_status(&output.operator.status);
    Ok(())
}

fn ready_blocked_by_missing_intent(output: &ReadyOutput) -> bool {
    output.report.merge_relation == "not_checked"
        && output
            .report
            .blockers
            .iter()
            .any(|blocker| blocker.contains("-m/--message/--intent"))
        && output
            .operator
            .recommended_action
            .as_deref()
            .is_some_and(|action| action == "heddle commit -m \"...\"")
}

fn write_trust_blocked_setup(recommended_action: Option<&str>) {
    println!();
    println!("{}", style::section("Setup needed"));
    println!(
        "  {}",
        style::field("status", &style::thread_state("blocked"))
    );
    println!("  {}", style::field("checks", "not run"));
    if let Some(recommended_action) = non_empty_action(recommended_action) {
        println!();
        print_next(recommended_action);
    }
}

fn trust_blocked_ready_output(
    requested_thread: Option<&str>,
    trust: RepositoryVerificationState,
) -> ReadyOutput {
    let thread = requested_thread
        .map(ToString::to_string)
        .or_else(|| trust.heddle_thread.clone())
        .or_else(|| trust.git_branch.clone())
        .unwrap_or_else(|| "unknown".to_string());
    let message = format!(
        "Thread '{thread}' cannot run readiness checks until repository verification is restored: {}",
        trust.summary
    );
    let operator = OperatorCommandOutput::blocked_by_repository_verification(
        OperatorAction::Ready,
        message,
        &trust,
    );
    let recommended_action = operator
        .recommended_action
        .clone()
        .unwrap_or_else(|| "heddle verify".to_string());
    ReadyOutput {
        operator,
        captured: false,
        captured_state: None,
        thread_state: "blocked".to_string(),
        trust,
        report: trust_blocked_report_for(&thread, "blocked", None, &recommended_action),
    }
}

pub(crate) fn worktree_dirty(
    repo: &Repository,
    options: &repo::WorktreeStatusOptions,
) -> Result<bool> {
    if repo.current_state()?.is_none()
        && let Some(status) = repo.git_overlay_worktree_status()?
    {
        return Ok(!status.is_clean());
    }
    let tree = match repo.current_state()? {
        Some(state) => repo.require_tree(&state.tree)?,
        None => Tree::new(),
    };
    let status = repo.compare_worktree_cached_with_options(&tree, options)?;
    Ok(!status.is_clean())
}

pub(crate) fn worktree_dirty_paths(
    repo: &Repository,
    options: &repo::WorktreeStatusOptions,
) -> Result<Vec<String>> {
    let status = if repo.current_state()?.is_none()
        && let Some(status) = repo.git_overlay_worktree_status()?
    {
        status
    } else {
        let tree = match repo.current_state()? {
            Some(state) => repo.require_tree(&state.tree)?,
            None => Tree::new(),
        };
        repo.compare_worktree_cached_with_options(&tree, options)?
    };

    let mut paths = Vec::new();
    paths.extend(status.modified);
    paths.extend(status.added);
    paths.extend(status.deleted);
    paths.sort();
    paths.dedup();
    Ok(paths
        .into_iter()
        .map(|path| path.display().to_string())
        .collect())
}

fn missing_ready_capture_intent_output(
    repo: &Repository,
    requested_thread: Option<&str>,
    dirty_paths: Vec<String>,
    trust: RepositoryVerificationState,
) -> Result<ReadyOutput> {
    let thread = requested_thread
        .map(ToString::to_string)
        .or_else(|| repo.current_lane().ok().flatten())
        .or_else(|| trust.heddle_thread.clone())
        .or_else(|| trust.git_branch.clone())
        .unwrap_or_else(|| "current".to_string());
    let path_summary = if dirty_paths.is_empty() {
        "uncaptured worktree paths".to_string()
    } else {
        let shown = dirty_paths
            .iter()
            .take(12)
            .cloned()
            .collect::<Vec<_>>()
            .join(", ");
        let overflow = dirty_paths.len().saturating_sub(12);
        if overflow == 0 {
            format!("uncaptured path(s): {shown}")
        } else {
            format!("uncaptured path(s): {shown}, and {overflow} more")
        }
    };
    let recommended_action = "heddle commit -m \"...\"".to_string();
    Ok(ReadyOutput {
        operator: OperatorCommandOutput {
            status: "blocked".to_string(),
            action: OperatorAction::Ready,
            message: format!(
                "Thread '{thread}' has uncaptured work; provide an intent before readiness checks"
            ),
            blockers: vec![format!(
                "{path_summary}; commit the work with -m/--message/--intent before readiness checks"
            )],
            warnings: Vec::new(),
            next_action: Some(recommended_action.clone()),
            recommended_action: Some(recommended_action.clone()),
        },
        captured: false,
        captured_state: None,
        thread_state: "blocked".to_string(),
        trust,
        report: missing_ready_capture_intent_report_for(&thread, dirty_paths, &recommended_action),
    })
}

fn missing_ready_capture_intent_report_for(
    thread: &str,
    dirty_paths: Vec<String>,
    recommended_action: &str,
) -> ThreadPreviewReport {
    let changed_path_count = dirty_paths.len();
    ThreadPreviewReport {
        thread: thread.to_string(),
        thread_mode: "blocked".to_string(),
        thread_state: "blocked".to_string(),
        freshness: "not_checked".to_string(),
        task: None,
        changed_paths: dirty_paths.into_iter().take(8).collect(),
        changed_path_count,
        impact_categories: Vec::new(),
        heavy_impact_paths: Vec::new(),
        merge_relation: "not_checked".to_string(),
        conflicts: Vec::new(),
        conflict_count: 0,
        blockers: vec![
            "commit the work with -m/--message/--intent before readiness checks".to_string(),
        ],
        recommended_action: recommended_action.to_string(),
        recommended_action_template: super::git_overlay_health::action_template(recommended_action),
        thread_health: "blocked".to_string(),
    }
}

fn write_preview_report(report: &ThreadPreviewReport, recommended_action: Option<&str>) {
    let no_target = report.merge_relation == "no_target";
    println!();
    println!("{}", style::section("Readiness"));
    println!("  {}", style::field("thread", &style::bold(&report.thread)));
    if no_target {
        println!(
            "  {}",
            style::field("status", &style::thread_state("clean"))
        );
        println!("  {}", style::field("integration", "none configured"));
    } else {
        println!(
            "  {}",
            style::field("state", &style::thread_state(&report.thread_state))
        );
        println!(
            "  {}",
            style::field(
                "freshness",
                &style::thread_state(&report.freshness.replace('_', " "))
            )
        );
        println!(
            "  {}",
            style::field(
                "merge type",
                &style::thread_state(&ready_merge_type_label(&report.merge_relation))
            )
        );
    }
    println!(
        "  {}",
        style::field(
            "changed paths",
            &style::bold(&report.changed_path_count.to_string())
        )
    );
    if !report.impact_categories.is_empty() {
        println!(
            "  {}",
            style::field("impact", &report.impact_categories.join(", "))
        );
    }
    if !report.blockers.is_empty() {
        println!();
        println!("{}", style::warn("Blocked by"));
        for blocker in &report.blockers {
            println!("  {} {}", style::warn("-"), style::warn(blocker));
        }
    }
    if let Some(recommended_action) = non_empty_action(recommended_action) {
        println!();
        print_next(recommended_action);
    }
}

fn ready_merge_type_label(result: &str) -> String {
    match result {
        "fast_forward" => "fast-forward".to_string(),
        "already_integrated" => "already integrated".to_string(),
        "no_target" => "none configured".to_string(),
        other => other.replace('_', " "),
    }
}

fn ready_report_recommended_action(report: &ThreadPreviewReport) -> Option<String> {
    if report.merge_relation == "no_target" {
        return None;
    }
    normalized_action(report.recommended_action.clone())
}

fn trust_blocked_report_for(
    thread: &str,
    thread_mode: &str,
    task: Option<String>,
    recommended_action: &str,
) -> ThreadPreviewReport {
    ThreadPreviewReport {
        thread: thread.to_string(),
        thread_mode: thread_mode.to_string(),
        thread_state: "blocked".to_string(),
        freshness: "not_checked".to_string(),
        task,
        changed_paths: Vec::new(),
        changed_path_count: 0,
        impact_categories: Vec::new(),
        heavy_impact_paths: Vec::new(),
        merge_relation: "blocked".to_string(),
        conflicts: Vec::new(),
        conflict_count: 0,
        blockers: vec!["repository verification is blocked".to_string()],
        recommended_action: recommended_action.to_string(),
        recommended_action_template: super::git_overlay_health::action_template(recommended_action),
        thread_health: "blocked".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::commands::git_overlay_health;

    fn report(merge_relation: &str, recommended_action: &str) -> ThreadPreviewReport {
        ThreadPreviewReport {
            thread: "main".to_string(),
            thread_mode: "solid".to_string(),
            thread_state: "ready".to_string(),
            freshness: "unknown".to_string(),
            task: None,
            changed_paths: Vec::new(),
            changed_path_count: 0,
            impact_categories: Vec::new(),
            heavy_impact_paths: Vec::new(),
            merge_relation: merge_relation.to_string(),
            conflicts: Vec::new(),
            conflict_count: 0,
            blockers: Vec::new(),
            recommended_action: recommended_action.to_string(),
            recommended_action_template: git_overlay_health::action_template(recommended_action),
            thread_health: "ready".to_string(),
        }
    }

    #[test]
    fn ready_suppresses_self_merge_when_thread_has_no_target() {
        assert_eq!(
            ready_report_recommended_action(&report("no_target", "heddle merge main --preview")),
            None
        );
    }

    #[test]
    fn ready_keeps_land_action_for_targeted_threads() {
        assert_eq!(
            ready_report_recommended_action(&report(
                "fast_forward",
                "heddle land --thread feature --no-push"
            )),
            Some("heddle land --thread feature --no-push".to_string())
        );
    }
}
