// SPDX-License-Identifier: Apache-2.0
//! End-to-end orchestration: git repo + transcripts + Heddle repo → imported.
//!
//! This module does not add new translation logic — it wires the existing
//! pieces together so `heddle-ingest import` can run a full pass:
//!
//! 1. Open the source git repo via [`GitSource`].
//! 2. Collect every live ref plus every reflog-only commit SHA so nothing
//!    gets dropped.
//! 3. Topologically order the union, then for each commit:
//!    translate its tree (memoized), then write the state.
//! 4. Emit threads/markers from the live refs.
//!
//! The reflog → oplog translation and the reasoning-point extraction live
//! in downstream modules; [`Importer::run`] leaves their seams wired but
//! stubbed behind TODOs so we can land milestones independently.

use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use objects::{
    object::{Blob, ContentHash, Tree, TreeEntry},
    store::{
        CompressionConfig, LocalObjectStore, PackMaintenanceStoreExt,
        pack::{ObjectType as PackObjectType, PackObjectId, StreamingPackBuilder},
    },
    util::{GitTreeNameClassification, GitTreeNameLossyAction, classify_git_tree_name},
};
use oplog::oplog::{LocalOpLogBackend, OpLog};
use refs::refs::RefBackend;
use tracing::info;

use crate::{
    IngestError,
    git_walk::{
        CommitEntry, GitSource, RefDiscoveryStats, RefHead, RefNamespace, TreeChild, TreeChildKind,
    },
    import_options::{
        ImportOptions, LossyImportEntry, entry_relative_to_prefix, fail_lossy_entry,
        join_tree_path, rebase_lossy_entry,
    },
    oplog_emit::{OplogEmitStats, OplogEmitter},
    ref_emit::{RefEmitStats, RefEmitter},
    sha_map::ShaMap,
    state_writer::state_from_commit,
};

static IMPORT_RUN_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Counters reported back from [`Importer::run`] — the post-import
/// equivalent of `git log --reflog --all | wc -l`.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ImportStats {
    /// Per-namespace counts of refs the walker saw on the source side.
    /// `seen` here is "what the walker enumerated *and resolved* to a
    /// commit"; refs that failed to peel land in `peel_failed`, refs
    /// the walker decided to suppress (e.g. `origin/HEAD` symbolic) land
    /// in `symbolic_skipped`. These two together give an "ignored" count.
    pub refs_seen: RefDiscoveryStats,
    /// Commits translated (live + reflog-only).
    pub commits_imported: usize,
    /// New Heddle states written during this import. Re-runs can inspect the
    /// same commits while creating zero new states.
    pub states_created: usize,
    /// Distinct trees materialized (after memoization).
    pub trees_imported: usize,
    /// Distinct blobs materialized (after memoization).
    pub blobs_imported: usize,
    pub refs: RefEmitStats,
    /// Oplog ops emitted from the reflog. Zero when the importer was
    /// constructed without an oplog backend — the mechanical import is
    /// still valid, it just loses the honest-history replay.
    pub oplog: OplogEmitStats,
    /// Commits found in the reflog that were not live-reachable. A
    /// non-zero count here is evidence the reflog rescued work the
    /// refs-only walker would have missed.
    pub reflog_only_commits: usize,
    /// Git tree entries that were dropped or converted because the caller
    /// explicitly opted into lossy import.
    pub lossy_entries: Vec<LossyImportEntry>,
}

/// Which Git refs a mechanical import should ingest.
///
/// The default is all refs. A non-empty ref list scopes the importer to
/// matching commit-pointing heads before it walks commits, writes refs, or
/// replays reflogs.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ImportScope {
    refs: Vec<String>,
}

impl ImportScope {
    pub fn all() -> Self {
        Self { refs: Vec::new() }
    }

    pub fn refs(refs: Vec<String>) -> Self {
        Self { refs }
    }

    pub fn is_all(&self) -> bool {
        self.refs.is_empty()
    }

    pub fn requested_refs(&self) -> &[String] {
        &self.refs
    }

    fn resolve_heads(
        &self,
        heads: Vec<RefHead>,
        refs_seen: RefDiscoveryStats,
    ) -> crate::Result<(Vec<RefHead>, RefDiscoveryStats)> {
        if self.is_all() {
            return Ok((heads, refs_seen));
        }

        let mut matched = vec![false; self.refs.len()];
        let mut selected = Vec::new();
        for head in heads {
            let mut selected_head = false;
            for (idx, spec) in self.refs.iter().enumerate() {
                if ref_head_matches(&head, spec) {
                    matched[idx] = true;
                    selected_head = true;
                }
            }
            if selected_head {
                selected.push(head);
            }
        }

        let missing = self
            .refs
            .iter()
            .enumerate()
            .filter(|(idx, _)| !matched[*idx])
            .map(|(_, spec)| spec.clone())
            .collect::<Vec<_>>();
        if !missing.is_empty() {
            return Err(IngestError::Git(format!(
                "requested ref(s) not found or not commit-pointing: {}",
                missing.join(", ")
            )));
        }

        let refs_seen = ref_stats_from_heads(&selected);
        Ok((selected, refs_seen))
    }
}

fn ref_head_matches(head: &RefHead, spec: &str) -> bool {
    let spec = spec.trim();
    !spec.is_empty() && (spec == head.full_name || spec == head.short_name)
}

fn ref_stats_from_heads(heads: &[RefHead]) -> RefDiscoveryStats {
    let mut stats = RefDiscoveryStats::default();
    for head in heads {
        match head.namespace {
            RefNamespace::Branch => stats.local_branches += 1,
            RefNamespace::Tag => stats.tags += 1,
            RefNamespace::RemoteBranch => stats.remote_branches += 1,
        }
    }
    stats
}

/// Orchestrates one import pass.
///
/// Generic over the ref, object-store, and oplog backends — the store `S`
/// is threaded as a borrowed concrete type so writes statically dispatch
/// through the heddle#283 enum rather than a vtable. `O` defaults to `OpLog`
/// so [`Importer::new`] (which starts without an oplog backend) has a
/// concrete type; [`Importer::with_oplog`] rebinds `O` to whatever backend
/// the caller attaches.
pub struct Importer<
    'a,
    R: RefBackend,
    S: LocalObjectStore + PackMaintenanceStoreExt,
    O: LocalOpLogBackend = OpLog,
> {
    git: &'a GitSource,
    store: &'a S,
    refs: &'a R,
    map: &'a mut ShaMap,
    oplog: Option<&'a O>,
    options: ImportOptions,
    scope: ImportScope,
    /// Where the streaming pack builder writes its in-flight pack
    /// file and 512 index-bucket files. Both are removed on a clean
    /// finalize. Defaults to `std::env::temp_dir()/heddle-ingest-<pid>`
    /// when the caller doesn't pass one — but production calls
    /// (`import_git_into`) override this to a path under the heddle
    /// store's directory so the final `rename(2)` lands on the same
    /// filesystem and stays atomic.
    pack_staging_dir: Option<PathBuf>,
    progress: Option<&'a mut dyn FnMut(ImportProgressEvent)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImportProgressEvent {
    /// Commits translated into Heddle states, or commits read while
    /// `total_commits` is still unknown during the reachability pre-pass.
    pub commits_imported: usize,
    /// Final import total once known. A value of `0` with a non-final event
    /// means the importer is still counting reachable commits.
    pub total_commits: usize,
    pub states_created: usize,
}

