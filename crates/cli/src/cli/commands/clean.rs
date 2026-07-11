// SPDX-License-Identifier: Apache-2.0
//! Clean command - remove untracked files from worktree.

use std::{collections::BTreeSet, fs, path::Path};

use anyhow::{Result, anyhow};
use heddle_core::clean_plan::clean_result_lines;
use objects::fs_ops::remove_path_recursively;
use repo::Repository;
use serde::Serialize;

use super::advice::RecoveryAdvice;
use crate::cli::{Cli, should_output_json, worktree_status_options};

#[derive(Serialize)]
struct CleanOutput {
    output_kind: &'static str,
    removed: Vec<String>,
    dry_run: bool,
}

pub fn cmd_clean(cli: &Cli, force: bool, dry_run: bool) -> Result<()> {
    let repo = cli.open_repo()?;

    if !force && !dry_run {
        return Err(anyhow!(RecoveryAdvice::destructive_requires_force(
            "clean",
            "untracked paths may contain work Heddle has not captured",
            "`clean --force` removes untracked files and directories from the worktree",
            "heddle clean --dry-run",
            "heddle clean --force",
            "nothing was removed",
        )));
    }

    let current_state = repo.current_state()?;
    let tree = match current_state.as_ref() {
        Some(s) => repo.require_tree(&s.tree)?,
        None => objects::object::Tree::new(),
    };

    let detailed = repo.compare_worktree_cached_detailed_with_options(
        &tree,
        &worktree_status_options(Some(repo.config())),
    )?;

    if detailed.untracked.is_empty() {
        output_result(cli, &repo, &[], dry_run)?;
        return Ok(());
    }

    if dry_run {
        let paths: Vec<String> = detailed
            .untracked
            .flatten_paths()
            .iter()
            .map(|p| p.display().to_string())
            .collect();
        output_result(cli, &repo, &paths, dry_run)?;
        return Ok(());
    }

    let mut removed: Vec<String> = Vec::new();
    let mut parent_dirs: BTreeSet<std::path::PathBuf> = BTreeSet::new();

    let removed_paths = detailed.untracked.flatten_paths();
    for path in detailed.untracked.removal_roots() {
        let full_path = repo.root().join(&path);

        if full_path.exists() {
            if full_path.is_symlink() {
                fs::remove_file(&full_path)?;
            } else if full_path.is_dir() {
                remove_path_recursively(&full_path)?;
                if let Some(parent) = path.parent()
                    && !parent.as_os_str().is_empty()
                {
                    parent_dirs.insert(repo.root().join(parent));
                }
            } else {
                fs::remove_file(&full_path)?;

                if let Some(parent) = path.parent()
                    && !parent.as_os_str().is_empty()
                {
                    parent_dirs.insert(repo.root().join(parent));
                }
            }
        }
    }

    removed.extend(removed_paths.iter().map(|path| path.display().to_string()));
    removed.sort();

    for dir in parent_dirs.iter().rev() {
        if dir.exists() && is_empty_dir(dir) {
            fs::remove_dir(dir)?;
        }
    }

    output_result(cli, &repo, &removed, dry_run)?;
    Ok(())
}

fn is_empty_dir(path: &Path) -> bool {
    match fs::read_dir(path) {
        Ok(mut entries) => entries.next().is_none(),
        Err(_) => false,
    }
}

fn output_result(cli: &Cli, repo: &Repository, removed: &[String], dry_run: bool) -> Result<()> {
    if should_output_json(cli, Some(repo.config())) {
        println!(
            "{}",
            serde_json::to_string(&CleanOutput {
                output_kind: "clean",
                removed: removed.to_vec(),
                dry_run
            })?
        );
    } else {
        for line in clean_result_lines(removed, dry_run) {
            println!("{line}");
        }
    }
    Ok(())
}
