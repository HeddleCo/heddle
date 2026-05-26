// SPDX-License-Identifier: Apache-2.0
//! Marker command definitions.

use clap::Subcommand;

#[derive(Subcommand, Clone)]
pub enum MarkerCommands {
    /// List markers, optionally filtered by name prefix.
    ///
    /// Pass `--filter <PREFIX>` to return only markers whose name
    /// starts with the given prefix (e.g. `--filter failed-`). The
    /// match is a literal `starts_with` check — not a glob — so
    /// `--filter 'failed-*'` matches markers literally named
    /// `failed-*…`, which is almost certainly not what you want.
    List {
        /// Return only markers whose name starts with this prefix.
        /// Prefix match (not a glob).
        #[arg(long, value_name = "PREFIX")]
        filter: Option<String>,
    },

    /// Create marker at current state.
    Create {
        /// Marker name.
        name: String,
    },

    /// Delete marker(s).
    ///
    /// Pass an exact marker name, or `--prefix <PFX>` to delete every marker
    /// whose name starts with the given prefix (e.g. `--prefix failed-`).
    /// Exactly one of `<NAME>` or `--prefix` must be supplied.
    Delete {
        /// Marker name (exact match). Mutually exclusive with `--prefix`.
        #[arg(required_unless_present = "prefix", conflicts_with = "prefix")]
        name: Option<String>,

        /// Delete every marker whose name starts with this prefix.
        #[arg(long)]
        prefix: Option<String>,
    },

    /// Show marker details.
    Show {
        /// Marker name.
        name: String,
    },
}
