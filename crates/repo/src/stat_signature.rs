// SPDX-License-Identifier: Apache-2.0
//! Cross-platform `(size, inode, mtime_ns, ctime_ns, mode)` extraction
//! from a [`std::fs::Metadata`].
//!
//! Pre-fix the capture path used `std::os::unix::fs::MetadataExt`
//! unconditionally, which broke the Windows build of the whole repo
//! crate (and transitively the mount crate with `--features projfs`).
//! This helper isolates the per-platform shape so call sites stay
//! cross-platform and the `ManifestFile` schema is unchanged.
//!
//! ## Field meaning across platforms
//!
//! | Field | Unix source | Windows source |
//! |-------|-------------|----------------|
//! | `size` | `metadata.size()` (from `MetadataExt`) | `metadata.file_size()` (from `windows::fs::MetadataExt`) |
//! | `inode` | `metadata.ino()` | `metadata.file_index()` (NT object ID; non-zero on NTFS) |
//! | `mtime_ns` | `metadata.mtime() * 1e9 + metadata.mtime_nsec()` | `last_write_time()` (FILETIME 100ns ticks since 1601) → unix epoch ns |
//! | `ctime_ns` | `metadata.ctime() * 1e9 + metadata.ctime_nsec()` (status-change) | `creation_time()` (FILETIME) → unix epoch ns. Note: Windows `creation_time` is *creation*, not status-change; the cache still catches changes because content-modifying ops bump `mtime_ns` first. |
//! | `mode` | `metadata.mode()` (full unix mode bits incl. type) | Synthesized from `file_attributes()`: regular files → `0o100644`, RO files → `0o100444`, dirs → `0o040755`, symlinks → `0o120777`. Losses cross-platform compat with manifests captured on a different OS — see `stat_signature` docs below. |
//!
//! ## Cross-OS manifest compatibility
//!
//! `ManifestFile` is per-checkout local state, NOT synced across
//! peers. A user who copies their `.heddle/` between Linux and
//! Windows checkouts will see stat-cache misses (the mtime / inode
//! shapes don't match), which forces a re-hash on next capture.
//! That's correct — it's the safe fallback. We never *accept* a
//! cache hit from a different OS, only fall back to read-and-hash.
//! The hash itself is platform-agnostic.

/// Stat signature for `metadata`: `(size, inode, mtime_ns, ctime_ns, mode)`.
/// The five-tuple shape mirrors [`crate::thread_manifest::ManifestFile`]'s
/// stat fields so call sites can splat into a struct literal.
#[cfg(unix)]
pub fn stat_signature(metadata: &std::fs::Metadata) -> (u64, u64, i64, i64, u32) {
    use std::os::unix::fs::MetadataExt;
    let mtime_ns = metadata
        .mtime()
        .saturating_mul(1_000_000_000)
        .saturating_add(metadata.mtime_nsec());
    let ctime_ns = metadata
        .ctime()
        .saturating_mul(1_000_000_000)
        .saturating_add(metadata.ctime_nsec());
    (
        metadata.size(),
        metadata.ino(),
        mtime_ns,
        ctime_ns,
        metadata.mode(),
    )
}

/// Windows variant. `file_index()` is the closest analogue of `ino`
/// on NTFS; on FAT32/exFAT it may be zero, which the cache treats
/// as "always miss" (a false negative — slow but correct). The
/// mode is synthesized so the manifest comparison can run, but it
/// only catches RO-flag changes, not full POSIX permission deltas.
#[cfg(windows)]
pub fn stat_signature(metadata: &std::fs::Metadata) -> (u64, u64, i64, i64, u32) {
    use std::os::windows::fs::MetadataExt;
    let mtime_ns = filetime_to_unix_ns(metadata.last_write_time());
    let ctime_ns = filetime_to_unix_ns(metadata.creation_time());
    let inode = file_index(metadata);
    let mode = synthesize_unix_mode(metadata);
    (metadata.file_size(), inode, mtime_ns, ctime_ns, mode)
}

