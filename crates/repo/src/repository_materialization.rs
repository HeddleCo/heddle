// SPDX-License-Identifier: Apache-2.0
//! Tree materialization helpers.

use objects::store::ObjectStore;
use std::{
    collections::BTreeSet,
    fs,
    num::NonZeroUsize,
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
    thread,
    time::Instant,
};

use objects::{
    fs_atomic::enrich_fs_error,
    object::{ChangeId, ContentHash, EntryType, Tree},
};
use tracing::{debug, instrument};

// Only consumed by the `#[cfg(unix)]` `remove_materialized_leaf`
// helper; gate the import so Windows builds don't warn it unused.
#[cfg(unix)]
use super::repository_worktree_apply::is_directory_not_empty;
use super::{HeddleError, Repository, Result};
use crate::{
    worktree_index::IndexEntry,
    worktree_walk::{build_cached_entry, cache_key},
};

/// State threaded through a single `materialize_write_ops_seeded` call.
/// Tracks whether filesystem-level reflinks (CoW clones) are viable on
/// this destination filesystem, so we don't pay the per-blob
/// `clonefile`/`FICLONE` retry tax once we've seen
/// `EXDEV`/`EOPNOTSUPP`/`ENOSYS` from one of them. Reflink and copy
/// counts are emitted at the end for observability.
///
/// SAFETY/CORRECTNESS NOTE on isolated blobs:
///   We materialize blobs via filesystem-level copy-on-write
///   ("reflink") where supported (`clonefile(2)` on macOS APFS,
///   `ioctl(FICLONE)` on Linux btrfs/XFS-with-reflinks/ZFS), and via
///   `fs::copy` everywhere else. **Both paths give the destination
///   its own inode.** A worktree file is never an alias of the
///   canonical loose blob nor of any other worktree's file — so an
///   agent that runs `chmod +w file && echo new > file` only mutates
///   *that* worktree's bytes. The OS handles the divergence: with a
///   reflink the kernel forks the underlying allocation on first
///   write; with a real copy the dest is a separate file from the
///   start. Either way, no shared-inode hazard exists.
///
///   This replaces an earlier hardlink-plus-`chmod 0o444` defense
///   that turned out to be trivially bypassable. The hardlink made
///   the worktree file an alias of the canonical loose blob; the
///   read-only mode was a soft hint that any agent could (and did)
///   undo with `chmod 644`. The new model is filesystem-level and
///   not bypassable from userspace.
struct MaterializationContext {
    reflink_supported: AtomicBool,
    reflink_count: std::sync::atomic::AtomicUsize,
    copy_count: std::sync::atomic::AtomicUsize,
}

