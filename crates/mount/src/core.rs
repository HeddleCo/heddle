// SPDX-License-Identifier: Apache-2.0
//! Content-addressed mount core.
//!
//! [`ContentAddressedMount`] is the platform-agnostic implementation
//! of [`PlatformShell`]. It speaks heddle: given a thread name, it
//! resolves it to a state via [`refs::RefManager`], pulls the tree
//! root from the object store, and answers filesystem queries by
//! walking the Merkle DAG lazily.
//!
//! ## Two-tier write model
//!
//! Writes don't go through a generic in-memory page cache that drains
//! to disk on `heddle capture`. They go straight into heddle's CAS as
//! soon as the file is closed:
//!
//! 1. **Hot tier (in-memory partial buffers).** A `write(offset, bytes)`
//!    is keyed by [`NodeId`] and accumulates in a single `Vec<u8>` per
//!    open file. Reads of the same node during the buffer's lifetime
//!    serve from the buffer (so a `write -> read` round-trip in the
//!    same FUSE session sees the new bytes immediately).
//!
//! 2. **Warm tier (CAS-promoted blobs).** When the kernel signals end
//!    of file (`flush`/`close`), or after an idle threshold (the
//!    [`PromotionPolicy::idle_after`] window), we hash the buffer,
//!    write a blob via the same [`ObjectStore`] API that
//!    `heddle capture` uses, and record `path -> blob_oid` in a
//!    per-thread *pending tree*. The hot buffer is dropped.
//!
//! 3. **Pending tree.** A `BTreeMap<RelPath, PendingEntry>` plus a
//!    `BTreeSet<RelPath>` of deletions that overlay the immutable
//!    state's tree. `lookup`/`enumerate`/`read` consult the pending
//!    tier first so the mount serves "what the agent just wrote"
//!    rather than the parent state.
//!
//! ### Crash semantics
//!
//! The hot tier lives only in process memory; an unclean unmount
//! discards in-flight writes. The warm tier is written to the heddle
//! object store via the same atomic write path that `heddle capture`
//! uses, so a promoted blob survives a crash even if the surrounding
//! `capture()` call never completes — the next agent that captures
//! the same content will hit the dedup fast path.
//!
//! ### Why this beats a worktree-walk capture
//!
//! `heddle capture` from a worktree currently walks every file,
//! hashes its contents, and writes the blob if new. Mount writes do
//! that work *during* the write itself, so capture-from-mount becomes:
//!  - drain pending tree into a real `Tree` object
//!  - record `State` referencing the tree
//!  - update the thread's HEAD
//!
//! No worktree walk, no re-hashing, no blob duplication across
//! threads — two agents writing the same `import { foo } from 'bar'`
//! to two different files write *one* blob.

use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    ffi::{OsStr, OsString},
    path::{Component, Path, PathBuf},
    sync::{
        Arc, Mutex, RwLock, Weak,
        atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering},
    },
    thread::JoinHandle,
    time::{Duration, Instant, SystemTime},
};

use objects::{
    object::{
        Attribution, Blob, ChangeId, ContentHash, EntryType, FileMode, State, Tree, TreeEntry,
    },
    store::{AnyStore, ObjectStore},
};
use oplog::OpLog;
use refs::RefManager;
use repo::Repository;
use tracing::{debug, instrument, warn};

use crate::{
    cache::BlobCachePool,
    error::{MountError, Result},
    shell::{
        AttrUpdate, Attrs, DIR_UNIX_MODE, Entry, NodeId, NodeKind, PlatformShell, RenameOptions,
        kind_for_mode,
    },
};

/// Default promotion idle window: a buffer with no writes for this
/// long is eligible to be drained to CAS without an explicit
/// flush/close. The kernel doesn't always issue `release` for short-
/// lived files (e.g. when the agent process is killed mid-write), so
/// the timer is the safety net.
const DEFAULT_PROMOTION_IDLE: Duration = Duration::from_secs(2);

/// Default cadence for the clock-driven safety-sweep. A worker thread
/// wakes up every `sweep_interval` and promotes any hot buffer that's
/// been idle longer than `idle_after`. Five seconds is well below
/// human attention but well above the kernel's flush cadence, so it
/// catches process-pause/agent-crash leaks without burning CPU.
const DEFAULT_SWEEP_INTERVAL: Option<Duration> = Some(Duration::from_secs(5));

/// Tunables for when buffered writes get promoted to CAS.
#[derive(Clone, Copy, Debug)]
pub struct PromotionPolicy {
    /// Drain buffers with no writes for at least this long. The check
    /// runs opportunistically on every mutating call; agents that go
    /// quiet without closing aren't left holding the buffer.
    pub idle_after: Duration,
    /// How often the clock-driven safety-sweep thread wakes up to
    /// drain idle buffers. `None` disables the timer entirely (useful
    /// for tests that want deterministic event-driven promotion).
    pub sweep_interval: Option<Duration>,
}

impl Default for PromotionPolicy {
    fn default() -> Self {
        Self {
            idle_after: DEFAULT_PROMOTION_IDLE,
            sweep_interval: DEFAULT_SWEEP_INTERVAL,
        }
    }
}

/// The kind of node a registered inode points at.
#[derive(Clone, Debug)]
enum NodeRecord {
    /// Root of the mount — the tree at the thread's current state.
    Root {
        tree: ContentHash,
    },
    /// A subdirectory resolved from the captured tree. `path` is the
    /// mount-relative path of this directory; `tree` is the content
    /// hash of its tree object. Carrying the path lets `lookup` /
    /// `enumerate` consult the pending tier for nested writes.
    Dir {
        tree: ContentHash,
        path: PathBuf,
    },
    /// A directory that exists only in the pending tier (the agent
    /// created `newdir/foo.rs` and `newdir/` is not yet in any
    /// captured tree). No backing tree hash exists yet — it lives
    /// virtually in the pending map.
    PendingDir {
        path: PathBuf,
    },
    /// A file resolved from the captured tree. We carry `path` so
    /// writes against this NodeId can route into the hot tier
    /// without re-walking from the root.
    File {
        blob: ContentHash,
        mode: FileMode,
        path: PathBuf,
    },
    Symlink {
        blob: ContentHash,
    },
    /// A file that exists only in the pending tier (created by the
    /// mount, not yet captured into a state). Its content lives at
    /// `path` in the [`Pending`] map.
    PendingFile {
        path: PathBuf,
        mode: FileMode,
    },
    /// A symlink created through the mount. Target bytes live in
    /// [`Pending::symlinks`]; we don't promote the symlink to a CAS
    /// blob until [`ContentAddressedMount::capture`].
    PendingSymlink {
        path: PathBuf,
    },
}

impl NodeRecord {
    fn kind(&self) -> NodeKind {
        match self {
            NodeRecord::Root { .. } | NodeRecord::Dir { .. } | NodeRecord::PendingDir { .. } => {
                NodeKind::Directory
            }
            NodeRecord::File { mode, .. } | NodeRecord::PendingFile { mode, .. } => {
                kind_for_mode(*mode)
            }
            NodeRecord::Symlink { .. } | NodeRecord::PendingSymlink { .. } => NodeKind::Symlink,
        }
    }

    fn unix_mode(&self) -> u32 {
        match self {
            NodeRecord::Root { .. } | NodeRecord::Dir { .. } | NodeRecord::PendingDir { .. } => {
                DIR_UNIX_MODE
            }
            NodeRecord::File { mode, .. } | NodeRecord::PendingFile { mode, .. } => {
                mode.to_unix_mode()
            }
            NodeRecord::Symlink { .. } | NodeRecord::PendingSymlink { .. } => {
                FileMode::Symlink.to_unix_mode()
            }
        }
    }
}

/// Inode registry — maps the opaque ids we hand out to platform
/// adapters back to the underlying object hashes.
#[derive(Default)]
struct Inodes {
    next: u64,
    by_id: BTreeMap<u64, NodeRecord>,
    /// Reverse index for tree records: a repeated lookup of the
    /// same content hash returns the same NodeId. FUSE caches
    /// inodes aggressively; handing out fresh ids per lookup
    /// explodes the kernel-side dcache.
    by_hash: BTreeMap<HashKey, u64>,
    /// Reverse index for files (both captured and pending): keyed
    /// by relative path. Two files with identical content but
    /// different paths get distinct inode numbers — that's required
    /// for the cross-thread dedup story (the *blob* is the same, the
    /// *inode* must not be).
    by_path: BTreeMap<PathBuf, u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct HashKey {
    /// 0 = tree, 1 = blob (file), 2 = blob (symlink). Distinguishing
    /// the same hash referenced as both a tree and a blob is paranoid
    /// — content hashes are typed — but it's cheap and self-documenting.
    kind: u8,
    hash: ContentHash,
}

impl Inodes {
    fn new(root_tree: ContentHash) -> Self {
        let mut me = Self {
            next: NodeId::ROOT.0 + 1,
            by_id: BTreeMap::new(),
            by_hash: BTreeMap::new(),
            by_path: BTreeMap::new(),
        };
        me.by_id
            .insert(NodeId::ROOT.0, NodeRecord::Root { tree: root_tree });
        me.by_hash.insert(
            HashKey {
                kind: 0,
                hash: root_tree,
            },
            NodeId::ROOT.0,
        );
        me
    }

    fn get(&self, id: NodeId) -> Option<NodeRecord> {
        self.by_id.get(&id.0).cloned()
    }

    fn intern(&mut self, record: NodeRecord) -> NodeId {
        match &record {
            NodeRecord::Root { tree } => {
                let key = HashKey {
                    kind: 0,
                    hash: *tree,
                };
                if let Some(&id) = self.by_hash.get(&key) {
                    return NodeId(id);
                }
                let id = self.next;
                self.next += 1;
                self.by_id.insert(id, record);
                self.by_hash.insert(key, id);
                NodeId(id)
            }
            NodeRecord::Dir { path, .. } | NodeRecord::PendingDir { path } => {
                // Coalesce by path so the same directory hands back
                // the same NodeId across lookups, even if the backing
                // tree hash flips after a capture.
                if let Some(&id) = self.by_path.get(path) {
                    self.by_id.insert(id, record);
                    return NodeId(id);
                }
                let id = self.next;
                self.next += 1;
                self.by_path.insert(path.clone(), id);
                self.by_id.insert(id, record);
                NodeId(id)
            }
            NodeRecord::File { path, .. }
            | NodeRecord::PendingFile { path, .. }
            | NodeRecord::PendingSymlink { path } => {
                if let Some(&id) = self.by_path.get(path) {
                    // If the path's record is being upgraded
                    // (e.g. PendingFile -> File after capture, or
                    // a File whose blob hash flipped), refresh the
                    // backing record so subsequent reads see the
                    // new identity.
                    self.by_id.insert(id, record);
                    return NodeId(id);
                }
                let id = self.next;
                self.next += 1;
                self.by_path.insert(path.clone(), id);
                self.by_id.insert(id, record);
                NodeId(id)
            }
            NodeRecord::Symlink { blob } => {
                let key = HashKey {
                    kind: 2,
                    hash: *blob,
                };
                if let Some(&id) = self.by_hash.get(&key) {
                    return NodeId(id);
                }
                let id = self.next;
                self.next += 1;
                self.by_id.insert(id, record);
                self.by_hash.insert(key, id);
                NodeId(id)
            }
        }
    }

    fn forget(&mut self, id: NodeId) {
        if id == NodeId::ROOT {
            // Root is a permanent fixture; the only way to retire it
            // is to drop the whole mount.
            return;
        }
        if let Some(record) = self.by_id.remove(&id.0) {
            match record {
                NodeRecord::Root { tree } => {
                    self.by_hash.remove(&HashKey {
                        kind: 0,
                        hash: tree,
                    });
                }
                NodeRecord::Dir { path, .. } | NodeRecord::PendingDir { path } => {
                    // Codex r12 thread 3293680448 (P1): only drop the
                    // path mapping if it still points at *this* inode.
                    // After unlink-then-recreate or rename-over, `path`
                    // may already be rebound to a live inode at a
                    // different NodeId; a blind `remove` would yank
                    // that fresh inode's binding too.
                    if self.by_path.get(&path) == Some(&id.0) {
                        self.by_path.remove(&path);
                    }
                }
                NodeRecord::File { path, .. }
                | NodeRecord::PendingFile { path, .. }
                | NodeRecord::PendingSymlink { path } => {
                    if self.by_path.get(&path) == Some(&id.0) {
                        self.by_path.remove(&path);
                    }
                }
                NodeRecord::Symlink { blob } => {
                    self.by_hash.remove(&HashKey {
                        kind: 2,
                        hash: blob,
                    });
                }
            }
        }
    }
}

/// A single in-flight write tier entry.
struct HotBuffer {
    /// Mount-relative path the buffer maps to.
    path: PathBuf,
    /// File mode (executable bit, etc.).
    mode: FileMode,
    /// Buffered bytes. Indexed by absolute file offset.
    bytes: Vec<u8>,
    /// Last write time, used by the idle-promotion check.
    last_touched: Instant,
}

/// A single warm-tier entry — a path that has been promoted to CAS
/// but not yet folded into a state.
#[derive(Clone, Debug)]
struct PendingEntry {
    blob: ContentHash,
    mode: FileMode,
    size: u64,
}

/// Per-NodeId lifecycle state. Tracks both whether an inode is still
/// resolvable via `inodes.by_path` (Live) or has had its directory
/// entry removed but is still held by an open fd (Orphan), and the
/// open-handle refcount that drives the final-close cleanup.
///
/// Absence from [`Pending::state`] is the third state — Released —
/// matching the spike model (`docs/design/mount-posix-semantics.md`
/// §1.1). The type system makes "orphaned with no open count" and
/// "open count without orphan flag" unrepresentable, replacing the
/// old `orphans: BTreeSet<u64>` + `open_handles: BTreeMap<u64, u32>`
/// pair with one map and forcing every callback that branches on
/// lifecycle to `match`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum NodeState {
    /// Live: the inode owns a binding in `inodes.by_path`. The
    /// refcount tracks how many FUSE `open` / `create` callbacks
    /// have minted handles to it; `release` drives it back down.
    /// At T1 (unlink with N ≥ 0), the count is carried over into
    /// `Orphan { open_count }`.
    Live { open_count: u32 },
    /// Orphan: directory entry gone (`unlink_entry` or `rename`-over
    /// of the displaced destination) but `open_count` kernel fds
    /// still hold the NodeId. Bytes in `hot[node]` / `warm[node]`
    /// outlive the transition; the cleanup happens on the final
    /// `release` (count drops to 0 → state entry removed + bytes
    /// dropped).
    Orphan { open_count: u32 },
}

/// The two-tier write state for a mount.
///
/// Post-spike (`docs/design/mount-posix-semantics.md` §2.1) the cache
/// is **NodeId-keyed throughout**: `hot[id]` and `warm[id]` carry the
/// bytes; path-keyed helpers (`hot_by_path`, `tombstones`,
/// `dir_tombstones`, `explicit_dirs`, `symlinks`) are
/// directory-entry-level concepts only. The Live → Orphan transition
/// never moves bytes — it just rewrites `state[id]` and the path-side
/// bookkeeping. That collapse eliminates the cache-layer asymmetry
/// (Bug Class A in the spike doc §4) that produced every Codex
/// finding r6 → r9 on PR #182.
#[derive(Default)]
#[doc(hidden)]
pub struct Pending<'brand> {
    /// Hot tier: per-`NodeId` open-file buffers.
    hot: BTreeMap<u64, HotBuffer>,
    /// Reverse-index for the hot tier: which NodeId currently owns a
    /// buffer for `path`. Path-keyed because FUSE `lookup` arrives
    /// with paths, not NodeIds. Only one at a time — opening the
    /// same file twice from different node ids resolves to the same
    /// buffer because the inode registry coalesces by path for
    /// pending files. Removed on `unlink_entry` / rebound on
    /// `rename_entry`.
    hot_by_path: BTreeMap<PathBuf, u64>,
    /// Warm tier: per-`NodeId` promoted bytes. Bytes survive the
    /// Live → Orphan transition without a migration step — that's
    /// Decision A of the spike. `pending_lookup` resolves a path to
    /// its current Live NodeId via `inodes.by_path` and then reads
    /// `warm[id]`; orphan branches in `read` / `attrs` / `write` /
    /// `apply_truncate` consult `warm[node]` directly.
    warm: BTreeMap<u64, PendingEntry>,
    /// Tombstones — paths the mount has deleted. Suppress the
    /// underlying state's entry on reads. File-only; directories
    /// use [`Self::dir_tombstones`].
    tombstones: BTreeSet<PathBuf>,
    /// Directory tombstones — captured-tree directories the mount
    /// has `rmdir`'d. Distinct from file tombstones because the
    /// capture-time `apply_pending_to_tree` walk has to drop the
    /// whole subtree, not a single leaf.
    dir_tombstones: BTreeSet<PathBuf>,
    /// Directories the mount has `mkdir`'d into the overlay that
    /// don't (yet) have any children. Without this, an empty
    /// `mkdir target/` wouldn't survive across a `lookup` /
    /// `enumerate` round-trip (nothing under it means
    /// [`pending_dir_exists`] would return false).
    explicit_dirs: BTreeSet<PathBuf>,
    /// Symlinks created through the mount, keyed by mount-relative
    /// path. The bytes are the target as the kernel handed them to
    /// `symlink`; capture hashes them into a CAS blob. Symlinks are
    /// not openable for IO; no orphan story applies.
    symlinks: BTreeMap<PathBuf, Vec<u8>>,
    /// Per-NodeId lifecycle state. See [`NodeState`]. Replaces the
    /// pre-spike `orphans: BTreeSet<u64>` + `open_handles:
    /// BTreeMap<u64, u32>` pair. Absence from this map is the third
    /// state (Released) — entries are removed on final `release`,
    /// `invalidate`, and `capture`.
    state: BTreeMap<u64, NodeState>,
    /// Invariant phantom: ties this `Pending` to a unique `'brand`
    /// introduced by [`Pending::with_brand`]. Witnesses minted under
    /// one `'brand` cannot be passed to methods on a `Pending`
    /// carrying a different `'brand` — closes Codex PR #217 r2
    /// finding `3293832936`. The `fn(&'brand ()) -> &'brand ()`
    /// shape makes `'brand` invariant (neither covariant nor
    /// contravariant), so the borrow checker refuses to unify two
    /// fresh brands handed out by separate `with_brand` calls.
    _brand: std::marker::PhantomData<fn(&'brand ()) -> &'brand ()>,
}

impl<'brand> Pending<'brand> {
    /// True iff the NodeId is currently Orphan (directory entry gone
    /// but `open_count >= 0` fds still reference the inode). Every
    /// callback that branches on lifecycle goes through this helper
    /// (or matches `state` directly) so the "implicitly assume Live"
    /// failure mode is hard to write.
    fn is_orphan(&self, id: u64) -> bool {
        matches!(self.state.get(&id), Some(NodeState::Orphan { .. }))
    }

    /// Current open-handle refcount for the NodeId, or zero if
    /// untracked. Used by [`MountInner::release_node`] to drive the
    /// final-close cleanup and by [`unlink_entry`] / [`rename_entry`]
    /// to carry the count over into `Orphan { open_count }` at T1/T3.
    fn open_count(&self, id: u64) -> u32 {
        match self.state.get(&id) {
            Some(NodeState::Live { open_count } | NodeState::Orphan { open_count }) => *open_count,
            None => 0,
        }
    }

    /// Read-only access to the per-NodeId lifecycle entry. Sole
    /// reachable point for the [`crate::pending`] witness constructors
    /// to query the FSM without taking a `pub(crate)` dependency on
    /// the underlying `state` field. Returning `Option<NodeState>` by
    /// value keeps the field private to this module — callers cannot
    /// mutate state through this handle.
    pub(crate) fn lookup_state(&self, id: u64) -> Option<NodeState> {
        self.state.get(&id).copied()
    }

    /// Witness-gated LiveNonZero → Orphan state transition. The
    /// `&Witness<'_, 'brand, Orphan>` parameter is the type-level
    /// proof that the caller has already gone through the FSM check —
    /// `Witness::new` is module-private to [`crate::pending`], so the
    /// only callers that can name this method's argument type are the
    /// [`crate::pending::BrandedPending::transition_to_orphan`] body
    /// (which constructs the witness after consuming a matching
    /// `Witness<LiveNonZero>`) and code that already held a
    /// `Witness<Orphan>` (in which case the state was already Orphan,
    /// and re-inserting is a no-op on the discriminant). Direct
    /// callers in this module have no way to mint a `Witness<Orphan>`,
    /// so they cannot bypass the witness discipline.
    pub(crate) fn apply_transition_to_orphan(
        &mut self,
        w: &crate::pending::Witness<'_, 'brand, crate::pending::Orphan>,
    ) {
        let id = w.id();
        let open_count = self.open_count(id);
        self.state.insert(id, NodeState::Orphan { open_count });
    }

