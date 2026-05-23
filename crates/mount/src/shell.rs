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
    path::Path,
    time::SystemTime,
};

use objects::object::FileMode;

use crate::error::{MountError, Result};

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
    ///
    /// Lifecycle note: FUSE `flush` fires on *every* descriptor close
    /// — including the close of a `dup`-derived fd — so it can be
    /// invoked multiple times before the last open handle is gone.
    /// Implementations that maintain per-inode "is the directory
    /// entry still gone?" state (orphan tracking) MUST defer the
    /// final clear to [`Self::release`]; touching it here would let a
    /// surviving fd's next write republish the unlinked pathname.
    fn flush(&self, _node: NodeId) -> Result<()> {
        Ok(())
    }

    /// Final close of `node`. The FUSE `release` callback dispatches
    /// here; it fires once per `open(2)` after the last fd derived
    /// from that open is closed. This is the canonical "last close of
    /// the inode" signal — it is the right hook (NOT [`Self::flush`])
    /// for retiring per-inode lifecycle state like orphan-tracking
    /// markers or open-handle refcounts. Default: identical to flush
    /// so shells that do not maintain per-inode lifecycle state
    /// inherit a uniform contract.
    fn release(&self, node: NodeId) -> Result<()> {
        self.flush(node)
    }

    /// Notify the shell that a new open file handle for `node` has
    /// been minted. FUSE adapters call this on the `open` / `create`
    /// callbacks so the shell can maintain a per-inode open-handle
    /// refcount — used to time the [`Self::release`] cleanup against
    /// the *final* close instead of the first one. Default: no-op so
    /// shells without lifecycle state are unaffected.
    fn on_open(&self, _node: NodeId) -> Result<()> {
        Ok(())
    }

    /// Create a fresh regular file under `parent`. Mints a [`NodeId`]
    /// for the new file in the writable overlay and returns its
    /// [`Entry`]; subsequent [`write`](PlatformShell::write) calls
    /// land in the per-thread hot tier.
    ///
    /// When `exclusive` is true (`O_CREAT|O_EXCL`), the call must
    /// fail with [`MountError::AlreadyExists`] if `name` already
    /// resolves under `parent` (either in the captured tree or the
    /// pending tier). When `exclusive` is false, a hit on an
    /// existing entry is returned as-is (same shape as `lookup`).
    ///
    /// Default: [`MountError::ReadOnly`] — implementations that
    /// don't support mutation inherit a uniform errno.
    fn create_file(
        &self,
        _parent: NodeId,
        _name: &OsStr,
        _mode: FileMode,
        _exclusive: bool,
    ) -> Result<Entry> {
        Err(MountError::ReadOnly)
    }

    /// Create an empty directory under `parent` in the overlay.
    /// Returns the new directory's [`Entry`]. Fails with
    /// [`MountError::AlreadyExists`] when `name` already resolves.
    fn make_dir(&self, _parent: NodeId, _name: &OsStr) -> Result<Entry> {
        Err(MountError::ReadOnly)
    }

    /// Delete the file named `name` under `parent`. The captured-tree
    /// entry (if any) is tombstoned so [`lookup`](Self::lookup) /
    /// [`enumerate`](Self::enumerate) skip it; any pending-tier hot
    /// buffer or warm blob for the path is dropped.
    ///
    /// Fails with [`MountError::NotFound`] if `name` doesn't resolve,
    /// or [`MountError::IsADirectory`] if it resolves to a directory.
    fn unlink_entry(&self, _parent: NodeId, _name: &OsStr) -> Result<()> {
        Err(MountError::ReadOnly)
    }

    /// Remove the empty directory named `name` under `parent`. Fails
    /// with [`MountError::NotADirectory`] for a file, with
    /// [`MountError::NotEmpty`] when the directory still has visible
    /// children (across captured tree + pending tier), or
    /// [`MountError::NotFound`] when nothing resolves.
    fn rmdir_entry(&self, _parent: NodeId, _name: &OsStr) -> Result<()> {
        Err(MountError::ReadOnly)
    }

    /// Atomically rename `(old_parent, old_name)` to
    /// `(new_parent, new_name)`. Handles both same-directory and
    /// cross-directory cases. Replacing an existing entry of the
    /// same kind is allowed (POSIX semantics); replacing a directory
    /// with a file (or vice-versa) fails with
    /// [`MountError::IsADirectory`] / [`MountError::NotADirectory`].
    fn rename_entry(
        &self,
        _old_parent: NodeId,
        _old_name: &OsStr,
        _new_parent: NodeId,
        _new_name: &OsStr,
    ) -> Result<()> {
        Err(MountError::ReadOnly)
    }

    /// Same as [`Self::rename_entry`] but honours [`RenameOptions`] —
    /// in particular `no_replace`, which atomically refuses the rename
    /// when the destination already resolves. The check + the
    /// directory-entry mutation MUST happen under a single critical
    /// section to avoid a TOCTOU window between the existence check
    /// and the rename itself. Default: ignore options and dispatch to
    /// `rename_entry` (preserving the existing trait surface for
    /// shells that do not yet support flags).
    fn rename_entry_with_options(
        &self,
        old_parent: NodeId,
        old_name: &OsStr,
        new_parent: NodeId,
        new_name: &OsStr,
        _options: RenameOptions,
    ) -> Result<()> {
        self.rename_entry(old_parent, old_name, new_parent, new_name)
    }

    /// Apply attribute updates to `node`. Returns the post-update
    /// [`Attrs`] so callers can reply without a second `getattr`
    /// round trip. See [`AttrUpdate`] for which fields the overlay
    /// actually persists; unsupported fields are no-ops.
    fn set_attrs(&self, _node: NodeId, _update: AttrUpdate) -> Result<Attrs> {
        Err(MountError::ReadOnly)
    }

    /// Create a symbolic link named `name` under `parent` whose
    /// target is the byte-equivalent of `target`. Returns the new
    /// link's [`Entry`].
    fn create_symlink(
        &self,
        _parent: NodeId,
        _name: &OsStr,
        _target: &Path,
    ) -> Result<Entry> {
        Err(MountError::ReadOnly)
    }

    /// Read the target of a symbolic link `node`. Returns the raw
    /// bytes of the link target (which may not be valid UTF-8 on
    /// some systems, hence [`OsString`]).
    fn read_link(&self, _node: NodeId) -> Result<OsString> {
        Err(MountError::ReadOnly)
    }
}