impl MaterializationContext {
    fn new() -> Self {
        Self {
            // Optimistic: try reflink on the first blob; a single
            // `EXDEV`/`EOPNOTSUPP` flips this for the rest of the batch.
            reflink_supported: AtomicBool::new(true),
            reflink_count: std::sync::atomic::AtomicUsize::new(0),
            copy_count: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    fn reflinks_enabled(&self) -> bool {
        self.reflink_supported.load(Ordering::Relaxed)
    }

    fn record_reflink(&self) {
        self.reflink_count.fetch_add(1, Ordering::Relaxed);
    }

    fn record_copy(&self) {
        self.copy_count.fetch_add(1, Ordering::Relaxed);
    }

    /// Disable reflink attempts for the rest of this materialization
    /// after the kernel told us the filesystem won't ever clone.
    fn disable_reflinks(&self) {
        self.reflink_supported.store(false, Ordering::Relaxed);
    }
}

const MATERIALIZE_PARALLEL_THRESHOLD: usize = 32;
const MATERIALIZE_THREADS_ENV: &str = "HEDDLE_MATERIALIZE_THREADS";

struct MaterializationPlan {
    directories: Vec<PathBuf>,
    directory_contexts: Vec<MaterializedDirectoryContext>,
    leaves: Vec<WorktreeWriteOp>,
    file_count: usize,
    symlink_count: usize,
}

#[derive(Debug)]
pub(crate) struct MaterializedTree {
    pub(crate) file_entries: Vec<SeededWorktreeEntry>,
    pub(crate) directory_contexts: Vec<MaterializedDirectoryContext>,
}

#[derive(Debug)]
pub(crate) struct SeededWorktreeEntry {
    pub(crate) key: String,
    pub(crate) entry: IndexEntry,
}

#[derive(Debug)]
pub(crate) struct MaterializedDirectoryContext {
    pub(crate) key: String,
    pub(crate) path: PathBuf,
    pub(crate) child_names: Vec<String>,
    pub(crate) tree_hash: ContentHash,
}

#[derive(Clone, Debug)]
pub(crate) enum WorktreeWriteOp {
    Blob {
        path: PathBuf,
        hash: ContentHash,
        executable: bool,
    },
    Symlink {
        path: PathBuf,
        hash: ContentHash,
    },
}

impl WorktreeWriteOp {
    pub(crate) fn path(&self) -> &Path {
        match self {
            Self::Blob { path, .. } | Self::Symlink { path, .. } => path,
        }
    }

    pub(crate) fn hash(&self) -> ContentHash {
        match self {
            Self::Blob { hash, .. } | Self::Symlink { hash, .. } => *hash,
        }
    }

    pub(crate) fn executable(&self) -> bool {
        match self {
            Self::Blob { executable, .. } => *executable,
            Self::Symlink { .. } => false,
        }
    }

    pub(crate) fn index_kind(&self) -> crate::worktree_index::IndexEntryKind {
        match self {
            Self::Blob { .. } => crate::worktree_index::IndexEntryKind::File,
            Self::Symlink { .. } => crate::worktree_index::IndexEntryKind::Symlink,
        }
    }
}

/// Result of `Repository::warm_canonical_store_for_state(s)`.
///
/// The reflink-first materializer can only clone from a canonical
/// loose-uncompressed file. After `pack_objects + prune_loose_objects`
/// (the steady state for any non-fresh repo) every blob is pack-only
/// and `loose_blob_path` returns `None`. The warm pass walks a
/// state's tree(s) and promotes every reachable blob in advance so
/// the next N materializations of that state across N worktrees all
/// hit the fast path.
///
/// This is the proactive twin of the lazy promotion that already
/// fires inside `materialize_blob`. Lazy is correct on its own; warm
/// is a latency optimization for the "I'm about to materialize this
/// state to N worktrees" case (e.g. `heddle delegate`).
#[derive(Debug, Default, Clone, Copy)]
pub struct WarmCanonicalStoreStats {
    /// Blobs we wrote to the canonical loose-uncompressed path
    /// because they were either pack-only or compressed-loose.
    pub promoted: usize,
    /// Blobs that were already loose+uncompressed; no work done.
    pub already_loose: usize,
    /// Blobs we tried to promote but `promote_to_loose_uncompressed`
    /// returned an error (e.g. the blob isn't in the store, or a
    /// transient I/O failure during the atomic write). Kept
    /// non-fatal: the lazy path will retry on materialize, and a
    /// real corruption shows up there with a louder error.
    pub errors: usize,
}

impl WarmCanonicalStoreStats {
    /// Total blobs visited.
    pub fn total(&self) -> usize {
        self.promoted + self.already_loose + self.errors
    }
}

impl Repository {
    /// Promote every reachable blob from `state_id`'s tree(s) into
    /// the canonical loose-uncompressed store, so a subsequent
    /// `materialize_tree` (or N parallel materializations) can
    /// reflink from the canonical store without paying the
    /// decompress-on-first-clone tax.
    ///
    /// Returns counts of work done. Errors per blob are accumulated
    /// rather than bubbled up so a single corrupt or missing object
    /// doesn't poison the whole warm pass — the lazy path inside
    /// `materialize_blob` will surface that loudly when it actually
    /// matters.
    #[instrument(skip(self), fields(state_id = %state_id))]
    pub fn warm_canonical_store_for_state(
        &self,
        state_id: &ChangeId,
    ) -> Result<WarmCanonicalStoreStats> {
        self.warm_canonical_store_for_states(std::slice::from_ref(state_id))
    }

    /// Multi-state variant. Walks each state's tree once, dedupes
    /// the union of reachable blob hashes across all of them, and
    /// promotes them. Useful when materializing several sibling
    /// states from the same parent in quick succession (the
    /// `heddle delegate`-style flow).
    #[instrument(skip(self, state_ids), fields(state_count = state_ids.len()))]
    pub fn warm_canonical_store_for_states(
        &self,
        state_ids: &[ChangeId],
    ) -> Result<WarmCanonicalStoreStats> {
        let mut blob_hashes = BTreeSet::new();
        for state_id in state_ids {
            let state = self
                .store
                .get_state(state_id)?
                .ok_or_else(|| HeddleError::NotFound(format!("state {} not in store", state_id)))?;
            let tree = self.store.get_tree(&state.tree)?.ok_or_else(|| {
                HeddleError::NotFound(format!("tree {} (for state {})", state.tree, state_id))
            })?;
            self.collect_blob_hashes(&tree, &mut blob_hashes)?;
        }

        let mut stats = WarmCanonicalStoreStats::default();
        for hash in &blob_hashes {
            match self.store.promote_to_loose_uncompressed(hash) {
                Ok(true) => stats.promoted += 1,
                Ok(false) => stats.already_loose += 1,
                Err(err) => {
                    debug!(
                        ?err,
                        hash = %hash,
                        "promote_to_loose_uncompressed failed during warm pass"
                    );
                    stats.errors += 1;
                }
            }
        }

        debug!(
            promoted = stats.promoted,
            already_loose = stats.already_loose,
            errors = stats.errors,
            "Warm canonical store pass complete"
        );

        Ok(stats)
    }

    fn collect_blob_hashes(&self, tree: &Tree, out: &mut BTreeSet<ContentHash>) -> Result<()> {
        for entry in tree.entries() {
            // Symlink targets are stored as blobs too — they're
            // small, so promotion cost is negligible, and a stored
            // symlink is materialized via `get_blob` (not hardlink),
            // so promoting them is technically wasted work. But
            // skipping symlinks would mean walking the tree with
            // the same defensive `is_symlink` guard we use in
            // `plan_materialization`, and the cost of warming a few
            // tiny symlink-target blobs is dwarfed by the
            // decompress cost of even one real source file. Keep
            // it simple: promote everything reachable.
            match entry.entry_type {
                EntryType::Blob | EntryType::Symlink => {
                    out.insert(entry.hash);
                }
                EntryType::Tree => {
                    let subtree = self
                        .store
                        .get_tree(&entry.hash)?
                        .ok_or_else(|| HeddleError::NotFound(format!("tree {}", entry.hash)))?;
                    self.collect_blob_hashes(&subtree, out)?;
                }
            }
        }
        Ok(())
    }

    /// Materialize a tree to the filesystem.
    #[instrument(skip(self, tree), fields(dir = %dir.display(), entries = tree.len()))]
    pub fn materialize_tree(&self, tree: &Tree, dir: &Path) -> Result<()> {
        self.materialize_tree_seeded(tree, dir).map(|_| ())
    }

    pub(crate) fn materialize_tree_seeded(
        &self,
        tree: &Tree,
        dir: &Path,
    ) -> Result<MaterializedTree> {
        let plan_start = Instant::now();
        let mut plan = MaterializationPlan {
            directories: Vec::new(),
            directory_contexts: Vec::new(),
            leaves: Vec::new(),
            file_count: 0,
            symlink_count: 0,
        };
        self.plan_materialization(tree, Path::new(""), dir, &mut plan)?;
        let plan_duration_ms = plan_start.elapsed().as_millis();

        let execution_start = Instant::now();
        let requested_threads = requested_materialization_threads();
        fs::create_dir_all(dir)
            .map_err(|e| HeddleError::Io(enrich_fs_error(dir, "creating", e)))?;
        for directory in &plan.directories {
            fs::create_dir_all(directory)
                .map_err(|e| HeddleError::Io(enrich_fs_error(directory, "creating", e)))?;
        }

        let (worker_count, file_entries) = self.materialize_write_ops_seeded(&plan.leaves)?;

        debug!(
            directories = plan.directories.len(),
            files = plan.file_count,
            symlinks = plan.symlink_count,
            workers = worker_count,
            requested_workers = requested_threads.map(NonZeroUsize::get),
            plan_duration_ms,
            execution_duration_ms = execution_start.elapsed().as_millis(),
            parallel = worker_count > 1,
            "Tree materialization complete"
        );

        Ok(MaterializedTree {
            file_entries,
            directory_contexts: plan.directory_contexts,
        })
    }

    fn plan_materialization(
        &self,
        tree: &Tree,
        rel_dir: &Path,
        dir: &Path,
        plan: &mut MaterializationPlan,
    ) -> Result<()> {
        plan.directory_contexts.push(MaterializedDirectoryContext {
            key: cache_key(rel_dir),
            path: dir.to_path_buf(),
            child_names: tree
                .entries()
                .iter()
                .map(|entry| entry.name.clone())
                .collect(),
            tree_hash: tree.hash(),
        });

        for entry in tree.entries() {
            let path = dir.join(&entry.name);
            let rel_path = rel_dir.join(&entry.name);
            // Defensive routing: a tree entry whose `mode` is Symlink should
            // be materialized as a real symlink even if its `entry_type`
            // says Blob. Pre-Phase-E imports stored symlinks as
            // `(EntryType::Blob, FileMode::Symlink)` and the resulting
            // worktree wrote the symlink target as plain file content.
            // This guard makes those legacy trees materialize correctly
            // on `goto` without requiring a re-import.
            let is_symlink = entry.entry_type == EntryType::Symlink
                || entry.mode == objects::object::FileMode::Symlink;
            if is_symlink {
                plan.symlink_count += 1;
                plan.leaves.push(WorktreeWriteOp::Symlink {
                    path,
                    hash: entry.hash,
                });
                continue;
            }
            match entry.entry_type {
                EntryType::Blob => {
                    plan.file_count += 1;
                    plan.leaves.push(WorktreeWriteOp::Blob {
                        path,
                        hash: entry.hash,
                        executable: entry.is_executable(),
                    });
                }
                EntryType::Tree => {
                    let subtree = self
                        .store
                        .get_tree(&entry.hash)?
                        .ok_or_else(|| HeddleError::NotFound(format!("tree {}", entry.hash)))?;
                    plan.directories.push(path.clone());
                    self.plan_materialization(&subtree, &rel_path, &path, plan)?;
                }
                EntryType::Symlink => {
                    // Already handled above; left here for exhaustiveness.
                    unreachable!(
                        "EntryType::Symlink should have been routed by the is_symlink guard"
                    );
                }
            }
        }

        Ok(())
    }

    pub(crate) fn materialize_write_ops(&self, writes: &[WorktreeWriteOp]) -> Result<usize> {
        self.materialize_write_ops_seeded(writes)
            .map(|(worker_count, _)| worker_count)
    }

    pub(crate) fn materialize_write_ops_seeded(
        &self,
        writes: &[WorktreeWriteOp],
    ) -> Result<(usize, Vec<SeededWorktreeEntry>)> {
        prepare_parent_directories(writes)?;

        let requested_threads = requested_materialization_threads();
        let worker_count = materialization_worker_count(writes.len(), requested_threads);

        // No probe — the per-blob path tries `clonefile`/FICLONE
        // first and flips a batch-wide flag on the first
        // `EXDEV`/`EOPNOTSUPP`/`ENOSYS` verdict, so the rest of the
        // batch falls straight through to `fs::copy` without paying
        // the syscall tax. The cost of one failed reflink call on a
        // non-CoW filesystem is one syscall; it's not worth a
        // dedicated probe.
        let context = MaterializationContext::new();

        let result = if worker_count <= 1 {
            let mut seeded = Vec::with_capacity(writes.len());
            for write in writes {
                seeded.push(self.materialize_write_op(write, &context)?);
            }
            Ok((worker_count, seeded))
        } else {
            let chunk_size = writes.len().div_ceil(worker_count);
            let seeded = thread::scope(|scope| -> Result<Vec<SeededWorktreeEntry>> {
                let mut workers = Vec::new();
                let context = &context;
                for chunk in writes.chunks(chunk_size) {
                    workers.push(scope.spawn(move || -> Result<Vec<SeededWorktreeEntry>> {
                        let mut seeded = Vec::with_capacity(chunk.len());
                        for write in chunk {
                            seeded.push(self.materialize_write_op(write, context)?);
                        }
                        Ok(seeded)
                    }));
                }

                let mut seeded = Vec::with_capacity(writes.len());
                for worker in workers {
                    seeded.extend(worker.join().map_err(|_| {
                        HeddleError::Config("materialization worker panicked".to_string())
                    })??);
                }

                Ok(seeded)
            })?;

            Ok((worker_count, seeded))
        };

        let reflinks = context.reflink_count.load(Ordering::Relaxed);
        let copies = context.copy_count.load(Ordering::Relaxed);
        if reflinks + copies > 0 {
            debug!(
                reflinks,
                copies,
                reflinks_enabled = context.reflinks_enabled(),
                "Materialized blobs"
            );
        }

        result
    }

    fn materialize_write_op(
        &self,
        write: &WorktreeWriteOp,
        context: &MaterializationContext,
    ) -> Result<SeededWorktreeEntry> {
        match write {
            WorktreeWriteOp::Blob {
                path,
                hash,
                executable,
            } => {
                self.materialize_blob(path, hash, *executable, context)?;
            }
            WorktreeWriteOp::Symlink { path, hash } => {
                let blob = self
                    .store
                    .get_blob(hash)?
                    .ok_or_else(|| HeddleError::NotFound(format!("blob {}", hash)))?;
                #[cfg(unix)]
                {
                    let target = std::str::from_utf8(blob.content()).map_err(|_| {
                        HeddleError::InvalidObject("invalid symlink target".to_string())
                    })?;
                    remove_materialized_leaf(path)?;
                    std::os::unix::fs::symlink(target, path)?;
                }
                // Windows symlink materialization is unimplemented;
                // the projection layer (ProjFS) handles symlinks
                // through reparse points instead of native symlinks,
                // and `heddle materialize` on Windows isn't part of
                // the daily-use mount story. Suppress the unused
                // bindings rather than ship a half-implementation.
                #[cfg(not(unix))]
                {
                    let _ = (blob, path);
                }
            }
        }

        let metadata = fs::symlink_metadata(write.path())?;
        let entry = build_cached_entry(
            write.hash(),
            &metadata,
            write.executable(),
            write.index_kind(),
        )
        .ok_or_else(|| {
            HeddleError::Config(format!(
                "seed materialized worktree entry for {}",
                write.path().display()
            ))
        })?;

        Ok(SeededWorktreeEntry {
            key: cache_key(
                write
                    .path()
                    .strip_prefix(self.root())
                    .unwrap_or(write.path()),
            ),
            entry,
        })
    }

    /// Materialize a single blob into the worktree.
    ///
    /// Strategy (in order):
    ///   1. Filesystem reflink (`clonefile(2)` on macOS APFS,
    ///      `ioctl(FICLONE)` on Linux btrfs/XFS/ZFS) from the
    ///      canonical loose-uncompressed blob into `dest`. The dest
    ///      gets its own inode; the kernel forks the underlying
    ///      allocation on first write to either side. On reflink-
    ///      capable filesystems this preserves the storage win
    ///      (~1× disk for N worktrees of the same state) without
    ///      any shared-inode hazard.
    ///   2. Lazy promotion + retry. If the canonical loose blob
    ///      isn't on disk (e.g. post-`pack_objects + prune_loose`),
    ///      promote it once and retry the reflink.
    ///   3. `fs::write` of the decompressed blob bytes. Used when the
    ///      filesystem doesn't support reflinks at all
    ///      (`EXDEV`/`EOPNOTSUPP`/`ENOSYS`), in which case we flip a
    ///      batch-wide flag and stop trying for the rest of this
    ///      materialization.
    ///
    /// Permission bits are normalized to `0o644` (or `0o755` for
    /// executables) on every path. There is no read-only-mode
    /// defense — agents can `chmod +w` and overwrite freely; the
    /// filesystem-level isolation is what keeps sibling worktrees
    /// safe.
    fn materialize_blob(
        &self,
        dest: &Path,
        hash: &ContentHash,
        executable: bool,
        context: &MaterializationContext,
    ) -> Result<()> {
        // Redaction short-circuit: if any redaction declares this
        // blob's bytes off-limits, materialize the human-readable
        // stub instead. The stub names who redacted it, when, why,
        // and whether the bytes have already been purged. Safe to
        // include in worktrees, semantic diffs, and bridge-git
        // exports (which themselves call through `materialize_tree`).
        // Errors loading the redactions store are propagated rather
        // than swallowed — a partial redaction read shouldn't
        // silently leak the original bytes.
        if let Some(stub) = self
            .redaction_stub_for_blob(hash)
            .map_err(|err| HeddleError::Config(format!("redaction lookup failed: {err}")))?
        {
            let _ = fs::remove_file(dest);
            fs::write(dest, stub.as_bytes())?;
            // Stubs are never executable — overwriting a tracked
            // executable with a stub correctly drops the +x bit so
            // operators don't accidentally run the redaction notice.
            set_file_mode(dest, false)?;
            // The redaction stub path doesn't reflink/clone — count
            // it as a copy so observability stays accurate.
            context.record_copy();
            let _ = executable;
            return Ok(());
        }

        if context.reflinks_enabled() {
            // First-pass: blob is already loose+uncompressed.
            if let Some(source) = self.store.loose_blob_path(hash)
                && self.try_clone(&source, dest, executable, context)?
            {
                return Ok(());
            }
            // Second-pass: lazy promotion. Pack-resident or
            // compressed-loose blob — promote it to the canonical
            // uncompressed-loose path, then retry the reflink.
            // Without this step `pack_objects + prune_loose_objects`
            // permanently degrades materialize to slow `fs::write`.
            //
            // The first materialize of any given hash pays
            // decompress + atomic write, but every subsequent one
            // (other worktrees, future `goto`s) is a single
            // `clonefile`/FICLONE. Net win for any N > 1
            // materializations on a CoW filesystem.
            match self.store.promote_to_loose_uncompressed(hash) {
                Ok(_) => {
                    if let Some(source) = self.store.loose_blob_path(hash)
                        && self.try_clone(&source, dest, executable, context)?
                    {
                        return Ok(());
                    }
                }
                Err(err) => {
                    debug!(
                        ?err,
                        hash = %hash,
                        "promote_to_loose_uncompressed failed; falling back to fs::write"
                    );
                }
            }
        }

        let blob = self
            .store
            .get_blob(hash)?
            .ok_or_else(|| HeddleError::NotFound(format!("blob {}", hash)))?;
        // Remove any stale dest before writing. We don't share inodes
        // with the canonical store anymore (no hardlinks), but a
        // previous `goto` could still have left an unrelated file
        // here that we should overwrite cleanly.
        let _ = fs::remove_file(dest);
        fs::write(dest, blob.content())?;
        set_file_mode(dest, executable)?;
        context.record_copy();
        Ok(())
    }

    /// One clone attempt: returns `Ok(true)` on a successful reflink
    /// or fallback `fs::copy`, `Ok(false)` only when the
    /// filesystem-level helper reports the operation isn't supported
    /// (`EXDEV`/`EOPNOTSUPP`/`ENOSYS`/`EINVAL`). On the unsupported
    /// verdict the context is flipped so the rest of the batch skips
    /// straight to the in-memory `fs::write` path without paying the
    /// failed-syscall tax. Genuine I/O errors bubble up.
    fn try_clone(
        &self,
        source: &Path,
        dest: &Path,
        executable: bool,
        context: &MaterializationContext,
    ) -> Result<bool> {
        // `clonefile`/`FICLONE` fail if `dest` already exists, so
        // make sure we're starting from a clean slate. A previous
        // `goto` could have left a regular file or a stale link here.
        let _ = fs::remove_file(dest);
        match objects::fs_clone::try_reflink(source, dest) {
            Ok(true) => {
                set_file_mode(dest, executable)?;
                context.record_reflink();
                Ok(true)
            }
            Ok(false) => {
                // Filesystem doesn't support reflinks. Disable for
                // the rest of the batch and let the caller fall
                // through to `fs::write` (which decompresses from
                // memory rather than reading the loose file twice).
                debug!(
                    source = %source.display(),
                    dest = %dest.display(),
                    "reflink not supported on this filesystem; switching batch to fs::write fallback"
                );
                context.disable_reflinks();
                Ok(false)
            }
            Err(err) => {
                debug!(
                    ?err,
                    source = %source.display(),
                    dest = %dest.display(),
                    "reflink failed with I/O error"
                );
                Err(err.into())
            }
        }
    }
}

fn prepare_parent_directories(writes: &[WorktreeWriteOp]) -> Result<()> {
    let mut parents = BTreeSet::new();
    for write in writes {
        if let Some(parent) = write.path().parent() {
            parents.insert(parent.to_path_buf());
        }
    }

    for parent in parents {
        fs::create_dir_all(&parent)
            .map_err(|e| HeddleError::Io(enrich_fs_error(&parent, "creating", e)))?;
    }

    Ok(())
}

/// Best-effort removal of a leaf path, used by the symlink-write
/// branch when a tree entry has changed shape (e.g. a directory has
/// become a symlink in the new tree).
///
/// Tolerates `ENOTEMPTY` from `remove_dir` for the same reason the
/// incremental apply path does: untracked or explicitly ignored siblings
/// may still occupy the directory after the planner has cleaned out the
/// tracked children. Without this
/// tolerance, a `goto` over a real-world worktree that mutates a
/// tracked directory into a symlink aborts mid-apply with `os error
/// 66`, leaving HEAD stuck and disk diverged from state.
///
/// Only called from the `#[cfg(unix)]` symlink-write branch above;
/// the `#[cfg(not(unix))]` build skips the call (no Windows symlink
/// materialization), which would warn "function never used" without
/// the matching gate here.
#[cfg(unix)]
fn remove_materialized_leaf(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            let file_type = metadata.file_type();
            if file_type.is_symlink() || file_type.is_file() {
                fs::remove_file(path)
                    .map_err(|e| HeddleError::Io(enrich_fs_error(path, "removing", e)))?;
            } else if file_type.is_dir() {
                match fs::remove_dir(path) {
                    Ok(()) => {}
                    Err(error) if is_directory_not_empty(&error) => {}
                    Err(error) => {
                        return Err(HeddleError::Io(enrich_fs_error(path, "removing", error)));
                    }
                }
            }
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(HeddleError::Io(enrich_fs_error(path, "inspecting", error))),
    }
}

fn set_file_mode(path: &Path, executable: bool) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        // `OpenOptions::mode(0o644)` is still filtered by the
        // process umask, and reflink/copy paths preserve the source
        // mode. Normalize the worktree-visible file mode here so
        // materialized checkouts do not inherit a restrictive object
        // store mode such as `0o600`.
        let mode = if executable { 0o755 } else { 0o644 };
        fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
    }
    #[cfg(not(unix))]
    {
        let _ = (path, executable);
    }
    Ok(())
}

