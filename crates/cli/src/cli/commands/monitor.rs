// SPDX-License-Identifier: Apache-2.0
//! Monitor inspection command.

use anyhow::Result;
use serde::Serialize;

use crate::cli::{Cli, should_output_json, worktree_status_options};

#[derive(Serialize)]
struct MonitorOutput {
    backend: String,
    status: String,
    reason: Option<String>,
    changed_path_count: usize,
    changed_paths: Vec<String>,
}

pub fn cmd_monitor(cli: &Cli, paths: bool, serve: bool) -> Result<()> {
    let repo = cli.open_repo()?;
    if serve {
        return repo::run_local_monitor_helper(repo.root()).map_err(Into::into);
    }
    let report =
        repo.inspect_change_monitor_with_options(&worktree_status_options(Some(repo.config())))?;
    let output = MonitorOutput {
        changed_path_count: report.changed_paths.len(),
        changed_paths: if paths {
            report.changed_paths
        } else {
            Vec::new()
        },
        backend: report.backend,
        status: report.status,
        reason: report.reason,
    };

    if should_output_json(cli, Some(repo.config())) {
        println!("{}", serde_json::to_string(&output)?);
    } else {
        println!("Backend: {}", output.backend);
        println!("Status: {}", output.status);
        if let Some(reason) = &output.reason {
            println!("Reason: {}", reason);
        }
        println!("Changed paths: {}", output.changed_path_count);
        if paths {
            for path in &output.changed_paths {
                println!("  {}", path);
            }
        }
    }

    Ok(())
}
