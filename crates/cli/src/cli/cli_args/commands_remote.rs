// SPDX-License-Identifier: Apache-2.0
//! Remote command definitions.

use clap::Subcommand;

#[derive(Subcommand, Clone)]
pub enum RemoteCommands {
    /// List configured remotes.
    List,

    /// Add a remote.
    Add {
        /// Remote name.
        name: String,
        /// Remote URL (host:port).
        url: String,
    },

    /// Remove a remote.
    Remove {
        /// Remote name.
        name: String,
    },

    /// Show remote details.
    Show {
        /// Remote name.
        name: String,
    },
}