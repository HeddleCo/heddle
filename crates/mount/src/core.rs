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
        atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering},
        Arc, Mutex, RwLock, Weak,
    },
    thread::JoinHandle,
    time::{Duration, Instant, SystemTime},
};

use crate::cache::BlobCachePool;

use objects::{
    object::{
        Attribution, Blob, ChangeId, ContentHash, EntryType, FileMode, State, Tree, TreeEntry,
    },
    store::ObjectStore,
};
use refs::Head;
use repo::Repository;
use tracing::{debug, instrument, warn};

use crate::{
    error::{MountError, Result},
    shell::{Attrs, DIR_UNIX_MODE, Entry, NodeId, NodeKind, PlatformShell, kind_for_mode},
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
            NodeRecord::Symlink { .. } => NodeKind::Symlink,
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
            NodeRecord::Symlink { .. } => FileMode::Symlink.to_unix_mode(),
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
            NodeRecord::File { path, .. } | NodeRecord::PendingFile { path, .. } => {
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
                    self.by_path.remove(&path);
                }
                NodeRecord::File { path, .. } | NodeRecord::PendingFile { path, .. } => {
                    self.by_path.remove(&path);
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

/// The two-tier write state for a mount.
#[derive(Default)]
struct Pending {
    /// Hot tier: per-`NodeId` open-file buffers.
    hot: BTreeMap<u64, HotBuffer>,
    /// Reverse: which NodeId currently owns a buffer for `path`. We
    /// only allow one at a time — opening the same file twice from
    /// different node ids resolves to the same buffer because the
    /// inode registry coalesces by path for pending files.
    hot_by_path: BTreeMap<PathBuf, u64>,
    /// Warm tier: paths whose latest content has been promoted.
    warm: BTreeMap<PathBuf, PendingEntry>,
    /// Tombstones — paths the mount has deleted. Suppress the
    /// underlying state's entry on reads.
    tombstones: BTreeSet<PathBuf>,
}

/// In-mount overlay: a snapshot-time view of the parent state plus
/// pending writes the agent has issued since.
///
/// Writes never modify the immutable state; they accumulate in
/// [`Pending`] until [`ContentAddressedMount::capture`] folds them
/// into a fresh state.
pub struct ContentAddressedMount {
    inner: Arc<MountInner>,
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
pub(crate) struct MountInner {
    repo: Repository,
    thread: String,
    state: RwLock<MountState>,
    inodes: Mutex<Inodes>,
    pending: Mutex<Pending>,
    promotion: RwLock<PromotionPolicy>,
    mounted_at: SystemTime,
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
    fn start(weak: Weak<MountInner>) -> Self {
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
fn prewarm_run(weak: Weak<MountInner>, cancel: Arc<AtomicBool>) -> PrewarmStats {
    let Some(inner) = weak.upgrade() else {
        return PrewarmStats::default();
    };

    // Phase 1: tree walk. Cheap (in-memory tree + recent-trees
    // cache) so we just do it on the coordinator thread.
    let mut stats = PrewarmStats::default();
    let mut hashes: Vec<ContentHash> = Vec::new();
    let root_tree = inner.state.read().expect("mount state lock").tree;
    let mut queue: VecDeque<ContentHash> = VecDeque::from([root_tree]);
    let mut seen_trees: std::collections::HashSet<ContentHash> =
        std::collections::HashSet::new();
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

impl Drop for ContentAddressedMount {
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

impl ContentAddressedMount {
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
    pub fn new(repo: Repository, thread: impl Into<String>) -> Result<Self> {
        Self::with_options(repo, thread, MountOptions::default())
    }

    /// Construct a mount with explicit options. Lets the caller share
    /// a blob cache across mounts in the same process — see
    /// [`MountOptions::blob_cache`].
    pub fn with_options(
        repo: Repository,
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

    fn store(&self) -> &dyn ObjectStore {
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
        let entry_path = if parent_path.as_os_str().is_empty() {
            PathBuf::from(&tree_entry.name)
        } else {
            parent_path.join(&tree_entry.name)
        };
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
            _ => None,
        }
    }

    // --- Pending tier helpers ------------------------------------------------

    fn promote_idle_buffers(&self) -> Result<()> {
        self.inner.sweep_idle_buffers()
    }

    /// Promote the hot buffer for `node` (if any) to a CAS blob and
    /// record it in the pending tree.
    pub fn flush_node(&self, node: NodeId) -> Result<()> {
        self.inner.flush_node(node)
    }

    /// Mark `path` as deleted in the pending tier. Subsequent
    /// `lookup`/`enumerate` calls will skip the underlying captured
    /// entry, and `capture()` will fold the deletion into the new
    /// state's tree (pruning empty parent dirs as needed).
    pub fn unlink_path(&self, path: impl AsRef<Path>) -> Result<()> {
        let path = path.as_ref().to_path_buf();
        let mut pending = self.inner.pending.lock().expect("pending lock");
        // Drop any in-flight buffer for this path.
        if let Some(node_id) = pending.hot_by_path.remove(&path) {
            pending.hot.remove(&node_id);
        }
        pending.warm.remove(&path);
        pending.tombstones.insert(path);
        Ok(())
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
    fn pending_lookup(&self, path: &Path) -> Option<PendingHit> {
        let pending = self.inner.pending.lock().expect("pending lock");
        if pending.tombstones.contains(path) {
            return Some(PendingHit::Tombstone);
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
        if let Some(entry) = pending.warm.get(path) {
            return Some(PendingHit::Warm {
                blob: entry.blob,
                size: entry.size,
                mode: entry.mode,
            });
        }
        None
    }

    /// Does any pending entry sit *under* `dir` as a strict prefix?
    /// I.e. has an agent created `dir/something` even though `dir`
    /// itself isn't in the captured tree yet?
    fn pending_dir_exists(&self, dir: &Path) -> bool {
        if dir.as_os_str().is_empty() {
            return false;
        }
        let pending = self.inner.pending.lock().expect("pending lock");
        let prefix = dir;
        let probe = |path: &Path| -> bool {
            path.strip_prefix(prefix)
                .ok()
                .and_then(|tail| tail.components().next())
                .is_some()
        };
        pending
            .warm
            .keys()
            .any(|p| !pending.tombstones.contains(p) && probe(p))
            || pending
                .hot_by_path
                .keys()
                .any(|p| !pending.tombstones.contains(p) && probe(p))
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
        for (path, entry) in pending.warm.iter() {
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
        out.into_iter().collect()
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
    Dir,
}

impl MountInner {
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
    fn flush_node(&self, node: NodeId) -> Result<()> {
        let (path, mode, bytes) = {
            let mut pending = self.pending.lock().expect("pending lock");
            let Some(buf) = pending.hot.remove(&node.0) else {
                return Ok(());
            };
            pending.hot_by_path.remove(&buf.path);
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
        pending.warm.insert(
            path.clone(),
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
}

/// Spawn the safety-sweep worker, if one is requested by the
/// inner's promotion policy. The worker holds a `Weak<MountInner>`
/// so the mount can drop normally; on each tick it upgrades the
/// weak handle and drains any hot buffer that's been idle longer
/// than `idle_after`. A `None` `sweep_interval` returns `None`,
/// meaning event-driven promotion only.
fn spawn_sweep_worker(inner: &Arc<MountInner>) -> Option<SweepHandle> {
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
fn sweep_worker_loop(
    inner: std::sync::Weak<MountInner>,
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

fn resolve_thread(repo: &Repository, thread: &str) -> Result<MountState> {
    let change_id = repo
        .refs()
        .get_thread(thread)?
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

impl PlatformShell for ContentAddressedMount {
    fn lookup(&self, parent: NodeId, name: &OsStr) -> Result<Option<Entry>> {
        // Re-derive the parent's authoritative state from the registry,
        // so callers can't make us walk a tree we haven't blessed.
        let record = self.record_for(parent)?;
        let parent_path = match self.dir_path_of(&record) {
            Some(p) => p,
            None => return Ok(None),
        };
        let Some(name_str) = name.to_str() else {
            return Ok(None);
        };
        let child_path = if parent_path.as_os_str().is_empty() {
            PathBuf::from(name_str)
        } else {
            parent_path.join(name_str)
        };

        // Pending tier wins over the immutable tree for files —
        // that's what makes "write then read" return the new bytes.
        match self.pending_lookup(&child_path) {
            Some(PendingHit::Tombstone) => return Ok(None),
            Some(PendingHit::Hot { node, size, mode }) => {
                return Ok(Some(Entry {
                    node,
                    name: OsString::from(name_str),
                    kind: kind_for_mode(mode),
                    size,
                    unix_mode: mode.to_unix_mode(),
                }));
            }
            Some(PendingHit::Warm {
                blob: _,
                size,
                mode,
            }) => {
                let node = self.intern(NodeRecord::PendingFile {
                    path: child_path.clone(),
                    mode,
                });
                return Ok(Some(Entry {
                    node,
                    name: OsString::from(name_str),
                    kind: kind_for_mode(mode),
                    size,
                    unix_mode: mode.to_unix_mode(),
                }));
            }
            None => {}
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
                // the buffer (e.g. after rename/coalesce).
                let warm_blob = {
                    let pending = self.inner.pending.lock().expect("pending lock");
                    if let Some(id) = pending.hot_by_path.get(path).copied()
                        && let Some(hot) = pending.hot.get(&id)
                    {
                        return Ok(copy_into(&hot.bytes, offset, buf));
                    }
                    pending.warm.get(path).map(|e| e.blob)
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
            NodeRecord::File { blob, .. } | NodeRecord::Symlink { blob } => {
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
            let pending = self.inner.pending.lock().expect("pending lock");
            if pending.hot.contains_key(&node.0)
                || pending
                    .hot_by_path
                    .get(&path)
                    .is_some_and(|id| pending.hot.contains_key(id))
            {
                // A buffer already exists — no seed needed; the
                // existing buffer's bytes are authoritative.
                Seed::None
            } else if pending.tombstones.contains(&path) {
                // Unlink-then-write: start from empty.
                Seed::None
            } else if let Some(entry) = pending.warm.get(&path) {
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
        // Coalesce two NodeIds for the same path onto the same buffer.
        if let Some(existing_id) = pending.hot_by_path.get(&path).copied()
            && existing_id != node.0
            && let Some(buf) = pending.hot.remove(&existing_id)
        {
            pending.hot.insert(node.0, buf);
        }
        pending.hot_by_path.insert(path.clone(), node.0);
        // A live hot buffer means the file exists again — clear any
        // tombstone for this path so subsequent `pending_lookup` calls
        // see the buffer instead of a "deleted" sentinel. POSIX:
        // unlink+open(O_CREAT)+pwrite reborns the path. The seed
        // logic above already starts the buffer empty when a tombstone
        // is present, so we don't need to inspect the tombstone here.
        pending.tombstones.remove(&path);
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

        // Pass 1: captured-tree entries, with pending overlay.
        for tree_entry in tree.entries() {
            let entry_path = if parent_path.as_os_str().is_empty() {
                PathBuf::from(&tree_entry.name)
            } else {
                parent_path.join(&tree_entry.name)
            };
            match self.pending_lookup(&entry_path) {
                Some(PendingHit::Tombstone) => continue,
                Some(PendingHit::Hot { node, size, mode }) => {
                    by_name.insert(
                        tree_entry.name.clone(),
                        Entry {
                            node,
                            name: OsString::from(&tree_entry.name),
                            kind: kind_for_mode(mode),
                            size,
                            unix_mode: mode.to_unix_mode(),
                        },
                    );
                    continue;
                }
                Some(PendingHit::Warm {
                    blob: _,
                    size,
                    mode,
                }) => {
                    let node = self.intern(NodeRecord::PendingFile {
                        path: entry_path,
                        mode,
                    });
                    by_name.insert(
                        tree_entry.name.clone(),
                        Entry {
                            node,
                            name: OsString::from(&tree_entry.name),
                            kind: kind_for_mode(mode),
                            size,
                            unix_mode: mode.to_unix_mode(),
                        },
                    );
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
            let full_path = if parent_path.as_os_str().is_empty() {
                PathBuf::from(&name)
            } else {
                parent_path.join(&name)
            };
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
                    let child_count = self.pending_children_at(&full_path).len() as u64;
                    let node = self.intern(NodeRecord::PendingDir { path: full_path });
                    by_name.insert(
                        name.clone(),
                        Entry {
                            node,
                            name: OsString::from(&name),
                            kind: NodeKind::Directory,
                            size: child_count,
                            unix_mode: DIR_UNIX_MODE,
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
            NodeRecord::File { blob, .. } | NodeRecord::Symlink { blob } => {
                // Hot buffer overrides the captured size if the agent
                // is currently editing this file via this NodeId.
                let pending = self.inner.pending.lock().expect("pending lock");
                if let Some(buf) = pending.hot.get(&node.0) {
                    (buf.bytes.len() as u64, 1)
                } else {
                    drop(pending);
                    (self.blob_size(blob)?, 1)
                }
            }
            NodeRecord::PendingFile { path, .. } => {
                let hit = self
                    .pending_lookup(path)
                    .ok_or_else(|| MountError::Stale(format!("pending file {}", path.display())))?;
                let size = match hit {
                    PendingHit::Hot { size, .. } | PendingHit::Warm { size, .. } => size,
                    PendingHit::Tombstone => 0,
                };
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
        // Drop any hot buffer attached to this NodeId — the kernel
        // is telling us our cached identity is no longer valid, and
        // we don't want a stale buffer surviving the inode flip.
        {
            let mut pending = self.inner.pending.lock().expect("pending lock");
            if let Some(buf) = pending.hot.remove(&node.0) {
                pending.hot_by_path.remove(&buf.path);
            }
        }
        self.inner.inodes.lock().expect("inode lock").forget(node);
        Ok(())
    }

    fn flush(&self, node: NodeId) -> Result<()> {
        self.flush_node(node)
    }

    fn release(&self, node: NodeId) -> Result<()> {
        self.flush_node(node)
    }
}

// --- Capture --------------------------------------------------------------

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
            apply_pending_to_tree(self.store(), &parent_tree, &pending)?
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
        self.store().put_state(&state).map_err(MountError::Store)?;

        // Step 3: advance the thread's HEAD. We respect whatever
        // head the repo currently has (Attached vs Detached): mounts
        // are always created against a thread name, so we walk the
        // attached path, but be defensive and fall back to setting
        // the named thread directly if HEAD is detached.
        let change_id = state.change_id;
        let prev_head_change_id = state_snapshot.change_id;
        match self.inner.repo.head_ref().map_err(MountError::Store)? {
            Head::Attached { thread } if thread == self.inner.thread => {
                self.inner
                    .repo
                    .refs()
                    .set_thread(&thread, &change_id)
                    .map_err(MountError::Store)?;
            }
            _ => {
                // Always update the named thread, even if HEAD is
                // pointed elsewhere. The mount serves a specific
                // thread; that's what should advance.
                self.inner
                    .repo
                    .refs()
                    .set_thread(&self.inner.thread, &change_id)
                    .map_err(MountError::Store)?;
            }
        }

        // Step 3a: record the snapshot in the oplog. Mirrors what
        // `repository_snapshot.rs` does after a worktree-walk
        // capture and what `cmd_snapshot` relies on for `heddle
        // undo` / `heddle log`. We pass `prev_head` so the entry
        // captures the parent-state edge for traversal.
        if let Err(err) = repo::snapshot_metadata::record_snapshot_in_oplog(
            &self.inner.repo,
            &change_id,
            Some(&prev_head_change_id),
            Some(&self.inner.thread),
        ) {
            warn!(?err, "oplog record_snapshot from mount capture failed");
        }

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

        // Step 4: clear the pending tier and refresh state.
        {
            let mut pending = self.inner.pending.lock().expect("pending lock");
            pending.hot.clear();
            pending.hot_by_path.clear();
            pending.warm.clear();
            pending.tombstones.clear();
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
    store: &dyn ObjectStore,
    parent: &Tree,
    pending: &Pending,
) -> Result<ContentHash> {
    /// In-memory virtual tree: a directory's local file overrides,
    /// tombstones, and named child directories. Built lazily during
    /// the walk; materialized recursively.
    #[derive(Default)]
    struct VDir {
        /// File leaves to plant in this directory (overrides any
        /// captured entry of the same name).
        files: BTreeMap<String, (ContentHash, FileMode)>,
        /// Names to tombstone (file or subdirectory).
        deletions: BTreeSet<String>,
        /// Named child directories that have pending content.
        children: BTreeMap<String, VDir>,
    }

    let mut root = VDir::default();

    fn descend<'a>(node: &'a mut VDir, components: &[&str]) -> &'a mut VDir {
        let mut cursor = node;
        for c in components {
            cursor = cursor.children.entry((*c).to_string()).or_default();
        }
        cursor
    }

    // Plant warm entries.
    for (path, entry) in &pending.warm {
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
        dir.deletions.insert((*leaf_name).to_string());
    }

    /// Materialize a virtual directory against its captured counterpart
    /// `captured` (or `Tree::new()` if no captured tree exists). Writes
    /// every subtree to `store` and returns the resulting tree's hash,
    /// or `None` if the resulting tree is empty (a signal the parent
    /// should drop the entry).
    fn materialize(
        v: &VDir,
        captured: &Tree,
        store: &dyn ObjectStore,
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

        // File overrides.
        for (name, (blob, mode)) in &v.files {
            let executable = matches!(mode, FileMode::Executable);
            let entry = TreeEntry::file(name.clone(), *blob, executable).map_err(|e| {
                MountError::Store(objects::error::HeddleError::InvalidObject(e.to_string()))
            })?;
            entries.insert(name.clone(), entry);
        }

        // Recurse into each pending subdirectory.
        for (name, child) in &v.children {
            // Captured counterpart: if `captured` already has a
            // subdir under this name, load it; otherwise start from
            // an empty tree.
            let child_captured = match captured.get(name) {
                Some(e) if e.is_tree() => store
                    .get_tree(&e.hash)
                    .map_err(MountError::Store)?
                    .unwrap_or_default(),
                _ => Tree::new(),
            };
            match materialize(child, &child_captured, store)? {
                Some(hash) => {
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

impl ContentAddressedMount {
    /// Test-only accessor for the warm tier so unit tests can verify
    /// promotions landed without going through `read`.
    #[cfg(test)]
    pub(crate) fn warm_keys(&self) -> Vec<PathBuf> {
        self.inner
            .pending
            .lock()
            .expect("pending lock")
            .warm
            .keys()
            .cloned()
            .collect()
    }

    /// Test-only accessor: was `path` promoted to a CAS blob? Returns
    /// the blob oid so dedup tests can compare across mounts.
    #[cfg(test)]
    pub(crate) fn warm_blob(&self, path: impl AsRef<Path>) -> Option<ContentHash> {
        self.inner
            .pending
            .lock()
            .expect("pending lock")
            .warm
            .get(path.as_ref())
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
    pub(crate) fn repo_handle(&self) -> &Repository {
        &self.inner.repo
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