    /// Read-only iterator over the per-NodeId lifecycle map. Sole
    /// enumeration point for [`crate::pending::Pending::drain_for_capture`]'s
    /// typed-match classification pass — keeps the underlying `state`
    /// field private to this module while letting the retrofitted drain
    /// classify every resident entry by [`crate::pending::ResidentLifecycle`].
    /// Returns owned `(u64, NodeState)` pairs (both are `Copy`) so the
    /// iterator does not borrow `self` past the classify pass; the drain
    /// then takes its own `&mut self` borrow to apply the retention.
    pub(crate) fn lifecycle_iter(&self) -> impl Iterator<Item = (u64, NodeState)> + '_ {
        self.state.iter().map(|(&id, &s)| (id, s))
    }

    /// Witness-gated FUSE-forget discharge. The
    /// `&KernelForgetWitness<'_, 'brand>` parameter is the type-level
    /// proof that the caller has already gone through the discharge-
    /// safety FSM check: the witness is constructed only inside
    /// [`crate::pending::BrandedPending::kernel_forget_inode`], whose
    /// body matches the same `None | Some(Live { open_count: 0 })`
    /// pattern as [`crate::pending::BrandedPending::witness_kernel_forget`].
    /// [`crate::pending::KernelForgetWitness::new`] is module-private
    /// to [`crate::pending`], so the only callers that can name this
    /// method's argument type are that one entry point (and code that
    /// already held a witness — same brand-gating chain as
    /// [`Self::apply_transition_to_orphan`]).
    ///
    /// Removes `hot[id]` (with its `hot_by_path` reverse-index
    /// cleanup) and `state[id]`, then returns `true` iff `warm[id]`
    /// is still populated — the caller in `MountInner::invalidate`
    /// uses that bool to decide whether the inode-side `forget` is
    /// safe to fire (warm is the durable pre-capture copy; if it's
    /// there, capture still needs the NodeId → path chain).
    ///
    /// `warm` is intentionally preserved here per Codex r12 threads
    /// 3293484634 / 3293510311 (P1): FUSE `forget` is a kernel-side
    /// dcache eviction, not a close — dropping warm bytes silently
    /// loses the user's committed-in-session data.
    pub(crate) fn apply_kernel_forget(
        &mut self,
        w: &crate::pending::KernelForgetWitness<'_, 'brand>,
    ) -> bool {
        let id = w.id();
        if let Some(buf) = self.hot.remove(&id)
            && self.hot_by_path.get(&buf.path) == Some(&id)
        {
            self.hot_by_path.remove(&buf.path);
        }
        self.state.remove(&id);
        self.warm.contains_key(&id)
    }

    /// Apply the retention/clear pass of [`crate::pending::Pending::drain_for_capture`].
    /// `surviving` is the set of NodeIds whose `state` / `hot[id]` /
    /// `warm[id]` entries must outlive the capture — produced by the
    /// typed-match classifier in [`crate::pending`]; every NodeId in
    /// the set was classified as [`crate::pending::ResidentLifecycle::LiveNonZero`]
    /// (open fds still hold the bytes — POSIX last-close-wins) or
    /// [`crate::pending::ResidentLifecycle::Orphan`] (directory entry
    /// gone but the bytes outlive it).
    ///
    /// The path-keyed overlays are unconditionally cleared: every path
    /// they covered is now folded into the new captured tree, and the
    /// unlink/rename T1/T3 transitions already removed `hot_by_path` /
    /// `symlinks` / `inodes.by_path` bindings for any orphan branch,
    /// so nothing here references a surviving NodeId by path.
    pub(crate) fn apply_drain_for_capture(&mut self, surviving: &BTreeSet<u64>) {
        self.hot.retain(|id, _| surviving.contains(id));
        self.warm.retain(|id, _| surviving.contains(id));
        self.state.retain(|id, _| surviving.contains(id));
        self.hot_by_path.clear();
        self.tombstones.clear();
        self.dir_tombstones.clear();
        self.explicit_dirs.clear();
        self.symlinks.clear();
    }

    /// Test-only: insert a per-NodeId lifecycle entry directly,
    /// bypassing the FSM entry points. Used by the
    /// [`crate::pending`] substrate tests to set up `Pending` states
    /// without dragging in the full mount lifecycle. Gated behind
    /// `cfg(test)` so it never reaches a release binary.
    #[cfg(test)]
    pub(crate) fn test_insert_state(&mut self, id: u64, state: NodeState) {
        self.state.insert(id, state);
    }

    /// Test-only: insert a hot-tier buffer for `id` with the given
    /// `path` and `bytes`. Used by the [`crate::pending`] tests to set
    /// up scenarios where `drain_for_capture` must preserve the
    /// per-NodeId byte storage alongside the lifecycle entry.
    #[cfg(test)]
    pub(crate) fn test_insert_hot(&mut self, id: u64, path: PathBuf, bytes: Vec<u8>) {
        self.hot.insert(
            id,
            HotBuffer {
                path,
                mode: FileMode::Normal,
                bytes,
                last_touched: Instant::now(),
            },
        );
    }

    /// Test-only: true iff `hot[id]` is currently populated. Mirror
    /// of [`Self::test_insert_hot`] for assertion in
    /// `drain_for_capture` tests.
    #[cfg(test)]
    pub(crate) fn test_has_hot(&self, id: u64) -> bool {
        self.hot.contains_key(&id)
    }
}

/// In-mount overlay: a snapshot-time view of the parent state plus
/// pending writes the agent has issued since.
///
/// Writes never modify the immutable state; they accumulate in
/// [`Pending`] until [`ContentAddressedMount::capture`] folds them
/// into a fresh state.
pub struct ContentAddressedMount<S: ObjectStore + 'static = AnyStore> {
    inner: Arc<MountInner<S>>,
    /// Background safety-sweep worker. Held in an `Option` so the
    /// `Drop` impl can `take()` it, signal shutdown, and join cleanly
    /// without needing to borrow `&mut self`.
    sweeper: Mutex<Option<SweepHandle>>,
}

/// All shared state — held inside an `Arc` so the safety-sweep
/// worker thread can hold a `Weak` reference, drain hot buffers
/// idly, and exit on its own when the mount is dropped.
///
/// `promotion` is wrapped in an `RwLock` so `with_promotion_policy`
/// can swap the active policy without having to rebuild the Arc.
///
/// # Lock ordering invariant
///
/// Three locks coexist inside `MountInner` (`state`, `pending`,
/// `inodes`) and the call sites use them in nested combinations.
/// To avoid deadlock, every code path that acquires more than one
/// MUST follow this order, top-to-bottom:
///
/// ```text
///   state    (RwLock — read or write)
///     │
///     ▼
///   pending  (Mutex)
///     │
///     ▼
///   inodes   (Mutex)
/// ```
///
/// Equivalently: never take `state` while holding `pending` or
/// `inodes`; never take `pending` while holding `inodes`. The
/// reverse direction (drop the inner first, then the outer) is the
/// only safe unwind. `promotion` is independent of all three — it
/// guards a config knob that's read everywhere but never co-locked
/// with the others — so it can be sequenced freely.
///
/// The discipline is currently safe-by-convention: there's no
/// lock-ordering enforcement at the type system level. When adding
/// a new code path that touches more than one of these locks,
/// audit against the diagram above before merging. The existing
/// call sites that take all three in the right order are good
/// templates — search for `state.write` / `state.read` and trace
/// the subsequent `pending.lock()` / `inodes.lock()` to see the
/// pattern in action.
pub(crate) struct MountInner<S: ObjectStore> {
    repo: Repository<RefManager, OpLog, S>,
    thread: String,
    state: RwLock<MountState>,
    inodes: Mutex<Inodes>,
    // Storage carries `Pending<'static>` as the long-lived shape;
    // every actual witness-minting access goes through
    // [`Pending::with_brand`], which re-borrows under a fresh
    // invariant `'brand` introduced by HRTB and hands the closure a
    // [`crate::pending::BrandedPending<'_, 'brand>`]. The `'static`
    // slot can never be exposed as a witness brand because
    // `Pending<'brand>` carries no witness constructors at all — the
    // `witness_*` methods live on [`crate::pending::BrandedPending`],
    // whose private field makes it unconstructible outside
    // `with_brand`'s body. This closes the structural gap Codex
    // flagged in r2 (`3293832936`) and the r3 follow-on
    // (`3293898540`).
    pending: Mutex<Pending<'static>>,
    promotion: RwLock<PromotionPolicy>,
    mounted_at: SystemTime,
    /// Write-side serialization. Acquired by structural-mutation
    /// methods (rename, create, mkdir, symlink) that need their
    /// existence-check + mutation pair to land atomically against
    /// other writers — see [`ContentAddressedMount::rename_entry_with_options`]
    /// and the RENAME_NOREPLACE atomicity contract (Codex r8 Thread
    /// 3293235163). Lock order: `write_mu` precedes every other lock
    /// in [`MountInner`]; never take it while holding `state`,
    /// `pending`, or `inodes`.
    write_mu: Mutex<()>,
    /// Shared materialised-blob cache. Without this every kernel
    /// `read` syscall re-decompresses the full blob from the object
    /// store, which makes chunked + mmap reads ~200× slower than
    /// vanilla FS on multi-MB files (see
    /// `crates/mount/benches/mount_read_paths.rs`). Held as an `Arc`
    /// so multiple mounts in the same process share warm state —
    /// forked-thread mounts inherit fully-warm cache for any blob
    /// the parent already touched.
    blob_cache: Arc<BlobCachePool>,
}

/// Owns the worker thread + its shutdown signal. Dropping this joins
/// the worker.
///
/// Shutdown is event-driven via a `Condvar` rather than polling: the
/// worker parks on `wait_timeout(interval)` and is woken either by
/// the timer firing (run a sweep) or by `signal_and_join` flipping
/// `shutdown` + notifying the condvar (exit immediately). Mount drop
/// used to pay up to 50 ms per `Drop` for a polled-AtomicBool worker
/// to notice — visible in any churn-y workload (the prewarm bench
/// uncovered this) — and now pays only the per-OS thread join cost.
struct SweepHandle {
    state: Arc<SweepShutdown>,
    join: Option<JoinHandle<()>>,
}

struct SweepShutdown {
    shutdown: Mutex<bool>,
    cv: std::sync::Condvar,
}

impl SweepShutdown {
    fn new() -> Self {
        Self {
            shutdown: Mutex::new(false),
            cv: std::sync::Condvar::new(),
        }
    }

    fn signal(&self) {
        *self.shutdown.lock().expect("sweep shutdown lock") = true;
        self.cv.notify_all();
    }

    /// Park the calling thread for up to `dur`, returning early if
    /// `shutdown` flips. Returns `true` when shutdown was requested.
    fn wait(&self, dur: Duration) -> bool {
        let guard = self.shutdown.lock().expect("sweep shutdown lock");
        let (guard, _timeout) = self
            .cv
            .wait_timeout_while(guard, dur, |s| !*s)
            .expect("sweep shutdown wait");
        *guard
    }
}

impl SweepHandle {
    fn signal_and_join(&mut self) {
        self.state.signal();
        if let Some(handle) = self.join.take() {
            // Best-effort: panics from a sweep iteration shouldn't
            // poison the mount drop. Worst case we leak the OS thread
            // for a few hundred ms while it finishes its current
            // promote_idle pass.
            let _ = handle.join();
        }
    }
}

impl Drop for SweepHandle {
    fn drop(&mut self) {
        self.signal_and_join();
    }
}

/// Number of parallel workers the pre-warmer spawns. Decompression
/// is CPU-bound and our blobs are independent, so this scales
/// linearly with cores. Picked low enough to leave headroom for
/// rustc (or whatever the agent is doing) — bumping past 4 wins on
/// idle machines but contends with compile workloads on every
/// laptop I tested.
const PREWARM_WORKERS: usize = 4;

/// Stop hydrating new blobs once the cache is this fraction full.
/// Without a cap the workers would happily decompress more blobs
/// than fit, then immediately watch the LRU evict them — pure churn
/// with no hit-rate benefit. 90% leaves a small headroom for
/// concurrent user reads that arrive while we're still warming.
const PREWARM_FULL_FRACTION: u8 = 90;

/// Cumulative outcome of a prewarm pass. Returned from
/// [`PrewarmHandle::wait`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PrewarmStats {
    /// Blob hashes the tree walk discovered (file + symlink entries
    /// across every reachable tree).
    pub hashes_discovered: u64,
    /// Hashes the workers tried to hydrate. Equal to `hashes_discovered`
    /// for a run that completed, less when workers exited early
    /// because the cache filled up or the caller cancelled.
    pub hashes_visited: u64,
    /// Hashes that hit the cache (sibling mount already warmed
    /// them — the fork-thread fast path).
    pub already_cached: u64,
    /// Hashes loaded from the object store and inserted into the
    /// cache by this pass.
    pub loaded: u64,
    /// Whether the pass terminated naturally vs. early-stopped on
    /// cache fill / cancel.
    pub completed: bool,
}

/// Handle to a running prewarm pass. See [`ContentAddressedMount::prewarm`].
pub struct PrewarmHandle {
    cancel: Arc<AtomicBool>,
    join: Option<JoinHandle<PrewarmStats>>,
}

impl PrewarmHandle {
    fn start<S: ObjectStore + 'static>(weak: Weak<MountInner<S>>) -> Self {
        let cancel = Arc::new(AtomicBool::new(false));
        let cancel_for_worker = Arc::clone(&cancel);
        let join = std::thread::Builder::new()
            .name("heddle-prewarm-coordinator".to_string())
            .spawn(move || prewarm_run(weak, cancel_for_worker))
            // If we can't even spawn the coordinator, surface that
            // as an empty completed run rather than panicking — the
            // mount stays fully usable, just lukewarm.
            .ok();
        Self { cancel, join }
    }

    /// Signal cancellation. Workers exit at the next poll point.
    /// Non-blocking; pair with [`Self::wait`] if you want to be
    /// sure the threads have actually stopped.
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::SeqCst);
    }

    /// Block until the prewarm pass finishes (naturally or via
    /// cancel) and return its stats. Returns the default-zero
    /// stats if the coordinator thread couldn't be spawned.
    pub fn wait(mut self) -> PrewarmStats {
        self.join
            .take()
            .and_then(|h| h.join().ok())
            .unwrap_or_default()
    }
}