impl<'a, R: RefBackend, S: LocalObjectStore + PackMaintenanceStoreExt> Importer<'a, R, S, OpLog> {
    pub fn new(git: &'a GitSource, store: &'a S, refs: &'a R, map: &'a mut ShaMap) -> Self {
        Self {
            git,
            store,
            refs,
            map,
            oplog: None,
            options: ImportOptions::default(),
            scope: ImportScope::all(),
            pack_staging_dir: None,
            progress: None,
        }
    }
}

impl<'a, R: RefBackend, S: LocalObjectStore + PackMaintenanceStoreExt, O: LocalOpLogBackend>
    Importer<'a, R, S, O>
{
    /// Attach an oplog backend so the importer also translates reflog
    /// entries into `OpRecord`s. Without one the import still produces a
    /// valid Heddle repo — you just don't get `heddle undo` reach past the
    /// import boundary.
    ///
    /// Rebinds the oplog type parameter to the attached backend's type.
    pub fn with_oplog<O2: LocalOpLogBackend>(self, oplog: &'a O2) -> Importer<'a, R, S, O2> {
        Importer {
            git: self.git,
            store: self.store,
            refs: self.refs,
            map: self.map,
            oplog: Some(oplog),
            options: self.options,
            scope: self.scope,
            pack_staging_dir: self.pack_staging_dir,
            progress: self.progress,
        }
    }

    pub fn with_options(mut self, options: ImportOptions) -> Self {
        self.options = options;
        self
    }

    pub fn with_scope(mut self, scope: ImportScope) -> Self {
        self.scope = scope;
        self
    }

    /// Override the directory used to stage the in-progress pack file
    /// and its index buckets. The directory is created if it doesn't
    /// exist. On a successful import the streaming pack file gets
    /// renamed into the heddle store's pack dir; the bucket subdir is
    /// always removed at finalize. On error the staged files may
    /// remain — they're keyed by a per-run basename so a re-run won't
    /// collide with them.
    pub fn with_pack_staging_dir(mut self, dir: PathBuf) -> Self {
        self.pack_staging_dir = Some(dir);
        self
    }

    pub fn with_progress(mut self, progress: &'a mut dyn FnMut(ImportProgressEvent)) -> Self {
        self.progress = Some(progress);
        self
    }

    /// Run the full import. Safe to re-invoke on the same `ShaMap` — the
    /// translators short-circuit on cache hits, so a second pass is
    /// effectively a no-op modulo any new commits since last time.
    ///
    /// `async` because ref emission awaits the backend's `async` marker
    /// read; for the local `RefManager` the future is immediately ready.
    pub async fn run(&mut self) -> crate::Result<ImportStats> {
        let (heads, refs_seen) = self.git.collect_refs_detailed()?;
        let (heads, refs_seen) = self.scope.resolve_heads(heads, refs_seen)?;
        info!(
            local_branches = refs_seen.local_branches,
            tags = refs_seen.tags,
            remote_branches = refs_seen.remote_branches,
            symbolic_skipped = refs_seen.symbolic_skipped,
            peel_failed = refs_seen.peel_failed,
            non_commit_skipped = refs_seen.non_commit_skipped,
            "collected refs"
        );

        // Seed commits = live refs + anything the reflog still mentions.
        // Reflog SHAs are filtered to those still in the odb, so this
        // can't steer us into dangling territory.
        let reflog_entries = if self.scope.is_all() {
            self.git.collect_reflog()?
        } else {
            self.git.collect_reflog_for_refs(&heads)?
        };
        let live_shas: Vec<String> = heads.iter().map(|h| h.target_sha.clone()).collect();
        let reflog_shas = self.git.reflog_commit_shas_from_entries(&reflog_entries);
        let mut seed_seen: HashSet<String> = live_shas.iter().cloned().collect();
        let reflog_only_commits = reflog_shas
            .iter()
            .filter(|s| !seed_seen.contains(*s))
            .count();

        let mut seed = live_shas;
        for s in reflog_shas {
            if seed_seen.insert(s.clone()) {
                seed.push(s);
            }
        }

        let commits = if let Some(progress) = self.progress.as_deref_mut() {
            let mut on_count = |commits_seen| {
                progress(ImportProgressEvent {
                    commits_imported: commits_seen,
                    total_commits: 0,
                    states_created: 0,
                });
            };
            self.git
                .commits_topo_with_progress(seed, Some(&mut on_count))?
        } else {
            self.git.commits_topo(seed)?
        };
        info!(commit_count = commits.len(), "topo-sorted commits");
        if let Some(progress) = self.progress.as_deref_mut() {
            progress(ImportProgressEvent {
                commits_imported: 0,
                total_commits: commits.len(),
                states_created: 0,
            });
        }

        // Translate each commit into a streaming native pack: tree first,
        // then state. Importing a git repo creates thousands of objects;
        // writing them as loose files means thousands of durable renames.
        //
        // We use [`StreamingPackBuilder`] so the pack data streams to a
        // single staging file on disk while the index entries are
        // bucketed across 512 small files (sorted at finalize). Peak
        // memory therefore stays bounded ~10s of MB regardless of
        // repo size, modulo the largest single object's compressed
        // payload (limited by the non-streaming zstd API).
        //
        // Delta search is disabled (`max_delta_size = 0`): the
        // previous delta-enabled experiment was slower than loose
        // writes on real Heddle history, and `StreamingPackBuilder`
        // can't do delta encoding anyway (it'd need random access to
        // recently-written objects).
        let staging_dir = self.pack_staging_dir.clone().unwrap_or_else(|| {
            std::env::temp_dir().join(format!("heddle-ingest-{}", std::process::id()))
        });
        std::fs::create_dir_all(&staging_dir).map_err(|e| {
            IngestError::Other(format!(
                "creating pack staging dir {}: {e}",
                staging_dir.display()
            ))
        })?;
        let run_id = format!(
            "import-{}-{}-{}",
            std::process::id(),
            IMPORT_RUN_COUNTER.fetch_add(1, Ordering::Relaxed),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
        );
        let pack_path = staging_dir.join(format!("{run_id}.pack"));
        let index_path = staging_dir.join(format!("{run_id}.idx"));
        let bucket_dir = staging_dir.join(format!("{run_id}-buckets"));

        self.map.begin_append_batch()?;
        let write_result = (|| -> crate::Result<PackedImportStats> {
            let pack_file = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(true)
                .open(&pack_path)
                .map_err(|e| {
                    IngestError::Other(format!(
                        "opening pack staging file {}: {e}",
                        pack_path.display()
                    ))
                })?;
            let compression = CompressionConfig {
                max_delta_size: 0,
                ..CompressionConfig::default()
            };
            let builder = StreamingPackBuilder::new(
                pack_file,
                index_path.clone(),
                compression,
                bucket_dir.clone(),
            )
            .map_err(IngestError::from)?;
            let mut packed = PackedImport::new(self.git, self.map, builder, self.options.clone());
            let mut last_log = 0usize;
            for (idx, commit) in commits.iter().enumerate() {
                // Canonical lossy marker (#567): translating this commit's tree
                // appends to the running lossy-entry log when an unrepresentable
                // entry is dropped/converted (even for cached subtrees). A growth
                // across the call means this commit's content is not byte-faithful
                // to the original, so record it on the State the same single way
                // the bridge `--lossy` import does.
                let lossy_before = packed.stats.lossy_entries.len();
                let tree_hash = packed.translate_tree(&commit.tree_sha)?;
                let git_lossy = packed.stats.lossy_entries.len() > lossy_before;
                packed.write_commit(commit, tree_hash, git_lossy)?;
                if let Some(progress) = self.progress.as_deref_mut() {
                    progress(ImportProgressEvent {
                        commits_imported: idx + 1,
                        total_commits: commits.len(),
                        states_created: packed.stats.states,
                    });
                }

                // Progress trace every ~500 commits keeps long imports from
                // looking hung without spamming at default `info` verbosity.
                if idx - last_log >= 500 {
                    info!(progress = idx + 1, total = commits.len(), "states written");
                    last_log = idx;
                }
            }
            let stats = packed.stats;
            if stats.object_count > 0 {
                let (_file, _) = packed.builder.finalize()?;
                self.store.install_pack_streaming(&pack_path, &index_path)?;
            } else {
                // No objects to install — drop the empty staging file.
                let _ = std::fs::remove_file(&pack_path);
                let _ = std::fs::remove_dir_all(&bucket_dir);
            }
            Ok(stats)
        })();
        let packed_stats = match write_result {
            Ok(stats) => {
                self.map.flush_append_batch()?;
                stats
            }
            Err(error) => {
                self.map.abort_append_batch();
                // Clean up the staged pack on failure so a retry
                // doesn't trip the install_pack rename's "already
                // exists" branch on a partial pack.
                let _ = std::fs::remove_file(&pack_path);
                let _ = std::fs::remove_dir_all(&bucket_dir);
                return Err(error);
            }
        };

        let ref_stats = RefEmitter::new(self.refs, self.store, self.map)
            .emit(&heads)
            .await?;
        info!(
            threads = ref_stats.threads_written,
            markers = ref_stats.markers_written,
            skipped = ref_stats.skipped_unmapped,
            "refs emitted"
        );

        // Reflog → oplog. Runs last so the oplog references only states
        // that definitely exist in the store. Skipped entirely when no
        // backend was attached (tests that don't care about undo history).
        let oplog_stats = if let Some(oplog) = self.oplog {
            let stats = OplogEmitter::new(oplog, self.map)
                .with_scope("ingest")
                .emit(&reflog_entries)?;
            info!(
                gotos = stats.gotos,
                thread_creates = stats.thread_creates,
                thread_updates = stats.thread_updates,
                thread_deletes = stats.thread_deletes,
                marker_creates = stats.marker_creates,
                marker_deletes = stats.marker_deletes,
                skipped_noop = stats.skipped_noop,
                skipped_unmapped = stats.skipped_unmapped,
                "oplog emitted"
            );
            stats
        } else {
            OplogEmitStats::default()
        };

        Ok(ImportStats {
            refs_seen,
            commits_imported: commits.len(),
            states_created: packed_stats.states,
            trees_imported: packed_stats.trees,
            blobs_imported: packed_stats.blobs,
            refs: ref_stats,
            oplog: oplog_stats,
            reflog_only_commits,
            lossy_entries: packed_stats.lossy_entries,
        })
    }
}

#[derive(Clone, Debug, Default)]
struct PackedImportStats {
    object_count: usize,
    states: usize,
    trees: usize,
    blobs: usize,
    lossy_entries: Vec<LossyImportEntry>,
}

struct PackedImport<'a, W: std::io::Write + std::io::Read + std::io::Seek> {
    git: &'a GitSource,
    map: &'a mut ShaMap,
    builder: StreamingPackBuilder<W>,
    stats: PackedImportStats,
    options: ImportOptions,
}

