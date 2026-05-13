// SPDX-License-Identifier: Apache-2.0
//! Index command - inspect and manage the worktree index.

use anyhow::Result;
use repo::{Repository, WorktreeIndex};

use crate::cli::Cli;

pub fn cmd_index(cli: &Cli, dump: bool) -> Result<()> {
    let repo = Repository::open(cli.repo.as_ref().unwrap_or(&std::env::current_dir()?))?;

    let index_path = repo.root().join(".heddle/state").join("index.bin");

    if dump {
        if !index_path.exists() {
            println!(
                "No index found at {}. Run a snapshot or status command first.",
                index_path.display()
            );
            return Ok(());
        }

        println!("{}", WorktreeIndex::dump_from_path(&index_path)?);
    }

    Ok(())
}