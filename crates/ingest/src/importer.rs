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
    object::{
        AnnotatedTag, AnnotatedTagMarker, Blob, ContentHash, HeddleNote, TreeEntry,
        thread_replication::git_import_graph::{
            ImportRefDisposition, ImportSkipReason, SkippedImportRef, classify_git_import_ref,
        },
    },
    store::{
        CompressionConfig, ObjectStore,
        pack::{ObjectType as PackObjectType, PackBuilder, PackObjectId, StreamingPackBuilder},
    },
    util::{GitTreeNameClassification, GitTreeNameLossyAction, classify_git_tree_name},
};
use oplog::oplog::{OpLog, OpLogBackend};
use refs::refs::RefBackend;
use tracing::info;

use crate::{
    IngestError,
    git_walk::{
        CommitEntry, GitSource, RefDiscoveryStats, RefHead, RefNamespace, TreeChild, TreeChildKind,
        git_tree_from_entries, reject_reserved_root_entries, reserved_tree_entry_error,
    },
    import_options::{
        ImportOptions, LossyImportEntry, entry_relative_to_prefix, fail_lossy_entry,
        join_tree_path, rebase_lossy_entry,
    },
    oplog_emit::{OplogEmitStats, OplogEmitter},
    ref_emit::{RefEmitStats, RefEmitter},
    sha_map::ShaMap,
    state_writer::state_from_commit_with_rewrites,
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
    /// Names and reasons for Git refs that could not become native refs.
    pub skipped_refs: Vec<SkippedImportRef>,
    /// Commits translated (live + reflog-only).
    pub commits_imported: usize,
    /// New Heddle states written during this import. Re-runs can inspect the
    /// same commits while creating zero new states.
    pub states_created: usize,
    /// Embedded State identities rebuilt to match actual Git parents.
    pub native_identity_changes: usize,
    /// Time spent installing the native pack and publishing its authoritative
    /// loose state bodies.
    pub state_store_write_ms: u128,
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
    /// Original Git commits whose native States cannot reconstruct their
    /// mapped bytes (lossy trees or non-UTF-8 identities). Git projection uses
    /// these exact roots to capture Raw Git Object Residual closures.
    pub non_reconstructable_commits: Vec<String>,
    /// Original Git trees at which a lossy path conversion occurred. This is
    /// retained as import evidence even though residual capture walks the full
    /// tree closure from each commit root.
    pub lossy_trees: Vec<String>,
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
pub struct Importer<'a, R: RefBackend, S: ObjectStore, O: OpLogBackend = OpLog> {
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

impl<'a, R: RefBackend, S: ObjectStore> Importer<'a, R, S, OpLog> {
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

impl<'a, R: RefBackend, S: ObjectStore, O: OpLogBackend> Importer<'a, R, S, O> {
    /// Attach an oplog backend so the importer also translates reflog
    /// entries into `OpRecord`s. Without one the import still produces a
    /// valid Heddle repo — you just don't get `heddle undo` reach past the
    /// import boundary.
    ///
    /// Rebinds the oplog type parameter to the attached backend's type.
    pub fn with_oplog<O2: OpLogBackend>(self, oplog: &'a O2) -> Importer<'a, R, S, O2> {
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
        let frozen_refs = self.git.collect_frozen_import_refs()?;
        let (mut heads, refs_seen) = self.git.collect_refs_from_frozen(&frozen_refs)?;
        let emitted_remote_names: HashSet<String> = heads
            .iter()
            .filter(|head| head.namespace == RefNamespace::RemoteBranch)
            .map(|head| head.full_name.clone())
            .collect();
        let mut supported = HashSet::new();
        let mut skipped_refs = Vec::new();
        for reference in &frozen_refs {
            if reference.raw_name == b"HEAD"
                || (std::str::from_utf8(&reference.raw_name).is_err()
                    && !reference.raw_name.starts_with(b"refs/heads/")
                    && !reference.raw_name.starts_with(b"refs/tags/"))
            {
                continue;
            }
            let disposition = classify_git_import_ref(reference).map_err(|error| {
                IngestError::Git(format!(
                    "classify Git ref {}: {error:?}",
                    String::from_utf8_lossy(&reference.raw_name)
                ))
            })?;
            match disposition {
                ImportRefDisposition::Branch | ImportRefDisposition::CommitTag => {
                    supported.insert(reference.raw_name.clone());
                }
                ImportRefDisposition::Unsupported {
                    reason: ImportSkipReason::RemoteTracking,
                } if std::str::from_utf8(&reference.raw_name)
                    .is_ok_and(|name| emitted_remote_names.contains(name)) =>
                {
                    supported.insert(reference.raw_name.clone());
                }
                ImportRefDisposition::DefaultHead
                | ImportRefDisposition::RequiredNotes
                | ImportRefDisposition::Unsupported {
                    reason: ImportSkipReason::Pull,
                } => {}
                ImportRefDisposition::Unsupported { reason } => {
                    skipped_refs.push(SkippedImportRef {
                        raw_name: reference.raw_name.clone(),
                        reason,
                    })
                }
            }
        }
        heads.retain(|head| supported.contains(head.full_name.as_bytes()));
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
            let mut entries = Vec::new();
            self.git
                .collect_reflog_from_frozen(&frozen_refs, &mut entries)?;
            entries
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

        // Translate each commit into one native pack: tree first, then state.
        // Importing a git repo creates thousands of objects; writing them as
        // loose files means thousands of durable renames.
        //
        // We use [`StreamingPackBuilder`] so the pack data streams to a
        // single staging file on disk while the index entries are
        // bucketed across 768 small files (sorted at finalize). Peak
        // memory therefore stays bounded ~10s of MB regardless of
        // repo size, modulo the largest single object's compressed
        // payload (limited by the non-streaming zstd API).
        //
        // The default remains the bounded-memory streaming builder. The
        // repository's import delta-search policy opts into `PackBuilder`,
        // which retains the full object set so it can search recent bases.
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
        let overlay_descriptor_dir = staging_dir
            .parent()
            .map(|parent| parent.join("overlay-states"));
        let mut descriptor_commits = Vec::new();
        for commit in &commits {
            let Some(state) = self.map.get_commit(&commit.sha)? else {
                continue;
            };
            if overlay_descriptor_dir.as_ref().is_some_and(|dir| {
                dir.join(format!("{}.state", state.to_string_full()))
                    .is_file()
            }) {
                descriptor_commits.push((commit.sha.clone(), state));
            }
        }
        let repair_mapped_objects = !descriptor_commits.is_empty();
        let mut remapped_commits = descriptor_commits.clone();
        if self.options.root_parent.is_some() {
            remapped_commits.clear();
            for commit in &commits {
                if let Some(state) = self.map.get_commit(&commit.sha)? {
                    remapped_commits.push((commit.sha.clone(), state));
                }
            }
        }

        let remapped_shas: HashSet<&str> = remapped_commits
            .iter()
            .map(|(git_sha, _)| git_sha.as_str())
            .collect();
        for commit in &commits {
            // These mappings are removed below and their rebuilt States claim
            // identities in topological order. Overlay descriptors may not
            // have a readable State body yet.
            if remapped_shas.contains(commit.sha.as_str()) {
                continue;
            }
            if let Some(cid) = self.map.get_commit(&commit.sha)? {
                let state = self.store.get_state(&cid)?.ok_or_else(|| {
                    IngestError::Other(format!(
                        "mapped state {cid} for Git commit {} is missing",
                        commit.sha
                    ))
                })?;
                self.map.claim_change_id(&commit.sha, state.change_id)?;
            }
        }

        self.map.begin_append_batch()?;
        let write_result = (|| -> crate::Result<PackedImportStats> {
            for (git_sha, _) in &remapped_commits {
                self.map.remove_commit(git_sha)?;
            }
            let builder = ImportPackBuilder::new(
                &pack_path,
                &index_path,
                &bucket_dir,
                self.options.delta_search,
            )?;
            let mut packed = PackedImport::new(self.git, self.map, builder, self.options.clone())
                .repair_mapped_objects(repair_mapped_objects);
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
                packed.write_commit_with_parent_policy(
                    commit,
                    tree_hash,
                    git_lossy,
                    ParentMapPolicy::RequireMapped,
                )?;
                if git_lossy {
                    packed
                        .stats
                        .non_reconstructable_commits
                        .insert(commit.sha.clone());
                }
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
            packed.write_annotated_tags(&heads)?;
            let mut stats = packed.stats;
            if stats.object_count > 0 {
                let state_store_write_start = std::time::Instant::now();
                packed.builder.finish(self.store, &pack_path, &index_path)?;
                stats.state_store_write_ms = state_store_write_start.elapsed().as_millis();
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

        let mut state_remaps = Vec::new();
        for (git_sha, old_state) in &remapped_commits {
            if let Some(new_state) = self.map.get_commit(git_sha)?
                && new_state != *old_state
            {
                state_remaps.push((*old_state, new_state));
            }
        }

        let ref_stats = RefEmitter::new(self.refs, self.store, self.map)
            .with_state_remaps(state_remaps)
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
            skipped_refs,
            commits_imported: commits.len(),
            states_created: packed_stats.states,
            native_identity_changes: packed_stats.native_identity_changes,
            state_store_write_ms: packed_stats.state_store_write_ms,
            trees_imported: packed_stats.trees,
            blobs_imported: packed_stats.blobs,
            refs: ref_stats,
            oplog: oplog_stats,
            reflog_only_commits,
            lossy_entries: packed_stats.lossy_entries,
            non_reconstructable_commits: {
                let mut commits = packed_stats
                    .non_reconstructable_commits
                    .into_iter()
                    .collect::<Vec<_>>();
                commits.sort();
                commits
            },
            lossy_trees: {
                let mut trees = packed_stats.lossy_trees.into_iter().collect::<Vec<_>>();
                trees.sort();
                trees
            },
        })
    }
}

#[derive(Clone, Debug, Default)]
struct PackedImportStats {
    object_count: usize,
    states: usize,
    native_identity_changes: usize,
    state_store_write_ms: u128,
    trees: usize,
    blobs: usize,
    lossy_entries: Vec<LossyImportEntry>,
    non_reconstructable_commits: HashSet<String>,
    lossy_trees: HashSet<String>,
}

trait ImportPackSink {
    fn add(
        &mut self,
        hash: ContentHash,
        obj_type: PackObjectType,
        data: Vec<u8>,
    ) -> crate::Result<()>;

    fn add_id(
        &mut self,
        id: PackObjectId,
        obj_type: PackObjectType,
        data: Vec<u8>,
    ) -> crate::Result<()>;
}

impl<W> ImportPackSink for StreamingPackBuilder<W>
where
    W: std::io::Write + std::io::Read + std::io::Seek + objects::store::SyncData,
{
    fn add(
        &mut self,
        hash: ContentHash,
        obj_type: PackObjectType,
        data: Vec<u8>,
    ) -> crate::Result<()> {
        StreamingPackBuilder::add(self, hash, obj_type, data).map_err(IngestError::from)
    }

    fn add_id(
        &mut self,
        id: PackObjectId,
        obj_type: PackObjectType,
        data: Vec<u8>,
    ) -> crate::Result<()> {
        StreamingPackBuilder::add_id(self, id, obj_type, data).map_err(IngestError::from)
    }
}

impl ImportPackSink for PackBuilder {
    fn add(
        &mut self,
        hash: ContentHash,
        obj_type: PackObjectType,
        data: Vec<u8>,
    ) -> crate::Result<()> {
        PackBuilder::add(self, hash, obj_type, data);
        Ok(())
    }

    fn add_id(
        &mut self,
        id: PackObjectId,
        obj_type: PackObjectType,
        data: Vec<u8>,
    ) -> crate::Result<()> {
        PackBuilder::add_id(self, id, obj_type, data);
        Ok(())
    }
}

enum ImportPackBuilder {
    Streaming(StreamingPackBuilder<std::fs::File>),
    DeltaSearch(PackBuilder),
}

impl ImportPackBuilder {
    fn new(
        pack_path: &Path,
        index_path: &Path,
        bucket_dir: &Path,
        delta_search: bool,
    ) -> crate::Result<Self> {
        if delta_search {
            return Ok(Self::DeltaSearch(PackBuilder::new(
                CompressionConfig::default(),
            )));
        }

        let pack_file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(pack_path)
            .map_err(|error| {
                IngestError::Other(format!(
                    "opening pack staging file {}: {error}",
                    pack_path.display()
                ))
            })?;
        let compression = CompressionConfig {
            max_delta_size: 0,
            ..CompressionConfig::default()
        };
        let builder = StreamingPackBuilder::new(
            pack_file,
            index_path.to_path_buf(),
            compression,
            bucket_dir.to_path_buf(),
        )?;
        Ok(Self::Streaming(builder))
    }

    fn finish<S: ObjectStore>(
        self,
        store: &S,
        pack_path: &Path,
        index_path: &Path,
    ) -> crate::Result<()> {
        match self {
            Self::Streaming(builder) => {
                let (_file, _) = builder.finalize()?;
                store.install_pack_streaming(pack_path, index_path)?;
            }
            Self::DeltaSearch(builder) => {
                let (pack, index, _) = builder.build()?;
                store.install_pack(&pack, &index)?;
            }
        }
        Ok(())
    }
}

impl ImportPackSink for ImportPackBuilder {
    fn add(
        &mut self,
        hash: ContentHash,
        obj_type: PackObjectType,
        data: Vec<u8>,
    ) -> crate::Result<()> {
        match self {
            Self::Streaming(builder) => ImportPackSink::add(builder, hash, obj_type, data),
            Self::DeltaSearch(builder) => ImportPackSink::add(builder, hash, obj_type, data),
        }
    }

    fn add_id(
        &mut self,
        id: PackObjectId,
        obj_type: PackObjectType,
        data: Vec<u8>,
    ) -> crate::Result<()> {
        match self {
            Self::Streaming(builder) => ImportPackSink::add_id(builder, id, obj_type, data),
            Self::DeltaSearch(builder) => ImportPackSink::add_id(builder, id, obj_type, data),
        }
    }
}

struct PackedImport<'a, B: ImportPackSink> {
    git: &'a GitSource,
    map: &'a mut ShaMap,
    builder: B,
    stats: PackedImportStats,
    options: ImportOptions,
    emit_objects: bool,
    repair_mapped_objects: bool,
    materialized_trees: HashSet<String>,
    materialized_blobs: HashSet<String>,
}

impl<'a, B: ImportPackSink> PackedImport<'a, B> {
    fn new(git: &'a GitSource, map: &'a mut ShaMap, builder: B, options: ImportOptions) -> Self {
        Self {
            git,
            map,
            builder,
            stats: PackedImportStats::default(),
            options,
            emit_objects: true,
            repair_mapped_objects: false,
            materialized_trees: HashSet::new(),
            materialized_blobs: HashSet::new(),
        }
    }

    fn mapping_only(mut self) -> Self {
        self.emit_objects = false;
        self
    }

    fn repair_mapped_objects(mut self, repair: bool) -> Self {
        self.repair_mapped_objects = repair;
        self
    }

    fn translate_tree(&mut self, git_tree_sha: &str) -> crate::Result<ContentHash> {
        self.translate_tree_at(git_tree_sha, "")
    }

    fn translate_tree_at(
        &mut self,
        git_tree_sha: &str,
        path_prefix: &str,
    ) -> crate::Result<ContentHash> {
        if let Some(hash) = self.map.get_tree(git_tree_sha)?
            && (!self.repair_mapped_objects
                || !self.materialized_trees.insert(git_tree_sha.to_string()))
        {
            if path_prefix.is_empty() {
                // A tree first translated as a subtree may hold a `.heddle`,
                // which is reserved only once the tree is a commit's root.
                reject_reserved_root_entries(self.git, git_tree_sha)?;
            }
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
                self.stats.lossy_trees.insert(git_tree_sha.to_string());
            }
            return Ok(hash);
        }
        self.materialized_trees.insert(git_tree_sha.to_string());

        let before_lossy = self.stats.lossy_entries.len();
        let children = self.git.read_tree(git_tree_sha)?;
        let mut entries = Vec::with_capacity(children.len());
        for child in children {
            if let Some(entry) = self.translate_child(git_tree_sha, &child, path_prefix)? {
                entries.push(entry);
            }
        }
        let tree_lossy_entries = self.stats.lossy_entries[before_lossy..]
            .iter()
            .map(|entry| entry_relative_to_prefix(path_prefix, entry))
            .collect::<Vec<_>>();
        if !tree_lossy_entries.is_empty() {
            self.stats.lossy_trees.insert(git_tree_sha.to_string());
        }

        let tree = git_tree_from_entries(git_tree_sha, entries)?;
        let hash = tree.hash();
        let data = tree
            .encode_canonical()
            .map_err(|e| IngestError::Other(format!("serialize tree for import pack: {e}")))?;
        if self.emit_objects {
            self.builder.add(hash, PackObjectType::Tree, data)?;
            self.stats.object_count += 1;
        }
        self.stats.trees += 1;

        self.map
            .insert_tree_with_lossy_entries(git_tree_sha, hash, &tree_lossy_entries)
            .map_err(IngestError::from)?;
        Ok(hash)
    }

    fn translate_child(
        &mut self,
        git_tree_sha: &str,
        child: &TreeChild,
        path_prefix: &str,
    ) -> crate::Result<Option<TreeEntry>> {
        let name = match classify_git_tree_name(&child.raw_name, path_prefix.is_empty()) {
            GitTreeNameClassification::Representable(name) => name,
            GitTreeNameClassification::Reserved(reason) => {
                return Err(reserved_tree_entry_error(
                    git_tree_sha,
                    path_prefix,
                    child,
                    reason,
                ));
            }
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

        let entry = match child.kind {
            TreeChildKind::Blob { executable } => {
                let hash = self.translate_blob(&child.sha)?;
                TreeEntry::file(name, hash, executable)
            }
            TreeChildKind::Tree => {
                let hash =
                    self.translate_tree_at(&child.sha, &join_tree_path(path_prefix, &name))?;
                TreeEntry::directory(name, hash)
            }
            TreeChildKind::Symlink => {
                let hash = self.translate_blob(&child.sha)?;
                TreeEntry::symlink(name, hash)
            }
            TreeChildKind::Gitlink => {
                let target = sley::ObjectId::from_hex(self.git.object_format(), &child.sha)
                    .map_err(|err| {
                        IngestError::Git(format!("parse gitlink {}: {err}", child.sha))
                    })?;
                TreeEntry::gitlink(name, target)
            }
        }
        .and_then(|entry| entry.with_raw_git_mode(child.mode))
        .map_err(|e| IngestError::Heddle(e.into()))?;
        Ok(Some(entry))
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
        if let Some(hash) = self.map.get_blob(git_blob_sha)?
            && (!self.repair_mapped_objects
                || !self.materialized_blobs.insert(git_blob_sha.to_string()))
        {
            return Ok(hash);
        }
        self.materialized_blobs.insert(git_blob_sha.to_string());

        let bytes = self.git.read_blob(git_blob_sha)?;
        let blob = Blob::from_slice(&bytes);
        let hash = blob.hash();
        if self.emit_objects {
            self.builder.add(hash, PackObjectType::Blob, bytes)?;
            self.stats.object_count += 1;
        }
        self.stats.blobs += 1;

        self.map
            .insert_blob(git_blob_sha, hash)
            .map_err(IngestError::from)?;
        Ok(hash)
    }

    fn write_annotated_tags(&mut self, heads: &[RefHead]) -> crate::Result<()> {
        for head in heads.iter().filter(|head| {
            head.namespace == RefNamespace::Tag && head.object_sha != head.target_sha
        }) {
            let peeled_state = self.map.get_commit(&head.target_sha)?.ok_or_else(|| {
                IngestError::Other(format!(
                    "annotated tag {} peels to unmapped commit {}",
                    head.full_name, head.target_sha
                ))
            })?;
            let chain = self.git.read_annotated_tag_chain(&head.object_sha)?;
            let outer_index = chain.len().checked_sub(1).ok_or_else(|| {
                IngestError::Git(format!(
                    "ref {} changed while importing and no longer names an annotated tag",
                    head.full_name
                ))
            })?;
            let mut target_tag = None;
            for (index, entry) in chain.into_iter().enumerate() {
                let marker = (index == outer_index).then(|| AnnotatedTagMarker {
                    name: head.short_name.clone(),
                    peeled_state,
                });
                let tag =
                    AnnotatedTag::new(self.git.object_format(), entry.body, target_tag, marker)
                        .map_err(|error| IngestError::Git(error.to_string()))?;
                let hash = tag.hash();
                self.builder.add_id(
                    PackObjectId::AnnotatedTag(hash),
                    PackObjectType::AnnotatedTag,
                    tag.encode_current_msgpack(),
                )?;
                self.stats.object_count += 1;
                target_tag = Some(hash);
            }
        }
        Ok(())
    }

    /// Write a commit state and return its physical Heddle state id (existing or new).
    fn write_commit_with_parent_policy(
        &mut self,
        commit: &CommitEntry,
        tree: ContentHash,
        git_lossy: bool,
        parent_policy: ParentMapPolicy,
    ) -> crate::Result<objects::object::StateId> {
        if let Some(cid) = self.map.get_commit(&commit.sha)? {
            return Ok(cid);
        }

        let mut parents = Vec::with_capacity(
            commit.parents.len() + usize::from(self.options.root_parent.is_some()),
        );
        for p in &commit.parents {
            match self.map.get_commit(p)? {
                Some(cid) => parents.push(cid),
                None => match parent_policy {
                    ParentMapPolicy::RequireMapped => {
                        return Err(IngestError::Other(format!(
                            "commit {} has parent {} that hasn't been translated yet — \
                             feed commits in topological order",
                            commit.sha, p
                        )));
                    }
                    // Single-tip bind: treat unmapped git parents as absent so
                    // the tip becomes a Heddle root while remaining mapped to
                    // the real git OID (export of children still parents onto
                    // that OID).
                    ParentMapPolicy::OrphanUnmapped => {}
                },
            }
        }
        if commit.parents.is_empty()
            && let Some(root_parent) = self.options.root_parent
        {
            parents.push(root_parent);
        }

        let state =
            state_from_commit_with_rewrites(commit, tree, parents, git_lossy, |original| {
                self.map.get_rewritten_state(original).map_err(Into::into)
            })?;
        self.map.claim_change_id(&commit.sha, state.change_id)?;
        if let Some(note_bytes) = &commit.heddle_note {
            let note = HeddleNote::from_json_bytes(note_bytes).map_err(|error| {
                IngestError::Git(format!(
                    "parse Heddle note for commit {}: {error}",
                    commit.sha
                ))
            })?;
            if let Some(original) = note.source_state
                && original.id() != state.id()
            {
                self.map.record_rewritten_state(original.id(), state.id())?;
                self.stats.native_identity_changes += 1;
            }
        }
        let data = state
            .encode_current_msgpack()
            .map_err(|e| IngestError::Other(format!("serialize state for import pack: {e}")))?;
        if self.emit_objects {
            self.builder.add_id(
                PackObjectId::StateId(state.id()),
                PackObjectType::State,
                data,
            )?;
            self.stats.object_count += 1;
        }
        self.stats.states += 1;

        self.map
            .insert_commit(&commit.sha, state.state_id)
            .map_err(IngestError::from)?;
        Ok(state.state_id)
    }
}

/// How [`PackedImport::write_commit_with_parent_policy`] resolves git parents.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ParentMapPolicy {
    /// Full history import: every parent must already be in the sha map.
    RequireMapped,
    /// Single-tip lazy bind: drop unmapped parents (tip becomes a Heddle root).
    OrphanUnmapped,
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
/// under `.heddle/ingest/sha_map.sqlite`; Git projection export owns its served
/// `git-projection/git-projection-mapping.json` cache separately.
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
    import_git_into_scoped_with_options_and_progress(
        git_path,
        heddle_path,
        options,
        ImportScope::all(),
        None,
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

    // Stage streaming packs inside `.heddle/ingest/staging/` so the final
    // `rename(2)` into `.heddle/packs/` lands on the same filesystem.
    // Delta-search imports use the buffered builder and install from memory.
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

/// Lazily bind a single Git commit tip into Heddle without walking ancestors.
///
/// For ordinary Git commits the tip is translated as a Heddle root because its
/// Git parents have not been mapped. A portable Heddle export note instead
/// preserves its embedded source State and parent identities exactly; a later
/// full [`import_git_into`] / `heddle import local` validates and materializes that
/// graph. The mapping always records the real Git OID for later export.
///
/// Returns the mapped Heddle state id for `git_sha`. Idempotent when the tip
/// is already mapped.
pub fn import_single_git_commit_into(
    git_path: impl AsRef<Path>,
    heddle_path: impl AsRef<Path>,
    git_sha: &str,
    options: ImportOptions,
) -> crate::Result<objects::object::StateId> {
    let git = GitSource::open(git_path)?;
    let heddle_path = heddle_path.as_ref();
    let root = strip_trailing_heddle(heddle_path);

    let repo = match repo::Repository::init(root) {
        Ok(r) => r,
        Err(objects::error::HeddleError::RepositoryExists(_)) => repo::Repository::open(root)?,
        Err(e) => return Err(e.into()),
    };

    let map_path = repo.heddle_dir().join("ingest").join("sha_map.sqlite");
    let mut map = ShaMap::open(&map_path)?;
    if let Some(existing) = map.get_commit(git_sha)? {
        // Already bound — still verify the state object is present.
        if repo.store().get_state(&existing)?.is_some() {
            return Ok(existing);
        }
        return Err(IngestError::Other(format!(
            "Git commit {git_sha} is mapped to missing Heddle state {}; run a full Git adoption to repair the mapping",
            existing.to_string_full()
        )));
    }

    let commit = git.read_commit(git_sha)?;
    let staging_dir = repo.heddle_dir().join("ingest").join("staging");
    std::fs::create_dir_all(&staging_dir).map_err(|e| {
        IngestError::Other(format!(
            "creating pack staging dir {}: {e}",
            staging_dir.display()
        ))
    })?;
    let run_id = format!(
        "tip-{}-{}-{}",
        std::process::id(),
        IMPORT_RUN_COUNTER.fetch_add(1, Ordering::Relaxed),
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
    );
    let pack_path = staging_dir.join(format!("{run_id}.pack"));
    let index_path = staging_dir.join(format!("{run_id}.idx"));
    let bucket_dir = staging_dir.join(format!("{run_id}-buckets"));

    map.begin_append_batch()?;
    let write_result = (|| -> crate::Result<objects::object::StateId> {
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
        let mut packed = PackedImport::new(&git, &mut map, builder, options);
        let lossy_before = packed.stats.lossy_entries.len();
        let tree_hash = packed.translate_tree(&commit.tree_sha)?;
        let git_lossy = packed.stats.lossy_entries.len() > lossy_before;
        let change_id = packed.write_commit_with_parent_policy(
            &commit,
            tree_hash,
            git_lossy,
            ParentMapPolicy::OrphanUnmapped,
        )?;
        if packed.stats.object_count > 0 {
            let (_file, _) = packed.builder.finalize()?;
            repo.store()
                .install_pack_streaming(&pack_path, &index_path)?;
        } else {
            let _ = std::fs::remove_file(&pack_path);
            let _ = std::fs::remove_dir_all(&bucket_dir);
        }
        Ok(change_id)
    })();

    match write_result {
        Ok(change_id) => {
            map.flush_append_batch()?;
            Ok(change_id)
        }
        Err(error) => {
            map.abort_append_batch();
            let _ = std::fs::remove_file(&pack_path);
            let _ = std::fs::remove_dir_all(&bucket_dir);
            Err(error)
        }
    }
}

/// Bind one Git commit as an overlay-native Heddle state without copying its
/// source tree or blobs into the native object store.
///
/// Translation reuses the full importer's canonical tree/blob mapping logic,
/// but emits only identity rows plus a compact state descriptor. Object reads
/// subsequently resolve through the authoritative `.git` database.
pub fn bind_single_git_commit_overlay(
    git_path: impl AsRef<Path>,
    heddle_path: impl AsRef<Path>,
    git_sha: &str,
    options: ImportOptions,
) -> crate::Result<objects::object::StateId> {
    let git = GitSource::open(git_path)?;
    let root = strip_trailing_heddle(heddle_path.as_ref());
    let repo = repo::Repository::open(root)?;
    let map_path = repo.heddle_dir().join("ingest").join("sha_map.sqlite");
    let mut map = ShaMap::open(&map_path)?;
    if let Some(existing) = map.get_commit(git_sha)? {
        if repo.store().get_state(&existing)?.is_some() {
            return Ok(existing);
        }
        return Err(IngestError::Other(format!(
            "Git commit {git_sha} maps to unreadable Heddle state {}",
            existing.to_string_full()
        )));
    }

    let commit = git.read_commit(git_sha)?;
    let staging_dir = repo.heddle_dir().join("ingest").join("staging");
    std::fs::create_dir_all(&staging_dir)?;
    let run_id = format!(
        "overlay-tip-{}-{}",
        std::process::id(),
        IMPORT_RUN_COUNTER.fetch_add(1, Ordering::Relaxed)
    );
    let pack_path = staging_dir.join(format!("{run_id}.pack"));
    let index_path = staging_dir.join(format!("{run_id}.idx"));
    let bucket_dir = staging_dir.join(format!("{run_id}-buckets"));
    let pack_file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(&pack_path)?;
    let builder = StreamingPackBuilder::new(
        pack_file,
        index_path.clone(),
        CompressionConfig {
            max_delta_size: 0,
            ..CompressionConfig::default()
        },
        bucket_dir.clone(),
    )
    .map_err(IngestError::from)?;

    map.begin_append_batch()?;
    let mut state_path = None;
    let result = (|| -> crate::Result<objects::object::StateId> {
        let mut packed = PackedImport::new(&git, &mut map, builder, options).mapping_only();
        let lossy_before = packed.stats.lossy_entries.len();
        let tree_hash = packed.translate_tree(&commit.tree_sha)?;
        let git_lossy = packed.stats.lossy_entries.len() > lossy_before;
        let state =
            crate::state_writer::descriptor_state_from_commit(&commit, tree_hash, git_lossy)?;
        packed
            .map
            .insert_commit(&commit.sha, state.state_id)
            .map_err(IngestError::from)?;
        let descriptor_dir = repo.heddle_dir().join("ingest").join("overlay-states");
        objects::fs_atomic::create_private_dir_all(&descriptor_dir)?;
        let path = descriptor_dir.join(format!("{}.state", state.state_id.to_string_full()));
        let bytes = state
            .encode_current_msgpack()
            .map_err(|error| IngestError::Other(format!("serialize overlay state: {error}")))?;
        objects::fs_atomic::write_file_atomic(&path, &bytes)?;
        state_path = Some(path);
        Ok(state.state_id)
    })();

    let _ = std::fs::remove_file(&pack_path);
    let _ = std::fs::remove_file(&index_path);
    let _ = std::fs::remove_dir_all(&bucket_dir);
    match result {
        Ok(state_id) => {
            if let Err(error) = map.flush_append_batch() {
                if let Some(path) = state_path {
                    let _ = std::fs::remove_file(path);
                }
                return Err(error.into());
            }
            Ok(state_id)
        }
        Err(error) => {
            map.abort_append_batch();
            if let Some(path) = state_path {
                let _ = std::fs::remove_file(path);
            }
            Err(error)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{io::Write, path::Path, process::Command};

    use objects::{
        object::{EntryType, FileMode, ThreadName},
        store::{
            FsStore, InMemoryStore,
            pack::{ObjectType, PackIndex, decode_tagged_entry_header},
        },
    };
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

    fn seed_delta_repo(path: &Path) {
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
        for version in 0..12 {
            let mut payload = vec![b'a'; 8 * 1024];
            payload[version * 31] = b'A' + version as u8;
            std::fs::write(path.join("history.txt"), payload).unwrap();
            run(&["add", "history.txt"]);
            run(&["commit", "-q", "-m", &format!("version {version}")]);
        }
    }

    fn count_pack_deltas(store: &FsStore) -> usize {
        let manager = store.pack_manager().read().unwrap();
        manager
            .pack_file_paths()
            .into_iter()
            .map(|(pack_path, index_path)| {
                let pack = std::fs::read(pack_path).unwrap();
                let index = PackIndex::from_bytes(&std::fs::read(index_path).unwrap()).unwrap();
                index
                    .ids()
                    .unwrap()
                    .into_iter()
                    .filter(|id| {
                        let offset = index.find(id).unwrap().expect("indexed fixture id") as usize;
                        decode_tagged_entry_header(&pack[offset..])
                            .is_ok_and(|header| header.obj_type == ObjectType::Delta)
                    })
                    .count()
            })
            .sum()
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

    fn append_git_oid(tree: &mut Vec<u8>, sha: &str) {
        for pair in sha.as_bytes().as_chunks::<2>().0 {
            let hex = std::str::from_utf8(pair).expect("Git object id is ASCII");
            tree.push(u8::from_str_radix(hex, 16).expect("Git object id is hexadecimal"));
        }
    }

    fn seed_raw_tree_repo(path: &Path, entries: &[(&str, &str)]) {
        let status = Command::new("git")
            .args(["init", "-q", "--initial-branch=main"])
            .current_dir(path)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .status()
            .expect("git init");
        assert!(status.success(), "git init failed");

        // Build the raw tree body instead of using `git mktree`: mktree
        // canonicalizes these historical modes before writing the object.
        let mut tree_body = Vec::new();
        for (mode, name) in entries {
            let blob = git_output(
                path,
                &["hash-object", "-w", "--stdin"],
                Some(format!("{name}\n").as_bytes()),
            );
            tree_body.extend_from_slice(format!("{mode} {name}\0").as_bytes());
            append_git_oid(&mut tree_body, &blob);
        }
        let tree = git_output(
            path,
            &["hash-object", "--literally", "-w", "-t", "tree", "--stdin"],
            Some(&tree_body),
        );
        let commit = git_output(path, &["commit-tree", &tree, "-m", "legacy modes"], None);
        git_output(path, &["update-ref", "refs/heads/main", &commit], None);
    }

    fn seed_noncanonical_mode_repo(path: &Path) {
        seed_raw_tree_repo(path, &[("100664", "legacy.txt"), ("100775", "run.sh")]);
    }

    fn seed_unknown_mode_repo(path: &Path) {
        seed_raw_tree_repo(path, &[("140000", "unknown")]);
    }

    fn refname_round_trip(name: &str) {
        refname_round_trip_input(name, name.len() > 240);
    }

    fn refname_round_trip_input(name: &str, packed: bool) {
        let source = TempDir::new().expect("source");
        let destination = TempDir::new().expect("destination");
        let oid = seed_multibranch_repo(source.path());
        let full = format!("refs/heads/{name}");
        // Packed input also permits a Git-valid single component longer than
        // the host filesystem's limit; Git itself writes this representation.
        if packed {
            std::fs::write(
                source.path().join(".git/packed-refs"),
                format!("{oid} {full}\n"),
            )
            .expect("packed Git ref");
        } else {
            git_output(source.path(), &["update-ref", &full, &oid], None);
        }
        let git = GitSource::open(source.path()).expect("open Git");
        let store = InMemoryStore::new();
        let refs = RefManager::new(destination.path());
        refs.init().expect("init refs");
        let mut map = ShaMap::new();
        pollster::block_on(Importer::new(&git, &store, &refs, &mut map).run()).expect("import");
        let native = ThreadName::from_git_branch(name).expect("Git branch mapping");
        let state = refs
            .get_thread(&native)
            .expect("read storage")
            .expect("imported branch");
        let head = refs::refs::Head::Attached {
            thread: native.clone(),
        };
        refs.write_head(&head).expect("HEAD write");
        assert_eq!(refs.read_head().expect("HEAD read"), head);
        refs.pack_refs().expect("native packed refs");
        let reopened = RefManager::new(destination.path());
        assert!(reopened.list_threads().expect("listing").contains(&native));
        assert_eq!(
            reopened.get_thread(&native).expect("fetch ref"),
            Some(state)
        );
        assert!(store.get_state(&state).expect("fetch state").is_some());
    }

    #[test]
    #[ignore = "sley 0.11.0 trims Unicode packed-ref suffixes; HeddleCo/sley#243"]
    fn refname_packed_git_nbsp_blocked_on_sley_243() {
        refname_round_trip_input("trailing\u{a0}", true);
    }

    #[test]
    fn refname_round_trip_equals() {
        refname_round_trip("feat/mcp=timeout");
    }
    #[test]
    fn refname_round_trip_comma() {
        refname_round_trip("a,b");
    }
    #[test]
    fn refname_round_trip_unicode() {
        refname_round_trip("ünicode/ブランチ");
    }
    #[test]
    fn refname_round_trip_at() {
        refname_round_trip("@");
    }
    #[test]
    fn refname_round_trip_plus() {
        refname_round_trip("x+y");
    }
    #[test]
    fn refname_round_trip_nbsp() {
        refname_round_trip("trailing\u{a0}");
    }
    #[test]
    fn refname_round_trip_replacement() {
        refname_round_trip("literal\u{fffd}");
    }
    #[test]
    fn refname_round_trip_long() {
        refname_round_trip(&"界".repeat(337));
    }
    #[test]
    fn refname_round_trip_reserved() {
        refname_round_trip("heddle/foo");
    }

    #[cfg(unix)]
    fn import_non_utf8_ref(namespace: &str, packed: bool) {
        use std::{ffi::OsString, os::unix::ffi::OsStringExt};
        let source = TempDir::new().expect("Git source");
        let destination = TempDir::new().expect("native destination");
        let oid = seed_multibranch_repo(source.path());
        let raw_name = format!("refs/{namespace}/bad-").into_bytes();
        let raw_name = [raw_name.as_slice(), b"\xff"].concat();
        if packed {
            let bytes = [oid.as_bytes(), b" ", &raw_name, b"\n"].concat();
            std::fs::write(source.path().join(".git/packed-refs"), bytes).expect("packed ref");
        } else {
            let path = source
                .path()
                .join(".git")
                .join(OsString::from_vec(raw_name.clone()));
            std::fs::create_dir_all(path.parent().expect("parent")).expect("namespace");
            std::fs::write(path, format!("{oid}\n")).expect("loose ref");
        }
        let git = GitSource::open(source.path()).expect("open Git");
        let store = InMemoryStore::new();
        let refs = RefManager::new(destination.path());
        refs.init().expect("native refs");
        let mut map = ShaMap::new();
        let stats = pollster::block_on(Importer::new(&git, &store, &refs, &mut map).run())
            .expect("other refs still import");
        assert!(
            refs.get_thread(&ThreadName::new("main"))
                .expect("main")
                .is_some()
        );
        assert!(
            !refs
                .list_threads()
                .expect("threads")
                .iter()
                .any(|name| name.contains("bad-"))
        );
        if namespace == "heads" || namespace == "tags" {
            assert!(
                stats
                    .skipped_refs
                    .iter()
                    .any(|excluded| excluded.raw_name == raw_name
                        && excluded.reason == ImportSkipReason::NonUtf8RefName),
                "excluded ref must retain its exact bytes: {:?}",
                stats.skipped_refs
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_ignored_namespace_does_not_abort_import() {
        for namespace in ["remotes/origin", "notes", "pull"] {
            for packed in [false, true] {
                import_non_utf8_ref(namespace, packed);
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_imported_ref_is_reported_and_other_refs_import() {
        for namespace in ["heads", "tags"] {
            for packed in [false, true] {
                import_non_utf8_ref(namespace, packed);
            }
        }
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
    fn import_ignores_github_provider_refs_without_skips() {
        let gitdir = TempDir::new().expect("Git temp dir");
        let heddledir = TempDir::new().expect("Heddle temp dir");
        let tip = seed_multibranch_repo(gitdir.path());
        for name in ["refs/pull/12/head", "refs/pull/12/merge"] {
            git_output(gitdir.path(), &["update-ref", name, &tip], None);
        }
        let git = GitSource::open(gitdir.path()).expect("Git source");
        let frozen = git.collect_frozen_import_refs().expect("frozen refs");
        let classified =
            objects::object::thread_replication::git_import_graph::classify_frozen_import_refs(
                &frozen,
            )
            .expect("classify GitHub-shaped refs");
        assert!(!classified.partial);
        assert!(classified.skipped_refs.is_empty());
        let store = InMemoryStore::new();
        let refs = RefManager::new(heddledir.path());
        refs.init().expect("refs");
        let mut map = ShaMap::new();
        let stats =
            pollster::block_on(Importer::new(&git, &store, &refs, &mut map).run()).expect("import");
        assert!(stats.skipped_refs.is_empty(), "{stats:?}");
        assert_eq!(stats.refs.threads_written, 2);
        assert_eq!(stats.refs.markers_written, 1);
        assert_eq!(stats.refs.skipped_unmapped, 0);
    }

    #[test]
    fn import_reports_skipped_ref_names_and_reasons() {
        let gitdir = TempDir::new().expect("Git temp dir");
        let heddledir = TempDir::new().expect("Heddle temp dir");
        let tip = seed_multibranch_repo(gitdir.path());
        git_output(
            gitdir.path(),
            &["update-ref", "refs/custom/keep-me-visible", &tip],
            None,
        );
        git_output(
            gitdir.path(),
            &["update-ref", "refs/heads/heddle/reserved", &tip],
            None,
        );
        let blob = git_output(
            gitdir.path(),
            &["hash-object", "-w", "--stdin"],
            Some(b"key"),
        );
        git_output(
            gitdir.path(),
            &["update-ref", "refs/tags/key-blob", &blob],
            None,
        );
        let git = GitSource::open(gitdir.path()).expect("Git source");
        let store = InMemoryStore::new();
        let refs = RefManager::new(heddledir.path());
        refs.init().expect("refs");
        let mut map = ShaMap::new();
        let stats =
            pollster::block_on(Importer::new(&git, &store, &refs, &mut map).run()).expect("import");
        assert!(
            stats
                .skipped_refs
                .iter()
                .any(|reference| { reference.raw_name == b"refs/custom/keep-me-visible" }),
            "{stats:?}"
        );
        assert!(
            stats
                .skipped_refs
                .iter()
                .any(|reference| { reference.raw_name == b"refs/tags/key-blob" }),
            "{stats:?}"
        );
        assert!(
            !stats
                .skipped_refs
                .iter()
                .any(|reference| reference.raw_name == b"refs/heads/heddle/reserved")
        );
        let reserved = ThreadName::from_git_branch("heddle/reserved").expect("mapped name");
        assert!(
            refs.get_thread(&reserved)
                .expect("reserved branch storage")
                .is_some()
        );
    }

    /// Seed a repository with exactly `native` branch + commit-tag refs: the
    /// multibranch seed's `main`, `feature/x` and `v0.1`, then alternating
    /// lightweight branches and tags at its tip, written in one `update-ref`.
    fn seed_native_refs(path: &Path, native: usize) {
        let tip = seed_multibranch_repo(path);
        let commands = (0..native - 3)
            .map(|index| {
                let namespace = if index % 2 == 0 { "heads" } else { "tags" };
                format!("create refs/{namespace}/bulk-{index:05} {tip}\n")
            })
            .collect::<String>();
        git_output(path, &["update-ref", "--stdin"], Some(commands.as_bytes()));
    }

    /// Classify a real source's frozen ref snapshot: the conversion gate that
    /// bounds native refs.
    fn classify_native_refs(
        git: &GitSource,
    ) -> objects::error::Result<
        objects::object::thread_replication::git_import_graph::ClassifiedImportRefs,
    > {
        let frozen = git.collect_frozen_import_refs().expect("frozen refs");
        objects::object::thread_replication::git_import_graph::classify_frozen_import_refs(&frozen)
    }

    /// heddle#2019: rails has 644 branches and tags; the old 512 bound
    /// refused it during conversion. Classify, then import every ref.
    #[test]
    fn import_converts_more_than_512_native_refs() {
        let native = 600;
        let gitdir = TempDir::new().expect("Git temp dir");
        let heddledir = TempDir::new().expect("Heddle temp dir");
        seed_native_refs(gitdir.path(), native);
        let git = GitSource::open(gitdir.path()).expect("Git source");
        let classified = classify_native_refs(&git)
            .unwrap_or_else(|error| panic!("{native} native refs must convert: {error}"));
        assert_eq!(classified.native_ref_count as usize, native);
        assert!(!classified.partial);
        let store = InMemoryStore::new();
        let refs = RefManager::new(heddledir.path());
        refs.init().expect("refs");
        let mut map = ShaMap::new();
        let stats =
            pollster::block_on(Importer::new(&git, &store, &refs, &mut map).run()).expect("import");
        assert_eq!(
            stats.refs.threads_written + stats.refs.markers_written,
            native,
            "{stats:?}"
        );
        assert_eq!(stats.refs.skipped_unmapped, 0);
        assert!(stats.skipped_refs.is_empty(), "{stats:?}");
    }

    /// heddle#2019: the bound matches heddle-api's `MAX_IMPORT_SOURCE_REFS`
    /// (4096); one more native ref is refused. Writing 4096 local refs costs
    /// about five fsyncs each, so this checks the gate, not ref emission.
    #[test]
    fn conversion_admits_the_maximum_native_refs_and_refuses_one_more() {
        let max = objects::object::thread_replication::git_import_graph::MAX_IMPORT_REFS;
        assert_eq!(max, 4096);
        let gitdir = TempDir::new().expect("Git temp dir");
        seed_native_refs(gitdir.path(), max);
        let git = GitSource::open(gitdir.path()).expect("Git source");
        let classified = classify_native_refs(&git)
            .unwrap_or_else(|error| panic!("{max} native refs must convert: {error}"));
        assert_eq!(classified.native_ref_count as usize, max);
        assert!(!classified.partial);
        git_output(gitdir.path(), &["tag", "one-past-the-bound", "main"], None);
        let git = GitSource::open(gitdir.path()).expect("Git source");
        let error = classify_native_refs(&git).expect_err("one ref past the bound");
        assert!(
            error
                .to_string()
                .contains("Git import has more than 4096 native refs"),
            "{error}"
        );
    }

    #[test]
    fn distinct_git_commits_cannot_claim_the_same_change_id() {
        let gitdir = TempDir::new().expect("Git temp dir");
        let heddledir = TempDir::new().expect("Heddle temp dir");
        let run = |args: &[&str]| {
            let status = Command::new("git")
                .args(args)
                .current_dir(gitdir.path())
                .env("GIT_AUTHOR_NAME", "Test")
                .env("GIT_AUTHOR_EMAIL", "test@example.com")
                .env("GIT_COMMITTER_NAME", "Test")
                .env("GIT_COMMITTER_EMAIL", "test@example.com")
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_SYSTEM", "/dev/null")
                .status()
                .expect("git command");
            assert!(status.success(), "git {args:?} failed");
        };
        run(&["init", "-q", "--initial-branch=main"]);
        let trailer = format!(
            "Heddle-Change-Id: {}",
            objects::object::ChangeId::from_bytes([7; 16]).to_string_full()
        );
        std::fs::write(gitdir.path().join("file"), "one").expect("first content");
        run(&["add", "file"]);
        run(&["commit", "-q", "-m", "first", "-m", &trailer]);
        std::fs::write(gitdir.path().join("file"), "two").expect("second content");
        run(&["add", "file"]);
        run(&["commit", "-q", "-m", "second", "-m", &trailer]);

        let git = GitSource::open(gitdir.path()).expect("Git source");
        let store = InMemoryStore::new();
        let refs = RefManager::new(heddledir.path());
        refs.init().expect("refs");
        let mut map = ShaMap::new();
        let result = pollster::block_on(Importer::new(&git, &store, &refs, &mut map).run());
        assert!(
            matches!(
                &result,
                Err(IngestError::ShaMap(
                    crate::sha_map::ShaMapError::ChangeIdCollision { .. }
                ))
            ),
            "{result:?}"
        );
        assert_eq!(
            refs.get_thread(&ThreadName::new("main"))
                .expect("thread read"),
            None,
            "collision must not publish a ref"
        );
    }

    #[test]
    fn cached_commit_claims_change_id_after_map_upgrade() {
        let gitdir = TempDir::new().expect("Git temp dir");
        let heddledir = TempDir::new().expect("Heddle temp dir");
        let run = |args: &[&str]| {
            let status = Command::new("git")
                .args(args)
                .current_dir(gitdir.path())
                .env("GIT_AUTHOR_NAME", "Test")
                .env("GIT_AUTHOR_EMAIL", "test@example.com")
                .env("GIT_COMMITTER_NAME", "Test")
                .env("GIT_COMMITTER_EMAIL", "test@example.com")
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_SYSTEM", "/dev/null")
                .status()
                .expect("git command");
            assert!(status.success(), "git {args:?} failed");
        };
        run(&["init", "-q", "--initial-branch=main"]);
        let trailer = format!(
            "Heddle-Change-Id: {}",
            objects::object::ChangeId::from_bytes([7; 16]).to_string_full()
        );
        std::fs::write(gitdir.path().join("file"), "one").expect("first content");
        run(&["add", "file"]);
        run(&["commit", "-q", "-m", "first", "-m", &trailer]);
        let git = GitSource::open(gitdir.path()).expect("Git source");
        let store = InMemoryStore::new();
        let refs = RefManager::new(heddledir.path());
        refs.init().expect("refs");
        let map_path = heddledir.path().join("sha-map.sqlite");
        {
            let mut map = ShaMap::open(&map_path).expect("map");
            pollster::block_on(Importer::new(&git, &store, &refs, &mut map).run())
                .expect("first import");
        }
        rusqlite::Connection::open(&map_path)
            .expect("map db")
            .execute("DROP TABLE git_import_change_ids", [])
            .expect("simulate old map");
        std::fs::write(gitdir.path().join("file"), "two").expect("second content");
        run(&["add", "file"]);
        run(&["commit", "-q", "-m", "second", "-m", &trailer]);
        let git = GitSource::open(gitdir.path()).expect("Git source");
        let mut map = ShaMap::open(&map_path).expect("upgraded map");
        let result = pollster::block_on(Importer::new(&git, &store, &refs, &mut map).run());
        assert!(
            matches!(
                result,
                Err(IngestError::ShaMap(
                    crate::sha_map::ShaMapError::ChangeIdCollision { .. }
                ))
            ),
            "{result:?}"
        );
    }

    #[test]
    fn delta_search_enabled_import_reconstructs_byte_exact_with_zero_blake3_failures() {
        let gitdir = TempDir::new().unwrap();
        let streaming_dir = TempDir::new().unwrap();
        let delta_dir = TempDir::new().unwrap();
        seed_delta_repo(gitdir.path());

        import_git_into_with_options(
            gitdir.path(),
            streaming_dir.path(),
            ImportOptions::default(),
        )
        .expect("streaming import");
        import_git_into_with_options(
            gitdir.path(),
            delta_dir.path(),
            ImportOptions {
                delta_search: true,
                ..ImportOptions::default()
            },
        )
        .expect("delta import");

        let streaming_store = FsStore::new(streaming_dir.path().join(".heddle"));
        let delta_store = FsStore::new(delta_dir.path().join(".heddle"));
        assert_eq!(
            count_pack_deltas(&streaming_store),
            0,
            "off must preserve the zero-delta streaming pack"
        );
        assert!(
            count_pack_deltas(&delta_store) > 0,
            "on must emit delta entries"
        );

        let hashes = delta_store.list_blobs().unwrap();
        assert!(!hashes.is_empty());
        let mut blake3_failures = 0;
        for hash in hashes {
            let blob = delta_store
                .get_blob(&hash)
                .unwrap()
                .expect("imported blob must reconstruct");
            if ContentHash::compute_typed("blob", blob.content()) != hash {
                blake3_failures += 1;
            }
        }
        assert_eq!(
            blake3_failures, 0,
            "delta-enabled import must have 0 BLAKE3 reconstruction failures"
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
    fn import_git_into_represents_gitlink_as_tree_entry() {
        let gitdir = TempDir::new().unwrap();
        let heddledir = TempDir::new().unwrap();
        seed_gitlink_repo(gitdir.path());

        let (stats, _map) =
            import_git_into(gitdir.path(), heddledir.path()).expect("gitlink import succeeds");

        assert!(stats.commits_imported >= 2);
        assert!(stats.lossy_entries.is_empty());

        let repo = repo::Repository::open(heddledir.path()).unwrap();
        let main_cid = repo
            .refs()
            .get_thread(&ThreadName::new("main"))
            .unwrap()
            .expect("main thread");
        let state = repo
            .store()
            .get_state(&main_cid)
            .unwrap()
            .expect("main state");
        assert!(!state.git_lossy, "gitlink import must remain byte-faithful");
        let tree = repo
            .store()
            .get_tree(&state.tree)
            .unwrap()
            .expect("root tree");
        let entry = tree.get("vendor").expect("vendor gitlink entry");

        assert_eq!(entry.entry_type(), EntryType::Gitlink);
        assert_eq!(entry.mode(), FileMode::Gitlink);
        assert_eq!(
            entry.gitlink_target().map(|oid| oid.to_string()),
            Some("0808080808080808080808080808080808080808".to_string())
        );
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

    /// Canonical corpus for the heddle#2018 import golden: every entry kind
    /// Git writes, plus `lib/` next to `lib.rs` and `lib-extra`, where Git's
    /// canonical order differs from native byte order.
    fn seed_canonical_golden_repo(path: &Path) {
        git_output(path, &["init", "-q", "--initial-branch=main"], None);
        for (name, content) in [
            ("README.md", "# golden\n"),
            ("build.sh", "#!/bin/sh\n"),
            ("lib.rs", "pub mod lib;\n"),
            ("lib-extra", "extra\n"),
            ("lib/mod.rs", "pub fn f() {}\n"),
            ("lib/deep/leaf.txt", "leaf\n"),
        ] {
            let file = path.join(name);
            std::fs::create_dir_all(file.parent().expect("parent")).expect("dirs");
            std::fs::write(&file, content).expect("write");
            git_output(path, &["add", "--", name], None);
        }
        git_output(path, &["update-index", "--chmod=+x", "build.sh"], None);
        let target = git_output(path, &["hash-object", "-w", "--stdin"], Some(b"README.md"));
        git_output(
            path,
            &[
                "update-index",
                "--add",
                "--cacheinfo",
                &format!("120000,{target},link"),
            ],
            None,
        );
        git_output(
            path,
            &[
                "update-index",
                "--add",
                "--cacheinfo",
                "160000,0808080808080808080808080808080808080808,vendor",
            ],
            None,
        );
        git_output(path, &["commit", "-q", "-m", "golden"], None);
    }

    /// heddle#2018 guard: an ordinary (canonical) Git repository imports to
    /// exactly the native tree ids it imported to before the Git-layout
    /// extension existed. These ids were captured on the pre-change importer.
    #[test]
    fn canonical_import_keeps_pinned_native_tree_ids() {
        let gitdir = TempDir::new().unwrap();
        let heddledir = TempDir::new().unwrap();
        seed_canonical_golden_repo(gitdir.path());
        import_git_into(gitdir.path(), heddledir.path()).expect("canonical import");

        let repo = repo::Repository::open(heddledir.path()).unwrap();
        let main = repo
            .refs()
            .get_thread(&ThreadName::new("main"))
            .unwrap()
            .expect("main thread");
        let state = repo.store().get_state(&main).unwrap().expect("main state");
        let root = repo
            .store()
            .get_tree(&state.tree)
            .unwrap()
            .expect("root tree");
        let lib = root
            .get("lib")
            .and_then(|entry| entry.tree_hash())
            .expect("lib");
        assert_eq!(
            [state.tree.to_hex(), lib.to_hex()],
            CANONICAL_IMPORT_GOLDEN.map(str::to_string),
        );
    }

    const CANONICAL_IMPORT_GOLDEN: [&str; 2] = [
        "7f97a3639e2e3b322f3a818071c57530ff3a683e245046b9f13f6381e2d3da1d",
        "e3736d64b9d3cb975b6eadc697c766c0c26fdf13e5fa768c08d34cbabc5fac3c",
    ];

    #[test]
    fn import_git_into_records_noncanonical_modes_with_canonical_meaning() {
        let gitdir = TempDir::new().unwrap();
        let heddledir = TempDir::new().unwrap();
        seed_noncanonical_mode_repo(gitdir.path());

        let (stats, _map) =
            import_git_into(gitdir.path(), heddledir.path()).expect("legacy modes import");
        assert_eq!(stats.commits_imported, 1);

        let repo = repo::Repository::open(heddledir.path()).unwrap();
        let main = repo
            .refs()
            .get_thread(&ThreadName::new("main"))
            .unwrap()
            .expect("main thread");
        let state = repo.store().get_state(&main).unwrap().expect("main state");
        let tree = repo
            .store()
            .get_tree(&state.tree)
            .unwrap()
            .expect("root tree");

        assert_eq!(
            tree.get("legacy.txt")
                .expect("legacy file")
                .mode()
                .to_unix_mode(),
            0o100644,
            "100664 must normalize to Git's 100644"
        );
        assert_eq!(
            tree.get("run.sh")
                .expect("executable file")
                .mode()
                .to_unix_mode(),
            0o100755,
            "100775 must normalize to Git's 100755"
        );
        // heddle#2018: the source modes are recorded for byte-exact export.
        for (name, digits) in [("legacy.txt", "100664"), ("run.sh", "100775")] {
            assert_eq!(
                tree.get(name).and_then(TreeEntry::raw_git_mode),
                Some(objects::object::RawGitMode::parse(digits.as_bytes()).unwrap()),
                "{name} keeps its source mode"
            );
        }
    }

    #[test]
    fn import_git_into_rejects_unknown_tree_modes() {
        let gitdir = TempDir::new().unwrap();
        let heddledir = TempDir::new().unwrap();
        seed_unknown_mode_repo(gitdir.path());

        let error = import_git_into(gitdir.path(), heddledir.path())
            .expect_err("unknown tree mode must fail");
        let message = error.to_string();
        assert!(
            message.contains("unsupported mode 140000"),
            "error should identify the unknown mode: {message}"
        );
    }

    #[test]
    fn import_git_into_lossy_converts_invalid_utf8_name_and_summarizes() {
        let gitdir = TempDir::new().unwrap();
        let heddledir = TempDir::new().unwrap();
        seed_invalid_utf8_name_repo(gitdir.path());

        let (stats, _map) = import_git_into_with_options(
            gitdir.path(),
            heddledir.path(),
            ImportOptions {
                lossy: true,
                ..ImportOptions::default()
            },
        )
        .expect("lossy import converts invalid UTF-8 name");
        let converted_name = "bad\u{fffd}name";

        assert_eq!(stats.commits_imported, 1);
        assert_eq!(stats.lossy_entries.len(), 1);
        assert_eq!(stats.non_reconstructable_commits.len(), 1);
        assert_eq!(stats.lossy_trees.len(), 1);
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
            ImportOptions {
                lossy: true,
                ..ImportOptions::default()
            },
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
            ImportOptions {
                lossy: true,
                ..ImportOptions::default()
            },
        )
        .expect("initial lossy import succeeds");
        drop(map);

        let (second, _map) = import_git_into_with_options(
            gitdir.path(),
            heddledir.path(),
            ImportOptions {
                lossy: true,
                ..ImportOptions::default()
            },
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
            ImportOptions {
                lossy: true,
                ..ImportOptions::default()
            },
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
    fn overlay_repair_materializes_each_mapped_object_once() {
        let gitdir = TempDir::new().unwrap();
        let run = |args: &[&str]| {
            let output = Command::new("git")
                .args(args)
                .current_dir(gitdir.path())
                .env("GIT_AUTHOR_NAME", "Test")
                .env("GIT_AUTHOR_EMAIL", "test@example.com")
                .env("GIT_COMMITTER_NAME", "Test")
                .env("GIT_COMMITTER_EMAIL", "test@example.com")
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_SYSTEM", "/dev/null")
                .output()
                .expect("git command");
            assert!(
                output.status.success(),
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            String::from_utf8_lossy(&output.stdout).trim().to_string()
        };

        run(&["init", "-q", "--initial-branch=main"]);
        std::fs::write(gitdir.path().join("shared.txt"), "shared\n").unwrap();
        std::fs::write(gitdir.path().join("evolving.txt"), "one\n").unwrap();
        run(&["add", "."]);
        run(&["commit", "-q", "-m", "one"]);
        for (content, message) in [("two\n", "two"), ("three\n", "three")] {
            std::fs::write(gitdir.path().join("evolving.txt"), content).unwrap();
            run(&["add", "evolving.txt"]);
            run(&["commit", "-q", "-m", message]);
        }
        let tip = run(&["rev-parse", "HEAD"]);

        repo::Repository::init(gitdir.path()).expect("initialize overlay repository");
        bind_single_git_commit_overlay(
            gitdir.path(),
            gitdir.path(),
            &tip,
            ImportOptions::default(),
        )
        .expect("bind lazy overlay tip");

        let (stats, _) = import_git_into(gitdir.path(), gitdir.path())
            .expect("full import repairs the descriptor-backed closure");
        assert_eq!(
            stats.trees_imported, 3,
            "each root tree is materialized once"
        );
        assert_eq!(
            stats.blobs_imported, 4,
            "the unchanged shared blob must not be appended once per commit"
        );
    }

    #[test]
    fn fresh_overlay_clone_binds_non_root_export_note_then_full_import_preserves_graph() {
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("source");
        let native = temp.path().join("native");
        let clone = temp.path().join("clone");
        std::fs::create_dir(&source).unwrap();
        std::fs::create_dir(&native).unwrap();
        git_output(&source, &["init", "-q", "--initial-branch=main"], None);
        std::fs::write(source.join("story.txt"), "one\n").unwrap();
        git_output(&source, &["add", "story.txt"], None);
        git_output(&source, &["commit", "-q", "-m", "root"], None);
        std::fs::write(source.join("story.txt"), "one\ntwo\n").unwrap();
        git_output(&source, &["add", "story.txt"], None);
        git_output(&source, &["commit", "-q", "-m", "child"], None);
        let tip = git_output(&source, &["rev-parse", "HEAD"], None);

        let (_, source_map) = import_git_into(&source, &native).expect("import source graph");
        let source_id = source_map
            .get_commit(&tip)
            .expect("read source mapping")
            .expect("mapped source tip");
        let source_repo = repo::Repository::open(&native).expect("open source Heddle repo");
        let source_state = source_repo
            .store()
            .get_state(&source_id)
            .expect("read source state")
            .expect("source state");
        assert_eq!(
            source_state.parents.len(),
            1,
            "fixture tip must be non-root"
        );
        let note = objects::object::HeddleNote::from_state(&source_state)
            .to_json_bytes()
            .expect("encode canonical note");
        git_output(
            &source,
            &["notes", "--ref=heddle", "add", "-f", "-F", "-", &tip],
            Some(&note),
        );

        let clone_output = Command::new("git")
            .args([
                "clone",
                "-q",
                "--no-local",
                source.to_str().unwrap(),
                clone.to_str().unwrap(),
            ])
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .expect("clone exported Git repository");
        assert!(
            clone_output.status.success(),
            "clone failed: {}",
            String::from_utf8_lossy(&clone_output.stderr)
        );
        git_output(
            &clone,
            &[
                "fetch",
                "-q",
                "origin",
                "refs/notes/heddle:refs/notes/heddle",
            ],
            None,
        );

        repo::Repository::bootstrap_git_overlay(&clone).expect("initialize fresh overlay clone");
        let bound = bind_single_git_commit_overlay(&clone, &clone, &tip, ImportOptions::default())
            .expect("bind exported non-root tip lazily");
        assert_eq!(bound, source_id);
        let clone_repo = repo::Repository::open(&clone).expect("open overlay clone");
        assert_eq!(
            clone_repo
                .store()
                .get_state(&bound)
                .expect("read descriptor")
                .expect("descriptor state"),
            source_state,
            "lazy bind must retain the exact portable source State"
        );
        drop(clone_repo);

        let (_, repaired_map) =
            import_git_into(&clone, &clone).expect("materialize full noted parent graph");
        assert_eq!(repaired_map.get_commit(&tip).unwrap(), Some(source_id));
        let repaired = repo::Repository::open(&clone).expect("open repaired clone");
        let repaired_state = repaired
            .store()
            .get_state(&source_id)
            .expect("read repaired state")
            .expect("repaired state");
        assert_eq!(repaired_state, source_state);
        assert!(
            repaired
                .store()
                .get_state(&source_state.parents[0])
                .expect("read materialized parent")
                .is_some(),
            "full import must materialize the exact parent graph retained by the note"
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
            state.state_id, main_cid,
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
        assert_eq!(stats.states_created, map.commit_shas().unwrap().len());
        let map_path = gitdir
            .path()
            .join(".heddle")
            .join("ingest")
            .join("sha_map.sqlite");
        assert!(map_path.is_file(), "ingest SHA map is missing");
        let reloaded = ShaMap::open(&map_path).unwrap();
        assert_eq!(
            reloaded.commit_shas().unwrap().len(),
            map.commit_shas().unwrap().len()
        );
        for git_oid in map.commit_shas().unwrap() {
            assert_eq!(
                reloaded.get_commit(&git_oid).unwrap(),
                map.get_commit(&git_oid).unwrap()
            );
        }

        let git_projection_mapping_path = gitdir
            .path()
            .join(".heddle")
            .join("git-projection")
            .join("git-projection-mapping.json");
        assert!(
            !git_projection_mapping_path.exists(),
            "ingest import must not publish the served Git Projection Mapping cache"
        );
        assert!(
            !gitdir.path().join(".heddle").join("git").exists(),
            "ingest-backed import must not create the legacy Bridge Mirror"
        );
    }

    #[test]
    fn import_single_git_commit_binds_tip_without_ancestors() {
        let gitdir = TempDir::new().unwrap();
        let run = |args: &[&str]| {
            let status = Command::new("git")
                .args(args)
                .current_dir(gitdir.path())
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
        std::fs::write(gitdir.path().join("a.txt"), "one").unwrap();
        run(&["add", "a.txt"]);
        run(&["commit", "-q", "-m", "first"]);
        std::fs::write(gitdir.path().join("a.txt"), "two").unwrap();
        run(&["add", "a.txt"]);
        run(&["commit", "-q", "-m", "second"]);
        let tip = git_output(gitdir.path(), &["rev-parse", "HEAD"], None);

        let change_id = import_single_git_commit_into(
            gitdir.path(),
            gitdir.path(),
            &tip,
            ImportOptions::default(),
        )
        .expect("single-tip import");

        let map = ShaMap::open(gitdir.path().join(".heddle/ingest/sha_map.sqlite")).unwrap();
        assert_eq!(map.get_commit(&tip).unwrap(), Some(change_id));
        // Only the tip — not the parent commit.
        assert_eq!(map.commit_shas().unwrap().len(), 1);

        let repo = repo::Repository::open(gitdir.path()).unwrap();
        let state = repo
            .store()
            .get_state(&change_id)
            .unwrap()
            .expect("tip state present");
        assert!(
            state.parents.is_empty(),
            "single-tip bind is a Heddle root mapped to the git tip"
        );

        // Idempotent re-bind.
        let again = import_single_git_commit_into(
            gitdir.path(),
            gitdir.path(),
            &tip,
            ImportOptions::default(),
        )
        .expect("re-bind");
        assert_eq!(again, change_id);
        assert_eq!(
            ShaMap::open(gitdir.path().join(".heddle/ingest/sha_map.sqlite"))
                .unwrap()
                .commit_shas()
                .unwrap()
                .len(),
            1
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

    /// Git import refuses trees whose entries alias a metadata directory
    /// (heddle#2028): `.git` at any depth, `.heddle` at a commit's root.
    ///
    /// Git refuses to write these trees itself, so they are built raw with
    /// `git hash-object --literally`, as a hostile repository would be.
    mod reserved_tree_entry_tests {
        use std::{io::Write, path::Path, process::Command};

        use objects::{
            object::{MetadataDir, ThreadName},
            store::InMemoryStore,
        };
        use refs::refs::RefManager;
        use tempfile::TempDir;

        use crate::{
            GitSource, ImportOptions, IngestError, OverlayHistory, importer::Importer,
            sha_map::ShaMap,
        };

        fn git(path: &Path, args: &[&str], stdin: Option<&[u8]>) -> String {
            let mut command = Command::new("git");
            command
                .args(args)
                .current_dir(path)
                .env("GIT_AUTHOR_NAME", "Test")
                .env("GIT_AUTHOR_EMAIL", "test@example.com")
                .env("GIT_COMMITTER_NAME", "Test")
                .env("GIT_COMMITTER_EMAIL", "test@example.com")
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_SYSTEM", "/dev/null")
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped());
            let mut child = command.spawn().expect("spawn git");
            child
                .stdin
                .take()
                .expect("stdin")
                .write_all(stdin.unwrap_or_default())
                .expect("write stdin");
            let output = child.wait_with_output().expect("git output");
            assert!(
                output.status.success(),
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            String::from_utf8(output.stdout).unwrap().trim().to_string()
        }

        /// A raw Git tree: `(mode, name, oid)` entries, written as given (Git's
        /// own order is not enforced, and no name is refused).
        fn raw_tree(path: &Path, entries: &[(&str, &[u8], &str)]) -> String {
            let mut body = Vec::new();
            for (mode, name, oid) in entries {
                body.extend_from_slice(mode.as_bytes());
                body.push(b' ');
                body.extend_from_slice(name);
                body.push(0);
                for pair in oid.as_bytes().chunks(2) {
                    body.push(u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap());
                }
            }
            git(
                path,
                &["hash-object", "--literally", "-w", "-t", "tree", "--stdin"],
                Some(&body),
            )
        }

        fn blob(path: &Path, content: &[u8]) -> String {
            git(path, &["hash-object", "-w", "--stdin"], Some(content))
        }

        fn commit(path: &Path, tree: &str, parent: Option<&str>) -> String {
            let mut args = vec!["commit-tree", tree, "-m", "hostile"];
            if let Some(parent) = parent {
                args.extend(["-p", parent]);
            }
            git(path, &args, None)
        }

        fn init(path: &Path) {
            git(path, &["init", "-q", "--initial-branch=main"], None);
        }

        /// `README.md` plus `<components…>/x`: the alias sits on the path to the
        /// leaf, e.g. `[".git", "hooks"]` or `["a", ".git", "hooks"]`.
        fn seed_hostile_repo(path: &Path, components: &[&[u8]]) {
            init(path);
            let readme = blob(path, b"readme\n");
            let hook = blob(path, b"#!/bin/sh\necho pwned\n");
            let mut child = raw_tree(path, &[("100755", b"x", &hook)]);
            for name in components.iter().skip(1).rev() {
                child = raw_tree(path, &[("40000", name, &child)]);
            }
            let root = raw_tree(
                path,
                &[
                    ("100644", b"README.md", &readme),
                    ("40000", components[0], &child),
                ],
            );
            let tip = commit(path, &root, None);
            git(path, &["update-ref", "refs/heads/main", &tip], None);
        }

        fn import(path: &Path, lossy: bool) -> crate::Result<()> {
            let destination = TempDir::new().unwrap();
            let git = GitSource::open(path).unwrap();
            let store = InMemoryStore::new();
            let refs = RefManager::new(destination.path());
            refs.init().unwrap();
            let mut map = ShaMap::new();
            let mut importer =
                Importer::new(&git, &store, &refs, &mut map).with_options(ImportOptions {
                    lossy,
                    ..ImportOptions::default()
                });
            pollster::block_on(importer.run()).map(drop)
        }

        fn expect_reserved(result: crate::Result<()>, path: &str, dir: MetadataDir) {
            match result {
                Err(IngestError::ReservedTreeEntry {
                    tree,
                    path: actual,
                    reason,
                }) => {
                    assert_eq!(actual, path);
                    assert_eq!(reason.dir, dir, "{path}");
                    assert_eq!(tree.len(), 40, "names the Git tree: {tree}");
                    let message = IngestError::ReservedTreeEntry {
                        tree: tree.clone(),
                        path: actual,
                        reason,
                    }
                    .to_string();
                    assert!(
                        message.contains(&tree) && message.contains(path),
                        "{message}"
                    );
                }
                other => panic!("{path}: expected ReservedTreeEntry, got {other:?}"),
            }
        }

        const GIT_ALIASES: [&[u8]; 8] = [
            b".git",
            b".GIT",
            b".git.",
            b".git ",
            b"GIT~1",
            b".git::$INDEX_ALLOCATION",
            ".g\u{200c}it".as_bytes(),
            b".git\\hooks",
        ];

        #[test]
        fn import_refuses_git_aliases_at_the_root_and_nested() {
            for alias in GIT_ALIASES {
                let name = String::from_utf8_lossy(alias).into_owned();
                for (components, path) in [
                    (vec![alias, b"hooks"], name.clone()),
                    (vec![b"a".as_slice(), alias, b"hooks"], format!("a/{name}")),
                ] {
                    let source = TempDir::new().unwrap();
                    seed_hostile_repo(source.path(), &components);
                    for lossy in [false, true] {
                        expect_reserved(import(source.path(), lossy), &path, MetadataDir::Git);
                    }
                    match OverlayHistory::open(source.path(), "main") {
                        Err(IngestError::ReservedTreeEntry { path: actual, .. }) => {
                            assert_eq!(actual, path);
                        }
                        Err(other) => panic!("{path}: overlay history: {other}"),
                        Ok(_) => panic!("{path}: overlay history accepted the tree"),
                    }
                }
            }
        }

        #[test]
        fn import_refuses_heddle_aliases_at_the_root() {
            for alias in [
                b".heddle".as_slice(),
                b".HEDDLE",
                b".heddle.",
                b"HEDDLE~1",
                ".hed\u{200c}dle".as_bytes(),
            ] {
                let source = TempDir::new().unwrap();
                seed_hostile_repo(source.path(), &[alias, b"hooks"]);
                let path = String::from_utf8_lossy(alias).into_owned();
                expect_reserved(import(source.path(), true), &path, MetadataDir::Heddle);
                assert!(OverlayHistory::open(source.path(), "main").is_err());
            }
        }

        /// A nested `.heddle` is a tracked fixture (weft keeps
        /// `examples/calculator/.heddle/`), and ordinary dotfiles are ordinary.
        #[test]
        fn import_keeps_nested_heddle_fixtures_and_dotfiles() {
            let source = TempDir::new().unwrap();
            init(source.path());
            let file = blob(source.path(), b"content\n");
            let fixture = raw_tree(source.path(), &[("100644", b"HEAD", &file)]);
            let calculator = raw_tree(source.path(), &[("40000", b".heddle", &fixture)]);
            let examples = raw_tree(source.path(), &[("40000", b"calculator", &calculator)]);
            let workflows = raw_tree(source.path(), &[("100644", b"ci.yml", &file)]);
            let root = raw_tree(
                source.path(),
                &[
                    ("40000", b".github", &workflows),
                    ("100644", b".gitignore", &file),
                    ("100644", b".heddleignore", &file),
                    ("40000", b"examples", &examples),
                ],
            );
            let tip = commit(source.path(), &root, None);
            git(
                source.path(),
                &["update-ref", "refs/heads/main", &tip],
                None,
            );

            import(source.path(), false).expect("ordinary tree imports");
            OverlayHistory::open(source.path(), "main").expect("overlay history");
        }

        /// A tree first translated as a subtree, where `.heddle` is allowed, is
        /// still refused when a later commit uses it as its root.
        #[test]
        fn a_subtree_reused_as_a_root_is_checked_again() {
            let source = TempDir::new().unwrap();
            init(source.path());
            let file = blob(source.path(), b"content\n");
            let metadata = raw_tree(source.path(), &[("100644", b"config.toml", &file)]);
            let inner = raw_tree(source.path(), &[("40000", b".heddle", &metadata)]);
            let first = raw_tree(source.path(), &[("40000", b"sub", &inner)]);
            let first = commit(source.path(), &first, None);
            let second = commit(source.path(), &inner, Some(&first));
            git(
                source.path(),
                &["update-ref", "refs/heads/main", &second],
                None,
            );

            expect_reserved(import(source.path(), false), ".heddle", MetadataDir::Heddle);
            assert!(OverlayHistory::open(source.path(), "main").is_err());
            // The first commit alone is fine: its `.heddle` is nested.
            git(
                source.path(),
                &["update-ref", "refs/heads/first", &first],
                None,
            );
            OverlayHistory::open(source.path(), "first").expect("nested .heddle projects");
        }

        #[test]
        fn reserved_threads_are_not_written() {
            let source = TempDir::new().unwrap();
            seed_hostile_repo(source.path(), &[b".git", b"hooks"]);
            let destination = TempDir::new().unwrap();
            let git = GitSource::open(source.path()).unwrap();
            let store = InMemoryStore::new();
            let refs = RefManager::new(destination.path());
            refs.init().unwrap();
            let mut map = ShaMap::new();
            assert!(
                pollster::block_on(Importer::new(&git, &store, &refs, &mut map).run()).is_err()
            );
            assert_eq!(
                refs.get_thread(&ThreadName::new("main")).unwrap(),
                None,
                "a refused import must not publish the branch"
            );
        }
    }
}
