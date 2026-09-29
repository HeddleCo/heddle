// SPDX-License-Identifier: Apache-2.0
//! `heddle ci` executor and hosted evidence recording.

mod compile;
mod render;
mod run;
mod target;

use anyhow::Result;
use heddle_cli_args::{CiCommands, Cli};

/// Dispatch a CI subcommand.
pub async fn cmd_ci(cli: &Cli, command: &CiCommands) -> Result<()> {
    match command {
        CiCommands::Run(args) => run::run(cli, args).await,
    }
}