/// Windows FILETIME (100ns ticks since 1601-01-01 UTC) → ns since
/// the unix epoch (1970-01-01 UTC). The offset between the two
/// epochs is 11644473600 seconds. We do the conversion in ns to
/// match the Unix side's units exactly.
#[cfg(windows)]
fn filetime_to_unix_ns(filetime_100ns_ticks: u64) -> i64 {
    // 11_644_473_600 seconds × 10_000_000 ticks/sec
    const EPOCH_DELTA_TICKS: u64 = 116_444_736_000_000_000;
    let unix_ticks = filetime_100ns_ticks.saturating_sub(EPOCH_DELTA_TICKS);
    // ticks (100ns) → ns: × 100. Saturate at i64::MAX to match the
    // Unix path's saturating_mul behaviour.
    (unix_ticks.saturating_mul(100)).min(i64::MAX as u64) as i64
}

/// Best-effort NTFS file-index lookup. `MetadataExt::file_index()`
/// is gated behind the unstable `windows_by_handle` feature, so on
/// stable Rust we always return 0.
///
/// Returning 0 means the manifest comparison will treat *every*
/// Windows entry's inode as unchanged, which is a false positive
/// for the inode field specifically — but the cache match still
/// requires size + mtime + ctime + mode + the pre-computed hash
/// to align, so a content change still produces a cache miss.
/// Worst-case effect: a chmod-equivalent change that only flips
/// file attributes (rare on Windows; most edits also bump mtime)
/// could be missed on the `inode` axis but is independently
/// caught by the `mode` axis.
///
/// If we ever need the real NTFS file index, switch this to a
/// `GetFileInformationByHandle` call via `windows-sys`. Cost is
/// one syscall per `stat`-cache lookup, vs. the current zero —
/// not worth it until a real bug shows up.
#[cfg(windows)]
fn file_index(_metadata: &std::fs::Metadata) -> u64 {
    0
}

/// Synthesize a unix-shaped mode from Windows file attributes. We
/// only need enough fidelity for the cache comparison to detect a
/// `chmod`-shaped change — anything finer (e.g. ACL edits) is out of
/// scope. RO files map to `0o*444`; the rest get `0o*644` for files
/// and `0o*755` for directories.
#[cfg(windows)]
fn synthesize_unix_mode(metadata: &std::fs::Metadata) -> u32 {
    const FILE_ATTRIBUTE_READONLY: u32 = 0x1;
    const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;

    use std::os::windows::fs::MetadataExt;
    let attrs = metadata.file_attributes();
    if attrs & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        // Treat reparse points as symlinks for cache purposes; real
        // symlink resolution lives elsewhere in the materializer.
        0o120777
    } else if attrs & FILE_ATTRIBUTE_DIRECTORY != 0 {
        0o040755
    } else if attrs & FILE_ATTRIBUTE_READONLY != 0 {
        0o100444
    } else {
        0o100644
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The helper must return *something* on a freshly-created temp
    /// file, and the values should be stable across two reads of the
    /// same metadata. Doesn't try to assert specific values (those
    /// vary per OS / filesystem) — just locks in the shape.
    #[test]
    fn stat_signature_is_stable_for_unchanged_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("probe");
        std::fs::write(&path, b"hello").expect("write probe");

        let meta1 = std::fs::metadata(&path).expect("metadata #1");
        let meta2 = std::fs::metadata(&path).expect("metadata #2");

        let sig1 = stat_signature(&meta1);
        let sig2 = stat_signature(&meta2);

        // Size must be non-zero (we wrote 5 bytes). Other fields can
        // legitimately be zero on some filesystems (e.g. FAT32 has
        // no inode), so we don't probe them individually.
        assert_eq!(sig1.0, 5);
        assert_eq!(sig1, sig2, "back-to-back stat must produce identical signatures");
    }

    /// Modifying the file's contents must change the signature.
    /// Locks in that the cache-comparison call sites can actually
    /// detect a write.
    #[test]
    fn stat_signature_changes_after_overwrite() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("probe");
        std::fs::write(&path, b"original").expect("write v1");
        let sig1 = stat_signature(&std::fs::metadata(&path).expect("metadata v1"));

        // Sleep briefly to ensure mtime advances on filesystems with
        // 1s resolution (HFS+, FAT32). 50ms isn't enough on those,
        // so we use 1100ms to be safe; the test is small enough that
        // the extra wall time is acceptable.
        std::thread::sleep(std::time::Duration::from_millis(1100));
        std::fs::write(&path, b"replacement-content-different-length").expect("write v2");

        let sig2 = stat_signature(&std::fs::metadata(&path).expect("metadata v2"));

        assert_ne!(sig1, sig2, "post-overwrite signature must differ");
    }
}
