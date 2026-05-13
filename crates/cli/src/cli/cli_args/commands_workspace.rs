// SPDX-License-Identifier: Apache-2.0
//! Workspace command definitions.

use clap::Subcommand;

/// Arguments for `workspace show`.
#[derive(Clone, Debug, clap::Args)]
pub struct WorkspaceShowArgs {
    /// Continuously refresh workspace status.
    #[arg(long)]
    pub watch: bool,

    /// Internal helper for tests: stop after N watch updates.
    #[arg(long, hide = true)]
    pub watch_iterations: Option<usize>,

    /// Internal helper for tests: polling interval in milliseconds.
    #[arg(long, hide = true)]
    pub watch_interval_ms: Option<u64>,
}

#[derive(Subcommand, Clone)]
pub enum WorkspaceCommands {
    /// Show the workspace control tower for the current repository.
    Show(WorkspaceShowArgs),
}