fn materialization_worker_count(
    operation_count: usize,
    requested_threads: Option<NonZeroUsize>,
) -> usize {
    if operation_count < MATERIALIZE_PARALLEL_THRESHOLD {
        return 1;
    }

    let available = requested_threads.unwrap_or_else(default_materialization_threads);
    available.get().min(operation_count.max(1))
}

fn default_materialization_threads() -> NonZeroUsize {
    std::thread::available_parallelism().unwrap_or(NonZeroUsize::MIN)
}

fn requested_materialization_threads() -> Option<NonZeroUsize> {
    let raw = std::env::var(MATERIALIZE_THREADS_ENV).ok()?;
    raw.trim().parse::<usize>().ok().and_then(NonZeroUsize::new)
}

#[cfg(test)]
mod tests {
    use std::{num::NonZeroUsize, path::PathBuf};

    use objects::store::ObjectStore;
    use objects::{fs_clone::filesystem_supports_reflink, object::Blob};
    use tempfile::TempDir;

    use super::{
        Repository, WorktreeWriteOp, materialization_worker_count, remove_materialized_leaf,
    };

    /// Regression: `remove_materialized_leaf` must tolerate `ENOTEMPTY` on
    /// the directory branch, mirroring `remove_existing_path` in the
    /// incremental apply path. Both tolerances are needed because the
    /// apply planner only removes tracked descendants — when the planner asks
    /// the materializer to clear a directory whose tracked children are gone
    /// but whose untracked or explicitly ignored children remain, `remove_dir` errors
    /// with `os error 66` (macOS/BSD) / `39` (Linux). Pre-fix the
    /// materialization branch propagated that error and aborted apply
    /// mid-walk, leaving HEAD stuck and disk diverged from state.
    #[test]
    fn remove_materialized_leaf_tolerates_directory_not_empty() {
        let temp = TempDir::new().unwrap();
        let dir = temp.path().join("web");
        std::fs::create_dir_all(dir.join("node_modules/lodash")).unwrap();
        std::fs::write(dir.join("node_modules/lodash/index.js"), "ignored").unwrap();

        // Pre-fix this would propagate ENOTEMPTY; post-fix it returns Ok
        // and leaves the directory (with its ignored content) on disk.
        remove_materialized_leaf(&dir).expect("must tolerate ENOTEMPTY");
        assert!(
            dir.join("node_modules/lodash/index.js").exists(),
            "ignored content must survive the tolerated removal"
        );
    }

