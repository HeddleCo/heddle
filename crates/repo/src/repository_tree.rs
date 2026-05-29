// SPDX-License-Identifier: Apache-2.0
//! Tree building and materialization helpers.

use objects::store::ObjectStore;
use std::{collections::HashSet, fs, path::Path, time::Instant};

use objects::{
    object::{Blob, ContentHash, Tree, TreeEntry},
    worktree::WorktreeStatus,
};
use tracing::{debug, instrument, trace, warn};

use super::{
    HeddleError, Repository, Result,
    repository_worktree_status::{WorktreeStatusDetailed, compare_worktree_with_index_detailed},
};
use crate::{
    FsMonitorSettings, WorktreeIndex, WorktreeStatusOptions,
    fsmonitor::ChangeMonitorSession,
    worktree_ignore::WorktreeIgnoreMatcher,
    worktree_index::{WorktreeIndexLoadStats, WorktreeIndexSaveStats},
    worktree_walk::{
        WalkDirectory, WalkEntry, WorktreeWalkPolicy, read_blob_with_hash, validate_symlink_target,
        walk_worktree,
    },
};

#[derive(Debug, Clone, Default)]
pub struct WorktreeCompareProfile {
    pub index_load_ms: u128,
    pub index_snapshot_load_ms: u128,
    pub index_journal_replay_ms: u128,
    pub index_snapshot_bytes: u64,
    pub index_journal_bytes: u64,
    pub index_journal_ops: usize,
    pub monitor_prepare_ms: u128,
    pub compare_ms: u128,
    pub index_save_ms: u128,
    pub index_snapshot_write_ms: u128,
    pub index_journal_append_ms: u128,
    pub index_save_snapshot_bytes: u64,
    pub index_save_journal_bytes: u64,
    pub index_save_journal_ops: usize,
    pub index_save_compacted: bool,
    pub monitor_persist_ms: u128,
    pub untracked_flatten_ms: u128,
    pub untracked_flattened_paths: usize,
    pub tracked_refresh_ms: u128,
    pub untracked_scan_ms: u128,
    pub hashing_ms: u128,
    pub directory_cache_compare_ms: u128,
    pub directories_scanned: u64,
    pub directories_skipped: u64,
    pub files_hashed: u64,
    pub cache_hits: u64,
    pub monitor_changed_paths: u64,
    pub monitor_skipped_directories: u64,
}

#[derive(Debug, Clone, Default)]
pub struct TreeBuildProfile {
    pub tree_walk_ms: u128,
    pub blob_prep_ms: u128,
    pub blob_write_ms: u128,
    pub tree_write_ms: u128,
    pub file_count: usize,
    pub dir_count: usize,
}

#[derive(Debug, Clone)]
struct TreeBuildOutput {
    tree: Tree,
    profile: TreeBuildProfile,
}

impl Repository {
    /// Build a tree from a directory.
    #[instrument(skip(self), fields(dir = %dir.display()))]
    pub fn build_tree(&self, dir: &Path) -> Result<Tree> {
        self.build_tree_profiled(dir).map(|(tree, _)| tree)
    }

    /// Build a tree from a directory, reusing per-file hashes from a
    /// thread manifest when the on-disk `(inode, mtime, ctime, mode)`
    /// still matches the recorded snapshot.
    ///
    /// Same output as [`Self::build_tree`] — a complete `Tree` object —
    /// but files whose stat fields match the cache skip the
    /// `read + hash + put_blob` cycle entirely. Net effect on
    /// `capture_thread_from_disk` for a single-file edit on a 643-file
    /// fixture: blob work drops from ~30 MB of reads to ~one file's
    /// worth. Wall-clock follows.
    ///
    /// Safe-by-default: any uncertainty (entry missing from cache,
    /// stat mismatch) falls back to the full read path for that
    /// specific file. Other files in the same tree still benefit.
    pub fn build_tree_with_stat_cache(
        &self,
        dir: &Path,
        manifest: &crate::thread_manifest::ThreadManifest,
    ) -> Result<Tree> {
        self.build_tree_profiled_inner(dir, Some(manifest))
            .map(|(tree, _)| tree)
    }

    #[instrument(skip(self), fields(dir = %dir.display()))]
    pub fn build_tree_profiled(&self, dir: &Path) -> Result<(Tree, TreeBuildProfile)> {
        self.build_tree_profiled_inner(dir, None)
    }

