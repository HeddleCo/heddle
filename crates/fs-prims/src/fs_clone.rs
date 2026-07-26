// SPDX-License-Identifier: Apache-2.0
//! Filesystem-level copy-on-write helpers.
//!
//! Heddle's worktree materializer needs the storage win of pointing
//! N worktrees at the same blob bytes (so checking out the same state
//! to many sibling worktrees costs ~1× disk, not N×) **without** the
//! mutation hazard that hardlinks bring. With hardlinks, an in-place
//! write — `chmod +w file && echo new > file`, `O_TRUNC`, etc. —
//! mutates the shared inode, corrupting every other worktree that
//! points at the same blob.
//!
//! Filesystem reflinks (a.k.a. CoW clones) solve this: the destination
//! starts out sharing physical blocks with the source, but the first
//! write to either side automatically forks the underlying allocation.
//! The OS guarantees isolation even if an agent strips the read-only
//! bit and overwrites the file in place.
//!
//! Platform support:
//! - **macOS / APFS:** `clonefile(2)` from `<sys/clonefile.h>`. True CoW.
//! - **Linux / btrfs / XFS-with-reflinks / ZFS:** `ioctl(dest_fd, FICLONE, src_fd)`.
//! - **Anywhere else** (or when reflink isn't supported by the
//!   underlying filesystem): caller falls back to a real copy.
//!
//! The core [`try_reflink`] returns a [`ReflinkOutcome`] so the caller
//! can tell three genuinely-different situations apart: a successful
//! clone, a "this filesystem can't reflink" verdict (batch-wide signal
//! to stop trying), and a "the source vanished from under us" race
//! (a per-blob fallback that must NOT poison the batch). Overloading the
//! last two — as a bare `Ok(false)` did — makes one concurrently-pruned
//! loose mirror needlessly disable reflinks for every remaining blob.

use std::{fs, io, path::Path};

/// The three outcomes of a reflink attempt, kept distinct so callers
/// don't conflate "filesystem can't reflink" (a batch-wide property)
/// with "this one source vanished mid-flight" (a per-blob race).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReflinkOutcome {
    /// CoW clone succeeded; `dest` now exists, sharing physical blocks
    /// with `source` until either side is written.
    Cloned,
    /// The kernel reported reflinks aren't supported for this
    /// filesystem / src+dst pair (`EXDEV`/`EOPNOTSUPP`/`ENOTSUP`/
    /// `ENOSYS`/`EINVAL`). This is a property of the destination
    /// filesystem, so a caller materializing a batch MAY disable
    /// reflinks for the rest of it and fall straight to copy/write.
    Unsupported,
    /// The `source` was gone by the time we looked (concurrent prune /
    /// torn NoSync promote). Reflink is only an optimization, so the
    /// caller should degrade to a real copy / bytes-write for THIS blob
    /// only — and crucially keep reflinks enabled for the rest of the
    /// batch, since the filesystem itself is perfectly capable. A blob
    /// that is genuinely absent (not just unreflinkable) still surfaces
    /// downstream when the copy/write fallback can't find its bytes.
    SourceVanished,
}

