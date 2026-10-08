// SPDX-License-Identifier: Apache-2.0
//! Worktree paths Heddle never writes, and never records (heddle#2028).
//!
//! A `.git` alias at any depth, a `.heddle` alias at the root, or a
//! `.gitmodules` alias as a symlink would write into a repository's metadata
//! (see `objects::object::reserved_tree_entry_name`). Git import refuses
//! them, but a repository captured before heddle#2028 can already hold one:
//! a vendored clone's `.git`, or a submodule's gitfile. Such a repository has
//! to stay usable, so whole-tree writers (checkout, incremental apply, merge
//! output, revert, thread moves, conflict markers) skip the path with a
//! warning instead of failing the whole command. Skipping is as safe as
//! refusing: nothing is written or removed there. A removal is skipped too,
//! because what sits on disk at such a path is the nested repository's own
//! metadata, not Heddle's content.
//!
//! Capture and status skip them the same way, warning once per path.

use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
};

use objects::{object::reserved_path_component, worktree::reserved_worktree_write};
use tracing::warn;

/// Whether a whole-tree write or removal at the worktree-relative
/// `rel_path` must be skipped; `symlink` says the entry is a symlink. Logs a
/// warning naming the path when it is.
pub fn skip_reserved_worktree_write(rel_path: &Path, symlink: bool) -> bool {
    let Some(reason) = reserved_worktree_write(rel_path, symlink) else {
        return false;
    };
    warn!(
        path = %rel_path.display(),
        "skipping '{}' in the worktree: {reason}",
        rel_path.display()
    );
    true
}

/// Warn, once per process and path, that capture and status skip the
/// worktree-relative `path`. Stays quiet for the repository's own root
/// `.git` and `.heddle`.
pub(crate) fn note_skipped_reserved_path(path: &Path) {
    let Some(reason) = reserved_path_component(path.as_os_str().as_encoded_bytes(), false) else {
        return;
    };
    if reason.is_own_metadata() {
        return;
    }
    static WARNED: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();
    let first = WARNED
        .get_or_init(Mutex::default)
        .lock()
        .map_or(true, |mut warned| warned.insert(path.to_path_buf()));
    if first {
        warn!(
            path = %path.display(),
            "capture and status skip '{}': {reason}",
            path.display()
        );
    }
}
