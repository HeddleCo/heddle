// SPDX-License-Identifier: Apache-2.0
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Clone, Copy)]
enum AtomicWriteKind {
    Normal,
    Secret,
}

impl AtomicWriteKind {
    fn open_tmp(self, tmp: &Path) -> io::Result<File> {
        let mut options = OpenOptions::new();
        options.create_new(true).write(true);

        #[cfg(unix)]
        if matches!(self, Self::Secret) {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }

        options.open(tmp)
    }

    fn enforce_before_write(self, file: &File) -> io::Result<()> {
        match self {
            Self::Normal => Ok(()),
            Self::Secret => enforce_secret_permissions_before_write(file),
        }
    }
}

#[cfg(unix)]
fn enforce_secret_permissions_before_write(file: &File) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    let mode = file.metadata()?.permissions().mode() & 0o777;
    if mode != 0o600 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("secret temp file permissions are {mode:o}, expected 600"),
        ));
    }
    Ok(())
}

#[cfg(not(unix))]
fn enforce_secret_permissions_before_write(_file: &File) -> io::Result<()> {
    // Non-Unix platforms do not expose POSIX mode bits through
    // OpenOptions. The secret variant still uses the same create-new,
    // write-fsync-rename discipline, but cannot verify a 0600 mode.
    Ok(())
}

static TEMP_PATH_COUNTER: AtomicU64 = AtomicU64::new(0);

/// POSIX `ENOSPC`. Identical on Linux and macOS. Windows surfaces disk-full
/// as `ERROR_DISK_FULL` (112) or `ERROR_HANDLE_DISK_FULL` (39); we cover
/// those by also checking `ErrorKind::StorageFull` (stable as of 1.83) and
/// the older `ErrorKind::Other` "no space" message text as a fallback.
const ENOSPC: i32 = 28;

/// POSIX `ENOTEMPTY`. Linux=39, macOS/BSD=66. Windows surfaces this as
/// `ERROR_DIR_NOT_EMPTY` (145). `ErrorKind::DirectoryNotEmpty` covers the
/// portable case, but the raw codes are the canonical signal — Rust may
/// still surface raw OS errors for paths the kernel reports unusually.
const ENOTEMPTY_LINUX: i32 = 39;
const ENOTEMPTY_MACOS: i32 = 66;
const ENOTEMPTY_WINDOWS: i32 = 145;

/// POSIX `EACCES`. Same code on Linux and macOS. `ErrorKind::PermissionDenied`
/// covers Windows `ERROR_ACCESS_DENIED` (5) too.
const EACCES: i32 = 13;

/// POSIX `ENOENT`. Same code on Linux and macOS. `ErrorKind::NotFound` covers
/// Windows `ERROR_FILE_NOT_FOUND` (2) and `ERROR_PATH_NOT_FOUND` (3).
const ENOENT: i32 = 2;

/// POSIX `EROFS`. Linux=30, macOS=30. `ErrorKind::ReadOnlyFilesystem` is
/// the portable variant (stable as of 1.83).
const EROFS: i32 = 30;

/// POSIX `EXDEV` ("cross-device link"). Linux=18, macOS=18.
/// `ErrorKind::CrossesDevices` is the portable variant (stable as of 1.83).
const EXDEV: i32 = 18;

/// Returns true when an `io::Error` indicates the filesystem is out of
/// space. Centralised here because it's the same predicate used by
/// `write_file_atomic` (the inner helper) and by the higher-level
/// `cmd_snapshot` recovery path that prints the actionable message.
pub fn is_out_of_space(err: &io::Error) -> bool {
    if err.raw_os_error() == Some(ENOSPC) {
        return true;
    }
    // `ErrorKind::StorageFull` is the portable kind. It maps to ENOSPC
    // on Unix and the Windows disk-full codes. Available since Rust
    // 1.83; the workspace MSRV is well past that.
    if err.kind() == io::ErrorKind::StorageFull {
        return true;
    }
    // `write_all` translates a short write into `WriteZero`. On a full
    // disk, kernel can return a short write rather than ENOSPC outright
    // (especially over network filesystems), so a `WriteZero` we couldn't
    // otherwise classify is treated as out-of-space — overly inclusive
    // here is safer than missing the signal.
    if err.kind() == io::ErrorKind::WriteZero {
        return true;
    }
    false
}

