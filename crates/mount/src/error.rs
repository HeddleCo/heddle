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

    /// An entry with this name already exists (e.g. `O_CREAT|O_EXCL`
    /// against an existing file, or `mkdir` against an existing dir).
    /// Maps to `EEXIST` so userspace tooling that exercises atomic
    /// "create-or-skip" semantics (cargo's lockfile lease, git's
    /// `objects/<n>/<n>.tmp` placement) sees the conventional errno.
    #[error("already exists: {0}")]
    AlreadyExists(String),

    /// Tried to operate on a file as if it were a directory
    /// (e.g. `unlink` against a path that resolves to a directory).
    /// Maps to `EISDIR`.
    #[error("is a directory: {0}")]
    IsADirectory(String),

    /// Tried to `rmdir` a directory that still has visible children
    /// (across the captured tree + pending overlay). Maps to
    /// `ENOTEMPTY`.
    #[error("directory not empty: {0}")]
    NotEmpty(String),

    /// Invalid argument from the caller (e.g. a name containing
    /// `/`, `\0`, or `.`/`..`). Maps to `EINVAL`.
    #[error("invalid argument: {0}")]
    InvalidArgument(String),

    /// Errors bubbling up from the underlying object store / repo.
    #[error(transparent)]
    Store(#[from] HeddleError),
}

impl MountError {
    /// Translate this error into a libc errno suitable for handing
    /// back to FUSE. Only the platform shell uses this — keeping it
    /// here means platform code stays one-liners.
    ///
    /// `ESTALE` is POSIX-only; on Windows `libc` doesn't define it,
    /// so the Windows build uses the POSIX value (`116`) verbatim.
    /// The ProjFS shell translates this back into a Win32
    /// `ERROR_FILE_INVALID` further downstream — no caller looks at
    /// the raw integer except as a `match` discriminant.
    pub fn to_errno(&self) -> i32 {
        match self {
            MountError::NotFound(_) | MountError::UnknownThread(_) => libc::ENOENT,
            MountError::Stale(_) => stale_errno(),
            MountError::NotADirectory(_) => libc::ENOTDIR,
            MountError::ReadOnly => libc::EROFS,
            MountError::AlreadyExists(_) => libc::EEXIST,
            MountError::IsADirectory(_) => libc::EISDIR,
            MountError::NotEmpty(_) => libc::ENOTEMPTY,
            MountError::InvalidArgument(_) => libc::EINVAL,
            MountError::Store(HeddleError::NotFound(_))
            | MountError::Store(HeddleError::StateNotFound(_))
            | MountError::Store(HeddleError::MissingObject { .. }) => libc::ENOENT,
            MountError::Store(HeddleError::Io(io)) => io.raw_os_error().unwrap_or(libc::EIO),
            MountError::Store(_) => libc::EIO,
        }
    }
}

#[cfg(unix)]
#[inline]
fn stale_errno() -> i32 {
    libc::ESTALE
}

/// POSIX `ESTALE = 116` on Linux. Reuse the value verbatim on
/// Windows where the libc crate doesn't expose the constant. The
/// ProjFS errno→Win32 table in `projfs.rs` maps this back to
/// `ERROR_FILE_INVALID (1632)`.
#[cfg(windows)]
#[inline]
fn stale_errno() -> i32 {
    116
}