    /// Profiled tree-build that reuses a manifest's stat-cache. Same
    /// contract as [`Self::build_tree_profiled`] — returns the full
    /// `(Tree, TreeBuildProfile)` for downstream timing — but skips
    /// the `read + hash + put_blob` cycle for files whose stat fields
    /// match the cache. The fall-through path for changed/new files
    /// is identical, so the resulting tree is byte-identical to what
    /// the un-cached build would produce.
    #[instrument(skip(self, manifest), fields(dir = %dir.display()))]
    pub fn build_tree_profiled_with_stat_cache(
        &self,
        dir: &Path,
        manifest: &crate::thread_manifest::ThreadManifest,
    ) -> Result<(Tree, TreeBuildProfile)> {
        self.build_tree_profiled_inner(dir, Some(manifest))
    }

    fn build_tree_profiled_inner(
        &self,
        dir: &Path,
        stat_cache: Option<&crate::thread_manifest::ThreadManifest>,
    ) -> Result<(Tree, TreeBuildProfile)> {
        let patterns = self.ignore_patterns()?;
        debug!(pattern_count = patterns.len(), "Starting tree build");
        let start = Instant::now();
        let nested_exclusions = self.nested_thread_worktree_exclusions(dir)?;
        let tree = self.build_tree_walk(dir, &patterns, nested_exclusions, stat_cache);
        let elapsed = start.elapsed().as_millis();
        debug!(duration_ms = elapsed, "Tree build complete");
        tree.map(|output| {
            let mut profile = output.profile;
            profile.tree_walk_ms = elapsed;
            (output.tree, profile)
        })
    }

    #[instrument(skip(self, patterns, nested_exclusions, stat_cache), fields(dir = %dir.display()))]
    fn build_tree_walk(
        &self,
        dir: &Path,
        patterns: &[String],
        nested_exclusions: Vec<std::path::PathBuf>,
        stat_cache: Option<&crate::thread_manifest::ThreadManifest>,
    ) -> Result<TreeBuildOutput> {
        let ignore_matcher =
            WorktreeIgnoreMatcher::new(patterns).with_nested_worktree_exclusions(nested_exclusions);
        let mut policy = TreeBuildPolicy::new(self, dir, stat_cache);
        let mut output = walk_worktree(self, dir, &ignore_matcher, None, &mut policy)?;

        // Flush every newly-seen blob as a single packfile. Stores
        // that don't override `put_blobs_packed` fall back to per-blob
        // writes (correct, just slower). Time is folded into
        // `blob_write_ms` so the existing perf profile keeps tracking
        // total blob-storage cost.
        if !policy.pending_blobs.is_empty() {
            let flush_start = Instant::now();
            let pending = std::mem::take(&mut policy.pending_blobs);
            self.store.put_blobs_packed(pending)?;
            output.profile.blob_write_ms += flush_start.elapsed().as_millis();
        }

        Ok(output)
    }

    /// Compare the worktree against a tree using the persisted binary index.
    pub fn compare_worktree_cached(&self, tree: &Tree) -> Result<WorktreeStatus> {
        self.compare_worktree_cached_with_options(tree, &self.default_worktree_status_options())
    }

    pub fn compare_worktree_cached_detailed(&self, tree: &Tree) -> Result<WorktreeStatusDetailed> {
        self.compare_worktree_cached_detailed_with_options(
            tree,
            &self.default_worktree_status_options(),
        )
    }

    /// Compare the worktree against a tree using the persisted binary index.
    pub fn compare_worktree_cached_with_options(
        &self,
        tree: &Tree,
        options: &WorktreeStatusOptions,
    ) -> Result<WorktreeStatus> {
        self.compare_worktree_cached_profiled_with_options(tree, options)
            .map(|(status, _)| status)
    }

    pub fn compare_worktree_cached_detailed_with_options(
        &self,
        tree: &Tree,
        options: &WorktreeStatusOptions,
    ) -> Result<WorktreeStatusDetailed> {
        self.compare_worktree_cached_detailed_profiled_with_options(tree, options)
            .map(|(status, _)| status)
    }

