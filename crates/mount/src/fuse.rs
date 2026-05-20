// SPDX-License-Identifier: Apache-2.0
//! Linux FUSE shell built on `fuser`.
//!
//! The shell is a thin adapter: every callback either translates
//! arguments and dispatches to a [`PlatformShell`], or replies with
//! the errno from a [`MountError`]. The mount is read-write: writes
//! are buffered in the core's hot tier, promoted to CAS on `flush`
//! /`release`, and folded into a real heddle state by
//! [`ContentAddressedMount::capture`].
//!
//! ## Implemented kernel callbacks
//!
//! Read path:
//! * `init` — opts into `FUSE_DIRECT_IO_ALLOW_MMAP` so mmap works
//!   alongside `FOPEN_DIRECT_IO`.
//! * `lookup` / `getattr` / `open` / `read` / `readdir` / `flush` /
//!   `release` / `opendir` / `releasedir` / `destroy`.
//!
//! Write path (heddle#180):
//! * `create` — `open(O_CREAT[|O_EXCL|O_TRUNC])`. EEXIST on O_EXCL
//!   clash. Returns `FOPEN_DIRECT_IO`.
//! * `mkdir` — empty directory in the overlay.
//! * `mknod` — regular files only; non-`S_IFREG` returns EPERM.
//! * `unlink` / `rmdir`.
//! * `rename` — overlay+captured file/symlink + overlay-only dir.
//!   RENAME_NOREPLACE honoured; EXCHANGE and WHITEOUT return EINVAL.
//! * `setattr` — chmod (folded to FileMode), ftruncate / O_TRUNC,
//!   mtime / uid / gid accepted as no-ops.
//! * `symlink` / `readlink`.
//! * `link` — returns EPERM (no nlink in heddle's tree model).
//! * `write` — already implemented; the freshly created file inherits
//!   the same hot-tier behaviour as a write to a captured file.
//!
//! Anything not listed (xattrs, locks, statfs, ioctl, fsync,
//! fsyncdir, copy_file_range, lseek, fallocate, etc.) inherits
//! fuser's default reply (ENOSYS / OK depending on the op) and is
//! out of scope until a real workload needs it. See
//! `crates/mount/README.md` ("Per-thread overlay semantics") for
//! the matching write-side state model.
//!
//! ## Panic safety
//!
//! Every callback body runs inside [`std::panic::catch_unwind`] via
//! the [`guarded`] helpers below. A panic deep in the
//! [`PlatformShell`] dispatch (poisoned mutex, integer overflow,
//! `unwrap()` on a freshly-evicted cache entry) translates to a
//! single `reply.error(EIO)` for the offending operation instead of
//! one of two production-hostile outcomes:
//!
//!  1. **Lost worker thread.** `fuser` spawns one worker per session
//!     (or a small pool for multi-threaded mode) and dispatches
//!     callbacks on it. A panic that escapes the callback unwinds
//!     out of the worker, terminating it. The kernel side keeps
//!     waiting for replies that will never come — userspace
//!     processes accessing the mount block until `fusermount -u` or
//!     a `SIGKILL` of the FUSE driver. The remaining mounts hosted
//!     by `heddled` keep serving, but this one wedges.
//!  2. **Process abort (Rust ≥1.81).** `fuser`'s reply path goes
//!     through `extern "C"` shims internally. A panic that unwinds
//!     across that boundary aborts the process — taking every
//!     mount, daemon worker, and unsaved hot-tier buffer with it.
//!
//! The FSKit shell hits the same risk and uses the same fix
//! ([`crate::fskit::guarded_c_int`]). Keeping the two adapters
//! symmetric means a panic in shared core code can't take down one
//! platform while sparing the other.

