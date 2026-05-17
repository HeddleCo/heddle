// SPDX-License-Identifier: Apache-2.0
//! Goto command.

use std::time::Instant;

use anyhow::{Result, anyhow};
use repo::Repository;
use serde::Serialize;
use tracing::debug;

use super::{history_target::resolve_state_id, snapshot::ensure_current_state};
use crate::{
    cli::{Cli, should_output_json, worktree_status_options},
    config::UserConfig,
};

#[derive(Serialize)]
struct GotoOutput {
    target: String,
    intent: Option<String>,
    message: String,
}

pub fn cmd_goto(cli: &Cli, target: String, force: bool) -> Result<()> {
    let repo_open_start = Instant::now();
    // `heddle goto X` advances *the active thread's* worktree. After
    // `thread switch modulo-race` the operator can run goto from any
    // directory and we still resolve the right checkout via metadata.
    // See `Repository::active_worktree_path`.
    let cwd_repo = Repository::open(cli.repo.as_ref().unwrap_or(&std::env::current_dir()?))?;
    let target_path = cwd_repo.active_worktree_path()?;
    let repo = if target_path == *cwd_repo.root() {
        cwd_repo
    } else {
        Repository::open(&target_path)?
    };
    let repo_open_ms = repo_open_start.elapsed().as_millis();
    let body_start = Instant::now();

    if matches!(target.as_str(), "HEAD" | "@") && repo.current_state()?.is_none() {
        ensure_current_state(
            &repo,
            &UserConfig::load_default().unwrap_or_default(),
            Some("Bootstrap git-overlay before goto HEAD".to_string()),
        )?;
    }
    let target_id = resolve_state_id(&repo, &target)?;

    let mut current_worktree_verified_clean = false;

    // Check for uncommitted changes
    if !force && let Some(current) = repo.current_state()? {
        let tree = repo.require_tree(&current.tree)?;
        let status = repo.compare_worktree_cached_with_options(
            &tree,
            &worktree_status_options(Some(repo.config())),
        )?;

        if !status.is_clean() {
            return Err(anyhow!(
                "Cannot goto: you have uncommitted changes.\n\
                     Use --force to discard them, or snapshot first."
            ));
        }

        current_worktree_verified_clean = true;
    }

    let target_state = repo
        .store()
        .get_state(&target_id)?
        .ok_or_else(|| anyhow!("State not found: {}", target))?;

    if current_worktree_verified_clean {
        repo.goto_verified_clean(&target_id)?;
    } else {
        repo.goto(&target_id)?;
    }

    let output = GotoOutput {
        target: target_id.short(),
        intent: target_state.intent.clone(),
        message: format!("Now at: {}", target_id.short()),
    };

    if should_output_json(cli, Some(repo.config())) {
        println!("{}", serde_json::to_string(&output)?);
    } else {
        println!("{}", output.message);
        if let Some(intent) = &output.intent {
            println!("  {}", intent);
        }
    }

    debug!(
        repo_open_ms,
        body_ms = body_start.elapsed().as_millis(),
        total_ms = repo_open_ms + body_start.elapsed().as_millis(),
        "Goto command complete"
    );

    Ok(())
}