    pub fn compare_worktree_cached_profiled_with_options(
        &self,
        tree: &Tree,
        options: &WorktreeStatusOptions,
    ) -> Result<(WorktreeStatus, WorktreeCompareProfile)> {
        let (detailed_status, mut profile) =
            self.compare_worktree_cached_detailed_profiled_with_options(tree, options)?;
        let flatten_start = Instant::now();
        let flattened_paths = detailed_status.untracked.flattened_path_count();
        let mut status = detailed_status.into_flat_status();
        profile.untracked_flatten_ms = flatten_start.elapsed().as_millis();
        profile.untracked_flattened_paths = flattened_paths;
        status.modified.sort();
        status.added.sort();
        status.deleted.sort();
        Ok((status, profile))
    }

    pub fn compare_worktree_cached_detailed_profiled_with_options(
        &self,
        tree: &Tree,
        options: &WorktreeStatusOptions,
    ) -> Result<(WorktreeStatusDetailed, WorktreeCompareProfile)> {
        let index_path = self.worktree_index_path();
        let load_start = Instant::now();
        let (mut index, load_stats) = match WorktreeIndex::load_profiled(&index_path) {
            Ok(result) => result,
            Err(error) => {
                warn!(path = %index_path.display(), %error, "Ignoring unreadable worktree index");
                (WorktreeIndex::new(), WorktreeIndexLoadStats::default())
            }
        };
        let index_load_ms = load_start.elapsed().as_millis();

        let monitor_prepare_start = Instant::now();
        let monitor = ChangeMonitorSession::prepare(self.root(), options.fsmonitor);
        let monitor_prepare_ms = monitor_prepare_start.elapsed().as_millis();

        let patterns = self.ignore_patterns()?;
        let nested_exclusions = self.nested_thread_worktree_exclusions(self.root())?;
        let ignore_matcher = WorktreeIgnoreMatcher::new(&patterns)
            .with_nested_worktree_exclusions(nested_exclusions);
        let compare_start = Instant::now();
        let (status, stats) = compare_worktree_with_index_detailed(
            self,
            tree,
            &ignore_matcher,
            &mut index,
            &monitor,
        )?;
        let compare_ms = compare_start.elapsed().as_millis();

        let save_start = Instant::now();
        let (index_save_ms, save_stats) = if index.is_dirty() {
            match index.save_profiled(&index_path) {
                Ok(stats) => {
                    index.mark_clean();
                    (save_start.elapsed().as_millis(), stats)
                }
                Err(error) => {
                    warn!(path = %index_path.display(), %error, "Failed to persist worktree index");
                    (0, WorktreeIndexSaveStats::default())
                }
            }
        } else {
            (0, WorktreeIndexSaveStats::default())
        };

        let persist_start = Instant::now();
        if let Err(error) = monitor.persist() {
            warn!(path = %self.root().display(), %error, "Failed to persist monitor state");
        }
        let monitor_persist_ms = persist_start.elapsed().as_millis();

        debug!(
            index_load_ms,
            index_snapshot_load_ms = load_stats.snapshot_load_ms,
            index_journal_replay_ms = load_stats.journal_replay_ms,
            index_snapshot_bytes = load_stats.snapshot_bytes,
            index_journal_bytes = load_stats.journal_bytes,
            index_journal_ops = load_stats.journal_ops,
            monitor_prepare_ms,
            compare_ms,
            index_save_ms,
            index_snapshot_write_ms = save_stats.snapshot_write_ms,
            index_journal_append_ms = save_stats.journal_append_ms,
            index_save_snapshot_bytes = save_stats.snapshot_bytes,
            index_save_journal_bytes = save_stats.journal_bytes,
            index_save_journal_ops = save_stats.journal_ops,
            index_save_compacted = save_stats.compacted,
            index_save_compact_reason = save_stats.compact_reason.unwrap_or("none"),
            monitor_persist_ms,
            tracked_refresh_ms = stats.tracked_refresh_ms,
            untracked_scan_ms = stats.untracked_scan_ms,
            untracked_flatten_ms = 0,
            untracked_flattened_paths = 0,
            hashing_ms = stats.hashing_ms,
            directory_cache_compare_ms = stats.directory_cache_compare_ms,
            directories_scanned = stats.directories_scanned,
            directories_skipped = stats.directories_skipped,
            files_hashed = stats.files_hashed,
            cache_hits = stats.cache_hits,
            monitor_backend = monitor.backend.unwrap_or("off"),
            monitor_status = ?monitor.status,
            monitor_reason = monitor.reason.as_deref().unwrap_or("ready"),
            monitor_changed_paths = stats.monitor_changed_paths,
            monitor_skipped_directories = stats.monitor_skipped_directories,
            "Worktree compare complete"
        );

        Ok((
            status,
            WorktreeCompareProfile {
                index_load_ms,
                index_snapshot_load_ms: load_stats.snapshot_load_ms,
                index_journal_replay_ms: load_stats.journal_replay_ms,
                index_snapshot_bytes: load_stats.snapshot_bytes,
                index_journal_bytes: load_stats.journal_bytes,
                index_journal_ops: load_stats.journal_ops,
                monitor_prepare_ms,
                compare_ms,
                index_save_ms,
                index_snapshot_write_ms: save_stats.snapshot_write_ms,
                index_journal_append_ms: save_stats.journal_append_ms,
                index_save_snapshot_bytes: save_stats.snapshot_bytes,
                index_save_journal_bytes: save_stats.journal_bytes,
                index_save_journal_ops: save_stats.journal_ops,
                index_save_compacted: save_stats.compacted,
                monitor_persist_ms,
                untracked_flatten_ms: 0,
                untracked_flattened_paths: 0,
                tracked_refresh_ms: stats.tracked_refresh_ms,
                untracked_scan_ms: stats.untracked_scan_ms,
                hashing_ms: stats.hashing_ms,
                directory_cache_compare_ms: stats.directory_cache_compare_ms,
                directories_scanned: stats.directories_scanned,
                directories_skipped: stats.directories_skipped,
                files_hashed: stats.files_hashed,
                cache_hits: stats.cache_hits,
                monitor_changed_paths: stats.monitor_changed_paths,
                monitor_skipped_directories: stats.monitor_skipped_directories,
            },
        ))
    }