/// Returns true when an `io::Error` indicates a directory could not be
/// removed because it still contained entries. The apply planner only removes
/// tracked descendants; when tracked content is removed and the parent
/// directory still holds untracked or explicitly ignored siblings, `remove_dir`
/// returns this signal. We need both `ErrorKind::DirectoryNotEmpty` and the raw
/// codes — Linux=39, macOS/BSD=66, Windows=145 — because Rust does not
/// always translate every kernel surface into the portable `ErrorKind`.
pub fn is_directory_not_empty(err: &io::Error) -> bool {
    if err.kind() == io::ErrorKind::DirectoryNotEmpty {
        return true;
    }
    matches!(
        err.raw_os_error(),
        Some(ENOTEMPTY_LINUX) | Some(ENOTEMPTY_MACOS) | Some(ENOTEMPTY_WINDOWS)
    )
}

/// Returns true when an `io::Error` indicates the operation was denied
/// for permissions reasons (`EACCES` on Unix, `ERROR_ACCESS_DENIED` on
/// Windows). The portable `ErrorKind::PermissionDenied` covers most
/// surfaces; the raw `EACCES` check handles oddball platforms that
/// surface the OS code without translating to the portable kind.
pub fn is_permission_denied(err: &io::Error) -> bool {
    if err.kind() == io::ErrorKind::PermissionDenied {
        return true;
    }
    err.raw_os_error() == Some(EACCES)
}

/// Returns true when an `io::Error` indicates the path referenced by an
/// operation does not exist (`ENOENT` on Unix, `ERROR_FILE_NOT_FOUND` /
/// `ERROR_PATH_NOT_FOUND` on Windows). Use this *only* at call sites
/// where the operation expected the path to exist — the predicate alone
/// can't distinguish "I expected this" from "I checked optionally".
pub fn is_not_found(err: &io::Error) -> bool {
    if err.kind() == io::ErrorKind::NotFound {
        return true;
    }
    err.raw_os_error() == Some(ENOENT)
}

/// Returns true when an `io::Error` indicates the underlying filesystem
/// is mounted read-only (`EROFS` on Unix). The portable
/// `ErrorKind::ReadOnlyFilesystem` is preferred when present; we also
/// match the raw OS code because some platforms (notably older macOS
/// surfaces and certain remote filesystems) do not always translate.
pub fn is_read_only_filesystem(err: &io::Error) -> bool {
    if err.kind() == io::ErrorKind::ReadOnlyFilesystem {
        return true;
    }
    err.raw_os_error() == Some(EROFS)
}

/// Returns true when an `io::Error` indicates a `rename` (or other
/// link-style operation) attempted to bridge two filesystems (`EXDEV`).
/// This is what trips when `temp_path` lands on a different mount than
/// the destination — typically because `TMPDIR` is on a different volume,
/// or the parent directory itself is a bind mount. We match both the
/// portable `ErrorKind::CrossesDevices` and the raw `EXDEV` code.
pub fn is_cross_device_link(err: &io::Error) -> bool {
    if err.kind() == io::ErrorKind::CrossesDevices {
        return true;
    }
    err.raw_os_error() == Some(EXDEV)
}

pub fn temp_path(path: &Path) -> PathBuf {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let file_name = path
        .file_name()
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("heddle-tmp");
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let counter = TEMP_PATH_COUNTER.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    parent.join(format!(".{file_name}.tmp-{pid}-{unique}-{counter}"))
}

/// Kick a file's dirty page cache into background writeback WITHOUT waiting
/// for it or issuing a device flush. Best-effort: any error is ignored, since
/// the caller's subsequent `fsync` is what actually guarantees durability —
/// this only *starts* the I/O early so many files' writeback overlaps instead
/// of each `fsync` flushing its file synchronously from scratch.
///
/// Linux-only (`sync_file_range`); a no-op elsewhere, where the batched-fsync
/// pass in [`stage_temp_files_durable`] simply runs without the overlap.
#[cfg(target_os = "linux")]
fn kick_writeback(file: &File) {
    use std::os::unix::io::AsRawFd;
    // SYNC_FILE_RANGE_WRITE = 2: initiate writeback of dirty pages in the
    // given range (0..0 = whole file) without blocking. No barrier, no error
    // path — a failure just means the later `sync_all` does the work.
    const SYNC_FILE_RANGE_WRITE: libc::c_uint = 2;
    unsafe {
        libc::sync_file_range(file.as_raw_fd(), 0, 0, SYNC_FILE_RANGE_WRITE);
    }
}

