// SPDX-License-Identifier: Apache-2.0
//! Tree building and materialization helpers.

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
    time::Instant,
};

use objects::{
    object::{Blob, ContentHash, State, StateId, Tree, TreeEntry},
    store::{ObjectStore, TreeWrite},
    util::gitlink_placeholder_bytes,
    worktree::WorktreeStatus,
};
use tracing::{debug, instrument, trace, warn};

use serde::{Deserialize, Serialize};

use super::{
    HeddleError, Repository, Result,
    repository_worktree_status::{WorktreeStatusDetailed, compare_worktree_with_index_detailed},
};
use crate::{
    FsMonitorSettings, WorktreeIndex, WorktreeStatusOptions,
    fsmonitor::{ChangeMonitorSession, ChangeMonitorToken, MonitorStatus},
    thread_manifest::ManifestFile,
    worktree_ignore::WorktreeIgnoreMatcher,
    worktree_index::{WorktreeIndexLoadStats, WorktreeIndexSaveStats},
    worktree_walk::{
        WalkDirectory, WalkEntry, WorktreeWalkPolicy, cache_key, read_blob_with_hash,
        validate_symlink_target, walk_worktree,
    },
};

#[derive(Debug, Clone, Default)]
pub struct WorktreeCompareProfile {
    pub scan_mode: String,
    pub fallback_reason: Option<String>,
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

pub(crate) type SnapshotTreeBuildOutput = (
    Tree,
    TreeBuildProfile,
    BTreeMap<String, ManifestFile>,
    Vec<(ContentHash, Vec<u8>)>,
    Vec<TreeWrite>,
    Option<ChangeMonitorToken>,
);

#[derive(Debug, Clone, Default)]
pub struct WorktreeStateLookupProfile {
    pub head_ms: u128,
    pub cache_read_ms: u128,
    pub cache_decode_ms: u128,
    pub cache_validate_ms: u128,
    pub store_read_ms: u128,
    pub cache_hit: bool,
}

#[derive(Serialize, Deserialize)]
struct WorktreeTreeChainCache {
    root: ContentHash,
    ignore_fingerprint: ContentHash,
    trees: Vec<Tree>,
}

#[derive(Debug, Clone)]
struct TreeBuildOutput {
    tree: Tree,
    profile: TreeBuildProfile,
    revalidation_files: BTreeMap<String, ManifestFile>,
    pending_blobs: Vec<(ContentHash, Vec<u8>)>,
    pending_trees: Vec<TreeWrite>,
    monitor_token: Option<ChangeMonitorToken>,
}

fn rewrite_single_tracked_file(
    repo: &Repository,
    tree: &Tree,
    components: &[&str],
    blob_hash: ContentHash,
    executable: bool,
    cached_trees: &HashMap<ContentHash, Tree>,
    descendant_trees: &mut Vec<TreeWrite>,
) -> Result<Option<Tree>> {
    let Some((name, rest)) = components.split_first() else {
        return Ok(None);
    };
    let Some(existing) = tree.get(name) else {
        return Ok(None);
    };
    let replacement = if rest.is_empty() {
        if existing.blob_hash().is_none() {
            return Ok(None);
        }
        TreeEntry::file((*name).to_string(), blob_hash, executable)?
    } else {
        let Some(child_hash) = existing.tree_hash() else {
            return Ok(None);
        };
        let child = match cached_trees.get(&child_hash) {
            Some(tree) => tree.clone(),
            None => {
                let Some(tree) = repo.store().get_tree(&child_hash)? else {
                    return Ok(None);
                };
                tree
            }
        };
        let Some(updated_child) = rewrite_single_tracked_file(
            repo,
            &child,
            rest,
            blob_hash,
            executable,
            cached_trees,
            descendant_trees,
        )?
        else {
            return Ok(None);
        };
        let updated_hash = updated_child.hash();
        descendant_trees.push(TreeWrite::descendant(updated_child, child_hash));
        TreeEntry::directory((*name).to_string(), updated_hash)?
    };
    let mut updated = tree.clone();
    updated.insert(replacement);
    Ok(Some(updated))
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
        self.build_tree_profiled_inner(dir, None, Some(manifest))
            .map(|(tree, _)| tree)
    }

    #[instrument(skip(self), fields(dir = %dir.display()))]
    pub fn build_tree_profiled(&self, dir: &Path) -> Result<(Tree, TreeBuildProfile)> {
        self.build_tree_profiled_inner(dir, None, None)
    }

    pub(crate) fn build_tree_profiled_against(
        &self,
        dir: &Path,
        baseline_tree: Option<&Tree>,
    ) -> Result<(Tree, TreeBuildProfile)> {
        self.build_tree_profiled_inner(dir, baseline_tree, None)
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
        self.build_tree_profiled_inner(dir, None, Some(manifest))
    }

    pub(crate) fn build_tree_profiled_with_stat_cache_against(
        &self,
        dir: &Path,
        baseline_tree: Option<&Tree>,
        manifest: &crate::thread_manifest::ThreadManifest,
    ) -> Result<(Tree, TreeBuildProfile)> {
        self.build_tree_profiled_inner(dir, baseline_tree, Some(manifest))
    }

    fn build_tree_profiled_inner(
        &self,
        dir: &Path,
        baseline_tree: Option<&Tree>,
        stat_cache: Option<&crate::thread_manifest::ThreadManifest>,
    ) -> Result<(Tree, TreeBuildProfile)> {
        self.build_tree_profiled_output(
            dir,
            baseline_tree,
            stat_cache,
            false,
            &self.default_worktree_status_options(),
        )
        .map(|output| (output.tree, output.profile))
    }