    /// Regression: empty directories still get cleaned up (the common
    /// case). The `ENOTEMPTY` tolerance must not regress the happy path.
    #[test]
    fn remove_materialized_leaf_removes_empty_directory() {
        let temp = TempDir::new().unwrap();
        let dir = temp.path().join("emptydir");
        std::fs::create_dir(&dir).unwrap();

        remove_materialized_leaf(&dir).expect("must remove empty dir");
        assert!(!dir.exists(), "empty directory must be removed");
    }

    /// Regression: missing paths are a no-op (NotFound), not an error.
    #[test]
    fn remove_materialized_leaf_is_noop_for_missing_path() {
        let temp = TempDir::new().unwrap();
        remove_materialized_leaf(&temp.path().join("does-not-exist"))
            .expect("missing path must be a no-op");
    }

    /// Regression: regular files are still removed (the common symlink-
    /// replacement case where the existing leaf was a tracked file).
    #[test]
    fn remove_materialized_leaf_removes_regular_file() {
        let temp = TempDir::new().unwrap();
        let file = temp.path().join("a.txt");
        std::fs::write(&file, "content").unwrap();

        remove_materialized_leaf(&file).expect("must remove regular file");
        assert!(!file.exists(), "regular file must be removed");
    }

    #[test]
    fn materialization_parallelism_stays_sequential_for_small_workloads() {
        assert_eq!(materialization_worker_count(31, Some(NonZeroUsize::MIN)), 1);
    }