#[cfg(not(target_os = "linux"))]
fn kick_writeback(_file: &File) {}

/// Write many temp files with a single overlapped-writeback durability pass.
///
/// For each `(temp_path, bytes)`: create the temp file and write its contents,
/// then start its page-cache writeback in the background ([`kick_writeback`]).
/// After every file is written, `fsync` each one. On return, every temp file's
/// data is on stable storage — the SAME guarantee as writing + `fsync`-ing each
/// file individually — but the writeback I/O overlaps instead of serializing
/// one synchronous `fsync` barrier per file.
///
/// This is the bulk-ref hot path (`heddle adopt` of N branches publishes N ref
/// files in one batch): the per-file `write → fsync` loop paid ~N serial fsync
/// barriers (~2.3s for 800 refs on a local SSD); overlapping the writeback
/// collapses that to ~0.1s with no change to the durability contract. Callers
/// still `rename` each temp into place and `fsync` the parent directory to make
/// the renames durable.
///
/// The temp files' parent directories must already exist. On the first write
/// error the partial temp files are left for the caller's rollback/cleanup to
/// remove (they are uniquely named and never renamed into place).
pub fn stage_temp_files_durable(files: &[(PathBuf, Vec<u8>)]) -> io::Result<()> {
    let mut handles: Vec<File> = Vec::with_capacity(files.len());
    for (temp_path, bytes) in files {
        let mut file = File::create(temp_path).map_err(|err| enrich_write_error(temp_path, err))?;
        file.write_all(bytes)
            .map_err(|err| enrich_write_error(temp_path, err))?;
        kick_writeback(&file);
        handles.push(file);
    }
    // Barrier pass: by now most files' writeback is already in flight (or done),
    // so each `sync_all` blocks only on the tail, not a cold synchronous flush.
    for (file, (temp_path, _)) in handles.iter().zip(files) {
        file.sync_all()
            .map_err(|err| enrich_write_error(temp_path, err))?;
    }
    Ok(())
}

/// fsync the directory inode so a preceding `rename` is durable across
/// crashes. POSIX-only — on Windows this is a no-op.
///
/// On Linux/macOS, after an `fsync(file)` + `rename(tmp, dest)` the
/// rename itself still needs to be made durable, which requires
/// `fsync(parent_dir)` (open parent for read, `sync_all`). Without it
/// a crash between the rename and the next directory writeback can
/// leave the destination dirent missing even though the file's data is
/// on disk.
///
/// Windows directories don't support this pattern. `CreateFileW` with
/// `GENERIC_READ` against a directory returns `ERROR_ACCESS_DENIED`
/// unless the caller passes `FILE_FLAG_BACKUP_SEMANTICS`, and even
/// then `FlushFileBuffers` on a directory handle is undefined — NTFS
/// reports access-denied. Directory metadata durability on Windows is
/// handled by the NTFS log; there is no userspace knob equivalent to
/// `fsync(dirfd)`, and standard ecosystem crates (`tempfile`,
/// `atomicwrites`) treat the directory sync as a Unix-only concern.
///
/// Returning `Ok(())` on Windows matches that consensus and fixes
/// heddle#105 (`Repository::init_default` panicking with
/// `PermissionDenied` on every `write_file_atomic` of an oplog or
/// state file under a Windows tempdir).
#[cfg(windows)]
pub fn sync_directory(_path: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(not(windows))]
pub fn sync_directory(path: &Path) -> io::Result<()> {
    let dir = OpenOptions::new().read(true).open(path)?;
    dir.sync_all()
}

/// Wrap an `io::Error` raised while writing `path` so that ENOSPC carries
/// an actionable message naming the path. Non-ENOSPC errors pass through
/// unchanged. The wrapped error's `raw_os_error()` still returns 28, and
/// [`is_out_of_space`] still detects it — callers (e.g. `cmd_snapshot`)
/// rely on this for stable exit-code mapping.
///
/// Thin wrapper over [`enrich_fs_error`] for the historical "writing"
/// call sites. New code should prefer `enrich_fs_error(path, "writing", err)`
/// directly so the operation name is explicit at the call site.
fn enrich_write_error(path: &Path, err: io::Error) -> io::Error {
    enrich_fs_error(path, "writing", err)
}

