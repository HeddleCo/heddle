// SPDX-License-Identifier: Apache-2.0
//! Ready command implementation.

use anyhow::Result;
use chrono::Utc;
use objects::object::Tree;
use repo::{Repository, ThreadState};
use serde::Serialize;

use super::{
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

    let report = build_thread_preview_report(&repo, &mut thread, true)?;
    let already_ready = !captured
        && thread.state == ThreadState::Ready
        && report.conflict_count == 0
        && report.blockers.is_empty();

    if !already_ready {
        thread.state = if report.conflict_count == 0 && report.blockers.is_empty() {
            ThreadState::Ready
        } else {
            ThreadState::Blocked
        };
        thread.updated_at = Utc::now();
        manager.save(&thread)?;
    }

    let message = if already_ready {
        format!("Thread '{}' is already ready", thread.id)
    } else if thread.state == ThreadState::Ready {
        format!("Thread '{}' is ready to integrate", thread.id)
    } else {
        format!("Thread '{}' is blocked", thread.id)
    };
    let operation = repo.operation_status()?;
    let remote_tracking = repo.git_remote_tracking_status()?;
    let import_hint = repo.git_overlay_import_hint()?;
    let recommended_action = primary_next_action(
        operation.as_ref(),
        remote_tracking.as_ref(),
        import_hint.as_ref(),
        Some(&report.recommended_action),
    );

    let output = ReadyOutput {
        operator: OperatorCommandOutput {
            status: if thread.state == ThreadState::Ready {
                "completed".to_string()
            } else {
                "blocked".to_string()
            },
            action: "ready".to_string(),
            message: message.clone(),
            blockers: report.blockers.clone(),
            warnings: Vec::new(),
            next_action: Some(recommended_action.clone()),
            recommended_action: Some(recommended_action.clone()),
        },
        captured,
        thread_state: thread.state.to_string(),
        report,
    };

    if should_output_json(cli, Some(repo.config())) {
        println!("{}", serde_json::to_string(&output)?);
    } else {
        let marker = if output.thread_state == ThreadState::Ready.to_string() {
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
        Some(state) => repo.store().get_tree(&state.tree)?.unwrap_or_default(),
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