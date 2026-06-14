// SPDX-License-Identifier: Apache-2.0
//! Compare a working-copy path against its indexed Git OID + mode.
//!
//! Sley owns the Git-compatible worktree comparison rules: type changes,
//! executable-bit changes, symlink target hashing, gitlinks, clean filters, and
//! the racy-clean stat shortcut. Heddle keeps this tiny wrapper so existing repo
//! and CLI call sites can share one return type while the implementation stays
//! in the Git substrate.

use std::path::Path;

use objects::error::{HeddleError, Result};
pub use sley::IndexStatProbe;

/// State of a single index entry relative to the worktree, mirroring the three
/// outcomes `git status` cares about for a known-tracked path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GitWorktreeEntryState {
    /// Worktree matches the indexed blob.
    Clean,
    /// Worktree differs (content, mode, type, or submodule presence).
    Modified,
    /// Worktree path no longer exists.
    Deleted,
}

impl From<sley::WorktreeEntryState> for GitWorktreeEntryState {
    fn from(state: sley::WorktreeEntryState) -> Self {
        match state {
            sley::WorktreeEntryState::Clean => Self::Clean,
            sley::WorktreeEntryState::Modified => Self::Modified,
            sley::WorktreeEntryState::Deleted => Self::Deleted,
        }
    }
}

/// Compare the worktree file at `root/path` to its indexed `expected_oid` +
/// mode (a raw Git file mode such as `0o100644`).
pub fn git_worktree_entry_state(
    root: &Path,
    path: &str,
    expected_oid: sley::ObjectId,
    mode: u32,
    index_probe: Option<IndexStatProbe>,
) -> Result<GitWorktreeEntryState> {
    let repo = sley::Repository::discover(root).map_err(sley_error)?;
    let workdir = repo.workdir().unwrap_or_else(|| root.to_path_buf());
    let state = sley::plumbing::sley_worktree::worktree_entry_state_by_git_path(
        &workdir,
        repo.git_dir(),
        repo.object_format(),
        path.as_bytes(),
        &expected_oid,
        mode,
        index_probe.as_ref(),
    )
    .map_err(sley_error)?;
    Ok(state.into())
}

fn sley_error(error: sley::GitError) -> HeddleError {
    HeddleError::Config(error.to_string())
}