    pub(crate) fn build_tree_profiled_for_snapshot_against(
        &self,
        dir: &Path,
        baseline_tree: Option<&Tree>,
        stat_cache: Option<&crate::thread_manifest::ThreadManifest>,
        known_worktree_changes: Option<&WorktreeStatus>,
    ) -> Result<SnapshotTreeBuildOutput> {
        self.build_tree_profiled_for_snapshot_against_with_options(
            dir,
            baseline_tree,
            stat_cache,
            &self.default_worktree_status_options(),
            known_worktree_changes,
        )
    }

    pub(crate) fn build_tree_profiled_for_snapshot_against_with_options(
        &self,
        dir: &Path,
        baseline_tree: Option<&Tree>,
        stat_cache: Option<&crate::thread_manifest::ThreadManifest>,
        options: &WorktreeStatusOptions,
        known_worktree_changes: Option<&WorktreeStatus>,
    ) -> Result<SnapshotTreeBuildOutput> {
        let fast_start = Instant::now();
        if let Some(mut output) = self.try_build_single_changed_file_tree(
            dir,
            baseline_tree,
            options,
            known_worktree_changes,
        )? {
            output.profile.tree_walk_ms = fast_start.elapsed().as_millis();
            return Ok((
                output.tree,
                output.profile,
                output.revalidation_files,
                output.pending_blobs,
                output.pending_trees,
                output.monitor_token,
            ));
        }
        // Capture preflight may have advanced an fsmonitor cursor while
        // discovering a shape that the one-file rewrite cannot handle. In
        // that case the fallback must walk authoritatively instead of asking
        // a second monitor session for an event that was already consumed.
        let authoritative_options = WorktreeStatusOptions {
            fsmonitor: crate::FsMonitorSettings {
                mode: crate::FsMonitorMode::Off,
            },
        };
        let fallback_options = if known_worktree_changes.is_some() {
            &authoritative_options
        } else {
            options
        };
        self.build_tree_profiled_output(dir, baseline_tree, stat_cache, true, fallback_options)
            .map(|output| {
                (
                    output.tree,
                    output.profile,
                    output.revalidation_files,
                    output.pending_blobs,
                    output.pending_trees,
                    output.monitor_token,
                )
            })
    }

