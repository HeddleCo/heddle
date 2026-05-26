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

use std::path::{Path, PathBuf};

use objects::{
    object::{Blob, ChangeId, ContentHash, Tree, TreeEntry},
    store::{
        pack::{ObjectType as PackObjectType, PackObjectId, StreamingPackBuilder},
        CompressionConfig, ObjectStore,
    },
};
use oplog::oplog::OpLogBackend;
use refs::refs::RefBackend;
use tracing::info;

use crate::{
    git_walk::{CommitEntry, GitSource, RefDiscoveryStats, TreeChild, TreeChildKind},
    oplog_emit::{OplogEmitStats, OplogEmitter},
    ref_emit::{RefEmitStats, RefEmitter},
    sha_map::ShaMap,
    state_writer::state_from_commit,
    IngestError,
};

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
}

/// Orchestrates one import pass.
pub struct Importer<'a> {
    git: &'a GitSource,
    store: &'a dyn ObjectStore,
    refs: &'a dyn RefBackend,
    map: &'a mut ShaMap,
    oplog: Option<&'a dyn OpLogBackend>,
    /// Where the streaming pack builder writes its in-flight pack
    /// file and 512 index-bucket files. Both are removed on a clean
    /// finalize. Defaults to `std::env::temp_dir()/heddle-ingest-<pid>`
    /// when the caller doesn't pass one — but production calls
    /// (`import_git_into`) override this to a path under the heddle
    /// store's directory so the final `rename(2)` lands on the same
    /// filesystem and stays atomic.
    pack_staging_dir: Option<PathBuf>,
}

impl<'a> Importer<'a> {
    pub fn new(
        git: &'a GitSource,
        store: &'a dyn ObjectStore,
        refs: &'a dyn RefBackend,
        map: &'a mut ShaMap,
    ) -> Self {
        Self {
            git,
            store,
            refs,
            map,
            oplog: None,
            pack_staging_dir: None,
        }
    }

    /// Attach an oplog backend so the importer also translates reflog
    /// entries into `OpRecord`s. Without one the import still produces a
    /// valid Heddle repo — you just don't get `heddle undo` reach past the
    /// import boundary.
    pub fn with_oplog(mut self, oplog: &'a dyn OpLogBackend) -> Self {
        self.oplog = Some(oplog);
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

    /// Run the full import. Safe to re-invoke on the same `ShaMap` — the
    /// translators short-circuit on cache hits, so a second pass is
    /// effectively a no-op modulo any new commits since last time.
    pub fn run(&mut self) -> crate::Result<ImportStats> {
        let (heads, refs_seen) = self.git.collect_refs_detailed()?;
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
        let live_shas: Vec<String> = heads.iter().map(|h| h.target_sha.clone()).collect();
        let reflog_shas = self.git.reflog_commit_shas()?;
        let reflog_only_commits = reflog_shas
            .iter()
            .filter(|s| !live_shas.iter().any(|live| live == *s))
            .count();

        let mut seed = live_shas;
        for s in reflog_shas {
            if !seed.contains(&s) {
                seed.push(s);
            }
        }

        let commits = self.git.commits_topo(seed)?;
        info!(commit_count = commits.len(), "topo-sorted commits");

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
            "import-{}-{}",
            std::process::id(),
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
            let mut packed = PackedImport::new(self.git, self.map, builder);
            let mut last_log = 0usize;
            for (idx, commit) in commits.iter().enumerate() {
                let tree_hash = packed.translate_tree(&commit.tree_sha)?;
                packed.write_commit(commit, tree_hash)?;

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

        let ref_stats = RefEmitter::new(self.refs, self.map).emit(&heads)?;
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
            let entries = self.git.collect_reflog()?;
            let stats = OplogEmitter::new(oplog, self.map)
                .with_scope("ingest")
                .emit(&entries)?;
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
            trees_imported: packed_stats.trees,
            blobs_imported: packed_stats.blobs,
            refs: ref_stats,
            oplog: oplog_stats,
            reflog_only_commits,
        })
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct PackedImportStats {
    object_count: usize,
    trees: usize,
    blobs: usize,
}

struct PackedImport<'a, W: std::io::Write + std::io::Read + std::io::Seek> {
    git: &'a GitSource,
    map: &'a mut ShaMap,
    builder: StreamingPackBuilder<W>,
    stats: PackedImportStats,
}

impl<'a, W: std::io::Write + std::io::Read + std::io::Seek> PackedImport<'a, W> {
    fn new(git: &'a GitSource, map: &'a mut ShaMap, builder: StreamingPackBuilder<W>) -> Self {
        Self {
            git,
            map,
            builder,
            stats: PackedImportStats::default(),
        }
    }