impl<'a, W: std::io::Write + std::io::Read + std::io::Seek> PackedImport<'a, W> {
    fn new(
        git: &'a GitSource,
        map: &'a mut ShaMap,
        builder: StreamingPackBuilder<W>,
        options: ImportOptions,
    ) -> Self {
        Self {
            git,
            map,
            builder,
            stats: PackedImportStats::default(),
            options,
        }
    }

    fn translate_tree(&mut self, git_tree_sha: &str) -> crate::Result<ContentHash> {
        self.translate_tree_at(git_tree_sha, "")
    }

    fn translate_tree_at(
        &mut self,
        git_tree_sha: &str,
        path_prefix: &str,
    ) -> crate::Result<ContentHash> {
        if let Some(hash) = self.map.get_tree(git_tree_sha) {
            let entries = self
                .map
                .get_tree_lossy_entries(git_tree_sha)
                .map_err(IngestError::from)?
                .unwrap_or_default();
            if !entries.is_empty() {
                if !self.options.lossy {
                    return Err(fail_lossy_entry(&rebase_lossy_entry(
                        path_prefix,
                        &entries[0],
                    )));
                }
                self.stats.lossy_entries.extend(
                    entries
                        .iter()
                        .map(|entry| rebase_lossy_entry(path_prefix, entry)),
                );
            }
            return Ok(hash);
        }

        let before_lossy = self.stats.lossy_entries.len();
        let children = self.git.read_tree(git_tree_sha)?;
        let mut entries = Vec::with_capacity(children.len());
        for child in children {
            if let Some(entry) = self.translate_child(&child, path_prefix)? {
                entries.push(entry);
            }
        }
        let tree_lossy_entries = self.stats.lossy_entries[before_lossy..]
            .iter()
            .map(|entry| entry_relative_to_prefix(path_prefix, entry))
            .collect::<Vec<_>>();

        let tree = Tree::from_entries(entries);
        let hash = tree.hash();
        // `to_vec_named` (struct-as-map) matches objects's convention.
        // Every reader in `store/fs/fs_impl.rs` and `store/mod.rs` calls
        // `rmp_serde::from_slice` which defaults to struct-as-map; the
        // earlier `to_vec` here produced struct-as-array bytes that
        // round-tripped through the pack but failed deserialization with
        // "invalid type: integer N, expected struct Tree".
        let data = rmp_serde::to_vec_named(&tree)
            .map_err(|e| IngestError::Other(format!("serialize tree for import pack: {e}")))?;
        self.builder.add(hash, PackObjectType::Tree, data)?;
        self.stats.object_count += 1;
        self.stats.trees += 1;

        self.map
            .insert_tree_with_lossy_entries(git_tree_sha, hash, &tree_lossy_entries)
            .map_err(IngestError::from)?;
        Ok(hash)
    }