/// Try a filesystem-level reflink (copy-on-write clone) from `source`
/// to `dest`. On success the destination has its own inode and shares
/// physical blocks with the source until either side is modified.
///
/// On a successful reflink: returns `Ok(ReflinkOutcome::Cloned)`. The
/// destination file has been created with the kernel's choice of
/// permissions (typically the source's). Callers should
/// `set_permissions` afterwards if they need a specific mode.
///
/// On a "filesystem doesn't support reflinks" verdict (`EXDEV`,
/// `EOPNOTSUPP`, `ENOTSUP`, `ENOSYS`, `EINVAL` from the ioctl form):
/// returns `Ok(ReflinkOutcome::Unsupported)`. The caller should fall
/// back to `fs::copy` and may skip future reflink attempts on this
/// filesystem.
///
/// When the `source` is gone (missing at the pre-check, or `ENOENT`
/// from the syscall in the TOCTOU window after it): returns
/// `Ok(ReflinkOutcome::SourceVanished)`. The caller should fall back
/// to a copy/bytes-write for this blob only and keep reflinks enabled
/// for the rest of the batch — a vanished mirror says nothing about
/// the filesystem's reflink capability.
///
/// On any other I/O error: returns `Err`.
///
/// `dest` must not already exist on macOS (`clonefile` requires a
/// nonexistent destination). On Linux `FICLONE` requires the dest fd
/// be opened for writing on a regular file, which we create with
/// `O_CREAT | O_WRONLY | O_TRUNC`.
pub fn try_reflink(source: &Path, dest: &Path) -> io::Result<ReflinkOutcome> {
    // Never hand `clonefile`/`FICLONE` a source that isn't there: a missing
    // source is reported as ENOENT, which `reflink_unsupported` deliberately
    // does NOT swallow (ENOENT is a genuinely-missing file, not "reflink
    // unsupported"), so it would hard-error. Reflink is only an optimization —
    // a vanished loose mirror (concurrent prune / torn promote) must degrade to
    // the caller's copy/bytes-write fallback, not crash. This is reported as
    // `SourceVanished` (NOT `Unsupported`) so a single pruned blob doesn't
    // disable reflinks for the whole batch. This guard is what stopped `heddle
    // start` from failing on macOS/APFS with `conflict: No such file or
    // directory` (heddle#571). A genuinely-missing blob still errors loudly
    // downstream — `get_blob` returns `NotFound` with the hash when the copy
    // fallback also can't find the bytes.
    if !source.exists() {
        return Ok(ReflinkOutcome::SourceVanished);
    }
    #[cfg(target_os = "macos")]
    {
        try_clonefile_macos(source, dest)
    }
    #[cfg(target_os = "linux")]
    {
        try_ficlone_linux(source, dest)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = (source, dest);
        Ok(ReflinkOutcome::Unsupported)
    }
}

/// Reflink if possible, otherwise fall back to a real copy. Returns
/// the same `Ok(true)/Ok(false)` discriminator as [`try_reflink`] —
/// `true` when the OS gave us a CoW clone, `false` when we paid the
/// full copy cost. Either way, on `Ok` the destination exists and has
/// the source's bytes.
///
/// The destination's permission bits are not normalized here. Callers
/// that need a specific mode (`0o644`, `0o755`) should call
/// `fs::set_permissions` after a successful return.
pub fn clonefile_or_copy(source: &Path, dest: &Path) -> io::Result<bool> {
    // `clonefile`/FICLONE require dest not to exist; remove any stale
    // entry first. Ignored if dest doesn't exist.
    let _ = fs::remove_file(dest);
    if matches!(try_reflink(source, dest)?, ReflinkOutcome::Cloned) {
        return Ok(true);
    }
    fs::copy(source, dest)?;
    Ok(false)
}

#[cfg(target_os = "macos")]
fn try_clonefile_macos(source: &Path, dest: &Path) -> io::Result<ReflinkOutcome> {
    use std::{ffi::CString, os::unix::ffi::OsStrExt};

    // SAFETY: linking the system `clonefile(2)` symbol. Signature
    // matches `<sys/clonefile.h>`:
    //   int clonefile(const char *src, const char *dst, uint32_t flags);
    unsafe extern "C" {
        fn clonefile(src: *const libc::c_char, dst: *const libc::c_char, flags: u32)
        -> libc::c_int;
    }

    let src_c = CString::new(source.as_os_str().as_bytes()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "source path contains interior NUL",
        )
    })?;
    let dst_c = CString::new(dest.as_os_str().as_bytes()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "destination path contains interior NUL",
        )
    })?;

    // SAFETY: both pointers are NUL-terminated C strings owned by
    // the local CStrings; flags=0 requests the default behavior
    // (clone metadata + data, follow no symlinks on the source).
    let rc = unsafe { clonefile(src_c.as_ptr(), dst_c.as_ptr(), 0) };
    if rc == 0 {
        return Ok(ReflinkOutcome::Cloned);
    }

    let err = io::Error::last_os_error();
    classify_clone_err(source, err)
}

