// SPDX-License-Identifier: Apache-2.0
//! Cross-platform `(size, inode, mtime_ns, ctime_ns, mode)` extraction
//! for a file on disk.
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
//! | `inode` | `metadata.ino()` | `GetFileInformationByHandleEx(FileIdInfo)` (NTFS 128-bit FileId + volume serial, folded to `u64`); falls back to `GetFileInformationByHandle` (`nFileIndexHigh/Low`) on older Windows, and to a hash of `(size, mtime, path)` if both fail. |
//! | `mtime_ns` | `metadata.mtime() * 1e9 + metadata.mtime_nsec()` | `last_write_time()` (FILETIME 100ns ticks since 1601) → unix epoch ns |
//! | `ctime_ns` | `metadata.ctime() * 1e9 + metadata.ctime_nsec()` (status-change) | `creation_time()` (FILETIME) → unix epoch ns. Note: Windows `creation_time` is *creation*, not status-change; the cache still catches changes because content-modifying ops bump `mtime_ns` first. |
//! | `mode` | `metadata.mode()` (full unix mode bits incl. type) | Synthesized from `file_attributes()`: regular files → `0o100644`, RO files → `0o100444`, dirs → `0o040755`, symlinks → `0o120777`. Loses cross-platform compat with manifests captured on a different OS — see `stat_signature` docs below. |
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

use std::path::Path;

/// Conservatively treat timestamps in the filesystem clock tick surrounding a
/// snapshot preparation as racy. Two seconds covers common one-second and FAT
/// two-second timestamp granularities without penalizing stable older files.
const RACY_TIMESTAMP_WINDOW_NS: i64 = 2_000_000_000;

pub(crate) fn racy_timestamp_cutoff() -> i64 {
    let now_ns = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos().min(i64::MAX as u128) as i64)
        .unwrap_or(0);
    now_ns.saturating_sub(RACY_TIMESTAMP_WINDOW_NS)
}

pub(crate) fn is_racy_timestamp(mtime_ns: i64, ctime_ns: i64, cutoff_ns: i64) -> bool {
    mtime_ns >= cutoff_ns || ctime_ns >= cutoff_ns
}

