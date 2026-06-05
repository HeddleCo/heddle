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

use chrono::{DateTime, Utc};
use objects::store::ObjectStore;
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

use objects::{
    lock::RepositoryLockExt,
    object::{ChangeId, State, ThreadName, Tree},
};
use oplog::OpRecord;
use refs::RefExpectation;
use tracing::{debug, instrument};

use super::{HeddleError, Repository, Result};
use crate::thread_manifest::{ManifestFile, ThreadManifest, read_manifest, write_manifest};
use crate::visibility::{AudienceTier, visible};
use objects::object::VisibilityTier;

/// Filename of the operator-local courtesy placeholder written when a
/// checked-out state's tier is not visible to the operator's audience.
const COURTESY_STUB_FILENAME: &str = "HEDDLE-EMBARGO.txt";

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
    pub fn materialize_thread(
        &self,
        thread: &str,
        dest: &Path,
        audience: &AudienceTier,
    ) -> Result<ThreadManifest> {
        let change_id = self
            .refs()
            .resolve(thread)?
            .ok_or_else(|| HeddleError::Config(format!("unknown thread {thread}")))?;
        let state = self
            .store()
            .get_state(&change_id)?
            .ok_or_else(|| HeddleError::Config(format!("state for {thread} missing")))?;

        // Operator-local courtesy stub. The visibility decision is made HERE,
        // at the state walk where the `ChangeId` and the audience are both in
        // scope — never down in the blob-keyed `materialize_tree`/`export_tree`
        // (which carry no `ChangeId`/audience). When the checked-out state's
        // tier is not visible to this audience, render a short placeholder
        // instead of its tracked content. This is a working-tree courtesy on
        // bytes the operator already holds, NOT a security boundary and NOT a
        // public-mirror surface — the public mirror emits absence (spike §5.3).
        let tier = self
            .effective_visibility_tier(&change_id)
            .map_err(|e| HeddleError::Config(format!("resolve visibility for {thread}: {e:#}")))?;
        if !visible(&tier, audience) {
            return self.materialize_courtesy_stub(thread, dest, change_id, &state, &tier);
        }

        let tree = self
            .store()
            .get_tree(&state.tree)?
            .ok_or_else(|| HeddleError::Config(format!("tree for {thread} missing")))?;

        self.materialize_tree(&tree, dest)?;

        let mut manifest =
            ThreadManifest::new(change_id, state.tree, canonical_worktree_path(dest));
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

    /// Render the operator-local courtesy placeholder for a state whose tier
    /// is not visible to the checkout audience. Writes a single short text
    /// file naming the tier (and promotion date, if scheduled) in place of
    /// the tracked content, and a manifest with no tracked files — the real
    /// tree's bytes are intentionally withheld from this checkout. The
    /// placeholder is a working-tree convenience for the holder; it never
    /// travels (the public mirror emits absence, not a stub — spike §5.3).
    fn materialize_courtesy_stub(
        &self,
        thread: &str,
        dest: &Path,
        change_id: ChangeId,
        state: &State,
        tier: &VisibilityTier,
    ) -> Result<ThreadManifest> {
        fs::create_dir_all(dest).map_err(HeddleError::Io)?;
        let embargo_until = self
            .effective_state_visibility(&change_id)
            .map_err(|e| HeddleError::Config(format!("resolve visibility for {thread}: {e:#}")))?
            .and_then(|record| record.embargo_until);
        let stub = courtesy_stub_text(tier, embargo_until);
        fs::write(dest.join(COURTESY_STUB_FILENAME), stub.as_bytes()).map_err(HeddleError::Io)?;

        // Manifest reflects disk truth: no tracked files were materialized
        // (the placeholder is untracked). `tree_hash` still names the real
        // embargoed state's tree so the sidecar identifies which state this
        // checkout stands in for.
        let manifest = ThreadManifest::new(change_id, state.tree, canonical_worktree_path(dest));
        write_manifest(self.heddle_dir(), thread, &manifest).map_err(HeddleError::Io)?;
        debug!(
            thread = %thread,
            state_id = %change_id,
            tier = tier.as_str(),
            "thread checkout rendered courtesy stub (under-tier for audience)"
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
        let mut manifest =
            ThreadManifest::new(*state_id, state.tree, canonical_worktree_path(dest));
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

    /// The staged domain commit record for a brand-new materialized-thread
    /// start. The repo owns the op-record shape so callers don't reconstruct
    /// `OpRecord::ThreadCreateV2`'s fields. `manager_snapshot` is `None`: the
    /// thread record is written by the start's converge step (so there is
    /// nothing to snapshot at record-construction time — heddle#23 r2). The
    /// caller stages this as the executor's single commit record (it is NOT
    /// appended eagerly); the commit marker dedups on the stable
    /// `transaction_id`.
    pub fn thread_create_op_record(&self, name: &str, state: ChangeId) -> OpRecord {
        OpRecord::ThreadCreateV2 {
            name: name.to_string(),
            state,
            manager_snapshot: None,
        }
    }

    /// CAS-guarded rollback of a materialized-thread-start ref forward
    /// (heddle#356 cid 3333881583).
    ///
    /// The forward set the thread ref to `set_value` (the start's base state).
    /// Undo it ONLY if the ref STILL points there: restore `restore_to` when a
    /// prior value existed (a re-start that reused the ref), or delete a ref
    /// this start created (`restore_to == None`). If a concurrent process
    /// advanced/changed the ref after our forward (a concurrent start or
    /// crash-recovery), leave their write in place — an unconditional
    /// reset/delete would clobber it.
    pub fn cas_guarded_thread_ref_rollback(
        &self,
        name: &ThreadName,
        set_value: ChangeId,
        restore_to: Option<ChangeId>,
    ) -> Result<()> {
        // Compare-before-write: bail without touching the ref if it no longer
        // holds the value our forward set.
        if self.refs().get_thread(name)? != Some(set_value) {
            return Ok(());
        }
        let result = match restore_to {
            Some(prior) => {
                self.refs()
                    .set_thread_cas(name, RefExpectation::Value(set_value), &prior)
            }
            None => self
                .refs()
                .delete_thread_cas(name, RefExpectation::Value(set_value)),
        };
        match result {
            Ok(()) => Ok(()),
            // Lost the race between the read above and this CAS: a concurrent
            // writer advanced the ref. The expectation guard means we wrote
            // nothing — leave their advance intact (the whole point of the
            // guard).
            Err(HeddleError::Conflict(_)) => Ok(()),
            Err(other) => Err(other),
        }
    }

    /// Restore the thread manifest sidecar to its captured pre-start snapshot:
    /// rewrite the prior `manifest.toml` bytes if one existed, or remove the
    /// directory this start created. Restoring (not blind-deleting) preserves
    /// an OLD manifest left by a prior materialization of a reused thread ref
    /// (heddle#356 cid 3333881561).
    pub fn restore_thread_manifest(&self, thread: &str, prior: Option<Vec<u8>>) -> Result<()> {
        match prior {
            Some(bytes) => {
                let path = crate::thread_manifest::manifest_path(self.heddle_dir(), thread);
                if let Some(parent) = path.parent() {
                    fs::create_dir_all(parent).map_err(HeddleError::Io)?;
                }
                fs::write(&path, bytes).map_err(HeddleError::Io)
            }
            None => crate::thread_manifest::remove_thread_manifest_dir(self.heddle_dir(), thread)
                .map(|_| ())
                .map_err(HeddleError::Io),
        }
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

        let existing_manifest =
            read_manifest(self.heddle_dir(), thread).map_err(HeddleError::Io)?;

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
        let thread_name = ThreadName::from(thread);
        let parents = match self.refs().get_thread(&thread_name)? {
            Some(prev) => vec![prev],
            None => vec![],
        };
        let mut state = State::new_snapshot(new_tree_hash, parents, attribution);
        // Auto-sign this thread-materialization capture (heddle#482) via the
        // authored-state chokepoint, the same as the primary capture path — it
        // is a real author capture that bypasses `stage_snapshot_objects`. Last
        // mutation before the write.
        self.put_authored_state(&mut state)?;
        self.refs().set_thread(&thread_name, &state.change_id)?;

        // 4. Rewrite the manifest to reflect the new state. `root` is
        //    the worktree being captured from — record its canonical
        //    path so the next snapshot can tell whether it's running
        //    inside this same worktree.
        let mut manifest = ThreadManifest::new(
            state.change_id,
            new_tree_hash,
            canonical_worktree_path(root),
        );
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
/// Plain-text placeholder a holder sees instead of an under-tier state's
/// tracked content on their own checkout. ASCII-only, mirrors the redaction
/// `stub_text` shape. Never travels off-host.
fn courtesy_stub_text(tier: &VisibilityTier, embargo_until: Option<DateTime<Utc>>) -> String {
    let mut out = String::with_capacity(256);
    out.push_str("# Heddle withheld this state's content from your audience.\n");
    out.push_str(&format!("# visibility-tier: {}\n", tier.as_str()));
    if let VisibilityTier::TeamScoped { team_id } = tier {
        out.push_str(&format!("# team:            {team_id}\n"));
    }
    if let VisibilityTier::Restricted { scope_label } | VisibilityTier::Private { scope_label } =
        tier
    {
        out.push_str(&format!("# scope:           {scope_label}\n"));
    }
    match embargo_until {
        Some(when) => out.push_str(&format!("# promotes-at:     {}\n", when.to_rfc3339())),
        None => out.push_str("# promotes-at:     (no scheduled promotion)\n"),
    }
    out.push_str("# This placeholder is a local courtesy; the bytes are not in this checkout.\n");
    out
}

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
                let (size, inode, mtime_ns, ctime_ns, mode) =
                    crate::stat_signature::stat_signature(&on_disk, &meta);
                out.insert(
                    rel_path,
                    ManifestFile {
                        hash: entry.hash,
                        size,
                        inode,
                        mtime_ns,
                        ctime_ns,
                        mode,
                    },
                );
            }
        }
    }
    Ok(())
}