impl Drop for PrewarmHandle {
    fn drop(&mut self) {
        // Cancel-on-drop so leaking the handle doesn't keep workers
        // running past the point the caller stopped caring. Workers
        // also self-terminate when the mount drops (Weak upgrade
        // fails), so this is belt-and-braces.
        self.cancel.store(true, Ordering::SeqCst);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

/// Coordinator thread body: walk the tree to collect blob hashes,
/// then fan the hashes out across [`PREWARM_WORKERS`] worker
/// threads. Returns aggregate stats.
fn prewarm_run<S: ObjectStore + 'static>(
    weak: Weak<MountInner<S>>,
    cancel: Arc<AtomicBool>,
) -> PrewarmStats {
    let Some(inner) = weak.upgrade() else {
        return PrewarmStats::default();
    };

    // Phase 1: tree walk. Cheap (in-memory tree + recent-trees
    // cache) so we just do it on the coordinator thread.
    let mut stats = PrewarmStats::default();
    let mut hashes: Vec<ContentHash> = Vec::new();
    let root_tree = inner.state.read().expect("mount state lock").tree;
    let mut queue: VecDeque<ContentHash> = VecDeque::from([root_tree]);
    let mut seen_trees: std::collections::HashSet<ContentHash> = std::collections::HashSet::new();
    while let Some(tree_hash) = queue.pop_front() {
        if cancel.load(Ordering::Relaxed) {
            return stats;
        }
        if !seen_trees.insert(tree_hash) {
            continue;
        }
        let Ok(Some(tree)) = inner.repo.store().get_tree(&tree_hash) else {
            continue;
        };
        for entry in tree.entries() {
            match entry.entry_type {
                EntryType::Tree => queue.push_back(entry.hash),
                EntryType::Blob | EntryType::Symlink => {
                    hashes.push(entry.hash);
                    stats.hashes_discovered += 1;
                }
            }
        }
    }
    drop(inner);

    if hashes.is_empty() {
        stats.completed = true;
        return stats;
    }

    // Phase 2: fan out. Each worker pulls indices off a shared
    // atomic counter — no per-worker chunking required, naturally
    // load-balances across blobs of varying sizes.
    let hashes = Arc::new(hashes);
    let cursor = Arc::new(AtomicUsize::new(0));
    let visited = Arc::new(AtomicU32::new(0));
    let already = Arc::new(AtomicU32::new(0));
    let loaded = Arc::new(AtomicU32::new(0));
    let stop_full = Arc::new(AtomicBool::new(false));

    let mut workers = Vec::with_capacity(PREWARM_WORKERS);
    for worker_id in 0..PREWARM_WORKERS {
        let weak = weak.clone();
        let cancel = Arc::clone(&cancel);
        let hashes = Arc::clone(&hashes);
        let cursor = Arc::clone(&cursor);
        let visited = Arc::clone(&visited);
        let already = Arc::clone(&already);
        let loaded = Arc::clone(&loaded);
        let stop_full = Arc::clone(&stop_full);
        let handle = std::thread::Builder::new()
            .name(format!("heddle-prewarm-{worker_id}"))
            .spawn(move || {
                loop {
                    if cancel.load(Ordering::Relaxed) || stop_full.load(Ordering::Relaxed) {
                        return;
                    }
                    let idx = cursor.fetch_add(1, Ordering::Relaxed);
                    if idx >= hashes.len() {
                        return;
                    }
                    let hash = hashes[idx];
                    let Some(inner) = weak.upgrade() else {
                        return;
                    };
                    visited.fetch_add(1, Ordering::Relaxed);
                    if inner.blob_cache.get(&hash).is_some() {
                        already.fetch_add(1, Ordering::Relaxed);
                        continue;
                    }
                    // Cooperative fill-stop: if the cache is already
                    // near-full when *we* are about to insert, drop
                    // out. Lets the agent's reads stay hot rather
                    // than us evicting our own work.
                    let pool = &inner.blob_cache;
                    let full_threshold = pool
                        .cap_bytes()
                        .saturating_mul(PREWARM_FULL_FRACTION as usize)
                        / 100;
                    if pool.resident_bytes() >= full_threshold {
                        stop_full.store(true, Ordering::Relaxed);
                        return;
                    }
                    match inner.repo.store().get_blob_bytes(&hash) {
                        Ok(Some(bytes)) => {
                            pool.insert(hash, bytes);
                            loaded.fetch_add(1, Ordering::Relaxed);
                        }
                        Ok(None) | Err(_) => {
                            // Best-effort: a missing or unreadable
                            // blob is the user's problem to surface
                            // on the real read path. The prewarmer
                            // silently skips so a corrupted blob
                            // doesn't take down the whole pass.
                        }
                    }
                }
            })
            .ok();
        if let Some(h) = handle {
            workers.push(h);
        }
    }

    for w in workers {
        let _ = w.join();
    }

    stats.hashes_visited = visited.load(Ordering::Relaxed) as u64;
    stats.already_cached = already.load(Ordering::Relaxed) as u64;
    stats.loaded = loaded.load(Ordering::Relaxed) as u64;
    stats.completed = !cancel.load(Ordering::Relaxed) && !stop_full.load(Ordering::Relaxed);
    stats
}

impl<S: ObjectStore + 'static> Drop for ContentAddressedMount<S> {
    fn drop(&mut self) {
        // Signal the worker before dropping the Arc<MountInner> so
        // it observes the shutdown promptly rather than waiting for
        // a Weak::upgrade failure on the next tick.
        if let Some(mut handle) = self.sweeper.lock().expect("sweeper lock").take() {
            handle.signal_and_join();
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct MountState {
    change_id: ChangeId,
    tree: ContentHash,
}

/// Knobs handed to [`ContentAddressedMount::with_options`]. The
/// default-constructed value is what [`ContentAddressedMount::new`]
/// uses internally; build one explicitly when the caller wants to
/// share a blob cache across mounts or tune the cache cap.
#[derive(Clone, Default)]
pub struct MountOptions {
    /// Shared blob cache. `None` means "give me a fresh pool with
    /// the default cap"; clone an existing `Arc<BlobCachePool>` here
    /// to share warm state with sibling mounts. The daemon pattern
    /// is to construct one pool at startup (sized from physical
    /// RAM) and hand the same `Arc` to every mount it spawns.
    pub blob_cache: Option<Arc<BlobCachePool>>,
}

impl<S: ObjectStore + 'static> ContentAddressedMount<S> {
    /// Open a writable mount of `thread` against `repo`.
    ///
    /// Resolves the thread once, up front, so every subsequent
    /// `lookup`/`read` walks from a fixed snapshot. Writes accumulate
    /// in the pending tier until [`Self::capture`] folds them into a
    /// new state. To advance to a newer state, call [`Self::refresh`].
    ///
    /// Equivalent to [`Self::with_options`] with default options:
    /// a fresh per-mount blob cache. Daemon callers that want
    /// cross-mount cache reuse should construct an
    /// [`Arc<BlobCachePool>`] once and use `with_options` instead.
    pub fn new(repo: Repository<RefManager, OpLog, S>, thread: impl Into<String>) -> Result<Self> {
        Self::with_options(repo, thread, MountOptions::default())
    }

    /// Construct a mount with explicit options. Lets the caller share
    /// a blob cache across mounts in the same process — see
    /// [`MountOptions::blob_cache`].
    pub fn with_options(
        repo: Repository<RefManager, OpLog, S>,
        thread: impl Into<String>,
        options: MountOptions,
    ) -> Result<Self> {
        let thread = thread.into();
        let state = resolve_thread(&repo, &thread)?;
        let inodes = Mutex::new(Inodes::new(state.tree));
        let blob_cache = options
            .blob_cache
            .unwrap_or_else(|| Arc::new(BlobCachePool::with_default_capacity()));
        let inner = Arc::new(MountInner {
            repo,
            thread,
            state: RwLock::new(state),
            inodes,
            pending: Mutex::new(Pending::default()),
            promotion: RwLock::new(PromotionPolicy::default()),
            mounted_at: SystemTime::now(),
            blob_cache,
            write_mu: Mutex::new(()),
        });
        let sweeper = spawn_sweep_worker(&inner);
        Ok(Self {
            inner,
            sweeper: Mutex::new(sweeper),
        })
    }

    /// Borrow the shared blob cache pool. Useful when the caller
    /// wants to spawn a [`BlobCachePool`]-aware pre-warmer or
    /// inspect cache stats.
    pub fn blob_cache_pool(&self) -> &Arc<BlobCachePool> {
        &self.inner.blob_cache
    }

    /// Override the promotion policy. Re-spawns (or terminates) the
    /// safety-sweep worker to honour the new `sweep_interval`.
    /// Mostly useful for tests that want a tight idle window or to
    /// disable idle-promotion entirely.
    pub fn with_promotion_policy(self, policy: PromotionPolicy) -> Self {
        // Terminate any pre-existing worker before mutating policy
        // so we never have two workers racing on `pending`.
        if let Some(mut handle) = self.sweeper.lock().expect("sweeper lock").take() {
            handle.signal_and_join();
        }
        // Swap the active policy in-place. The worker has been
        // joined above, so there's no concurrent reader.
        *self.inner.promotion.write().expect("promotion lock") = policy;
        // Spawn a fresh worker matching the new policy.
        let sweeper = spawn_sweep_worker(&self.inner);
        *self.sweeper.lock().expect("sweeper lock") = sweeper;
        self
    }

    /// Re-resolve the thread and adopt the new state. Existing
    /// inodes are *not* invalidated — callers who want a clean slate
    /// should drop the mount and recreate.
    pub fn refresh(&self) -> Result<()> {
        let next = resolve_thread(&self.inner.repo, &self.inner.thread)?;
        *self.inner.state.write().expect("mount state lock") = next;
        Ok(())
    }

    /// The thread name this mount serves.
    pub fn thread(&self) -> &str {
        &self.inner.thread
    }

    /// The change id this mount currently points at.
    pub fn current_change_id(&self) -> ChangeId {
        self.inner.state.read().expect("mount state lock").change_id
    }

    fn store(&self) -> &S {
        self.inner.repo.store()
    }

    fn load_tree(&self, hash: &ContentHash) -> Result<Tree> {
        self.store()
            .get_tree(hash)?
            .ok_or_else(|| MountError::NotFound(format!("tree {hash}")))
    }

    /// Drop every cached blob — both the mount-side LRU and the
    /// underlying `ObjectStore`'s `recent_blobs`/`recent_trees`
    /// caches. The next `read` on each blob pays full I/O +
    /// decompression cost. Exposed for benchmarks that want to
    /// measure the true cold-cache path without rebuilding the
    /// whole mount.
    pub fn clear_blob_cache(&self) {
        self.inner.blob_cache.clear();
        self.inner.repo.store().clear_recent_caches();
    }

    /// Spawn a background tree-walker that hydrates every file blob
    /// in the captured tree into the shared blob cache. The first
    /// kernel `read` after this finishes is served from memory at
    /// `Arc::clone` + `memcpy` cost — beats `std::fs::read` on every
    /// tier we benchmark.
    ///
    /// The returned [`PrewarmHandle`] is the caller's lever:
    ///   * Drop it without calling anything → the prewarmer keeps
    ///     running until natural completion or the mount drops
    ///     (the workers hold `Weak<MountInner>` and self-terminate
    ///     when the strong count hits zero).
    ///   * `.cancel()` signals shutdown without joining.
    ///   * `.wait()` joins all workers and returns the final stats.
    ///
    /// Workers stop early when the cache is ≥ 90% full to avoid
    /// churn-evicting work they just did. Blobs already cached
    /// (from a sibling mount sharing the same pool) are skipped
    /// cheaply — this is the fork-thread fast path.
    pub fn prewarm(&self) -> PrewarmHandle {
        PrewarmHandle::start(Arc::downgrade(&self.inner))
    }

    fn load_blob_bytes(&self, hash: &ContentHash) -> Result<bytes::Bytes> {
        if let Some(hit) = self.inner.blob_cache.get(hash) {
            return Ok(hit);
        }
        let bytes = self
            .store()
            .get_blob_bytes(hash)?
            .ok_or_else(|| MountError::NotFound(format!("blob {hash}")))?;
        self.inner.blob_cache.insert(*hash, bytes.clone());
        Ok(bytes)
    }

    /// Header-only size lookup. Avoids loading the full blob just to
    /// learn its size — the hot path for `ls -l`.
    fn blob_size(&self, hash: &ContentHash) -> Result<u64> {
        self.store()
            .blob_size(hash)?
            .ok_or_else(|| MountError::NotFound(format!("blob {hash}")))
    }

    fn record_for(&self, id: NodeId) -> Result<NodeRecord> {
        self.inner
            .inodes
            .lock()
            .expect("inode lock")
            .get(id)
            .ok_or_else(|| MountError::Stale(format!("node {}", id.0)))
    }

    fn intern(&self, record: NodeRecord) -> NodeId {
        self.inner.inodes.lock().expect("inode lock").intern(record)
    }

    /// Resolve a mount-relative path to a [`NodeId`]. Used by tests
    /// that don't go through `lookup` step-by-step.
    pub fn lookup_path(&self, path: impl AsRef<Path>) -> Result<NodeId> {
        let mut node = NodeId::ROOT;
        for component in path.as_ref().components() {
            match component {
                Component::CurDir | Component::RootDir => continue,
                Component::Prefix(_) => {
                    return Err(MountError::NotFound(format!(
                        "unsupported path component in {}",
                        path.as_ref().display()
                    )));
                }
                Component::ParentDir => {
                    return Err(MountError::NotFound(format!(
                        "parent traversal not supported: {}",
                        path.as_ref().display()
                    )));
                }
                Component::Normal(name) => {
                    let entry = self
                        .lookup(node, name)?
                        .ok_or_else(|| MountError::NotFound(name.to_string_lossy().into_owned()))?;
                    node = entry.node;
                }
            }
        }
        Ok(node)
    }

    fn entry_from_tree_entry(&self, parent_path: &Path, tree_entry: &TreeEntry) -> Result<Entry> {
        let entry_path = join_child(parent_path, &tree_entry.name);
        let (kind, size, unix_mode, record) = match tree_entry.entry_type {
            EntryType::Tree => {
                // We deliberately load the subtree here so the entry
                // count (the conventional "size" for a directory)
                // matches what userspace expects from `stat`.
                let subtree = self.load_tree(&tree_entry.hash)?;
                (
                    NodeKind::Directory,
                    subtree.entries().len() as u64,
                    DIR_UNIX_MODE,
                    NodeRecord::Dir {
                        tree: tree_entry.hash,
                        path: entry_path,
                    },
                )
            }
            EntryType::Blob => {
                let size = self.blob_size(&tree_entry.hash)?;
                let mode = tree_entry.mode;
                (
                    kind_for_mode(mode),
                    size,
                    mode.to_unix_mode(),
                    NodeRecord::File {
                        blob: tree_entry.hash,
                        mode,
                        path: entry_path,
                    },
                )
            }
            EntryType::Symlink => {
                let size = self.blob_size(&tree_entry.hash)?;
                (
                    NodeKind::Symlink,
                    size,
                    FileMode::Symlink.to_unix_mode(),
                    NodeRecord::Symlink {
                        blob: tree_entry.hash,
                    },
                )
            }
        };
        let node = self.intern(record);
        Ok(Entry {
            node,
            name: OsString::from(&tree_entry.name),
            kind,
            size,
            unix_mode,
        })
    }

    /// Build an [`Entry`] from a [`PendingHit`]. `path` is the child's
    /// mount-relative path (used to intern the `PendingFile` /
    /// `PendingSymlink` record for warm/symlink hits); `name` is the
    /// leaf name of the returned entry. Returns `None` for
    /// [`PendingHit::Tombstone`] — the caller treats that as "entry
    /// hidden". Shared by `lookup` and `enumerate`.
    fn entry_from_pending_hit(&self, hit: PendingHit, path: &Path, name: &OsStr) -> Option<Entry> {
        match hit {
            PendingHit::Tombstone => None,
            PendingHit::Hot { node, size, mode } => Some(Entry {
                node,
                name: name.to_os_string(),
                kind: kind_for_mode(mode),
                size,
                unix_mode: mode.to_unix_mode(),
            }),
            PendingHit::Warm {
                blob: _,
                size,
                mode,
            } => {
                let node = self.intern(NodeRecord::PendingFile {
                    path: path.to_path_buf(),
                    mode,
                });
                Some(Entry {
                    node,
                    name: name.to_os_string(),
                    kind: kind_for_mode(mode),
                    size,
                    unix_mode: mode.to_unix_mode(),
                })
            }
            PendingHit::Symlink { target_len } => {
                let node = self.intern(NodeRecord::PendingSymlink {
                    path: path.to_path_buf(),
                });
                Some(Entry {
                    node,
                    name: name.to_os_string(),
                    kind: NodeKind::Symlink,
                    size: target_len,
                    unix_mode: FileMode::Symlink.to_unix_mode(),
                })
            }
        }
    }

    fn tree_for_record(&self, record: &NodeRecord) -> Result<Tree> {
        match record {
            NodeRecord::Root { tree } | NodeRecord::Dir { tree, .. } => self.load_tree(tree),
            // Pending-only dirs have no captured tree to load yet —
            // their content lives entirely in the pending tier.
            NodeRecord::PendingDir { .. } => Ok(Tree::new()),
            _ => Err(MountError::NotADirectory(format!("{record:?}"))),
        }
    }

    /// Mount-relative path for a directory record. Root resolves to
    /// `""`, captured Dirs and pending dirs to their stored path.
    fn dir_path_of(&self, record: &NodeRecord) -> Option<PathBuf> {
        match record {
            NodeRecord::Root { .. } => Some(PathBuf::new()),
            NodeRecord::Dir { path, .. } | NodeRecord::PendingDir { path } => Some(path.clone()),
            _ => None,
        }
    }

    /// Build the relative path of `node` from the mount root, used to
    /// rendezvous a NodeId with its pending-tier entry. Returns `None`
    /// for the root or for nodes that don't carry a path identity.
    fn path_of(&self, record: &NodeRecord) -> Option<PathBuf> {
        match record {
            NodeRecord::PendingFile { path, .. } | NodeRecord::File { path, .. } => {
                Some(path.clone())
            }
            NodeRecord::Dir { path, .. } | NodeRecord::PendingDir { path } => Some(path.clone()),
            NodeRecord::PendingSymlink { path } => Some(path.clone()),
            _ => None,
        }
    }

    // --- Pending tier helpers ------------------------------------------------

    fn promote_idle_buffers(&self) -> Result<()> {
        self.inner.sweep_idle_buffers()
    }

    /// Promote the hot buffer for `node` (if any) to a CAS blob and
    /// record it in the pending tree. Routed from the FUSE `flush`
    /// callback (per-descriptor-close). Orphaned nodes deliberately
    /// do nothing here — see [`MountInner::flush_node`] for the
    /// lifecycle rationale.
    pub fn flush_node(&self, node: NodeId) -> Result<()> {
        self.inner.flush_node(node)
    }

    /// Final close of `node` from a FUSE `release` callback. Decrements
    /// the open-handle refcount; on the last close, drops orphan
    /// state and (for non-orphans) promotes any surviving hot buffer.
    pub fn release_node(&self, node: NodeId) -> Result<()> {
        self.inner.release_node(node)
    }

    /// Notify the mount that a new open handle for `node` was minted
    /// (FUSE `open` / `create` callback). Used to time the orphan
    /// cleanup against the *final* close (see
    /// [`Self::release_node`] / [`MountInner::release_node`]).
    ///
    /// Bumps the open count on the existing `NodeState`, minting a
    /// `Live { open_count: 1 }` entry if the node is untracked. An
    /// Orphan can also be opened (rare — only via an fh the kernel
    /// still holds across a re-lookup race); we bump its refcount so
    /// the final release fires correctly.
    pub fn on_open(&self, node: NodeId) -> Result<()> {
        let mut pending = self.inner.pending.lock().expect("pending lock");
        let next = match pending.state.get(&node.0).copied() {
            None => NodeState::Live { open_count: 1 },
            Some(NodeState::Live { open_count }) => NodeState::Live {
                open_count: open_count.saturating_add(1),
            },
            Some(NodeState::Orphan { open_count }) => NodeState::Orphan {
                open_count: open_count.saturating_add(1),
            },
        };
        pending.state.insert(node.0, next);
        Ok(())
    }

    /// Mark `path` as deleted in the pending tier. Subsequent
    /// `lookup`/`enumerate` calls will skip the underlying captured
    /// entry, and `capture()` will fold the deletion into the new
    /// state's tree (pruning empty parent dirs as needed).
    ///
    /// Low-level (path-based) helper — unlike [`Self::unlink_entry`]
    /// it does not honour POSIX open-unlinked semantics. Used by
    /// tests that bypass the FUSE-callback lifecycle. The
    /// NodeId-keyed buffers for the path's current owner are dropped
    /// (no orphan tracking).
    pub fn unlink_path(&self, path: impl AsRef<Path>) -> Result<()> {
        let path = path.as_ref().to_path_buf();
        // Resolve path → NodeId via the inode registry so we can drop
        // the per-NodeId warm/hot bytes.
        let bound_id = {
            let inodes = self.inner.inodes.lock().expect("inode lock");
            inodes.by_path.get(&path).copied()
        };
        let mut pending = self.inner.pending.lock().expect("pending lock");
        if let Some(node_id) = pending.hot_by_path.remove(&path) {
            pending.hot.remove(&node_id);
            pending.warm.remove(&node_id);
            pending.state.remove(&node_id);
        }
        if let Some(node_id) = bound_id {
            pending.hot.remove(&node_id);
            pending.warm.remove(&node_id);
            pending.state.remove(&node_id);
        }
        pending.symlinks.remove(&path);
        pending.tombstones.insert(path.clone());
        drop(pending);
        if bound_id.is_some() {
            let mut inodes = self.inner.inodes.lock().expect("inode lock");
            inodes.by_path.remove(&path);
        }
        Ok(())
    }

    // --- Write-side overlay ops (heddle#180) -----------------------------------
    //
    // Each method below corresponds to one FUSE callback the kernel
    // emits on cargo / git / npm style workloads:
    //
    //   create  → `create_file`      open(O_CREAT)
    //   mkdir   → `make_dir`
    //   unlink  → `unlink_entry`
    //   rmdir   → `rmdir_entry`
    //   rename  → `rename_entry`
    //   setattr → `set_attrs`        chmod / ftruncate / O_TRUNC
    //   symlink → `create_symlink`
    //   readlink→ `read_link`
    //
    // All mutations land in the per-thread overlay (pending tier):
    //
    //   * `Pending::hot` / `Pending::warm` — file bytes (existing).
    //   * `Pending::tombstones`            — file deletions (existing).
    //   * `Pending::dir_tombstones`        — `rmdir` of a captured dir.
    //   * `Pending::explicit_dirs`         — empty mkdirs.
    //   * `Pending::symlinks`              — link target bytes.
    //
    // None of these touch the underlying CAS until `capture()` folds
    // the overlay into a real heddle state.

    /// Open-or-create a regular file under `parent`, mirroring
    /// `open(O_CREAT[|O_EXCL])` from userspace.
    ///
    /// When the named entry doesn't exist, mints a fresh
    /// [`NodeRecord::PendingFile`] inode + an empty hot buffer so the
    /// new path is immediately visible to [`lookup`](Self::lookup) /
    /// [`attrs`](Self::attrs) and the first
    /// [`write`](Self::write) drops cleanly into the existing
    /// two-tier model.
    ///
    /// When the named entry already exists:
    ///   * `exclusive=true` ⇒ [`MountError::AlreadyExists`] (errno
    ///     `EEXIST`).
    ///   * `exclusive=false` ⇒ returns the existing entry. The kernel
    ///     follows up with `setattr(size=0)` for `O_TRUNC` callers,
    ///     which we honour in [`set_attrs`](Self::set_attrs).
    pub fn create_file(
        &self,
        parent: NodeId,
        name: &OsStr,
        mode: FileMode,
        exclusive: bool,
    ) -> Result<Entry> {
        // R8: serialize against rename / mkdir / symlink so an
        // exclusivity check (O_EXCL or rename-noreplace) lands its
        // existence-test and its mutation under the same write-side
        // critical section.
        let _write_guard = self.inner.write_mu.lock().expect("write mu");
        let name_str = validate_entry_name(name)?;
        if let Some(existing) = self.lookup(parent, name)? {
            if exclusive {
                return Err(MountError::AlreadyExists(name_str.to_string()));
            }
            return Ok(existing);
        }
        let parent_record = self.record_for(parent)?;
        let parent_path = self
            .dir_path_of(&parent_record)
            .ok_or_else(|| MountError::NotADirectory(format!("{parent_record:?}")))?;
        let child_path = join_child(&parent_path, name_str);

        {
            let mut pending = self.inner.pending.lock().expect("pending lock");
            // A prior unlink left a tombstone — clear it; the file
            // exists again.
            pending.tombstones.remove(&child_path);
            // An overlay-only directory used to live here; it's gone.
            pending.explicit_dirs.remove(&child_path);
        }

        let node = self.intern(NodeRecord::PendingFile {
            path: child_path.clone(),
            mode,
        });

        // Seed an empty hot buffer so the freshly-minted inode reads
        // as a 0-byte file even before any `write` callback fires.
        // Mirrors what userspace expects from `open(O_CREAT)`: the
        // file exists at length 0 immediately on return.
        {
            let mut pending = self.inner.pending.lock().expect("pending lock");
            pending.hot.insert(
                node.0,
                HotBuffer {
                    path: child_path.clone(),
                    mode,
                    bytes: Vec::new(),
                    last_touched: Instant::now(),
                },
            );
            pending.hot_by_path.insert(child_path, node.0);
        }

        Ok(Entry {
            node,
            name: name.to_os_string(),
            kind: kind_for_mode(mode),
            size: 0,
            unix_mode: mode.to_unix_mode(),
        })
    }

    /// Create an empty directory under `parent`. Recorded as an
    /// [`Pending::explicit_dirs`] entry so the new path is visible to
    /// lookup/enumerate even when no child has been written yet.
    pub fn make_dir(&self, parent: NodeId, name: &OsStr) -> Result<Entry> {
        // R8: serialize with other write-side mutations.
        let _write_guard = self.inner.write_mu.lock().expect("write mu");
        let name_str = validate_entry_name(name)?;
        if self.lookup(parent, name)?.is_some() {
            return Err(MountError::AlreadyExists(name_str.to_string()));
        }
        let parent_record = self.record_for(parent)?;
        let parent_path = self
            .dir_path_of(&parent_record)
            .ok_or_else(|| MountError::NotADirectory(format!("{parent_record:?}")))?;
        let child_path = join_child(&parent_path, name_str);

        {
            let mut pending = self.inner.pending.lock().expect("pending lock");
            // A rmdir of this exact path now reverts to "present".
            pending.dir_tombstones.remove(&child_path);
            // Clear any colliding file tombstone too.
            pending.tombstones.remove(&child_path);
            pending.explicit_dirs.insert(child_path.clone());
        }

        let node = self.intern(NodeRecord::PendingDir { path: child_path });
        Ok(Entry {
            node,
            name: name.to_os_string(),
            kind: NodeKind::Directory,
            size: 0,
            unix_mode: DIR_UNIX_MODE,
        })
    }

    /// Delete a regular file (or symlink) named `name` under `parent`.
    ///
    /// POSIX open-unlinked semantics: the directory entry goes (path
    /// tombstoned, `inodes.by_path[path]` retired), but if any fd
    /// still references the inode, the bytes survive in `hot[node]` /
    /// `warm[node]` until the final `release`. Under the post-spike
    /// unified NodeId-keyed model
    /// (`docs/design/mount-posix-semantics.md` §1.2 T1/T2), this is a
    /// state transition only — no byte migration. Pre-spike code
    /// dropped `pending.hot[node_id]` here (Codex thread 3293307302
    /// r9) and migrated `warm[path]` into `orphan_warm[node]` (r8);
    /// both steps go away.
    pub fn unlink_entry(&self, parent: NodeId, name: &OsStr) -> Result<()> {
        // R8: serialize with other write-side mutations.
        let _write_guard = self.inner.write_mu.lock().expect("write mu");
        let name_str = validate_entry_name(name)?;
        let entry = self
            .lookup(parent, name)?
            .ok_or_else(|| MountError::NotFound(name_str.to_string()))?;
        if entry.kind == NodeKind::Directory {
            return Err(MountError::IsADirectory(name_str.to_string()));
        }
        let parent_record = self.record_for(parent)?;
        let parent_path = self
            .dir_path_of(&parent_record)
            .ok_or_else(|| MountError::NotADirectory(format!("{parent_record:?}")))?;
        let child_path = join_child(&parent_path, name_str);
        let node_id = entry.node.0;

        {
            let mut pending = self.inner.pending.lock().expect("pending lock");
            // Detach the path-level hot binding. The bytes follow the
            // NodeId, so `hot[node_id]` / `warm[node_id]` stay put —
            // the surviving fd reads them via the orphan branches in
            // `read` / `attrs` / `write` / `apply_truncate`.
            // (r9 fix: pre-spike code called `pending.hot.remove(&node_id)`
            // here and the unflushed bytes vanished.)
            pending.hot_by_path.remove(&child_path);
            // Transition T1: Live{open_count >= 1} → Orphan{open_count}.
            // The witness-gated retrofit (heddle#209) makes the FSM
            // check the gate: `bp.transition_to_orphan(node_id)`
            // returns `None` (without touching `state`) for any
            // non-`LiveNonZero` state, and the missing
            // `Witness<Orphan>` IS the short-circuit at this call
            // site.
            //
            // That subsumes two earlier defensive checks: Codex r12
            // thread 3293510317 (symlinks have no `open`/`release`
            // lifecycle, so they never enter `state` and the
            // transition never fires for them), and r11 finding
            // 3293575534 (orphaning a `Live { open_count: 0 }` node
            // creates a record nothing will ever reap — same shape,
            // same fix).
            pending.with_brand(|bp| {
                let _ = bp.transition_to_orphan(node_id);
            });
            // Symlinks are path-keyed; their overlay goes when the
            // directory entry goes.
            pending.symlinks.remove(&child_path);
            pending.tombstones.insert(child_path.clone());
        }
        // Retire the path→inode mapping so a subsequent `create_file`
        // at the same name mints a fresh inode (POSIX unlink/recreate
        // isolation — open-unlinked temp files must not be aliased by
        // a replacement at the same path). The `by_id` record stays so
        // any still-open kernel handle keeps resolving until `forget`.
        {
            let mut inodes = self.inner.inodes.lock().expect("inode lock");
            inodes.by_path.remove(&child_path);
        }
        Ok(())
    }

    /// Remove the empty directory `name` under `parent`. Fails with
    /// `ENOTEMPTY` if any child resolves through the mount.
    pub fn rmdir_entry(&self, parent: NodeId, name: &OsStr) -> Result<()> {
        // R8: serialize with other write-side mutations.
        let _write_guard = self.inner.write_mu.lock().expect("write mu");
        let name_str = validate_entry_name(name)?;
        let entry = self
            .lookup(parent, name)?
            .ok_or_else(|| MountError::NotFound(name_str.to_string()))?;
        if entry.kind != NodeKind::Directory {
            return Err(MountError::NotADirectory(name_str.to_string()));
        }
        // Empty check via enumerate — already overlay-aware (hot,
        // warm, symlinks, captured-with-pending-overlay).
        let children = self.enumerate(entry.node)?;
        if !children.is_empty() {
            return Err(MountError::NotEmpty(name_str.to_string()));
        }
        let parent_record = self.record_for(parent)?;
        let parent_path = self
            .dir_path_of(&parent_record)
            .ok_or_else(|| MountError::NotADirectory(format!("{parent_record:?}")))?;
        let child_path = join_child(&parent_path, name_str);

        {
            let mut pending = self.inner.pending.lock().expect("pending lock");
            pending.explicit_dirs.remove(&child_path);
            pending.dir_tombstones.insert(child_path.clone());
        }
        // Codex r12 thread 3293510310 (P1): retire the path → inode
        // mapping. Otherwise `Inodes::intern` would coalesce a
        // subsequent `create_file` / `make_dir` at this path onto the
        // removed directory's NodeId, rebinding a cached directory
        // inode to a different object type — the same stale-handle
        // class `unlink_entry` already guards against. The `by_id`
        // record stays so any kernel handle the FS still holds keeps
        // resolving until `forget`.
        {
            let mut inodes = self.inner.inodes.lock().expect("inode lock");
            inodes.by_path.remove(&child_path);
        }
        Ok(())
    }

    /// Move `(old_parent, old_name)` to `(new_parent, new_name)`.
    /// Handles file + symlink renames across any pair of overlay /
    /// captured paths, and overlay-only directory rename (a captured
    /// directory rename would require recursively rewriting the
    /// tombstone/warm map — out of scope for the cargo / git path).
    pub fn rename_entry(
        &self,
        old_parent: NodeId,
        old_name: &OsStr,
        new_parent: NodeId,
        new_name: &OsStr,
    ) -> Result<()> {
        self.rename_entry_with_options(
            old_parent,
            old_name,
            new_parent,
            new_name,
            RenameOptions::default(),
        )
    }

    /// Same as [`Self::rename_entry`] but honours [`RenameOptions`].
    /// `no_replace` (Linux `RENAME_NOREPLACE`) refuses the rename when
    /// the destination already resolves; the check is performed inside
    /// the same write-side critical section as the mutation, so a
    /// concurrent writer cannot install the destination between the
    /// check and the rename.
    pub fn rename_entry_with_options(
        &self,
        old_parent: NodeId,
        old_name: &OsStr,
        new_parent: NodeId,
        new_name: &OsStr,
        options: RenameOptions,
    ) -> Result<()> {
        // R8 (Codex Thread 3293235163): the existence-check + the
        // directory-entry mutation must land under the same mutation
        // lock. Holding `write_mu` for the duration of this method
        // serializes the rename against every other write-side op
        // that could install the destination (create_file, make_dir,
        // create_symlink, another rename) — that's the atomicity the
        // POSIX NOREPLACE flag promises.
        let _write_guard = self.inner.write_mu.lock().expect("write mu");

        let old_name_str = validate_entry_name(old_name)?;
        let new_name_str = validate_entry_name(new_name)?;
        let src = self
            .lookup(old_parent, old_name)?
            .ok_or_else(|| MountError::NotFound(format!("rename src {old_name_str}")))?;
        let old_parent_record = self.record_for(old_parent)?;
        let new_parent_record = self.record_for(new_parent)?;
        let old_parent_path = self
            .dir_path_of(&old_parent_record)
            .ok_or_else(|| MountError::NotADirectory(format!("{old_parent_record:?}")))?;
        let new_parent_path = self
            .dir_path_of(&new_parent_record)
            .ok_or_else(|| MountError::NotADirectory(format!("{new_parent_record:?}")))?;
        let old_path = join_child(&old_parent_path, old_name_str);
        let new_path = join_child(&new_parent_path, new_name_str);
        if old_path == new_path {
            return Ok(());
        }

        // POSIX: destination of a different kind is an error. We also
        // honour NOREPLACE here while still holding `write_mu` so the
        // check + the subsequent move are atomic against concurrent
        // writers. `dst` is shadowed for the kind-mismatch arm and
        // hoisted into `displaced_inode_id` so the move primitives
        // can preserve the displaced inode's warm bytes (r8).
        let dst = self.lookup(new_parent, new_name)?;
        if dst.is_some() && options.no_replace {
            return Err(MountError::AlreadyExists(new_name_str.to_string()));
        }
        if let Some(ref d) = dst {
            match (src.kind, d.kind) {
                (NodeKind::Directory, NodeKind::Directory) => {
                    let dst_children = self.enumerate(d.node)?;
                    if !dst_children.is_empty() {
                        return Err(MountError::NotEmpty(new_name_str.to_string()));
                    }
                }
                (NodeKind::Directory, _) => {
                    return Err(MountError::NotADirectory(new_name_str.to_string()));
                }
                (_, NodeKind::Directory) => {
                    return Err(MountError::IsADirectory(new_name_str.to_string()));
                }
                _ => {}
            }
        }
        let displaced_inode_id = dst.as_ref().map(|d| d.node.0);

        match src.kind {
            NodeKind::File => self.move_file(&old_path, &new_path, displaced_inode_id)?,
            NodeKind::Symlink => self.move_symlink(&old_path, &new_path, displaced_inode_id)?,
            NodeKind::Directory => self.move_overlay_dir(&old_path, &new_path)?,
        }
        // Maintain the inode↔path invariant for both the source and
        // destination: the kernel may have cached either dentry from
        // a prior lookup, and (for FUSE) it does not re-issue lookup
        // after `rename` — it just rewrites its own dentry → inode
        // table. So the source inode now resolves through dentry
        // `new_name`, and any read against it must serve the new
        // path's overlay state. Rewriting the source record's stored
        // path is what keeps that consistent. The dest's old inode
        // (which the kernel will issue `forget` for) gets dropped
        // from `by_path` so the next lookup mints a fresh id.
        {
            let mut inodes = self.inner.inodes.lock().expect("inode lock");
            // Detach the destination's path mapping. The inode record
            // stays in `by_id` so any kernel handle the FS still holds
            // for the replaced file keeps resolving (the kernel cleans
            // up via `forget` on close). POSIX semantics: rename-over
            // must not invalidate an already-open dest descriptor.
            let displaced_dest = inodes.by_path.remove(&new_path);
            // Rewrite the source inode's stored path so subsequent
            // reads/attrs against it serve the new-path overlay.
            // The kernel keeps using the source's NodeId after rename
            // (it's just a dentry-table rewrite on its side) — without
            // this, every read against the rebased dentry sees the
            // stale path and returns ESTALE.
            let rebased_src = if let Some(src_id) = inodes.by_path.remove(&old_path) {
                if let Some(
                    NodeRecord::PendingFile { path, .. }
                    | NodeRecord::File { path, .. }
                    | NodeRecord::PendingSymlink { path }
                    | NodeRecord::Dir { path, .. }
                    | NodeRecord::PendingDir { path },
                ) = inodes.by_id.get_mut(&src_id)
                {
                    *path = new_path.clone();
                }
                inodes.by_path.insert(new_path.clone(), src_id);
                // For a directory rename, also rebase every cached
                // descendant inode. The kernel may already hold dentry
                // → inode bindings for `old_path/<child>` from prior
                // lookups, and reads against those inodes would
                // otherwise resolve through the stale path (ESTALE on
                // PendingFile, or the wrong overlay on File). Walk
                // by_path once, collect the entries under the old
                // prefix, then rewrite both the mapping and the
                // NodeRecord's stored path.
                if src.kind == NodeKind::Directory {
                    let descendants: Vec<(PathBuf, PathBuf, u64)> = inodes
                        .by_path
                        .iter()
                        .filter_map(|(p, id)| {
                            let tail = p.strip_prefix(&old_path).ok()?;
                            if tail.as_os_str().is_empty() {
                                return None;
                            }
                            Some((p.clone(), new_path.join(tail), *id))
                        })
                        .collect();
                    for (old_key, new_key, id) in descendants {
                        inodes.by_path.remove(&old_key);
                        if let Some(
                            NodeRecord::PendingFile { path, .. }
                            | NodeRecord::File { path, .. }
                            | NodeRecord::PendingSymlink { path }
                            | NodeRecord::Dir { path, .. }
                            | NodeRecord::PendingDir { path },
                        ) = inodes.by_id.get_mut(&id)
                        {
                            *path = new_key.clone();
                        }
                        inodes.by_path.insert(new_key, id);
                    }
                }
                Some(src_id)
            } else {
                None
            };
            drop(inodes);
            // Reach into pending for two cleanups under one lock:
            //   * The source's hot buffer (if any) carries the old
            //     path; rebase it. Descendant hot-buffer paths are
            //     already handled by `move_overlay_dir`'s
            //     `hot_path_updates` pass.
            //   * The displaced destination (if any) becomes an
            //     orphan: its directory entry is gone but the inode
            //     id may still be held by a kernel fd. Subsequent
            //     `write` / `apply_truncate` / `set_attrs` /
            //     `read` / `attrs` calls through that fd consult
            //     `Pending::orphans` and take the per-NodeId branch
            //     instead of the rebased path overlay. The companion
            //     orphan branch in `flush_node` drops any preserved
            //     buffer without warm-promoting.
            let mut pending = self.inner.pending.lock().expect("pending lock");
            if let Some(src_id) = rebased_src
                && let Some(buf) = pending.hot.get_mut(&src_id)
            {
                buf.path = new_path.clone();
            }
            if let Some(dest_id) = displaced_dest {
                // T3: the displaced destination transitions to Orphan
                // iff it's currently `Live { open_count >= 1 }`. Bytes
                // (hot[dest_id], warm[dest_id]) stay put so the
                // surviving fd keeps reading the inode's own data
                // (spike doc §1.2 T3).
                //
                // Closes Codex PR #182 r11 finding 3293575541 (heddle
                // #209): `bp.transition_to_orphan(dest_id)` returns
                // `None` (without touching `state`) for any
                // non-`LiveNonZero` displaced destination, and the
                // missing `Witness<Orphan>` IS the short-circuit at
                // this call site. Pre-retrofit this branch
                // unconditionally inserted `Orphan { open_count: 0 }`
                // for non-`Live` destinations — including symlinks,
                // which have no `open`/`release` lifecycle and would
                // never reap the entry, growing `state` under symlink
                // churn until capture / invalidate.
                pending.with_brand(|bp| {
                    let _ = bp.transition_to_orphan(dest_id);
                });
            }
        }
        Ok(())
    }

    /// Rename a regular file. Under the post-spike unified
    /// NodeId-keyed model
    /// (`docs/design/mount-posix-semantics.md` §2.4), the source's
    /// bytes follow its NodeId — no byte migration step. The displaced
    /// destination keeps its own `hot[id]` / `warm[id]` so the
    /// surviving fd reads its own data. The work here is path-level:
    /// retire the destination's path-keyed hot binding, rebase the
    /// source's hot buffer's `path` field (so a subsequent `flush`
    /// promotes under the new path), seed warm if the source had only
    /// captured-tree bytes (so capture can plant the file at the new
    /// path), and tombstone the old path.
    ///
    /// `displaced_inode_id` is no longer used as a side-channel for
    /// byte preservation — the caller (`rename_entry_with_options`)
    /// handles the orphan state transition independently.
    fn move_file(
        &self,
        old_path: &Path,
        new_path: &Path,
        displaced_inode_id: Option<u64>,
    ) -> Result<()> {
        // Snapshot whether the source has a hot buffer (drain it to
        // warm so the warm tier becomes authoritative for capture
        // under the new path) and whether the source is captured-only
        // (then synthesize a warm entry so capture plants the bytes
        // at new_path).
        let src_id_opt = self
            .inner
            .pending
            .lock()
            .expect("pending lock")
            .hot_by_path
            .get(old_path)
            .copied();
        if let Some(id) = src_id_opt {
            self.flush_node(NodeId(id))?;
        }
        // After the flush, the source's bytes (if any) live in
        // `warm[src_id]`. If the source had no warm entry — captured
        // only — synthesize one keyed by the source's NodeId so
        // `apply_pending_to_tree` plants the file under new_path. We
        // resolve src_id via the path → inode reverse-index (or via
        // the captured-tree walk for a captured-only source).
        let src_id = {
            let inodes = self.inner.inodes.lock().expect("inode lock");
            inodes.by_path.get(old_path).copied()
        };
        let needs_synth = match src_id {
            Some(id) => !self
                .inner
                .pending
                .lock()
                .expect("pending lock")
                .warm
                .contains_key(&id),
            None => true,
        };
        let captured_seed = if needs_synth {
            // Captured-only source: pull (blob, mode, size) from the
            // captured tree so the rename survives `capture`.
            Some(self.captured_file_at(old_path)?)
        } else {
            None
        };

        let mut pending = self.inner.pending.lock().expect("pending lock");
        // Detach the destination's path-keyed hot binding. The
        // displaced inode's bytes are keyed by NodeId — they stay put
        // for the surviving fd. POSIX rename-over: open destination
        // descriptors keep referencing the displaced inode until close.
        pending.hot_by_path.remove(new_path);
        // Symlinks are path-keyed; clear at both endpoints.
        pending.symlinks.remove(new_path);
        pending.symlinks.remove(old_path);
        // Source: if a hot buffer survived the flush above (only
        // possible if the source was Orphan, which can't happen for
        // a valid rename source — but be defensive), rebase its
        // path-binding.
        if let Some(id) = pending.hot_by_path.remove(old_path) {
            if let Some(buf) = pending.hot.get_mut(&id) {
                buf.path = new_path.to_path_buf();
            }
            pending.hot_by_path.insert(new_path.to_path_buf(), id);
        }
        // Captured-only source: synthesize a warm entry so capture
        // plants the bytes at new_path. The entry is keyed by the
        // source's NodeId; `apply_pending_to_tree` resolves its
        // current path through `inodes.by_path`.
        if let (Some(id), Some((blob, mode, size))) = (src_id, captured_seed) {
            pending.warm.insert(id, PendingEntry { blob, mode, size });
        }
        // Path-level bookkeeping: tombstone old_path so the captured
        // tree's old entry is hidden; clear any tombstone at
        // new_path (rename made it valid again).
        pending.tombstones.insert(old_path.to_path_buf());
        pending.tombstones.remove(new_path);
        // The displaced inode is handled by the caller via the
        // NodeState transition; no byte work here.
        let _ = displaced_inode_id;
        Ok(())
    }

    fn move_symlink(
        &self,
        old_path: &Path,
        new_path: &Path,
        displaced_inode_id: Option<u64>,
    ) -> Result<()> {
        // Resolve target bytes from the pending overlay or the
        // captured-tree blob — symlinks are path-keyed (not openable
        // for IO; no orphan story applies).
        let target_bytes = {
            let pending = self.inner.pending.lock().expect("pending lock");
            pending.symlinks.get(old_path).cloned()
        };
        let target_bytes = match target_bytes {
            Some(b) => b,
            None => {
                let blob = self.captured_symlink_at(old_path)?;
                (*self.load_blob_bytes(&blob)?).to_vec()
            }
        };
        let mut pending = self.inner.pending.lock().expect("pending lock");
        // Detach the displaced destination's path-keyed hot binding.
        // Its NodeId-keyed bytes stay put for the surviving fd; the
        // caller's NodeState transition handles the orphan tracking.
        pending.hot_by_path.remove(new_path);
        pending.symlinks.remove(new_path);
        pending.symlinks.remove(old_path);
        pending
            .symlinks
            .insert(new_path.to_path_buf(), target_bytes);
        pending.tombstones.remove(new_path);
        pending.tombstones.insert(old_path.to_path_buf());
        let _ = displaced_inode_id;
        Ok(())
    }

    fn move_overlay_dir(&self, old_path: &Path, new_path: &Path) -> Result<()> {
        // We only support overlay-only directory renames here. If the
        // source dir has any captured-tree backing, refuse — a full
        // captured-tree rename would need to rewrite every descendant
        // tombstone entry.
        if self.captured_dir_exists(old_path)? {
            return Err(MountError::InvalidArgument(format!(
                "cross-tree directory rename {} → {} not supported by the overlay",
                old_path.display(),
                new_path.display()
            )));
        }
        let mut pending = self.inner.pending.lock().expect("pending lock");
        // Path-keyed structures under `old_path/` need to be rebased
        // to `new_path/`. Warm bytes follow the NodeId (unified shape)
        // so warm[id] is unaffected by this rewrite — descendant
        // NodeRecord paths get rebased in `rename_entry_with_options`.
        fn rebase(p: &Path, old: &Path, new: &Path) -> Option<PathBuf> {
            let tail = p.strip_prefix(old).ok()?;
            Some(new.join(tail))
        }
        let mut new_explicit: BTreeSet<PathBuf> = BTreeSet::new();
        let mut new_symlinks: BTreeMap<PathBuf, Vec<u8>> = BTreeMap::new();
        let mut new_tombstones: BTreeSet<PathBuf> = BTreeSet::new();
        let mut new_hot_by_path: BTreeMap<PathBuf, u64> = BTreeMap::new();
        let mut hot_path_updates: Vec<(u64, PathBuf)> = Vec::new();
        for explicit in std::mem::take(&mut pending.explicit_dirs) {
            match rebase(&explicit, old_path, new_path) {
                Some(rebased) => {
                    new_explicit.insert(rebased);
                }
                None => {
                    if explicit != old_path {
                        new_explicit.insert(explicit);
                    }
                }
            }
        }
        for (path, target) in std::mem::take(&mut pending.symlinks) {
            match rebase(&path, old_path, new_path) {
                Some(rebased) => {
                    new_symlinks.insert(rebased, target);
                }
                None => {
                    new_symlinks.insert(path, target);
                }
            }
        }
        for path in std::mem::take(&mut pending.tombstones) {
            match rebase(&path, old_path, new_path) {
                Some(rebased) => {
                    new_tombstones.insert(rebased);
                }
                None => {
                    new_tombstones.insert(path);
                }
            }
        }
        for (path, id) in std::mem::take(&mut pending.hot_by_path) {
            match rebase(&path, old_path, new_path) {
                Some(rebased) => {
                    hot_path_updates.push((id, rebased.clone()));
                    new_hot_by_path.insert(rebased, id);
                }
                None => {
                    new_hot_by_path.insert(path, id);
                }
            }
        }
        // Rewrite hot-buffer path fields to match.
        for (id, new_p) in hot_path_updates {
            if let Some(buf) = pending.hot.get_mut(&id) {
                buf.path = new_p;
            }
        }
        // Ensure the destination directory itself is registered.
        new_explicit.insert(new_path.to_path_buf());
        pending.explicit_dirs = new_explicit;
        pending.symlinks = new_symlinks;
        pending.tombstones = new_tombstones;
        pending.hot_by_path = new_hot_by_path;
        Ok(())
    }

    /// Resolve a captured-tree file at `path`; returns its
    /// `(blob, mode, size)`. Errors with `NotFound` if no captured
    /// entry exists.
    fn captured_file_at(&self, path: &Path) -> Result<(ContentHash, FileMode, u64)> {
        let entry = self.captured_tree_entry(path)?;
        let mode = entry.mode;
        let size = self.blob_size(&entry.hash)?;
        Ok((entry.hash, mode, size))
    }

    fn captured_symlink_at(&self, path: &Path) -> Result<ContentHash> {
        let entry = self.captured_tree_entry(path)?;
        if !matches!(entry.entry_type, objects::object::EntryType::Symlink) {
            return Err(MountError::InvalidArgument(format!(
                "{} is not a symlink in the captured tree",
                path.display()
            )));
        }
        Ok(entry.hash)
    }

    fn captured_tree_entry(&self, path: &Path) -> Result<TreeEntry> {
        let root_record = self.record_for(NodeId::ROOT)?;
        let mut tree = self.tree_for_record(&root_record)?;
        let comps: Vec<&str> = path
            .components()
            .filter_map(|c| match c {
                Component::Normal(n) => n.to_str(),
                _ => None,
            })
            .collect();
        let (leaf, dirs) = comps
            .split_last()
            .ok_or_else(|| MountError::NotFound(path.display().to_string()))?;
        for d in dirs {
            let e = tree
                .get(d)
                .ok_or_else(|| MountError::NotFound(path.display().to_string()))?;
            if !e.is_tree() {
                return Err(MountError::NotADirectory(d.to_string()));
            }
            tree = self.load_tree(&e.hash)?;
        }
        let entry = tree
            .get(leaf)
            .cloned()
            .ok_or_else(|| MountError::NotFound(path.display().to_string()))?;
        Ok(entry)
    }

    fn captured_dir_exists(&self, path: &Path) -> Result<bool> {
        match self.captured_tree_entry(path) {
            Ok(e) => Ok(e.is_tree()),
            Err(MountError::NotFound(_)) => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// Apply attribute updates from a FUSE `setattr` / FSKit
    /// `setattr` / etc. Returns post-update [`Attrs`] for an
    /// inline reply.
    pub fn set_attrs(&self, node: NodeId, update: AttrUpdate) -> Result<Attrs> {
        // Codex r13 thread 3293733165 (P1): every mutating branch of
        // `set_attrs` must serialize against `rename` / `create` /
        // `unlink` / `rmdir` under `write_mu`. Without it, a
        // `setattr(size=...)` racing with a `rename` re-uses the
        // pre-rename pathname in `apply_truncate`'s phase-2
        // bookkeeping — `tombstones.remove(old)` clears the rename's
        // tombstone and `hot_by_path.insert(old, node)` resurrects
        // the file at the old name. The mode-mutation branch has the
        // same shape (touches `hot_by_path[path]` / `warm[id]` derived
        // from `inodes.by_path[path]`), so we hold the lock for the
        // whole mutating prologue.
        let _write_guard = self.inner.write_mu.lock().expect("write mu");

        // Mode mutation: only meaningful for file-kind records.
        if let Some(raw_mode) = update.mode {
            // Codex r13 thread 3293733164 (P2): the Normal↔Executable
            // fold is gated on the user execute bit (S_IXUSR = 0o100)
            // only, not on any of the three execute bits. A
            // `chmod 0o010` (group execute only) must leave the record
            // as Normal — otherwise capture would persist a
            // `FileMode::Executable` and grant owner+other execute
            // bits the agent never requested.
            let new_mode = if (raw_mode & 0o100) != 0 {
                FileMode::Executable
            } else {
                FileMode::Normal
            };
            let mut inodes = self.inner.inodes.lock().expect("inode lock");
            if let Some(NodeRecord::File { mode, .. } | NodeRecord::PendingFile { mode, .. }) =
                inodes.by_id.get_mut(&node.0)
            {
                *mode = new_mode;
            }
            drop(inodes);
            // Reflect the mode in any open hot buffer + warm-tier
            // entry so a subsequent `capture` keeps the new mode.
            let record = self.record_for(node)?;
            if let Some(path) = match &record {
                NodeRecord::File { path, .. } | NodeRecord::PendingFile { path, .. } => Some(path),
                _ => None,
            } {
                let path = path.clone();
                let mut pending = self.inner.pending.lock().expect("pending lock");
                // Always flip the per-NodeId buffer's mode — that's
                // the orphan's own bookkeeping when fd-based, and
                // the live buffer for non-orphan callers.
                if let Some(buf) = pending.hot.get_mut(&node.0) {
                    buf.mode = new_mode;
                }
                // Orphan branch: `unlink_entry` / `rename_entry`
                // recorded this NodeId because the kernel still
                // holds an fd to it, but the directory entry is
                // gone (or rebound to a sibling). POSIX is explicit:
                // an fd-based attribute change applies only to the
                // file referenced by that fd. Touching
                // `hot_by_path[path]` would mutate the fresh inode
                // now living at the same name; touching
                // `warm[path]` would land the change on the sibling
                // at capture time.
                if !pending.is_orphan(node.0) {
                    if let Some(other_id) = pending.hot_by_path.get(&path).copied()
                        && let Some(buf) = pending.hot.get_mut(&other_id)
                    {
                        buf.mode = new_mode;
                    }
                    // Warm is NodeId-keyed: rebind via inodes if the
                    // path still resolves Live to a tracked NodeId.
                    let warm_id = {
                        let inodes = self.inner.inodes.lock().expect("inode lock");
                        inodes.by_path.get(&path).copied()
                    };
                    if let Some(id) = warm_id
                        && let Some(entry) = pending.warm.get_mut(&id)
                    {
                        entry.mode = new_mode;
                    }
                }
            }
        }

        // Size mutation: O_TRUNC, ftruncate, etc.
        if let Some(new_size) = update.size {
            self.apply_truncate(node, new_size)?;
        }
        // uid/gid/mtime: accepted as no-ops. The overlay doesn't carry
        // per-node ownership / timestamps yet (capture re-derives both
        // from the agent's principal + mount mtime).
        self.attrs(node)
    }

    fn apply_truncate(&self, node: NodeId, new_size: u64) -> Result<()> {
        let record = self.record_for(node)?;
        let (path, mode, captured_blob) = match &record {
            NodeRecord::File {
                path, mode, blob, ..
            } => (path.clone(), *mode, Some(*blob)),
            NodeRecord::PendingFile { path, mode } => (path.clone(), *mode, None),
            _ => {
                return Err(MountError::IsADirectory(format!(
                    "setattr(size) on non-file {record:?}"
                )));
            }
        };

        // Phase 1: under the lock, decide whether a buffer already
        // exists (resize in place), and otherwise record orphan-ness
        // + the seed source. Drop the lock for the CAS read.
        //
        // POSIX `ftruncate` on an open-unlinked / rename-displaced fd
        // (an orphan in our terminology) must touch only the
        // anonymous open inode. The orphan branch never resizes a
        // sibling buffer at the rebased path, never seeds from
        // `warm[path]` (now owned by the sibling), and in Phase 2
        // never republishes `hot_by_path[path]` nor clears the
        // tombstone.
        enum Phase1 {
            ResizedInPlace,
            NeedSeed {
                orphan: bool,
                seed: Option<ContentHash>,
            },
        }
        let phase1 = {
            // Resolve the path's current Live NodeId via the inode
            // registry — under the unified shape `warm` is
            // NodeId-keyed, and the Live owner of `path` is the
            // sibling we'd seed from when no per-inode buffer exists.
            let path_owner = {
                let inodes = self.inner.inodes.lock().expect("inode lock");
                inodes.by_path.get(&path).copied()
            };
            let mut pending = self.inner.pending.lock().expect("pending lock");
            let orphan = pending.is_orphan(node.0);
            let id = if pending.hot.contains_key(&node.0) {
                Some(node.0)
            } else if orphan {
                // Never resize a sibling buffer through the orphan
                // fd — that buffer belongs to a fresh inode at the
                // rebound name.
                None
            } else {
                pending.hot_by_path.get(&path).copied()
            };
            if let Some(id) = id
                && let Some(buf) = pending.hot.get_mut(&id)
            {
                buf.bytes.resize(new_size as usize, 0);
                buf.last_touched = Instant::now();
                Phase1::ResizedInPlace
            } else {
                let seed = if orphan {
                    // Orphan: only the inode's pre-displacement
                    // content is valid. Under the unified shape its
                    // own warm bytes live at `warm[node.0]`; fall
                    // back to the captured blob (this inode's own,
                    // not the sibling at the rebound name).
                    pending.warm.get(&node.0).map(|e| e.blob).or(captured_blob)
                } else {
                    // Live: the path's bytes live at `warm[id]` where
                    // id is the Live owner via `inodes.by_path`.
                    path_owner
                        .and_then(|id| pending.warm.get(&id).map(|e| e.blob))
                        .or(captured_blob)
                };
                Phase1::NeedSeed { orphan, seed }
            }
        };
        let (orphan, seed_blob) = match phase1 {
            Phase1::ResizedInPlace => return Ok(()),
            Phase1::NeedSeed { orphan, seed } => (orphan, seed),
        };

        let mut bytes = match seed_blob {
            Some(hash) => (*self.load_blob_bytes(&hash)?).to_vec(),
            None => Vec::new(),
        };
        bytes.resize(new_size as usize, 0);
        let mut pending = self.inner.pending.lock().expect("pending lock");
        if orphan {
            // Per-NodeId buffer only. Skip the tombstone-clear and
            // the `hot_by_path` rebind — the directory entry must
            // stay gone (open-unlinked) or stay rebound to the
            // sibling (rename-over). The companion orphan branch in
            // `flush_node` drops this buffer on release without
            // warm-promoting it.
            pending.hot.insert(
                node.0,
                HotBuffer {
                    path,
                    mode,
                    bytes,
                    last_touched: Instant::now(),
                },
            );
        } else {
            pending.tombstones.remove(&path);
            pending.hot.insert(
                node.0,
                HotBuffer {
                    path: path.clone(),
                    mode,
                    bytes,
                    last_touched: Instant::now(),
                },
            );
            pending.hot_by_path.insert(path, node.0);
        }
        Ok(())
    }

    /// Create a symbolic link under `parent`. Target bytes are kept
    /// in the pending tier verbatim; `capture` writes them as a CAS
    /// blob and emits a `Symlink` tree entry.
    pub fn create_symlink(&self, parent: NodeId, name: &OsStr, target: &Path) -> Result<Entry> {
        // R8: serialize with other write-side mutations.
        let _write_guard = self.inner.write_mu.lock().expect("write mu");
        let name_str = validate_entry_name(name)?;
        if self.lookup(parent, name)?.is_some() {
            return Err(MountError::AlreadyExists(name_str.to_string()));
        }
        let parent_record = self.record_for(parent)?;
        let parent_path = self
            .dir_path_of(&parent_record)
            .ok_or_else(|| MountError::NotADirectory(format!("{parent_record:?}")))?;
        let child_path = join_child(&parent_path, name_str);
        let target_bytes = target.as_os_str().as_encoded_bytes().to_vec();
        let target_len = target_bytes.len() as u64;

        {
            let mut pending = self.inner.pending.lock().expect("pending lock");
            pending.tombstones.remove(&child_path);
            pending.symlinks.insert(child_path.clone(), target_bytes);
        }
        let node = self.intern(NodeRecord::PendingSymlink { path: child_path });
        Ok(Entry {
            node,
            name: name.to_os_string(),
            kind: NodeKind::Symlink,
            size: target_len,
            unix_mode: FileMode::Symlink.to_unix_mode(),
        })
    }

    /// Read the target of a symlink `node`. Works for both overlay
    /// (`PendingSymlink`) and captured (`Symlink`) records.
    ///
    /// Codex r12 thread 3293510316 (P1): the prior implementation
    /// used `OsStr::from_encoded_bytes_unchecked` on bytes loaded
    /// from the object store, which is unsound — that API's safety
    /// contract requires bytes minted by `OsStr::as_encoded_bytes`
    /// in *this* process and Rust version, but captured-tree blobs
    /// can come from any process and version. The corrected path
    /// delegates to [`symlink_target_from_bytes`], which uses
    /// platform-safe APIs (`OsStrExt::from_bytes` on Unix, UTF-8
    /// validation on Windows).
    pub fn read_link(&self, node: NodeId) -> Result<OsString> {
        let record = self.record_for(node)?;
        match record {
            NodeRecord::PendingSymlink { path } => {
                let pending = self.inner.pending.lock().expect("pending lock");
                let bytes = pending
                    .symlinks
                    .get(&path)
                    .ok_or_else(|| MountError::Stale(format!("symlink {}", path.display())))?;
                symlink_target_from_bytes(bytes)
            }
            NodeRecord::Symlink { blob } => {
                let bytes = self.load_blob_bytes(&blob)?;
                symlink_target_from_bytes(&bytes)
            }
            other => Err(MountError::InvalidArgument(format!(
                "read_link on non-symlink record: {other:?}"
            ))),
        }
    }

    /// Flush all hot buffers to CAS. Useful at the start of `capture`
    /// or when tests want a deterministic warm state.
    pub fn flush_all(&self) -> Result<()> {
        let ids: Vec<u64> = self
            .inner
            .pending
            .lock()
            .expect("pending lock")
            .hot
            .keys()
            .copied()
            .collect();
        for id in ids {
            self.flush_node(NodeId(id))?;
        }
        Ok(())
    }

    /// Look up a path in the pending tier. Order: hot buffer (in-flight
    /// writes), then warm tier (promoted blob), then None (caller must
    /// fall back to the immutable state's tree).
    ///
    /// Under the unified NodeId-keyed model warm bytes live at
    /// `warm[id]`; the path → id resolution goes through
    /// `inodes.by_path` (lock order: pending ⊐ inodes).
    fn pending_lookup(&self, path: &Path) -> Option<PendingHit> {
        let pending = self.inner.pending.lock().expect("pending lock");
        if pending.tombstones.contains(path) {
            return Some(PendingHit::Tombstone);
        }
        if let Some(target) = pending.symlinks.get(path) {
            return Some(PendingHit::Symlink {
                target_len: target.len() as u64,
            });
        }
        if let Some(node_id) = pending.hot_by_path.get(path)
            && let Some(buf) = pending.hot.get(node_id)
        {
            return Some(PendingHit::Hot {
                node: NodeId(*node_id),
                size: buf.bytes.len() as u64,
                mode: buf.mode,
            });
        }
        // Warm needs path → NodeId resolution. Acquire inodes inside
        // the pending lock (lock order: pending ⊐ inodes).
        let inodes = self.inner.inodes.lock().expect("inode lock");
        let id = *inodes.by_path.get(path)?;
        let entry = pending.warm.get(&id)?;
        Some(PendingHit::Warm {
            blob: entry.blob,
            size: entry.size,
            mode: entry.mode,
        })
    }

    /// True if the parent dir or any ancestor of `path` has been
    /// `rmdir`'d through the mount. Used by lookup/enumerate so the
    /// kernel never sees stale captured children of a directory the
    /// agent removed.
    fn ancestor_is_dir_tombstoned(&self, pending: &Pending, path: &Path) -> bool {
        let mut cursor = path.parent();
        while let Some(p) = cursor {
            if p.as_os_str().is_empty() {
                break;
            }
            if pending.dir_tombstones.contains(p) {
                return true;
            }
            cursor = p.parent();
        }
        false
    }

    /// Does any pending entry sit *under* `dir` as a strict prefix?
    /// I.e. has an agent created `dir/something` even though `dir`
    /// itself isn't in the captured tree yet? An explicit `mkdir dir`
    /// also counts (so an empty mkdir survives without children).
    fn pending_dir_exists(&self, dir: &Path) -> bool {
        if dir.as_os_str().is_empty() {
            return false;
        }
        let pending = self.inner.pending.lock().expect("pending lock");
        if pending.explicit_dirs.contains(dir) {
            return true;
        }
        let prefix = dir;
        let probe = |path: &Path| -> bool {
            path.strip_prefix(prefix)
                .ok()
                .and_then(|tail| tail.components().next())
                .is_some()
        };
        // Warm is NodeId-keyed under the unified shape; resolve paths
        // via the inode registry. Skip orphans (their NodeRecord may
        // still carry the pre-orphan path, but bytes are unreachable
        // by path post-T1/T3).
        let warm_under = {
            let inodes = self.inner.inodes.lock().expect("inode lock");
            pending.warm.keys().any(|id| {
                if pending.is_orphan(*id) {
                    return false;
                }
                let Some(record) = inodes.by_id.get(id) else {
                    return false;
                };
                match warm_path_of_record(record) {
                    Some(p) => !pending.tombstones.contains(p) && probe(p),
                    None => false,
                }
            })
        };
        warm_under
            || pending
                .hot_by_path
                .keys()
                .any(|p| !pending.tombstones.contains(p) && probe(p))
            || pending.symlinks.keys().any(|p| probe(p))
    }

    /// Direct children of `dir` that exist purely in the pending
    /// tier (created/written by the mount, not in the captured tree).
    /// Returns each immediate child as either a file (with hot or
    /// warm metadata) or an implicit directory (because some pending
    /// path is *under* this dir, e.g. `src/foo.rs` makes `src` an
    /// implicit dir of root). Tombstones suppress paths.
    fn pending_children_at(&self, dir: &Path) -> Vec<(String, PendingChildKind)> {
        let pending = self.inner.pending.lock().expect("pending lock");
        let mut out: BTreeMap<String, PendingChildKind> = BTreeMap::new();

        let project = |path: &Path| -> Option<(String, bool)> {
            let suffix = if dir.as_os_str().is_empty() {
                Some(path)
            } else {
                path.strip_prefix(dir).ok()
            }?;
            let mut comps = suffix.components();
            let first = comps.next()?;
            let name = match first {
                Component::Normal(n) => n.to_str()?.to_string(),
                _ => return None,
            };
            let is_dir = comps.next().is_some();
            Some((name, is_dir))
        };

        for (path, node_id) in pending.hot_by_path.iter() {
            if pending.tombstones.contains(path) {
                continue;
            }
            let Some((name, is_dir)) = project(path) else {
                continue;
            };
            if is_dir {
                out.entry(name).or_insert(PendingChildKind::Dir);
            } else if let Some(buf) = pending.hot.get(node_id) {
                out.insert(
                    name,
                    PendingChildKind::HotFile {
                        node: NodeId(*node_id),
                        size: buf.bytes.len() as u64,
                        mode: buf.mode,
                    },
                );
            }
        }
        // Warm is NodeId-keyed; resolve each entry's current path via
        // the inode registry. Skip orphans (no path identity) and
        // skip entries whose path tombstones (deleted overlays).
        let inodes = self.inner.inodes.lock().expect("inode lock");
        for (id, entry) in pending.warm.iter() {
            if pending.is_orphan(*id) {
                continue;
            }
            let Some(record) = inodes.by_id.get(id) else {
                continue;
            };
            let Some(path) = warm_path_of_record(record) else {
                continue;
            };
            if pending.tombstones.contains(path) {
                continue;
            }
            let Some((name, is_dir)) = project(path) else {
                continue;
            };
            if is_dir {
                out.entry(name).or_insert(PendingChildKind::Dir);
            } else {
                out.entry(name).or_insert(PendingChildKind::WarmFile {
                    size: entry.size,
                    mode: entry.mode,
                });
            }
        }
        drop(inodes);
        for (path, target) in pending.symlinks.iter() {
            let Some((name, is_dir)) = project(path) else {
                continue;
            };
            if is_dir {
                out.entry(name).or_insert(PendingChildKind::Dir);
            } else {
                out.entry(name).or_insert(PendingChildKind::Symlink {
                    size: target.len() as u64,
                });
            }
        }
        // Explicit empty mkdirs that haven't picked up any pending
        // children yet. Surface them as direct children when their
        // parent matches `dir`, and as transitive implicit dirs when
        // an ancestor matches.
        for explicit in pending.explicit_dirs.iter() {
            let Some((name, _is_deeper)) = project(explicit) else {
                continue;
            };
            out.entry(name).or_insert(PendingChildKind::Dir);
        }
        out.into_iter().collect()
    }
}

/// Reject FUSE entry names that wouldn't survive a `TreeEntry`'s
/// validator. Delegates to [`objects::object::validate_tree_entry_name`]
/// so the mount's write-side reject set stays in lockstep with the
/// tree serializer's — Codex r13 thread 3293733163 (P2) caught the
/// drift where the overlay accepted backslash and control bytes that
/// the serializer later rejected at capture with a confusing
/// "invalid object" error. The NUL pre-check is here (not in the
/// shared validator) because `OsStr` on Unix can carry interior NUL
/// bytes that `to_str()` would otherwise round-trip through to the
/// validator as an unmarked control byte; we surface a more specific
/// error.
fn validate_entry_name(name: &OsStr) -> Result<&str> {
    let bytes = name.as_encoded_bytes();
    if bytes.contains(&0) {
        return Err(MountError::InvalidArgument(format!(
            "entry name {name:?} contains NUL"
        )));
    }
    let name_str = name.to_str().ok_or_else(|| {
        MountError::InvalidArgument(format!("entry name {name:?} is not valid UTF-8"))
    })?;
    objects::object::validate_tree_entry_name(name_str)
        .map_err(|e| MountError::InvalidArgument(e.to_string()))?;
    Ok(name_str)
}

/// Mount-relative path for a warm-tier entry, derived from its
/// [`NodeRecord`]. The NodeId-keyed warm tier doesn't store the path
/// directly; `apply_pending_to_tree` / `pending_dir_exists` /
/// `pending_children_at` resolve it via the inode registry. Only
/// file-like records (`File`, `PendingFile`) carry warm bytes; the
/// other variants return `None`.
fn warm_path_of_record(record: &NodeRecord) -> Option<&Path> {
    match record {
        NodeRecord::File { path, .. } | NodeRecord::PendingFile { path, .. } => Some(path),
        _ => None,
    }
}

/// Decode symlink target bytes back into an `OsString`. The Unix
/// branch uses `OsStrExt::from_bytes`, which is sound for any byte
/// sequence (the inverse of `OsStrExt::as_bytes`). The Windows branch
/// validates as UTF-8 and returns [`MountError::InvalidArgument`]
/// otherwise — `OsStr` on Windows is a process-internal encoding
/// (WTF-8 today, but not promised), so accepting arbitrary captured
/// bytes is unsound. Replaces a prior
/// `unsafe { OsStr::from_encoded_bytes_unchecked(bytes) }` call site
/// (Codex r12 thread 3293510316).
fn symlink_target_from_bytes(bytes: &[u8]) -> Result<OsString> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        Ok(OsStr::from_bytes(bytes).to_os_string())
    }
    #[cfg(not(unix))]
    {
        match std::str::from_utf8(bytes) {
            Ok(s) => Ok(OsString::from(s)),
            Err(_) => Err(MountError::InvalidArgument(
                "captured symlink target bytes are not valid UTF-8".into(),
            )),
        }
    }
}

/// Join a parent mount-relative path with a leaf name. Mirrors the
/// shape every write-side op uses, so the construction stays
/// consistent across the file.
#[inline]
fn join_child(parent: &Path, name: &str) -> PathBuf {
    if parent.as_os_str().is_empty() {
        PathBuf::from(name)
    } else {
        parent.join(name)
    }
}

/// Copy `[offset, offset+buf.len())` from `src` into `buf`, returning
/// the number of bytes actually copied (0 when `offset` is past EOF,
/// or `min(buf.len(), src.len() - offset)` otherwise). Pulled out so
/// the `read` hot path is a single slice copy rather than a Vec
/// allocation per call.
#[inline]
fn copy_into(src: &[u8], offset: u64, buf: &mut [u8]) -> usize {
    let offset = offset as usize;
    if offset >= src.len() {
        return 0;
    }
    let take = std::cmp::min(buf.len(), src.len() - offset);
    buf[..take].copy_from_slice(&src[offset..offset + take]);
    take
}

/// Pending-tier overlay for a captured-tree path. Consumed by `read`
/// to decide whether to serve the captured blob (`None` returned by
/// the lookup) or the pending overlay's bytes.
enum Overlay {
    /// Promoted warm-tier blob. Same path now points at this blob in
    /// the pending tier; the captured `File` record's blob is
    /// effectively stale until capture folds the warm tier in.
    Warm(ContentHash),
    /// Tombstoned through the mount. The kernel will get a stale
    /// inode reply; subsequent dentry refresh resolves the entry as
    /// gone.
    Gone,
}

/// What `pending_lookup` found at a given path.
#[allow(dead_code)] // `blob` reserved for cross-mount dedup callers.
enum PendingHit {
    Hot {
        node: NodeId,
        size: u64,
        mode: FileMode,
    },
    Warm {
        blob: ContentHash,
        size: u64,
        mode: FileMode,
    },
    Symlink {
        target_len: u64,
    },
    Tombstone,
}

/// Direct-child summary for `pending_children_at`. Either a file
/// (with metadata enough to answer `lookup`/`stat`) or an implicit
/// subdirectory whose actual content lives further down in the
/// pending map.
enum PendingChildKind {
    HotFile {
        node: NodeId,
        size: u64,
        mode: FileMode,
    },
    WarmFile {
        size: u64,
        mode: FileMode,
    },
    Symlink {
        size: u64,
    },
    Dir,
}

impl<S: ObjectStore> MountInner<S> {
    /// Drain any hot buffer whose `last_touched` is older than
    /// `idle_after`. Mirrors `ContentAddressedMount::promote_idle_buffers`
    /// but is callable from the worker thread which only holds a
    /// `Weak<MountInner>`.
    fn sweep_idle_buffers(&self) -> Result<()> {
        let now = Instant::now();
        let idle_after = self.promotion.read().expect("promotion lock").idle_after;
        let to_promote: Vec<u64> = {
            let pending = self.pending.lock().expect("pending lock");
            pending
                .hot
                .iter()
                .filter(|(_, buf)| now.saturating_duration_since(buf.last_touched) >= idle_after)
                .map(|(id, _)| *id)
                .collect()
        };
        for id in to_promote {
            let _ = self.flush_node(NodeId(id));
        }
        Ok(())
    }

    /// Promote a single hot buffer to CAS. Inner-side flush so the
    /// sweep worker can drain idle buffers without bouncing back
    /// through `ContentAddressedMount`.
    ///
    /// Lifecycle note (R8 — Codex Thread 3293235165): FUSE `flush`
    /// fires on every descriptor close including each close of a
    /// `dup`-derived fd. For an orphaned node we must NOT touch the
    /// orphan marker here and must NOT drop the hot buffer (surviving
    /// fds need both). Only [`Self::release_node`] — invoked on the
    /// last-close-per-FUSE-open — clears the marker.
    fn flush_node(&self, node: NodeId) -> Result<()> {
        let (path, mode, bytes) = {
            let mut pending = self.pending.lock().expect("pending lock");
            // Orphan: keep the buffer alive across `flush` events.
            // POSIX open-unlinked semantics: bytes persist for the
            // surviving fds; the state survives so subsequent writes
            // through those fds keep taking the orphan branch (no
            // path republish, no warm promotion). The final clear
            // happens in `release_node`.
            if pending.is_orphan(node.0) {
                return Ok(());
            }
            let Some(buf) = pending.hot.remove(&node.0) else {
                return Ok(());
            };
            // Only retract the path mapping if it still points at
            // us; an unlink-then-recreate that happened between
            // write and flush may have moved the live mapping to a
            // fresh inode (whose path coincidentally matches), and
            // a blind `remove` would yank that fresh entry.
            if pending.hot_by_path.get(&buf.path) == Some(&node.0) {
                pending.hot_by_path.remove(&buf.path);
            }
            (buf.path, buf.mode, buf.bytes)
        };
        let size = bytes.len() as u64;
        let blob = Blob::new(bytes);
        let blob_oid = self
            .repo
            .store()
            .put_blob(&blob)
            .map_err(MountError::Store)?;
        debug!(?path, %blob_oid, size, "promoted hot buffer to CAS");
        let mut pending = self.pending.lock().expect("pending lock");
        // Warm is NodeId-keyed. The path-keyed tombstone clear below
        // is a separate concern (directory-entry level).
        pending.warm.insert(
            node.0,
            PendingEntry {
                blob: blob_oid,
                mode,
                size,
            },
        );
        // Promotion supersedes any prior tombstone for this path.
        pending.tombstones.remove(&path);
        Ok(())
    }

    /// Final close of `node` from a FUSE `release` callback. Drives
    /// the per-NodeId lifecycle: decrement the open count carried on
    /// `state[node]`; on the final close of an Orphan, drop bytes
    /// and remove the state entry; on the final close of a Live
    /// (or untracked) node, promote any hot buffer to warm via
    /// `flush_node`.
    ///
    /// The orphan branch never warm-promotes — an orphan's bytes are
    /// unreachable by path post-T1/T3, so promoting them would leak
    /// data into the captured tree at a now-tombstoned path.
    fn release_node(&self, node: NodeId) -> Result<()> {
        // Action determined under the lock so we don't re-read state
        // after dropping bytes.
        enum Outcome {
            /// Mid-life (non-final) close OR final close of a Live
            /// node. Either way: forward to `flush_node`. (`flush_node`
            /// is a no-op for Orphan, so the mid-life Orphan case is
            /// also safe to forward.)
            Flush,
            /// Final close of an Orphan. Bytes were dropped under the
            /// lock; nothing else to do (no warm promotion — see the
            /// doc comment above).
            OrphanFinalDone,
        }
        let outcome = {
            let mut pending = self.pending.lock().expect("pending lock");
            match pending.state.get(&node.0).copied() {
                None => {
                    // Untracked release (no on_open was ever
                    // recorded). Treat as Live final-close — flush
                    // any hot buffer.
                    Outcome::Flush
                }
                Some(NodeState::Live { open_count }) => {
                    let n = open_count.saturating_sub(1);
                    if n == 0 {
                        pending.state.remove(&node.0);
                    } else {
                        pending
                            .state
                            .insert(node.0, NodeState::Live { open_count: n });
                    }
                    Outcome::Flush
                }
                Some(NodeState::Orphan { open_count }) => {
                    let n = open_count.saturating_sub(1);
                    if n == 0 {
                        // Final release of an Orphan — POSIX "inode
                        // lives until last close" ends here. Drop
                        // bytes and the state entry.
                        pending.state.remove(&node.0);
                        pending.hot.remove(&node.0);
                        pending.warm.remove(&node.0);
                        Outcome::OrphanFinalDone
                    } else {
                        pending
                            .state
                            .insert(node.0, NodeState::Orphan { open_count: n });
                        // Mid-life Orphan release: forward to
                        // flush_node (which no-ops for orphans).
                        Outcome::Flush
                    }
                }
            }
        };
        match outcome {
            Outcome::Flush => self.flush_node(node),
            Outcome::OrphanFinalDone => Ok(()),
        }
    }
}

/// Spawn the safety-sweep worker, if one is requested by the
/// inner's promotion policy. The worker holds a `Weak<MountInner>`
/// so the mount can drop normally; on each tick it upgrades the
/// weak handle and drains any hot buffer that's been idle longer
/// than `idle_after`. A `None` `sweep_interval` returns `None`,
/// meaning event-driven promotion only.
fn spawn_sweep_worker<S: ObjectStore + 'static>(
    inner: &Arc<MountInner<S>>,
) -> Option<SweepHandle> {
    let interval = inner
        .promotion
        .read()
        .expect("promotion lock")
        .sweep_interval?;
    let weak = Arc::downgrade(inner);
    let state = Arc::new(SweepShutdown::new());
    let state_for_thread = Arc::clone(&state);
    let join = std::thread::Builder::new()
        .name("heddle-mount-sweep".into())
        .spawn(move || sweep_worker_loop(weak, state_for_thread, interval))
        .ok()?;
    Some(SweepHandle {
        state,
        join: Some(join),
    })
}

/// Tick body for the safety-sweep worker. Parks on the shutdown
/// condvar until either the timer interval elapses (run a sweep) or
/// `signal_and_join` wakes us (exit). Also exits when the weak
/// `MountInner` reference can no longer be upgraded.
fn sweep_worker_loop<S: ObjectStore + 'static>(
    inner: std::sync::Weak<MountInner<S>>,
    state: Arc<SweepShutdown>,
    interval: Duration,
) {
    loop {
        // Wait returns true on shutdown, false on timeout — either
        // way we re-check the upgrade afterwards.
        if state.wait(interval) {
            return;
        }
        let Some(mount) = inner.upgrade() else {
            return;
        };
        if let Err(err) = mount.sweep_idle_buffers() {
            warn!(?err, "sweep worker hit error promoting idle buffers");
        }
        // Drop the strong-count immediately so the mount can drop
        // even if our next wait is still pending.
        drop(mount);
    }
}

