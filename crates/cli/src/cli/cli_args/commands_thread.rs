// SPDX-License-Identifier: Apache-2.0
//! Thread command definitions.

use clap::{Args, Subcommand};

use super::{
    ThreadAbsorbArgs, ThreadApprovalsArgs, ThreadApproveArgs, ThreadCapturesArgs,
    ThreadCheckMergeArgs, ThreadDropArgs, ThreadMoveArgs, ThreadNameArgs, ThreadPromoteArgs,
    ThreadRenameArgs, ThreadResolveArgs, ThreadRevokeApprovalArgs, ThreadShowArgs,
};

#[derive(Subcommand, Clone)]
pub enum ThreadCommands {
    /// Create a thread ref at the current state.
    Create {
        /// Thread identifier.
        name: String,
        /// Mark the thread ephemeral. Auto-collapses after `--ttl` if not
        /// promoted. The collapse is recorded as
        /// `OpRecord::EphemeralThreadCollapse`; underlying states stay
        /// addressable. (W1/A13.)
        #[arg(long)]
        ephemeral: bool,
        /// TTL in seconds. Defaults to 24h when `--ephemeral` is set
        /// without `--ttl`.
        #[arg(long, requires = "ephemeral")]
        ttl_secs: Option<u32>,
    },

    /// Switch the current checkout to an existing thread ref.
    Switch {
        /// Thread identifier.
        name: String,
        /// Print only the target thread's checkout path on stdout and
        /// exit. Used by the shell hook (`heddle shell init`) to auto-cd
        /// into the new thread:
        ///   dir=$(heddle thread switch X --print-cd-path) && cd "$dir"
        /// Auto-capture still runs; rich output is suppressed.
        #[arg(long, hide_short_help = true)]
        print_cd_path: bool,
    },

    /// Print the on-disk path for a thread. Read-only — no state change,
    /// no auto-capture. Pair with the shell hook (`heddle shell init`)
    /// to land in the right directory:
    ///   eval "$(heddle thread cd X)"
    /// Or use the shell function directly: `heddle thread cd X` becomes
    /// `cd <path>` when the hook is installed.
    Cd {
        /// Thread identifier.
        name: String,
    },

    /// List threads.
    List(ThreadListArgs),

    /// Show one thread with actor and workflow context.
    Show(ThreadShowArgs),

    /// Show granular captures on a thread.
    Captures(ThreadCapturesArgs),

    /// Rename a thread ref.
    Rename(ThreadRenameArgs),

    /// Refresh a thread onto its target thread.
    Refresh(ThreadNameArgs),

    /// Move selected captured paths from one thread into another.
    Move(ThreadMoveArgs),

    /// Absorb a child thread into its parent or another thread.
    Absorb(ThreadAbsorbArgs),

    /// Guide a blocked or stale thread toward its next clean state.
    Resolve(ThreadResolveArgs),

    /// Promote a thread to a heavy checkout at a chosen path.
    Promote(ThreadPromoteArgs),

    /// Drop a thread and mark it abandoned.
    Drop(ThreadDropArgs),

    /// Record a merge approval for `<source> -> <target>`.
    Approve(ThreadApproveArgs),

    /// List approvals recorded for `<source> -> <target>`.
    Approvals(ThreadApprovalsArgs),

    /// Revoke a previously recorded approval by id.
    RevokeApproval(ThreadRevokeApprovalArgs),

    /// Check whether `<source> -> <target>` would merge under
    /// the repo's branch-protection policies. Read-only.
    CheckMerge(ThreadCheckMergeArgs),

    /// Sweep threads that have outlived their usefulness — drop their
    /// checkouts and registry entries to reclaim disk and de-clutter
    /// `heddle thread list`.
    ///
    /// Two modes are supported:
    ///   * `--merged`: drop threads in [`ThreadState::Merged`].
    ///   * `--auto --older-than <duration>`: drop harness-created
    ///     threads that have not been touched in the given duration.
    ///
    /// The two flags can be combined to sweep both classes in one
    /// invocation. Pair with `--dry-run` to preview the work without
    /// changing anything on disk.
    Cleanup(ThreadCleanupArgs),
}

/// Arguments for `heddle thread list`.
///
/// The default view hides harness-auto-created threads (those marked
/// with `auto: true` on disk). Pass `--include-auto` to surface them.
#[derive(Args, Clone, Debug, Default)]
pub struct ThreadListArgs {
    /// Include threads created automatically by harness integrations
    /// (e.g. Claude Code segment-rotation). Hidden by default to keep
    /// the view focused on threads the user explicitly created.
    #[arg(long)]
    pub include_auto: bool,
}

/// Arguments for `heddle thread cleanup`.
///
/// At least one of `--merged` or `--auto` must be set; otherwise the
/// command refuses with a clear message. `--older-than` is required
/// when `--auto` is set.
#[derive(Args, Clone, Debug)]
pub struct ThreadCleanupArgs {
    /// Drop threads whose recorded state is `merged`. Their checkouts
    /// and registry entries are removed; the underlying ref and
    /// states remain addressable.
    #[arg(long)]
    pub merged: bool,

    /// Drop harness-auto-created threads (those tagged `auto: true`).
    /// Combine with `--older-than` to gate the sweep on staleness.
    #[arg(long)]
    pub auto: bool,

    /// Maximum age (since `updated_at`) for an auto-thread to be
    /// considered live. Threads older than this are eligible for
    /// sweep when `--auto` is set. Accepts a Go-style duration like
    /// `7d`, `24h`, `30m`, `15s` (or a raw integer interpreted as
    /// seconds).
    #[arg(long, value_name = "DURATION")]
    pub older_than: Option<String>,

    /// Print what would be dropped without actually dropping it.
    #[arg(long)]
    pub dry_run: bool,
}
