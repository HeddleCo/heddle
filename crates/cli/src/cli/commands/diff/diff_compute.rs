// SPDX-License-Identifier: Apache-2.0
//! CLI adapter for the core diff facade.

use std::path::Path;

use anyhow::{Result, anyhow};
use objects::worktree::WorktreeStatus;
use repo::{Config, Repository, RepositoryCapability, discover_heddle_root};
use verbs::{
    DiffOptions, DiffReport, PlainGitDiffProbe, attach_show_context, diff as core_diff,
    diff_worktree_status, plain_git_head_diff, worktree_context_state,
};

use super::{
    super::{
        next_action::{NextActionValidationContext, write_command_json},
        verification_health::{
            build_plain_git_verification_probe, build_repository_verification_state,
            plain_git_setup_advice, trust_visible_worktree_status,
        },
    },
    diff_output::{
        print_context, print_diff, print_diff_patch, print_semantic_changes, print_stat,
    },
    diff_paths::classify_diff_refs,
};
use crate::{
    cli::{Cli, execution_context_from_cli_parts, output_is_compact, should_output_json},
    config::UserConfig,
};

#[allow(clippy::too_many_arguments)]
pub fn cmd_diff(
    cli: &Cli,
    from: Option<String>,
    to: Option<String>,
    path_filters: Vec<String>,
    trailing_paths: Vec<String>,
    semantic: bool,
    stat: bool,
    name_only: bool,
    unified: usize,
    show_context: bool,
    patch: bool,
) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let start = cli.repo.as_ref().unwrap_or(&cwd);
    let opened = open_repo_for_classification(start)?;
    let mut extra_paths = path_filters;
    extra_paths.extend(trailing_paths);
    let classified = classify_diff_refs(start, opened.as_ref(), from, to, extra_paths);
    let from = classified.from;
    let to = classified.to;
    let paths = classified.paths;
    let from_is_head_or_default = from
        .as_deref()
        .map(|spec| matches!(spec, "HEAD" | "@"))
        .unwrap_or(true);

    if opened.is_none()
        && to.is_none()
        && from_is_head_or_default
        && let Some(probe) = build_plain_git_verification_probe(start)?
    {
        if probe.changes.is_clean() {
            return Err(anyhow!(plain_git_setup_advice(&probe, "diff", None)));
        }
        let options = diff_options(
            from,
            to,
            paths,
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

    let repo = match opened {
        Some(repo) => repo,
        None => Repository::open(start)?,
    };
    let trust = (repo.capability() == RepositoryCapability::GitOverlay)
        .then(|| build_repository_verification_state(&repo));
    let json = should_output_json(cli, Some(repo.config()));
    let options = diff_options(
        from.clone(),
        to.clone(),
        paths,
        semantic,
        stat,
        name_only,
        unified,
        show_context,
        patch,
        json,
    );

    if let Some(trust) = trust.as_ref()
        && to.is_none()
        && from_is_head_or_default
        && let Some(status) = trust_visible_worktree_status(&repo, trust)?
    {
        let report = authority_worktree_diff(&repo, &status, &options)?;
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
    if to.is_none()
        && from_is_head_or_default
        && trust
            .as_ref()
            .is_some_and(|trust| trust.mapping_state == "git_backed")
    {
        let status = repo.git_overlay_worktree_status()?.unwrap_or_default();
        let report = authority_worktree_diff(&repo, &status, &options)?;
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

    if repo.current_state()?.is_none()
        && (matches!(from.as_deref(), Some("HEAD" | "@"))
            || matches!(to.as_deref(), Some("HEAD" | "@")))
        && repo.capability() == RepositoryCapability::GitOverlay
    {
        crate::cli::commands::snapshot::ensure_current_state(
            &repo,
            &UserConfig::load_default().unwrap_or_default(),
            Some("Bootstrap git-overlay before diffing HEAD".to_string()),
        )?;
    }

    let config = UserConfig::load_default().unwrap_or_default();
    let ctx = execution_context_from_cli_parts(cli, start, Some(repo), &config)?;
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
    paths: Vec<String>,
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
        paths,
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
        write_command_json(
            report,
            output_is_compact(cli),
            NextActionValidationContext::without_repo(&["diff"]),
        )?;
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

/// Open an existing Heddle store for `diff` classification. Missing stores
/// stay missing so plain Git can still be probed; any other open error
/// is returned instead of being dropped.
fn open_repo_for_classification(start: &Path) -> Result<Option<Repository>> {
    if discover_heddle_root(start).is_none() {
        return Ok(None);
    }
    Ok(Some(Repository::open(start)?))
}

fn clone_worktree_status(status: &WorktreeStatus) -> WorktreeStatus {
    WorktreeStatus {
        modified: status.modified.clone(),
        added: status.added.clone(),
        deleted: status.deleted.clone(),
    }
}

fn authority_worktree_diff(
    repo: &Repository,
    status: &WorktreeStatus,
    options: &DiffOptions,
) -> Result<DiffReport> {
    let mut report = if repo.capability() == RepositoryCapability::GitOverlay {
        plain_git_head_diff(
            &PlainGitDiffProbe {
                root: repo.root().to_path_buf(),
                changes: clone_worktree_status(status),
            },
            options,
        )?
    } else {
        diff_worktree_status(status, options, Some(repo), true)?
    };
    if options.show_context
        && let Some(state) = worktree_context_state(repo)?
    {
        attach_show_context(repo, &mut report, &state, options.paths.is_empty())?;
    }
    Ok(report)
}
