// SPDX-License-Identifier: Apache-2.0
//! Stash command definitions.

use clap::Subcommand;

#[derive(Subcommand, Clone)]
pub enum StashCommands {
    /// Save changes to stash.
    Push {
        /// Stash message.
        #[arg(short = 'm', long)]
        message: Option<String>,
    },

    /// List all stashes.
    List,

    /// Apply and remove top stash.
    Pop,

    /// Apply top stash without removing.
    Apply,

    /// Drop top stash.
    Drop,

    /// Clear all stashes.
    Clear,

    /// Show stash contents.
    Show,
}