    /// Return whether the worktree matches the provided tree.
    pub fn worktree_is_clean_cached(&self, tree: &Tree) -> Result<bool> {
        self.worktree_is_clean_cached_with_options(tree, &self.default_worktree_status_options())
    }

    /// Return whether the worktree matches the provided tree.
    pub fn worktree_is_clean_cached_with_options(
        &self,
        tree: &Tree,
        options: &WorktreeStatusOptions,
    ) -> Result<bool> {
        Ok(self
            .compare_worktree_cached_detailed_with_options(tree, options)?
            .is_clean())
    }

    fn worktree_index_path(&self) -> std::path::PathBuf {
        self.root.join(".heddle/state").join("index.bin")
    }

    fn default_worktree_status_options(&self) -> WorktreeStatusOptions {
        WorktreeStatusOptions {
            fsmonitor: FsMonitorSettings::from(self.config.worktree.fsmonitor),
        }
    }

    pub fn inspect_change_monitor_with_options(
        &self,
        options: &WorktreeStatusOptions,
    ) -> Result<crate::ChangeMonitorReport> {
        let session = ChangeMonitorSession::prepare(self.root(), options.fsmonitor);
        let report = session.report();
        session.persist()?;
        Ok(report)
    }
}

#[derive(Default)]
struct TreeBuildState {
    entries: Vec<TreeEntry>,
    profile: TreeBuildProfile,
}

struct TreeBuildPolicy<'a> {
    repo: &'a Repository,
    /// Walk root, used to compute paths relative to it so they line
    /// up with manifest keys (`src/foo.rs`, not absolute paths).
    walk_root: &'a Path,
    /// Optional stat-cache. When present, files whose disk stat
    /// `(inode, mtime, ctime, mode)` matches the recorded entry get
    /// their hash reused — no `read + hash + put_blob` cycle. Tracked
    /// in `stat_cache_hits` for diagnostics.
    stat_cache: Option<&'a crate::thread_manifest::ThreadManifest>,
    stat_cache_hits: u64,
    /// Blobs encountered during the walk that aren't already in the
    /// store. Drained once at the end of the walk into a single
    /// packfile via `ObjectStore::put_blobs_packed` — turns N×fsync
    /// per blob into 2×fsync total (the .pack + .idx).
    pending_blobs: Vec<(ContentHash, Vec<u8>)>,
    /// Hashes already queued in `pending_blobs` so we don't double-add
    /// content-equal files (which is common: README.md, .gitkeep, etc).
    seen: HashSet<ContentHash>,
}

