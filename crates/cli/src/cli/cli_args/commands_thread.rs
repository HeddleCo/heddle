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
    #[command(after_help = "\
Advanced split form:
  heddle start <name> --path <dir> is the normal one-step isolated-checkout path.
  heddle thread create <name> only creates the thread ref. Pair it later with
  heddle thread promote <name> --path <dir> when you intentionally need to
  create the ref now and materialize the checkout later.
")]
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

    /// Print the name of the current thread (the thread the working
    /// checkout is attached to). Read-only — no state change.
    /// Useful in shell pipelines: `cd "$(heddle thread cd "$(heddle thread current)")"`.
    Current,

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
        /// Discard uncommitted changes in the current checkout before switching.
        #[arg(short, long)]
        force: bool,
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

    /// Materialize an existing thread ref at a chosen path.
    #[command(after_help = "\
Advanced split form:
  heddle start <name> --path <dir> creates the thread ref and isolated checkout
  in one step. `thread promote` is the second step after
  `heddle thread create <name>` when you intentionally created the ref first
  and want to materialize it later.
")]
    Promote(ThreadPromoteArgs),

    /// Drop a thread and mark it abandoned.
    #[command(visible_alias = "delete")]
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

    /// Sweep merged or stale auto-created threads.
    #[command(
        long_about = "\
Sweep threads that have outlived their usefulness. Cleanup removes recorded checkouts, marks matching thread records abandoned, and prunes live thread refs so everyday thread lists stay focused.

Modes:
  - --merged: clean up threads recorded as merged.
  - --auto --older-than <duration>: clean up harness-created threads that have not been touched in the given duration.

The two modes can be combined. Pair with --dry-run to preview the work without changing anything on disk.",
        after_help = "\
Examples:
  heddle thread cleanup --merged --dry-run
  heddle thread cleanup --merged
  heddle thread cleanup --auto --older-than 7d --dry-run
"
    )]
    Cleanup(ThreadCleanupArgs),

    /// Manage named state markers under the thread namespace.
    Marker {
        #[command(subcommand)]
        command: ThreadMarkerCommands,
    },
}

#[derive(Subcommand, Clone)]
pub enum ThreadMarkerCommands {
    /// List markers, optionally filtered by name prefix.
    ///
    /// Pass `--filter <PREFIX>` to return only markers whose name
    /// starts with the given prefix. The match is a literal
    /// `starts_with` check, not a glob.
    List {
        /// Return only markers whose name starts with this prefix.
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
    /// whose name starts with the given prefix. Exactly one of `<NAME>` or
    /// `--prefix` must be supplied.
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
    /// Clean up threads whose recorded state is `merged`.
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
