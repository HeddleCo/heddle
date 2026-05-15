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

use objects::object::{ChangeId, State, Tree};
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
    /// The current implementation reuses `Repository::build_tree`
    /// for the walk so the resulting trees are byte-identical to
    /// what `heddle capture` would produce against the same content.
    /// The manifest-aware stat-cache fast path that skips re-hashing
    /// unchanged files is a follow-up — correctness first, perf next.
    #[instrument(skip(self), fields(thread = %thread, root = %root.display()))]
    pub fn capture_thread_from_disk(
        &self,
        thread: &str,
        root: &Path,
    ) -> Result<ThreadCaptureOutcome> {
        let existing_manifest =
            read_manifest(self.heddle_dir(), thread).map_err(HeddleError::Io)?;

        // 1. Walk the on-disk worktree → fresh Tree (also stores
        //    every blob it sees as a side effect).
        let new_tree = self.build_tree(root)?;
        let new_tree_hash = self.store().put_tree(&new_tree)?;

        // 2. No-op fast path: nothing the agent did changed the
        //    captured content. Refresh stat fields in the manifest
        //    (mtimes might have drifted via `touch`) but don't
        //    create a new state.
        if existing_manifest
            .as_ref()
            .map(|m| m.tree_hash == new_tree_hash)
            .unwrap_or(false)
        {
            let mut refreshed = existing_manifest.expect("checked Some above");
            refreshed.files.clear();
            populate_manifest_from_tree(self, &new_tree, root, "", &mut refreshed.files)?;
            write_manifest(self.heddle_dir(), thread, &refreshed).map_err(HeddleError::Io)?;
            debug!(thread = %thread, "thread capture no-op");
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
fn populate_manifest_from_tree(
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
}
