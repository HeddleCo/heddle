// SPDX-License-Identifier: Apache-2.0
//! Ready command implementation.

use anyhow::Result;
use chrono::Utc;
use heddle_core::status::next_action::{
    NextActionInput, effective_next_action, non_empty_action,
};
use objects::object::Tree;
use repo::{
    GitOverlayImportHint, GitRemoteTrackingStatus, Repository, RepositoryOperationStatus,
    ThreadFreshness, ThreadState,
};
use serde::{
    Serialize, Serializer,
    ser::{Error as SerError, SerializeStruct},
};

use super::{
    action_line::print_next,
    advice::RecoveryAdvice,
    git_overlay_health::{
        RepositoryVerificationState, build_plain_git_verification_probe,
        build_repository_verification_state,
        build_repository_verification_state_with_worktree_status,
        override_trust_recommended_action,
    },
    merge::{ThreadPreviewReport, build_thread_preview_report},
    next_action::{NextActionValidationContext, normalized_action, write_command_json},
    operator_core::{
        OperatorAction, OperatorCommandOutput, VerificationClaimPolicy,
        exit_if_blocked_operator_status,
    },
    snapshot::{SnapshotAgentOverrides, create_snapshot, ensure_current_state},
    thread::contextual_thread_action,
    thread_cmd::{
        current_thread, load_thread, refresh_thread, thread_manager, thread_not_found_advice,
    },
    thread_landing::land_local_command,
};
use crate::{
    cli::{Cli, ReadyArgs, output_is_compact, should_output_json, style, worktree_status_options},
    config::UserConfig,
};

struct ReadyOutput {
    operator: OperatorCommandOutput,
    captured: bool,
    captured_state: Option<String>,
    thread_state: String,
    trust: RepositoryVerificationState,
    report: ThreadPreviewReport,
}

#[derive(Debug, Clone, Serialize)]
struct ReadyReadinessSummary {
    status: String,
    captured: bool,
    captured_state: Option<String>,
    checks: ReadyChecksSummary,
    integration: String,
    freshness: String,
    merge_type: String,
    changed_path_count: usize,
    changed_paths: Vec<String>,
    conflict_count: usize,
    conflicts: Vec<String>,
    impact: String,
    impact_categories: Vec<String>,
    blockers: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
struct ReadyChecksSummary {
    status: String,
    reason: String,
}

impl ReadyOutput {
    fn readiness_summary(&self) -> ReadyReadinessSummary {
        let checks = ready_checks_summary(self);
        let integration = ready_integration_summary(&self.report);
        let freshness = ready_freshness_summary(&self.report);
        let merge_type = ready_merge_type_summary(&self.report);
        let impact = if self.report.impact_categories.is_empty() {
            "none".to_string()
        } else {
            self.report.impact_categories.join(", ")
        };
        ReadyReadinessSummary {
            status: ready_status_summary(&self.report),
            captured: self.captured,
            captured_state: self.captured_state.clone(),
            checks,
            integration,
            freshness,
            merge_type,
            changed_path_count: self.report.changed_path_count,
            changed_paths: self.report.changed_paths.clone(),
            conflict_count: self.report.conflict_count,
            conflicts: self.report.conflicts.clone(),
            impact,
            impact_categories: self.report.impact_categories.clone(),
            blockers: self.report.blockers.clone(),
        }
    }
}

impl Serialize for ReadyOutput {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let next_action = normalized_action(self.operator.next_action.clone().unwrap_or_default());
        let recommended_action =
            normalized_action(self.operator.recommended_action.clone().unwrap_or_default());
        let next_action_template = next_action
            .as_deref()
            .and_then(super::git_overlay_health::action_template);
        let recommended_action_template = recommended_action
            .as_deref()
            .and_then(super::git_overlay_health::action_template);
        let verification = serde_json::to_value(&self.trust).map_err(S::Error::custom)?;
        let readiness = self.readiness_summary();

        let mut state = serializer.serialize_struct("ReadyOutput", 18)?;
        state.serialize_field("output_kind", "ready")?;
        state.serialize_field("status", &self.operator.status)?;
        state.serialize_field("action", &self.operator.action)?;
        state.serialize_field("message", &self.operator.message)?;
        state.serialize_field("blockers", &self.operator.blockers)?;
        state.serialize_field("warnings", &self.operator.warnings)?;
        state.serialize_field("next_action", &next_action)?;
        state.serialize_field("next_action_template", &next_action_template)?;
        state.serialize_field("recommended_action", &recommended_action)?;
        state.serialize_field("recommended_action_template", &recommended_action_template)?;
        state.serialize_field("captured", &self.captured)?;
        state.serialize_field("captured_state", &self.captured_state)?;
        state.serialize_field("thread_state", &self.thread_state)?;
        state.serialize_field("readiness", &readiness)?;
        state.serialize_field("report", &self.report)?;
        state.serialize_field("verification", &verification)?;
        state.end()
    }
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
    // Compute the git-overlay worktree status ONCE up front. It feeds the initial
    // verification preflight here and the second preflight further down, which
    // `ready` previously recomputed from scratch — a full worktree walk that
    // re-reads + SHA-1s every tracked file. The second preflight can reuse this
    // status ONLY when no bootstrap capture intervened (see below): a bootstrap
    // capture advances the Heddle state and flips the git-overlay health
    // classification, so after one the second preflight must take a FRESH walk.
    let worktree_status = repo.git_overlay_worktree_status();
    let initial_trust =
        build_repository_verification_state_with_worktree_status(&repo, &worktree_status);
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