    /// Apply an authoritative one-file monitor delta directly to the baseline
    /// tree. This is the common capture path and avoids enumerating 1,000 root
    /// directories plus every sibling in the changed file's directory.
    /// Unsupported shapes (new/deleted paths, symlinks, policy changes, or a
    /// non-authoritative monitor) fall back to the general walker.
    fn try_build_single_changed_file_tree(
        &self,
        dir: &Path,
        baseline_tree: Option<&Tree>,
        options: &WorktreeStatusOptions,
        known_worktree_changes: Option<&WorktreeStatus>,
    ) -> Result<Option<TreeBuildOutput>> {
        if dir != self.root() {
            return Ok(None);
        }
        let Some(baseline_tree) = baseline_tree else {
            return Ok(None);
        };
        let monitor = ChangeMonitorSession::prepare(self.root(), options.fsmonitor);
        let changed = match known_worktree_changes {
            Some(status)
                if status.modified.len() == 1
                    && status.added.is_empty()
                    && status.deleted.is_empty() =>
            {
                status.modified[0].clone()
            }
            Some(_) => return Ok(None),
            None => match monitor.single_changed_path() {
                Some(path) => PathBuf::from(path),
                None => return Ok(None),
            },
        };
        let rel_path = changed.as_path();
        if rel_path.as_os_str().is_empty() {
            return Ok(None);
        }
        let components = rel_path
            .components()
            .map(|component| match component {
                std::path::Component::Normal(name) => name.to_str(),
                _ => None,
            })
            .collect::<Option<Vec<_>>>();
        let Some(components) = components.filter(|parts| !parts.is_empty()) else {
            return Ok(None);
        };

        let expected_ignore_fingerprint =
            WorktreeIgnoreMatcher::fingerprint_patterns(&self.ignore_patterns()?);
        let Some(cached_trees) = self
            .load_current_worktree_tree_chain(&baseline_tree.hash(), &expected_ignore_fingerprint)
        else {
            return Ok(None);
        };
        let nested_exclusions = self.nested_thread_worktree_exclusions(dir)?;
        let mut absolute = self.root().to_path_buf();
        for component in &components {
            absolute.push(component);
        }
        let canonical_absolute = absolute.canonicalize().unwrap_or_else(|_| absolute.clone());
        if nested_exclusions
            .iter()
            .any(|excluded| canonical_absolute.starts_with(excluded))
        {
            return Ok(None);
        }

        let metadata = match fs::symlink_metadata(&absolute) {
            Ok(metadata) if metadata.is_file() => metadata,
            Ok(_) => return Ok(None),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        #[cfg(unix)]
        let executable = {
            use std::os::unix::fs::PermissionsExt;
            metadata.permissions().mode() & 0o111 != 0
        };
        #[cfg(not(unix))]
        let executable = false;

        let (blob, hash) = read_blob_with_hash(&absolute, metadata.len())?;
        let mut pending_trees = Vec::with_capacity(components.len().saturating_sub(1));
        let Some(tree) = rewrite_single_tracked_file(
            self,
            baseline_tree,
            &components,
            hash,
            executable,
            &cached_trees,
            &mut pending_trees,
        )?
        else {
            return Ok(None);
        };
        let (size, inode, mtime_ns, ctime_ns, mode) =
            crate::stat_signature::stat_signature(&absolute, &metadata);
        let revalidation_files = BTreeMap::from([(
            cache_key(rel_path),
            ManifestFile {
                hash,
                size,
                inode,
                mtime_ns,
                ctime_ns,
                mode,
            },
        )]);
        Ok(Some(TreeBuildOutput {
            tree,
            profile: TreeBuildProfile {
                file_count: 1,
                dir_count: components.len().saturating_sub(1),
                ..TreeBuildProfile::default()
            },
            revalidation_files,
            pending_blobs: vec![(hash, blob.into_content())],
            pending_trees,
            monitor_token: monitor.revalidation_token(),
        }))
    }

    fn build_tree_profiled_output(
        &self,
        dir: &Path,
        baseline_tree: Option<&Tree>,
        stat_cache: Option<&crate::thread_manifest::ThreadManifest>,
        defer_object_writes: bool,
        options: &WorktreeStatusOptions,
    ) -> Result<TreeBuildOutput> {
        let patterns = self.ignore_patterns()?;
        debug!(pattern_count = patterns.len(), "Starting tree build");
        let start = Instant::now();
        let nested_exclusions = self.nested_thread_worktree_exclusions(dir)?;
        let tree = self.build_tree_walk(
            dir,
            &patterns,
            nested_exclusions,
            baseline_tree,
            stat_cache,
            defer_object_writes,
            options,
        );
        let elapsed = start.elapsed().as_millis();
        debug!(duration_ms = elapsed, "Tree build complete");
        tree.map(|mut output| {
            let mut profile = output.profile;
            profile.tree_walk_ms = elapsed;
            output.profile = profile;
            output
        })
    }

    #[allow(clippy::too_many_arguments)]
    #[instrument(skip(self, patterns, nested_exclusions, baseline_tree, stat_cache), fields(dir = %dir.display()))]
    fn build_tree_walk(
        &self,
        dir: &Path,
        patterns: &[String],
        nested_exclusions: Vec<std::path::PathBuf>,
        baseline_tree: Option<&Tree>,
        stat_cache: Option<&crate::thread_manifest::ThreadManifest>,
        defer_object_writes: bool,
        options: &WorktreeStatusOptions,
    ) -> Result<TreeBuildOutput> {
        let ignore_matcher = WorktreeIgnoreMatcher::cached(patterns)
            .with_nested_worktree_exclusions(nested_exclusions);
        let incremental_state = (dir == self.root() && baseline_tree.is_some()).then(|| {
            let monitor = ChangeMonitorSession::prepare(self.root(), options.fsmonitor);
            let index = if monitor.status == MonitorStatus::Usable {
                WorktreeIndex::load_hot_profiled_for_directories(
                    &self.worktree_index_path(),
                    &monitor.changed_directory_keys(),
                )
            } else {
                WorktreeIndex::load_profiled(&self.worktree_index_path())
            }
            .map(|(index, _)| index)
            .unwrap_or_default();
            (index, monitor)
        });
        let mut policy = TreeBuildPolicy::new(
            self,
            dir,
            stat_cache,
            incremental_state,
            defer_object_writes,
        );
        let mut output = walk_worktree(self, dir, &ignore_matcher, baseline_tree, &mut policy)?;
        output.monitor_token = policy
            .incremental_state
            .as_ref()
            .and_then(|(_, monitor)| monitor.revalidation_token());

        // Flush every newly-seen blob as a single packfile. Stores
        // that don't override `put_blobs_packed` fall back to per-blob
        // writes (correct, just slower). Time is folded into
        // `blob_write_ms` so the existing perf profile keeps tracking
        // total blob-storage cost.
        if defer_object_writes {
            output.pending_blobs = std::mem::take(&mut policy.pending_blobs);
            output.pending_trees = std::mem::take(&mut policy.pending_trees);
        } else if !policy.pending_blobs.is_empty() {
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

    pub fn require_tree_for_worktree_status(&self, hash: &ContentHash) -> Result<Tree> {
        let cache_path = self.root().join(".heddle/state/worktree-current-tree.bin");
        if let Ok(bytes) = fs::read(&cache_path)
            && let Ok(tree) = rmp_serde::from_slice::<Tree>(&bytes)
            && tree.validate().is_ok()
            && tree.hash() == *hash
        {
            return Ok(tree);
        }
        let tree = self.require_tree(hash)?;
        if let Ok(bytes) = rmp_serde::to_vec_named(&tree)
            && let Err(error) =
                objects::fs_atomic::write_file_atomic_reconstructible(&cache_path, &bytes)
        {
            warn!(path = %cache_path.display(), %error, "Could not refresh worktree tree cache");
        }
        Ok(tree)
    }

    pub fn state_for_worktree_status(&self, id: &StateId) -> Result<State> {
        self.state_for_worktree_status_profiled(id)
            .map(|(state, _)| state)
    }

    fn state_for_worktree_status_profiled(
        &self,
        id: &StateId,
    ) -> Result<(State, WorktreeStateLookupProfile)> {
        let mut profile = WorktreeStateLookupProfile::default();
        let cache_path = self.root().join(".heddle/state/worktree-current-state.bin");
        let cache_read_started = Instant::now();
        let cached = fs::read(&cache_path);
        profile.cache_read_ms = cache_read_started.elapsed().as_millis();
        if let Ok(bytes) = cached {
            let cache_decode_started = Instant::now();
            let decoded = rmp_serde::from_slice::<State>(&bytes);
            profile.cache_decode_ms = cache_decode_started.elapsed().as_millis();
            if let Ok(mut state) = decoded {
                let cache_validate_started = Instant::now();
                let matches = state.id() == *id;
                profile.cache_validate_ms = cache_validate_started.elapsed().as_millis();
                if matches {
                    state.state_id = *id;
                    profile.cache_hit = true;
                    return Ok((state, profile));
                }
            }
        }
        let store_read_started = Instant::now();
        let state = self
            .store()
            .get_state(id)?
            .ok_or(HeddleError::StateNotFound(*id))?;
        profile.store_read_ms = store_read_started.elapsed().as_millis();
        if let Ok(bytes) = rmp_serde::to_vec_named(&state)
            && let Err(error) =
                objects::fs_atomic::write_file_atomic_reconstructible(&cache_path, &bytes)
        {
            warn!(path = %cache_path.display(), %error, "Could not refresh worktree state cache");
        }
        Ok((state, profile))
    }

    pub fn current_state_for_worktree_status(&self) -> Result<Option<State>> {
        self.current_state_for_worktree_status_profiled()
            .map(|(state, _)| state)
    }

    pub fn current_state_for_worktree_status_profiled(
        &self,
    ) -> Result<(Option<State>, WorktreeStateLookupProfile)> {
        let head_started = Instant::now();
        let head = self.head()?;
        let head_ms = head_started.elapsed().as_millis();
        let Some(id) = head else {
            return Ok((
                None,
                WorktreeStateLookupProfile {
                    head_ms,
                    ..WorktreeStateLookupProfile::default()
                },
            ));
        };
        let (state, mut profile) = self.state_for_worktree_status_profiled(&id)?;
        profile.head_ms = head_ms;
        Ok((Some(state), profile))
    }

    /// Refresh the rebuildable current-state/tree materialized views from a
    /// snapshot that has already committed and published its authoritative
    /// state. A later process can resolve HEAD without searching pack indexes;
    /// torn/missing views are harmless because the readers validate hashes and
    /// fall back to the object store.
    pub(crate) fn cache_current_worktree_state(
        &self,
        state: &State,
        tree: &Tree,
        tree_chain: &[Tree],
    ) {
        let state_path = self.root().join(".heddle/state/worktree-current-state.bin");
        if let Ok(bytes) = rmp_serde::to_vec_named(state)
            && let Err(error) = fs::write(&state_path, &bytes)
        {
            warn!(path = %state_path.display(), %error, "Could not refresh worktree state cache");
        }
        let tree_path = self.root().join(".heddle/state/worktree-current-tree.bin");
        if let Ok(bytes) = rmp_serde::to_vec_named(tree)
            && let Err(error) = fs::write(&tree_path, &bytes)
        {
            warn!(path = %tree_path.display(), %error, "Could not refresh worktree tree cache");
        }
        self.cache_current_worktree_tree_chain(tree, tree_chain);
    }

    fn cache_current_worktree_tree_chain(&self, root: &Tree, trees: &[Tree]) {
        let path = self
            .root()
            .join(".heddle/state/worktree-current-tree-chain.bin");
        let Ok(patterns) = self.ignore_patterns() else {
            return;
        };
        let cache = WorktreeTreeChainCache {
            root: root.hash(),
            ignore_fingerprint: WorktreeIgnoreMatcher::fingerprint_patterns(&patterns),
            trees: trees.to_vec(),
        };
        if let Ok(bytes) = rmp_serde::to_vec_named(&cache)
            && let Err(error) = fs::write(&path, &bytes)
        {
            warn!(path = %path.display(), %error, "Could not refresh worktree tree-chain cache");
        }
    }

    fn load_current_worktree_tree_chain(
        &self,
        expected_root: &ContentHash,
        expected_ignore_fingerprint: &ContentHash,
    ) -> Option<HashMap<ContentHash, Tree>> {
        let path = self
            .root()
            .join(".heddle/state/worktree-current-tree-chain.bin");
        let Ok(bytes) = fs::read(path) else {
            return None;
        };
        let Ok(cache) = rmp_serde::from_slice::<WorktreeTreeChainCache>(&bytes) else {
            return None;
        };
        if cache.root != *expected_root || cache.ignore_fingerprint != *expected_ignore_fingerprint
        {
            return None;
        }
        let mut trees = HashMap::with_capacity(cache.trees.len());
        for tree in cache.trees {
            if tree.validate().is_err() {
                return None;
            }
            trees.insert(tree.hash(), tree);
        }
        Some(trees)
    }

    /// Return the complete gitlink summary cached for `tree`, when the hot
    /// index proves that its root directory summary was built from that tree.
    pub fn cached_gitlinks_for_tree(&self, tree: &Tree) -> Option<Vec<(String, String)>> {
        let (root, gitlinks) =
            WorktreeIndex::load_hot_gitlinks_summary(&self.worktree_index_path()).ok()??;
        (root == tree.hash()).then_some(gitlinks)
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
        let index_existed = index_path.exists();
        let mut index_invalidation_reason = None;
        let monitor_prepare_start = Instant::now();
        let monitor = ChangeMonitorSession::prepare(self.root(), options.fsmonitor);
        let monitor_prepare_ms = monitor_prepare_start.elapsed().as_millis();
        let load_start = Instant::now();
        let index_result = if monitor.status == MonitorStatus::Usable {
            WorktreeIndex::load_hot_profiled_for_directories(
                &index_path,
                &monitor.changed_directory_keys(),
            )
        } else {
            WorktreeIndex::load_profiled(&index_path)
        };
        let (mut index, load_stats) = match index_result {
            Ok(result) => result,
            Err(error) => {
                index_invalidation_reason = Some(match &error {
                    crate::worktree_index::IndexError::VersionMismatch { .. } => {
                        "index_version_changed"
                    }
                    crate::worktree_index::IndexError::ChecksumMismatch
                    | crate::worktree_index::IndexError::InvalidFormat(_)
                    | crate::worktree_index::IndexError::InvalidUtf8(_) => "index_corrupt",
                    crate::worktree_index::IndexError::Io(_) => "index_io_error",
                });
                warn!(path = %index_path.display(), %error, "Ignoring unreadable worktree index");
                (WorktreeIndex::new(), WorktreeIndexLoadStats::default())
            }
        };
        if !index_existed {
            index_invalidation_reason = Some("missing_index");
        }
        let index_load_ms = load_start.elapsed().as_millis();

        let patterns = self.ignore_patterns()?;
        let nested_exclusions = self.nested_thread_worktree_exclusions(self.root())?;
        let ignore_matcher = WorktreeIgnoreMatcher::cached(&patterns)
            .with_nested_worktree_exclusions(nested_exclusions);
        let changed_path_mode = monitor.can_filter_directory_children(Path::new(""), &index);
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
        let (index_save_ms, save_stats, index_persisted) = if index.is_dirty() {
            match index.save_profiled(&index_path) {
                Ok(stats) => {
                    index.mark_clean();
                    (save_start.elapsed().as_millis(), stats, true)
                }
                Err(error) => {
                    warn!(path = %index_path.display(), %error, "Failed to persist worktree index");
                    (0, WorktreeIndexSaveStats::default(), false)
                }
            }
        } else {
            (0, WorktreeIndexSaveStats::default(), true)
        };

        let persist_start = Instant::now();
        if index_persisted && let Err(error) = monitor.persist(status.is_clean()) {
            warn!(path = %self.root().display(), %error, "Failed to persist monitor state");
        }
        let monitor_persist_ms = persist_start.elapsed().as_millis();

        heddle_perf_contract::record_worktree_scan(
            stats.directories_scanned,
            stats.directories_skipped,
            stats.files_hashed,
            stats.monitor_changed_paths,
            monitor_prepare_ms.try_into().unwrap_or(u64::MAX),
        );

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

        let fallback_reason = (!changed_path_mode).then(|| {
            index_invalidation_reason
                .map(str::to_string)
                .or_else(|| monitor.reason.clone())
                .unwrap_or_else(|| "missing_index_baseline".to_string())
        });

        Ok((
            status,
            WorktreeCompareProfile {
                scan_mode: if changed_path_mode {
                    "changed_paths".to_string()
                } else {
                    "fallback_scan".to_string()
                },
                fallback_reason,
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
        Ok(report)
    }
}

#[derive(Default)]
struct TreeBuildState {
    entries: Vec<TreeEntry>,
    profile: TreeBuildProfile,
    revalidation_files: BTreeMap<String, ManifestFile>,
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
    incremental_state: Option<(WorktreeIndex, ChangeMonitorSession)>,
    stat_cache_hits: u64,
    /// Blobs encountered during the walk that aren't already in the
    /// store. Drained once at the end of the walk into a single
    /// packfile via `ObjectStore::put_blobs_packed` — turns N×fsync
    /// per blob into 2×fsync total (the .pack + .idx).
    pending_blobs: Vec<(ContentHash, Vec<u8>)>,
    /// Hashes already queued in `pending_blobs` so we don't double-add
    /// content-equal files (which is common: README.md, .gitkeep, etc).
    seen: HashSet<ContentHash>,
    pending_trees: Vec<TreeWrite>,
    defer_object_writes: bool,
}

impl<'a> TreeBuildPolicy<'a> {
    fn new(
        repo: &'a Repository,
        walk_root: &'a Path,
        stat_cache: Option<&'a crate::thread_manifest::ThreadManifest>,
        incremental_state: Option<(WorktreeIndex, ChangeMonitorSession)>,
        defer_object_writes: bool,
    ) -> Self {
        Self {
            repo,
            walk_root,
            stat_cache,
            incremental_state,
            stat_cache_hits: 0,
            pending_blobs: Vec::new(),
            seen: HashSet::new(),
            pending_trees: Vec::new(),
            defer_object_writes,
        }
    }

    /// Look up `entry`'s manifest record by relative path and, if
    /// found, compare the on-disk `(inode, mtime, ctime, mode)` to
    /// the recorded snapshot. Returns the cached hash when the
    /// match is exact; `None` otherwise. The caller falls back to
    /// the read-and-hash path.
    fn lookup_stat_cache_hash(&self, entry: &WalkEntry<'_>) -> Option<ContentHash> {
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
        if let Some(cached) = self.stat_cache.and_then(|cache| cache.files.get(&rel_str)) {
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
                return Some(cached.hash);
            }
        }
        let cached = self
            .incremental_state
            .as_ref()
            .and_then(|(index, _)| index.fresh_entry(&rel_str, &entry.metadata))?;
        Some(cached.hash)
    }

    fn monitor_proves_unchanged(&self, entry: &WalkEntry<'_>) -> bool {
        let Some((index, monitor)) = &self.incremental_state else {
            return false;
        };
        entry
            .path
            .strip_prefix(self.walk_root)
            .is_ok_and(|path| monitor.can_reuse_unchanged_child(path, index))
    }

    fn changed_path_mode(&self) -> bool {
        self.incremental_state
            .as_ref()
            .is_some_and(|(index, monitor)| {
                monitor.status == MonitorStatus::Usable && index.get_directory("").is_some()
            })
    }

    /// Push a blob into the pending pack if it's not already in the
    /// store and not already queued. The hash is always the canonical
    /// blob hash — caller passes a precomputed one to avoid hashing
    /// twice.
    fn enqueue_blob(&mut self, blob: Blob, hash: ContentHash) -> Result<()> {
        if self.seen.contains(&hash) {
            return Ok(());
        }
        if !self.changed_path_mode() && self.repo.store.has_blob_locally(&hash)? {
            self.seen.insert(hash);
            return Ok(());
        }
        self.seen.insert(hash);
        self.pending_blobs.push((hash, blob.into_content()));
        Ok(())
    }

    fn record_revalidation_file(
        &self,
        entry: &WalkEntry<'_>,
        hash: ContentHash,
        state: &mut TreeBuildState,
    ) -> Result<()> {
        let rel = entry.path.strip_prefix(self.walk_root).map_err(|_| {
            HeddleError::Config(format!(
                "worktree entry {} escaped snapshot root {}",
                entry.path.display(),
                self.walk_root.display()
            ))
        })?;
        let (size, inode, mtime_ns, ctime_ns, mode) =
            crate::stat_signature::stat_signature(entry.path, &entry.metadata);
        state.revalidation_files.insert(
            cache_key(rel),
            ManifestFile {
                hash,
                size,
                inode,
                mtime_ns,
                ctime_ns,
                mode,
            },
        );
        Ok(())
    }
}

impl WorktreeWalkPolicy for TreeBuildPolicy<'_> {
    type DirectoryState = TreeBuildState;
    type Output = TreeBuildOutput;

    fn reuse_tree_entry_before_metadata(
        &mut self,
        rel_path: &Path,
        tree_entry: &TreeEntry,
        state: &mut Self::DirectoryState,
    ) -> Result<bool> {
        let Some(_) = tree_entry.tree_hash() else {
            return Ok(false);
        };
        let Some((index, monitor)) = &self.incremental_state else {
            return Ok(false);
        };
        let reusable = monitor.can_reuse_unchanged_child(rel_path, index);
        if reusable {
            state.entries.push(tree_entry.clone());
            state.profile.dir_count += 1;
        }
        Ok(reusable)
    }

    fn cached_tree_for_entry(&self, rel_path: &Path, tree_hash: &ContentHash) -> Option<Tree> {
        let key = crate::worktree_walk::cache_key(rel_path);
        self.incremental_state
            .as_ref()
            .and_then(|(index, _)| index.clean_tree(&key, tree_hash))
            .cloned()
    }

    fn skip_directory_before_enumeration(
        &mut self,
        rel_path: &Path,
        _metadata: &fs::Metadata,
        tree: Option<&Tree>,
    ) -> Result<Option<Self::Output>> {
        let Some((index, monitor)) = &self.incremental_state else {
            return Ok(None);
        };
        let Some(tree) = tree else {
            return Ok(None);
        };
        Ok(monitor
            .can_skip_directory(rel_path, Some(tree), index)
            .then(|| TreeBuildOutput {
                tree: tree.clone(),
                profile: TreeBuildProfile::default(),
                revalidation_files: BTreeMap::new(),
                pending_blobs: Vec::new(),
                pending_trees: Vec::new(),
                monitor_token: None,
            }))
    }

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
        tree_entry: Option<&TreeEntry>,
        state: &mut Self::DirectoryState,
    ) -> Result<()> {
        trace!(file = %entry.path.display(), size = entry.metadata.len(), "Processing file");

        if self.monitor_proves_unchanged(&entry)
            && let Some(tree_entry) = tree_entry
            && let Some(hash) = tree_entry.blob_hash()
        {
            self.record_revalidation_file(&entry, hash, state)?;
            state.profile.file_count += 1;
            state.entries.push(tree_entry.clone());
            return Ok(());
        }

        if let Some(target) = tree_entry.and_then(TreeEntry::gitlink_target) {
            let read_start = Instant::now();
            let (blob, hash) = read_blob_with_hash(entry.path, entry.metadata.len())?;
            let read_elapsed = read_start.elapsed().as_millis();
            if blob.content() == gitlink_placeholder_bytes(&target) {
                self.record_revalidation_file(&entry, hash, state)?;
                state.profile.file_count += 1;
                state.profile.blob_prep_ms += read_elapsed;
                state
                    .entries
                    .push(TreeEntry::gitlink(entry.name.to_string(), target)?);
                return Ok(());
            }

            let enqueue_start = Instant::now();
            self.enqueue_blob(blob, hash)?;
            let enqueue_elapsed = enqueue_start.elapsed().as_millis();
            state.profile.file_count += 1;
            state.profile.blob_prep_ms += read_elapsed;
            state.profile.blob_write_ms += enqueue_elapsed;
            self.record_revalidation_file(&entry, hash, state)?;
            state.entries.push(TreeEntry::file(
                entry.name.to_string(),
                hash,
                entry.executable,
            )?);
            return Ok(());
        }

        // Stat-cache fast path: when this build is on behalf of a
        // capture against a previously-materialised thread, reuse the
        // recorded hash if the file's stat fields haven't shifted
        // since materialise time. Skips the read+hash entirely for
        // unchanged files — the dominant cost on a "one file edited
        // in a big repo" capture.
        if !self.changed_path_mode()
            && let Some(hash) = self.lookup_stat_cache_hash(&entry)
            && self.repo.store.has_blob_locally(&hash)?
        {
            self.record_revalidation_file(&entry, hash, state)?;
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
        self.record_revalidation_file(&entry, hash, state)?;
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
        tree_entry: Option<&TreeEntry>,
        state: &mut Self::DirectoryState,
    ) -> Result<()> {
        if self.monitor_proves_unchanged(&entry)
            && let Some(tree_entry) = tree_entry
            && let Some(hash) = tree_entry.symlink_hash()
        {
            self.record_revalidation_file(&entry, hash, state)?;
            state.entries.push(tree_entry.clone());
            return Ok(());
        }
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
            return Err(HeddleError::InvalidSymlinkTarget {
                path: entry
                    .path
                    .strip_prefix(self.walk_root)
                    .unwrap_or(entry.path)
                    .to_path_buf(),
                target,
            });
        }

        let blob = Blob::new(objects::util::symlink_target_bytes(&target));
        let hash = blob.hash();
        let enqueue_start = Instant::now();
        self.enqueue_blob(blob, hash)?;
        state.profile.blob_write_ms += enqueue_start.elapsed().as_millis();
        self.record_revalidation_file(&entry, hash, state)?;
        state
            .entries
            .push(TreeEntry::symlink(entry.name.to_string(), hash)?);
        Ok(())
    }

    fn visit_directory_output(
        &mut self,
        entry: WalkEntry<'_>,
        tree_entry: Option<&TreeEntry>,
        subtree: TreeBuildOutput,
        state: &mut Self::DirectoryState,
    ) -> Result<()> {
        trace!(dir = %entry.path.display(), "Processing directory");
        state.profile.blob_prep_ms += subtree.profile.blob_prep_ms;
        state.profile.blob_write_ms += subtree.profile.blob_write_ms;
        state.profile.tree_write_ms += subtree.profile.tree_write_ms;
        state.profile.file_count += subtree.profile.file_count;
        state.profile.dir_count += subtree.profile.dir_count + 1;
        state.revalidation_files.extend(subtree.revalidation_files);
        let hash = subtree.tree.hash();
        if self.defer_object_writes {
            let write = match tree_entry.and_then(TreeEntry::tree_hash) {
                Some(parent) => TreeWrite::descendant(subtree.tree, parent),
                None => TreeWrite::anchor(subtree.tree),
            };
            self.pending_trees.push(write);
        } else {
            let store_start = Instant::now();
            self.repo.store.put_tree(&subtree.tree)?;
            state.profile.tree_write_ms += store_start.elapsed().as_millis();
        }
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
            revalidation_files: state.revalidation_files,
            pending_blobs: Vec::new(),
            pending_trees: Vec::new(),
            monitor_token: None,
        })
    }
}
impl Repository {
    pub fn ignore_patterns(&self) -> Result<Vec<String>> {
        let mut patterns = self.config.worktree.ignore.clone();
        // Default config includes `.heddle`, but that is a last-match
        // pattern. Root `.heddle/` is reserved after this list is
        // compiled — see `objects::worktree::is_reserved_worktree_path`.
        // Reserve the operator-local courtesy-stub filename. It is a Heddle
        // artifact written for under-tier checkouts, never tracked content.
        // Excluding it here is the single tree-build chokepoint every capture path
        // consults (`build_tree`, `build_tree_with_stat_cache`, and the stat-cache
        // no-op predicate), so the stub can never be pulled into a captured thread
        // by any of them — including a plain `snapshot`/`capture` taken from inside
        // a withheld worktree, which does not go through the withheld-manifest guard
        // (heddle#316). ROOT-ANCHORED (`/HEDDLE-EMBARGO.txt`): the stub is only ever
        // written at the worktree root, so the bare filename — which gitignore
        // matches at ANY depth — would silently drop a user's own
        // `sub/HEDDLE-EMBARGO.txt` from capture (heddle#316 #9).
        patterns.push(format!(
            "/{}",
            super::repository_thread_materialize::COURTESY_STUB_FILENAME
        ));
        // Root Git metadata is repository-engine state, never source content.
        patterns.push("/.git/".to_string());
        // Native and Git-overlay repositories share the root `.gitignore`
        // convention. This is intentionally a plain worktree-file read: a
        // native repository does not need `.git` metadata for these rules to
        // apply. `.heddleignore` is appended below, so the two files form one
        // ordered matcher rather than one shadowing the other.
        append_ignore_file_patterns(&mut patterns, &self.root.join(".gitignore"))?;
        // Worktree-local, never-captured excludes (heddle's analogue of
        // `.git/info/exclude`). Lives under THIS worktree's own `.heddle/`
        // (`root/.heddle`, which is local even for a shared-store checkout), so
        // it is never captured. Lets `start --hydrate` ignore symlinked deps
        // without dirtying a tracked `.heddleignore` (heddle#356 cid 3333881577).
        // `append_ignore_file_patterns` no-ops when the file is absent — the
        // common case for a plain repo.
        append_ignore_file_patterns(
            &mut patterns,
            &self.root.join(".heddle").join("info").join("exclude"),
        )?;
        let path = self.root.join(".heddleignore");

        if path.exists() {
            append_ignore_file_patterns(&mut patterns, &path)?;
        }

        Ok(patterns)
    }