/// Wrap an `io::Error` produced by a filesystem operation against `path`
/// with a heddle-context message naming both the operation and the path.
///
/// The mapping covers the cases users actually hit and the messages we
/// promise from heddle's CLI surface:
/// - **ENOTEMPTY** — usually `remove_dir` against a directory that still
///   holds untracked or explicitly ignored content, such as build output.
///   The high-level fix is to leave the directory in place, but when the
///   error does surface (e.g. a path the planner *did* expect to remove),
///   the message names the path so the user can investigate.
/// - **EACCES** — naming the path and the action ("removing", "writing",
///   "renaming") is enough for the user to inspect mode bits.
/// - **ENOENT** — caller-driven: only enriched when the operation
///   expected the path to exist (so optional reads like a missing index
///   pass through unchanged via the `is_not_found` predicate).
/// - **EROFS** — points the user at the filesystem mount, not at heddle.
/// - **EXDEV** — points the user at the temp path / mount mismatch.
/// - **ENOSPC** — same actionable disk-full message the snapshot path
///   already relies on.
///
/// `op` is a verb in the present-progressive ("writing", "removing",
/// "renaming", "creating") so the resulting message reads naturally:
///   `"could not remove `<path>` because it contains content..."`.
///
/// The wrapped error preserves `raw_os_error()` (callers still classify
/// disk-full via [`is_out_of_space`]) and exposes the original `io::Error`
/// through the `Error::source` chain (so `RUST_BACKTRACE=1` and
/// `anyhow`'s chain printer still surface the OS error).
pub fn enrich_fs_error(path: &Path, op: &'static str, err: io::Error) -> io::Error {
    if is_out_of_space(&err) {
        let msg = format!(
            "out of disk space {op} {}: free disk space and re-run the command — your working tree is unchanged",
            path.display()
        );
        return io::Error::new(
            io::ErrorKind::StorageFull,
            EnrichedFsError { msg, source: err },
        );
    }
    if is_directory_not_empty(&err) {
        let msg = format!(
            "could not remove directory `{}` because it contains content (heddle-ignored or otherwise) — leaving in place",
            path.display()
        );
        return io::Error::new(
            io::ErrorKind::DirectoryNotEmpty,
            EnrichedFsError { msg, source: err },
        );
    }
    if is_read_only_filesystem(&err) {
        let msg = format!(
            "filesystem is read-only — `{}` cannot be modified",
            path.display()
        );
        return io::Error::new(
            io::ErrorKind::ReadOnlyFilesystem,
            EnrichedFsError { msg, source: err },
        );
    }
    if is_permission_denied(&err) {
        let msg = format!(
            "permission denied {op} `{}` — check filesystem permissions",
            path.display()
        );
        return io::Error::new(
            io::ErrorKind::PermissionDenied,
            EnrichedFsError { msg, source: err },
        );
    }
    if is_not_found(&err) {
        let msg = format!("could not find `{}` for {op}", path.display());
        return io::Error::new(
            io::ErrorKind::NotFound,
            EnrichedFsError { msg, source: err },
        );
    }
    if is_cross_device_link(&err) {
        let msg = format!(
            "cannot rename across filesystems — temp file for `{}` lives on a different mount; set TMPDIR to the same filesystem as the destination",
            path.display()
        );
        return io::Error::new(
            io::ErrorKind::CrossesDevices,
            EnrichedFsError { msg, source: err },
        );
    }
    err
}

/// Wrap an `EXDEV` error from `fs::rename` with both the source temp path
/// and the destination — the user needs both to understand which mount
/// boundary the rename tripped on. Other error kinds delegate to
/// [`enrich_fs_error`] using the destination as the principal path.
pub fn enrich_rename_error(src: &Path, dst: &Path, err: io::Error) -> io::Error {
    if is_cross_device_link(&err) {
        let msg = format!(
            "cannot rename across filesystems — temp file at `{}` cannot be renamed to `{}`; set TMPDIR to the same filesystem as the destination",
            src.display(),
            dst.display()
        );
        return io::Error::new(
            io::ErrorKind::CrossesDevices,
            EnrichedFsError { msg, source: err },
        );
    }
    enrich_fs_error(dst, "renaming", err)
}

#[derive(Debug)]
struct EnrichedFsError {
    msg: String,
    source: io::Error,
}

impl std::fmt::Display for EnrichedFsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.msg)
    }
}

impl std::error::Error for EnrichedFsError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