/// Stat signature for `metadata`: `(size, inode, mtime_ns, ctime_ns, mode)`.
/// The five-tuple shape mirrors [`crate::thread_manifest::ManifestFile`]'s
/// stat fields so call sites can splat into a struct literal.
///
/// `path` is the on-disk path the metadata was read from. On Unix
/// it's unused (the inode is read straight out of `Metadata`); on
/// Windows it's reopened with `FILE_READ_ATTRIBUTES | FILE_FLAG_OPEN_REPARSE_POINT`
/// so [`GetFileInformationByHandleEx`] can return the real NTFS
/// file identity (see the module-level docs for the fallback chain).
#[cfg(unix)]
pub fn stat_signature(_path: &Path, metadata: &std::fs::Metadata) -> (u64, u64, i64, i64, u32) {
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

/// Windows variant. The `inode` slot is filled by [`file_index`], which
/// opens a read-attributes handle and asks NTFS for the real per-file
/// identity. Constant-0 inodes (the pre-fix behaviour) silently broke
/// the stat-cache's ability to detect replace-in-place edits where the
/// size/mtime/ctime/mode collapsed to the same values — the cache hit
/// would falsely reuse the old blob hash and the capture would skip
/// the changed content. See the module-level docs for the fallback
/// chain when the handle can't be opened.
#[cfg(windows)]
pub fn stat_signature(path: &Path, metadata: &std::fs::Metadata) -> (u64, u64, i64, i64, u32) {
    use std::os::windows::fs::MetadataExt;
    let mtime_ns = filetime_to_unix_ns(metadata.last_write_time());
    let ctime_ns = filetime_to_unix_ns(metadata.creation_time());
    let inode = file_index(path, metadata);
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

/// Real NTFS file-identity lookup for the `inode` slot of the
/// stat signature.
///
/// Strategy (first one that succeeds wins):
///
/// 1. Open the file with `FILE_READ_ATTRIBUTES` + share-all + reparse-don't-follow
///    and call [`GetFileInformationByHandleEx`] with `FileIdInfo`. That
///    returns a `FILE_ID_INFO { VolumeSerialNumber, FileId[16] }` —
///    the 128-bit ID that works for both NTFS (high 64 bits zero) and
///    ReFS (real 128-bit). Fold the 16 bytes XOR the volume serial
///    into a `u64`.
/// 2. If `GetFileInformationByHandleEx` fails (older Windows / unusual
///    filesystem), fall back to legacy [`GetFileInformationByHandle`]
///    which returns `nFileIndexHigh:nFileIndexLow` (the classic 64-bit
///    NTFS file index) plus the volume serial; combine them the same
///    way.
/// 3. If we can't even open a handle (sharing violation, ACL deny,
///    file disappeared between `metadata` and here), fall back to a
///    SipHash of `(size, last_write_time_100ns, path_bytes)`. Weaker
///    than a real file ID — two files with the same size/mtime/path
///    won't happen on the same volume, and the path mixes in enough
///    entropy that distinct files almost never collide — but strictly
///    better than the pre-fix constant 0, which made every file alias
///    to every other file on the `inode` axis.
///
/// Cost: one extra `CreateFileW` + `GetFileInformationByHandleEx` +
/// `CloseHandle` per stat-cache lookup on Windows. Roughly two
/// additional syscalls per file probe. The cache-hit path saves a
/// full blob read so the extra syscalls are net-positive on any file
/// over a few KB.
#[cfg(windows)]
fn file_index(path: &Path, metadata: &std::fs::Metadata) -> u64 {
    if let Some(id) = file_index_via_handle(path) {
        return id;
    }
    file_index_fallback_hash(path, metadata)
}

#[cfg(windows)]
fn file_index_via_handle(path: &Path) -> Option<u64> {
    use std::os::windows::ffi::OsStrExt;

    use windows_sys::Win32::{
        Foundation::{CloseHandle, INVALID_HANDLE_VALUE},
        Storage::FileSystem::{
            BY_HANDLE_FILE_INFORMATION, CreateFileW, FILE_FLAG_BACKUP_SEMANTICS,
            FILE_FLAG_OPEN_REPARSE_POINT, FILE_ID_INFO, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE,
            FILE_SHARE_READ, FILE_SHARE_WRITE, FileIdInfo, GetFileInformationByHandle,
            GetFileInformationByHandleEx, OPEN_EXISTING,
        },
    };

    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();

    // SAFETY: `wide` is a NUL-terminated UTF-16 buffer owned by us
    // for the duration of the call. `FILE_FLAG_BACKUP_SEMANTICS`
    // lets the same call open both files and directories.
    // `FILE_FLAG_OPEN_REPARSE_POINT` is critical: without it, an
    // open against a symlink would silently follow into the target,
    // which would tag the symlink with the target's inode and make
    // the cache mistake "the link still points at the same file" for
    // "the link itself is unchanged".
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            FILE_READ_ATTRIBUTES,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            std::ptr::null_mut(),
        )
    };
    if handle.is_null() || handle == INVALID_HANDLE_VALUE {
        return None;
    }

    // Prefer the modern API: 128-bit FileId works on ReFS, and the
    // structure includes the volume serial in one syscall.
    let mut info: FILE_ID_INFO = unsafe { std::mem::zeroed() };
    let ex_ok = unsafe {
        GetFileInformationByHandleEx(
            handle,
            FileIdInfo,
            (&mut info as *mut FILE_ID_INFO).cast(),
            std::mem::size_of::<FILE_ID_INFO>() as u32,
        )
    };
    if ex_ok != 0 {
        let id = fold_file_id_info(&info);
        unsafe {
            CloseHandle(handle);
        }
        return Some(id);
    }

    // Fall back to the legacy 64-bit file index.
    let mut legacy: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    let legacy_ok = unsafe { GetFileInformationByHandle(handle, &mut legacy) };
    unsafe {
        CloseHandle(handle);
    }
    if legacy_ok == 0 {
        return None;
    }
    let file_index = ((legacy.nFileIndexHigh as u64) << 32) | (legacy.nFileIndexLow as u64);
    // XOR the volume serial in so two distinct volumes that happen
    // to assign the same file index don't collide.
    Some(file_index ^ (legacy.dwVolumeSerialNumber as u64))
}

#[cfg(windows)]
fn fold_file_id_info(info: &windows_sys::Win32::Storage::FileSystem::FILE_ID_INFO) -> u64 {
    // `FileId` is a 16-byte identifier. On NTFS the high 8 bytes are
    // zero (it's really a 64-bit MFT entry); on ReFS the full 128
    // bits matter. Fold as two u64s XORed together, then mix in the
    // volume serial so the same FileId on two different volumes
    // doesn't alias.
    let bytes = info.FileId.Identifier;
    let low = u64::from_le_bytes([
        bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
    ]);
    let high = u64::from_le_bytes([
        bytes[8], bytes[9], bytes[10], bytes[11], bytes[12], bytes[13], bytes[14], bytes[15],
    ]);
    (low ^ high) ^ info.VolumeSerialNumber
}

