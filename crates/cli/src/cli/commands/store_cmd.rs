// SPDX-License-Identifier: Apache-2.0
//! Object-store maintenance commands. Currently exposes
//! `heddle store warm <state>` for the proactive canonical-store
//! warming pass — see `Repository::warm_canonical_store_for_state`
//! for the underlying mechanism.

use anyhow::{Result, anyhow};
use serde::Serialize;

use crate::cli::{Cli, StoreCommands, should_output_json};

#[derive(Serialize)]
struct WarmOutput {
    state: String,
    promoted: usize,
    already_loose: usize,
    errors: usize,
    total: usize,
}

pub fn cmd_store(cli: &Cli, command: StoreCommands) -> Result<()> {
    let repo = repo::Repository::open(cli.repo.as_ref().unwrap_or(&std::env::current_dir()?))?;

    match command {
        StoreCommands::Warm { state } => {
            let spec = state.unwrap_or_else(|| "HEAD".to_string());
            let resolved = repo
                .resolve_state(&spec)?
                .ok_or_else(|| anyhow!("could not resolve state '{}'", spec))?;
            let stats = repo.warm_canonical_store_for_state(&resolved)?;
            let output = WarmOutput {
                state: resolved.to_string(),
                promoted: stats.promoted,
                already_loose: stats.already_loose,
                errors: stats.errors,
                total: stats.total(),
            };
            if should_output_json(cli, Some(repo.config())) {
                println!("{}", serde_json::to_string(&output)?);
            } else {
                println!("Warmed canonical store for state {}", output.state);
                println!(
                    "  promoted     {} (decompressed/wrote raw bytes to canonical loose path)",
                    output.promoted
                );
                println!(
                    "  already loose {} (already uncompressed; no work)",
                    output.already_loose
                );
                if output.errors > 0 {
                    println!(
                        "  errors       {} (non-fatal; lazy promotion will retry on materialize)",
                        output.errors
                    );
                }
                println!("  total        {}", output.total);
            }
        }
    }
    Ok(())
}