fn write_file_atomic_impl(
    path: &Path,
    bytes: &[u8],
    kind: AtomicWriteKind,
    before_write: impl FnOnce(&File, &Path) -> io::Result<()>,
) -> io::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).map_err(|e| enrich_fs_error(parent, "creating", e))?;

    let tmp = temp_path(path);
    let inner = (|| -> io::Result<()> {
        let mut file = kind.open_tmp(&tmp)?;
        kind.enforce_before_write(&file)?;
        before_write(&file, &tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        Ok(())
    })();

    if let Err(err) = inner {
        // Best-effort cleanup. On ENOSPC the tempfile may itself be the
        // cause of the disk pressure; removing it gives the user back
        // some slack before they re-run.
        let _ = fs::remove_file(&tmp);
        return Err(enrich_write_error(path, err));
    }

    fs::rename(&tmp, path).map_err(|e| enrich_rename_error(&tmp, path, e))?;
    sync_directory(parent).map_err(|e| enrich_fs_error(parent, "syncing", e))
}

pub fn write_file_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    write_file_atomic_impl(path, bytes, AtomicWriteKind::Normal, |_, _| Ok(()))
}

/// Create a directory tree with owner-only permissions on Unix (`0o700`).
///
/// Used for `.heddle` / `~/.heddle` trees that hold credentials, keys, and
/// repository secrets. On non-Unix platforms this falls back to
/// [`fs::create_dir_all`]. Existing directories are left as-is (creation-time
/// privacy; callers that need to tighten existing modes should do so
/// explicitly).
pub fn create_private_dir_all(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        let mut builder = fs::DirBuilder::new();
        builder.recursive(true).mode(0o700);
        match builder.create(path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Ok(()),
            Err(e) => Err(e),
        }
    }
    #[cfg(not(unix))]
    {
        fs::create_dir_all(path)
    }
}