#[cfg(target_os = "linux")]
fn try_ficlone_linux(source: &Path, dest: &Path) -> io::Result<ReflinkOutcome> {
    use std::{fs::OpenOptions, os::unix::io::AsRawFd};

    // FICLONE = _IOW(0x94, 9, int) on Linux. The kernel header
    // `<linux/fs.h>` (and `<linux/fs.h>` UAPI) define this as
    // 0x40049409 = (1 << 30) | (4 << 16) | (0x94 << 8) | 9
    // i.e. _IOC_WRITE | sizeof(int) | type=0x94 | nr=9.
    const FICLONE: libc::c_ulong = 0x4004_9409;

    // Opening the source can race a concurrent prune: the pre-check in
    // `try_reflink` saw it, but it can vanish before this open. Map that
    // to `SourceVanished` so the caller degrades per-blob rather than
    // disabling reflinks for the batch (or hard-erroring).
    let src = match OpenOptions::new().read(true).open(source) {
        Ok(f) => f,
        Err(err) => return classify_clone_err(source, err),
    };
    let dst = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(dest)?;

    // SAFETY: ioctl with two valid fds; FICLONE expects an `int` fd
    // as the third arg.
    let rc = unsafe { libc::ioctl(dst.as_raw_fd(), FICLONE, src.as_raw_fd()) };
    if rc == 0 {
        return Ok(ReflinkOutcome::Cloned);
    }

    let err = io::Error::last_os_error();
    // Clean up the empty dest we just created so the caller's
    // `fs::copy` fallback starts from a known state.
    drop(dst);
    let _ = fs::remove_file(dest);
    classify_clone_err(source, err)
}

/// Classify a clonefile/FICLONE (or source-open) failure into the
/// caller-meaningful [`ReflinkOutcome`] or a genuine error.
///
/// * `Unsupported` — the filesystem (or src/dst pair) can't reflink
///   (`reflink_unsupported`). A batch-wide property.
/// * `SourceVanished` — the failure is `ENOENT` and the source is in
///   fact gone now (concurrent prune / torn promote in the TOCTOU
///   window after the pre-check). A per-blob race; reflinks stay viable
///   for the rest of the batch. An `ENOENT` whose source still exists
///   (e.g. a missing dest parent) is NOT swallowed here — it surfaces
///   as an `Err` for the caller to attribute correctly.
/// * `Err` — anything else; the caller should surface it.
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn classify_clone_err(source: &Path, err: io::Error) -> io::Result<ReflinkOutcome> {
    if reflink_unsupported(&err) {
        Ok(ReflinkOutcome::Unsupported)
    } else if err.kind() == io::ErrorKind::NotFound && !source.exists() {
        Ok(ReflinkOutcome::SourceVanished)
    } else {
        Err(err)
    }
}

/// Decide whether a clonefile/FICLONE error means "this filesystem
/// (or this src/dst pair) won't ever reflink" vs a transient or
/// caller-bug failure that we should surface.
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn reflink_unsupported(err: &io::Error) -> bool {
    let Some(code) = err.raw_os_error() else {
        return false;
    };
    // EXDEV: cross-device — the two paths live on different filesystems.
    // EOPNOTSUPP / ENOTSUP: filesystem doesn't implement reflinks
    //    (e.g. ext4 on Linux, HFS+ on macOS). On Linux these two are
    //    aliases (both = 95) so listing both makes one branch
    //    unreachable; on macOS they're distinct (102 vs 45), so we need
    //    both to be matched. `#[allow(unreachable_patterns)]` keeps the
    //    portable spelling without a `cfg`-split.
    // ENOSYS: kernel too old to know the syscall.
    // EINVAL: FICLONE returns this when the src/dst aren't on the same
    //    filesystem on some kernels, or when the filesystem is mounted
    //    without reflink support.
    #[allow(unreachable_patterns)]
    let is_unsupported = matches!(
        code,
        libc::EXDEV | libc::EOPNOTSUPP | libc::ENOTSUP | libc::ENOSYS | libc::EINVAL
    );
    is_unsupported
}

