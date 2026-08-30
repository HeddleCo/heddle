// SPDX-License-Identifier: Apache-2.0
//! Local CI executor arguments.

use std::path::PathBuf;

use clap::{Args, Subcommand};

/// CI executor subcommands.
#[derive(Clone, Debug, Subcommand)]
pub enum CiCommands {
    /// Compile an SDK authoring file if needed, then run `.heddle/treadle.definition.bin`.
    Run(CiRunArgs),
}

/// Arguments for `heddle ci run`.
#[derive(Clone, Debug, Args)]
pub struct CiRunArgs {
    /// Select the local, device-signed executor.
    #[arg(long, required = true)]
    pub local: bool,

    /// Evaluate an immutable state instead of the current working tree.
    #[arg(long, value_name = "STATE")]
    pub state: Option<String>,

    /// Run this `.bin` (lock next to it, do not compile), or compile this `ci.*` source into `.heddle/` then run.
    #[arg(long, value_name = "PATH")]
    pub config: Option<PathBuf>,

    /// Run only this named check (not a job); may be repeated. Unlisted checks are omitted.
    #[arg(long = "check", value_name = "NAME")]
    pub checks: Vec<String>,
}
