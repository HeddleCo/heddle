// SPDX-License-Identifier: Apache-2.0
//! Initialize command.

use std::path::PathBuf;

use anyhow::Result;
use repo::Repository;
use serde::Serialize;
use tracing::{debug, info};

use crate::{
    cli::{Cli, InitArgs, should_output_json},
    config::UserConfig,
};

#[derive(Serialize)]
struct InitOutput {
    path: PathBuf,
    message: String,
}

pub fn cmd_init(cli: &Cli, args: InitArgs) -> Result<()> {
    let path = match args.path.clone() {
        Some(p) => p,
        None => std::env::current_dir()
            .map_err(|e| anyhow::anyhow!("Failed to determine current directory: {}", e))?,
    };
    let path = path.canonicalize().unwrap_or(path.clone());

    info!(path = %path.display(), "Initializing repository");

    // If the directory already has a `.git` (or is inside one), leave the
    // `main` thread unseeded: the user almost certainly wants to import from
    // Git next, and pre-seeding would make `main` point at a throwaway
    // empty-tree snapshot. Otherwise, seed `main` so the repo is immediately
    // usable for snapshot/history/etc.
    let has_git = gix::discover(&path).is_ok();
    let repo = if has_git {
        Repository::bootstrap_git_overlay(&path)?
    } else {
        Repository::init_default(&path)?
    };

    debug!(heddle_dir = %repo.heddle_dir().display(), "Repository initialized");

    if args.principal_name.is_some() || args.principal_email.is_some() {
        let name = args
            .principal_name
            .clone()
            .ok_or_else(|| anyhow::anyhow!("--principal-name is required"))?;
        let email = args
            .principal_email
            .clone()
            .ok_or_else(|| anyhow::anyhow!("--principal-email is required"))?;
        let mut config = UserConfig::load_default()?;
        config.set_principal(name.clone(), email.clone());
        let config_path = config.save_default()?;
        info!(principal_name = %name, principal_email = %email, "Principal configured");
        debug!(config_path = %config_path.display(), "User config updated");
    }

    super::maybe_prompt_init_install(cli, &repo, &args)?;

    let output = InitOutput {
        path: repo.heddle_dir().to_path_buf(),
        message: if has_git {
            format!(
                "Initialized Heddle sidecar in {} for Git-overlay workflows",
                repo.heddle_dir().display()
            )
        } else {
            format!(
                "Initialized Heddle repository in {}",
                repo.heddle_dir().display()
            )
        },
    };

    render_init(&output, should_output_json(cli, Some(repo.config())))
}

fn render_init(output: &InitOutput, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string(output)?);
    } else {
        println!("{}", output.message);
    }
    Ok(())
}