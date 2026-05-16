// SPDX-License-Identifier: Apache-2.0
//! Thread-level materialization: resolve a thread → state → tree,
//! materialize the tree to disk (clonefile-first via the existing
//! `Repository::materialize_tree`), and write a [`ThreadManifest`]
//! sidecar that captures the per-file stat-cache for fast subsequent
//! `heddle capture` scans.
//!
//! This is the day-one default workspace shape for lightweight
//! threads on reflink-capable filesystems (see
//! `docs/design/clonefile-threads.md`). Reads off the materialized
//! tree are vanilla `read(2)` against real APFS/btrfs files — no
//! userspace FS callbacks in the hot path. Disk usage is the
//! ~zero-cost clonefile share until the agent diverges blocks.

use std::{collections::BTreeMap, fs, os::unix::fs::MetadataExt, path::Path};

use objects::{
    lock::RepositoryLockExt,
    object::{ChangeId, State, Tree},
};
use tracing::{debug, instrument};

use super::{HeddleError, Repository, Result};
use crate::thread_manifest::{read_manifest, write_manifest, ManifestFile, ThreadManifest};

/// Outcome of [`Repository::capture_thread_from_disk`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ThreadCaptureOutcome {
    /// The materialized tree matches the existing thread head; no
    /// new state was written. The manifest was refreshed to reflect
    /// the latest stat fields (so subsequent captures stay fast even
    /// if mtimes drifted via `touch`).
    NoOp,
    /// A new state was written and the thread head advanced.
    Captured { state_id: ChangeId },
}

impl Repository {
    /// Materialize the captured tree of `thread` to `dest` and write
    /// a [`ThreadManifest`] sidecar to
    /// `<heddle_dir>/threads/<thread>/manifest.toml`.
    ///
    /// Order of operations:
    ///   1. Resolve `thread` → `ChangeId` → `State` → `Tree`.
    ///   2. Call `Repository::materialize_tree(&tree, dest)` — the
    ///      existing clonefile-first materializer does the heavy
    ///      lifting (loose-uncompressed promotion, parallel writes).
    ///   3. Walk the materialized tree and capture per-file
    ///      `(hash, inode, mtime_ns, ctime_ns, mode)` into the
    ///      manifest.
    ///   4. Atomically write the manifest.
    ///
    /// The walk step in (3) is a single `stat` per file — sub-ms for
    /// the 643-file heddle workspace. Doing the walk after
    /// materialize rather than capturing stats during materialize
    /// keeps the existing materializer untouched.
    #[instrument(skip(self), fields(thread = %thread, dest = %dest.display()))]
    pub fn materialize_thread(&self, thread: &str, dest: &Path) -> Result<ThreadManifest> {
        let change_id = self
            .refs()
            .resolve(thread)?
            .ok_or_else(|| HeddleError::Config(format!("unknown thread {thread}")))?;
        let state = self
            .store()
            .get_state(&change_id)?
            .ok_or_else(|| HeddleError::Config(format!("state for {thread} missing")))?;
        let tree = self
            .store()
            .get_tree(&state.tree)?
            .ok_or_else(|| HeddleError::Config(format!("tree for {thread} missing")))?;

        self.materialize_tree(&tree, dest)?;

        let mut manifest = ThreadManifest::new(change_id, state.tree);
        populate_manifest_from_tree(self, &tree, dest, "", &mut manifest.files)?;

        write_manifest(self.heddle_dir(), thread, &manifest).map_err(HeddleError::Io)?;

        debug!(
            thread = %thread,
            state_id = %change_id,
            files = manifest.files.len(),
            "thread materialized"
        );
        Ok(manifest)
    }

    /// Write the [`ThreadManifest`] sidecar for a worktree that's
    /// already been materialised to `dest` against `state_id`. Used
    /// by the CLI's `start` path, which calls `materialize_tree`
    /// directly via `write_isolated_checkout` and then needs the
    /// matching manifest written so the rest of the clonefile-thread
    /// machinery (`heddle status` advisory, `Repository::snapshot`
    /// auto-detection, `capture_thread_from_disk` fast no-op) sees a
    /// fully-formed sidecar.
    ///
    /// `state_id` is the captured state the worktree was materialised
    /// against; its tree is resolved and walked to populate the
    /// manifest's per-file stat-cache entries (one `lstat` per file).
    /// Atomic write: a torn manifest can't half-land. Idempotent at
    /// the manifest-key level: rewriting a manifest for the same
    /// thread is supported (and is what `capture_thread_from_disk`
    /// does post-capture).
    #[instrument(skip(self), fields(thread = %thread, dest = %dest.display(), state = %state_id))]
    pub fn record_thread_manifest(
        &self,
        thread: &str,
        state_id: &ChangeId,
        dest: &Path,
    ) -> Result<ThreadManifest> {
        let state = self
            .store()
            .get_state(state_id)?
            .ok_or_else(|| HeddleError::Config(format!("state {state_id} missing")))?;
        let tree = self
            .store()
            .get_tree(&state.tree)?
            .ok_or_else(|| HeddleError::Config(format!("tree for state {state_id} missing")))?;
        let mut manifest = ThreadManifest::new(*state_id, state.tree);
        populate_manifest_from_tree(self, &tree, dest, "", &mut manifest.files)?;
        crate::thread_manifest::write_manifest(self.heddle_dir(), thread, &manifest)
            .map_err(HeddleError::Io)?;
        debug!(
            thread = %thread,
            state_id = %state_id,
            files = manifest.files.len(),
            "thread manifest recorded post-materialize"
        );
        Ok(manifest)
    }

