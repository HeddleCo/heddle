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
//! ## Cache coherence — active invalidation (heddle#87)
//!
//! The mount lets the kernel cache page data, attrs, and dentries
//! (no `FOPEN_DIRECT_IO`), and keeps that cache coherent the way
//! FSKit does on macOS: by *actively pushing invalidations* to the
//! kernel the instant heddle mutates the backing content. Every
//! mutation flows through a FUSE callback in *this* shell, so the
//! invalidation is self-referential — the same callback that mutates
//! the overlay queues the matching `notify_inval_*` (dispatched off
//! the worker thread; see below).
//!
//! The notifier handle ([`fuser::Notifier`]) only exists *after* the
//! session is mounted (it wraps the `/dev/fuse` channel). [`FuseShell`]
//! therefore holds an `Arc<NotifyGate>` that [`Self::mount`] /
//! [`Self::mount_background`] *start* immediately after spawning the
//! session. Before the gate is started (and after an unmount), every
//! invalidate is a best-effort no-op; the un-started window only spans
//! the mount handshake, during which nothing the kernel has cached can
//! be stale.
//!
//! ### Off-thread dispatch (deadlock avoidance)
//!
//! Crucially, callbacks do **not** call the notifier inline. A
//! whole-inode invalidation makes the kernel acquire `inode->i_rwsem`
//! — a lock the in-flight `write`/`flush`/`setattr` callback is already
//! holding — so a synchronous notify from inside the callback
//! deadlocks the mount. Instead [`NotifyGate`] runs a dedicated
//! invalidator thread: callbacks `enqueue` the invalidation and return
//! (releasing the kernel lock), and the thread drains the queue and
//! issues the actual `notify_inval_*`. This matches how libfuse's own
//! `notify_inval_*` examples are structured (notify only off the
//! request loop). Invalidation is therefore *asynchronous* — exactly
//! like FSKit's `invalidateNode` — so a re-reader sees fresh content
//! once the queued notify lands, which is effectively immediate but
//! not synchronously ordered against the mutating syscall's return.
//!
//! ### Invalidation contract — which writes trigger which invals
//!
//! | callback              | mutation                         | invalidation |
//! |-----------------------|----------------------------------|--------------|
//! | `write`               | bytes into the hot tier          | `inval_inode(ino, 0, -1)` — drop cached pages + attrs for the file |
//! | `setattr`             | chmod / truncate / mtime         | `inval_inode(ino, 0, -1)` — attrs (and, on truncate, data) changed |
//! | `create` / `mknod`    | new file entry under `parent`    | `inval_entry(parent, name)` — refresh the now-stale negative/positive dentry |
//! | `mkdir`               | new dir entry under `parent`     | `inval_entry(parent, name)` |
//! | `symlink`             | new symlink entry under `parent` | `inval_entry(parent, name)` |
//! | `unlink` / `rmdir`    | remove entry under `parent`      | `inval_entry(parent, name)` — drop the cached dentry so a re-lookup 404s |
//! | `rename`              | move `src→dst`                   | `inval_entry(src_parent, src_name)` + `inval_entry(dst_parent, dst_name)` |
//!
//! `flush` / `release` deliberately do **not** invalidate: they promote
//! the hot tier into CAS but don't change the bytes the shell serves
//! (the content-changing `write` / `setattr` callbacks already
//! invalidated), and `flush` fires on *every* `close(2)` including a
//! reader's — invalidating there would throw away the cached-read win.
//!
//! `inval_inode(ino, 0, -1)` invalidates the *entire* data range plus
//! the cached attrs; we use the whole-file form because the overlay
//! re-materialises the blob wholesale on promotion rather than tracking
//! dirty byte ranges. `inval_entry` is best-effort: the kernel returns
//! `ENOENT` if it never cached the entry, which `fuser` swallows. All
//! invalidations are *advisory for performance, mandatory for
//! correctness* — a dropped notification (channel error) is logged but
//! cannot corrupt state, only risk a stale read until the attr TTL
//! lapses, so the callbacks never fail on an invalidation error.
//!
//! ## Implemented kernel callbacks
//!
//! Read path:
//! * `init` — default page-cache mode; no `FUSE_DIRECT_IO_ALLOW_MMAP`
//!   opt-in needed now that we don't return `FOPEN_DIRECT_IO`.
//! * `lookup` / `getattr` / `open` / `read` / `readdir` / `flush` /
//!   `release` / `opendir` / `releasedir` / `destroy`.
//!
//! Write path (heddle#180):
//! * `create` — `open(O_CREAT[|O_EXCL|O_TRUNC])`. EEXIST on O_EXCL
//!   clash. Fires `inval_entry(parent, name)`.
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
//! ## Process model — the `heddle-fuse-worker` subprocess (heddle#190)
//!
//! On Linux + `--features mount`, the CLI's mount lifecycle no
//! longer holds the FUSE session in-process. Instead it
//! `Command::new("heddle-fuse-worker").spawn()`s a small Linux-only
//! binary that owns the kernel-side mount and exchanges control-
//! plane messages with the supervisor (CLI today; daemon in the
//! follow-up tracked by spike heddle#88 §4) over an inherited Unix
//! socketpair. See `crates/mount/src/worker.rs` for the runtime
//! and `crates/mount/src/bin/heddle-fuse-worker.rs` for the binary's
//! `main` shim.
//!
//! Why subprocess: the panic guard documented below catches a
//! callback panic at the C ABI, but the panicking callback may have
//! already corrupted the *parent's* heap (poisoned mutex, partially
//! mutated cache) before the guard fired. Putting FUSE dispatch in
//! its own address space makes that class of bug impossible —
//! a panic in a FUSE callback aborts only the worker, the kernel
//! auto-unmounts when `/dev/fuse` closes, and the parent's heap
//! stays intact. The two red-commit tests in
//! `crates/mount/tests/fuse_worker_crash.rs` lock that contract in.
//!
//! ### IPC protocol — minimal-by-design
//!
//! The spike's locked decision is gRPC-over-UDS (heddle#88 §3) for
//! the *daemon-supervisor* shape. heddle#190 ships the
//! CLI-dispatched variant the issue AC requires — the daemon
//! follow-up adds gRPC + per-mount sockets — and a single
//! inherited socketpair carrying length-prefixed JSON frames is
//! the right shape for the CLI variant: no wire crate, no
//! `tokio` in the worker, no UDS discovery file. Each frame is a
//! [`worker::SupervisorCommand`] (parent → worker; today
//! `Stop` + `Status`, with `Capture` + `Invalidate` joining when
//! the daemon-side surface lands) or a [`worker::WorkerEvent`]
//! (worker → parent; `MountReady` / `MountError` / `StatusOk` /
//! `Stopping`). The framing is defined in
//! [`worker::framing`]; the wire format is u32 LE length followed
//! by the JSON body.
//!
//! [`worker::SupervisorCommand`]: crate::worker::SupervisorCommand
//! [`worker::WorkerEvent`]: crate::worker::WorkerEvent
//! [`worker::framing`]: crate::worker::framing
//!
//! ### What still runs in this process
//!
//! Everything in this file runs **inside the worker process** —
//! the [`Filesystem`] trait impl, the panic guard, the entire
//! callback dispatch surface, [`ContentAddressedMount::read`] /
//! `write` / `unlink` / etc. The supervisor (CLI / daemon) does
//! NOT see individual kernel callbacks; only the high-level
//! lifecycle events on the IPC socket. That keeps the per-syscall
//! cost at FUSE-native latency, which is the whole point of the
//! spike's "stateful worker" decision (heddle#88 §1 Decision B).
//!
//! ### Crash recovery — current state
//!
//! heddle#190 ships only the **propagate cleanly** half of the
//! crash story: a worker exit is observed by the supervisor's
//! watcher thread and surfaced via `tracing::warn`. The
//! 3-strikes RestartBudget + SCM_RIGHTS-respawn the spike's §5
//! describes lands with the daemon-supervisor follow-up; under
//! the current CLI dispatch a worker crash is a terminal mount
//! failure for that thread. See
//! `docs/design/fuse-worker-ipc-decision.md` §5–§7 for the
//! restart shape that the daemon will eventually implement.
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
    ffi::{OsStr, OsString},
    panic::AssertUnwindSafe,
    path::Path,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicU64, Ordering},
        mpsc::{Sender, channel},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use fuser::{
    BackgroundSession, BsdFileFlags, Config, Errno, FileAttr, FileHandle, FileType, Filesystem,
    FopenFlags, Generation, INodeNo, KernelConfig, LockOwner, MountOption, Notifier, OpenFlags,
    RenameFlags, ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory, ReplyEmpty, ReplyEntry,
    ReplyOpen, ReplyWrite, Request, Session, TimeOrNow, WriteFlags,
};
use objects::object::FileMode;
use tracing::{debug, warn};