/// Test whether the filesystem at `parent_dir` supports reflinks by
/// trying one against a temp source/dest pair. Returns `true` on
/// success. Useful for tests that want to soft-skip on filesystems
/// without CoW support, and for any caller that wants a runtime
/// capability check before asserting on reflink-specific properties.
pub fn filesystem_supports_reflink(parent_dir: &Path) -> bool {
    use std::io::Write;

    let src = parent_dir.join(".heddle-reflink-probe-src");
    let dst = parent_dir.join(".heddle-reflink-probe-dst");
    let _ = fs::remove_file(&src);
    let _ = fs::remove_file(&dst);

    let mut f = match fs::File::create(&src) {
        Ok(f) => f,
        Err(_) => return false,
    };
    if f.write_all(b"reflink-probe").is_err() {
        let _ = fs::remove_file(&src);
        return false;
    }
    drop(f);

    let supported = matches!(try_reflink(&src, &dst), Ok(ReflinkOutcome::Cloned));
    let _ = fs::remove_file(&src);
    let _ = fs::remove_file(&dst);
    supported
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    /// heddle#571 (Bug 2): reflink must be gated on the source existing. A
    /// vanished loose mirror (concurrent prune / torn promote) must degrade to
    /// the caller's copy/bytes-write fallback, NOT hard-error with the ENOENT
    /// that `clonefile` raises on macOS (and that `reflink_unsupported`
    /// correctly refuses to swallow). It must report `SourceVanished` —
    /// distinct from `Unsupported` — so one pruned blob doesn't disable
    /// reflinks for the whole batch (heddle#571 r3). Verifiable on Linux: no
    /// syscall is issued.
    #[test]
    fn try_reflink_missing_source_reports_vanished_not_unsupported() {
        let temp = TempDir::new().unwrap();
        let src = temp.path().join("does-not-exist.txt");
        let dst = temp.path().join("dst.txt");
        assert!(!src.exists());

        let result = try_reflink(&src, &dst);
        assert!(
            matches!(result, Ok(ReflinkOutcome::SourceVanished)),
            "a missing reflink source must report SourceVanished (per-blob fallback, \
             NOT the batch-wide Unsupported), got {result:?}"
        );
        assert!(
            !dst.exists(),
            "no destination should be created when the source is missing"
        );
    }

    #[test]
    fn clonefile_or_copy_creates_destination_with_source_bytes() {
        let temp = TempDir::new().unwrap();
        let src = temp.path().join("src.txt");
        let dst = temp.path().join("dst.txt");
        fs::write(&src, b"hello reflink").unwrap();

        let _ = clonefile_or_copy(&src, &dst).unwrap();
        assert_eq!(fs::read(&dst).unwrap(), b"hello reflink");
    }

    #[test]
    fn clonefile_or_copy_overwrites_existing_destination() {
        let temp = TempDir::new().unwrap();
        let src = temp.path().join("src.txt");
        let dst = temp.path().join("dst.txt");
        fs::write(&src, b"new content").unwrap();
        fs::write(&dst, b"old content").unwrap();

        let _ = clonefile_or_copy(&src, &dst).unwrap();
        assert_eq!(fs::read(&dst).unwrap(), b"new content");
    }

    /// Core isolation property: writing to the cloned destination
    /// must not change the source's bytes. With a real CoW clone the
    /// kernel forks blocks on first write; with the `fs::copy`
    /// fallback the dest is a separate file from the start. Either
    /// way the source must be untouched.
    #[test]
    fn writing_to_destination_does_not_mutate_source() {
        let temp = TempDir::new().unwrap();
        let src = temp.path().join("src.txt");
        let dst = temp.path().join("dst.txt");
        fs::write(&src, b"original source").unwrap();

        let _ = clonefile_or_copy(&src, &dst).unwrap();
        fs::write(&dst, b"mutated dest").unwrap();

        assert_eq!(fs::read(&src).unwrap(), b"original source");
        assert_eq!(fs::read(&dst).unwrap(), b"mutated dest");
    }

    /// Reflinks (unlike hardlinks) give the destination its own
    /// inode. On a CoW filesystem this is the key correctness
    /// distinction: agents can chmod or write in place without
    /// reaching across worktrees.
    #[cfg(unix)]
    #[test]
    fn successful_reflink_yields_distinct_inode() {
        use std::os::unix::fs::MetadataExt;

        let temp = TempDir::new().unwrap();
        if !filesystem_supports_reflink(temp.path()) {
            eprintln!(
                "[skip] filesystem at {:?} does not support reflinks; cannot assert inode property",
                temp.path()
            );
            return;
        }

        let src = temp.path().join("src.txt");
        let dst = temp.path().join("dst.txt");
        fs::write(&src, b"reflink inode test").unwrap();

        let outcome = try_reflink(&src, &dst).unwrap();
        assert_eq!(
            outcome,
            ReflinkOutcome::Cloned,
            "filesystem advertised reflink support"
        );

        let src_inode = fs::metadata(&src).unwrap().ino();
        let dst_inode = fs::metadata(&dst).unwrap().ino();
        assert_ne!(
            src_inode, dst_inode,
            "reflinked files must have distinct inodes (got {} for both)",
            src_inode
        );
    }
}