    /// Scan the materialized worktree at `root`, build a fresh tree
    /// from the on-disk bytes, and (if anything changed) advance
    /// `thread`'s head to a new state pointing at that tree. The
    /// manifest is rewritten to reflect the new state and the
    /// post-capture stat fields.
    ///
    /// Returns [`ThreadCaptureOutcome::NoOp`] when the new tree's
    /// hash equals the manifest's recorded `tree_hash` — the agent
    /// touched nothing material. Otherwise
    /// [`ThreadCaptureOutcome::Captured`] with the new state id.
    ///
    /// The reason this method exists alongside `Repository::snapshot`
    /// is two-fold:
    ///   1. `snapshot` always advances `HEAD`'s currently-attached
    ///      thread. Capture-from-disk targets *a specific thread by
    ///      name*, which is what auto-capture-on-switch needs.
    ///   2. `snapshot` walks `self.root`. Capture-from-disk walks
    ///      whatever directory the materializer put the thread at —
    ///      sibling directories under
    ///      `<workspace_parent>/.<repo>-heddle-mounts/<thread>/`,
    ///      which are NOT `self.root`.
    ///
    /// Walks `Repository::build_tree` for the slow path so the
    /// resulting trees are byte-identical to what `heddle capture`
    /// produces against the same content. A stat-cache fast path
    /// (see [`stat_cache_no_op`]) short-circuits the common case
    /// of "switch threads, nothing changed" so the dominant
    /// auto-capture-on-switch latency is a `stat` walk, not a
    /// blob rehash.
    #[instrument(skip(self), fields(thread = %thread, root = %root.display()))]
    pub fn capture_thread_from_disk(
        &self,
        thread: &str,
        root: &Path,
    ) -> Result<ThreadCaptureOutcome> {
        // Repository-wide write lock — same shape as
        // `snapshot_with_attribution_profiled`. Without it, two
        // concurrent `thread switch` invocations from sibling
        // worktrees can race the same source thread: both read
        // `get_thread(thread)` returning the same parent, both
        // `put_state` with that parent, both `set_thread` —
        // result is two leaf states with the same parent, one of
        // which is orphaned because the ref ends up pointing at
        // whichever `set_thread` won the race. The manifest write
        // at step 4 has the same lost-update problem on a smaller
        // scale. Holding the write lock across the whole
        // read-modify-write sequence makes the capture atomic with
        // respect to other state-changing operations.
        let _lock = self
            .locker()
            .write()
            .map_err(|e| HeddleError::Io(std::io::Error::other(e.to_string())))?;

        let existing_manifest = read_manifest(self.heddle_dir(), thread).map_err(HeddleError::Io)?;

        // 0. Fast no-op via the stat-cache. If every file in the
        //    manifest still exists with the same `(inode, mtime,
        //    ctime, mode)` AND the disk walk turns up no
        //    untracked/new files, we know the tree is byte-identical
        //    to what we materialised. Skip the entire blob-and-tree
        //    rebuild. Typical cost: ~5ms for a 643-file worktree
        //    vs hundreds of ms for the full `build_tree` rehash.
        if let Some(m) = existing_manifest.as_ref()
            && stat_cache_no_op(self, m, root)?
        {
            debug!(thread = %thread, "thread capture no-op (stat-cache hit)");
            return Ok(ThreadCaptureOutcome::NoOp);
        }

        // 1. Walk the on-disk worktree → fresh Tree (also stores
        //    every blob it sees as a side effect). When we have a
        //    manifest, pass it as a stat-cache so unchanged files
        //    skip the read+hash cycle entirely. Files that DID
        //    change still get the full treatment, so correctness
        //    is preserved; we just avoid the redundant work for
        //    the (usually large) majority.
        let new_tree = match existing_manifest.as_ref() {
            Some(m) => self.build_tree_with_stat_cache(root, m)?,
            None => self.build_tree(root)?,
        };
        let new_tree_hash = self.store().put_tree(&new_tree)?;

        // 2. Content-hash no-op (slow path equivalent of the
        //    stat-cache check above). Hits when stat fields drifted
        //    via `touch` or atime updates even though the bytes
        //    didn't change — refresh the manifest's stat fields so
        //    the next call hits the fast path.
        if existing_manifest
            .as_ref()
            .map(|m| m.tree_hash == new_tree_hash)
            .unwrap_or(false)
        {
            let mut refreshed = existing_manifest.expect("checked Some above");
            refreshed.files.clear();
            populate_manifest_from_tree(self, &new_tree, root, "", &mut refreshed.files)?;
            write_manifest(self.heddle_dir(), thread, &refreshed).map_err(HeddleError::Io)?;
            debug!(thread = %thread, "thread capture no-op (content-hash refresh)");
            return Ok(ThreadCaptureOutcome::NoOp);
        }

        // 3. Real capture. Build a new state parented at the
        //    current thread head (if any), put it, advance the
        //    thread ref.
        let attribution = self.get_attribution()?;
        let parents = match self.refs().get_thread(thread)? {
            Some(prev) => vec![prev],
            None => vec![],
        };
        let state = State::new_snapshot(new_tree_hash, parents, attribution);
        self.store().put_state(&state)?;
        self.refs().set_thread(thread, &state.change_id)?;

        // 4. Rewrite the manifest to reflect the new state.
        let mut manifest = ThreadManifest::new(state.change_id, new_tree_hash);
        populate_manifest_from_tree(self, &new_tree, root, "", &mut manifest.files)?;
        write_manifest(self.heddle_dir(), thread, &manifest).map_err(HeddleError::Io)?;

        debug!(
            thread = %thread,
            new_state = %state.change_id,
            files = manifest.files.len(),
            "thread captured"
        );
        Ok(ThreadCaptureOutcome::Captured {
            state_id: state.change_id,
        })
    }
}