fn resolve_thread<S: ObjectStore>(
    repo: &Repository<RefManager, OpLog, S>,
    thread: &str,
) -> Result<MountState> {
    let thread_name = objects::object::ThreadName::from(thread);
    let change_id = repo
        .refs()
        .get_thread(&thread_name)?
        .ok_or_else(|| MountError::UnknownThread(thread.to_string()))?;
    let state = repo
        .store()
        .get_state(&change_id)?
        .ok_or_else(|| MountError::UnknownThread(thread.to_string()))?;
    Ok(MountState {
        change_id,
        tree: state.tree,
    })
}

impl<S: ObjectStore + 'static> PlatformShell for ContentAddressedMount<S> {
    fn lookup(&self, parent: NodeId, name: &OsStr) -> Result<Option<Entry>> {
        let record = self.record_for(parent)?;
        let parent_path = match self.dir_path_of(&record) {
            Some(p) => p,
            None => return Ok(None),
        };
        let Some(name_str) = name.to_str() else {
            return Ok(None);
        };
        let child_path = join_child(&parent_path, name_str);

        // Pending tier wins over the immutable tree for files —
        // that's what makes "write then read" return the new bytes.
        match self.pending_lookup(&child_path) {
            Some(PendingHit::Tombstone) => return Ok(None),
            Some(hit) => {
                // Non-tombstone hits always yield an entry; tombstone
                // is handled above.
                if let Some(entry) = self.entry_from_pending_hit(hit, &child_path, name) {
                    return Ok(Some(entry));
                }
                return Ok(None);
            }
            None => {}
        }

        // Did an ancestor get rmdir'd? Then the captured-tree entry
        // is no longer addressable through this mount.
        {
            let pending = self.inner.pending.lock().expect("pending lock");
            if pending.dir_tombstones.contains(&child_path)
                || self.ancestor_is_dir_tombstoned(&pending, &child_path)
            {
                return Ok(None);
            }
        }

        // Captured tree wins over implicit pending dirs: if both
        // the captured tree has `nested/` AND the pending tier has
        // `nested/c.txt`, we want callers to descend through the
        // captured `Dir` record (which still overlays pending on
        // its way down) rather than through a `PendingDir` shell
        // that would hide the captured siblings.
        let parent_tree = self.tree_for_record(&record)?;
        if let Some(tree_entry) = parent_tree.get(name_str) {
            return Ok(Some(self.entry_from_tree_entry(&parent_path, tree_entry)?));
        }

        // Implicit directory introduced by a deeper pending write
        // (e.g. write to `newdir/foo.rs` makes `newdir` resolvable
        // as a directory before capture).
        if self.pending_dir_exists(&child_path) {
            let node = self.intern(NodeRecord::PendingDir {
                path: child_path.clone(),
            });
            return Ok(Some(Entry {
                node,
                name: OsString::from(name_str),
                kind: NodeKind::Directory,
                size: self.pending_children_at(&child_path).len() as u64,
                unix_mode: DIR_UNIX_MODE,
            }));
        }

        Ok(None)
    }

    fn read(&self, node: NodeId, offset: u64, buf: &mut [u8]) -> Result<usize> {
        let record = self.record_for(node)?;

        // Hot-tier fast path: if there's an in-flight buffer for
        // *this* NodeId, copy the requested slice directly under the
        // lock without cloning the whole buffer. Sub-microsecond on
        // small writes; avoids one `Vec::clone` per `read` callback.
        {
            let pending = self.inner.pending.lock().expect("pending lock");
            if let Some(hot) = pending.hot.get(&node.0) {
                return Ok(copy_into(&hot.bytes, offset, buf));
            }
        }

        match &record {
            NodeRecord::PendingFile { path, .. } => {
                // Same shape, keyed by path: another NodeId may own
                // the buffer (e.g. after rename/coalesce). Orphan
                // PendingFiles skip the path overlay — the path is
                // gone (open-unlinked) or rebound (rename-over) — but
                // the unified shape preserves the inode's own warm
                // bytes (if any) at `warm[node.0]`. With no warm
                // fallback there is no captured-tier source either,
                // so the read errors with Stale.
                let warm_blob = {
                    let pending = self.inner.pending.lock().expect("pending lock");
                    if pending.is_orphan(node.0) {
                        return match pending.warm.get(&node.0).map(|e| e.blob) {
                            Some(blob) => {
                                drop(pending);
                                let bytes = self.load_blob_bytes(&blob)?;
                                Ok(copy_into(&bytes, offset, buf))
                            }
                            None => Err(MountError::Stale(format!(
                                "orphan pending file {} has no readable bytes",
                                path.display()
                            ))),
                        };
                    }
                    if let Some(id) = pending.hot_by_path.get(path).copied()
                        && let Some(hot) = pending.hot.get(&id)
                    {
                        return Ok(copy_into(&hot.bytes, offset, buf));
                    }
                    // Warm is NodeId-keyed; resolve path → id via the
                    // inode registry.
                    let inodes = self.inner.inodes.lock().expect("inode lock");
                    inodes
                        .by_path
                        .get(path)
                        .copied()
                        .and_then(|id| pending.warm.get(&id).map(|e| e.blob))
                };
                match warm_blob {
                    Some(blob) => {
                        let bytes = self.load_blob_bytes(&blob)?;
                        Ok(copy_into(&bytes, offset, buf))
                    }
                    None => Err(MountError::Stale(format!(
                        "pending file {}",
                        path.display()
                    ))),
                }
            }
            NodeRecord::File { blob, path, .. } => {
                // A captured-tree file whose path now has a pending
                // overlay (hot buffer on a sibling NodeId, warm-tier
                // promotion, or tombstone) must serve the overlay,
                // not the captured blob. Without this, a FUSE
                // `write → flush → read` round-trip through the
                // *same* kernel-cached NodeId silently returns the
                // pre-write bytes (the kernel reuses its dentry for
                // the duration of the entry TTL and never re-issues
                // `lookup`, so the inode record is never refreshed
                // from `File` to `PendingFile`).
                //
                // Priority: hot @ another NodeId → warm → tombstone
                // (ENOENT-shaped Stale) → captured blob.
                //
                // Orphan exception: an open-unlinked or
                // rename-displaced inode must skip the path overlay
                // entirely. `tombstones[path]` / `hot_by_path[path]`
                // / `warm[path]` now reflect a sibling at the same
                // name; serving them would let the open fd observe
                // (or even modify, via Overlay::Hot) bytes that
                // POSIX assigns to the sibling. Fall through to the
                // captured blob — that's the inode's own data.
                let overlay = {
                    let pending = self.inner.pending.lock().expect("pending lock");
                    if pending.is_orphan(node.0) {
                        pending
                            .warm
                            .get(&node.0)
                            .map(|warm| Overlay::Warm(warm.blob))
                    } else if pending.tombstones.contains(path) {
                        Some(Overlay::Gone)
                    } else if let Some(other_id) = pending.hot_by_path.get(path).copied()
                        && let Some(hot) = pending.hot.get(&other_id)
                    {
                        return Ok(copy_into(&hot.bytes, offset, buf));
                    } else {
                        let inodes = self.inner.inodes.lock().expect("inode lock");
                        inodes.by_path.get(path).copied().and_then(|id| {
                            pending.warm.get(&id).map(|warm| Overlay::Warm(warm.blob))
                        })
                    }
                };
                match overlay {
                    Some(Overlay::Gone) => Err(MountError::Stale(format!(
                        "file {} was unlinked through the mount",
                        path.display()
                    ))),
                    Some(Overlay::Warm(blob)) => {
                        let bytes = self.load_blob_bytes(&blob)?;
                        Ok(copy_into(&bytes, offset, buf))
                    }
                    None => {
                        let bytes = self.load_blob_bytes(blob)?;
                        Ok(copy_into(&bytes, offset, buf))
                    }
                }
            }
            NodeRecord::Symlink { blob } => {
                let bytes = self.load_blob_bytes(blob)?;
                Ok(copy_into(&bytes, offset, buf))
            }
            _ => Err(MountError::NotFound(format!(
                "read on non-file node {}",
                node.0
            ))),
        }
    }

    fn write(&self, node: NodeId, offset: u64, data: &[u8]) -> Result<usize> {
        // Determine the mount-relative path and mode to key the hot
        // buffer on. New files (`PendingFile`) carry their path
        // directly; pre-existing files identify by the parent's
        // tree entry. Any other node type rejects writes.
        let record = self.record_for(node)?;
        let (path, mode, captured_blob) = match &record {
            NodeRecord::PendingFile { path, mode } => (path.clone(), *mode, None),
            NodeRecord::File {
                path, mode, blob, ..
            } => (path.clone(), *mode, Some(*blob)),
            _ => return Err(MountError::ReadOnly),
        };

        // Phase 1: under the lock, decide whether a buffer already
        // exists, and if not, what durable source we should seed it
        // from. Snapshot the seed source's blob oid (if any) and drop
        // the lock so we can do CAS IO without blocking other writers.
        //
        // POSIX `pwrite` preserves bytes outside the [offset, offset+len)
        // range. The kernel never re-issues those bytes on a partial
        // overwrite, so the hot buffer must already contain them when
        // we apply `data`. The seed sources, in priority order:
        //
        //   1. The warm tier — a previously-flushed write to this same
        //      path in this mount session. This is the most recent
        //      durable view and supersedes the captured tree.
        //   2. The captured tree's blob for this path — the underlying
        //      file the agent is editing. Only applicable when the
        //      record was minted from a captured tree entry (i.e.
        //      `NodeRecord::File`); a `PendingFile` with no warm entry
        //      means the agent already unlinked-and-recreated.
        //   3. Empty — no durable predecessor, so this write builds a
        //      file from scratch.
        //
        // A tombstone for the path overrides everything: the agent
        // deleted the file and is now creating a fresh one.
        enum Seed {
            None,
            Blob(ContentHash),
        }
        let seed = {
            // Resolve the path's current Live owner via the inode
            // registry — warm bytes for the path live at
            // `warm[live_id]` under the unified shape.
            let path_owner = {
                let inodes = self.inner.inodes.lock().expect("inode lock");
                inodes.by_path.get(&path).copied()
            };
            let pending = self.inner.pending.lock().expect("pending lock");
            let orphan = pending.is_orphan(node.0);
            if pending.hot.contains_key(&node.0) {
                // The per-NodeId buffer is always authoritative —
                // both for live writes (this fd's accumulated bytes)
                // and for orphan writes (POSIX says the bytes belong
                // to the open handle).
                Seed::None
            } else if !orphan
                && pending
                    .hot_by_path
                    .get(&path)
                    .is_some_and(|id| pending.hot.contains_key(id))
            {
                // Sibling at the same path has a buffer — coalesce
                // onto it. Orphans never look at the path's overlay
                // (the sibling at `hot_by_path[path]` is a different
                // inode, not us).
                Seed::None
            } else if orphan {
                // Orphan-aware seeding. The path's overlay belongs
                // to the sibling at the rebound name; this inode's
                // own bytes live at `warm[node.0]` (or in the
                // captured blob).
                pending
                    .warm
                    .get(&node.0)
                    .map(|e| Seed::Blob(e.blob))
                    .or_else(|| captured_blob.map(Seed::Blob))
                    .unwrap_or(Seed::None)
            } else if pending.tombstones.contains(&path) {
                // Unlink-then-write through a fresh inode (POSIX
                // unlink+open(O_CREAT)): start from empty.
                Seed::None
            } else if let Some(entry) = path_owner.and_then(|id| pending.warm.get(&id)) {
                Seed::Blob(entry.blob)
            } else if let Some(blob) = captured_blob {
                Seed::Blob(blob)
            } else {
                Seed::None
            }
        };
        let seed_bytes = match seed {
            Seed::None => None,
            // The hot buffer is owned + mutated, so we materialize a
            // Vec here. One alloc + copy per first-write per file;
            // subsequent writes hit the existing buffer.
            Seed::Blob(hash) => Some((*self.load_blob_bytes(&hash)?).to_vec()),
        };

        // Phase 2: re-acquire the lock, install or update the hot
        // buffer, apply the write. If a buffer materialized between
        // phases (e.g. a coalesce from another NodeId), prefer the
        // existing buffer's bytes — our `seed_bytes` are stale.
        let mut pending = self.inner.pending.lock().expect("pending lock");
        // POSIX unlink+open semantics. Two write shapes share this
        // method and must be kept separate:
        //
        //   * unlink-then-create (`unlink P; open(P, O_CREAT); write`)
        //     — `create_file` minted a fresh `NodeId` and cleared the
        //     tombstone for P. Our `node.0` is not in `orphans` and
        //     the write republishes the name normally.
        //
        //   * open-then-unlink (`open(P); unlink P; write through old
        //     fd`) — `unlink_entry` recorded `node.0` in `orphans`
        //     and left the tombstone in place. POSIX is explicit:
        //     the inode lives behind the fd, but the directory entry
        //     must stay gone. Republishing `hot_by_path[P] = node.0`
        //     or clearing the tombstone would resurrect the pathname
        //     for every other observer (lookup, enumerate, capture).
        //     The orphan branch updates only the per-NodeId buffer;
        //     `flush_node` reads the same `orphans` signal at
        //     promotion time and drops the buffer instead of warming
        //     it.
        let orphan = pending.is_orphan(node.0);
        if !orphan {
            // Coalesce two NodeIds for the same path onto the same buffer.
            if let Some(existing_id) = pending.hot_by_path.get(&path).copied()
                && existing_id != node.0
                && let Some(buf) = pending.hot.remove(&existing_id)
            {
                pending.hot.insert(node.0, buf);
            }
            pending.hot_by_path.insert(path.clone(), node.0);
            // A live hot buffer means the file exists again — clear
            // any tombstone for this path so subsequent
            // `pending_lookup` calls see the buffer instead of a
            // "deleted" sentinel. POSIX:
            // unlink+open(O_CREAT)+pwrite reborns the path. The seed
            // logic above already starts the buffer empty when a
            // tombstone is present, so we don't need to inspect the
            // tombstone here.
            pending.tombstones.remove(&path);
        }
        let buf = pending.hot.entry(node.0).or_insert_with(|| HotBuffer {
            path: path.clone(),
            mode,
            bytes: seed_bytes.unwrap_or_default(),
            last_touched: Instant::now(),
        });
        let offset = offset as usize;
        let end = offset + data.len();
        // POSIX `pwrite` past EOF zero-fills the gap.
        if buf.bytes.len() < end {
            buf.bytes.resize(end, 0);
        }
        buf.bytes[offset..end].copy_from_slice(data);
        buf.last_touched = Instant::now();
        let written = data.len();
        drop(pending);
        // Cheap idle-promotion sweep — an agent that's gone quiet on
        // *other* files for longer than the policy window gets its
        // buffers drained without an explicit close.
        let _ = self.promote_idle_buffers();
        Ok(written)
    }

    fn enumerate(&self, dir: NodeId) -> Result<Vec<Entry>> {
        let record = self.record_for(dir)?;
        let parent_path = match self.dir_path_of(&record) {
            Some(p) => p,
            None => return Err(MountError::NotADirectory(format!("{record:?}"))),
        };
        let tree = self.tree_for_record(&record)?;
        let mut by_name: BTreeMap<String, Entry> = BTreeMap::new();

        // If this directory itself is dir-tombstoned, enumerate
        // returns empty regardless of any captured children. (A
        // child rmdir doesn't affect us — only an ancestor or self
        // tombstone does.)
        {
            let pending = self.inner.pending.lock().expect("pending lock");
            if pending.dir_tombstones.contains(&parent_path)
                || self.ancestor_is_dir_tombstoned(&pending, &parent_path)
            {
                return Ok(vec![]);
            }
        }

        // Pass 1: captured-tree entries, with pending overlay.
        for tree_entry in tree.entries() {
            let entry_path = join_child(&parent_path, &tree_entry.name);
            // Whole-subtree rmdir on a captured dir entry.
            {
                let pending = self.inner.pending.lock().expect("pending lock");
                if pending.dir_tombstones.contains(&entry_path) {
                    continue;
                }
            }
            match self.pending_lookup(&entry_path) {
                Some(PendingHit::Tombstone) => continue,
                Some(hit) => {
                    if let Some(entry) =
                        self.entry_from_pending_hit(hit, &entry_path, OsStr::new(&tree_entry.name))
                    {
                        by_name.insert(tree_entry.name.clone(), entry);
                    }
                    continue;
                }
                None => {}
            }
            let entry = self.entry_from_tree_entry(&parent_path, tree_entry)?;
            by_name.insert(tree_entry.name.clone(), entry);
        }

        // Pass 2: pending-only children of `parent_path` (mount-only
        // files and implicit subdirectories the agent created).
        let pending_children = self.pending_children_at(&parent_path);
        for (name, kind) in pending_children {
            // Don't shadow a captured-tree entry (already handled in
            // pass 1 via pending_lookup).
            if by_name.contains_key(&name) {
                continue;
            }
            let full_path = join_child(&parent_path, &name);
            match kind {
                PendingChildKind::HotFile { node, size, mode } => {
                    by_name.insert(
                        name.clone(),
                        Entry {
                            node,
                            name: OsString::from(&name),
                            kind: kind_for_mode(mode),
                            size,
                            unix_mode: mode.to_unix_mode(),
                        },
                    );
                }
                PendingChildKind::WarmFile { size, mode } => {
                    let node = self.intern(NodeRecord::PendingFile {
                        path: full_path,
                        mode,
                    });
                    by_name.insert(
                        name.clone(),
                        Entry {
                            node,
                            name: OsString::from(&name),
                            kind: kind_for_mode(mode),
                            size,
                            unix_mode: mode.to_unix_mode(),
                        },
                    );
                }
                PendingChildKind::Dir => {
                    let node = self.intern(NodeRecord::PendingDir { path: full_path });
                    by_name.insert(
                        name.clone(),
                        Entry {
                            node,
                            name: OsString::from(&name),
                            kind: NodeKind::Directory,
                            size: 0,
                            unix_mode: DIR_UNIX_MODE,
                        },
                    );
                }
                PendingChildKind::Symlink { size } => {
                    let node = self.intern(NodeRecord::PendingSymlink { path: full_path });
                    by_name.insert(
                        name.clone(),
                        Entry {
                            node,
                            name: OsString::from(&name),
                            kind: NodeKind::Symlink,
                            size,
                            unix_mode: FileMode::Symlink.to_unix_mode(),
                        },
                    );
                }
            }
        }
        Ok(by_name.into_values().collect())
    }

    fn attrs(&self, node: NodeId) -> Result<Attrs> {
        let record = self.record_for(node)?;
        let kind = record.kind();
        let unix_mode = record.unix_mode();
        let (size, nlink) = match &record {
            NodeRecord::Root { tree } | NodeRecord::Dir { tree, .. } => {
                let tree = self.load_tree(tree)?;
                // 2 = `.` + the parent's entry pointing at us. Heddle
                // doesn't model hard links, so we don't try to count
                // subdirectories' `..` entries.
                (tree.entries().len() as u64, 2)
            }
            NodeRecord::PendingDir { path } => {
                // Implicit dir — content lives entirely in the
                // pending tier. Size = direct-child count.
                (self.pending_children_at(path).len() as u64, 2)
            }
            NodeRecord::File { blob, path, .. } => {
                // Same overlay priority as `read`: hot @ this NodeId
                // → hot @ another NodeId for the same path → warm-tier
                // promotion → tombstone (stale) → captured blob.
                // Keeping `attrs` and `read` symmetric is mandatory:
                // `read` consults the warm tier for captured files
                // (so `WORLD` shadows `world`), and a stale `attrs`
                // that still reports the captured size would clip the
                // returned bytes in the kernel's read buffer.
                //
                // Orphan exception: same as `read`. An open-unlinked
                // or rename-displaced inode skips the path overlay
                // and reports the captured blob's size (or the
                // per-NodeId hot buffer's length, checked first).
                let overlay_size = {
                    let pending = self.inner.pending.lock().expect("pending lock");
                    if let Some(buf) = pending.hot.get(&node.0) {
                        Some(Some(buf.bytes.len() as u64))
                    } else if pending.is_orphan(node.0) {
                        // Prefer the orphan's own warm size (unified
                        // shape: `warm[node.0]`). With no warm, fall
                        // through to `blob_size(blob)` — the captured
                        // size is the orphan's own.
                        pending.warm.get(&node.0).map(|e| Some(e.size))
                    } else if pending.tombstones.contains(path) {
                        // Tombstoned via the mount: treat as
                        // not-yet-collected. The path is gone but the
                        // inode is still registered.
                        Some(None)
                    } else if let Some(other_id) = pending.hot_by_path.get(path).copied()
                        && let Some(hot) = pending.hot.get(&other_id)
                    {
                        Some(Some(hot.bytes.len() as u64))
                    } else {
                        // Warm is NodeId-keyed; resolve path → id via
                        // the inode registry.
                        let inodes = self.inner.inodes.lock().expect("inode lock");
                        inodes
                            .by_path
                            .get(path)
                            .copied()
                            .and_then(|id| pending.warm.get(&id).map(|warm| Some(warm.size)))
                    }
                };
                match overlay_size {
                    Some(Some(size)) => (size, 1),
                    Some(None) => {
                        return Err(MountError::Stale(format!(
                            "file {} was unlinked through the mount",
                            path.display()
                        )));
                    }
                    None => (self.blob_size(blob)?, 1),
                }
            }
            NodeRecord::Symlink { blob } => (self.blob_size(blob)?, 1),
            NodeRecord::PendingFile { path, .. } => {
                // Orphan branch: a rename-displaced or
                // unlinked-but-still-open PendingFile reports either
                // its per-NodeId hot buffer length, or its own
                // `warm[node.0]` size. `pending_lookup` would
                // otherwise consult the rebound path overlay and
                // serve the sibling's size.
                let orphan_size = {
                    let pending = self.inner.pending.lock().expect("pending lock");
                    if pending.is_orphan(node.0) {
                        Some(
                            pending
                                .hot
                                .get(&node.0)
                                .map(|buf| buf.bytes.len() as u64)
                                .or_else(|| pending.warm.get(&node.0).map(|e| e.size)),
                        )
                    } else {
                        None
                    }
                };
                if let Some(opt) = orphan_size {
                    let size = opt.ok_or_else(|| {
                        MountError::Stale(format!(
                            "orphan pending file {} has no buffered bytes",
                            path.display()
                        ))
                    })?;
                    (size, 1)
                } else {
                    let hit = self.pending_lookup(path).ok_or_else(|| {
                        MountError::Stale(format!("pending file {}", path.display()))
                    })?;
                    let size = match hit {
                        PendingHit::Hot { size, .. } | PendingHit::Warm { size, .. } => size,
                        PendingHit::Symlink { target_len } => target_len,
                        PendingHit::Tombstone => 0,
                    };
                    (size, 1)
                }
            }
            NodeRecord::PendingSymlink { path } => {
                let pending = self.inner.pending.lock().expect("pending lock");
                let size = pending
                    .symlinks
                    .get(path)
                    .map(|t| t.len() as u64)
                    .ok_or_else(|| {
                        MountError::Stale(format!("pending symlink {}", path.display()))
                    })?;
                (size, 1)
            }
        };
        let _ = self.path_of(&record);
        Ok(Attrs {
            node,
            kind,
            size,
            unix_mode,
            nlink,
            mtime: self.inner.mounted_at,
        })
    }

    fn invalidate(&self, node: NodeId) -> Result<()> {
        // Witness-gated discharge: `bp.kernel_forget_inode(node.0)`
        // returns:
        //
        // * `Some(warm_still_references)` — the FSM check passed
        //   (state is `Released` or `Live { open_count == 0 }`);
        //   `hot[node]` (with its `hot_by_path` reverse-index
        //   cleanup) and `state[node]` have been dropped, and the
        //   bool tells us whether `warm[node]` is still populated.
        //   Retire the inode-side record iff warm doesn't reference
        //   — otherwise capture still needs the NodeId → path chain
        //   to plant the warm bytes back into the new tree.
        // * `None` — the FSM check failed (state is
        //   `Live { open_count >= 1 }` or any `Orphan`); the bytes
        //   are still referenced. The witness-gated retrofit
        //   (heddle#211) makes the entire forget path short-circuit
        //   here: `hot[node]` / `state[node]` are preserved and the
        //   inode-side `forget` is skipped. The kernel will re-issue
        //   `forget` once the surviving fd closes (or never, and the
        //   next `release_node` retires the record). Closes Codex
        //   r11 finding #3 — the pre-retrofit path removed
        //   `hot[node]` before any FSM check, stranding an open
        //   Orphan fd with no readable bytes.
        //
        // Warm preservation (Codex r12 threads 3293484634 /
        // 3293510311, P1): `apply_kernel_forget` intentionally
        // leaves `warm[node]` alone — warm is the only durable
        // pre-capture copy of flushed writes, and FUSE `forget` is
        // a kernel-side dcache eviction (not a close), so dropping
        // warm would silently lose the user's committed-in-session
        // data.
        let retire_inode_record = {
            let mut pending = self.inner.pending.lock().expect("pending lock");
            pending.with_brand(|bp| {
                bp.kernel_forget_inode(node.0)
                    .map(|warm_still_references| !warm_still_references)
                    .unwrap_or(false)
            })
        };
        if retire_inode_record {
            self.inner.inodes.lock().expect("inode lock").forget(node);
        }
        Ok(())
    }

    fn flush(&self, node: NodeId) -> Result<()> {
        self.flush_node(node)
    }

    fn release(&self, node: NodeId) -> Result<()> {
        self.release_node(node)
    }

    fn on_open(&self, node: NodeId) -> Result<()> {
        ContentAddressedMount::on_open(self, node)
    }

    fn create_file(
        &self,
        parent: NodeId,
        name: &OsStr,
        mode: FileMode,
        exclusive: bool,
    ) -> Result<Entry> {
        ContentAddressedMount::create_file(self, parent, name, mode, exclusive)
    }

    fn make_dir(&self, parent: NodeId, name: &OsStr) -> Result<Entry> {
        ContentAddressedMount::make_dir(self, parent, name)
    }

    fn unlink_entry(&self, parent: NodeId, name: &OsStr) -> Result<()> {
        ContentAddressedMount::unlink_entry(self, parent, name)
    }

    fn rmdir_entry(&self, parent: NodeId, name: &OsStr) -> Result<()> {
        ContentAddressedMount::rmdir_entry(self, parent, name)
    }

    fn rename_entry(
        &self,
        old_parent: NodeId,
        old_name: &OsStr,
        new_parent: NodeId,
        new_name: &OsStr,
    ) -> Result<()> {
        ContentAddressedMount::rename_entry(self, old_parent, old_name, new_parent, new_name)
    }

    fn rename_entry_with_options(
        &self,
        old_parent: NodeId,
        old_name: &OsStr,
        new_parent: NodeId,
        new_name: &OsStr,
        options: RenameOptions,
    ) -> Result<()> {
        ContentAddressedMount::rename_entry_with_options(
            self, old_parent, old_name, new_parent, new_name, options,
        )
    }

    fn set_attrs(&self, node: NodeId, update: AttrUpdate) -> Result<Attrs> {
        ContentAddressedMount::set_attrs(self, node, update)
    }

    fn create_symlink(&self, parent: NodeId, name: &OsStr, target: &Path) -> Result<Entry> {
        ContentAddressedMount::create_symlink(self, parent, name, target)
    }

    fn read_link(&self, node: NodeId) -> Result<OsString> {
        ContentAddressedMount::read_link(self, node)
    }
}