/// Optional fields a caller may update via
/// [`PlatformShell::set_attrs`]. Every field is `Option<_>`; `None`
/// means "leave alone" (the kernel passes `None` for slots the
/// `chmod`/`chown`/`truncate`/`utimensat` call didn't touch).
///
/// Heddle's tree model only carries three modes ([`FileMode::Normal`],
/// [`FileMode::Executable`], [`FileMode::Symlink`]) — see
/// `crates/objects/src/object/tree_types.rs`. A `chmod` that flips
/// the user-executable bit (`0o100`) maps to the closest mode; bits
/// outside that don't persist across `capture`.
#[derive(Clone, Copy, Debug, Default)]
pub struct AttrUpdate {
    /// New unix mode bits (including the type bits). When set, the
    /// shell folds the user-executable bit into the captured
    /// [`FileMode`]; other bits don't persist.
    pub mode: Option<u32>,
    /// New uid. The mount has no per-node uid storage (every node
    /// reports the mount-owner's uid); shells may accept this as a
    /// no-op so `chown` doesn't return an error to callers that
    /// don't actually need ownership tracking.
    pub uid: Option<u32>,
    /// New gid. Same no-op contract as `uid`.
    pub gid: Option<u32>,
    /// New size. Truncates the hot-tier buffer (or seeds one from
    /// the durable predecessor and truncates) when set. `O_TRUNC`
    /// on the kernel side delivers `setattr(size=0)` before the
    /// first `write`.
    pub size: Option<u64>,
    /// New mtime in seconds since the UNIX epoch. The overlay has
    /// no per-node mtime storage today; shells accept this as a
    /// no-op so the kernel's `utimensat` doesn't return an error.
    pub mtime_sec: Option<i64>,
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

/// Optional flags for [`PlatformShell::rename_entry_with_options`].
/// Mirrors the subset of Linux `renameat2(2)` flags the mount
/// supports; non-applicable flags on non-Linux adapters can be left
/// as their defaults.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RenameOptions {
    /// `RENAME_NOREPLACE`: refuse the rename with [`MountError::AlreadyExists`]
    /// when the destination already resolves. Must be enforced inside
    /// the same critical section as the rename so a concurrent writer
    /// cannot install the destination between the check and the
    /// mutation.
    pub no_replace: bool,
}
