// SPDX-License-Identifier: Apache-2.0
//! Error types for the mount crate.
//!
//! Mount-side errors map cleanly to libc errno codes so the FUSE
//! shell can hand them back to the kernel without a translation
//! layer of its own. The mapping lives in [`MountError::to_errno`].

use objects::error::HeddleError;

/// Result alias used throughout the mount crate.
pub type Result<T> = std::result::Result<T, MountError>;

/// Errors surfaced by the content-addressed mount core.
#[derive(Debug, thiserror::Error)]
pub enum MountError {
    /// The requested path or node does not exist in the current state.
    #[error("not found: {0}")]
    NotFound(String),

    /// The node referenced by the caller is no longer valid (stale
    /// inode, invalidated cache, etc).
    #[error("stale node: {0}")]
    Stale(String),

    /// A path component traversed something that wasn't a directory.
    #[error("not a directory: {0}")]
    NotADirectory(String),

    /// The thread name does not resolve to a current state.
    #[error("thread {0} has no current state")]
    UnknownThread(String),

    /// Read-only filesystem (used while overlay-write is stubbed).
    #[error("read-only filesystem")]
    ReadOnly,

    /// Errors bubbling up from the underlying object store / repo.
    #[error(transparent)]
    Store(#[from] HeddleError),
}

impl MountError {
    /// Translate this error into a libc errno suitable for handing
    /// back to FUSE. Only the platform shell uses this — keeping it
    /// here means platform code stays one-liners.
    pub fn to_errno(&self) -> i32 {
        match self {
            MountError::NotFound(_) | MountError::UnknownThread(_) => libc::ENOENT,
            MountError::Stale(_) => libc::ESTALE,
            MountError::NotADirectory(_) => libc::ENOTDIR,
            MountError::ReadOnly => libc::EROFS,
            MountError::Store(HeddleError::NotFound(_))
            | MountError::Store(HeddleError::StateNotFound(_))
            | MountError::Store(HeddleError::MissingObject { .. }) => libc::ENOENT,
            MountError::Store(HeddleError::Io(io)) => io.raw_os_error().unwrap_or(libc::EIO),
            MountError::Store(_) => libc::EIO,
        }
    }
}