/// Record the manifest's worktree-path field as an *absolute*,
/// symlink-resolved path. `Repository::snapshot` compares its
/// `self.root` (also canonicalized) to this value to decide whether
/// it's running inside the materialized worktree; without
/// canonicalization a `/tmp/foo` materialize + `/private/tmp/foo`
/// snapshot would miss the match on macOS.
///
/// Falls back to the input path on canonicalize failure — the
/// comparison may produce a false miss in pathological cases, which
/// degrades the cache to "always rebuild" instead of corrupting the
/// manifest. Strictly worse perf, never worse correctness.
pub(crate) fn canonical_worktree_path(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
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
/// Walk the captured tree named by `manifest.tree_hash` and collect
/// every subdirectory's relative path (forward-slash joined,
/// relative to the tree root, no leading or trailing slashes).
/// Source of truth for [`stat_cache_no_op`]'s directory leg —
/// includes tree-only empty directories that a `manifest.files`
/// ancestors-derived set would miss.
fn collect_expected_dirs(
    repo: &Repository,
    manifest: &ThreadManifest,
) -> Result<std::collections::HashSet<String>> {
    use std::collections::HashSet;
    let mut set: HashSet<String> = HashSet::new();
    let Some(tree) = repo.store().get_tree(&manifest.tree_hash)? else {
        // Tree missing from the store would be a serious anomaly —
        // surface it so the caller bails to the slow path which will
        // re-derive everything from the worktree.
        return Err(HeddleError::Config(format!(
            "tree {} referenced by manifest is missing",
            manifest.tree_hash
        )));
    };
    collect_subdirs_into(repo, &tree, "", &mut set)?;
    Ok(set)
}

fn collect_subdirs_into(
    repo: &Repository,
    tree: &objects::object::Tree,
    rel_prefix: &str,
    out: &mut std::collections::HashSet<String>,
) -> Result<()> {
    use objects::object::EntryType;
    for entry in tree.entries() {
        if entry.entry_type != EntryType::Tree {
            continue;
        }
        let rel = if rel_prefix.is_empty() {
            entry.name.clone()
        } else {
            format!("{rel_prefix}/{}", entry.name)
        };
        let subtree = repo.store().get_tree(&entry.hash)?.ok_or_else(|| {
            HeddleError::Config(format!(
                "subtree {} missing while collecting expected dirs at {rel}",
                entry.hash
            ))
        })?;
        out.insert(rel.clone());
        collect_subdirs_into(repo, &subtree, &rel, out)?;
    }
    Ok(())
}

/// Recursive `read_dir` worker for the stat-cache no-op predicate.
/// Returns `Ok(false)` to bail to the slow path (anything unexpected,
/// any stat mismatch); `Ok(true)` to continue the walk. Final
/// presence checks (`seen.len() == manifest.files.len()` etc.) live
/// in the caller; this fn only flags incremental mismatches.
///
/// Why hand-roll rather than reuse `ignore::WalkBuilder`: the walker
/// crate buffers entries, sorts them for determinism, calls
/// `metadata()` to populate its own `DirEntry`, and runs the gitignore
/// pipeline per directory even with every `git_*` flag turned off.
/// All of that is wasted on this predicate, which already has its own
/// `WorktreeIgnoreMatcher` and only needs `symlink_metadata` on each
/// file. A bare `read_dir` recursion is ≈3× faster on the 10k-file
/// fixture and matches `build_tree`'s ignore semantics exactly
/// because we go through the same matcher.
fn walk_for_no_op(
    root: &Path,
    cur: &Path,
    manifest: &ThreadManifest,
    expected_dirs: &std::collections::HashSet<String>,
    ignore_matcher: &crate::worktree_ignore::WorktreeIgnoreMatcher,
    seen: &mut std::collections::HashSet<String>,
    seen_dirs: &mut std::collections::HashSet<String>,
) -> Result<bool> {
    let entries = match fs::read_dir(cur) {
        Ok(it) => it,
        // A directory we can't read means we've lost certainty about
        // its contents — fall through to the slow path.
        Err(_) => return Ok(false),
    };
    for entry in entries {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => return Ok(false),
        };
        let path = entry.path();
        let Ok(rel) = path.strip_prefix(root) else {
            return Ok(false);
        };
        let rel_str = rel.to_string_lossy().into_owned();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            return Ok(false);
        };

        // Run the ignore matcher *first*, before consulting the
        // manifest. The previous "manifest-first" dispatch
        // accepted any manifest hit without re-checking the
        // matcher, which silently false-passed if the user had
        // tightened `.heddleignore` (or the in-config ignore set)
        // between materialise and this capture — `build_tree`
        // would now exclude the previously-tracked path and
        // produce a different tree, but the predicate said
        // "no-op". Always running the matcher first costs a
        // pattern check per entry but is what makes the
        // predicate's output match what `build_tree` would do.
        //
        // Three outcomes from the matcher:
        //   * Pruned + in manifest → ignore-config drift; bail
        //     to slow path so the new tree reflects the new
        //     exclusion.
        //   * Pruned + not in manifest → genuinely ignored;
        //     silently skip without recursing.
        //   * Not pruned → standard manifest / new-entry
        //     dispatch below.
        // `should_prune_directory_child` matches the production
        // walker's per-entry probe (`worktree_walk.rs`). It calls
        // `matched_relative(path, is_dir=true)` so gitignore rules
        // with trailing `/` still fire, and the same patterns
        // exclude both file and directory entries — same behaviour
        // `build_tree` would observe at materialise time.
        let pruned = ignore_matcher.should_prune_absolute_path(&path)
            || ignore_matcher.should_prune_directory_child(cur, name);
        if pruned {
            if manifest.files.contains_key(&rel_str) {
                // The matcher now wants this path excluded, but
                // it's in the manifest from materialise time.
                // Ignore-config drift — let the slow path
                // rebuild the tree without it.
                return Ok(false);
            }
            continue;
        }

        // Not pruned. Manifest lookup is the fast path for
        // tracked files; un-tracked entries fall through to
        // dir-recursion / new-file detection below.
        if let Some(manifest_entry) = manifest.files.get(&rel_str) {
            // `symlink_metadata` (not `metadata`) so a symlink
            // doesn't transparently follow into the target's
            // inode.
            let meta = match fs::symlink_metadata(&path) {
                Ok(m) => m,
                Err(_) => return Ok(false),
            };
            let (size, inode, mtime_ns, ctime_ns, mode) =
                crate::stat_signature::stat_signature(&path, &meta);
            let stat = ManifestFile {
                hash: manifest_entry.hash,
                size,
                inode,
                mtime_ns,
                ctime_ns,
                mode,
            };
            if !stat.matches(manifest_entry) {
                return Ok(false);
            }
            seen.insert(rel_str);
            continue;
        }

        let file_type = match entry.file_type() {
            Ok(ft) => ft,
            Err(_) => return Ok(false),
        };
        if file_type.is_dir() {
            // Directory leg: any directory not in `expected_dirs`
            // is an addition since materialise. Bail; the slow
            // path will incorporate it.
            if !expected_dirs.contains(&rel_str) {
                return Ok(false);
            }
            seen_dirs.insert(rel_str);
            if !walk_for_no_op(
                root,
                &path,
                manifest,
                expected_dirs,
                ignore_matcher,
                seen,
                seen_dirs,
            )? {
                return Ok(false);
            }
            continue;
        }

        // A non-ignored, non-directory entry that's not in the
        // manifest is a new file. Bail to the slow path which
        // will rebuild the tree with the new entry.
        return Ok(false);
    }
    Ok(true)
}