impl<'a> TreeBuildPolicy<'a> {
    fn new(
        repo: &'a Repository,
        walk_root: &'a Path,
        stat_cache: Option<&'a crate::thread_manifest::ThreadManifest>,
    ) -> Self {
        Self {
            repo,
            walk_root,
            stat_cache,
            stat_cache_hits: 0,
            pending_blobs: Vec::new(),
            seen: HashSet::new(),
        }
    }

    /// Look up `entry`'s manifest record by relative path and, if
    /// found, compare the on-disk `(inode, mtime, ctime, mode)` to
    /// the recorded snapshot. Returns the cached hash when the
    /// match is exact; `None` otherwise. The caller falls back to
    /// the read-and-hash path.
    fn lookup_stat_cache_hash(&self, entry: &WalkEntry<'_>) -> Option<ContentHash> {
        let cache = self.stat_cache?;
        let rel = entry.path.strip_prefix(self.walk_root).ok()?;
        // Manifest keys use forward-slash separators (cross-platform
        // by construction; see `populate_manifest_from_tree`).
        let mut rel_str = String::with_capacity(rel.as_os_str().len());
        for (i, component) in rel.components().enumerate() {
            let std::path::Component::Normal(s) = component else {
                return None;
            };
            if i > 0 {
                rel_str.push('/');
            }
            rel_str.push_str(s.to_str()?);
        }
        let cached = cache.files.get(&rel_str)?;
        let (size, inode, mtime_ns, ctime_ns, mode) =
            crate::stat_signature::stat_signature(entry.path, &entry.metadata);
        let stat = crate::thread_manifest::ManifestFile {
            hash: cached.hash,
            size,
            inode,
            mtime_ns,
            ctime_ns,
            mode,
        };
        if stat.matches(cached) {
            Some(cached.hash)
        } else {
            None
        }
    }

    /// Push a blob into the pending pack if it's not already in the
    /// store and not already queued. The hash is always the canonical
    /// blob hash — caller passes a precomputed one to avoid hashing
    /// twice.
    fn enqueue_blob(&mut self, blob: Blob, hash: ContentHash) -> Result<()> {
        if self.seen.contains(&hash) {
            return Ok(());
        }
        if self.repo.store.has_blob(&hash)? {
            self.seen.insert(hash);
            return Ok(());
        }
        self.seen.insert(hash);
        self.pending_blobs.push((hash, blob.into_content()));
        Ok(())
    }
}