use std::{
    ffi::OsStr,
    panic::AssertUnwindSafe,
    path::Path,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use fuser::{
    BackgroundSession, BsdFileFlags, Config, Errno, FileAttr, FileHandle, FileType, Filesystem,
    FopenFlags, Generation, INodeNo, InitFlags, KernelConfig, LockOwner, MountOption, OpenFlags,
    RenameFlags, ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory, ReplyEmpty, ReplyEntry,
    ReplyOpen, ReplyWrite, Request, Session, TimeOrNow, WriteFlags,
};
use objects::object::FileMode;
use tracing::{debug, warn};

use crate::{
    core::ContentAddressedMount,
    error::Result,
    shell::{AttrUpdate, Attrs, Entry, NodeId, NodeKind, PlatformShell},
};

/// FUSE attribute timeout. Heddle's mount is content-addressed —
/// nothing under a fixed `(state, path)` ever changes — so a long
/// TTL is correct. We pick one second to stay polite toward
/// invalidation: when the thread advances and we eventually wire
/// `notify_inval_*` through, callers want a snappy reaction.
const TTL: Duration = Duration::from_secs(1);

/// Generation number for FUSE inodes. We don't reuse ids across
/// remounts, so a constant is fine.
const GENERATION: Generation = Generation(0);

/// Adapter that exposes a [`ContentAddressedMount`] to the kernel
/// via FUSE. Owns the mount in an `Arc` so the FUSE worker thread(s)
/// share the same registry.
pub struct FuseShell {
    inner: Arc<ContentAddressedMount>,
}

impl FuseShell {
    /// Wrap a mount into a FUSE filesystem.
    pub fn new(mount: ContentAddressedMount) -> Self {
        Self {
            inner: Arc::new(mount),
        }
    }

    /// Best-effort mtime to attach to entry replies for newly
    /// created paths. The core hands out a fixed `mounted_at`
    /// timestamp for every attrs call, but ReplyEntry needs one
    /// inline before the kernel has issued any followup `getattr`.
    /// We approximate by reading the root's attrs (a hash-table
    /// hit) and falling back to UNIX_EPOCH on any error.
    fn inner_mtime(&self) -> std::time::SystemTime {
        self.inner
            .attrs(NodeId::ROOT)
            .map(|a| a.mtime)
            .unwrap_or(UNIX_EPOCH)
    }

    /// Hand out a shared handle to the underlying mount.
    ///
    /// `mount_background` / `mount` consume `self`, so the only
    /// chance to grab a long-lived handle is before mounting. Used
    /// by the `fuse_e2e` bench to call
    /// [`ContentAddressedMount::clear_blob_cache`] between iterations
    /// without rebuilding the entire session — which is what makes
    /// the cold-blob-cache benchmark intent (see the bench module
    /// docs) actually hold across samples.
    pub fn mount_handle(&self) -> Arc<ContentAddressedMount> {
        Arc::clone(&self.inner)
    }

    /// Mount synchronously. Blocks the calling thread for the lifetime
    /// of the mount (returns when unmounted or on error).
    pub fn mount(self, mountpoint: impl AsRef<Path>) -> Result<()> {
        let config = default_config();
        fuser::mount2(self, mountpoint.as_ref(), &config)
            .map_err(|e| crate::error::MountError::Store(objects::error::HeddleError::Io(e)))?;
        Ok(())
    }

    /// Mount in a background session. Caller holds the returned
    /// [`BackgroundSession`]; dropping it triggers an unmount.
    pub fn mount_background(self, mountpoint: impl AsRef<Path>) -> Result<BackgroundSession> {
        let config = default_config();
        let session = Session::new(self, mountpoint.as_ref(), &config)
            .map_err(|e| crate::error::MountError::Store(objects::error::HeddleError::Io(e)))?;
        session
            .spawn()
            .map_err(|e| crate::error::MountError::Store(objects::error::HeddleError::Io(e)))
    }
}

fn default_config() -> Config {
    // Read-write: writes flow through `Filesystem::write` into the
    // core's hot tier, promote to CAS on `flush`/`release`, and get
    // folded into a state by `capture`.
    //
    // `Config` is `#[non_exhaustive]` so we mutate a `Default` value
    // instead of constructing fields directly — that keeps us
    // forward-compatible with future Config additions.
    //
    // We deliberately do *not* set `AutoUnmount`: fuser 0.17 rejects
    // `AutoUnmount` unless the session ACL is `AllowOther` or
    // `AllowRoot`, which in turn requires `user_allow_other` in
    // `/etc/fuse.conf` on most distros. That's a host-side gate we
    // can't assume — and for heddle's single-user-mount model
    // `Owner`-scoped ACLs are correct anyway. Clean unmount on
    // `BackgroundSession::drop` is the real safety net (see
    // [`crate::fuse::FuseShell::mount_background`]); for the
    // `kill -9 heddled` case operators run `fusermount3 -u
    // <mountpoint>` to clear the stale mount.
    //
    // See `crates/mount/README.md` for the full operational note.
    //
    // We also deliberately do *not* set `DefaultPermissions`. That
    // option tells the kernel to enforce the unix-mode bits we hand
    // back in `getattr` against the caller's uid/gid. Heddle mounts
    // are single-user (the default `Owner` ACL already gates the
    // mountpoint at the kernel boundary — only the mount-owner and
    // root can `open(2)` anything underneath), so a second layer of
    // mode-based checks adds nothing and *blocks* writes: the
    // mount's captured-tree files report `mode 0644 uid 0 gid 0`,
    // and a non-root mount-owner would fail the kernel's permission
    // check with `EACCES`. Letting the FUSE-side
    // [`PlatformShell::write`] decide what's permitted matches the
    // FSKit / ProjFS shells and the daily-use shape we want.
    let mut config = Config::default();
    config.mount_options = vec![MountOption::FSName("heddle-mount".into())];
    config
}

/// The uid/gid the shell reports for every node it serves. Resolved
/// once per process: heddle mounts are single-user and the mount
/// owner is by definition the process owner. Reporting the caller's
/// actual uid keeps `ls -l` from showing `0 0` for every file
/// (cosmetic, but the kind of cosmetic that has tripped operators
/// expecting "owned by me" semantics).
fn process_uid() -> u32 {
    // SAFETY: `getuid` is async-signal-safe and has no preconditions.
    unsafe { libc::getuid() }
}

fn process_gid() -> u32 {
    // SAFETY: `getgid` is async-signal-safe and has no preconditions.
    unsafe { libc::getgid() }
}

/// Fold the unix mode bits the kernel passes to `create` / `mknod`
/// into the closest [`FileMode`] heddle tracks. Only the `+x` bit
/// is preserved across capture; the rest of the permission bits
/// don't survive (see `crates/objects/src/object/tree_types.rs` —
/// FileMode is a three-way enum, not a u16). Document this in the
/// README's "what writes persist" section.
fn file_mode_from_unix(mode: u32) -> FileMode {
    if (mode & 0o111) != 0 {
        FileMode::Executable
    } else {
        FileMode::Normal
    }
}

fn file_type_for_kind(kind: NodeKind) -> FileType {
    match kind {
        NodeKind::Directory => FileType::Directory,
        NodeKind::File => FileType::RegularFile,
        NodeKind::Symlink => FileType::Symlink,
    }
}

fn file_attr_from(attrs: Attrs) -> FileAttr {
    let kind = file_type_for_kind(attrs.kind);
    FileAttr {
        ino: INodeNo(attrs.node.0),
        size: attrs.size,
        blocks: attrs.size.div_ceil(512),
        atime: attrs.mtime,
        mtime: attrs.mtime,
        ctime: attrs.mtime,
        crtime: attrs.mtime,
        kind,
        // The `unix_mode` we store includes the type bits; FUSE wants
        // just the permission bits in `perm`.
        perm: (attrs.unix_mode & 0o7777) as u16,
        nlink: attrs.nlink,
        uid: process_uid(),
        gid: process_gid(),
        rdev: 0,
        blksize: 4096,
        flags: 0,
    }
}

fn entry_attr_from(entry: &Entry, mtime: std::time::SystemTime) -> FileAttr {
    FileAttr {
        ino: INodeNo(entry.node.0),
        size: entry.size,
        blocks: entry.size.div_ceil(512),
        atime: mtime,
        mtime,
        ctime: mtime,
        crtime: mtime,
        kind: file_type_for_kind(entry.kind),
        perm: (entry.unix_mode & 0o7777) as u16,
        nlink: 1,
        uid: process_uid(),
        gid: process_gid(),
        rdev: 0,
        blksize: 4096,
        flags: 0,
    }
}

/// Convert a `MountError`'s errno back into the `Errno` newtype that
/// fuser 0.17's `reply.error()` requires. `MountError::to_errno()`
/// returns the raw `i32` so the rest of the crate stays
/// platform-neutral; we only do the wrap at the FUSE boundary.
fn errno_from_mount_error(err: crate::error::MountError) -> Errno {
    Errno::from_i32(err.to_errno())
}

/// Run `f` and translate the outcome into either the trait result or
/// a kernel-replied errno. Catches panics so a buggy inner call can't
/// kill the FUSE worker (which would wedge every userspace process
/// holding the mount) or — worse, post Rust 1.81 — abort the whole
/// daemon process across an `extern "C"` frame inside `fuser`.
///
/// `AssertUnwindSafe` is sound here: the closure borrows only the
/// `Arc<ContentAddressedMount>` (whose interior is `Mutex` / `RwLock`-
/// guarded and tolerates poisoning at construction sites), and any
/// outparams it would write live behind the `Reply*` types — which
/// we deliberately do not touch on the error path, leaving them to
/// the single `reply.error(...)` below.
fn guard_call<T>(label: &'static str, f: impl FnOnce() -> Result<T>) -> Result<T> {
    match std::panic::catch_unwind(AssertUnwindSafe(f)) {
        Ok(result) => result,
        Err(payload) => {
            let msg = panic_payload_str(&payload);
            tracing::error!(callback = label, %msg, "FUSE callback panicked; returning EIO");
            Err(crate::error::MountError::Store(
                objects::error::HeddleError::Io(std::io::Error::other(format!(
                    "panic in FUSE {label}: {msg}"
                ))),
            ))
        }
    }
}

fn panic_payload_str(payload: &Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = payload.downcast_ref::<&'static str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "<non-string panic payload>".to_string()
    }
}