    /// Canonical absolute paths of *other* threads' worktrees that are
    /// strict descendants of `walk_root`. The walker uses these to
    /// avoid scanning a sibling thread's files into the current
    /// thread's tree (a common shape when an agent worktree is
    /// materialized inside the parent repo, e.g. `--path-prefix
    /// ./agents`). Computed once per scan, not once per file.
    ///
    /// Returns paths that
    ///   - are strict descendants of canonical `walk_root`, and
    ///   - are NOT equal to `walk_root` itself (each thread can scan
    ///     its own worktree without excluding itself).
    ///
    /// Threads with no recorded worktree, or worktrees that no longer
    /// exist on disk, are skipped without error.
    pub fn nested_thread_worktree_exclusions(&self, walk_root: &Path) -> Result<Vec<PathBuf>> {
        let canonical_walk_root = walk_root
            .canonicalize()
            .unwrap_or_else(|_| walk_root.to_path_buf());
        let manager = crate::thread_storage::ThreadManager::new(self.heddle_dir());
        let mut exclusions: Vec<PathBuf> = Vec::new();
        let mut seen: HashSet<PathBuf> = HashSet::new();
        for thread in manager.list()? {
            for candidate in [
                Some(&thread.execution_path),
                thread.materialized_path.as_ref(),
            ]
            .into_iter()
            .flatten()
            {
                if candidate.as_os_str().is_empty() {
                    continue;
                }
                let canonical = match candidate.canonicalize() {
                    Ok(path) => path,
                    Err(_) => continue,
                };
                if canonical == canonical_walk_root {
                    continue;
                }
                if !canonical.starts_with(&canonical_walk_root) {
                    continue;
                }
                if seen.insert(canonical.clone()) {
                    exclusions.push(canonical);
                }
            }
        }
        Ok(exclusions)
    }
}

fn append_ignore_file_patterns(patterns: &mut Vec<String>, path: &Path) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let contents = fs::read_to_string(path)?;
    for line in contents.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        // Preserve repetitions. Gitignore matching is last-match-wins, so a
        // repeated rule after a negation can change the outcome even when the
        // same text appeared in an earlier file.
        patterns.push(trimmed.to_string());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use objects::object::{ContentHash, LeafPolicy, Tree, TreeEntry, resolve_tree_path};
    use objects::store::ObjectStore;
    use tempfile::TempDir;