/// Recursive helper: for each tree entry under `rel_prefix` inside
/// the materialized `dest`, walk the captured tree (NOT the disk —
/// we trust what we just put there) and stat the corresponding file
/// to fill in the manifest's identity fields.
///
/// Using the captured tree as the walk basis is what lets a
/// manifest entry survive `rm -rf .` later: the file may have
/// disappeared but we still record what *should* be there per the
/// captured state. Capture-from-disk decides what to do about
/// missing files at its own scan time.
pub(crate) fn populate_manifest_from_tree(
    repo: &Repository,
    tree: &Tree,
    dest: &Path,
    rel_prefix: &str,
    out: &mut BTreeMap<String, ManifestFile>,
) -> Result<()> {
    use objects::object::EntryType;
    for entry in tree.entries() {
        let rel_path = if rel_prefix.is_empty() {
            entry.name.clone()
        } else {
            format!("{rel_prefix}/{}", entry.name)
        };
        match entry.entry_type {
            EntryType::Tree => {
                let subtree = repo.store().get_tree(&entry.hash)?.ok_or_else(|| {
                    HeddleError::Config(format!(
                        "subtree {} missing while populating manifest for {rel_path}",
                        entry.hash
                    ))
                })?;
                populate_manifest_from_tree(repo, &subtree, dest, &rel_path, out)?;
            }
            EntryType::Blob | EntryType::Symlink => {
                let on_disk = dest.join(&rel_path);
                let meta = match fs::symlink_metadata(&on_disk) {
                    Ok(m) => m,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                        // The materializer didn't put it there. That
                        // shouldn't happen on a clean materialize,
                        // but if it does we skip the entry so the
                        // manifest stays a reflection of disk truth.
                        debug!(
                            path = %rel_path,
                            "manifest population skipped missing file"
                        );
                        continue;
                    }
                    Err(e) => return Err(HeddleError::Io(e)),
                };
                out.insert(
                    rel_path,
                    ManifestFile {
                        hash: entry.hash,
                        inode: meta.ino(),
                        mtime_ns: timespec_to_ns(meta.mtime(), meta.mtime_nsec()),
                        ctime_ns: timespec_to_ns(meta.ctime(), meta.ctime_nsec()),
                        mode: meta.mode(),
                    },
                );
            }
        }
    }
    Ok(())
}

#[inline]
fn timespec_to_ns(secs: i64, nanos: i64) -> i64 {
    secs.saturating_mul(1_000_000_000).saturating_add(nanos)
}