    fn translate_tree(&mut self, git_tree_sha: &str) -> crate::Result<ContentHash> {
        if let Some(hash) = self.map.get_tree(git_tree_sha) {
            return Ok(hash);
        }

        let children = self.git.read_tree(git_tree_sha)?;
        let mut entries = Vec::with_capacity(children.len());
        for child in children {
            if let Some(entry) = self.translate_child(&child)? {
                entries.push(entry);
            }
        }

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
            .insert_tree(git_tree_sha, hash)
            .map_err(IngestError::from)?;
        Ok(hash)
    }

    fn translate_child(&mut self, child: &TreeChild) -> crate::Result<Option<TreeEntry>> {
        if child.name.is_empty()
            || child.name == "."
            || child.name == ".."
            || child.name.contains('/')
            || child.name.bytes().any(|b| b < 0x20 || b == 0x7f)
        {
            tracing::warn!(name = %child.name, "skipping tree child with unusable name");
            return Ok(None);
        }

        match child.kind {
            TreeChildKind::Blob { executable } => {
                let hash = self.translate_blob(&child.sha)?;
                Ok(Some(
                    TreeEntry::file(child.name.clone(), hash, executable)
                        .map_err(|e| IngestError::Heddle(e.into()))?,
                ))
            }
            TreeChildKind::Tree => {
                let hash = self.translate_tree(&child.sha)?;
                Ok(Some(
                    TreeEntry::directory(child.name.clone(), hash)
                        .map_err(|e| IngestError::Heddle(e.into()))?,
                ))
            }
            TreeChildKind::Symlink => {
                let hash = self.translate_blob(&child.sha)?;
                Ok(Some(
                    TreeEntry::symlink(child.name.clone(), hash)
                        .map_err(|e| IngestError::Heddle(e.into()))?,
                ))
            }
            TreeChildKind::Gitlink => {
                tracing::warn!(
                    name = %child.name,
                    submodule_sha = %child.sha,
                    "dropping gitlink (submodule) entry — no Heddle equivalent"
                );
                Ok(None)
            }
        }
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

    fn write_commit(&mut self, commit: &CommitEntry, tree: ContentHash) -> crate::Result<ChangeId> {
        if let Some(cid) = self.map.get_commit(&commit.sha) {
            return Ok(cid);
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

        let state = state_from_commit(commit, tree, parents);
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

        self.map
            .insert_commit(&commit.sha, state.change_id)
            .map_err(IngestError::from)?;
        Ok(state.change_id)
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
/// Returns both the stats and the final sha map so callers can persist
/// it alongside the heddle repo.
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

    let stats = Importer::new(&git, repo.store(), repo.refs(), &mut map)
        .with_oplog(repo.oplog())
        .with_pack_staging_dir(staging_dir)
        .run()?;
    Ok((stats, map))
}

#[cfg(test)]
mod tests {
    use std::{path::Path, process::Command};

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

        let stats = Importer::new(&git, &store, &refs, &mut map).run().unwrap();

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
        assert!(refs
            .get_thread(&ThreadName::new("feature/x"))
            .unwrap()
            .is_some());
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

        let first = Importer::new(&git, &store, &refs, &mut map).run().unwrap();
        let states_after_first = store.list_states().unwrap().len();
        let second = Importer::new(&git, &store, &refs, &mut map).run().unwrap();
        let states_after_second = store.list_states().unwrap().len();

        assert_eq!(first.commits_imported, second.commits_imported);
        assert_eq!(
            states_after_first, states_after_second,
            "second run minted new states — import is not idempotent"
        );
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

        let stats = Importer::new(&git, &store, &refs, &mut map).run().unwrap();
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

        let stats = Importer::new(&git, &store, &refs, &mut map)
            .with_oplog(&oplog)
            .run()
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
                | OpRecord::ThreadCreateV2 { .. }
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

        let stats = Importer::new(&git, &store, &refs, &mut map).run().unwrap();
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
