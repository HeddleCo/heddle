// SPDX-License-Identifier: Apache-2.0
//! Bisect command - binary search for bugs.

use std::{fs, path::PathBuf};

use anyhow::{Result, anyhow};
use repo::Repository;

use super::{advice::RecoveryAdvice, snapshot::ensure_current_state};
use crate::{
    cli::{Cli, cli_args::BisectCommands, should_output_json},
    config::UserConfig,
};

const BISECT_STATE_FILE: &str = "BISECT_STATE";

fn bisect_state_path(repo: &Repository) -> PathBuf {
    repo.heddle_dir().join(BISECT_STATE_FILE)
}

pub(crate) fn is_bisect_active(repo: &Repository) -> bool {
    bisect_state_path(repo).exists()
}

pub(crate) fn reset_bisect_state(repo: &Repository) -> Result<()> {
    let state_path = bisect_state_path(repo);
    if state_path.exists() {
        fs::remove_file(&state_path)?;
    }
    Ok(())
}

fn resolve_commit(repo: &Repository, commit: Option<&str>) -> Result<String> {
    match commit {
        Some(c) => repo
            .resolve_state(c)?
            .ok_or_else(|| anyhow!("Commit {} not found", c))
            .map(|_| c.to_string()),
        None => {
            ensure_current_state(
                repo,
                &UserConfig::load_default().unwrap_or_default(),
                Some("Bootstrap git-overlay before bisect".to_string()),
            )?;
            Ok("HEAD".to_string())
        }
    }
}

pub fn cmd_bisect(cli: &Cli, command: BisectCommands) -> Result<()> {
    let repo = Repository::open(cli.repo.as_ref().unwrap_or(&std::env::current_dir()?))?;

    match command {
        BisectCommands::Start => {
            let state_path = bisect_state_path(&repo);
            fs::write(&state_path, "{}\n")?;

            if should_output_json(cli, Some(repo.config())) {
                println!("{{\"status\": \"started\"}}");
            } else {
                println!("Bisect started");
            }
        }
        BisectCommands::Good { commit } => {
            if !is_bisect_active(&repo) {
                return Err(anyhow!(no_bisect_in_progress_advice("mark bisect good")));
            }

            let resolved = resolve_commit(&repo, commit.as_deref())?;
            if should_output_json(cli, Some(repo.config())) {
                println!(
                    "{{\"status\": \"marked_good\", \"commit\": \"{}\"}}",
                    resolved
                );
            } else {
                println!("Marked {} as good", resolved);
            }
        }
        BisectCommands::Bad { commit } => {
            if !is_bisect_active(&repo) {
                return Err(anyhow!(no_bisect_in_progress_advice("mark bisect bad")));
            }

            let resolved = resolve_commit(&repo, commit.as_deref())?;
            if should_output_json(cli, Some(repo.config())) {
                println!(
                    "{{\"status\": \"marked_bad\", \"commit\": \"{}\"}}",
                    resolved
                );
            } else {
                println!("Marked {} as bad", resolved);
            }
        }
        BisectCommands::Reset => {
            reset_bisect_state(&repo)?;

            if should_output_json(cli, Some(repo.config())) {
                println!("{{\"status\": \"reset\"}}");
            } else {
                println!("Bisect reset");
            }
        }
    }

    Ok(())
}

fn no_bisect_in_progress_advice(action: &'static str) -> RecoveryAdvice {
    RecoveryAdvice::safety_refusal(
        "no_bisect_in_progress",
        "No bisect in progress",
        "Start one with `heddle bisect start`, then mark states with `heddle bisect good <state>` or `heddle bisect bad <state>`.",
        "the repository has no persisted Heddle bisect state",
        format!("{action} would need to update an active bisect search"),
        "repository state was left unchanged",
        "heddle bisect start",
        vec![
            "heddle bisect start".to_string(),
            "heddle bisect good <state>".to_string(),
            "heddle bisect bad <state>".to_string(),
        ],
    )
}