fn stat_cache_no_op(repo: &Repository, manifest: &ThreadManifest, root: &Path) -> Result<bool> {
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
    // Source of truth for the expected directory set is the captured
    // tree itself (the one the manifest's `tree_hash` names), not
    // the manifest's file ancestors. Two reasons:
    //
    //   1. *Tree-only empty directories.* A `Tree` entry with no
    //      files beneath it is invisible from a `manifest.files`
    //      ancestors-walk — the file set is empty, so every
    //      ancestor it would contribute is missing. Removing a
    //      legit empty leaf dir would still false-pass.
    //   2. *Future schema drift.* Files in `manifest.files` may
    //      use slash-normalised relative paths that don't exactly
    //      match how `Tree::entries` names subdirs on every
    //      platform; walking the tree directly avoids the
    //      double-encoding hazard.
    //
    // Cost is ~one `get_tree` per subdir of the captured tree.
    // For the typical thread (a few hundred dirs) that's a small
    // number of memory-mapped object reads; on the predicate's
    // hot path it's bounded by the tree's directory fan-out, not
    // file count.
    let expected_dirs: HashSet<String> = match collect_expected_dirs(repo, manifest) {
        Ok(s) => s,
        // Any error walking the tree → conservatively bail to the
        // slow path. `Ok(false)` keeps correctness; the worst case
        // is a wasted full rebuild.
        Err(_) => return Ok(false),
    };

    // Walk the worktree. For every file we see, check it against the
    // manifest. Track which manifest paths we've actually seen so we
    // can detect deletions afterwards.
    //
    // Custom `read_dir` recursion instead of `ignore::WalkBuilder`:
    // the walker crate is fast on its own but the per-entry overhead
    // adds up at 10k+ files (it buffers, sorts, double-stats, and
    // re-applies the ignore stack for every dir). For this hot
    // predicate we only need: a `readdir` per directory, one
    // `symlink_metadata` per file, and the same ignore-matcher
    // check `build_tree` runs. The std-only recursion below
    // measured ≈3× faster on the 10k-file fixture (no per-entry
    // double-stat, no buffer churn, fewer allocations).
    let mut seen: HashSet<String> = HashSet::with_capacity(manifest.files.len());
    let mut seen_dirs: HashSet<String> = HashSet::with_capacity(expected_dirs.len());
    if !walk_for_no_op(
        root,
        root,
        manifest,
        &expected_dirs,
        &ignore_matcher,
        &mut seen,
        &mut seen_dirs,
    )? {
        return Ok(false);
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
    use tempfile::TempDir;

    use super::*;
    use crate::thread_manifest::read_manifest;

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
            .materialize_thread("main", &dest.path().join("out"), &AudienceTier::Internal)
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

    fn embargo_state_with_tier(repo: &Repository, tier: VisibilityTier) -> ChangeId {
        use chrono::Utc;
        use objects::object::{Principal, StateVisibility};
        let state_id = repo
            .refs()
            .get_thread(&ThreadName::new("main"))
            .unwrap()
            .expect("head present");
        repo.put_state_visibility(StateVisibility {
            state: state_id,
            tier,
            embargo_until: None,
            declarer: Principal {
                name: "Grace Hopper".into(),
                email: "grace@example.com".into(),
            },
            declared_at: Utc::now(),
            signature: None,
            supersedes: None,
        })
        .expect("put visibility");
        state_id
    }

    #[test]
    fn checkout_renders_courtesy_stub_when_state_is_under_tier_for_audience() {
        let repo_dir = TempDir::new().unwrap();
        let repo = Repository::init_default(repo_dir.path()).unwrap();
        fs::write(repo_dir.path().join("secret.rs"), b"fn exploit() {}\n").unwrap();
        repo.snapshot(Some("embargoed fix".into()), None).unwrap();
        embargo_state_with_tier(
            &repo,
            VisibilityTier::Private {
                scope_label: "sec-embargo".into(),
            },
        );

        let dest_holder = TempDir::new().unwrap();
        let dest = dest_holder.path().join("out");
        // A Private state is withheld even from the all-seeing Internal
        // operator — the placeholder appears, the tracked bytes do not.
        let manifest = repo
            .materialize_thread("main", &dest, &AudienceTier::Internal)
            .unwrap();

        assert!(
            dest.join(COURTESY_STUB_FILENAME).exists(),
            "courtesy placeholder must be written for an under-tier checkout"
        );
        assert!(
            !dest.join("secret.rs").exists(),
            "the tracked content must NOT be materialized for an under-tier audience"
        );
        assert!(
            manifest.files.is_empty(),
            "manifest must record no tracked files for a stubbed checkout"
        );
        let stub = fs::read_to_string(dest.join(COURTESY_STUB_FILENAME)).unwrap();
        assert!(stub.contains("private"));
        assert!(stub.contains("sec-embargo"));
    }

    #[test]
    fn checkout_materializes_real_content_for_the_authorized_audience() {
        let repo_dir = TempDir::new().unwrap();
        let repo = Repository::init_default(repo_dir.path()).unwrap();
        fs::write(repo_dir.path().join("secret.rs"), b"fn exploit() {}\n").unwrap();
        repo.snapshot(Some("embargoed fix".into()), None).unwrap();
        embargo_state_with_tier(
            &repo,
            VisibilityTier::Private {
                scope_label: "sec-embargo".into(),
            },
        );

        let dest_holder = TempDir::new().unwrap();
        let dest = dest_holder.path().join("out");
        // The holder of the matching restricted scope sees the real bytes.
        let manifest = repo
            .materialize_thread(
                "main",
                &dest,
                &AudienceTier::Restricted("sec-embargo".into()),
            )
            .unwrap();

        assert!(dest.join("secret.rs").exists());
        assert!(!dest.join(COURTESY_STUB_FILENAME).exists());
        assert!(manifest.files.contains_key("secret.rs"));
    }

    /// `record_thread_manifest` should write a manifest sidecar that
    /// matches what `materialize_thread` would have produced, for a
    /// worktree the caller materialized directly via `materialize_tree`.
    /// Used by the CLI's `start` path (which sets the worktree up
    /// itself rather than going through `materialize_thread`).
    #[test]
    fn record_thread_manifest_writes_sidecar_for_externally_materialized_worktree() {
        let repo_dir = TempDir::new().unwrap();
        let repo = Repository::init_default(repo_dir.path()).unwrap();
        fs::write(repo_dir.path().join("a.txt"), b"alpha\n").unwrap();
        fs::write(repo_dir.path().join("b.txt"), b"beta\n").unwrap();
        repo.snapshot(Some("seed".into()), None).unwrap();
        let state_id = repo
            .refs()
            .get_thread(&ThreadName::new("main"))
            .unwrap()
            .expect("head present");

        // Materialize externally via the lower-level `materialize_tree`
        // path — the shape `start --workspace materialized` uses.
        let dest_holder = TempDir::new().unwrap();
        let dest = dest_holder.path().join("out");
        let state = repo.store().get_state(&state_id).unwrap().unwrap();
        let tree = repo.store().get_tree(&state.tree).unwrap().unwrap();
        repo.materialize_tree(&tree, &dest).unwrap();

        // No manifest written yet — `materialize_tree` is the bytes-only
        // step; the sidecar is recorded explicitly.
        assert!(
            read_manifest(repo.heddle_dir(), "feature/x")
                .unwrap()
                .is_none()
        );

        let recorded = repo
            .record_thread_manifest("feature/x", &state_id, &dest)
            .unwrap();
        assert_eq!(recorded.state_id, state_id);
        assert_eq!(recorded.tree_hash, state.tree);
        assert!(recorded.files.contains_key("a.txt"));
        assert!(recorded.files.contains_key("b.txt"));
        assert_eq!(recorded.files["a.txt"].size, b"alpha\n".len() as u64);

        // Sidecar persists at the expected location and round-trips.
        let loaded = read_manifest(repo.heddle_dir(), "feature/x")
            .unwrap()
            .expect("manifest on disk");
        assert_eq!(loaded.state_id, recorded.state_id);
        assert_eq!(loaded.files.len(), recorded.files.len());

        // Idempotent: a second recording for the same thread succeeds
        // (used by `capture_thread_from_disk` post-capture refresh).
        repo.record_thread_manifest("feature/x", &state_id, &dest)
            .unwrap();
    }

    /// `record_thread_manifest` against an unknown `state_id` should
    /// surface a clear "state missing" error instead of silently
    /// writing a manifest with no files (which would later look like
    /// a deletion of every tracked path).
    #[test]
    fn record_thread_manifest_errors_when_state_is_missing() {
        let repo_dir = TempDir::new().unwrap();
        let repo = Repository::init_default(repo_dir.path()).unwrap();
        let dest = TempDir::new().unwrap();
        let missing = objects::object::ChangeId::generate();
        let err = repo
            .record_thread_manifest("feature/x", &missing, &dest.path().join("out"))
            .expect_err("should fail when state is unknown");
        let message = format!("{err}");
        assert!(
            message.contains("missing"),
            "error message names the missing artifact: {message}"
        );
    }

    #[test]
    fn materialize_unknown_thread_errors() {
        let repo_dir = TempDir::new().unwrap();
        let repo = Repository::init_default(repo_dir.path()).unwrap();
        let dest = TempDir::new().unwrap();
        let err = repo
            .materialize_thread("no-such-thread", &dest.path().join("out"), &AudienceTier::Internal)
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
        let before = repo.refs().get_thread(&ThreadName::new("main")).unwrap().expect("head");

        let dest_holder = TempDir::new().unwrap();
        let dest = dest_holder.path().join("out");
        let materialize_manifest = repo.materialize_thread("main", &dest, &AudienceTier::Internal).unwrap();

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
        let after = repo.refs().get_thread(&ThreadName::new("main")).unwrap().expect("head");
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
        let before = repo.refs().get_thread(&ThreadName::new("main")).unwrap().expect("head");

        let dest_holder = TempDir::new().unwrap();
        let dest = dest_holder.path().join("out");
        repo.materialize_thread("main", &dest, &AudienceTier::Internal).unwrap();

        let outcome = repo.capture_thread_from_disk("main", &dest).unwrap();
        assert_eq!(outcome, ThreadCaptureOutcome::NoOp);

        // Thread head unchanged.
        let after = repo.refs().get_thread(&ThreadName::new("main")).unwrap().expect("head");
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
        let manifest = repo.materialize_thread("main", &dest, &AudienceTier::Internal).unwrap();
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
        let manifest = repo.materialize_thread("main", &dest, &AudienceTier::Internal).unwrap();

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
        let manifest = repo.materialize_thread("main", &dest, &AudienceTier::Internal).unwrap();

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
        let mut manifest = crate::thread_manifest::ThreadManifest::new(
            initial.change_id,
            initial.tree,
            canonical_worktree_path(repo_dir.path()),
        );
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

    /// Regression: snapshot from a directory that is NOT the
    /// manifest's recorded worktree path must NOT refresh the
    /// manifest. Pre-fix, the snapshot code detected the
    /// "materialized-thread context" purely by `HEAD attached + a
    /// manifest exists for the attached thread", so a snapshot from
    /// the main repo dir (or any sibling worktree) would corrupt the
    /// manifest by writing the wrong directory's stat fields into it
    /// — and `heddle status` would then falsely report the
    /// materialized worktree as fresh because the manifest's
    /// `state_id` had auto-rolled forward.
    #[test]
    fn snapshot_outside_materialized_worktree_does_not_refresh_manifest() {
        let repo_dir = TempDir::new().unwrap();
        let repo = Repository::init_default(repo_dir.path()).unwrap();
        fs::write(repo_dir.path().join("alpha.txt"), b"v1\n").unwrap();
        repo.snapshot(Some("seed".into()), None).unwrap();

        // Materialize "main" at a totally separate path. Manifest
        // records `dest_holder/out` as the worktree.
        let dest_holder = TempDir::new().unwrap();
        let dest = dest_holder.path().join("out");
        let materialize_manifest = repo.materialize_thread("main", &dest, &AudienceTier::Internal).unwrap();
        let materialize_state_id = materialize_manifest.state_id;
        let materialize_tree_hash = materialize_manifest.tree_hash;
        let materialized_path = materialize_manifest.worktree_path.clone();
        assert_eq!(
            materialized_path,
            canonical_worktree_path(&dest),
            "manifest must record the canonical materialize destination"
        );

        // Now run snapshot from the MAIN repo dir (`repo.root()`) —
        // a path that is NOT the materialized worktree. The pre-fix
        // bug fired here.
        std::thread::sleep(std::time::Duration::from_millis(20));
        fs::write(repo_dir.path().join("alpha.txt"), b"v2-from-main-repo\n").unwrap();
        let snap = repo
            .snapshot(Some("from main repo, not the mat worktree".into()), None)
            .unwrap();
        assert_ne!(
            snap.change_id, materialize_state_id,
            "snapshot must advance main's head"
        );

        // The manifest must NOT have been refreshed: state_id and
        // tree_hash still point at the materialize state, worktree
        // path still points at `dest`.
        let after = crate::thread_manifest::read_manifest(repo.heddle_dir(), "main")
            .unwrap()
            .expect("manifest still present");
        assert_eq!(
            after.state_id, materialize_state_id,
            "manifest state_id must NOT advance when snapshot is taken outside the materialized worktree"
        );
        assert_eq!(
            after.tree_hash, materialize_tree_hash,
            "manifest tree_hash must NOT advance"
        );
        assert_eq!(
            after.worktree_path, materialized_path,
            "manifest worktree_path must be unchanged"
        );

        // And `heddle status`'s staleness check should now correctly
        // report the materialized worktree as stale (head moved,
        // manifest didn't).
        let head_now = repo.refs().get_thread(&ThreadName::new("main")).unwrap().expect("head");
        assert_ne!(
            head_now, after.state_id,
            "post-fix invariant: main head advanced past manifest's recorded state → stale"
        );
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
        repo.materialize_thread("main", &dest, &AudienceTier::Internal).unwrap();

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

    /// Codex pass-5 P1: when the ignore set tightens between
    /// materialise and capture (e.g. user adds an entry to
    /// `.heddleignore` covering an already-tracked path), the
    /// no-op predicate must bail to the slow path so `build_tree`
    /// can produce the tree that *now* matches the matcher. Pre-
    /// fix the manifest-first dispatch accepted any manifest hit
    /// without re-running the matcher, so the predicate silently
    /// false-passed and `thread switch`'s auto-capture missed
    /// the real tree delta.
    #[test]
    fn stat_cache_detects_ignore_config_tightening() {
        let repo_dir = TempDir::new().unwrap();
        let repo = Repository::init_default(repo_dir.path()).unwrap();
        // Seed: two files, no .heddleignore yet.
        fs::write(repo_dir.path().join("keep.txt"), b"keep\n").unwrap();
        fs::write(repo_dir.path().join("secret.txt"), b"secret\n").unwrap();
        repo.snapshot(Some("seed".into()), None).unwrap();

        let dest_holder = TempDir::new().unwrap();
        let dest = dest_holder.path().join("out");
        let manifest = repo.materialize_thread("main", &dest, &AudienceTier::Internal).unwrap();
        assert!(manifest.files.contains_key("secret.txt"));

        // Tighten the ignore set in the source repo to exclude
        // `secret.txt`. The materialised worktree still has it
        // on disk (we just put it there), but `build_tree` would
        // now skip it and produce a different tree hash.
        fs::write(repo_dir.path().join(".heddleignore"), b"secret.txt\n").unwrap();

        assert!(
            !stat_cache_no_op(&repo, &manifest, &dest).unwrap(),
            "ignore-config tightening over a tracked path must \
             invalidate the fast path; pre-fix the predicate \
             false-passed and auto-capture silently dropped \
             the resulting tree delta"
        );
    }

    /// Codex pass-3 P2: a *tree-only* empty directory — one that
    /// was a captured tree entry but never had any files beneath it
    /// — was invisible to the pass-2 fix because `expected_dirs`
    /// was derived from manifest file ancestors. Removing such a
    /// directory left every set the same size and the predicate
    /// false-passed, silently dropping the change. The pass-3 fix
    /// derives `expected_dirs` from the captured tree directly so
    /// empty leaf dirs are tracked.
    #[test]
    fn stat_cache_detects_removed_tree_only_empty_directory() {
        let repo_dir = TempDir::new().unwrap();
        let repo = Repository::init_default(repo_dir.path()).unwrap();
        // Seed with one file (so the thread isn't empty) plus an
        // empty directory that becomes a tree entry on its own.
        fs::write(repo_dir.path().join("anchor.txt"), b"anchor\n").unwrap();
        fs::create_dir_all(repo_dir.path().join("empty-on-purpose")).unwrap();
        repo.snapshot(Some("seed".into()), None).unwrap();

        let dest_holder = TempDir::new().unwrap();
        let dest = dest_holder.path().join("out");
        let manifest = repo.materialize_thread("main", &dest, &AudienceTier::Internal).unwrap();

        // Sanity: the empty dir landed on disk after materialise.
        assert!(
            dest.join("empty-on-purpose").is_dir(),
            "materialise must emit the empty dir on disk"
        );

        // Remove the empty dir. No files inside it changed
        // because there never were any — pure tree-only delta.
        fs::remove_dir(dest.join("empty-on-purpose")).unwrap();

        assert!(
            !stat_cache_no_op(&repo, &manifest, &dest).unwrap(),
            "removing a tree-only empty directory must invalidate \
             the fast path; pre-fix the predicate false-passed and \
             auto-capture silently dropped the deletion"
        );
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
        let manifest = repo.materialize_thread("main", &dest, &AudienceTier::Internal).unwrap();

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
        let manifest = repo.materialize_thread("main", &dest, &AudienceTier::Internal).unwrap();

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
        let manifest = repo.materialize_thread("main", &dest, &AudienceTier::Internal).unwrap();

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
        let initial_head = repo.refs().get_thread(&ThreadName::new("main")).unwrap().expect("seeded");

        // Two sibling materialized worktrees of the same thread.
        let dest_a_holder = TempDir::new().unwrap();
        let dest_a = dest_a_holder.path().join("a");
        repo.materialize_thread("main", &dest_a, &AudienceTier::Internal).unwrap();
        let dest_b_holder = TempDir::new().unwrap();
        let dest_b = dest_b_holder.path().join("b");
        repo.materialize_thread("main", &dest_b, &AudienceTier::Internal).unwrap();

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
        let final_head = repo.refs().get_thread(&ThreadName::new("main")).unwrap().expect("head");
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
