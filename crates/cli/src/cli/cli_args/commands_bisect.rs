// SPDX-License-Identifier: Apache-2.0
//! Bisect command definitions.

use clap::Subcommand;

#[derive(Subcommand, Clone)]
pub enum BisectCommands {
    /// Start bisecting.
    Start,

    /// Mark commit as good.
    Good {
        /// Commit to mark as good (defaults to HEAD).
        commit: Option<String>,
    },

    /// Mark commit as bad.
    Bad {
        /// Commit to mark as bad (defaults to HEAD).
        commit: Option<String>,
    },

    /// Reset bisect state.
    Reset,
}