impl WorktreeWalkPolicy for TreeBuildPolicy<'_> {
    type DirectoryState = TreeBuildState;
    type Output = TreeBuildOutput;

    fn enter_directory(
        &mut self,
        _directory: &WalkDirectory<'_>,
        _tree: Option<&Tree>,
    ) -> Result<Self::DirectoryState> {
        Ok(TreeBuildState::default())
    }

    fn visit_file(
        &mut self,
        entry: WalkEntry<'_>,
        _tree_entry: Option<&TreeEntry>,
        state: &mut Self::DirectoryState,
    ) -> Result<()> {
        trace!(file = %entry.path.display(), size = entry.metadata.len(), "Processing file");

        // Stat-cache fast path: when this build is on behalf of a
        // capture against a previously-materialised thread, reuse the
        // recorded hash if the file's stat fields haven't shifted
        // since materialise time. Skips the read+hash entirely for
        // unchanged files — the dominant cost on a "one file edited
        // in a big repo" capture.
        if let Some(hash) = self.lookup_stat_cache_hash(&entry) {
            self.stat_cache_hits += 1;
            state.profile.file_count += 1;
            state.entries.push(TreeEntry::file(
                entry.name.to_string(),
                hash,
                entry.executable,
            )?);
            return Ok(());
        }

        let read_start = Instant::now();
        let (blob, hash) = read_blob_with_hash(entry.path, entry.metadata.len())?;
        let read_elapsed = read_start.elapsed().as_millis();
        trace!(duration_ms = read_elapsed, "File read complete");

        // Defer the actual write — we accumulate every new blob and
        // install them as a single pack at the end of the walk
        // (one fsync regardless of file count, vs. ~30ms per loose
        // file on macOS). The tree entry only needs the hash.
        let enqueue_start = Instant::now();
        self.enqueue_blob(blob, hash)?;
        let enqueue_elapsed = enqueue_start.elapsed().as_millis();

        state.profile.file_count += 1;
        state.profile.blob_prep_ms += read_elapsed;
        state.profile.blob_write_ms += enqueue_elapsed;
        state.entries.push(TreeEntry::file(
            entry.name.to_string(),
            hash,
            entry.executable,
        )?);
        Ok(())
    }

    fn visit_symlink(
        &mut self,
        entry: WalkEntry<'_>,
        _tree_entry: Option<&TreeEntry>,
        state: &mut Self::DirectoryState,
    ) -> Result<()> {
        let target = fs::read_link(entry.path)?;
        // Validate symlink escape against the *walk root*, not
        // `repo.root()`. When `capture_thread_from_disk` builds a
        // tree from a dedicated thread worktree, the walk root is
        // the thread's checkout path (not the main repo) and
        // symlinks should be allowed to point inside it. Pre-fix
        // every symlink in such a worktree was rejected the moment
        // the slow path ran, breaking `thread switch` auto-capture
        // for any thread containing a symlink. For the common case
        // where `build_tree(self.root)` runs against the main repo
        // root, `walk_root == self.repo.root()` and behaviour is
        // unchanged.
        let symlink_dir = entry.path.parent().unwrap_or(self.walk_root);
        if !validate_symlink_target(self.walk_root, symlink_dir, &target) {
            return Err(HeddleError::InvalidSymlinkTarget(target));
        }

        let blob = Blob::new(objects::util::symlink_target_bytes(&target));
        let hash = blob.hash();
        let enqueue_start = Instant::now();
        self.enqueue_blob(blob, hash)?;
        state.profile.blob_write_ms += enqueue_start.elapsed().as_millis();
        state
            .entries
            .push(TreeEntry::symlink(entry.name.to_string(), hash)?);
        Ok(())
    }

    fn visit_directory_output(
        &mut self,
        entry: WalkEntry<'_>,
        _tree_entry: Option<&TreeEntry>,
        subtree: TreeBuildOutput,
        state: &mut Self::DirectoryState,
    ) -> Result<()> {
        trace!(dir = %entry.path.display(), "Processing directory");
        state.profile.blob_prep_ms += subtree.profile.blob_prep_ms;
        state.profile.blob_write_ms += subtree.profile.blob_write_ms;
        state.profile.tree_write_ms += subtree.profile.tree_write_ms;
        state.profile.file_count += subtree.profile.file_count;
        state.profile.dir_count += subtree.profile.dir_count + 1;
        let store_start = Instant::now();
        let hash = self.repo.store.put_tree(&subtree.tree)?;
        state.profile.tree_write_ms += store_start.elapsed().as_millis();
        state
            .entries
            .push(TreeEntry::directory(entry.name.to_string(), hash)?);
        Ok(())
    }

    fn visit_missing(
        &mut self,
        _rel_path: &Path,
        _tree_entry: &TreeEntry,
        _state: &mut Self::DirectoryState,
    ) -> Result<()> {
        Ok(())
    }

    fn leave_directory(
        &mut self,
        directory: &WalkDirectory<'_>,
        _tree: Option<&Tree>,
        state: Self::DirectoryState,
    ) -> Result<TreeBuildOutput> {
        debug!(
            dir = %self.repo.root().join(directory.rel_path).display(),
            files = state.profile.file_count,
            dirs = state.profile.dir_count,
            "Directory processed"
        );
        Ok(TreeBuildOutput {
            tree: Tree::from_entries(state.entries),
            profile: state.profile,
        })
    }
}

#[cfg(test)]
mod tests {
    use objects::object::ContentHash;
    use tempfile::TempDir;

    use crate::worktree_walk::{read_blob_with_hash, read_file_hash};

    #[test]
    fn read_blob_with_hash_uses_bytes_read_when_file_grows() {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("file.txt");

        std::fs::write(&path, b"abc").unwrap();
        let initial_size = std::fs::metadata(&path).unwrap().len();
        std::fs::write(&path, b"abcdef").unwrap();

        let (blob, hash) = read_blob_with_hash(&path, initial_size).unwrap();

        assert_eq!(blob.content(), b"abcdef");
        assert_eq!(hash, blob.hash());
    }

    #[test]
    fn read_file_hash_uses_bytes_read_when_file_grows() {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("file.txt");

        std::fs::write(&path, b"abc").unwrap();
        let initial_size = std::fs::metadata(&path).unwrap().len();
        std::fs::write(&path, b"abcdef").unwrap();

        let hash = read_file_hash(&path, initial_size).unwrap();

        assert_eq!(hash, ContentHash::compute_typed("blob", b"abcdef"));
    }
}
