// SPDX-License-Identifier: Apache-2.0
//! Oplog command definitions.

use clap::Subcommand;

#[derive(Subcommand, Clone)]
pub enum OplogCommands {
    /// Salvage a truncated or torn operation log and report what was recovered.
    ///
    /// Runs the same recovery the everyday read path runs automatically:
    /// keeps every complete oplog record, quarantines the damaged file to
    /// `oplog.bin.corrupt`, writes an `oplog.bin.oplog.recovery` sidecar, and
    /// rebuilds `oplog.bin`. Unlike the silent auto-fallback, this reports the
    /// outcome (records recovered/lost, damaged byte range, quarantine path).
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
