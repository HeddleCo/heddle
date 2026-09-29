// SPDX-License-Identifier: Apache-2.0
//! CI executor arguments.

use std::path::PathBuf;

use clap::{Args, Subcommand};

/// CI executor subcommands.
#[derive(Clone, Debug, Subcommand)]
pub enum CiCommands {
    /// Run checks locally or record signed evidence for a published State.
    Run(CiRunArgs),
}

/// Arguments for `heddle ci run`.
#[derive(Clone, Debug, Args)]
pub struct CiRunArgs {
    /// Run checks locally and print device-signed verdicts.
    #[arg(long, required_unless_present = "record", conflicts_with = "record")]
    pub local: bool,

    /// Run checks and record signed evidence for the exact published State on hosted.
    #[arg(long, required_unless_present = "local", conflicts_with = "local")]
    pub record: bool,

    /// Evaluate an immutable state instead of the current working tree.
    #[arg(long, value_name = "STATE")]
    pub state: Option<String>,

    /// Run this `.bin` (lock next to it, do not compile), or compile this source (`.ts`/`.mjs`/`.rs`/`.go`) into `.heddle/` then run.
    #[arg(long, value_name = "PATH")]
    pub config: Option<PathBuf>,

    /// Run only this named check (not a job); may be repeated. Unlisted checks are omitted.
    #[arg(long = "check", value_name = "NAME")]
    pub checks: Vec<String>,
}
