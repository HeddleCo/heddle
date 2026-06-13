// SPDX-License-Identifier: Apache-2.0
//! Args for the W2 advanced verbs (A11–A15).

use clap::{Args, Subcommand};

#[derive(Clone, Debug, Args)]
pub struct CheckpointArgs {
    /// Git-facing commit summary for the current captured work.
    #[arg(short = 'm', long)]
    pub message: Option<String>,

    /// Internal recovery: retry a failed staged-index commit checkpoint against the current
    /// preserved Heddle state without requiring the dirty worktree to match it.
    #[arg(long = "from-index-snapshot", hide = true)]
    pub from_index_snapshot: bool,
}

#[derive(Clone, Debug, Subcommand)]
pub enum TransactionCommands {
    /// Begin a new transaction. Returns its id.
    Begin(TransactionBeginArgs),
    /// Commit a transaction. Buffered ops apply atomically.
    Commit(TransactionIdArgs),
    /// Abort a transaction. Buffered ops are discarded.
    Abort(TransactionAbortArgs),
    /// Show a transaction's current state.
    Status(TransactionIdArgs),
}

#[derive(Clone, Debug, Args)]
pub struct TransactionBeginArgs {
    /// Thread the transaction targets. Defaults to HEAD-attached thread.
    #[arg(long)]
    pub thread: Option<String>,
    /// Optional message describing the transaction's purpose.
    #[arg(long)]
    pub message: Option<String>,
}

#[derive(Clone, Debug, Args)]
pub struct TransactionIdArgs {
    pub transaction_id: String,
}

#[derive(Clone, Debug, Args)]
pub struct TransactionAbortArgs {
    pub transaction_id: String,
    /// Reason for aborting (recorded with the abort op).
    #[arg(long, default_value = "user-requested abort")]
    pub reason: String,
}
