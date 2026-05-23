// SPDX-License-Identifier: Apache-2.0
//! Platform-agnostic shell trait.
//!
//! [`PlatformShell`] is the seam where a thin per-platform adapter
//! (FUSE on Linux, FSKit on macOS, ProjFS / CfAPI on Windows) plugs
//! into the content-addressed core. The core implements this trait
//! once, and each platform binding wraps it.
//!
//! Conceptually the trait is six pure operations: lookup, read,
//! write, enumerate, attrs, invalidate. They mirror what every
//! kernel-side filesystem hook ultimately needs to ask, so they can
//! be implemented for an in-memory test mount, a Git-backed mount,
//! a Heddle-state-backed mount, etc.

use std::{
    ffi::{OsStr, OsString},
    time::SystemTime,
};

use objects::object::FileMode;

use crate::error::Result;

/// Identifier for a filesystem node within a single mount session.
///
/// Reserved value `1` is the root, mirroring FUSE convention. Beyond
/// that, the core hands out opaque ids that are stable for the
/// lifetime of the mount but may be invalidated by [`PlatformShell::invalidate`]
/// when the underlying state moves.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct NodeId(pub u64);

impl NodeId {
    /// Root inode id. FUSE always starts here.
    pub const ROOT: NodeId = NodeId(1);
}

/// What a filesystem entry is, structurally.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NodeKind {
    Directory,
    File,
    Symlink,
}

/// A single directory entry, returned from [`PlatformShell::lookup`]
/// and [`PlatformShell::enumerate`].
#[derive(Clone, Debug)]
pub struct Entry {
    pub node: NodeId,
    pub name: OsString,
    pub kind: NodeKind,
    pub size: u64,
    /// Unix mode bits, including type. Cached so the platform shell
    /// can answer `attrs` without a second walk.
    pub unix_mode: u32,
}

/// Stat-style attributes for a single node.
#[derive(Clone, Copy, Debug)]
pub struct Attrs {
    pub node: NodeId,
    pub kind: NodeKind,
    pub size: u64,
    pub unix_mode: u32,
    pub nlink: u32,
    /// Modification / change times. The mount has no per-blob clock,
    /// so we report a single fixed timestamp captured when the mount
    /// was created. This keeps `ls -l` from showing nonsense and
    /// makes diffs against a stable reference deterministic.
    pub mtime: SystemTime,
}

/// Platform-agnostic operations every adapter implements against
/// a shared core. Names mirror the eventual FUSE callbacks (and the
/// equivalent FSKit / ProjFS hooks) so the platform layer can be
/// almost trivial.
///
/// ## Write lifecycle
///
/// Mount writes flow through three calls:
///
/// 1. [`write`](PlatformShell::write) — kernel issues a sequence of
///    `write(offset, bytes)` calls against an open file. The core
///    accumulates these in an in-memory hot-tier buffer keyed by
///    `NodeId`.
/// 2. [`flush`](PlatformShell::flush) — kernel signals the buffer
///    can be made durable (mapped to FUSE's `flush` callback, which
///    fires on `close(2)` and on explicit fsync). The core promotes
///    the hot buffer to a CAS blob and records `path -> blob_oid` in
///    the per-thread pending tree. Buffer is dropped.
/// 3. [`release`](PlatformShell::release) — kernel signals the file
///    is closed and the inode handle can be retired. The default
///    contract: identical to flush. FUSE doesn't always issue
///    `flush` cleanly on every close path, so adapters should call
///    `release` here too as a belt-and-braces measure.
///
/// Implementations MAY also promote a hot buffer opportunistically
/// (e.g. after an idle window) — this is a safety net for files that
/// the kernel never explicitly closes.
///
/// ## Platform notes
///
/// The three-call write lifecycle above describes the Linux/FUSE
/// path verbatim — `fuser` delivers each `write(2)` syscall as a
/// `write` callback, then `close(2)` triggers `flush` and `release`.
/// FSKit on macOS exposes the same per-write granularity.
///
/// On Windows, ProjFS does not intercept individual writes: after a
/// virtualized file is "hydrated" by the first read, subsequent
/// writes go straight to NTFS and ProjFS only notifies the provider
/// after the handle closes. The ProjFS adapter bridges this by
/// reading the now-fully-hydrated file at close time and synthesizing
/// a single `write(node, 0, full_contents)` + `flush(node)` against
/// this trait. The hot-tier per-write buffer is therefore a
/// Linux/FUSE (and FSKit) optimization — implementations of this
/// trait can rely on the buffer being non-empty only on platforms
/// that deliver per-write callbacks.
pub trait PlatformShell {
    /// Look up `name` inside `parent`. Returns `None` for ENOENT.
    fn lookup(&self, parent: NodeId, name: &OsStr) -> Result<Option<Entry>>;

    /// Read up to `buf.len()` bytes from `node`, starting at `offset`.
    /// Returns the number of bytes actually written into `buf`.
    fn read(&self, node: NodeId, offset: u64, buf: &mut [u8]) -> Result<usize>;

    /// Write `data` to `node` at `offset`. Returns bytes written.
    fn write(&self, node: NodeId, offset: u64, data: &[u8]) -> Result<usize>;

    /// List the children of `dir`.
    fn enumerate(&self, dir: NodeId) -> Result<Vec<Entry>>;

    /// Stat `node`.
    fn attrs(&self, node: NodeId) -> Result<Attrs>;

    /// Drop any cached identity for `node`. The platform layer calls
    /// this when the underlying state moves and previously-handed-out
    /// inode numbers may now point at the wrong content.
    fn invalidate(&self, node: NodeId) -> Result<()>;

    /// Promote any hot-tier buffer for `node` into a CAS blob. The
    /// FUSE `flush` callback dispatches here (fires on `close(2)`
    /// and explicit fsync). Default: no-op for read-only mounts.
    fn flush(&self, _node: NodeId) -> Result<()> {
        Ok(())
    }

    /// Final close of `node`. Adapters call this on FUSE `release`
    /// so a buffer that survived a missed `flush` still gets
    /// promoted before the inode handle is retired. Default:
    /// identical to flush.
    fn release(&self, node: NodeId) -> Result<()> {
        self.flush(node)
    }
}

/// Convert a Heddle [`FileMode`] into a node kind.
pub(crate) fn kind_for_mode(mode: FileMode) -> NodeKind {
    match mode {
        FileMode::Normal | FileMode::Executable => NodeKind::File,
        FileMode::Symlink => NodeKind::Symlink,
    }
}

/// The unix mode bits for a directory. Trees don't carry a mode of
/// their own — they're synthesised at materialization time — so we
/// keep one canonical value here.
pub(crate) const DIR_UNIX_MODE: u32 = 0o040755;
