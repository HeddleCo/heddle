// SPDX-License-Identifier: Apache-2.0
//! Monitor inspection command.

use anyhow::Result;
use heddle_core::monitor_plan::monitor_human_lines;
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
        for line in monitor_human_lines(
            &output.backend,
            &output.status,
            output.reason.as_deref(),
            output.changed_path_count,
            &output.changed_paths,
            paths,
        ) {
            println!("{line}");
        }
    }

    Ok(())
}