    fn translate_child(
        &mut self,
        child: &TreeChild,
        path_prefix: &str,
    ) -> crate::Result<Option<TreeEntry>> {
        let name = match classify_git_tree_name(&child.raw_name) {
            GitTreeNameClassification::Representable(name) => name,
            GitTreeNameClassification::NeedsLossy(lossy) => {
                let path = join_tree_path(path_prefix, &lossy.name);
                let entry = match lossy.action {
                    GitTreeNameLossyAction::Dropped => {
                        LossyImportEntry::dropped(path, Some(child.sha.clone()), lossy.reason)
                    }
                    GitTreeNameLossyAction::Converted => {
                        LossyImportEntry::converted(path, Some(child.sha.clone()), lossy.reason)
                    }
                };
                self.record_lossy(entry)?;
                if matches!(lossy.action, GitTreeNameLossyAction::Dropped) {
                    return Ok(None);
                }
                lossy.name
            }
        };

        match child.kind {
            TreeChildKind::Blob { executable } => {
                let hash = self.translate_blob(&child.sha)?;
                Ok(Some(
                    TreeEntry::file(name, hash, executable)
                        .map_err(|e| IngestError::Heddle(e.into()))?,
                ))
            }
            TreeChildKind::Tree => {
                let hash =
                    self.translate_tree_at(&child.sha, &join_tree_path(path_prefix, &name))?;
                Ok(Some(
                    TreeEntry::directory(name, hash).map_err(|e| IngestError::Heddle(e.into()))?,
                ))
            }
            TreeChildKind::Symlink => {
                let hash = self.translate_blob(&child.sha)?;
                Ok(Some(
                    TreeEntry::symlink(name, hash).map_err(|e| IngestError::Heddle(e.into()))?,
                ))
            }
            TreeChildKind::Gitlink => {
                let entry = LossyImportEntry::dropped(
                    join_tree_path(path_prefix, &name),
                    Some(child.sha.clone()),
                    "gitlink/submodule entries have no Heddle tree equivalent",
                );
                self.record_lossy(entry)?;
                Ok(None)
            }
        }
    }

    fn record_lossy(&mut self, entry: LossyImportEntry) -> crate::Result<()> {
        if !self.options.lossy {
            return Err(fail_lossy_entry(&entry));
        }
        tracing::warn!(entry = %entry.summary_line(), "lossy git import accepted");
        self.stats.lossy_entries.push(entry);
        Ok(())
    }

    fn translate_blob(&mut self, git_blob_sha: &str) -> crate::Result<ContentHash> {
        if let Some(hash) = self.map.get_blob(git_blob_sha) {
            return Ok(hash);
        }

        let bytes = self.git.read_blob(git_blob_sha)?;
        let blob = Blob::from_slice(&bytes);
        let hash = blob.hash();
        self.builder.add(hash, PackObjectType::Blob, bytes)?;
        self.stats.object_count += 1;
        self.stats.blobs += 1;

        self.map
            .insert_blob(git_blob_sha, hash)
            .map_err(IngestError::from)?;
        Ok(hash)
    }

    fn write_commit(
        &mut self,
        commit: &CommitEntry,
        tree: ContentHash,
        git_lossy: bool,
    ) -> crate::Result<bool> {
        if let Some(cid) = self.map.get_commit(&commit.sha) {
            let _ = cid;
            return Ok(false);
        }

        let mut parents = Vec::with_capacity(commit.parents.len());
        for p in &commit.parents {
            match self.map.get_commit(p) {
                Some(cid) => parents.push(cid),
                None => {
                    return Err(IngestError::Other(format!(
                        "commit {} has parent {} that hasn't been translated yet — \
                         feed commits in topological order",
                        commit.sha, p
                    )));
                }
            }
        }

        let state = state_from_commit(commit, tree, parents, git_lossy)?;
        // `to_vec_named` matches objects's convention; see the longer
        // explanation in `translate_tree`.
        let data = rmp_serde::to_vec_named(&state)
            .map_err(|e| IngestError::Other(format!("serialize state for import pack: {e}")))?;
        self.builder.add_id(
            PackObjectId::ChangeId(state.change_id),
            PackObjectType::State,
            data,
        )?;
        self.stats.object_count += 1;
        self.stats.states += 1;

        self.map
            .insert_commit(&commit.sha, state.change_id)
            .map_err(IngestError::from)?;
        Ok(true)
    }
}

/// Strip a trailing `.heddle` component if present, otherwise return
/// the path unchanged. Kept lenient: a callsite that passes the
/// worktree root (recommended) is a no-op, and one that passes the
/// `.heddle` subdir (what the CLI help historically suggested) also
/// resolves to the same place.
fn strip_trailing_heddle(p: &Path) -> &Path {
    if p.file_name().map(|n| n == ".heddle").unwrap_or(false) {
        p.parent().unwrap_or(p)
    } else {
        p
    }
}

/// Convenience: open a git repo at `git_path` and a Heddle repo at
/// `heddle_path` (initializing it if missing), then run one import pass.
/// Returns both the stats and the final sha map. The map is persisted
/// under `.heddle/ingest/sha_map.sqlite`; bridge export owns its served
/// `git-bridge/bridge-mapping.json` cache separately.
///
/// `heddle_path` is the worktree root — `Repository::init` appends `.heddle`
/// itself. For tolerance with callers who pass the `.heddle`-suffixed form
/// (the old CLI help told them to), a trailing `.heddle` component is
/// stripped before the init, so both `/repo` and `/repo/.heddle` resolve
/// to the same worktree instead of producing a doubly-nested
/// `.heddle/.heddle/`.
pub fn import_git_into(
    git_path: impl AsRef<Path>,
    heddle_path: impl AsRef<Path>,
) -> crate::Result<(ImportStats, ShaMap)> {
    import_git_into_with_options(git_path, heddle_path, ImportOptions::default())
}

pub fn import_git_into_with_options(
    git_path: impl AsRef<Path>,
    heddle_path: impl AsRef<Path>,
    options: ImportOptions,
) -> crate::Result<(ImportStats, ShaMap)> {
    import_git_into_with_options_and_progress(git_path, heddle_path, options, None)
}

pub fn import_git_into_with_options_and_progress(
    git_path: impl AsRef<Path>,
    heddle_path: impl AsRef<Path>,
    options: ImportOptions,
    progress: Option<&mut dyn FnMut(ImportProgressEvent)>,
) -> crate::Result<(ImportStats, ShaMap)> {
    import_git_into_scoped_with_options_and_progress(
        git_path,
        heddle_path,
        options,
        ImportScope::all(),
        progress,
    )
}