use crate::{
    core::ContentAddressedMount,
    error::Result,
    shell::{AttrUpdate, Attrs, Entry, NodeId, NodeKind, PlatformShell, RenameOptions},
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

/// A single kernel-invalidation request, enqueued by a FUSE callback
/// and drained by the background invalidator thread.
enum InvalMsg {
    /// Drop cached pages + attrs for the whole inode.
    Inode(u64),
    /// Drop the cached dentry for `(parent, name)`.
    Entry(u64, OsString),
}

/// Dispatcher that pushes active cache invalidations to the kernel for
/// a live mount, **off the FUSE worker thread**.
///
/// ## Why a separate thread (the deadlock this avoids)
///
/// `fuse_lowlevel_notify_inval_inode` makes the kernel walk the inode's
/// page cache and acquire `inode->i_rwsem`. An in-flight `write` /
/// `flush` / `setattr` callback is *already holding* that lock when it
/// runs — so calling the notifier **synchronously from inside the
/// callback** is a classic AB-BA deadlock: the callback waits for the
/// notify ack, the kernel waits for the callback's lock. (Empirically:
/// the first cut of heddle#87 did exactly this and wedged the mount
/// hard, leaving zombie worker threads.) libfuse's own
/// `notify_inval_*` examples sidestep it by only ever notifying from a
/// thread that is *not* the request loop. We do the same: callbacks
/// `enqueue` a message and return immediately (releasing the kernel
/// lock); a dedicated thread drains the queue and issues the notifier
/// calls once the originating op has completed.
///
/// The [`Notifier`] only exists after the session mounts, so the gate
/// starts empty (a [`OnceLock`] of the sender half) and
/// [`NotifyGate::start`] fills it — spawning the thread — the moment
/// the session is live. Before that, and after the session drops (the
/// `Notifier`'s channel dies, the thread exits, the receiver is gone),
/// every `enqueue` is a best-effort no-op.
#[derive(Default)]
struct NotifyGate {
    /// Sender half of the invalidation queue, installed by [`Self::start`].
    tx: OnceLock<Sender<InvalMsg>>,
    /// Join handle for the invalidator thread, kept so the gate can be
    /// dropped cleanly. Behind a `Mutex<Option<…>>` because `Drop`
    /// needs `&mut`-style access through the shared `Arc`.
    worker: Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl NotifyGate {
    /// Install the live notifier and spawn the invalidator thread.
    /// Called once, right after the session mounts. A second call (it
    /// can't happen with the current flow) is ignored — the first
    /// notifier wins and the second thread never starts.
    fn start(&self, notifier: Notifier) {
        let (tx, rx) = channel::<InvalMsg>();
        if self.tx.set(tx).is_err() {
            // Already started; drop the freshly-built channel.
            return;
        }
        let handle = std::thread::Builder::new()
            .name("heddle-fuse-inval".to_string())
            .spawn(move || {
                // Drains until every sender is dropped (i.e. the gate /
                // mount is torn down), then exits.
                for msg in rx {
                    match msg {
                        InvalMsg::Inode(ino) => {
                            // offset 0, len -1 == the entire data range
                            // plus the cached attrs.
                            if let Err(err) = notifier.inval_inode(INodeNo(ino), 0, -1) {
                                debug!(
                                    ?err,
                                    ino, "inval_inode failed; stale read possible until TTL"
                                );
                            }
                        }
                        InvalMsg::Entry(parent, name) => {
                            if let Err(err) = notifier.inval_entry(INodeNo(parent), &name) {
                                debug!(
                                    ?err,
                                    parent,
                                    ?name,
                                    "inval_entry failed; stale dentry possible until TTL"
                                );
                            }
                        }
                    }
                }
            })
            .expect("spawn heddle-fuse-inval thread");
        *self.worker.lock().expect("notify worker mutex") = Some(handle);
    }

    /// Enqueue a whole-inode invalidation. Best-effort: a no-op before
    /// the gate is started or after the worker has exited. Never blocks
    /// the FUSE callback on the kernel — that's the whole point.
    fn inval_inode(&self, ino: u64) {
        if let Some(tx) = self.tx.get() {
            let _ = tx.send(InvalMsg::Inode(ino));
        }
    }

    /// Enqueue a dentry invalidation for `(parent, name)`. Best-effort,
    /// same rationale as [`Self::inval_inode`].
    fn inval_entry(&self, parent: u64, name: &OsStr) {
        if let Some(tx) = self.tx.get() {
            let _ = tx.send(InvalMsg::Entry(parent, name.to_os_string()));
        }
    }
}

impl Drop for NotifyGate {
    fn drop(&mut self) {
        // Field drop order would already close the channel (dropping
        // the `Sender` in `tx`) and let the thread exit, but be explicit
        // and deterministic: take the sender out first so the thread's
        // `for msg in rx` loop ends, then join it. Joining keeps the
        // invalidator from outliving the mount it serves.
        //
        // We can't move the `Sender` out of the `OnceLock` behind `&mut
        // self` without `OnceLock::take` (stable), so use it to drop the
        // sender, then join.
        let _ = self.tx.take();
        if let Some(handle) = self.worker.lock().ok().and_then(|mut g| g.take()) {
            let _ = handle.join();
        }
    }
}

/// Adapter that exposes a [`ContentAddressedMount`] to the kernel
/// via FUSE. Owns the mount in an `Arc` so the FUSE worker thread(s)
/// share the same registry.
pub struct FuseShell {
    inner: Arc<ContentAddressedMount>,
    /// Kernel-notification handle, populated at mount time. Shared with
    /// nothing else; the `Arc` is only so [`Self::mount`] can hand a
    /// clone to the spawned session while keeping one for the shell
    /// that's been moved into the session. See [`NotifyGate`].
    notify: Arc<NotifyGate>,
    /// Count of `read` callbacks served since mount. Lets a test prove
    /// the kernel page cache is live (cached mode, heddle#87): in
    /// cached mode a re-read of unchanged content is served from the
    /// page cache and never reaches this callback, so the counter
    /// stays flat across the second read; in the old `FOPEN_DIRECT_IO`
    /// mode every userspace `read(2)` would bump it. Shared via `Arc`
    /// so a caller can hold a handle ([`Self::read_calls_handle`])
    /// after the shell is consumed by `mount_background`.
    read_calls: Arc<AtomicU64>,
}

impl FuseShell {
    /// Wrap a mount into a FUSE filesystem.
    pub fn new(mount: ContentAddressedMount) -> Self {
        Self {
            inner: Arc::new(mount),
            notify: Arc::new(NotifyGate::default()),
            read_calls: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Shared handle to the FUSE `read`-callback counter. Grab this
    /// *before* mounting (the shell is consumed by `mount` /
    /// `mount_background`) to observe how many reads actually reach
    /// userspace — used by the cache-mode-active test to confirm the
    /// kernel page cache is serving repeated reads (heddle#87).
    pub fn read_calls_handle(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.read_calls)
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
    ///
    /// We build the [`Session`] explicitly (rather than the
    /// `fuser::mount2` convenience) so we can install the kernel
    /// [`Notifier`] into the shell's [`NotifyGate`] *before* the event
    /// loop starts serving callbacks — that's what lets the write-side
    /// callbacks push active cache invalidations (heddle#87). The gate
    /// is shared via `self.notify`, which gets moved into the session
    /// along with `self`; we kept a clone of the `Arc` before the move.
    ///
    /// `Session::run` is crate-private in fuser, so to keep blocking
    /// semantics we spawn the worker thread and `join()` it — the join
    /// returns when the mount is torn down, exactly as the old
    /// `mount2` call did.
    pub fn mount(self, mountpoint: impl AsRef<Path>) -> Result<()> {
        let config = default_config();
        let gate = Arc::clone(&self.notify);
        let session = Session::new(self, mountpoint.as_ref(), &config)
            .map_err(|e| crate::error::MountError::Store(objects::error::HeddleError::Io(e)))?;
        gate.start(session.notifier());
        let bg = session
            .spawn()
            .map_err(|e| crate::error::MountError::Store(objects::error::HeddleError::Io(e)))?;
        bg.join()
            .map_err(|e| crate::error::MountError::Store(objects::error::HeddleError::Io(e)))?;
        Ok(())
    }

    /// Mount in a background session. Caller holds the returned
    /// [`BackgroundSession`]; dropping it triggers an unmount.
    ///
    /// As with [`Self::mount`], the kernel [`Notifier`] is installed
    /// into the shell's [`NotifyGate`] right after the session mounts
    /// so the write-side callbacks can invalidate the kernel cache.
    pub fn mount_background(self, mountpoint: impl AsRef<Path>) -> Result<BackgroundSession> {
        let config = default_config();
        let gate = Arc::clone(&self.notify);
        let session = Session::new(self, mountpoint.as_ref(), &config)
            .map_err(|e| crate::error::MountError::Store(objects::error::HeddleError::Io(e)))?;
        // Install the notifier before spawning the worker thread: once
        // the thread is live the kernel can issue callbacks, and we
        // want the gate filled before the first mutation arrives. The
        // `Session`'s notifier and the `BackgroundSession`'s notifier
        // wrap clones of the same channel sender, so a notifier taken
        // here stays valid for the whole session lifetime.
        gate.start(session.notifier());
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

/// Shared `FileAttr` builder. The 14 constant-shape fields (block
/// count, the four mirrored timestamps, uid/gid, rdev/blksize/flags,
/// and the type-bit-masked `perm`) live here once; the two callers
/// supply only the values that actually differ between an `Attrs`
/// snapshot and a freshly-resolved `Entry`.
fn make_file_attr(
    node: NodeId,
    size: u64,
    mtime: std::time::SystemTime,
    kind: NodeKind,
    unix_mode: u32,
    nlink: u32,
) -> FileAttr {
    FileAttr {
        ino: INodeNo(node.0),
        size,
        blocks: size.div_ceil(512),
        atime: mtime,
        mtime,
        ctime: mtime,
        crtime: mtime,
        kind: file_type_for_kind(kind),
        // The `unix_mode` we store includes the type bits; FUSE wants
        // just the permission bits in `perm`.
        perm: (unix_mode & 0o7777) as u16,
        nlink,
        uid: process_uid(),
        gid: process_gid(),
        rdev: 0,
        blksize: 4096,
        flags: 0,
    }
}

fn file_attr_from(attrs: Attrs) -> FileAttr {
    make_file_attr(
        attrs.node,
        attrs.size,
        attrs.mtime,
        attrs.kind,
        attrs.unix_mode,
        attrs.nlink,
    )
}

fn entry_attr_from(entry: &Entry, mtime: std::time::SystemTime) -> FileAttr {
    make_file_attr(
        entry.node,
        entry.size,
        mtime,
        entry.kind,
        entry.unix_mode,
        1,
    )
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
            let msg = crate::error::panic_payload_str(&payload);
            tracing::error!(callback = label, %msg, "FUSE callback panicked; returning EIO");
            Err(crate::error::MountError::Store(
                objects::error::HeddleError::Io(std::io::Error::other(format!(
                    "panic in FUSE {label}: {msg}"
                ))),
            ))
        }
    }
}

impl Filesystem for FuseShell {
    fn init(&mut self, _req: &Request, _config: &mut KernelConfig) -> std::io::Result<()> {
        // Nothing to opt into. The mount runs in the kernel's default
        // caching mode: `open` no longer returns `FOPEN_DIRECT_IO`, so
        // the page cache is live and shared `mmap(MAP_SHARED, ...)`
        // works out of the box without the `FUSE_DIRECT_IO_ALLOW_MMAP`
        // capability (and therefore without the Linux 5.16+ floor that
        // cap required). Coherence comes from active invalidation: see
        // the module-level "Invalidation contract" — every write-side
        // callback pushes the matching `notify_inval_*` so the kernel
        // can never serve stale cached bytes, attrs, or dentries.
        Ok(())
    }

    fn open(&self, _req: &Request, ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
        // R8 (Codex Thread 3293235165): notify the core that an open
        // handle was minted. The core's open-handle refcount times
        // the orphan cleanup against the final `release` rather than
        // the first `flush` — without this hook, a kernel that opens
        // an inode twice (rather than dup'ing) clears the marker on
        // the first close. Errors here can only come from the panic
        // guard; the cost of skipping the bookkeeping is correctness
        // under that pathological case, so we log and fail open.
        if let Err(err) = guard_call("open", || self.inner.on_open(NodeId(ino.0))) {
            warn!(
                ?err,
                "on_open bookkeeping failed; orphan cleanup may misfire"
            );
        }
        // Cached mode (heddle#87). Two flags, no `FOPEN_DIRECT_IO`:
        //
        // * (absence of `FOPEN_DIRECT_IO`) — reads flow through the
        //   kernel page cache instead of bypassing it, so repeated
        //   reads of unchanged content are served without a FUSE
        //   round-trip (the throughput win), and shared `mmap` works
        //   without the `FUSE_DIRECT_IO_ALLOW_MMAP` cap (no Linux 5.16+
        //   floor).
        // * `FOPEN_KEEP_CACHE` — tells the kernel *not* to drop the
        //   page cache on `open`. FUSE's default is to invalidate the
        //   data cache on every open, which would defeat cross-open
        //   caching entirely (each fresh `open`+`read` would re-hit the
        //   shell). With KEEP_CACHE the cache persists across opens and
        //   we own coherence explicitly via active invalidation.
        //
        // Coherence is maintained actively: the content-changing
        // callbacks (`write`, `setattr`) push a `notify_inval_*` after
        // they mutate the overlay (see the module-level invalidation
        // contract). The classic stale-read hazard — write a captured
        // file, close, reopen, and have the kernel serve the *pre-write*
        // bytes from its page cache — is closed because `write` /
        // `setattr` enqueue `inval_inode(ino, 0, -1)`, dropping those
        // cached pages so a re-reader misses cache and re-asks us. This
        // is the same invalidate-on-change model FSKit uses on macOS,
        // giving both platforms identical caching semantics.
        //
        // FH=0 mirrors the fuser default (we don't track per-handle
        // state — open files identify by inode in [`PlatformShell`]).
        reply.opened(FileHandle(0), FopenFlags::FOPEN_KEEP_CACHE);
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
        // Bump the read-callback counter. In cached mode (heddle#87)
        // the kernel serves repeated reads of unchanged content from
        // its page cache, so this fires far less often than userspace
        // issues `read(2)` — the cache-mode-active test asserts on the
        // gap. `Relaxed` is fine: the test reads the counter only after
        // the relevant filesystem ops have completed and been observed.
        self.read_calls.fetch_add(1, Ordering::Relaxed);
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
            Ok(n) => {
                // The hot tier now holds bytes the kernel's page cache
                // doesn't know about. Drop the cached pages + attrs for
                // this inode so any concurrent/subsequent reader re-asks
                // us. (heddle#87 invalidation contract.)
                self.notify.inval_inode(ino.0);
                reply.written(n as u32);
            }
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
        //
        // We deliberately do *not* invalidate here. Promotion moves the
        // bytes between tiers but doesn't change the *content* the shell
        // serves — and the `write` (and `setattr`) callbacks already
        // invalidated the kernel's cache for every byte they changed. A
        // blanket invalidate on every `flush` would also drop the cache
        // on a *read-only* close (flush fires on every `close(2)`, not
        // just writers), defeating the cached-read win this whole change
        // exists to deliver. (heddle#87 invalidation contract.)
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
        //
        // As with `flush`: no invalidation here. The content-changing
        // callbacks (`write` / `setattr`) already invalidated, and an
        // unconditional release-time invalidate would drop the cache on
        // every reader's close. (heddle#87 invalidation contract.)
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
                // R8: bump the open-handle refcount the same as
                // `Self::open` does. The kernel won't issue a
                // separate `open` callback after `create`.
                if let Err(err) = guard_call("create", || self.inner.on_open(entry.node)) {
                    warn!(
                        ?err,
                        "on_open bookkeeping failed; orphan cleanup may misfire"
                    );
                }
                // A new entry now exists under `parent`. The kernel may
                // have cached a *negative* dentry for this name (e.g.
                // from an `O_CREAT` open that first `stat`ed and got
                // ENOENT); invalidate it so the entry becomes visible.
                // (heddle#87 invalidation contract.)
                self.notify.inval_entry(parent.0, name);
                // Mirror the `open` callback: cached mode with
                // `FOPEN_KEEP_CACHE`, no `FOPEN_DIRECT_IO` (see
                // `Self::open` for the full reasoning + the
                // invalidation contract).
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
                    FopenFlags::FOPEN_KEEP_CACHE,
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
                // New dir entry under `parent`; refresh any cached
                // (likely negative) dentry. (heddle#87.)
                self.notify.inval_entry(parent.0, name);
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
            self.inner
                .create_file(NodeId(parent.0), name, file_mode, true)
        });
        match result {
            Ok(entry) => {
                // New file entry under `parent`; refresh the dentry.
                // (heddle#87.)
                self.notify.inval_entry(parent.0, name);
                let attr = entry_attr_from(&entry, self.inner_mtime());
                reply.entry(&TTL, &attr, GENERATION);
            }
            Err(err) => reply.error(errno_from_mount_error(err)),
        }
    }

    fn unlink(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        match guard_call("unlink", || self.inner.unlink_entry(NodeId(parent.0), name)) {
            Ok(()) => {
                // Drop the now-removed entry's cached dentry so a
                // re-lookup 404s instead of resurrecting it from cache.
                // (heddle#87.)
                self.notify.inval_entry(parent.0, name);
                reply.ok();
            }
            Err(err) => reply.error(errno_from_mount_error(err)),
        }
    }

    fn rmdir(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        match guard_call("rmdir", || self.inner.rmdir_entry(NodeId(parent.0), name)) {
            Ok(()) => {
                // Same as `unlink`: drop the removed dir's dentry.
                // (heddle#87.)
                self.notify.inval_entry(parent.0, name);
                reply.ok();
            }
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
        // R8 (Codex Thread 3293235163): `RENAME_NOREPLACE` is plumbed
        // through the core's `rename_entry_with_options` so the
        // existence-check + the directory-entry mutation land under
        // the same write-side critical section. The old shell-side
        // pre-check left a TOCTOU window between the lookup and the
        // rename — a concurrent writer could install the
        // destination in between and the rename would clobber it.
        if flags.contains(RenameFlags::RENAME_EXCHANGE)
            || flags.contains(RenameFlags::RENAME_WHITEOUT)
        {
            reply.error(Errno::from_i32(libc::EINVAL));
            return;
        }
        let no_replace = flags.contains(RenameFlags::RENAME_NOREPLACE);
        let options = RenameOptions { no_replace };
        match guard_call("rename", || {
            self.inner.rename_entry_with_options(
                NodeId(parent.0),
                name,
                NodeId(newparent.0),
                newname,
                options,
            )
        }) {
            Ok(()) => {
                // Both the source (now gone) and destination (now
                // present, possibly replacing a cached entry) dentries
                // are stale. Invalidate each. (heddle#87.)
                self.notify.inval_entry(parent.0, name);
                self.notify.inval_entry(newparent.0, newname);
                reply.ok();
            }
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
            Ok(attrs) => {
                // chmod / truncate / mtime all change cached attrs, and
                // a truncate changes the cached data too. Whole-inode
                // invalidate covers both. (heddle#87.)
                self.notify.inval_inode(ino.0);
                reply.attr(&TTL, &file_attr_from(attrs));
            }
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
            self.inner
                .create_symlink(NodeId(parent.0), link_name, target)
        });
        match result {
            Ok(entry) => {
                // New symlink entry under `parent`; refresh the dentry.
                // (heddle#87.)
                self.notify.inval_entry(parent.0, link_name);
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