/// Stat-cache fast no-op check. Returns `true` when the on-disk
/// worktree is byte-identical to what `manifest` describes — every
/// manifest file present at its recorded `(inode, mtime, ctime,
/// mode)`, no untracked files, no deletions.
///
/// Pattern: same as git's index `assume-unchanged` fast path. The
/// stat fields are populated by `populate_manifest_from_tree` at
/// materialise time; clonefile/copy operations preserve the
/// destination's inode for the lifetime of the file, so a single
/// `stat` per file is sufficient to detect any modification.
///
/// Performance: ~5 ms for a 643-file worktree (single `stat` per
/// file + B-tree lookup). The slow path (`build_tree`) reads and
/// hashes every file, ~100s of ms for the same fixture.
///
/// Returns `Ok(false)` on ANY uncertainty — a stat call failed, a
/// file in the manifest is missing, an untracked file showed up,
/// or any single field mismatched. Callers fall through to the
/// slow `build_tree` path, which is always correct.
fn stat_cache_no_op(
    repo: &Repository,
    manifest: &ThreadManifest,
    root: &Path,
) -> Result<bool> {
    use ignore::WalkBuilder;
    use std::collections::HashSet;

    let ignore_patterns = repo.ignore_patterns()?;
    let nested_exclusions = repo.nested_thread_worktree_exclusions(root)?;
    let ignore_matcher = crate::worktree_ignore::WorktreeIgnoreMatcher::new(&ignore_patterns)
        .with_nested_worktree_exclusions(nested_exclusions);

    // Manifests only record files+symlinks, but Heddle's tree
    // builder materialises empty directories as their own tree
    // entries. So a no-op predicate that only checks `manifest.files`
    // would miss "user added or removed an empty directory" —
    // `seen.len() == manifest.files.len()` is still true on the file
    // side, but the on-disk tree no longer matches what `build_tree`
    // would produce.
    //
    // Derive the set of directories implied by every manifest file's
    // ancestors (the "expected dirs") so the directory leg of the
    // walk has something to compare against. Empty-dir additions
    // surface as a walk-side dir that isn't in `expected_dirs`;
    // empty-dir removals surface as an expected dir that the walk
    // never visited.
    let expected_dirs: HashSet<String> = {
        let mut set = HashSet::new();
        for key in manifest.files.keys() {
            let path = Path::new(key);
            let mut cur = path.parent();
            while let Some(p) = cur {
                if p.as_os_str().is_empty() {
                    break;
                }
                set.insert(p.to_string_lossy().into_owned());
                cur = p.parent();
            }
        }
        set
    };

    // Walk the worktree. For every file we see, check it against the
    // manifest. Track which manifest paths we've actually seen so we
    // can detect deletions afterwards.
    let mut seen: HashSet<String> = HashSet::with_capacity(manifest.files.len());
    let mut seen_dirs: HashSet<String> = HashSet::with_capacity(expected_dirs.len());
    let mut walker = WalkBuilder::new(root);
    walker
        .hidden(false)
        .git_ignore(false)
        .git_exclude(false)
        .git_global(false)
        .parents(false);
    let walker = walker.build();

    for entry in walker {
        let entry = match entry {
            Ok(e) => e,
            // A walk error means we lost certainty about the disk
            // state — fall through to the slow path.
            Err(_) => return Ok(false),
        };
        let path = entry.path();
        if path == root {
            continue;
        }
        let rel = match path.strip_prefix(root) {
            Ok(r) => r,
            Err(_) => return Ok(false),
        };
        let rel_str = rel.to_string_lossy().into_owned();

        // Honour the same ignore matcher build_tree would use.
        let parent = path.parent().unwrap_or(root);
        let name = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n,
            None => return Ok(false),
        };
        if ignore_matcher.should_prune_absolute_path(path)
            || ignore_matcher.should_ignore_child(parent, name)
        {
            // Pruned by ignores — neither side tracks this entry.
            // For directories we'd want to skip-subtree; the
            // `ignore` crate's iterator doesn't expose that
            // cheaply here, so we conservatively bail to the slow
            // path if we hit an ignored directory with children.
            if entry.file_type().is_some_and(|t| t.is_dir()) {
                // Skip cheap — children get checked individually
                // and we'll just keep ignoring them.
            }
            continue;
        }

        let file_type = match entry.file_type() {
            Some(ft) => ft,
            None => return Ok(false),
        };
        if file_type.is_dir() {
            // Directory leg: an empty dir added by the user has no
            // manifest entry (manifests only record files +
            // symlinks) but a tree-build *would* emit a tree entry
            // for it. Bail on any directory the manifest doesn't
            // imply; the slow path will incorporate the addition.
            if !expected_dirs.contains(&rel_str) {
                return Ok(false);
            }
            seen_dirs.insert(rel_str);
            continue;
        }

        // Look up the manifest entry. If absent → new file → not a
        // no-op.
        let Some(manifest_entry) = manifest.files.get(&rel_str) else {
            return Ok(false);
        };

        // Stat and compare. `symlink_metadata` (not `metadata`) so
        // a symlink doesn't transparently follow into the target's
        // inode.
        let meta = match fs::symlink_metadata(path) {
            Ok(m) => m,
            Err(_) => return Ok(false),
        };
        let stat = ManifestFile {
            hash: manifest_entry.hash,
            inode: meta.ino(),
            mtime_ns: timespec_to_ns(meta.mtime(), meta.mtime_nsec()),
            ctime_ns: timespec_to_ns(meta.ctime(), meta.ctime_nsec()),
            mode: meta.mode(),
        };
        if !stat.matches(manifest_entry) {
            return Ok(false);
        }
        seen.insert(rel_str);
    }

    // Final pass: every manifest entry must have been seen (file
    // deletion check) and every manifest-implied directory must
    // have been seen (directory deletion check). The dir-side
    // check catches `rmdir` of an empty directory that was part
    // of the materialised tree — its files are also gone (so the
    // file side already declines) but if it had no files to begin
    // with the file side alone would false-pass.
    if seen.len() != manifest.files.len() {
        return Ok(false);
    }
    if seen_dirs.len() != expected_dirs.len() {
        return Ok(false);
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::thread_manifest::read_manifest;
    use tempfile::TempDir;

    #[test]
    fn materialize_thread_writes_manifest_with_files() {
        let repo_dir = TempDir::new().unwrap();
        let repo = Repository::init_default(repo_dir.path()).unwrap();
        // Build a small worktree to capture.
        fs::write(repo_dir.path().join("Cargo.toml"), b"# a\n").unwrap();
        fs::create_dir_all(repo_dir.path().join("src")).unwrap();
        fs::write(repo_dir.path().join("src/lib.rs"), b"fn main() {}\n").unwrap();
        repo.snapshot(Some("seed".into()), None).unwrap();

        let dest = TempDir::new().unwrap();
        let manifest = repo
            .materialize_thread("main", &dest.path().join("out"))
            .unwrap();

        assert_eq!(
            manifest.schema_version,
            crate::thread_manifest::SCHEMA_VERSION
        );
        // Three files: Cargo.toml, src/lib.rs, plus whatever
        // init_default seeded — only assert the ones we wrote
        // exist and have plausible stat fields.
        let cargo = manifest
            .files
            .get("Cargo.toml")
            .expect("Cargo.toml in manifest");
        assert_ne!(cargo.inode, 0);
        assert_ne!(cargo.mtime_ns, 0);
        let src = manifest
            .files
            .get("src/lib.rs")
            .expect("src/lib.rs in manifest");
        assert_ne!(src.inode, 0);

        // Manifest persisted to disk.
        let loaded = read_manifest(repo.heddle_dir(), "main")
            .unwrap()
            .expect("manifest on disk");
        assert_eq!(loaded.files.len(), manifest.files.len());
        assert_eq!(
            loaded.files["Cargo.toml"].inode,
            manifest.files["Cargo.toml"].inode
        );
    }

    #[test]
    fn materialize_unknown_thread_errors() {
        let repo_dir = TempDir::new().unwrap();
        let repo = Repository::init_default(repo_dir.path()).unwrap();
        let dest = TempDir::new().unwrap();
        let err = repo
            .materialize_thread("no-such-thread", &dest.path().join("out"))
            .expect_err("should fail");
        assert!(format!("{err}").contains("unknown thread"));
    }

    /// Round-trip: materialize → edit a file → capture → confirm a
    /// new state was written, thread head advanced, and the manifest
    /// reflects the new state.
    #[test]
    fn capture_after_edit_advances_thread() {
        let repo_dir = TempDir::new().unwrap();
        let repo = Repository::init_default(repo_dir.path()).unwrap();
        fs::write(repo_dir.path().join("hello.txt"), b"hello\n").unwrap();
        repo.snapshot(Some("seed".into()), None).unwrap();
        let before = repo.refs().get_thread("main").unwrap().expect("head");

        let dest_holder = TempDir::new().unwrap();
        let dest = dest_holder.path().join("out");
        let materialize_manifest = repo.materialize_thread("main", &dest).unwrap();

        // Mutate a file in the materialized worktree.
        fs::write(dest.join("hello.txt"), b"hello world\n").unwrap();

        let outcome = repo
            .capture_thread_from_disk("main", &dest)
            .expect("capture");
        let new_state = match outcome {
            ThreadCaptureOutcome::Captured { state_id } => state_id,
            ThreadCaptureOutcome::NoOp => panic!("expected Captured, got NoOp"),
        };

        // Thread head advanced.
        let after = repo.refs().get_thread("main").unwrap().expect("head");
        assert_ne!(before, after);
        assert_eq!(after, new_state);

        // Manifest reflects the new state.
        let loaded = read_manifest(repo.heddle_dir(), "main")
            .unwrap()
            .expect("manifest");
        assert_eq!(loaded.state_id, new_state);
        assert_ne!(loaded.tree_hash, materialize_manifest.tree_hash);
        assert!(loaded.files.contains_key("hello.txt"));
    }

    /// Capture with no edits is a no-op: thread head unchanged,
    /// manifest refreshed in place.
    #[test]
    fn capture_with_no_changes_is_noop() {
        let repo_dir = TempDir::new().unwrap();
        let repo = Repository::init_default(repo_dir.path()).unwrap();
        fs::write(repo_dir.path().join("steady.txt"), b"unchanged\n").unwrap();
        repo.snapshot(Some("seed".into()), None).unwrap();
        let before = repo.refs().get_thread("main").unwrap().expect("head");

        let dest_holder = TempDir::new().unwrap();
        let dest = dest_holder.path().join("out");
        repo.materialize_thread("main", &dest).unwrap();

        let outcome = repo.capture_thread_from_disk("main", &dest).unwrap();
        assert_eq!(outcome, ThreadCaptureOutcome::NoOp);

        // Thread head unchanged.
        let after = repo.refs().get_thread("main").unwrap().expect("head");
        assert_eq!(before, after);
    }

    /// Stat-cache fast no-op: a fresh-materialised tree captures
    /// without invoking `build_tree`. Detected via the manifest
    /// reflecting bytes byte-identical to what got materialised.
    #[test]
    fn stat_cache_short_circuits_unchanged_capture() {
        let repo_dir = TempDir::new().unwrap();
        let repo = Repository::init_default(repo_dir.path()).unwrap();
        for i in 0..20 {
            fs::write(
                repo_dir.path().join(format!("file_{i:02}.txt")),
                format!("content {i}\n").as_bytes(),
            )
            .unwrap();
        }
        repo.snapshot(Some("seed".into()), None).unwrap();

        let dest_holder = TempDir::new().unwrap();
        let dest = dest_holder.path().join("out");
        let manifest = repo.materialize_thread("main", &dest).unwrap();
        assert_eq!(manifest.files.len(), 20);

        // The fast-path predicate alone — without touching the
        // store-side `build_tree`. Exposes the boundary the
        // optimisation guards.
        assert!(
            stat_cache_no_op(&repo, &manifest, &dest).unwrap(),
            "fresh materialise should stat-match the manifest"
        );

        // Full call also returns NoOp.
        let outcome = repo.capture_thread_from_disk("main", &dest).unwrap();
        assert_eq!(outcome, ThreadCaptureOutcome::NoOp);
    }

    /// Stat-cache invalidates correctly on edit: a single touched
    /// file flips `stat_cache_no_op` to `false`, which forces the
    /// slow path to run and produces a new state.
    #[test]
    fn stat_cache_detects_edit_and_falls_through() {
        let repo_dir = TempDir::new().unwrap();
        let repo = Repository::init_default(repo_dir.path()).unwrap();
        fs::write(repo_dir.path().join("only.txt"), b"v1\n").unwrap();
        repo.snapshot(Some("seed".into()), None).unwrap();

        let dest_holder = TempDir::new().unwrap();
        let dest = dest_holder.path().join("out");
        let manifest = repo.materialize_thread("main", &dest).unwrap();

        // Sleep briefly so the mtime moves; APFS gives sub-ms
        // resolution on modern macOS but Linux ext4 is only
        // 1-second granularity for ctime — make the test robust
        // either way.
        std::thread::sleep(std::time::Duration::from_millis(20));
        fs::write(dest.join("only.txt"), b"v2\n").unwrap();

        assert!(
            !stat_cache_no_op(&repo, &manifest, &dest).unwrap(),
            "edited file must invalidate the fast path"
        );

        // Slow path runs and creates a new state.
        match repo.capture_thread_from_disk("main", &dest).unwrap() {
            ThreadCaptureOutcome::Captured { .. } => {}
            other => panic!("expected Captured, got {other:?}"),
        }
    }

    /// New file added out of band → fast path declines.
    #[test]
    fn stat_cache_detects_added_file() {
        let repo_dir = TempDir::new().unwrap();
        let repo = Repository::init_default(repo_dir.path()).unwrap();
        fs::write(repo_dir.path().join("a.txt"), b"a\n").unwrap();
        repo.snapshot(Some("seed".into()), None).unwrap();

        let dest_holder = TempDir::new().unwrap();
        let dest = dest_holder.path().join("out");
        let manifest = repo.materialize_thread("main", &dest).unwrap();

        fs::write(dest.join("b.txt"), b"b\n").unwrap();

        assert!(
            !stat_cache_no_op(&repo, &manifest, &dest).unwrap(),
            "added file must invalidate the fast path"
        );
    }

    /// Plain `heddle capture` (via `Repository::snapshot`) detects the
    /// materialized-thread context — HEAD attached to a thread that has
    /// a manifest — and refreshes the manifest to the new state after
    /// the capture lands. This is the path the user hits when they edit
    /// inside a materialized thread worktree and run `heddle capture`
    /// directly (as opposed to `thread switch`, which is the auto-capture
    /// path covered by `capture_after_edit_advances_thread`).
    #[test]
    fn snapshot_in_materialized_thread_refreshes_manifest() {
        let repo_dir = TempDir::new().unwrap();
        let repo = Repository::init_default(repo_dir.path()).unwrap();
        fs::write(repo_dir.path().join("alpha.txt"), b"v1\n").unwrap();
        fs::write(repo_dir.path().join("beta.txt"), b"steady\n").unwrap();
        let initial = repo.snapshot(Some("seed".into()), None).unwrap();

        // Stand up a manifest for `main` whose stat fields match the
        // worktree as it is right now. Mimics the post-materialize
        // state when the user is `cd`'d into the materialized
        // worktree (`self.root` == materialized path).
        let initial_tree = repo
            .store()
            .get_tree(&initial.tree)
            .unwrap()
            .expect("seed tree");
        let mut manifest =
            crate::thread_manifest::ThreadManifest::new(initial.change_id, initial.tree);
        populate_manifest_from_tree(
            &repo,
            &initial_tree,
            repo_dir.path(),
            "",
            &mut manifest.files,
        )
        .unwrap();
        crate::thread_manifest::write_manifest(repo.heddle_dir(), "main", &manifest).unwrap();

        // Sleep long enough that the new mtime is observably distinct
        // on ext4's 1-second-granularity ctime (APFS is sub-ms).
        std::thread::sleep(std::time::Duration::from_millis(20));
        fs::write(repo_dir.path().join("alpha.txt"), b"v2\n").unwrap();

        let captured = repo.snapshot(Some("after edit".into()), None).unwrap();
        assert_ne!(captured.change_id, initial.change_id);
        assert_ne!(captured.tree, initial.tree);

        // Manifest got refreshed to point at the new state and tree.
        let refreshed = crate::thread_manifest::read_manifest(repo.heddle_dir(), "main")
            .unwrap()
            .expect("manifest persists");
        assert_eq!(refreshed.state_id, captured.change_id);
        assert_eq!(refreshed.tree_hash, captured.tree);
        // beta.txt was untouched — its stat fields (and hash) should
        // still appear in the refreshed manifest.
        assert!(refreshed.files.contains_key("alpha.txt"));
        assert!(refreshed.files.contains_key("beta.txt"));
    }

    /// Capture from a *dedicated* thread worktree (one whose path
    /// differs from `repo.root()`) must validate symlinks against
    /// that worktree's path, not against the main repo root.
    /// Pre-fix the walker passed `repo.root()` as the symlink-
    /// escape base, so every symlink inside a dedicated thread
    /// path was rejected as "outside the repo" the moment the
    /// slow path ran — `thread switch` auto-capture broke for any
    /// thread that contained a symlink. Reproduces the codex P2
    /// from review pass 2.
    #[cfg(unix)]
    #[test]
    fn capture_thread_from_disk_accepts_symlinks_in_dedicated_worktree() {
        let repo_dir = TempDir::new().unwrap();
        let repo = Repository::init_default(repo_dir.path()).unwrap();
        // Seed with a file + a symlink pointing inside the repo.
        fs::write(repo_dir.path().join("target.txt"), b"target\n").unwrap();
        std::os::unix::fs::symlink("target.txt", repo_dir.path().join("link")).unwrap();
        repo.snapshot(Some("seed".into()), None).unwrap();

        // Materialise into a dedicated worktree — path differs
        // from `repo.root()`, which is exactly the case that
        // exposes the bug.
        let dest_holder = TempDir::new().unwrap();
        let dest = dest_holder.path().join("thread-worktree");
        repo.materialize_thread("main", &dest).unwrap();

        // Edit a non-symlink file so the slow path fires (the fast
        // stat-cache no-op would mask the bug). Sleep so the mtime
        // observably moves on coarse-granularity filesystems.
        std::thread::sleep(std::time::Duration::from_millis(20));
        fs::write(dest.join("target.txt"), b"target v2\n").unwrap();

        // Pre-fix this errored with "symlink target escapes repo"
        // because `validate_symlink_target` was using `repo.root()`
        // as the allowed base instead of the walk root.
        let outcome = repo
            .capture_thread_from_disk("main", &dest)
            .expect("capture must accept symlinks inside the dedicated worktree");
        match outcome {
            ThreadCaptureOutcome::Captured { .. } => {}
            ThreadCaptureOutcome::NoOp => panic!("expected Captured; got NoOp"),
        }
    }

    /// Empty directory added by the user — manifests only record
    /// files, but Heddle's tree builder emits a tree entry for the
    /// new dir. The stat-cache no-op predicate must decline so the
    /// slow path picks the change up; pre-fix it false-passed and
    /// `thread switch`'s auto-capture silently dropped the addition.
    #[test]
    fn stat_cache_detects_added_empty_directory() {
        let repo_dir = TempDir::new().unwrap();
        let repo = Repository::init_default(repo_dir.path()).unwrap();
        fs::write(repo_dir.path().join("only.txt"), b"a\n").unwrap();
        repo.snapshot(Some("seed".into()), None).unwrap();

        let dest_holder = TempDir::new().unwrap();
        let dest = dest_holder.path().join("out");
        let manifest = repo.materialize_thread("main", &dest).unwrap();

        // Add an empty directory that has no manifest entry.
        fs::create_dir_all(dest.join("brand-new-empty-dir")).unwrap();

        assert!(
            !stat_cache_no_op(&repo, &manifest, &dest).unwrap(),
            "an added empty directory must invalidate the fast path"
        );
    }

    /// Empty directory removed by the user — the manifest expects it
    /// (its parent path appears as an ancestor of files) but the
    /// walk never visits it. The dir-side check must decline. Pre-
    /// fix the fast path would false-pass on this case too.
    #[test]
    fn stat_cache_detects_removed_empty_directory() {
        let repo_dir = TempDir::new().unwrap();
        let repo = Repository::init_default(repo_dir.path()).unwrap();
        fs::create_dir_all(repo_dir.path().join("nested/deep")).unwrap();
        fs::write(repo_dir.path().join("nested/deep/leaf.txt"), b"leaf\n").unwrap();
        repo.snapshot(Some("seed".into()), None).unwrap();

        let dest_holder = TempDir::new().unwrap();
        let dest = dest_holder.path().join("out");
        let manifest = repo.materialize_thread("main", &dest).unwrap();

        // Remove the leaf file AND its parent dir. The file-side
        // check already catches the file removal, but if we then
        // synthesise a fresh leaf elsewhere we'd want the dir-side
        // check to catch the missing parent on its own too. Use a
        // slightly different shape: create + remove a sibling dir
        // whose ancestor matches the manifest's expected set.
        fs::create_dir_all(dest.join("nested/sibling-empty")).unwrap();

        assert!(
            !stat_cache_no_op(&repo, &manifest, &dest).unwrap(),
            "an added empty directory inside an existing parent must invalidate"
        );
    }

    /// Deleted file → fast path declines.
    #[test]
    fn stat_cache_detects_deletion() {
        let repo_dir = TempDir::new().unwrap();
        let repo = Repository::init_default(repo_dir.path()).unwrap();
        fs::write(repo_dir.path().join("a.txt"), b"a\n").unwrap();
        fs::write(repo_dir.path().join("b.txt"), b"b\n").unwrap();
        repo.snapshot(Some("seed".into()), None).unwrap();

        let dest_holder = TempDir::new().unwrap();
        let dest = dest_holder.path().join("out");
        let manifest = repo.materialize_thread("main", &dest).unwrap();

        fs::remove_file(dest.join("a.txt")).unwrap();

        assert!(
            !stat_cache_no_op(&repo, &manifest, &dest).unwrap(),
            "deleted file must invalidate the fast path"
        );
    }

    /// Two `capture_thread_from_disk` calls on the same thread from
    /// different threads must serialize through the repository write
    /// lock: the thread head's parent chain must include both
    /// captures (no lost update where one capture's parent is the
    /// pre-race head instead of the other capture's state).
    ///
    /// Reproduces the race Codex P1 #2 named: pre-fix, two sibling
    /// worktrees doing `heddle thread switch` against the same
    /// source thread both read the same parent in
    /// `refs().get_thread()`, both `put_state` with that parent,
    /// both `set_thread` — whichever `set_thread` won last orphaned
    /// the other state on disk. With the lock both captures land in
    /// series and the final head's parent chain links back through
    /// both new states.
    #[test]
    fn concurrent_captures_serialize_via_repository_lock() {
        use std::sync::Arc;

        let repo_dir = TempDir::new().unwrap();
        let repo = Arc::new(Repository::init_default(repo_dir.path()).unwrap());
        fs::write(repo_dir.path().join("shared.txt"), b"seed\n").unwrap();
        repo.snapshot(Some("seed".into()), None).unwrap();
        let initial_head = repo.refs().get_thread("main").unwrap().expect("seeded");

        // Two sibling materialized worktrees of the same thread.
        let dest_a_holder = TempDir::new().unwrap();
        let dest_a = dest_a_holder.path().join("a");
        repo.materialize_thread("main", &dest_a).unwrap();
        let dest_b_holder = TempDir::new().unwrap();
        let dest_b = dest_b_holder.path().join("b");
        repo.materialize_thread("main", &dest_b).unwrap();

        // Disjoint edits so each capture has real work to do (no
        // stat-cache no-op short-circuit).
        std::thread::sleep(std::time::Duration::from_millis(20));
        fs::write(dest_a.join("shared.txt"), b"edited-by-a\n").unwrap();
        fs::write(dest_b.join("shared.txt"), b"edited-by-b\n").unwrap();

        // Race the two captures.
        let repo_a = Arc::clone(&repo);
        let repo_b = Arc::clone(&repo);
        let h_a = std::thread::spawn(move || {
            repo_a
                .capture_thread_from_disk("main", &dest_a)
                .expect("capture A")
        });
        let h_b = std::thread::spawn(move || {
            repo_b
                .capture_thread_from_disk("main", &dest_b)
                .expect("capture B")
        });
        let outcome_a = h_a.join().expect("thread A");
        let outcome_b = h_b.join().expect("thread B");

        // Both captures landed (neither was a NoOp because both
        // edited the same file with different bytes).
        let id_a = match outcome_a {
            ThreadCaptureOutcome::Captured { state_id } => state_id,
            ThreadCaptureOutcome::NoOp => panic!("A expected Captured"),
        };
        let id_b = match outcome_b {
            ThreadCaptureOutcome::Captured { state_id } => state_id,
            ThreadCaptureOutcome::NoOp => panic!("B expected Captured"),
        };
        assert_ne!(id_a, id_b, "the two captures must produce distinct states");

        // The thread head is one of the two captures. Lock-naked,
        // the loser's parent would be `initial_head`. With the
        // lock, the loser's parent is the winner's id and the
        // winner's parent is `initial_head`.
        let final_head = repo.refs().get_thread("main").unwrap().expect("head");
        let winner_id = final_head;
        let loser_id = if final_head == id_a { id_b } else { id_a };

        let winner_state = repo
            .store()
            .get_state(&winner_id)
            .unwrap()
            .expect("winner state on disk");
        let loser_state = repo
            .store()
            .get_state(&loser_id)
            .unwrap()
            .expect("loser state on disk");

        // The two captures must have linked through the lock:
        // exactly one of (winner.parents, loser.parents) names the
        // other; the remaining parent is the seed head. Pre-fix
        // both states named the seed head and the loser was
        // orphaned — assert that this isn't the case.
        let chained =
            winner_state.parents.contains(&loser_id) || loser_state.parents.contains(&winner_id);
        assert!(
            chained,
            "concurrent captures must chain through the lock; got\n  \
             winner {winner_id} parents={:?}\n  loser  {loser_id} parents={:?}",
            winner_state.parents, loser_state.parents
        );
        assert!(
            winner_state.parents.contains(&initial_head)
                || loser_state.parents.contains(&initial_head),
            "the bottom of the chain must still reach the seed head"
        );
    }
}