// --- Capture --------------------------------------------------------------

// The capture/snapshot write path drives the repository's snapshot,
// attribution, and oplog-recording methods, which live on the default
// local flavor (`Repository<RefManager, OpLog, AnyStore>`). Mounts are only
// ever captured against that flavor, so this block stays concrete rather
// than threading `S` through the snapshot surface.
impl ContentAddressedMount {
    /// Drain the pending tier into a fresh heddle state and update
    /// the thread to point at it.
    ///
    /// This is the mount-side analogue of `heddle capture`/`heddle
    /// snapshot`: rather than walking a worktree to discover changed
    /// files, it folds the in-memory pending map into a real
    /// [`Tree`] object, records a [`State`], and advances the
    /// thread's HEAD ref.
    ///
    /// `intent` is propagated to `state.intent`. Attribution is
    /// pulled from the repository's default attribution path
    /// ([`Repository::get_attribution`]) — this honours the
    /// `HEDDLE_AGENT_*` env, the repo config, and the user's
    /// principal. Richer attribution paths (CLI overrides,
    /// `AgentRegistry`, session segments) live in
    /// `crates/cli/src/cli/commands/snapshot.rs::build_attribution`;
    /// when the CLI wires this up it should call
    /// [`Self::capture_with_attribution`] instead and pass the result
    /// of that helper.
    pub fn capture(&self, intent: impl Into<Option<String>>) -> Result<ChangeId> {
        let attribution = self
            .inner
            .repo
            .get_attribution()
            .map_err(MountError::Store)?;
        self.capture_with_attribution(intent, attribution)
    }

