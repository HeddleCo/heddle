// SPDX-License-Identifier: Apache-2.0
//! Linux FUSE shell built on `fuser`.
//!
//! The shell is a thin adapter: every callback either translates
//! arguments and dispatches to a [`PlatformShell`], or replies with
//! the errno from a [`MountError`]. The mount is read-write: writes
//! are buffered in the core's hot tier, promoted to CAS on `flush`
//! /`release`, and folded into a real heddle state by
//! [`ContentAddressedMount::capture`].

use std::{
    ffi::OsStr,
    path::Path,
    sync::Arc,
    time::{Duration, UNIX_EPOCH},
};

use fuser::{
    BackgroundSession, Config, Errno, FileAttr, FileHandle, FileType, Filesystem, Generation,
    INodeNo, LockOwner, MountOption, OpenFlags, ReplyAttr, ReplyData, ReplyDirectory, ReplyEmpty,
    ReplyEntry, ReplyWrite, Request, Session, WriteFlags,
};
use tracing::warn;

use crate::{
    core::ContentAddressedMount,
    error::Result,
    shell::{Attrs, Entry, NodeId, NodeKind, PlatformShell},
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
    let mut config = Config::default();
    config.mount_options = vec![
        MountOption::FSName("heddle-mount".into()),
        MountOption::AutoUnmount,
        MountOption::DefaultPermissions,
    ];
    config
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
        uid: 0,
        gid: 0,
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
        uid: 0,
        gid: 0,
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

impl Filesystem for FuseShell {
    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        match self.inner.lookup(NodeId(parent.0), name) {
            Ok(Some(entry)) => {
                // Ask the core for canonical attrs (it already loaded
                // the blob in `lookup`, so this is a hash-table hit).
                let mtime = match self.inner.attrs(entry.node) {
                    Ok(a) => a.mtime,
                    Err(_) => UNIX_EPOCH,
                };
                let attr = entry_attr_from(&entry, mtime);
                reply.entry(&TTL, &attr, GENERATION);
            }
            Ok(None) => reply.error(Errno::ENOENT),
            Err(err) => reply.error(errno_from_mount_error(err)),
        }
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        match self.inner.attrs(NodeId(ino.0)) {
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
        let mut buf = vec![0u8; size as usize];
        match self.inner.read(NodeId(ino.0), offset, &mut buf) {
            Ok(n) => reply.data(&buf[..n]),
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
        let entries = match self.inner.enumerate(NodeId(ino.0)) {
            Ok(e) => e,
            Err(err) => {
                reply.error(errno_from_mount_error(err));
                return;
            }
        };

        // FUSE expects `.` and `..` first, then the actual entries.
        // The `offset` is opaque-but-monotonic; we just use index+1
        // as the next-offset cookie, which is the standard recipe.
        let mut all: Vec<(u64, FileType, std::ffi::OsString)> =
            Vec::with_capacity(entries.len() + 2);
        all.push((ino.0, FileType::Directory, ".".into()));
        all.push((ino.0, FileType::Directory, "..".into()));
        for entry in entries {
            all.push((entry.node.0, file_type_for_kind(entry.kind), entry.name));
        }

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
        match self.inner.write(NodeId(ino.0), offset, data) {
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
        match self.inner.flush(NodeId(ino.0)) {
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
        match self.inner.release(NodeId(ino.0)) {
            Ok(()) => reply.ok(),
            Err(err) => reply.error(errno_from_mount_error(err)),
        }
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