    // Reuse the status computed at the top only when no bootstrap capture ran
    // (`had_current_state`): between the initial preflight and here, the only
    // mutation is `ensure_current_state`'s bootstrap capture, which fires iff
    // `!had_current_state`. After a bootstrap capture the git-overlay health
    // classification flips, so that case must take a FRESH walk.
    let preflight_trust = if had_current_state {
        build_repository_verification_state_with_worktree_status(&repo, &worktree_status)
    } else {
        build_repository_verification_state(&repo)
    };
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
    if report.freshness == ThreadFreshness::Stale.to_string()
        && report.conflict_count == 0
        && super::workflow::non_staleness_blockers(&report.blockers).is_empty()
    {
        thread = refresh_thread(&repo, &thread.id, cli)?;
        report = build_thread_preview_report(&repo, &mut thread, true)?;
    }
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
        }
        if !output.trust.verified && !missing_intent {
            write_trust_blocked_setup(output.operator.recommended_action.as_deref());
        } else {
            write_preview_report(output, output.operator.recommended_action.as_deref());
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
    println!(
        "  {}",
        style::field("checks", "not run (repository verification is blocked)")
    );
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

fn write_preview_report(output: &ReadyOutput, recommended_action: Option<&str>) {
    let report = &output.report;
    let summary = output.readiness_summary();
    println!();
    println!("{}", style::section("Readiness"));
    println!("  {}", style::field("thread", &style::bold(&report.thread)));
    println!(
        "  {}",
        style::field("status", &style::thread_state(&summary.status))
    );
    println!(
        "  {}",
        style::field("captured", &ready_captured_label(&summary))
    );
    println!(
        "  {}",
        style::field("checks", &ready_checks_label(&summary.checks))
    );
    println!("  {}", style::field("integration", &summary.integration));
    println!("  {}", style::field("freshness", &summary.freshness));
    println!("  {}", style::field("merge type", &summary.merge_type));
    println!(
        "  {}",
        style::field(
            "changed paths",
            &style::bold(&summary.changed_path_count.to_string())
        )
    );
    println!(
        "  {}",
        style::field(
            "conflicts",
            &style::bold(&summary.conflict_count.to_string())
        )
    );
    println!("  {}", style::field("impact", &summary.impact));
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

fn ready_status_summary(report: &ThreadPreviewReport) -> String {
    if report.merge_relation == "no_target" && report.blockers.is_empty() {
        "clean".to_string()
    } else {
        report.thread_health.replace('_', " ")
    }
}

fn ready_checks_summary(output: &ReadyOutput) -> ReadyChecksSummary {
    if ready_blocked_by_missing_intent(output) {
        ReadyChecksSummary {
            status: "not_run".to_string(),
            reason: "commit intent is required before readiness checks can run".to_string(),
        }
    } else if !output.trust.verified {
        ReadyChecksSummary {
            status: "not_run".to_string(),
            reason: "repository verification is blocked".to_string(),
        }
    } else if output.report.merge_relation == "not_checked" {
        ReadyChecksSummary {
            status: "not_run".to_string(),
            reason: "readiness preview was not reached".to_string(),
        }
    } else {
        ReadyChecksSummary {
            status: "completed".to_string(),
            reason: "readiness preview ran".to_string(),
        }
    }
}

fn ready_integration_summary(report: &ThreadPreviewReport) -> String {
    if report.merge_relation == "no_target" {
        "n/a (no integration target configured)".to_string()
    } else if report.merge_relation == "not_checked" {
        "not checked (readiness checks did not run)".to_string()
    } else if report.merge_relation == "blocked" {
        "not checked (repository verification is blocked)".to_string()
    } else {
        "configured".to_string()
    }
}

fn ready_freshness_summary(report: &ThreadPreviewReport) -> String {
    match report.merge_relation.as_str() {
        "no_target" => "n/a (no integration target configured)".to_string(),
        "not_checked" => "not checked (readiness checks did not run)".to_string(),
        "blocked" => "not checked (repository verification is blocked)".to_string(),
        _ => report.freshness.replace('_', " "),
    }
}

fn ready_merge_type_summary(report: &ThreadPreviewReport) -> String {
    match report.merge_relation.as_str() {
        "no_target" => "n/a (no integration target configured)".to_string(),
        "not_checked" => "not checked (readiness checks did not run)".to_string(),
        "blocked" => "not checked (repository verification is blocked)".to_string(),
        other => ready_merge_type_label(other),
    }
}

fn ready_captured_label(summary: &ReadyReadinessSummary) -> String {
    match summary.captured_state.as_deref() {
        Some(state) => format!("yes (state {})", style::change_id(state)),
        None if summary.captured => "yes".to_string(),
        None => "no".to_string(),
    }
}

fn ready_checks_label(checks: &ReadyChecksSummary) -> String {
    format!("{} ({})", checks.status.replace('_', " "), checks.reason)
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