pub fn import_git_into_scoped_with_options(
    git_path: impl AsRef<Path>,
    heddle_path: impl AsRef<Path>,
    options: ImportOptions,
    scope: ImportScope,
) -> crate::Result<(ImportStats, ShaMap)> {
    import_git_into_scoped_with_options_and_progress(git_path, heddle_path, options, scope, None)
}

pub fn import_git_into_scoped_with_options_and_progress(
    git_path: impl AsRef<Path>,
    heddle_path: impl AsRef<Path>,
    options: ImportOptions,
    scope: ImportScope,
    progress: Option<&mut dyn FnMut(ImportProgressEvent)>,
) -> crate::Result<(ImportStats, ShaMap)> {
    let git = GitSource::open(git_path)?;
    let heddle_path = heddle_path.as_ref();
    let root = strip_trailing_heddle(heddle_path);

    // Init or open — init fails if `.heddle` exists, open fails if it
    // doesn't. Try init first; if that trips the "already exists"
    // error, fall back to open.
    let repo = match repo::Repository::init(root) {
        Ok(r) => r,
        Err(objects::error::HeddleError::RepositoryExists(_)) => repo::Repository::open(root)?,
        Err(e) => return Err(e.into()),
    };

    let map_path = repo.heddle_dir().join("ingest").join("sha_map.sqlite");
    let mut map = ShaMap::open(&map_path)?;

    // Stage the streaming pack file inside `.heddle/ingest/staging/` so
    // the final `rename(2)` into `.heddle/objects/packs/` lands on the
    // same filesystem (atomic move, no copy).
    let staging_dir = repo.heddle_dir().join("ingest").join("staging");

    // `run` is `async`, but the local `RefManager`/`OpLog` futures are
    // immediately ready. `pollster::block_on` drives them to completion
    // without a Tokio runtime, so this is safe even when `import_git_into`
    // is invoked from inside the CLI's Tokio runtime. The importer is
    // scoped so its `&mut map` borrow ends before `map` is returned.
    let stats = {
        let mut importer = Importer::new(&git, repo.store(), repo.refs(), &mut map)
            .with_options(options)
            .with_scope(scope)
            .with_oplog(repo.oplog())
            .with_pack_staging_dir(staging_dir);
        if let Some(progress) = progress {
            importer = importer.with_progress(progress);
        }
        pollster::block_on(importer.run())?
    };
    Ok((stats, map))
}

#[cfg(test)]
mod tests {
    use std::{io::Write, path::Path, process::Command};

    use objects::{object::ThreadName, store::InMemoryStore};
    use refs::refs::RefManager;
    use tempfile::TempDir;

    use super::*;

    /// Seed a tiny repo with two branches and a tag so the importer has
    /// something non-trivial to chew on.
    fn seed_multibranch_repo(path: &Path) -> String {
        let run = |args: &[&str]| {
            let status = Command::new("git")
                .args(args)
                .current_dir(path)
                .env("GIT_AUTHOR_NAME", "Test")
                .env("GIT_AUTHOR_EMAIL", "test@example.com")
                .env("GIT_COMMITTER_NAME", "Test")
                .env("GIT_COMMITTER_EMAIL", "test@example.com")
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_SYSTEM", "/dev/null")
                .status()
                .expect("git cmd");
            assert!(status.success(), "git {:?} failed", args);
        };
        run(&["init", "-q", "--initial-branch=main"]);
        std::fs::write(path.join("a.txt"), "hello").unwrap();
        run(&["add", "a.txt"]);
        run(&["commit", "-q", "-m", "first"]);
        run(&["tag", "-a", "v0.1", "-m", "tag"]);
        // Side branch with one extra commit.
        run(&["checkout", "-q", "-b", "feature/x"]);
        std::fs::write(path.join("b.txt"), "world").unwrap();
        run(&["add", "b.txt"]);
        run(&["commit", "-q", "-m", "second"]);
        run(&["checkout", "-q", "main"]);

        let out = Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(path)
            .output()
            .unwrap();
        String::from_utf8(out.stdout).unwrap().trim().to_string()
    }

    fn seed_gitlink_repo(path: &Path) {
        let run = |args: &[&str]| {
            let status = Command::new("git")
                .args(args)
                .current_dir(path)
                .env("GIT_AUTHOR_NAME", "Test")
                .env("GIT_AUTHOR_EMAIL", "test@example.com")
                .env("GIT_COMMITTER_NAME", "Test")
                .env("GIT_COMMITTER_EMAIL", "test@example.com")
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_SYSTEM", "/dev/null")
                .status()
                .expect("git cmd");
            assert!(status.success(), "git {:?} failed", args);
        };
        run(&["init", "-q", "--initial-branch=main"]);
        std::fs::write(path.join("README.md"), "# hello\n").unwrap();
        run(&["add", "README.md"]);
        run(&["commit", "-q", "-m", "initial"]);
        run(&[
            "update-index",
            "--add",
            "--cacheinfo",
            "160000,0808080808080808080808080808080808080808,vendor",
        ]);
        run(&["commit", "-q", "-m", "add gitlink"]);
    }

