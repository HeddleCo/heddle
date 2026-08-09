// SPDX-License-Identifier: Apache-2.0
//! Oplog command definitions.

use clap::Subcommand;

#[derive(Subcommand, Clone)]
pub enum OplogCommands {
    /// Salvage a truncated or torn operation log and report what was recovered.
    ///
    /// Explicitly authorizes recovery before ordinary repository open: keeps
    /// every complete oplog record, quarantines a damaged selected container
    /// beside the original with a `.corrupt` suffix, writes its
    /// `.oplog.recovery` sidecar, and rebuilds that container. If repairing an
    /// earlier immutable segment breaks the EntryId chain, the command also
    /// reports the complete later segments and entries it discarded.
    ///
    /// When the oplog is already healthy it makes no changes and, if a prior
    /// recovery left a sidecar, reports that last recovery instead.
    #[command(after_help = "\
Examples:
  heddle oplog recover                 # salvage and print a human report
  heddle oplog recover --output json   # machine-readable recovery report
")]
    Recover,
}