    #[test]
    fn materialization_parallelism_respects_requested_thread_cap() {
        assert_eq!(materialization_worker_count(128, NonZeroUsize::new(4)), 4);
    }

    #[test]
    fn materialize_write_ops_prepares_missing_parent_directories() {
        let temp_dir = TempDir::new().unwrap();
        let repo = Repository::init_default(temp_dir.path()).unwrap();

        let blob = Blob::from("cold pull payload");
        let hash = repo.store().put_blob(&blob).unwrap();
        let file_path = temp_dir.path().join("nested/deep/file.txt");

        repo.materialize_write_ops(&[WorktreeWriteOp::Blob {
            path: file_path.clone(),
            hash,
            executable: false,
        }])
        .unwrap();

        assert_eq!(
            std::fs::read_to_string(&file_path).unwrap(),
            "cold pull payload"
        );
    }

    /// Materialized blobs must be writable by default. The
    /// previous hardlink+chmod-0o444 approach was a footgun:
    /// `chmod 644` then in-place write would mutate the canonical
    /// store inode, corrupting every other worktree. The fix is
    /// filesystem-level CoW (or full copy), so each worktree gets
    /// its own inode and a normal `0o644`/`0o755` mode.
    #[test]
    #[cfg(unix)]
    fn materialized_blob_uses_normal_writable_mode() {
        use std::os::unix::fs::PermissionsExt;

        let temp_dir = TempDir::new().unwrap();
        let repo = Repository::init_default(temp_dir.path()).unwrap();

        let blob = Blob::from("normal mode payload");
        let hash = repo.store().put_blob(&blob).unwrap();
        let regular = temp_dir.path().join("worktree/file.txt");
        let exec = temp_dir.path().join("worktree/run.sh");

        repo.materialize_write_ops(&[
            WorktreeWriteOp::Blob {
                path: regular.clone(),
                hash,
                executable: false,
            },
            WorktreeWriteOp::Blob {
                path: exec.clone(),
                hash,
                executable: true,
            },
        ])
        .unwrap();

        let regular_mode = std::fs::metadata(&regular).unwrap().permissions().mode() & 0o777;
        let exec_mode = std::fs::metadata(&exec).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            regular_mode, 0o644,
            "regular blob must be 0o644 (got 0o{:o})",
            regular_mode
        );
        assert_eq!(
            exec_mode, 0o755,
            "executable blob must be 0o755 (got 0o{:o})",
            exec_mode
        );