/// Last-resort fingerprint when we couldn't open a handle to ask
/// NTFS directly. SipHash of `(size, last_write_time_100ns,
/// path_bytes)`. Stable across two reads of the same file at the
/// same path. Two genuinely different files at the same path with
/// the same size and mtime would collide — that's the scenario the
/// fall-through accepts; it's the same scenario the constant-0
/// pre-fix accepted, except we now ALSO mix in size + mtime + path
/// instead of pretending every file has the same identity.
#[cfg(windows)]
fn file_index_fallback_hash(path: &Path, metadata: &std::fs::Metadata) -> u64 {
    use std::{
        hash::{Hash, Hasher},
        os::windows::fs::MetadataExt,
    };

    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    metadata.file_size().hash(&mut hasher);
    metadata.last_write_time().hash(&mut hasher);
    path.hash(&mut hasher);
    hasher.finish()
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

    #[test]
    fn racy_timestamp_guard_covers_coarse_filesystem_ticks() {
        let cutoff = 10_000_000_000;
        assert!(is_racy_timestamp(cutoff, 0, cutoff));
        assert!(is_racy_timestamp(0, cutoff, cutoff));
        assert!(!is_racy_timestamp(cutoff - 1, cutoff - 1, cutoff));
    }

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

        let sig1 = stat_signature(&path, &meta1);
        let sig2 = stat_signature(&path, &meta2);

        // Size must be non-zero (we wrote 5 bytes). Other fields can
        // legitimately be zero on some filesystems (e.g. FAT32 has
        // no inode), so we don't probe them individually.
        assert_eq!(sig1.0, 5);
        assert_eq!(
            sig1, sig2,
            "back-to-back stat must produce identical signatures"
        );
    }

    /// Modifying the file's contents must change the signature.
    /// Locks in that the cache-comparison call sites can actually
    /// detect a write.
    #[test]
    fn stat_signature_changes_after_overwrite() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("probe");
        std::fs::write(&path, b"original").expect("write v1");
        let sig1 = stat_signature(&path, &std::fs::metadata(&path).expect("metadata v1"));

        // Sleep briefly to ensure mtime advances on filesystems with
        // 1s resolution (HFS+, FAT32). 50ms isn't enough on those,
        // so we use 1100ms to be safe; the test is small enough that
        // the extra wall time is acceptable.
        std::thread::sleep(std::time::Duration::from_millis(1100));
        std::fs::write(&path, b"replacement-content-different-length").expect("write v2");

        let sig2 = stat_signature(&path, &std::fs::metadata(&path).expect("metadata v2"));

        assert_ne!(sig1, sig2, "post-overwrite signature must differ");
    }

    /// Two distinct files in the same directory must produce
    /// distinct stat signatures. Pre-fix the Windows `inode` slot
    /// was a constant 0, so two newly-created same-size files
    /// written within the same mtime tick could end up with
    /// byte-identical signatures — exactly the silent-corruption
    /// hazard `ManifestFile::matches` relies on the inode to
    /// disambiguate.
    #[test]
    fn stat_signature_differs_between_distinct_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        let a = dir.path().join("a");
        let b = dir.path().join("b");
        // Same-size content makes the size axis useless for
        // disambiguation; if mtime collapses too (cheap on filesystems
        // with second-resolution mtime), the inode is the only thing
        // standing between us and a false cache hit.
        std::fs::write(&a, b"same-bytes").expect("write a");
        std::fs::write(&b, b"same-bytes").expect("write b");

        let sig_a = stat_signature(&a, &std::fs::metadata(&a).expect("metadata a"));
        let sig_b = stat_signature(&b, &std::fs::metadata(&b).expect("metadata b"));

        assert_ne!(
            sig_a, sig_b,
            "two distinct files must have distinct stat signatures",
        );
    }

    /// Windows-specific: the inode slot must be non-zero for an
    /// ordinary file on the test filesystem (TMP is typically
    /// NTFS). Pre-fix this returned a constant 0; if a future
    /// refactor accidentally reintroduces that, this test fails
    /// loudly instead of letting the silent-corruption regress.
    #[cfg(windows)]
    #[test]
    fn windows_inode_is_non_zero_for_regular_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("probe");
        std::fs::write(&path, b"probe").expect("write probe");
        let sig = stat_signature(&path, &std::fs::metadata(&path).expect("metadata"));
        assert_ne!(
            sig.1, 0,
            "Windows file_index must be non-zero on NTFS; got {sig:?}",
        );
    }
}