impl Filesystem for FuseShell {
    fn init(&mut self, _req: &Request, config: &mut KernelConfig) -> std::io::Result<()> {
        // Opt into `FUSE_DIRECT_IO_ALLOW_MMAP`. We hand back
        // `FOPEN_DIRECT_IO` from `open` (see the comment there), and
        // under default kernel semantics that disables shared `mmap`
        // on every fd — calls to `mmap(MAP_SHARED, ...)` return
        // `ENODEV`. The kernel cap added in 5.16 keeps direct-IO
        // bypass for `read(2)`/`write(2)` but re-enables shared mmap,
        // which is the daily-use path for rust-analyzer, cargo, IDEs,
        // and `grep --mmap` on heddle-mounted trees.
        //
        // `add_capabilities` errors when the kernel is < 5.16 (the
        // bit wasn't defined). That's not fatal — older kernels just
        // get the pre-cap behaviour (no shared mmap), which is the
        // same shape r1 of this fix shipped with. Log it once at
        // mount time so operators on old kernels can correlate
        // `ENODEV` from a mapping syscall with the missing cap.
        if let Err(unsupported) = config.add_capabilities(InitFlags::FUSE_DIRECT_IO_ALLOW_MMAP) {
            debug!(
                ?unsupported,
                "kernel does not support FUSE_DIRECT_IO_ALLOW_MMAP (requires 5.16+); \
                 shared mmap on mounted files will fail with ENODEV"
            );
        }
        Ok(())
    }