    use crate::{
        Repository,
        worktree_ignore::WorktreeIgnoreMatcher,
        worktree_walk::{read_blob_with_hash, read_file_hash},
    };

    #[test]
    fn current_tree_chain_cache_is_hash_validated_and_root_scoped() {
        let temp_dir = TempDir::new().unwrap();
        let repo = Repository::init_default(temp_dir.path()).unwrap();
        let blob = ContentHash::compute_typed("blob", b"cached");
        let child = Tree::from_entries(vec![TreeEntry::file("file", blob, false).unwrap()]);
        let root = Tree::from_entries(vec![TreeEntry::directory("dir", child.hash()).unwrap()]);
        let ignore_fingerprint =
            WorktreeIgnoreMatcher::fingerprint_patterns(&repo.ignore_patterns().unwrap());

        repo.cache_current_worktree_tree_chain(&root, &[]);
        assert!(
            repo.load_current_worktree_tree_chain(&root.hash(), &ignore_fingerprint)
                .unwrap()
                .is_empty()
        );
        repo.cache_current_worktree_tree_chain(&root, std::slice::from_ref(&child));
        let loaded = repo
            .load_current_worktree_tree_chain(&root.hash(), &ignore_fingerprint)
            .unwrap();
        assert_eq!(loaded.get(&child.hash()), Some(&child));
        assert!(
            repo.load_current_worktree_tree_chain(
                &ContentHash::compute_typed("tree", b"other"),
                &ignore_fingerprint,
            )
            .is_none()
        );
        assert!(
            repo.load_current_worktree_tree_chain(
                &root.hash(),
                &ContentHash::compute_typed("heddle.ignore", b"changed"),
            )
            .is_none()
        );

        std::fs::write(
            temp_dir
                .path()
                .join(".heddle/state/worktree-current-tree-chain.bin"),
            b"torn",
        )
        .unwrap();
        assert!(
            repo.load_current_worktree_tree_chain(&root.hash(), &ignore_fingerprint)
                .is_none()
        );
    }

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