    /// Same as [`Self::capture`] but with caller-supplied attribution.
    /// The CLI uses this so it can mirror `build_attribution` from
    /// `snapshot.rs` (CLI overrides, agent registry lookup, etc.).
    #[instrument(skip(self, attribution, intent), fields(thread = %self.inner.thread))]
    pub fn capture_with_attribution(
        &self,
        intent: impl Into<Option<String>>,
        attribution: Attribution,
    ) -> Result<ChangeId> {
        // Step 0: drain hot buffers. Anything that was still being
        // edited gets promoted now so the resulting state captures
        // the agent's last writes even if it never closed the file.
        self.flush_all()?;

        let state_snapshot = *self.inner.state.read().expect("mount state lock");
        let parent_tree = self.load_tree(&state_snapshot.tree)?;

        // Step 1: build the new root tree. Walks the pending map as
        // a path-keyed virtual tree, descends into existing captured
        // subtrees where they exist, and writes every fresh subtree
        // to the store on the way up. Tombstones with empty parent
        // dirs prune naturally.
        let tree_hash = {
            let pending = self.inner.pending.lock().expect("pending lock");
            let inodes = self.inner.inodes.lock().expect("inode lock");
            apply_pending_to_tree(self.store(), &parent_tree, &pending, &inodes)?
        };

        // Step 2: record a new state. Mirrors
        // `Repository::snapshot_with_attribution_profiled`'s
        // happy-path body, minus the worktree walk and the
        // merge-conflict handling (a mount has no worktree).
        let parent_id = self.inner.repo.head().map_err(MountError::Store)?;
        let parents = match parent_id {
            Some(id) => vec![id],
            None => vec![],
        };
        let mut state = State::new_snapshot(tree_hash, parents, attribution);
        if let Some(intent) = intent.into() {
            state = state.with_intent(intent);
        }
        // Match the snapshot path: carry forward the configured
        // default confidence so downstream tools that key on it
        // don't see a sudden None for mount-captured states.
        state = state.with_confidence(self.inner.repo.config().defaults.confidence);
        // Auto-sign before persisting (heddle#482): route through the same
        // authored-state chokepoint the repo capture/commit/merge paths use, so
        // a mount-captured state is signed identically and no write bypasses it.
        self.inner
            .repo
            .put_authored_state(&mut state)
            .map_err(MountError::Store)?;

        // Step 3 + 3a unified: advance the served thread and record the
        // `OpRecord::Snapshot` **record-first** through the write chokepoint
        // (heddle#354 r8). The pre-r8 path published the thread ref FIRST and
        // recorded SECOND — the same cross-crate publish-first snapshot class as
        // `repository_snapshot.rs`. Because the reconciler folds a `Snapshot`
        // record authoritatively, a late snapshot record carrying a stale thread
        // value could clobber a newer concurrent write. Routing through
        // `commit_snapshot_atomic` makes the record commit before the publish,
        // so the newest committed record is the newest write.
        //
        // A mount always serves one specific thread, so the snapshot always
        // advances `self.inner.thread` — HEAD being attached elsewhere (or
        // detached) does not change which ref the mount advances.
        let change_id = state.change_id;
        let prev_head_change_id = state_snapshot.change_id;
        let served_thread = objects::object::ThreadName::from(self.inner.thread.as_str());
        self.inner
            .repo
            .commit_snapshot_atomic(&change_id, Some(prev_head_change_id), Some(&served_thread))
            .map_err(MountError::Store)?;

        // Step 3b: refresh the active thread record's metadata
        // (changed paths, heavy-impact paths, freshness, etc).
        // Resolution is by the repo's execution-root path, so
        // capture-from-mount lands the same updates as
        // `cmd_snapshot`. A missing thread record (e.g. a mount
        // opened on a thread that has no `Thread` row yet) is a
        // no-op that returns the default refresh report.
        let new_tree = self.load_tree(&tree_hash)?;
        if let Err(err) = repo::snapshot_metadata::refresh_active_thread_metadata(
            &self.inner.repo,
            &state,
            &new_tree,
        ) {
            warn!(?err, "thread metadata refresh from mount capture failed");
        }

        // Step 4: drain the pending tier. See
        // [`crate::pending::Pending::drain_for_capture`] for the
        // contract: `LiveZero` retires; `LiveNonZero` (open fds still
        // hold bytes — POSIX last-close-wins) and `Orphan`
        // (open-but-unlinked) survive with their `hot[id]`/`warm[id]`
        // bytes; the path-keyed overlays clear because every path they
        // covered is now folded into the new tree.
        {
            let mut pending = self.inner.pending.lock().expect("pending lock");
            pending.drain_for_capture();
        }
        let mut state_lock = self.inner.state.write().expect("mount state lock");
        *state_lock = MountState {
            change_id,
            tree: tree_hash,
        };
        // The new state's tree becomes the new root; we don't
        // remap the existing root inode (it's a permanent fixture)
        // but we do refresh its backing tree hash.
        let mut inodes = self.inner.inodes.lock().expect("inode lock");
        if let Some(record) = inodes.by_id.get_mut(&NodeId::ROOT.0) {
            *record = NodeRecord::Root { tree: tree_hash };
        }
        warn!(
            thread = %self.inner.thread,
            change = %change_id,
            "captured mount writes into new state"
        );

        Ok(change_id)
    }
}

