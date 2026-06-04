// SPDX-License-Identifier: Apache-2.0
//! Completion command - generate shell completion scripts.

use anyhow::{Result, anyhow};
use clap::CommandFactory;
use clap_complete::{Shell, generate};

use super::advice::RecoveryAdvice;
use crate::cli::Cli;

pub fn cmd_completion(shell: String) -> Result<()> {
    let mut cmd = Cli::command();

    match shell.as_str() {
        "bash" => {
            generate(Shell::Bash, &mut cmd, "heddle", &mut std::io::stdout());
        }
        "zsh" => {
            generate(Shell::Zsh, &mut cmd, "heddle", &mut std::io::stdout());
        }
        "fish" => {
            generate(Shell::Fish, &mut cmd, "heddle", &mut std::io::stdout());
        }
        _ => {
            return Err(anyhow!(RecoveryAdvice::invalid_usage(
                "completion_shell_unsupported",
                format!("Unsupported shell: {shell}. Supported shells: bash, zsh, fish"),
                "Use one of: bash, zsh, fish.",
                "heddle shell completion bash",
            )));
        }
    }

    Ok(())
}