    fn open(&self, _req: &Request, _ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
        // Open every file in `direct_io` mode so the kernel never
        // serves bytes from its page cache. The content-addressed
        // mount maintains its own blob cache ([`BlobCachePool`]),
        // which already deduplicates repeated reads against the
        // captured tree — so the kernel-side page cache wins us
        // nothing, and *costs* correctness on the write-then-read
        // path:
        //
        // Without `direct_io`, after a captured file is opened-for-
        // write, mutated, closed (→ `flush` promotes the hot tier
        // into the warm tier), and reopened, the kernel happily
        // serves the *pre-write* bytes from its page cache. The
        // dentry/inode caching at our 1-second TTL doesn't help —
        // the page cache is keyed off the kernel-side inode, and
        // the kernel has no way to know we replaced the blob behind
        // it. `direct_io` short-circuits the page cache entirely;
        // every kernel `read(2)` becomes a FUSE `read` callback,
        // which we serve from the hot-tier-then-warm-tier-then-
        // captured-blob priority chain.
        //
        // The kernel's default rule for direct-IO files is "no shared
        // mmap" (the page cache is the mapping substrate; bypass it
        // and `mmap(MAP_SHARED, ...)` returns `ENODEV`). We opt out
        // of that rule by requesting `FUSE_DIRECT_IO_ALLOW_MMAP` in
        // [`Self::init`] — on Linux 5.16+ the kernel keeps the
        // direct-IO bypass for `read`/`write` but lets shared mmap
        // through, which is the daily-use path for rust-analyzer,
        // cargo, IDEs, and `grep --mmap` on heddle-mounted trees.
        //
        // FH=0 mirrors the fuser default (we don't track per-handle
        // state — open files identify by inode in [`PlatformShell`]).
        reply.opened(FileHandle(0), FopenFlags::FOPEN_DIRECT_IO);
    }

    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        let result = guard_call("lookup", || {
            let entry = self.inner.lookup(NodeId(parent.0), name)?;
            match entry {
                None => Ok(None),
                Some(entry) => {
                    // Ask the core for canonical attrs (it already loaded
                    // the blob in `lookup`, so this is a hash-table hit).
                    let mtime = self
                        .inner
                        .attrs(entry.node)
                        .map(|a| a.mtime)
                        .unwrap_or(UNIX_EPOCH);
                    Ok(Some(entry_attr_from(&entry, mtime)))
                }
            }
        });
        match result {
            Ok(Some(attr)) => reply.entry(&TTL, &attr, GENERATION),
            Ok(None) => reply.error(Errno::ENOENT),
            Err(err) => reply.error(errno_from_mount_error(err)),
        }
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        match guard_call("getattr", || self.inner.attrs(NodeId(ino.0))) {
            Ok(attrs) => reply.attr(&TTL, &file_attr_from(attrs)),
            Err(err) => reply.error(errno_from_mount_error(err)),
        }
    }

    fn read(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        size: u32,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyData,
    ) {
        let result = guard_call("read", || {
            let mut buf = vec![0u8; size as usize];
            let n = self.inner.read(NodeId(ino.0), offset, &mut buf)?;
            buf.truncate(n);
            Ok(buf)
        });
        match result {
            Ok(buf) => reply.data(&buf),
            Err(err) => reply.error(errno_from_mount_error(err)),
        }
    }

    fn readdir(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        // Precompute the full vec inside the panic guard. The `reply.add`
        // loop below is infallible and can't panic — leaving it outside
        // the guard means a torn buffer state on partial-fill never
        // matters.
        let prepared = guard_call("readdir", || {
            let entries = self.inner.enumerate(NodeId(ino.0))?;
            let mut all: Vec<(u64, FileType, std::ffi::OsString)> =
                Vec::with_capacity(entries.len() + 2);
            all.push((ino.0, FileType::Directory, ".".into()));
            all.push((ino.0, FileType::Directory, "..".into()));
            for entry in entries {
                all.push((entry.node.0, file_type_for_kind(entry.kind), entry.name));
            }
            Ok(all)
        });
        let all = match prepared {
            Ok(v) => v,
            Err(err) => {
                reply.error(errno_from_mount_error(err));
                return;
            }
        };

        // FUSE expects `.` and `..` first (already prepended), then the
        // actual entries. `offset` is opaque-but-monotonic; we use
        // `index+1` as the next-offset cookie, which is the standard
        // recipe.
        for (i, (child_ino, kind, name)) in all.into_iter().enumerate().skip(offset as usize) {
            let next_offset = (i + 1) as u64;
            if reply.add(INodeNo(child_ino), next_offset, kind, &name) {
                // Buffer full — kernel will call us again with the
                // last-returned offset.
                break;
            }
        }
        reply.ok();
    }

    fn write(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        data: &[u8],
        _write_flags: WriteFlags,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyWrite,
    ) {
        match guard_call("write", || self.inner.write(NodeId(ino.0), offset, data)) {
            Ok(n) => reply.written(n as u32),
            Err(err) => reply.error(errno_from_mount_error(err)),
        }
    }

    fn flush(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        _lock_owner: LockOwner,
        reply: ReplyEmpty,
    ) {
        // Flush fires on `close(2)` from userspace. This is the
        // natural place to promote the hot buffer to CAS.
        match guard_call("flush", || self.inner.flush(NodeId(ino.0))) {
            Ok(()) => reply.ok(),
            Err(err) => reply.error(errno_from_mount_error(err)),
        }
    }

    fn release(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        // Belt-and-braces: a process that exits without an explicit
        // close still gets a release on the inode. Promote any
        // surviving buffer.
        match guard_call("release", || self.inner.release(NodeId(ino.0))) {
            Ok(()) => reply.ok(),
            Err(err) => reply.error(errno_from_mount_error(err)),
        }
    }

    fn create(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        _umask: u32,
        flags: i32,
        reply: ReplyCreate,
    ) {
        // The kernel calls `create` for `open(O_CREAT)`. `O_EXCL`
        // requires us to fail with EEXIST when the entry already
        // exists; without it we return-or-create.
        let exclusive = (flags & libc::O_EXCL) != 0;
        let file_mode = file_mode_from_unix(mode);
        let result = guard_call("create", || {
            self.inner
                .create_file(NodeId(parent.0), name, file_mode, exclusive)
        });
        match result {
            Ok(entry) => {
                // Mirror the `open` callback: opt the new fd into
                // FOPEN_DIRECT_IO so kernel page-cache reads don't
                // shadow hot-tier writes (see `Self::open` for the
                // full reasoning).
                let attr = match guard_call("create", || self.inner.attrs(entry.node)) {
                    Ok(attrs) => file_attr_from(attrs),
                    Err(err) => {
                        reply.error(errno_from_mount_error(err));
                        return;
                    }
                };
                reply.created(
                    &TTL,
                    &attr,
                    GENERATION,
                    FileHandle(0),
                    FopenFlags::FOPEN_DIRECT_IO,
                );
            }
            Err(err) => reply.error(errno_from_mount_error(err)),
        }
    }

    fn mkdir(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        reply: ReplyEntry,
    ) {
        let result = guard_call("mkdir", || self.inner.make_dir(NodeId(parent.0), name));
        match result {
            Ok(entry) => {
                let attr = entry_attr_from(&entry, self.inner_mtime());
                reply.entry(&TTL, &attr, GENERATION);
            }
            Err(err) => reply.error(errno_from_mount_error(err)),
        }
    }

    /// `mknod` for regular files routes through `create_file`. Heddle
    /// doesn't model device / FIFO / socket nodes — those return
    /// `EPERM`, which is what fuse's default mknod handler also does
    /// for the unsupported types. cargo / git / npm only ever issue
    /// `mknod` with `S_IFREG`, so the supported subset is enough for
    /// the issue's acceptance criteria. See README ("per-thread
    /// overlay semantics") for the full enumeration.
    fn mknod(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        _umask: u32,
        _rdev: u32,
        reply: ReplyEntry,
    ) {
        let kind = mode & libc::S_IFMT;
        if kind != libc::S_IFREG && kind != 0 {
            reply.error(Errno::from_i32(libc::EPERM));
            return;
        }
        let file_mode = file_mode_from_unix(mode);
        let result = guard_call("mknod", || {
            self.inner.create_file(NodeId(parent.0), name, file_mode, true)
        });
        match result {
            Ok(entry) => {
                let attr = entry_attr_from(&entry, self.inner_mtime());
                reply.entry(&TTL, &attr, GENERATION);
            }
            Err(err) => reply.error(errno_from_mount_error(err)),
        }
    }

    fn unlink(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        match guard_call("unlink", || {
            self.inner.unlink_entry(NodeId(parent.0), name)
        }) {
            Ok(()) => reply.ok(),
            Err(err) => reply.error(errno_from_mount_error(err)),
        }
    }

    fn rmdir(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        match guard_call("rmdir", || {
            self.inner.rmdir_entry(NodeId(parent.0), name)
        }) {
            Ok(()) => reply.ok(),
            Err(err) => reply.error(errno_from_mount_error(err)),
        }
    }

    fn rename(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        newparent: INodeNo,
        newname: &OsStr,
        flags: RenameFlags,
        reply: ReplyEmpty,
    ) {
        // `renameat2` flags we don't support yet:
        //   * RENAME_EXCHANGE — needs an atomic swap primitive; the
        //     overlay would have to journal both halves.
        //   * RENAME_WHITEOUT — overlayfs-specific, not meaningful
        //     for a CAS-backed mount.
        // `RENAME_NOREPLACE` we honour the easy way: if the
        // destination exists, refuse with EEXIST before invoking
        // the overlay rename.
        #[cfg(target_os = "linux")]
        if flags.contains(RenameFlags::RENAME_EXCHANGE)
            || flags.contains(RenameFlags::RENAME_WHITEOUT)
        {
            reply.error(Errno::from_i32(libc::EINVAL));
            return;
        }
        #[cfg(target_os = "linux")]
        if flags.contains(RenameFlags::RENAME_NOREPLACE) {
            let probe = guard_call("rename", || {
                self.inner.lookup(NodeId(newparent.0), newname)
            });
            match probe {
                Ok(Some(_)) => {
                    reply.error(Errno::from_i32(libc::EEXIST));
                    return;
                }
                Ok(None) => {}
                Err(err) => {
                    reply.error(errno_from_mount_error(err));
                    return;
                }
            }
        }
        #[cfg(not(target_os = "linux"))]
        let _ = flags;
        match guard_call("rename", || {
            self.inner.rename_entry(
                NodeId(parent.0),
                name,
                NodeId(newparent.0),
                newname,
            )
        }) {
            Ok(()) => reply.ok(),
            Err(err) => reply.error(errno_from_mount_error(err)),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn setattr(
        &self,
        _req: &Request,
        ino: INodeNo,
        mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
        size: Option<u64>,
        _atime: Option<TimeOrNow>,
        mtime: Option<TimeOrNow>,
        _ctime: Option<SystemTime>,
        _fh: Option<FileHandle>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<BsdFileFlags>,
        reply: ReplyAttr,
    ) {
        let mtime_sec = mtime.and_then(|t| match t {
            TimeOrNow::SpecificTime(st) => st
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .ok(),
            TimeOrNow::Now => SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .ok(),
        });
        let update = AttrUpdate {
            mode,
            uid,
            gid,
            size,
            mtime_sec,
        };
        match guard_call("setattr", || self.inner.set_attrs(NodeId(ino.0), update)) {
            Ok(attrs) => reply.attr(&TTL, &file_attr_from(attrs)),
            Err(err) => reply.error(errno_from_mount_error(err)),
        }
    }

    fn symlink(
        &self,
        _req: &Request,
        parent: INodeNo,
        link_name: &OsStr,
        target: &Path,
        reply: ReplyEntry,
    ) {
        let result = guard_call("symlink", || {
            self.inner.create_symlink(NodeId(parent.0), link_name, target)
        });
        match result {
            Ok(entry) => {
                let attr = entry_attr_from(&entry, self.inner_mtime());
                reply.entry(&TTL, &attr, GENERATION);
            }
            Err(err) => reply.error(errno_from_mount_error(err)),
        }
    }

    fn readlink(&self, _req: &Request, ino: INodeNo, reply: ReplyData) {
        use std::os::unix::ffi::OsStrExt;
        match guard_call("readlink", || self.inner.read_link(NodeId(ino.0))) {
            Ok(target) => reply.data(target.as_os_str().as_bytes()),
            Err(err) => reply.error(errno_from_mount_error(err)),
        }
    }

    /// Hard links would alias two paths onto the same inode; the
    /// per-thread CAS overlay (and heddle's tree model) addresses
    /// blobs by content-hash but identifies *entries* by path, with
    /// no nlink fan-out. Refuse with `EPERM` to match POSIX's
    /// behaviour for filesystems that don't support hard links. The
    /// fuser default already returns `EPERM`; we override only to
    /// route through the same panic-guard wrapper for consistency
    /// with the other write-side callbacks.
    fn link(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _newparent: INodeNo,
        _newname: &OsStr,
        reply: ReplyEntry,
    ) {
        reply.error(Errno::from_i32(libc::EPERM));
    }

    fn destroy(&mut self) {
        // Surface a cheap log line so debugging unmount-during-test
        // hangs is easier. No-op otherwise.
        warn!(
            thread = %self.inner.thread(),
            "fuse mount destroyed"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::mocks::PanicShell;

    /// A panic in `guard_call`'s closure must translate to an `Err`
    /// with a usable errno, not unwind out of the callback. The FUSE
    /// dispatcher relies on this so a poisoned mutex deep in the core
    /// can't either (a) kill the fuser worker thread (wedging every
    /// userspace process holding the mount) or (b) abort the daemon
    /// process via Rust ≥1.81's "no unwind across extern C" rule —
    /// `fuser` has extern C shims internally on the reply path.
    ///
    /// We exercise `guard_call` directly rather than driving the full
    /// `Filesystem::lookup` path because constructing a fuser
    /// `ReplyEntry` requires a private channel handle. The translation
    /// is the part that needs locking down; the trait dispatch is
    /// straight-line code around it.
    #[test]
    fn guard_call_translates_panic_to_eio() {
        let result: Result<()> = guard_call("test", || {
            // Panic from the same shape of bug a real shell could
            // raise — a poisoned mutex unwrap inside core dispatch.
            panic!("simulated mutex poison");
        });
        let err = result.expect_err("expected guard_call to return Err on panic");
        assert_eq!(
            err.to_errno(),
            libc::EIO,
            "panic must translate to EIO, got errno {} ({err})",
            err.to_errno()
        );
    }

    /// And the happy path: a successful inner call passes through
    /// unchanged.
    #[test]
    fn guard_call_passes_through_ok() {
        let result: Result<i32> = guard_call("test", || Ok(42));
        assert_eq!(result.expect("ok"), 42);
    }

    /// A `PlatformShell` that panics on every operation must surface
    /// as `Err(MountError)` with errno `EIO` when driven through
    /// `guard_call`. This is the FUSE-side analogue of the FSKit
    /// `trampoline_lookup_recovers_eio_on_panic` test and locks in
    /// cross-platform parity: a future change to either shell that
    /// breaks panic recovery must trip one of these two tests.
    #[test]
    fn panic_shell_dispatch_yields_eio() {
        let shell = Arc::new(PanicShell) as Arc<dyn PlatformShell + Send + Sync>;
        let result: Result<usize> = guard_call("read", || shell.read(NodeId(1), 0, &mut [0u8; 4]));
        let err = result.expect_err("expected PanicShell to panic");
        assert_eq!(err.to_errno(), libc::EIO);
    }
}