/// Fold a [`Pending`] map into a fresh tree rooted at `parent`,
/// honouring nested paths.
///
/// Algorithm: build an in-memory virtual-DAG keyed by mount-relative
/// directory path. For each pending warm entry `dir/.../leaf`, walk
/// the path components; at the leaf, plant a file entry in the
/// virtual tree; tombstones plant deletions. Then materialize the
/// DAG bottom-up: for each directory, start from its captured
/// counterpart (if present), apply the local file overrides and
/// tombstones, recurse into each child directory, and write the
/// resulting `Tree` to the store. Empty directories are pruned —
/// a tombstone of `dir/only.rs` removes `dir` from the parent too.
///
/// Returns the root tree's content hash. The caller writes this to
/// the new state.
fn apply_pending_to_tree(
    store: &impl ObjectStore,
    parent: &Tree,
    pending: &Pending,
    inodes: &Inodes,
) -> Result<ContentHash> {
    /// In-memory virtual tree: a directory's local file overrides,
    /// tombstones, and named child directories. Built lazily during
    /// the walk; materialized recursively.
    #[derive(Default)]
    struct VDir {
        /// File leaves to plant in this directory (overrides any
        /// captured entry of the same name).
        files: BTreeMap<String, (ContentHash, FileMode)>,
        /// Symlink leaves: name → (blob_oid, target_len). The blob
        /// is hashed + written at apply time so empty-symlink rename
        /// flows just need to point at the same hash.
        symlinks: BTreeMap<String, ContentHash>,
        /// Names to tombstone (file or subdirectory).
        deletions: BTreeSet<String>,
        /// Subtrees to drop entirely (captured-dir `rmdir`). The
        /// materialize pass removes both the captured entry and any
        /// pending children planted by a deeper write under it.
        dir_deletions: BTreeSet<String>,
        /// Empty mkdirs that should survive even when no child was
        /// written. Captured-dir collision is impossible here: an
        /// existing captured dir would have made the `mkdir` itself
        /// fail with EEXIST.
        explicit_empty: BTreeSet<String>,
        /// Named child directories that have pending content.
        children: BTreeMap<String, VDir>,
    }

    let mut root = VDir::default();

    fn descend<'a>(node: &'a mut VDir, components: &[&str]) -> &'a mut VDir {
        let mut cursor = node;
        for c in components {
            if !cursor.children.contains_key(*c) {
                cursor.children.insert((*c).to_string(), VDir::default());
            }
            cursor = cursor.children.get_mut(*c).unwrap();
        }
        cursor
    }

    // Plant warm entries. Warm is NodeId-keyed; resolve each entry's
    // current Live path via the inode registry. Skip Orphan entries —
    // their bytes are unreachable by path post-T1/T3 and must not
    // resurface in the captured tree.
    for (id, entry) in &pending.warm {
        if pending.is_orphan(*id) {
            continue;
        }
        let Some(record) = inodes.by_id.get(id) else {
            continue;
        };
        let Some(path) = warm_path_of_record(record) else {
            continue;
        };
        // Sanity: the record's stored path must still bind to this
        // NodeId in `by_path`. If `inodes.by_path[path]` resolves to
        // a different inode the record is stale (e.g. a Live → Orphan
        // transition that didn't update the state map yet) and we
        // skip rather than plant phantom bytes.
        if inodes.by_path.get(path) != Some(id) {
            continue;
        }
        let comps: Vec<&str> = path
            .components()
            .filter_map(|c| match c {
                Component::Normal(n) => n.to_str(),
                _ => None,
            })
            .collect();
        let Some((leaf_name, dir_components)) = comps.split_last() else {
            continue;
        };
        let dir = descend(&mut root, dir_components);
        dir.files
            .insert((*leaf_name).to_string(), (entry.blob, entry.mode));
        dir.deletions.remove(*leaf_name);
    }

    // Plant symlinks. Their target bytes get written as a CAS blob
    // here (lazily) so we never duplicate the hashing cost in the
    // hot path of `symlink`.
    for (path, target_bytes) in &pending.symlinks {
        let comps: Vec<&str> = path
            .components()
            .filter_map(|c| match c {
                Component::Normal(n) => n.to_str(),
                _ => None,
            })
            .collect();
        let Some((leaf_name, dir_components)) = comps.split_last() else {
            continue;
        };
        let blob = Blob::new(target_bytes.clone());
        let blob_oid = store.put_blob(&blob).map_err(MountError::Store)?;
        let dir = descend(&mut root, dir_components);
        dir.symlinks.insert((*leaf_name).to_string(), blob_oid);
        dir.deletions.remove(*leaf_name);
    }

    // Plant explicit-empty directories. We descend to the leaf dir
    // and mark its name in the *parent*'s `explicit_empty` set, so
    // materialize emits a zero-entry subtree even with no children.
    for explicit in &pending.explicit_dirs {
        let comps: Vec<&str> = explicit
            .components()
            .filter_map(|c| match c {
                Component::Normal(n) => n.to_str(),
                _ => None,
            })
            .collect();
        let Some((leaf_name, dir_components)) = comps.split_last() else {
            continue;
        };
        let parent_dir = descend(&mut root, dir_components);
        parent_dir.explicit_empty.insert((*leaf_name).to_string());
        // Also ensure the dir's own VDir exists so materialize
        // visits it (descend into the leaf one extra step).
        parent_dir
            .children
            .entry((*leaf_name).to_string())
            .or_default();
    }

    // Plant tombstones. Each tombstone names a *file* the agent
    // deleted; we record it on the leaf directory so materialization
    // skips both any pre-existing entry and any virtual file with
    // the same name. Empty parent dirs prune naturally.
    for tomb in &pending.tombstones {
        let comps: Vec<&str> = tomb
            .components()
            .filter_map(|c| match c {
                Component::Normal(n) => n.to_str(),
                _ => None,
            })
            .collect();
        let Some((leaf_name, dir_components)) = comps.split_last() else {
            continue;
        };
        let dir = descend(&mut root, dir_components);
        dir.files.remove(*leaf_name);
        dir.symlinks.remove(*leaf_name);
        dir.deletions.insert((*leaf_name).to_string());
    }

    // Plant directory tombstones. Each names a captured-tree
    // directory the agent `rmdir`'d. The materialize pass deletes
    // both the captured entry and any virtual children that ended
    // up under it (a pathological case — `rmdir` requires empty —
    // but cheap to guard against).
    for tomb in &pending.dir_tombstones {
        let comps: Vec<&str> = tomb
            .components()
            .filter_map(|c| match c {
                Component::Normal(n) => n.to_str(),
                _ => None,
            })
            .collect();
        let Some((leaf_name, dir_components)) = comps.split_last() else {
            continue;
        };
        let dir = descend(&mut root, dir_components);
        dir.children.remove(*leaf_name);
        dir.explicit_empty.remove(*leaf_name);
        dir.dir_deletions.insert((*leaf_name).to_string());
    }

    /// Materialize a virtual directory against its captured counterpart
    /// `captured` (or `Tree::new()` if no captured tree exists). Writes
    /// every subtree to `store` and returns the resulting tree's hash,
    /// or `None` if the resulting tree is empty (a signal the parent
    /// should drop the entry).
    fn materialize(
        v: &VDir,
        captured: &Tree,
        store: &impl ObjectStore,
    ) -> Result<Option<ContentHash>> {
        let mut entries: BTreeMap<String, TreeEntry> = captured
            .entries()
            .iter()
            .map(|e| (e.name.clone(), e.clone()))
            .collect();

        // Tombstones first so deletions don't get re-added by other
        // overrides.
        for name in &v.deletions {
            entries.remove(name);
        }
        for name in &v.dir_deletions {
            entries.remove(name);
        }

        // File overrides.
        for (name, (blob, mode)) in &v.files {
            let executable = matches!(mode, FileMode::Executable);
            let entry = TreeEntry::file(name.clone(), *blob, executable).map_err(|e| {
                MountError::Store(objects::error::HeddleError::InvalidObject(e.to_string()))
            })?;
            entries.insert(name.clone(), entry);
        }

        // Symlink overrides.
        for (name, blob) in &v.symlinks {
            let entry = TreeEntry::symlink(name.clone(), *blob).map_err(|e| {
                MountError::Store(objects::error::HeddleError::InvalidObject(e.to_string()))
            })?;
            entries.insert(name.clone(), entry);
        }

        // Recurse into each pending subdirectory.
        for (name, child) in &v.children {
            // dir_deletions wins: if the agent `rmdir`'d this name,
            // drop the whole subtree regardless of any pending
            // virtual children (which `rmdir` requires to be empty
            // — this branch is the belt + braces).
            if v.dir_deletions.contains(name) {
                entries.remove(name);
                continue;
            }
            // Captured counterpart: if `captured` already has a
            // subdir under this name, load it; otherwise start from
            // an empty tree.
            let child_captured = match captured.get(name) {
                Some(e) if e.is_tree() => store
                    .get_tree(&e.hash)
                    .map_err(MountError::Store)?
                    .ok_or_else(|| {
                        MountError::Store(objects::error::HeddleError::MissingObject {
                            object_type: "tree".to_string(),
                            id: e.hash.to_string(),
                        })
                    })?,
                _ => Tree::new(),
            };
            let force_empty = v.explicit_empty.contains(name);
            match materialize(child, &child_captured, store)? {
                Some(hash) => {
                    let entry = TreeEntry::directory(name.clone(), hash).map_err(|e| {
                        MountError::Store(objects::error::HeddleError::InvalidObject(e.to_string()))
                    })?;
                    entries.insert(name.clone(), entry);
                }
                None if force_empty => {
                    // Empty mkdir survives capture as a 0-entry tree.
                    let hash = store.put_tree(&Tree::new()).map_err(MountError::Store)?;
                    let entry = TreeEntry::directory(name.clone(), hash).map_err(|e| {
                        MountError::Store(objects::error::HeddleError::InvalidObject(e.to_string()))
                    })?;
                    entries.insert(name.clone(), entry);
                }
                None => {
                    // Subtree is empty — drop the entry from the
                    // parent.
                    entries.remove(name);
                }
            }
        }

        if entries.is_empty() {
            return Ok(None);
        }
        let tree = Tree::from_entries(entries.into_values().collect());
        let hash = store.put_tree(&tree).map_err(MountError::Store)?;
        Ok(Some(hash))
    }

    // Materialize the root. An empty tree is still a valid root.
    let hash = match materialize(&root, parent, store)? {
        Some(h) => h,
        None => store.put_tree(&Tree::new()).map_err(MountError::Store)?,
    };
    Ok(hash)
}

