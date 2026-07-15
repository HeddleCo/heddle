// SPDX-License-Identifier: Apache-2.0
//! Out-of-process native filesystem monitor for Heddle worktrees.

use std::{path::PathBuf, process::ExitCode};

fn main() -> ExitCode {
    let mut args = std::env::args_os().skip(1);
    let repo_root = match (args.next(), args.next(), args.next()) {
        (Some(flag), Some(path), None) if flag == "--repo-root" => PathBuf::from(path),
        _ => {
            eprintln!("usage: heddle-fsmonitor-worker --repo-root <path>");
            return ExitCode::from(2);
        }
    };

    match repo::run_local_monitor_helper(&repo_root) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("heddle-fsmonitor-worker: {error}");
            ExitCode::FAILURE
        }
    }
}
