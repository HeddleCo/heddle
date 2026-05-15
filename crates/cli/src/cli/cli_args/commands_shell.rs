// SPDX-License-Identifier: Apache-2.0
//! Shell integration helpers.

use clap::Subcommand;

#[derive(Clone, Copy, Debug, clap::ValueEnum, PartialEq, Eq)]
pub enum ShellKind {
    Zsh,
    Bash,
    Fish,
}

#[derive(Subcommand, Clone)]
pub enum ShellCommands {
    /// Emit a shell wrapper function on stdout. Source it from your
    /// shell rc to make `heddle start`, `heddle thread switch`, and
    /// `heddle thread cd` auto-`cd` into the target thread's
    /// worktree.
    ///
    /// Example install:
    ///   echo 'eval "$(heddle shell init zsh)"' >> ~/.zshrc
    Init {
        /// Shell to emit a function for.
        #[arg(value_enum)]
        kind: ShellKind,
    },
}