impl<S: ObjectStore + 'static> ContentAddressedMount<S> {
    /// Test-only accessor for the warm tier so unit tests can verify
    /// promotions landed without going through `read`. Returns paths
    /// resolved via the inode registry (warm is NodeId-keyed under
    /// the unified shape).
    #[cfg(test)]
    pub(crate) fn warm_keys(&self) -> Vec<PathBuf> {
        let pending = self.inner.pending.lock().expect("pending lock");
        let inodes = self.inner.inodes.lock().expect("inode lock");
        pending
            .warm
            .keys()
            .filter(|id| !pending.is_orphan(**id))
            .filter_map(|id| inodes.by_id.get(id).and_then(warm_path_of_record))
            .map(Path::to_path_buf)
            .collect()
    }

    /// Test-only accessor: was `path` promoted to a CAS blob? Returns
    /// the blob oid so dedup tests can compare across mounts.
    #[cfg(test)]
    pub(crate) fn warm_blob(&self, path: impl AsRef<Path>) -> Option<ContentHash> {
        let path = path.as_ref();
        let id = self
            .inner
            .inodes
            .lock()
            .expect("inode lock")
            .by_path
            .get(path)
            .copied()?;
        self.inner
            .pending
            .lock()
            .expect("pending lock")
            .warm
            .get(&id)
            .map(|e| e.blob)
    }

    /// Test-only accessor: are there any open hot-tier buffers?
    #[cfg(test)]
    pub(crate) fn hot_buffer_count(&self) -> usize {
        self.inner.pending.lock().expect("pending lock").hot.len()
    }

    /// Test-only accessor: snapshot of currently tombstoned paths.
    #[cfg(test)]
    #[allow(dead_code)]
    pub(crate) fn tombstones(&self) -> Vec<PathBuf> {
        self.inner
            .pending
            .lock()
            .expect("pending lock")
            .tombstones
            .iter()
            .cloned()
            .collect()
    }

    /// Test-only accessor for the wrapped repository.
    #[cfg(test)]
    pub(crate) fn repo_handle(&self) -> &Repository<RefManager, OpLog, S> {
        &self.inner.repo
    }

    /// Test-only accessor: is `node` currently marked as an orphaned
    /// inode (open-unlinked or rename-displaced with surviving fds)?
    #[cfg(test)]
    pub(crate) fn orphans_contains(&self, node: NodeId) -> bool {
        self.inner
            .pending
            .lock()
            .expect("pending lock")
            .is_orphan(node.0)
    }
}

/// Low-level test helpers. The mount doesn't yet expose a `create()`
/// entrypoint (the FUSE adapter will eventually wire that callback);
/// for now tests bypass the kernel-walk and install pending records
/// directly. The shape mirrors what `Filesystem::create` will do once
/// it lands.
#[cfg(test)]
pub(crate) mod test_helpers {
    use super::*;

    /// Mint a fresh pending-file at any (possibly nested) mount-relative
    /// path. Path components are taken verbatim — the helper does no
    /// validation beyond path normalization.
    pub(crate) fn install_pending_file(
        mount: &ContentAddressedMount,
        name: &str,
        mode: FileMode,
    ) -> NodeId {
        let path = PathBuf::from(name);
        mount.intern(NodeRecord::PendingFile { path, mode })
    }
}