    #[test]
    fn build_tree_hard_denies_root_heddle_after_gitignore_unignore() {
        let temp_dir = TempDir::new().unwrap();
        let repo = Repository::init_default(temp_dir.path()).unwrap();
        std::fs::write(
            temp_dir.path().join(".gitignore"),
            "!.heddle/\n!.heddle/**\n",
        )
        .unwrap();
        std::fs::write(
            temp_dir.path().join(".heddleignore"),
            "!.heddle/\n!.heddle/identity.toml\n",
        )
        .unwrap();
        std::fs::write(temp_dir.path().join("kept.txt"), "ok\n").unwrap();
        std::fs::write(
            temp_dir.path().join(".heddle").join("identity.toml"),
            "secret-key-material\n",
        )
        .unwrap();
        std::fs::create_dir_all(temp_dir.path().join("examples/calculator/.heddle")).unwrap();
        std::fs::write(
            temp_dir
                .path()
                .join("examples/calculator/.heddle/identity.toml"),
            "fixture\n",
        )
        .unwrap();

        let tree = repo.build_tree(temp_dir.path()).unwrap();
        repo.store().put_tree(&tree).unwrap();
        let hash = tree.hash();

        assert!(
            resolve_tree_path(
                repo.store(),
                &hash,
                Path::new("kept.txt"),
                LeafPolicy::Entry
            )
            .unwrap()
            .is_some()
        );
        assert!(
            tree.get(".heddle").is_none(),
            "root .heddle must stay out of the captured tree"
        );
        assert!(
            resolve_tree_path(
                repo.store(),
                &hash,
                Path::new(".heddle/identity.toml"),
                LeafPolicy::Entry,
            )
            .unwrap()
            .is_none()
        );
        assert!(
            resolve_tree_path(
                repo.store(),
                &hash,
                Path::new("examples/calculator/.heddle/identity.toml"),
                LeafPolicy::Entry,
            )
            .unwrap()
            .is_some(),
            "nested fixture .heddle must remain capturable"
        );
    }
}
