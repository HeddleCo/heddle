// SPDX-License-Identifier: Apache-2.0
//! CLI adapter for the core diff facade.

use anyhow::{Result, anyhow};
use heddle_core::{
    DiffOptions, DiffReport, PlainGitDiffProbe, diff as core_diff, diff_worktree_status,
    plain_git_head_diff,
};
use objects::worktree::WorktreeStatus;
use repo::{Config, Repository};

use super::{
    super::verification_health::{
        build_plain_git_verification_probe, build_repository_verification_state,
        plain_git_setup_advice, trust_visible_worktree_status,
    },
    diff_output::{
        print_context, print_diff, print_diff_patch, print_semantic_changes, print_stat,
    },
};
use crate::{
    cli::{Cli, should_output_json},
    config::UserConfig,
};

#[allow(clippy::too_many_arguments)]
pub fn cmd_diff(
    cli: &Cli,
    from: Option<String>,
    to: Option<String>,
    semantic: bool,
    stat: bool,
    name_only: bool,
    unified: usize,
    show_context: bool,
    patch: bool,
) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let start = cli.repo.as_ref().unwrap_or(&cwd);
    let from_is_head_or_default = from
        .as_deref()
        .map(|spec| matches!(spec, "HEAD" | "@"))
        .unwrap_or(true);

    if to.is_none()
        && from_is_head_or_default
        && let Some(probe) = build_plain_git_verification_probe(start)?
    {
        if probe.changes.is_clean() {
            return Err(anyhow!(plain_git_setup_advice(&probe, "diff", None)));
        }
        let options = diff_options(
            from,
            to,
            semantic,
            stat,
            name_only,
            unified,
            show_context,
            patch,
            should_output_json(cli, None),
        );
        let report = plain_git_head_diff(
            &PlainGitDiffProbe {
                root: probe.root.clone(),
                changes: clone_worktree_status(&probe.changes),
            },
            &options,
        )?;
        return render_diff_report(cli, None, &report, stat, name_only, show_context, patch);
    }

    let repo = Repository::open(start)?;
    let trust = build_repository_verification_state(&repo);
    let json = should_output_json(cli, Some(repo.config()));
    let options = diff_options(
        from.clone(),
        to.clone(),
        semantic,
        stat,
        name_only,
        unified,
        show_context,
        patch,
        json,
    );

    if to.is_none()
        && from_is_head_or_default
        && let Some(status) = trust_visible_worktree_status(&repo, &trust)?
    {
        let report = diff_worktree_status(&status, &options, Some(&repo), true)?;
        return render_diff_report(
            cli,
            Some(repo.config()),
            &report,
            stat,
            name_only,
            show_context,
            patch,
        );
    }
    if to.is_none() && from_is_head_or_default && trust.mapping_state == "git_backed" {
        let status = repo.git_overlay_worktree_status()?.unwrap_or_default();
        let report = diff_worktree_status(&status, &options, Some(&repo), true)?;
        return render_diff_report(
            cli,
            Some(repo.config()),
            &report,
            stat,
            name_only,
            show_context,
            patch,
        );
    }

    let git_overlay_head_worktree_diff = repo.current_state()?.is_none()
        && to.is_none()
        && matches!(from.as_deref(), Some("HEAD" | "@"));
    if !git_overlay_head_worktree_diff
        && repo.current_state()?.is_none()
        && (matches!(from.as_deref(), Some("HEAD" | "@"))
            || matches!(to.as_deref(), Some("HEAD" | "@")))
    {
        crate::cli::commands::snapshot::ensure_current_state(
            &repo,
            &UserConfig::load_default().unwrap_or_default(),
            Some("Bootstrap git-overlay before diffing HEAD".to_string()),
        )?;
    }

    let config = UserConfig::load_default().unwrap_or_default();
    let ctx = heddle_core::ExecutionContext::builder()
        .repo(repo)
        .start_path(start.to_path_buf())
        .config(config)
        .build();
    let report = core_diff(&ctx, options)?;
    render_diff_report(
        cli,
        ctx.repo().map(|repo| repo.config()),
        &report,
        stat,
        name_only,
        show_context,
        patch,
    )
}

#[allow(clippy::too_many_arguments)]
fn diff_options(
    from: Option<String>,
    to: Option<String>,
    semantic: bool,
    stat: bool,
    name_only: bool,
    unified: usize,
    show_context: bool,
    patch: bool,
    json: bool,
) -> DiffOptions {
    DiffOptions {
        from,
        to,
        semantic,
        stat,
        name_only,
        unified,
        show_context,
        include_patch_text: patch || json,
    }
}

fn render_diff_report(
    cli: &Cli,
    config: Option<&Config>,
    report: &DiffReport,
    stat: bool,
    name_only: bool,
    show_context: bool,
    patch: bool,
) -> Result<()> {
    if should_output_json(cli, config) {
        println!("{}", serde_json::to_string(report)?);
    } else if name_only {
        for change in &report.changes {
            println!("{}", change.path);
        }
    } else if stat {
        print_stat(report);
    } else if patch {
        print_diff_patch(report);
    } else {
        if show_context {
            print_context(report);
        }
        print_diff(report);
        if let Some(ref semantic) = report.semantic_changes {
            print_semantic_changes(semantic);
        }
    }
    Ok(())
}

fn clone_worktree_status(status: &WorktreeStatus) -> WorktreeStatus {
    WorktreeStatus {
        modified: status.modified.clone(),
        added: status.added.clone(),
        deleted: status.deleted.clone(),
    }
}
