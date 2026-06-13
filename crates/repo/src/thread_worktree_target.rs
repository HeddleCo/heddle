// SPDX-License-Identifier: Apache-2.0
//! Shared thread-worktree target shape validation.

use std::path::{Path, PathBuf};

/// Whether a target accepted by [`validate_thread_worktree_target`] already
/// existed or must be created by the caller.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ThreadWorktreeTargetDisposition {
    /// The target path does not exist yet.
    Absent,
    /// The target is an existing, empty, non-symlink directory.
    EmptyDirectory,
}

/// Why a thread-worktree target is not safe to materialize into.
#[derive(Debug)]
pub enum ThreadWorktreeTargetError {
    Symlink {
        path: PathBuf,
    },
    NotDirectory {
        path: PathBuf,
    },
    NotEmpty {
        path: PathBuf,
    },
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
}

impl std::fmt::Display for ThreadWorktreeTargetError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Symlink { path } => {
                write!(
                    f,
                    "thread worktree target '{}' cannot be a symlink",
                    path.display()
                )
            }
            Self::NotDirectory { path } => {
                write!(
                    f,
                    "thread worktree target '{}' must be a directory",
                    path.display()
                )
            }
            Self::NotEmpty { path } => {
                write!(
                    f,
                    "thread worktree target '{}' is not empty",
                    path.display()
                )
            }
            Self::Io { path, source } => {
                write!(
                    f,
                    "inspect thread worktree target '{}': {source}",
                    path.display()
                )
            }
        }
    }
}

impl std::error::Error for ThreadWorktreeTargetError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::Symlink { .. } | Self::NotDirectory { .. } | Self::NotEmpty { .. } => None,
        }
    }
}

/// Enforce the shared thread-create directory contract:
///
/// - absent path: accepted; caller creates it
/// - existing empty, non-symlink directory: accepted; caller adopts it
/// - symlink, non-directory, or non-empty directory: rejected
///
/// This intentionally mirrors the original CLI `validate_worktree_target`
/// leaf-shape check. Repo-specific reserved-path policy remains in the CLI
/// validator; this function owns only the filesystem shape contract that both
/// the CLI and `Repository::materialize_thread` must share.
pub fn validate_thread_worktree_target(
    path: &Path,
) -> std::result::Result<ThreadWorktreeTargetDisposition, ThreadWorktreeTargetError> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(_) => return Ok(ThreadWorktreeTargetDisposition::Absent),
    };

    if metadata.file_type().is_symlink() {
        return Err(ThreadWorktreeTargetError::Symlink {
            path: path.to_path_buf(),
        });
    }

    if !metadata.is_dir() {
        return Err(ThreadWorktreeTargetError::NotDirectory {
            path: path.to_path_buf(),
        });
    }

    let has_entry = std::fs::read_dir(path)
        .and_then(|mut entries| entries.next().transpose().map(|entry| entry.is_some()))
        .map_err(|source| ThreadWorktreeTargetError::Io {
            path: path.to_path_buf(),
            source,
        })?;
    if has_entry {
        return Err(ThreadWorktreeTargetError::NotEmpty {
            path: path.to_path_buf(),
        });
    }

    Ok(ThreadWorktreeTargetDisposition::EmptyDirectory)
}
