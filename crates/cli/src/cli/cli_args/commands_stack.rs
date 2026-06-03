// SPDX-License-Identifier: Apache-2.0
//! `heddle stack` subcommand definitions.
//!
//! A "stack" here is the descendant tree of a thread formed by walking
//! [`repo::ThreadRecord::parent_thread`] links. The top-level
//! `heddle stack` (with no subcommand) describes the stack the current
//! thread participates in. Sub-verbs surface stack-aware queries.

use clap::{Args, Subcommand};

/// Top-level args for `heddle stack [<subcommand>]`.
///
/// The subcommand is optional; when omitted, `heddle stack` describes
/// the stack containing the current thread (or all stacks, when
/// detached). When a `--thread` override is passed at the top level it
/// is forwarded to the relevant subcommand so the CLI surface stays
/// composable.
#[derive(Args, Clone, Debug)]
pub struct StackArgs {
    /// Operate on the stack containing this thread instead of the
    /// currently-attached thread. Accepts either the thread ref name or
    /// any descendant — discovery walks up to the root automatically.
    #[arg(long)]
    pub thread: Option<String>,

    #[command(subcommand)]
    pub command: Option<StackCommands>,
}

#[derive(Subcommand, Clone, Debug)]
pub enum StackCommands {
    /// Surface the next stack-level action: ready, blocked, or
    /// waiting-on-review.
    ///
    /// Walks the stack containing the named thread (or the current
    /// thread, by default) and emits one of three verdicts:
    ///
    /// * `ready` — every member of the stack is Ready / Merged /
    ///   Promoted; you can land the bottom.
    /// * `blocked` — at least one member is Blocked; that thread is
    ///   named in the output so you know where to look.
    /// * `waiting-on-review` — the stack is otherwise clean but the
    ///   top is still Active / Draft. The leaf is the bottleneck.
    Ready {
        /// Override the thread whose stack to inspect.
        #[arg(long)]
        thread: Option<String>,
    },

    /// Print a serialized `RepositorySnapshot` for the current thread's
    /// stack.
    ///
    /// The JSON shape is documented inline on
    /// `repo::stack_snapshot::RepositorySnapshot`. Future tooling
    /// (agentic harnesses, remote viewers) should consume it directly.
    Snapshot {
        /// Override the thread whose stack to capture.
        #[arg(long)]
        thread: Option<String>,
    },
}
