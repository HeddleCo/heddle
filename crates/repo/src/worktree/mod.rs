// SPDX-License-Identifier: Apache-2.0
//! Worktree stack: index, ignore rules, walking, status scans, stat
//! signatures, and fsmonitors. Grouped here; the crate root re-exports
//! the established module paths so callers don't churn.

pub(crate) mod fsmonitor;
pub mod git_worktree_status;
pub(crate) mod stat_signature;
pub(crate) mod status_tracked_refresh;
pub(crate) mod status_untracked_scan;
pub(crate) mod worktree_ignore;
pub mod worktree_index;
pub(crate) mod worktree_state;
pub mod worktree_status_options;
pub mod worktree_walk;
