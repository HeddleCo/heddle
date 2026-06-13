// SPDX-License-Identifier: Apache-2.0
//! State checkout implementation behind `heddle switch`.

use std::time::Instant;

use anyhow::Result;
use repo::Repository;
use serde::Serialize;
use tracing::debug;

use super::{
    history_target::{require_resolved_state, resolve_state_id},
    snapshot::ensure_current_state,
    worktree_safety::ensure_worktree_clean,
};
use crate::{
    cli::{Cli, should_output_json},
    config::UserConfig,
};

#[derive(Serialize)]
struct SwitchOutput {
    output_kind: &'static str,
    target: String,
    intent: Option<String>,
    message: String,
}

pub fn cmd_switch_state_checkout(cli: &Cli, target: String, force: bool) -> Result<()> {
    let repo_open_start = Instant::now();
    // `heddle switch X` advances *the active thread's* worktree. After
    // `thread switch modulo-race` the operator can run switch from any
    // directory and we still resolve the right checkout via metadata.
    // See `Repository::active_worktree_path`.
    let cwd_repo = cli.open_repo()?;
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
            Some("Bootstrap git-overlay before switch HEAD".to_string()),
        )?;
    }
    let target_id = resolve_state_id(&repo, &target)?;

    let current_worktree_verified_clean = if !force {
        ensure_worktree_clean(&repo, "switch")?;
        if let Some(current) = repo.current_state()? {
            let _ = repo.require_tree(&current.tree)?;
            true
        } else {
            false
        }
    } else {
        false
    };

    let target_state = require_resolved_state(&repo, &target_id)?;

    if current_worktree_verified_clean {
        repo.goto_verified_clean(&target_id)?;
    } else if force {
        repo.goto_discard_local(&target_id)?;
    } else {
        repo.goto(&target_id)?;
    }

    let output = SwitchOutput {
        output_kind: "thread_switch",
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
        "Switch command complete"
    );

    Ok(())
}