    fn git_output(path: &Path, args: &[&str], stdin: Option<&[u8]>) -> String {
        let mut command = Command::new("git");
        command
            .args(args)
            .current_dir(path)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .env("GIT_AUTHOR_NAME", "Test")
            .env("GIT_AUTHOR_EMAIL", "test@example.com")
            .env("GIT_COMMITTER_NAME", "Test")
            .env("GIT_COMMITTER_EMAIL", "test@example.com")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null");
        if stdin.is_some() {
            command.stdin(std::process::Stdio::piped());
        }
        let mut child = command.spawn().expect("git cmd");
        if let Some(stdin) = stdin {
            child
                .stdin
                .as_mut()
                .expect("stdin")
                .write_all(stdin)
                .expect("write stdin");
        }
        let output = child.wait_with_output().expect("git output");
        assert!(
            output.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_string()
    }

    fn seed_invalid_utf8_name_repo(path: &Path) {
        let status = Command::new("git")
            .args(["init", "-q", "--initial-branch=main"])
            .current_dir(path)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .status()
            .expect("git init");
        assert!(status.success(), "git init failed");

        let blob = git_output(path, &["hash-object", "-w", "--stdin"], Some(b"hello\n"));
        let mut tree_input = Vec::new();
        write!(&mut tree_input, "100644 blob {blob}\t").expect("tree record");
        tree_input.extend_from_slice(b"bad\xffname\0");
        let tree = git_output(path, &["mktree", "-z"], Some(&tree_input));
        let commit = git_output(path, &["commit-tree", &tree, "-m", "invalid name"], None);
        git_output(path, &["update-ref", "refs/heads/main", &commit], None);
    }

    #[test]
    fn imports_commits_refs_and_tag_end_to_end() {
        let gitdir = TempDir::new().unwrap();
        let heddledir = TempDir::new().unwrap();
        let _head = seed_multibranch_repo(gitdir.path());

        let git = GitSource::open(gitdir.path()).unwrap();
        let store = InMemoryStore::new();
        let refs = RefManager::new(heddledir.path());
        refs.init().unwrap();
        let mut map = ShaMap::new();

        let stats = pollster::block_on(Importer::new(&git, &store, &refs, &mut map).run()).unwrap();

        // Two commits on main + one more on feature/x = 2 unique commits
        // (main is a prefix of feature/x), or 1+1 depending on graph.
        // Just assert at least 2 and the two refs landed.
        assert!(
            stats.commits_imported >= 2,
            "expected >=2 commits, got {}",
            stats.commits_imported
        );
        assert_eq!(stats.refs.threads_written, 2); // main + feature/x
        assert_eq!(stats.refs.markers_written, 1); // v0.1
        assert_eq!(stats.refs.skipped_unmapped, 0);
        assert!(refs.get_thread(&ThreadName::new("main")).unwrap().is_some());
        assert!(
            refs.get_thread(&ThreadName::new("feature/x"))
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn scoped_import_only_imports_selected_branch_ref() {
        let gitdir = TempDir::new().unwrap();
        let heddledir = TempDir::new().unwrap();
        seed_multibranch_repo(gitdir.path());

        let git = GitSource::open(gitdir.path()).unwrap();
        let store = InMemoryStore::new();
        let refs = RefManager::new(heddledir.path());
        refs.init().unwrap();
        let mut map = ShaMap::new();

        let stats = pollster::block_on(
            Importer::new(&git, &store, &refs, &mut map)
                .with_scope(ImportScope::refs(vec!["main".to_string()]))
                .run(),
        )
        .unwrap();

        assert_eq!(stats.refs_seen.local_branches, 1);
        assert_eq!(stats.refs_seen.tags, 0);
        assert_eq!(stats.refs.threads_written, 1);
        assert_eq!(stats.refs.markers_written, 0);
        assert_eq!(stats.commits_imported, 1);
        assert!(refs.get_thread(&ThreadName::new("main")).unwrap().is_some());
        assert!(
            refs.get_thread(&ThreadName::new("feature/x"))
                .unwrap()
                .is_none()
        );
        assert!(
            refs.get_marker(&objects::object::MarkerName::new("v0.1"))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn scoped_import_accepts_full_ref_name() {
        let gitdir = TempDir::new().unwrap();
        let heddledir = TempDir::new().unwrap();
        seed_multibranch_repo(gitdir.path());

        let git = GitSource::open(gitdir.path()).unwrap();
        let store = InMemoryStore::new();
        let refs = RefManager::new(heddledir.path());
        refs.init().unwrap();
        let mut map = ShaMap::new();

        let stats = pollster::block_on(
            Importer::new(&git, &store, &refs, &mut map)
                .with_scope(ImportScope::refs(vec!["refs/heads/main".to_string()]))
                .run(),
        )
        .unwrap();

        assert_eq!(stats.refs_seen.local_branches, 1);
        assert_eq!(stats.refs.threads_written, 1);
        assert!(refs.get_thread(&ThreadName::new("main")).unwrap().is_some());
    }

    #[test]
    fn scoped_import_errors_for_missing_ref() {
        let gitdir = TempDir::new().unwrap();
        let heddledir = TempDir::new().unwrap();
        seed_multibranch_repo(gitdir.path());

        let git = GitSource::open(gitdir.path()).unwrap();
        let store = InMemoryStore::new();
        let refs = RefManager::new(heddledir.path());
        refs.init().unwrap();
        let mut map = ShaMap::new();

        let err = pollster::block_on(
            Importer::new(&git, &store, &refs, &mut map)
                .with_scope(ImportScope::refs(vec!["missing".to_string()]))
                .run(),
        )
        .expect_err("missing scoped ref should fail");
        let message = err.to_string();

        assert!(
            message.contains("requested ref(s) not found or not commit-pointing: missing"),
            "unexpected error: {message}"
        );
    }

    #[test]
    fn import_git_into_rejects_gitlink_by_default() {
        let gitdir = TempDir::new().unwrap();
        let heddledir = TempDir::new().unwrap();
        seed_gitlink_repo(gitdir.path());

        let err = import_git_into(gitdir.path(), heddledir.path())
            .expect_err("gitlink import must fail without --lossy");
        let message = err.to_string();

        assert!(message.contains("vendor"), "error names entry: {message}");
        assert!(message.contains("--lossy"), "error names opt-in: {message}");
    }

    #[test]
    fn import_git_into_lossy_drops_gitlink_and_summarizes() {
        let gitdir = TempDir::new().unwrap();
        let heddledir = TempDir::new().unwrap();
        seed_gitlink_repo(gitdir.path());

        let (stats, _map) = import_git_into_with_options(
            gitdir.path(),
            heddledir.path(),
            ImportOptions { lossy: true },
        )
        .expect("lossy import succeeds");

        assert!(stats.commits_imported >= 2);
        assert_eq!(stats.lossy_entries.len(), 1);
        assert_eq!(stats.lossy_entries[0].path, "vendor");
        assert!(stats.lossy_entries[0].summary_line().contains("dropped"));
    }

    #[test]
    fn import_git_into_rejects_invalid_utf8_name_by_default() {
        let gitdir = TempDir::new().unwrap();
        let heddledir = TempDir::new().unwrap();
        seed_invalid_utf8_name_repo(gitdir.path());

        let err = import_git_into(gitdir.path(), heddledir.path())
            .expect_err("invalid UTF-8 name must fail without --lossy");
        let message = err.to_string();

        assert!(message.contains("bad"), "error names entry: {message}");
        assert!(
            message.contains("not valid UTF-8"),
            "error explains conversion: {message}"
        );
        assert!(message.contains("--lossy"), "error names opt-in: {message}");
    }

    #[test]
    fn import_git_into_lossy_converts_invalid_utf8_name_and_summarizes() {
        let gitdir = TempDir::new().unwrap();
        let heddledir = TempDir::new().unwrap();
        seed_invalid_utf8_name_repo(gitdir.path());

        let (stats, _map) = import_git_into_with_options(
            gitdir.path(),
            heddledir.path(),
            ImportOptions { lossy: true },
        )
        .expect("lossy import converts invalid UTF-8 name");
        let converted_name = "bad\u{fffd}name";

        assert_eq!(stats.commits_imported, 1);
        assert_eq!(stats.lossy_entries.len(), 1);
        assert_eq!(stats.lossy_entries[0].path, converted_name);
        assert!(stats.lossy_entries[0].summary_line().contains("converted"));
    }

    #[test]
    fn default_import_fails_on_cached_lossy_tree_from_prior_run() {
        let gitdir = TempDir::new().unwrap();
        let heddledir = TempDir::new().unwrap();
        seed_invalid_utf8_name_repo(gitdir.path());

        let (first, map) = import_git_into_with_options(
            gitdir.path(),
            heddledir.path(),
            ImportOptions { lossy: true },
        )
        .expect("initial lossy import succeeds");
        drop(map);
        assert_eq!(first.lossy_entries.len(), 1);

        let err = import_git_into(gitdir.path(), heddledir.path())
            .expect_err("default import must not reuse cached lossy tree silently");
        let message = err.to_string();

        assert!(
            message.contains("bad"),
            "error names cached entry: {message}"
        );
        assert!(
            message.contains("not valid UTF-8"),
            "error explains cached conversion: {message}"
        );
        assert!(message.contains("--lossy"), "error names opt-in: {message}");
    }

    #[test]
    fn lossy_import_reports_cached_lossy_tree_from_prior_run() {
        let gitdir = TempDir::new().unwrap();
        let heddledir = TempDir::new().unwrap();
        seed_invalid_utf8_name_repo(gitdir.path());

        let (_first, map) = import_git_into_with_options(
            gitdir.path(),
            heddledir.path(),
            ImportOptions { lossy: true },
        )
        .expect("initial lossy import succeeds");
        drop(map);

        let (second, _map) = import_git_into_with_options(
            gitdir.path(),
            heddledir.path(),
            ImportOptions { lossy: true },
        )
        .expect("lossy rerun reports persisted lossy entries");

        assert_eq!(second.lossy_entries.len(), 1);
        assert_eq!(second.lossy_entries[0].path, "bad\u{fffd}name");
        assert!(second.lossy_entries[0].summary_line().contains("converted"));
    }

    #[test]
    fn import_git_into_lossy_clean_repo_reports_no_lossy_entries() {
        let gitdir = TempDir::new().unwrap();
        let heddledir = TempDir::new().unwrap();
        seed_multibranch_repo(gitdir.path());

        let (stats, _map) = import_git_into_with_options(
            gitdir.path(),
            heddledir.path(),
            ImportOptions { lossy: true },
        )
        .expect("clean lossy import succeeds");

        assert!(stats.commits_imported >= 2);
        assert!(stats.lossy_entries.is_empty());
    }

    #[test]
    fn second_run_is_a_noop_for_unchanged_repo() {
        // Idempotency of the whole pipeline — key invariant for
        // incremental imports.
        let gitdir = TempDir::new().unwrap();
        let heddledir = TempDir::new().unwrap();
        let _head = seed_multibranch_repo(gitdir.path());

        let git = GitSource::open(gitdir.path()).unwrap();
        let store = InMemoryStore::new();
        let refs = RefManager::new(heddledir.path());
        refs.init().unwrap();
        let mut map = ShaMap::new();

        let first = pollster::block_on(Importer::new(&git, &store, &refs, &mut map).run()).unwrap();
        let states_after_first = store.list_states().unwrap().len();
        let second =
            pollster::block_on(Importer::new(&git, &store, &refs, &mut map).run()).unwrap();
        let states_after_second = store.list_states().unwrap().len();

        assert_eq!(first.commits_imported, second.commits_imported);
        assert_eq!(
            states_after_first, states_after_second,
            "second run minted new states — import is not idempotent"
        );
    }

    #[test]
    fn progress_reports_total_and_new_state_count() {
        let gitdir = TempDir::new().unwrap();
        let heddledir = TempDir::new().unwrap();
        seed_multibranch_repo(gitdir.path());

        let git = GitSource::open(gitdir.path()).unwrap();
        let store = InMemoryStore::new();
        let refs = RefManager::new(heddledir.path());
        refs.init().unwrap();
        let mut map = ShaMap::new();
        let mut events = Vec::new();

        let stats = {
            let mut on_progress = |event| events.push(event);
            pollster::block_on(
                Importer::new(&git, &store, &refs, &mut map)
                    .with_progress(&mut on_progress)
                    .run(),
            )
            .unwrap()
        };

        assert_eq!(
            events.first(),
            Some(&ImportProgressEvent {
                commits_imported: 0,
                total_commits: 0,
                states_created: 0,
            })
        );
        assert!(
            events.iter().any(|event| event.total_commits == 0
                && event.commits_imported == stats.commits_imported),
            "progress should report the full reachable count before the final total is known: {events:?}"
        );
        assert!(
            events.iter().any(|event| {
                let total_known = ImportProgressEvent {
                    commits_imported: 0,
                    total_commits: stats.commits_imported,
                    states_created: 0,
                };
                *event == total_known
            }),
            "progress should reset to 0 imported once the final total is known: {events:?}"
        );
        assert_eq!(
            events.last(),
            Some(&ImportProgressEvent {
                commits_imported: stats.commits_imported,
                total_commits: stats.commits_imported,
                states_created: stats.states_created,
            })
        );
        assert_eq!(stats.states_created, store.list_states().unwrap().len());
    }

    #[test]
    fn reflog_only_commits_are_still_imported() {
        let gitdir = TempDir::new().unwrap();
        let heddledir = TempDir::new().unwrap();
        seed_multibranch_repo(gitdir.path());

        // Force-reset main back one commit on feature/x so there's a
        // reflog-only tip.
        let git_cmd = |args: &[&str]| {
            Command::new("git")
                .args(args)
                .current_dir(gitdir.path())
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_SYSTEM", "/dev/null")
                .status()
                .unwrap()
        };
        assert!(git_cmd(&["checkout", "-q", "feature/x"]).success());
        assert!(git_cmd(&["reset", "--hard", "HEAD~1"]).success());
        assert!(git_cmd(&["checkout", "-q", "main"]).success());

        let git = GitSource::open(gitdir.path()).unwrap();
        let store = InMemoryStore::new();
        let refs = RefManager::new(heddledir.path());
        refs.init().unwrap();
        let mut map = ShaMap::new();

        let stats = pollster::block_on(Importer::new(&git, &store, &refs, &mut map).run()).unwrap();
        assert!(
            stats.reflog_only_commits >= 1,
            "expected reflog to rescue the dropped tip, stats={stats:?}"
        );
    }

    #[test]
    fn with_oplog_produces_thread_ops_for_every_branch() {
        // End-to-end: wire an OpLog into the importer and confirm the
        // reflog on each branch becomes a corresponding oplog entry.
        // The seed repo makes two commits on `main` and one on
        // `feature/x`, so we expect at minimum a ThreadCreate + update
        // on `main` and a ThreadCreate on `feature/x`.
        use oplog::oplog::{OpLog, OpRecord};

        let gitdir = TempDir::new().unwrap();
        let heddledir = TempDir::new().unwrap();
        let _head = seed_multibranch_repo(gitdir.path());

        let git = GitSource::open(gitdir.path()).unwrap();
        let store = InMemoryStore::new();
        let refs = RefManager::new(heddledir.path());
        refs.init().unwrap();
        let oplog = OpLog::new_unattributed(heddledir.path());
        oplog.init().unwrap();
        let mut map = ShaMap::new();

        let stats = pollster::block_on(
            Importer::new(&git, &store, &refs, &mut map)
                .with_oplog(&oplog)
                .run(),
        )
        .unwrap();

        assert_eq!(stats.oplog.skipped_unmapped, 0, "stats={stats:?}");
        assert!(
            stats.oplog.thread_creates >= 2,
            "expected create for both branches, stats={stats:?}"
        );
        assert!(
            stats.oplog.thread_updates >= 1,
            "expected main's reflog to include a commit → thread update, stats={stats:?}"
        );

        // Inspect the recorded ops directly — confirm we only emitted
        // thread/marker ops (not duplicated Snapshots from the HEAD
        // reflog).
        let recent = oplog.recent(1024).unwrap();
        assert!(!recent.is_empty(), "oplog should not be empty");
        for entry in &recent {
            match &entry.operation {
                OpRecord::ThreadCreate { .. }
                | OpRecord::ThreadUpdate { .. }
                | OpRecord::ThreadDelete { .. }
                | OpRecord::MarkerCreate { .. }
                | OpRecord::MarkerDelete { .. }
                | OpRecord::Goto { .. } => {}
                other => panic!("unexpected op kind from importer: {}", other.description()),
            }
        }
    }

    #[test]
    fn without_oplog_backend_the_oplog_stats_are_zero() {
        // Sanity: the default constructor produces no oplog side-effects.
        let gitdir = TempDir::new().unwrap();
        let heddledir = TempDir::new().unwrap();
        let _head = seed_multibranch_repo(gitdir.path());

        let git = GitSource::open(gitdir.path()).unwrap();
        let store = InMemoryStore::new();
        let refs = RefManager::new(heddledir.path());
        refs.init().unwrap();
        let mut map = ShaMap::new();

        let stats = pollster::block_on(Importer::new(&git, &store, &refs, &mut map).run()).unwrap();
        assert_eq!(stats.oplog, OplogEmitStats::default());
    }

    #[test]
    fn imported_states_round_trip_through_the_pack_reader() {
        // Regression for a real production bug found via dogfood:
        // the streaming pack import was serializing State and Tree with
        // `rmp_serde::to_vec` (struct-as-array), but every reader in
        // objects uses `rmp_serde::from_slice` which defaults to
        // struct-as-map. The pack round-tripped the bytes faithfully,
        // but `Repository::store().get_state(...)` came back with
        // "invalid type: integer N, expected struct State" — meaning a
        // freshly imported repo couldn't service `heddle log`, `heddle
        // show`, or anything else that touches state objects.
        //
        // The fix is `to_vec_named` in `translate_tree` and
        // `write_commit`. This test covers the full end-to-end flow
        // (FS-backed store + streaming pack + read back) so the bug
        // can't sneak back in via the in-memory test path.
        let gitdir = TempDir::new().unwrap();
        let heddledir = TempDir::new().unwrap();
        let _head = seed_multibranch_repo(gitdir.path());

        let (stats, _map) = import_git_into(gitdir.path(), heddledir.path()).unwrap();
        assert!(stats.commits_imported >= 2);

        // Open the Heddle repo the way the CLI does and walk every
        // imported state through the store. Any deserialization error
        // surfaces here.
        let repo = repo::Repository::open(heddledir.path()).unwrap();
        let store = repo.store();
        let main_cid = repo
            .refs()
            .get_thread(&ThreadName::new("main"))
            .unwrap()
            .expect("main thread should resolve to a state");
        let state = store
            .get_state(&main_cid)
            .expect("get_state must succeed for an imported commit")
            .expect("imported main state should exist in the store");
        assert_eq!(
            state.change_id, main_cid,
            "round-tripped state's change id should match the ref target"
        );

        // Walk parents — exercises a second state read and confirms the
        // graph edge fields survived the round-trip too.
        for parent_cid in &state.parents {
            store
                .get_state(parent_cid)
                .expect("parent state read must succeed")
                .expect("parent state should exist in the store");
        }

        // The state's tree must also round-trip — same bug, same fix.
        let tree = store
            .get_tree(&state.tree)
            .expect("get_tree must succeed for an imported tree")
            .expect("tree should exist in the store");
        assert!(
            !tree.entries().is_empty(),
            "imported tree must contain at least one entry (a.txt or b.txt)"
        );
    }

    #[test]
    fn import_git_into_git_overlay_persists_ingest_mapping_without_bridge_cache_or_mirror() {
        let gitdir = TempDir::new().unwrap();
        seed_multibranch_repo(gitdir.path());

        let (stats, map) = import_git_into(gitdir.path(), gitdir.path()).unwrap();

        assert!(stats.commits_imported >= 2);
        assert_eq!(stats.states_created, map.commit_shas().len());
        let map_path = gitdir
            .path()
            .join(".heddle")
            .join("ingest")
            .join("sha_map.sqlite");
        assert!(map_path.is_file(), "ingest SHA map is missing");
        let reloaded = ShaMap::open(&map_path).unwrap();
        assert_eq!(reloaded.commit_shas().len(), map.commit_shas().len());
        for git_oid in map.commit_shas() {
            assert_eq!(reloaded.get_commit(&git_oid), map.get_commit(&git_oid));
        }

        let bridge_mapping_path = gitdir
            .path()
            .join(".heddle")
            .join("git-bridge")
            .join("bridge-mapping.json");
        assert!(
            !bridge_mapping_path.exists(),
            "ingest import must not publish the served bridge mapping cache"
        );
        assert!(
            !gitdir.path().join(".heddle").join("git").exists(),
            "ingest-backed import must not create the legacy internal Git mirror"
        );
    }

    #[test]
    fn import_git_into_tolerates_trailing_dot_heddle() {
        // The old CLI help told users to pass `--heddle <repo>/.heddle`,
        // which the underlying `Repository::init` would naively expand
        // into `<repo>/.heddle/.heddle`. Guard against the doubly-nested
        // layout: passing either the worktree root or its `.heddle` subdir
        // must land the repo at `<root>/.heddle` and nowhere else.
        let gitdir = TempDir::new().unwrap();
        let heddledir = TempDir::new().unwrap();
        seed_multibranch_repo(gitdir.path());

        let dot_heddle = heddledir.path().join(".heddle");
        let (_stats, _map) = import_git_into(gitdir.path(), &dot_heddle).unwrap();

        assert!(
            dot_heddle.is_dir(),
            ".heddle directory should be created at the requested path"
        );
        assert!(
            dot_heddle.join("objects").is_dir(),
            "expected `.heddle/objects` — got a malformed layout"
        );
        assert!(
            !dot_heddle.join(".heddle").exists(),
            "must not create a nested `.heddle/.heddle/` when caller passes a `.heddle`-suffixed path"
        );
    }
}