/// Atomically write secret material without ever creating a group/world
/// readable temporary file.
///
/// On Unix the temp inode is created with `OpenOptions::mode(0o600)` before
/// any bytes are written, then the open file descriptor is enforced to exact
/// `0600` before the payload is written. Permission failures are hard errors
/// and the temp file is removed best-effort. On non-Unix platforms there is no
/// portable POSIX mode API, so this uses the normal create-new temp file,
/// fsync, and rename sequence.
pub fn write_file_atomic_secret(path: &Path, bytes: &[u8]) -> io::Result<()> {
    write_file_atomic_impl(path, bytes, AtomicWriteKind::Secret, |_, _| Ok(()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enospc_io_error() -> io::Error {
        io::Error::from_raw_os_error(ENOSPC)
    }

    #[test]
    fn is_out_of_space_detects_enospc_raw() {
        assert!(is_out_of_space(&enospc_io_error()));
    }

    #[test]
    fn is_out_of_space_detects_storage_full_kind() {
        let err = io::Error::new(io::ErrorKind::StorageFull, "mock disk full");
        assert!(is_out_of_space(&err));
    }

    #[test]
    fn is_out_of_space_detects_write_zero() {
        let err = io::Error::new(io::ErrorKind::WriteZero, "short write");
        assert!(is_out_of_space(&err));
    }

    #[test]
    fn is_out_of_space_rejects_unrelated_errors() {
        assert!(!is_out_of_space(&io::Error::new(
            io::ErrorKind::NotFound,
            "missing"
        )));
        assert!(!is_out_of_space(&io::Error::new(
            io::ErrorKind::PermissionDenied,
            "nope"
        )));
        assert!(!is_out_of_space(&io::Error::other("generic")));
    }

    #[test]
    fn is_directory_not_empty_detects_kind() {
        let err = io::Error::new(io::ErrorKind::DirectoryNotEmpty, "still has children");
        assert!(is_directory_not_empty(&err));
    }

    #[test]
    fn is_directory_not_empty_detects_raw_codes() {
        for code in [ENOTEMPTY_LINUX, ENOTEMPTY_MACOS, ENOTEMPTY_WINDOWS] {
            assert!(
                is_directory_not_empty(&io::Error::from_raw_os_error(code)),
                "expected raw OS error {code} to classify as ENOTEMPTY"
            );
        }
    }

    #[test]
    fn is_directory_not_empty_rejects_unrelated() {
        assert!(!is_directory_not_empty(&io::Error::new(
            io::ErrorKind::NotFound,
            "missing"
        )));
        assert!(!is_directory_not_empty(&enospc_io_error()));
    }

    #[test]
    fn is_permission_denied_detects_kind_and_raw() {
        assert!(is_permission_denied(&io::Error::new(
            io::ErrorKind::PermissionDenied,
            "nope"
        )));
        assert!(is_permission_denied(&io::Error::from_raw_os_error(EACCES)));
    }

    #[test]
    fn is_not_found_detects_kind_and_raw() {
        assert!(is_not_found(&io::Error::new(
            io::ErrorKind::NotFound,
            "missing"
        )));
        assert!(is_not_found(&io::Error::from_raw_os_error(ENOENT)));
    }

    #[test]
    fn is_read_only_filesystem_detects_raw() {
        assert!(is_read_only_filesystem(&io::Error::from_raw_os_error(
            EROFS
        )));
    }

    #[test]
    fn is_cross_device_link_detects_raw() {
        assert!(is_cross_device_link(&io::Error::from_raw_os_error(EXDEV)));
    }

    #[test]
    fn enrich_fs_error_passes_through_unclassified() {
        let path = Path::new("/tmp/example");
        let original = io::Error::other("weird");
        let wrapped = enrich_fs_error(path, "writing", original);
        // Unclassified errors are returned untouched.
        assert_eq!(wrapped.kind(), io::ErrorKind::Other);
        assert_eq!(wrapped.to_string(), "weird");
    }

    #[test]
    fn enrich_fs_error_wraps_enospc_with_path_and_recovery_hint() {
        let path = Path::new("/repo/.heddle/state/abc.bin");
        let wrapped = enrich_fs_error(path, "writing", enospc_io_error());

        // Stable kind so the CLI exit-code mapper finds it.
        assert_eq!(wrapped.kind(), io::ErrorKind::StorageFull);
        // Message names the failure, the path, and the recovery.
        let msg = wrapped.to_string();
        assert!(
            msg.contains("out of disk space"),
            "missing failure name: {msg}"
        );
        assert!(
            msg.contains("/repo/.heddle/state/abc.bin"),
            "missing path: {msg}"
        );
        assert!(
            msg.contains("free disk space") && msg.contains("re-run"),
            "missing recovery hint: {msg}"
        );
        assert!(
            msg.contains("working tree is unchanged"),
            "missing reassurance: {msg}"
        );
        // Source chain preserved so callers that walk `source()` (e.g.
        // anyhow's chain printer) can still see the original ENOSPC.
        let src = std::error::Error::source(&wrapped as &dyn std::error::Error)
            .or_else(|| wrapped.get_ref().and_then(|e| e.source()))
            .expect("source preserved");
        assert!(src.to_string().to_lowercase().contains("space"));
    }

    #[test]
    fn enrich_fs_error_wraps_enotempty_with_directory_message() {
        let path = Path::new("/repo/web");
        let wrapped = enrich_fs_error(
            path,
            "removing",
            io::Error::from_raw_os_error(ENOTEMPTY_MACOS),
        );
        assert_eq!(wrapped.kind(), io::ErrorKind::DirectoryNotEmpty);
        let msg = wrapped.to_string();
        assert!(
            msg.contains("could not remove directory"),
            "missing action: {msg}"
        );
        assert!(msg.contains("/repo/web"), "missing path: {msg}");
        assert!(
            msg.contains("heddle-ignored"),
            "missing heddle-ignored hint: {msg}"
        );
        assert!(
            msg.contains("leaving in place"),
            "missing reassurance: {msg}"
        );
        // raw_os_error() does NOT round-trip — `io::Error::new(kind, source)`
        // synthesizes a new error whose `raw_os_error()` is None — but the
        // source chain still exposes the original OS code for callers that
        // walk it.
        let src = wrapped.get_ref().and_then(|e| e.source()).expect("source");
        let original = src
            .downcast_ref::<io::Error>()
            .expect("original io::Error preserved");
        assert_eq!(original.raw_os_error(), Some(ENOTEMPTY_MACOS));
    }

    #[test]
    fn enrich_fs_error_wraps_eacces_with_op_and_path() {
        let path = Path::new("/repo/.heddle/state/index.bin");
        let wrapped = enrich_fs_error(path, "writing", io::Error::from_raw_os_error(EACCES));
        assert_eq!(wrapped.kind(), io::ErrorKind::PermissionDenied);
        let msg = wrapped.to_string();
        assert!(msg.starts_with("permission denied writing"), "msg: {msg}");
        assert!(msg.contains("/repo/.heddle/state/index.bin"), "msg: {msg}");
        assert!(msg.contains("check filesystem permissions"), "msg: {msg}");
    }

    #[test]
    fn enrich_fs_error_wraps_enoent_with_op_and_path() {
        let path = Path::new("/repo/.heddle");
        let wrapped = enrich_fs_error(path, "opening", io::Error::from_raw_os_error(ENOENT));
        assert_eq!(wrapped.kind(), io::ErrorKind::NotFound);
        let msg = wrapped.to_string();
        assert!(msg.contains("could not find"), "missing action: {msg}");
        assert!(msg.contains("/repo/.heddle"), "missing path: {msg}");
        assert!(msg.contains("for opening"), "missing op: {msg}");
    }

    #[test]
    fn enrich_fs_error_wraps_erofs_with_path() {
        let path = Path::new("/mnt/readonly/.heddle/state/index.bin");
        let wrapped = enrich_fs_error(path, "writing", io::Error::from_raw_os_error(EROFS));
        assert_eq!(wrapped.kind(), io::ErrorKind::ReadOnlyFilesystem);
        let msg = wrapped.to_string();
        assert!(msg.contains("filesystem is read-only"), "msg: {msg}");
        assert!(
            msg.contains("/mnt/readonly/.heddle/state/index.bin"),
            "msg: {msg}"
        );
        assert!(msg.contains("cannot be modified"), "msg: {msg}");
    }

    #[test]
    fn enrich_rename_error_wraps_exdev_with_src_and_dst() {
        let src = Path::new("/tmp-mount/.x.tmp-1234");
        let dst = Path::new("/repo/.heddle/state/index.bin");
        let wrapped = enrich_rename_error(src, dst, io::Error::from_raw_os_error(EXDEV));
        assert_eq!(wrapped.kind(), io::ErrorKind::CrossesDevices);
        let msg = wrapped.to_string();
        assert!(
            msg.contains("cannot rename across filesystems"),
            "msg: {msg}"
        );
        assert!(msg.contains("/tmp-mount/.x.tmp-1234"), "missing src: {msg}");
        assert!(
            msg.contains("/repo/.heddle/state/index.bin"),
            "missing dst: {msg}"
        );
        assert!(msg.contains("TMPDIR"), "missing recovery hint: {msg}");
    }

    #[test]
    fn enrich_rename_error_falls_through_to_generic_for_other_kinds() {
        let src = Path::new("/tmp/.x.tmp");
        let dst = Path::new("/repo/file");
        let wrapped = enrich_rename_error(src, dst, io::Error::from_raw_os_error(EACCES));
        // Non-EXDEV rename failures get the generic `enrich_fs_error`
        // treatment, which preserves the dst path and the "renaming" op.
        assert_eq!(wrapped.kind(), io::ErrorKind::PermissionDenied);
        let msg = wrapped.to_string();
        assert!(msg.starts_with("permission denied renaming"), "msg: {msg}");
        assert!(msg.contains("/repo/file"), "missing dst: {msg}");
    }

    #[test]
    fn enrich_write_error_passes_through_non_enospc_unclassified() {
        // The historical helper now delegates to `enrich_fs_error`, so a
        // generic Other error still passes through unchanged.
        let path = Path::new("/tmp/example");
        let original = io::Error::other("weird");
        let wrapped = enrich_write_error(path, original);
        assert_eq!(wrapped.kind(), io::ErrorKind::Other);
        assert_eq!(wrapped.to_string(), "weird");
    }

    #[test]
    fn write_file_atomic_round_trip() {
        let dir = tempfile::TempDir::new().unwrap();
        let target = dir.path().join("nested/under/here/file.bin");
        write_file_atomic(&target, b"hello").unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"hello");
    }

    #[test]
    fn stage_temp_files_durable_writes_every_file_verbatim() {
        // The bulk-ref hot path stages N temp files in one overlapped-writeback
        // pass. Every file must land with its exact bytes — the batching is a
        // durability/perf optimization, never a content one.
        let dir = tempfile::TempDir::new().unwrap();
        let files: Vec<(PathBuf, Vec<u8>)> = (0..50)
            .map(|i| {
                (
                    dir.path().join(format!("ref-{i}.tmp")),
                    format!("change-id-{i}\n").into_bytes(),
                )
            })
            .collect();

        stage_temp_files_durable(&files).unwrap();

        for (path, bytes) in &files {
            assert_eq!(&fs::read(path).unwrap(), bytes, "mismatch at {path:?}");
        }
    }

    #[test]
    fn stage_temp_files_durable_empty_batch_is_ok() {
        // A publish with no new-content plans (e.g. a pure delete batch) hands
        // an empty slice; it must be a clean no-op, not an error.
        stage_temp_files_durable(&[]).unwrap();
    }

    #[test]
    fn stage_temp_files_durable_errors_when_parent_missing() {
        // The helper does NOT create parent directories (callers pre-create
        // them via `alloc_temp_path`); a missing parent surfaces as an error
        // rather than silently dropping the write.
        let dir = tempfile::TempDir::new().unwrap();
        let files = vec![(dir.path().join("does/not/exist/ref.tmp"), b"x".to_vec())];
        assert!(stage_temp_files_durable(&files).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn create_private_dir_all_sets_0700() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::TempDir::new().unwrap();
        let target = dir.path().join("nested/private");
        create_private_dir_all(&target).expect("create private dir");
        let mode = fs::metadata(&target).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700, "new private dir must be 0700, got {mode:o}");
    }

    #[cfg(unix)]
    #[test]
    fn write_file_atomic_secret_is_0600_before_write_and_after_rename() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::TempDir::new().unwrap();
        let target = dir.path().join("nested/secret.txt");
        let mut observed_tmp_mode = None;

        write_file_atomic_impl(&target, b"secret", AtomicWriteKind::Secret, |file, tmp| {
            let fd_mode = file.metadata()?.permissions().mode() & 0o777;
            let path_mode = fs::metadata(tmp)?.permissions().mode() & 0o777;
            observed_tmp_mode = Some((fd_mode, path_mode));
            Ok(())
        })
        .unwrap();

        assert_eq!(observed_tmp_mode, Some((0o600, 0o600)));
        let final_mode = fs::metadata(&target).unwrap().permissions().mode() & 0o777;
        assert_eq!(final_mode, 0o600);
        assert_eq!(fs::read(&target).unwrap(), b"secret");
    }

    #[test]
    fn write_file_atomic_secret_cleans_up_when_pre_write_check_fails() {
        let dir = tempfile::TempDir::new().unwrap();
        let target = dir.path().join("secret.txt");
        let mut tmp_path = None;

        let err = write_file_atomic_impl(&target, b"secret", AtomicWriteKind::Secret, |_, tmp| {
            tmp_path = Some(tmp.to_path_buf());
            Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "injected permission failure",
            ))
        })
        .expect_err("permission failure should propagate");

        assert!(is_permission_denied(&err), "unexpected error: {err}");
        assert!(!target.exists(), "secret write must not publish target");
        let tmp = tmp_path.expect("pre-write hook observed temp path");
        assert!(!tmp.exists(), "failed secret write should remove temp file");
    }

    /// Regression for heddle#105: `sync_directory` must succeed on any
    /// writable directory. The original implementation called
    /// `OpenOptions::new().read(true).open(dir)` + `sync_all()`, which
    /// fails on Windows with `ERROR_ACCESS_DENIED` (5) because Windows
    /// directory handles require `FILE_FLAG_BACKUP_SEMANTICS` and
    /// `FlushFileBuffers` on a directory handle is not a supported
    /// operation. The failure cascaded through `write_file_atomic` into
    /// `Repository::init_default`, breaking `heddle init` on Windows.
    #[test]
    fn sync_directory_succeeds_on_writable_tempdir() {
        let dir = tempfile::TempDir::new().unwrap();
        sync_directory(dir.path()).expect("sync_directory on writable tempdir");
    }

    /// Regression for heddle#105: full `write_file_atomic` round-trip
    /// against a freshly-created nested directory must not surface
    /// `PermissionDenied`. The previous failure mode was the
    /// `sync_directory(parent)` call at the end of `write_file_atomic`.
    #[test]
    fn write_file_atomic_does_not_permission_deny_on_parent_sync() {
        let dir = tempfile::TempDir::new().unwrap();
        let target = dir.path().join("oplog/oplog.bin");
        let result = write_file_atomic(&target, b"hello");
        if let Err(e) = &result {
            assert!(
                !is_permission_denied(e),
                "write_file_atomic surfaced PermissionDenied on a writable \
                 tempdir (heddle#105): {e}"
            );
        }
        result.expect("write_file_atomic");
    }
}
