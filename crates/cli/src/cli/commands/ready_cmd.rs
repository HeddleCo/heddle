// SPDX-License-Identifier: Apache-2.0
//! Ready command implementation.

use anyhow::Result;
use chrono::Utc;
use objects::object::Tree;
use repo::{Repository, ThreadState};
use serde::Serialize;

use super::{
    git_overlay_health::{RepositoryTrustState, build_repository_trust_state},
    merge::{ThreadPreviewReport, build_thread_preview_report},
    operator_core::OperatorCommandOutput,
    operator_loop::primary_next_action,
    snapshot::{SnapshotAgentOverrides, create_snapshot, ensure_current_state},
    thread_cmd::{current_thread, load_thread, thread_manager},
};
use crate::{
    cli::{Cli, ReadyArgs, should_output_json, style, worktree_status_options},
    config::UserConfig,
};

#[derive(Serialize)]
struct ReadyOutput {
    #[serde(flatten)]
    operator: OperatorCommandOutput,
    captured: bool,
    thread_state: String,
    trust: RepositoryTrustState,
    report: ThreadPreviewReport,
}

pub async fn cmd_ready(cli: &Cli, args: ReadyArgs) -> Result<()> {
    let repo = Repository::open(cli.repo.as_ref().unwrap_or(&std::env::current_dir()?))?;
    let user_config = UserConfig::load_default().unwrap_or_default();
    let had_current_state = repo.current_state()?.is_some();
    let bootstrap_dirty = if !had_current_state {
        let status_options = worktree_status_options(Some(repo.config()));
        worktree_dirty(&repo, &status_options)?
    } else {
        false
    };
    if !had_current_state {
        ensure_current_state(
            &repo,
            &user_config,
            args.message
                .clone()
                .or_else(|| Some("Bootstrap git-overlay readiness state".to_string())),
        )?;
    }
    let manager = thread_manager(&repo);
    let mut thread = match args.thread {
        Some(thread_id) => load_thread(&repo, &thread_id)?,
        None => current_thread(&repo)?
            .ok_or_else(|| anyhow::anyhow!("No current thread; pass --thread"))?,
    };

    let mut captured = !had_current_state && bootstrap_dirty;
    let status_options = worktree_status_options(Some(repo.config()));
    let dirty = worktree_dirty(&repo, &status_options)?;
    if dirty {
        create_snapshot(
            &repo,
            &user_config,
            args.message.clone(),
            None,
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
        thread = manager
            .load(&thread.id)?
            .or_else(|| current_thread(&repo).ok().flatten())
            .ok_or_else(|| anyhow::anyhow!("Thread '{}' not found after capture", thread.id))?;
        captured = true;
    }

    let mut report = build_thread_preview_report(&repo, &mut thread, true)?;
    let has_integration_target = report.semantic_result != "no_target";
    if !has_integration_target && report.conflict_count == 0 && report.blockers.is_empty() {
        report.recommended_action.clear();
        report.thread_health = "clean".to_string();
    }
    let already_ready = has_integration_target
        && !captured
        && thread.state == ThreadState::Ready
        && report.conflict_count == 0
        && report.blockers.is_empty();

    let ready_without_target =
        !has_integration_target && report.conflict_count == 0 && report.blockers.is_empty();

    if !already_ready && (has_integration_target || ready_without_target) {
        thread.state = if report.conflict_count == 0 && report.blockers.is_empty() {
            ThreadState::Ready
        } else {
            ThreadState::Blocked
        };
        thread.updated_at = Utc::now();
        manager.save(&thread)?;
        report.thread_state = thread.state.to_string();
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
    let trust = build_repository_trust_state(&repo);
    let trust_blockers = trust
        .checks
        .iter()
        .filter(|check| !check.clean)
        .map(|check| format!("{}: {}", check.name, check.summary))
        .collect::<Vec<_>>();
    let report_recommended_action = ready_report_recommended_action(&report);
    let recommended_action = if trust.trusted {
        primary_next_action(
            operation.as_ref(),
            remote_tracking.as_ref(),
            import_hint.as_ref(),
            report_recommended_action.as_deref(),
        )
    } else {
        trust.recommended_action.clone()
    };

    let status = if !trust.trusted {
        "blocked"
    } else if thread.state == ThreadState::Ready || !has_integration_target {
        "completed"
    } else {
        "blocked"
    };
    let message = if !trust.trusted {
        format!(
            "Thread '{}' reached readiness checks, but repository trust is blocked: {}",
            thread.id, trust.summary
        )
    } else {
        message.clone()
    };

    let output = ReadyOutput {
        operator: OperatorCommandOutput {
            status: status.to_string(),
            action: "ready".to_string(),
            message: message.clone(),
            blockers: if trust.trusted {
                report.blockers.clone()
            } else {
                trust_blockers
            },
            warnings: Vec::new(),
            next_action: Some(recommended_action.clone()),
            recommended_action: Some(recommended_action.clone()),
        },
        captured,
        thread_state: thread.state.to_string(),
        trust,
        report,
    };

    if should_output_json(cli, Some(repo.config())) {
        println!("{}", serde_json::to_string(&output)?);
    } else {
        let marker = if output.operator.status == "completed" {
            style::ok_marker()
        } else {
            style::warn_marker()
        };
        println!("{marker} {message}");
        print_preview_report(&output.report, &recommended_action);
    }

    Ok(())
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

fn print_preview_report(report: &ThreadPreviewReport, recommended_action: &str) {
    println!();
    println!("{}", style::section("Readiness"));
    println!("  {}", style::field("thread", &style::bold(&report.thread)));
    println!(
        "  {}",
        style::field("state", &style::thread_state(&report.thread_state))
    );
    println!(
        "  {}",
        style::field("freshness", &style::thread_state(&report.freshness))
    );
    println!(
        "  {}",
        style::field("semantic", &style::thread_state(&report.semantic_result))
    );
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
    if !recommended_action.is_empty() {
        println!();
        println!("{}", style::field("next", &style::bold(recommended_action)));
    }
}

fn ready_report_recommended_action(report: &ThreadPreviewReport) -> Option<String> {
    if report.semantic_result == "no_target" {
        return None;
    }
    if report.recommended_action.trim().is_empty() {
        None
    } else {
        Some(report.recommended_action.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(semantic_result: &str, recommended_action: &str) -> ThreadPreviewReport {
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
            semantic_result: semantic_result.to_string(),
            conflicts: Vec::new(),
            conflict_count: 0,
            blockers: Vec::new(),
            recommended_action: recommended_action.to_string(),
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
    fn ready_keeps_merge_action_for_targeted_threads() {
        assert_eq!(
            ready_report_recommended_action(&report(
                "fast_forward",
                "heddle merge feature --preview"
            )),
            Some("heddle merge feature --preview".to_string())
        );
    }
}