        // Sanity: a plain in-place write on the materialized file
        // must succeed (no chmod gymnastics required).
        std::fs::write(&regular, b"agent edits this").unwrap();
        assert_eq!(std::fs::read(&regular).unwrap(), b"agent edits this");
    }

    /// THE core isolation property. An agent in worktree-A that
    /// chmods +w (no-op since we already ship 0o644) and writes
    /// in-place must not affect worktree-B's bytes. Under the old
    /// hardlink+chmod model this exact sequence corrupted sibling
    /// worktrees through the shared inode. Under the new
    /// CoW/copy model the worktrees have distinct inodes and the
    /// kernel guarantees isolation.
    #[test]
    #[cfg(unix)]
    fn materialize_then_chmod_and_write_does_not_affect_sibling_worktree() {
        use std::os::unix::fs::PermissionsExt;

        let temp_dir = TempDir::new().unwrap();
        let repo = Repository::init_default(temp_dir.path()).unwrap();

        let blob = Blob::from("canonical bytes that must never change");
        let hash = repo.store().put_blob(&blob).unwrap();

        let worktree_a = temp_dir.path().join("wt-a/file.txt");
        let worktree_b = temp_dir.path().join("wt-b/file.txt");

        repo.materialize_write_ops(&[WorktreeWriteOp::Blob {
            path: worktree_a.clone(),
            hash,
            executable: false,
        }])
        .unwrap();
        repo.materialize_write_ops(&[WorktreeWriteOp::Blob {
            path: worktree_b.clone(),
            hash,
            executable: false,
        }])
        .unwrap();

        // Simulate a misbehaving agent: re-assert mode 0o644 (the
        // old defense rendered this a no-op for blocking writes),
        // then truncate-and-overwrite in place via the shell-style
        // `> file` pathway.
        std::fs::set_permissions(&worktree_a, std::fs::Permissions::from_mode(0o644)).unwrap();
        std::fs::write(&worktree_a, b"AGENT_TAMPERED_WITH_WORKTREE_A").unwrap();

        // Sibling worktree's bytes are unchanged.
        assert_eq!(
            std::fs::read(&worktree_b).unwrap(),
            blob.content(),
            "sibling worktree must keep canonical bytes despite in-place write to worktree-a"
        );
        // And the canonical loose blob in the store is untouched.
        if let Some(loose) = repo.store().loose_blob_path(&hash) {
            assert_eq!(
                std::fs::read(&loose).unwrap(),
                blob.content(),
                "canonical loose blob must keep canonical bytes despite in-place write to worktree-a"
            );
        }
    }

    /// Atomic-rename writes (write-tempfile + `rename(2)` over
    /// target) must also leave sibling worktrees untouched. This
    /// path was always safe under the old model too — proving it
    /// keeps working with the new isolation strategy.
    #[test]
    #[cfg(unix)]
    fn materialize_atomic_rename_does_not_affect_sibling_worktree() {
        let temp_dir = TempDir::new().unwrap();
        let repo = Repository::init_default(temp_dir.path()).unwrap();

        let blob = Blob::from("atomic-rename canonical bytes");
        let hash = repo.store().put_blob(&blob).unwrap();

        let worktree_a = temp_dir.path().join("wt-a/file.txt");
        let worktree_b = temp_dir.path().join("wt-b/file.txt");

        repo.materialize_write_ops(&[WorktreeWriteOp::Blob {
            path: worktree_a.clone(),
            hash,
            executable: false,
        }])
        .unwrap();
        repo.materialize_write_ops(&[WorktreeWriteOp::Blob {
            path: worktree_b.clone(),
            hash,
            executable: false,
        }])
        .unwrap();

        let tmp = temp_dir.path().join("wt-a/file.txt.tmp");
        std::fs::write(&tmp, b"NEW_CONTENT_VIA_ATOMIC_RENAME").unwrap();
        std::fs::rename(&tmp, &worktree_a).unwrap();

        assert_eq!(
            std::fs::read(&worktree_a).unwrap(),
            b"NEW_CONTENT_VIA_ATOMIC_RENAME"
        );
        assert_eq!(
            std::fs::read(&worktree_b).unwrap(),
            blob.content(),
            "sibling worktree must keep canonical bytes despite atomic rename in worktree-a"
        );
    }

    /// On a CoW filesystem (APFS, btrfs, XFS-with-reflinks, ZFS)
    /// the materialized worktree file must have a **distinct**
    /// inode from the canonical loose blob. This is the key
    /// correctness assertion that distinguishes reflinks from
    /// hardlinks: hardlinks share inodes (the bug we fixed),
    /// reflinks do not.
    ///
    /// On non-CoW filesystems the test soft-skips — `fs::copy`
    /// also gives distinct inodes, but the test is targeted at
    /// the reflink path specifically.
    #[test]
    #[cfg(unix)]
    fn materialize_uses_reflink_when_filesystem_supports_it() {
        use std::os::unix::fs::MetadataExt;

        let temp_dir = TempDir::new().unwrap();
        if !filesystem_supports_reflink(temp_dir.path()) {
            eprintln!(
                "[skip] filesystem at {:?} does not advertise reflink support",
                temp_dir.path()
            );
            return;
        }

        let repo = Repository::init_default(temp_dir.path()).unwrap();
        let blob = Blob::from("reflink correctness check, kept under compression threshold");
        let hash = repo.store().put_blob(&blob).unwrap();
        let worktree = temp_dir.path().join("wt/file.txt");

        repo.materialize_write_ops(&[WorktreeWriteOp::Blob {
            path: worktree.clone(),
            hash,
            executable: false,
        }])
        .unwrap();

        let loose = repo
            .store()
            .loose_blob_path(&hash)
            .expect("blob must be loose+uncompressed (under threshold)");
        let loose_inode = std::fs::metadata(&loose).unwrap().ino();
        let worktree_inode = std::fs::metadata(&worktree).unwrap().ino();
        assert_ne!(
            loose_inode, worktree_inode,
            "reflinked worktree file must have a distinct inode from canonical loose blob (got {} for both — that's a hardlink, the bug we fixed)",
            loose_inode
        );
        // And nlink on the canonical blob is 1: nothing aliases it.
        let nlink = std::fs::metadata(&loose).unwrap().nlink();
        assert_eq!(
            nlink, 1,
            "canonical loose blob must not be aliased (nlink={}); reflinks share blocks, not inodes",
            nlink
        );
    }

    /// Functional readback after N materializations of the same
    /// blob across N worktrees on the same filesystem. Replaces
    /// the old "shared inode" assertion which is no longer the
    /// correctness model. Now we just assert every worktree reads
    /// back the canonical bytes (and they're independent — see
    /// the isolation tests above).
    #[test]
    #[cfg(unix)]
    fn materialize_blob_into_two_worktrees_reads_back_canonical_bytes() {
        let temp_dir = TempDir::new().unwrap();
        let repo = Repository::init_default(temp_dir.path()).unwrap();

        let blob = Blob::from("two-worktree readback payload");
        let hash = repo.store().put_blob(&blob).unwrap();

        let worktree_a = temp_dir.path().join("worktree-a/file.txt");
        let worktree_b = temp_dir.path().join("worktree-b/file.txt");

        repo.materialize_write_ops(&[WorktreeWriteOp::Blob {
            path: worktree_a.clone(),
            hash,
            executable: false,
        }])
        .unwrap();
        repo.materialize_write_ops(&[WorktreeWriteOp::Blob {
            path: worktree_b.clone(),
            hash,
            executable: false,
        }])
        .unwrap();

        assert_eq!(std::fs::read(&worktree_a).unwrap(), blob.content());
        assert_eq!(std::fs::read(&worktree_b).unwrap(), blob.content());
    }

    /// Symlinks are routed through the existing path; introducing
    /// hardlinks must not regress the symlink test that lives in
    /// `repository_tests.rs`. Locally we just confirm a symlink op
    /// still produces a real symlink (not a hardlink to the target
    /// blob's loose path).
    #[test]
    #[cfg(unix)]
    fn materialize_symlink_op_produces_real_symlink_not_hardlink() {
        let temp_dir = TempDir::new().unwrap();
        let repo = Repository::init_default(temp_dir.path()).unwrap();

        let symlink_blob = Blob::new(b"../canonical".to_vec());
        let symlink_hash = repo.store().put_blob(&symlink_blob).unwrap();
        let path = temp_dir.path().join("worktree/link.txt");

        repo.materialize_write_ops(&[WorktreeWriteOp::Symlink {
            path: path.clone(),
            hash: symlink_hash,
        }])
        .unwrap();

        let meta = std::fs::symlink_metadata(&path).unwrap();
        assert!(
            meta.file_type().is_symlink(),
            "Symlink op must produce a real symlink, not a hardlinked regular file"
        );
        assert_eq!(
            std::fs::read_link(&path).unwrap(),
            PathBuf::from("../canonical")
        );
    }

    #[test]
    #[cfg(unix)]
    fn materialize_symlink_op_replaces_existing_symlink() {
        let temp_dir = TempDir::new().unwrap();
        let repo = Repository::init_default(temp_dir.path()).unwrap();

        let first_hash = repo.store().put_blob(&Blob::from("first")).unwrap();
        let second_hash = repo.store().put_blob(&Blob::from("second")).unwrap();
        let path = temp_dir.path().join("worktree/link.txt");

        repo.materialize_write_ops(&[WorktreeWriteOp::Symlink {
            path: path.clone(),
            hash: first_hash,
        }])
        .unwrap();
        repo.materialize_write_ops(&[WorktreeWriteOp::Symlink {
            path: path.clone(),
            hash: second_hash,
        }])
        .unwrap();

        assert_eq!(std::fs::read_link(&path).unwrap(), PathBuf::from("second"));
    }

    #[test]
    #[cfg(unix)]
    fn materialize_write_ops_reuses_prepared_parent_for_multiple_writes() {
        let temp_dir = TempDir::new().unwrap();
        let repo = Repository::init_default(temp_dir.path()).unwrap();

        let symlink_target = Blob::new(b"../target.txt".to_vec());
        let target_hash = repo.store().put_blob(&Blob::from("target")).unwrap();
        let symlink_hash = repo.store().put_blob(&symlink_target).unwrap();
        let base_dir = temp_dir.path().join("nested/deep");
        let target_path = base_dir.join("target.txt");
        let link_path = base_dir.join("link.txt");

        repo.materialize_write_ops(&[
            WorktreeWriteOp::Blob {
                path: target_path.clone(),
                hash: target_hash,
                executable: false,
            },
            WorktreeWriteOp::Symlink {
                path: link_path.clone(),
                hash: symlink_hash,
            },
        ])
        .unwrap();

        assert_eq!(std::fs::read_to_string(&target_path).unwrap(), "target");
        assert_eq!(
            std::fs::read_link(&link_path).unwrap(),
            PathBuf::from("../target.txt")
        );
    }

    /// After `pack_objects + prune_loose_objects`, every blob is
    /// pack-only. The lazy-promotion path inside `materialize_blob`
    /// must (a) succeed without errors, (b) read back the canonical
    /// bytes in both worktrees, and (c) leave a real loose
    /// uncompressed mirror on disk under
    /// `.heddle/objects/blobs/<2-char>/<rest>` so subsequent
    /// reflinks have something to clone from.
    #[test]
    #[cfg(unix)]
    fn lazy_promotion_after_pack_and_prune_restores_loose_mirror() {
        let temp_dir = TempDir::new().unwrap();
        let repo = Repository::init_default(temp_dir.path()).unwrap();

        let blob = Blob::from(
            "lazy-promotion payload, packed-then-pruned, kept under compression threshold",
        );
        let hash = repo.store().put_blob(&blob).unwrap();

        // Move the loose copy into a packfile, then drop the loose
        // copy. The store now has only the pack-resident blob.
        repo.store().pack_objects(false).unwrap();
        repo.store().prune_loose_objects().unwrap();
        assert!(
            repo.store().loose_blob_path(&hash).is_none(),
            "after pack+prune, the canonical loose path must be empty"
        );

        let worktree_a = temp_dir.path().join("worktree-a/file.txt");
        let worktree_b = temp_dir.path().join("worktree-b/file.txt");
        repo.materialize_write_ops(&[WorktreeWriteOp::Blob {
            path: worktree_a.clone(),
            hash,
            executable: false,
        }])
        .unwrap();
        repo.materialize_write_ops(&[WorktreeWriteOp::Blob {
            path: worktree_b.clone(),
            hash,
            executable: false,
        }])
        .unwrap();

        // (a)+(b) read back ok.
        assert_eq!(std::fs::read(&worktree_a).unwrap(), blob.content());
        assert_eq!(std::fs::read(&worktree_b).unwrap(), blob.content());

        // (c) the loose-uncompressed mirror exists.
        let loose = repo
            .store()
            .loose_blob_path(&hash)
            .expect("after lazy promotion the canonical loose path must exist");
        assert_eq!(std::fs::read(&loose).unwrap(), blob.content());
    }

    /// Proactive warm: walk a state's tree, promote every reachable
    /// blob, then materialize. Every blob must be loose-uncompressed
    /// after warm so the materialize step can reflink directly
    /// without paying the decompress tax. Cross-worktree readback
    /// must give the canonical bytes.
    #[test]
    #[cfg(unix)]
    fn proactive_warm_promotes_all_state_blobs() {
        let temp_dir = TempDir::new().unwrap();
        let repo = Repository::init_default(temp_dir.path()).unwrap();

        // Materialize a few files and snapshot.
        for i in 0..4 {
            std::fs::write(
                temp_dir.path().join(format!("file-{i}.txt")),
                format!("warm-pass payload {i} {}", "x".repeat(140)),
            )
            .unwrap();
        }
        let state = repo
            .snapshot(Some("warm-pass test".to_string()), None)
            .unwrap();

        // Pack + prune so every blob is pack-only.
        repo.store().pack_objects(false).unwrap();
        repo.store().prune_loose_objects().unwrap();

        // Sanity: with a packed-then-pruned store, no canonical loose
        // file exists yet for the snapshot's blobs.
        let tree = repo.store().get_tree(&state.tree).unwrap().unwrap();
        let mut hashes = std::collections::BTreeSet::new();
        repo.collect_blob_hashes(&tree, &mut hashes).unwrap();
        for hash in &hashes {
            assert!(
                repo.store().loose_blob_path(hash).is_none(),
                "blob {} should be pack-only before warm",
                hash
            );
        }

        // Warm: every blob should now be loose-uncompressed.
        let stats = repo
            .warm_canonical_store_for_state(&state.change_id)
            .unwrap();
        assert_eq!(stats.errors, 0, "warm pass produced errors: {:?}", stats);
        assert_eq!(stats.total(), hashes.len());
        assert!(
            stats.promoted >= hashes.len(),
            "expected to promote all {} blobs, got {} (already_loose={})",
            hashes.len(),
            stats.promoted,
            stats.already_loose
        );
        for hash in &hashes {
            assert!(
                repo.store().loose_blob_path(hash).is_some(),
                "blob {} should be loose+uncompressed after warm",
                hash
            );
        }

        // Materialize across two worktrees on the same FS. Reading
        // back from each must yield the canonical bytes; isolation
        // is guaranteed by filesystem-level CoW (or full copy).
        let worktree_a = temp_dir.path().join("wt-a");
        let worktree_b = temp_dir.path().join("wt-b");
        repo.materialize_tree(&tree, &worktree_a).unwrap();
        repo.materialize_tree(&tree, &worktree_b).unwrap();

        for entry in tree.entries() {
            let path_a = worktree_a.join(&entry.name);
            let path_b = worktree_b.join(&entry.name);
            assert_eq!(
                std::fs::read(&path_a).unwrap(),
                std::fs::read(&path_b).unwrap(),
                "{} must read back identically across worktrees",
                entry.name
            );
        }
    }

    /// Idempotent warm: a second pass over the same state must not
    /// rewrite anything. Every blob is `already_loose`.
    #[test]
    #[cfg(unix)]
    fn warm_canonical_store_is_idempotent() {
        let temp_dir = TempDir::new().unwrap();
        let repo = Repository::init_default(temp_dir.path()).unwrap();

        for i in 0..3 {
            std::fs::write(
                temp_dir.path().join(format!("idem-{i}.txt")),
                format!("idem payload {i} {}", "x".repeat(160)),
            )
            .unwrap();
        }
        let state = repo
            .snapshot(Some("idempotent warm".to_string()), None)
            .unwrap();
        repo.store().pack_objects(false).unwrap();
        repo.store().prune_loose_objects().unwrap();

        let first = repo
            .warm_canonical_store_for_state(&state.change_id)
            .unwrap();
        let second = repo
            .warm_canonical_store_for_state(&state.change_id)
            .unwrap();

        assert_eq!(first.total(), second.total(), "blob count must be stable");
        assert_eq!(
            second.promoted, 0,
            "second warm must not promote anything (got {})",
            second.promoted
        );
        assert_eq!(
            second.already_loose,
            second.total(),
            "every blob must be already_loose on second pass"
        );
        assert_eq!(second.errors, 0);
    }

    /// Storage win after warm + materialize on a CoW filesystem.
    /// We can no longer dedupe via inode (reflinks have distinct
    /// inodes by design), so on CoW filesystems we instead assert
    /// that **every materialized file has its own inode**, distinct
    /// from the canonical loose blob — proving the materializer
    /// took the reflink path (which gives the storage win on CoW
    /// without aliasing) rather than the in-memory `fs::write` path
    /// (which costs full duplicates).
    ///
    /// On non-CoW filesystems the test soft-skips. The materializer
    /// will use `fs::copy` and the storage win is not recoverable
    /// without reflink support.
    #[test]
    #[cfg(unix)]
    fn packed_repo_storage_win_after_warm_and_materialize() {
        use std::{collections::HashSet, os::unix::fs::MetadataExt};

        let temp_dir = TempDir::new().unwrap();
        if !filesystem_supports_reflink(temp_dir.path()) {
            eprintln!(
                "[skip] filesystem at {:?} does not support reflinks; storage-win test is reflink-specific",
                temp_dir.path()
            );
            return;
        }

        let repo = Repository::init_default(temp_dir.path()).unwrap();

        let blob_count = 5;
        for i in 0..blob_count {
            std::fs::write(
                temp_dir.path().join(format!("file-{i}.txt")),
                format!("packed-storage-win payload {i} {}", "x".repeat(140 + i * 8)),
            )
            .unwrap();
        }
        let state = repo
            .snapshot(Some("packed storage win".to_string()), None)
            .unwrap();
        // Realistic steady state.
        repo.store().pack_objects(false).unwrap();
        repo.store().prune_loose_objects().unwrap();

        // Warm so the first materialize doesn't pay decompress cost.
        let stats = repo
            .warm_canonical_store_for_state(&state.change_id)
            .unwrap();
        assert_eq!(stats.errors, 0);

        let n_worktrees = 6;
        let tree = repo.store().get_tree(&state.tree).unwrap().unwrap();
        let mut all_paths = Vec::new();
        for w in 0..n_worktrees {
            let worktree = temp_dir.path().join(format!("wt-{w}"));
            repo.materialize_tree(&tree, &worktree).unwrap();
            for i in 0..blob_count {
                all_paths.push(worktree.join(format!("file-{i}.txt")));
            }
        }

        // Every materialized file has its own inode (reflinks, not
        // hardlinks). Total inodes = files materialized.
        let mut inodes = HashSet::new();
        for path in &all_paths {
            inodes.insert(std::fs::metadata(path).unwrap().ino());
        }
        assert_eq!(
            inodes.len(),
            all_paths.len(),
            "every reflinked worktree file must have its own inode (got {} for {} files)",
            inodes.len(),
            all_paths.len()
        );

        // No materialized file shares an inode with the canonical
        // loose blob — that would be the hardlink bug.
        let mut canonical_inodes = HashSet::new();
        for hash in tree.entries().iter().map(|e| &e.hash) {
            if let Some(loose) = repo.store().loose_blob_path(hash) {
                canonical_inodes.insert(std::fs::metadata(&loose).unwrap().ino());
            }
        }
        for inode in &inodes {
            assert!(
                !canonical_inodes.contains(inode),
                "worktree file inode {} aliases the canonical loose blob — that's the hardlink bug",
                inode
            );
        }

        eprintln!(
            "[packed-storage-win] n_worktrees={} blobs/tree={} reflink_path_confirmed=true",
            n_worktrees, blob_count
        );
    }

    /// `promote_to_loose_uncompressed` is idempotent for an already
    /// loose+uncompressed blob — fast-path returns `Ok(false)` so a
    /// caller can distinguish "no work needed" from "promoted".
    #[test]
    fn promote_to_loose_uncompressed_idempotent_on_loose_blob() {
        let temp_dir = TempDir::new().unwrap();
        let repo = Repository::init_default(temp_dir.path()).unwrap();

        let blob = Blob::from("idempotent promote payload");
        let hash = repo.store().put_blob(&blob).unwrap();
        // Already loose+uncompressed (under compression threshold).
        assert!(repo.store().loose_blob_path(&hash).is_some());

        let did_work = repo.store().promote_to_loose_uncompressed(&hash).unwrap();
        assert!(
            !did_work,
            "promote on already-loose+uncompressed blob must be a no-op"
        );
    }

    /// `promote_to_loose_uncompressed` on a missing blob bubbles a
    /// `NotFound`, not a silent success. Callers can degrade
    /// gracefully (e.g. lazy-path falls back to `fs::write`), but
    /// the failure must not be invisible.
    #[test]
    fn promote_to_loose_uncompressed_returns_error_for_missing_blob() {
        use objects::object::ContentHash;

        let temp_dir = TempDir::new().unwrap();
        let repo = Repository::init_default(temp_dir.path()).unwrap();

        let bogus = ContentHash::compute_typed("blob", b"never-stored");
        let result = repo.store().promote_to_loose_uncompressed(&bogus);
        assert!(
            result.is_err(),
            "promote on missing blob must error, got {:?}",
            result
        );
    }
}
