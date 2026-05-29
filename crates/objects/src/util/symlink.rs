// SPDX-License-Identifier: Apache-2.0
//! Canonical symlink-target → blob-bytes conversion.
//!
//! Git stores a symlink's target as the raw bytes of the link, which on Unix
//! is an arbitrary byte sequence and need not be valid UTF-8. Every site that
//! turns a symlink target into a blob (capture, status hashing, diff/rename
//! similarity, patch generation) must use these raw bytes — a `to_string_lossy`
//! conversion replaces invalid bytes with U+FFFD, producing a hash that never
//! matches git's and a patch that recreates the link with a corrupted target.
//! This is the single source of truth so the platform byte extraction is not
//! forked across crates.

use std::path::Path;

/// The bytes git would store as a symlink's blob: the raw OS bytes of the
/// link target. On non-Unix platforms the target is text and lossy conversion
/// is acceptable because the filesystem does not preserve arbitrary bytes.
pub fn symlink_target_bytes(target: &Path) -> Vec<u8> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        target.as_os_str().as_bytes().to_vec()
    }
    #[cfg(not(unix))]
    {
        target.to_string_lossy().as_bytes().to_vec()
    }
}
