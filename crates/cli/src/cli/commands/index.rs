// SPDX-License-Identifier: Apache-2.0
//! Index command - inspect and manage the worktree index.

use anyhow::Result;
use repo::WorktreeIndex;
use serde::Serialize;

use crate::cli::{Cli, should_output_json};

#[derive(Debug, Serialize)]
struct IndexOutput {
    output_kind: &'static str,
    present: bool,
    path: String,
    file_entries: usize,
    directory_entries: usize,
    untracked_directory_entries: usize,
    snapshot_bytes: u64,
    journal_bytes: u64,
    journal_ops: usize,
    journal_replay_ms: u128,
    dump: Option<String>,
}

pub fn cmd_index(cli: &Cli, dump: bool) -> Result<()> {
    let repo = cli.open_repo()?;

    let index_path = repo.root().join(".heddle/state").join("index.bin");
    let journal_path = repo.root().join(".heddle/state").join("index.journal");

    if should_output_json(cli, Some(repo.config())) {
        let output = match WorktreeIndex::load_profiled(&index_path) {
            Ok((index, stats)) => IndexOutput {
                output_kind: "index",
                present: true,
                path: index_path.display().to_string(),
                file_entries: index.len(),
                directory_entries: index.directory_len(),
                untracked_directory_entries: index.untracked_directory_len(),
                snapshot_bytes: stats.snapshot_bytes,
                journal_bytes: stats.journal_bytes,
                journal_ops: stats.journal_ops,
                journal_replay_ms: stats.journal_replay_ms,
                dump: dump.then(|| index.dump()),
            },
            Err(_) if !index_path.exists() => IndexOutput {
                output_kind: "index",
                present: false,
                path: index_path.display().to_string(),
                file_entries: 0,
                directory_entries: 0,
                untracked_directory_entries: 0,
                snapshot_bytes: 0,
                journal_bytes: file_len_or_zero(&journal_path),
                journal_ops: 0,
                journal_replay_ms: 0,
                dump: None,
            },
            Err(error) => return Err(error.into()),
        };
        crate::cli::render::write_json_stdout(&output)?;
        return Ok(());
    }

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

fn file_len_or_zero(path: &std::path::Path) -> u64 {
    std::fs::metadata(path).map(|meta| meta.len()).unwrap_or(0)
}
