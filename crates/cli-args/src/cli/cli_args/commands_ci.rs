// SPDX-License-Identifier: Apache-2.0
//! Local CI executor arguments.

use std::path::PathBuf;

use clap::{Args, Subcommand};

/// CI executor subcommands.
#[derive(Clone, Debug, Subcommand)]
pub enum CiCommands {
    /// Run the SDK compile output at `.heddle/treadle.definition.bin`.
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

    /// Read a canonical TreadleDefinition protobuf instead of `.heddle/treadle.definition.bin`. `treadle.lock.json` next to the bin is required.
    #[arg(long, value_name = "PATH")]
    pub config: Option<PathBuf>,

    /// Run only this named check (not a job); may be repeated. Unlisted checks are omitted.
    #[arg(long = "check", value_name = "NAME")]
    pub checks: Vec<String>,
}
