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
    collections::{BTreeMap, BTreeSet},
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
pub(crate) const COURTESY_STUB_FILENAME: &str = "HEDDLE-EMBARGO.txt";

/// Outcome of the visibility-gated checkout chokepoint
/// [`Repository::checkout_state_gated`].
#[derive(Clone, Debug)]
pub enum CheckoutMaterialization {
    /// The state was visible to the audience: its real tree was materialized
    /// to `dest`. Carries the resolved tree so callers can populate a manifest
    /// without a second store lookup.
    Materialized { tree: Tree },
    /// The state was under-tier for the audience: the operator-local courtesy
    /// stub was written to `dest` and the tracked bytes withheld.
    Withheld { tier: VisibilityTier },
}

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

        // Route through the single visibility-gated checkout chokepoint, which
        // either materializes the real tree or writes the operator-local
        // courtesy stub. The manifest is this method's own concern (it lives
        // outside the checkout dir), so it is written here based on the gate
        // outcome — not in the chokepoint, which `write_isolated_checkout` also
        // calls without wanting a thread manifest.
        match self.checkout_state_gated(&change_id, &state, dest, audience)? {
            CheckoutMaterialization::Withheld { tier } => {
                // Manifest reflects disk truth: no tracked files were
                // materialized (the placeholder is untracked). `tree_hash`
                // still names the real embargoed state's tree so the sidecar
                // identifies which state this checkout stands in for. The
                // `withheld` flag here is diagnostic only — it records that the
                // *last* materialize of this thread was withheld, but the
                // per-thread manifest is clobbered by a sibling worktree of the
                // same thread. The authoritative, per-worktree non-capturable
                // signal is the withheld marker written by
                // `checkout_state_gated`, keyed on the worktree root (heddle#316).
                let mut manifest =
                    ThreadManifest::new(change_id, state.tree, canonical_worktree_path(dest));
                manifest.withheld = true;
                write_manifest(self.heddle_dir(), thread, &manifest).map_err(HeddleError::Io)?;
                debug!(
                    thread = %thread,
                    state_id = %change_id,
                    tier = tier.as_str(),
                    "thread checkout rendered courtesy stub (under-tier for audience)"
                );
                Ok(manifest)
            }
            CheckoutMaterialization::Materialized { tree } => {
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
        }
    }

    /// THE visibility-gated checkout chokepoint. Resolve `change_id`'s
    /// effective tier against `audience` and either materialize its real tree
    /// to `dest` (visible) or write the operator-local courtesy stub and
    /// withhold the tracked bytes (under-tier).
    ///
    /// Every path that serves a *named committed state*'s content to a local
    /// checkout MUST funnel through here — `materialize_thread` and the CLI's
    /// `write_isolated_checkout` (`heddle start --path`) both do — so the
    /// visibility gate cannot be bypassed by a caller reaching for the raw,
    /// blob-keyed `materialize_tree`. The decision is made HERE, where the
    /// `ChangeId` and the audience are both in scope; `materialize_tree`
    /// carries neither and so cannot make it. `materialize_tree` stays the
    /// primitive for *computed* trees (merge/cherry-pick results), which are
    /// not a single named state and carry no audience.
    ///
    /// The courtesy stub is a working-tree convenience on bytes the operator
    /// already holds — NOT a security boundary and NOT a public-mirror surface
    /// (the public mirror emits absence, spike §5.3).
    pub fn checkout_state_gated(
        &self,
        change_id: &ChangeId,
        state: &State,
        dest: &Path,
        audience: &AudienceTier,
    ) -> Result<CheckoutMaterialization> {
        let tier = self.effective_visibility_tier(change_id).map_err(|e| {
            HeddleError::Config(format!("resolve visibility for {change_id}: {e:#}"))
        })?;
        if !visible(&tier, audience) {
            fs::create_dir_all(dest).map_err(HeddleError::Io)?;
            // Canonicalize ONLY after the directory exists. `canonical_worktree_path`
            // falls back to the raw input when `dest` does not yet resolve (a relative
            // path, or a path through a not-yet-created symlink), so a pre-creation
            // canonicalize would key the withheld marker and the `.leaves` record on a
            // path `capture_thread_from_disk` never resolves to at read-time — the read
            // canonicalizes the now-existing root, misses the marker, and captures a
            // withheld checkout as a stub-only tree instead of no-oping. Resolving here,
            // once `create_dir_all` has made `dest` exist, guarantees the write-time
            // canonical root equals the read-time one (heddle#316).
            let canonical = canonical_worktree_path(dest);
            // Reconcile the root DOWN to the withheld tier: every tracked leaf a
            // prior materialize of this root wrote must be removed, so the
            // checkout holds ONLY the courtesy stub — never the very bytes the
            // gate is withholding. `keep` is empty (the withheld tier permits no
            // tracked content). `must_remove` additionally names the withheld
            // state's own tree leaves, so the leak is closed even when no prior
            // manifest survives for this root (a sibling worktree clobbered it).
            // The stub itself is untracked and so never in either set (heddle#316
            // CLASS 1).
            let mut withheld_leaves = BTreeSet::new();
            if let Some(tree) = self.store().get_tree(&state.tree)? {
                collect_tree_leaf_paths(self, &tree, "", &mut withheld_leaves)?;
            }
            self.reconcile_materialized_root(dest, &canonical, &BTreeSet::new(), &withheld_leaves)?;
            // Persist the clobber-proof per-root record: a withheld materialize
            // leaves ONLY the untracked courtesy stub, so the tracked-leaf set is
            // empty. Written here so the single chokepoint owns the record for
            // every funnel path, and so a later reconcile of this root reads an
            // authoritative empty set instead of falling to the backstop
            // (heddle#316 CLASS 1).
            crate::thread_manifest::write_materialized_leaves(
                self.heddle_dir(),
                &canonical,
                &BTreeSet::new(),
            )
            .map_err(HeddleError::Io)?;
            let embargo_until = self
                .effective_state_visibility(change_id)
                .map_err(|e| {
                    HeddleError::Config(format!("resolve visibility for {change_id}: {e:#}"))
                })?
                .and_then(|record| record.embargo_until);
            let stub = courtesy_stub_text(&tier, embargo_until);
            fs::write(dest.join(COURTESY_STUB_FILENAME), stub.as_bytes())
                .map_err(HeddleError::Io)?;
            // Record the withheld status keyed by THIS worktree root, not by
            // thread — a sibling worktree of the same thread materialized at a
            // visible tier must keep its own capturable status (heddle#316).
            crate::thread_manifest::mark_withheld_checkout(self.heddle_dir(), &canonical)
                .map_err(HeddleError::Io)?;
            return Ok(CheckoutMaterialization::Withheld { tier });
        }

        let tree = self
            .store()
            .get_tree(&state.tree)?
            .ok_or_else(|| HeddleError::Config(format!("tree for {change_id} missing")))?;
        self.materialize_tree(&tree, dest)?;
        // Canonicalize only now that `materialize_tree` (via `create_dir_all`) has made
        // `dest` exist — same read/write-root agreement as the withheld branch above
        // (heddle#316).
        let canonical = canonical_worktree_path(dest);
        // Reconcile the root UP to the served tier: `materialize_tree` wrote the
        // real tree's leaves but does NOT remove a stale leaf a prior
        // materialize of a *different* tree left at this root. `keep` is the set
        // of leaves the served tree just wrote — any prior tracked leaf NOT in
        // it is removed, so the root holds exactly this tier's content
        // (heddle#316 CLASS 1).
        let mut served_leaves = BTreeSet::new();
        collect_tree_leaf_paths(self, &tree, "", &mut served_leaves)?;
        self.reconcile_materialized_root(dest, &canonical, &served_leaves, &BTreeSet::new())?;
        // Persist the clobber-proof per-root record of exactly the tracked leaves
        // this visible materialize left on disk, so a later withheld
        // re-materialize of this root removes precisely them even if a sibling
        // worktree of the same thread clobbered the per-thread manifest in the
        // interim (heddle#316 CLASS 1).
        crate::thread_manifest::write_materialized_leaves(
            self.heddle_dir(),
            &canonical,
            &served_leaves,
        )
        .map_err(HeddleError::Io)?;
        // This root now holds real served bytes: clear any stale withheld marker
        // a prior under-tier materialize of the same root may have left, so it
        // can't suppress this worktree's capture (heddle#316).
        crate::thread_manifest::clear_withheld_checkout(self.heddle_dir(), &canonical)
            .map_err(HeddleError::Io)?;
        // Remove any leftover courtesy stub a prior under-tier materialize of the
        // same root wrote: the stub is untracked, so the reconcile leaf-removal
        // above leaves it in place. Cosmetic — capture ignores it — but an
        // authorized re-materialize should leave a clean tree (heddle#316).
        match fs::remove_file(dest.join(COURTESY_STUB_FILENAME)) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(HeddleError::Io(e)),
        }
        Ok(CheckoutMaterialization::Materialized { tree })
    }

    /// Reconcile the worktree root at `dest` so it holds EXACTLY the content the
    /// target tier permits, regardless of what a prior materialization of the
    /// same root left behind. THE single chokepoint both branches of
    /// [`Repository::checkout_state_gated`] funnel through to enforce the
    /// invariant by construction rather than via two opposite one-off cleanups
    /// (heddle#316 CLASS 1).
    ///
    /// Removes every tracked leaf that (a) a prior materialization recorded for
    /// this root in its clobber-proof per-root **materialized-leaves record**
    /// (keyed by the canonical worktree root, so a sibling worktree of the same
    /// thread can never erase it) UNION (b) the caller's `must_remove` set —
    /// MINUS the `keep` set the target tier permits. Removal is guarded per file
    /// (`NotFound` ignored) and empty ancestor directories it leaves behind are
    /// pruned via `remove_dir` (which fails on non-empty dirs, so untracked
    /// siblings keep their directory alive).
    ///
    /// Sourcing the prior leaves from the per-root record — NOT the single
    /// per-thread `manifest.toml` — is what makes the withheld reduction
    /// correct-by-construction: the manifest is clobbered the instant a sibling
    /// worktree of the same thread materializes, which would drop a prior
    /// *visible* leaf (e.g. an `old-secret.txt` removed before the withheld
    /// target state) out of the removal set and leak it next to the stub. The
    /// per-root record is immune to that race (heddle#316 CLASS 1).
    ///
    /// Never blanket-`rm -rf`s: only paths sourced from the per-root record /
    /// `must_remove` are touched, so user-untracked files and `.git`/heddle
    /// metadata are never removed.
    fn reconcile_materialized_root(
        &self,
        dest: &Path,
        canonical_root: &Path,
        keep: &BTreeSet<String>,
        must_remove: &BTreeSet<String>,
    ) -> Result<()> {
        let mut to_remove: BTreeSet<String> = must_remove.clone();
        match crate::thread_manifest::read_materialized_leaves(self.heddle_dir(), canonical_root)
            .map_err(HeddleError::Io)?
        {
            Some(prior_leaves) => {
                // Clobber-proof per-root record of exactly the tracked leaves a
                // prior materialize of THIS root left on disk. Authoritative —
                // survives a sibling worktree's clobber of the per-thread
                // manifest.
                to_remove.extend(prior_leaves);
            }
            None => {
                // Fail-closed backstop: no per-root record yet. Reached only on a
                // first-ever materialize of this root (nothing prior to remove)
                // or a root last materialized by a binary predating the per-root
                // record. Fall back to the best-effort per-thread manifest so an
                // upgrade-window reconcile still drops a recorded prior tree's
                // leaves; `must_remove` (the target tier's own leaves) covers the
                // rest. Strictly safer than trusting `must_remove` alone, and —
                // like the primary path — touches only recorded leaves, never
                // untracked/non-heddle files.
                if let Some(prior) = crate::thread_manifest::manifest_for_worktree_root(
                    self.heddle_dir(),
                    canonical_root,
                )
                .map_err(HeddleError::Io)?
                {
                    to_remove.extend(prior.files.keys().cloned());
                }
            }
        }

        let mut prune_dirs: BTreeSet<PathBuf> = BTreeSet::new();
        for rel in &to_remove {
            if keep.contains(rel) {
                continue;
            }
            let path = dest.join(rel);
            match fs::remove_file(&path) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(HeddleError::Io(e)),
            }
            // Collect ancestor directories (within `dest`) so the now-empty ones
            // left by the removed leaf can be pruned after the pass.
            let mut parent = path.parent();
            while let Some(p) = parent {
                if p == dest || !p.starts_with(dest) {
                    break;
                }
                prune_dirs.insert(p.to_path_buf());
                parent = p.parent();
            }
        }

        // Prune deepest-first so a parent only sees its children already gone.
        // `remove_dir` errors on a non-empty dir, which we ignore — that is
        // exactly how an untracked sibling keeps its directory.
        let mut dirs: Vec<PathBuf> = prune_dirs.into_iter().collect();
        dirs.sort_by_key(|d| std::cmp::Reverse(d.components().count()));
        for d in dirs {
            let _ = fs::remove_dir(&d);
        }
        Ok(())
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

    /// Record a WITHHELD-consistent manifest sidecar for a worktree whose
    /// checkout was withheld — the base state's visibility tier was not visible
    /// to the materializing audience, so [`Repository::checkout_state_gated`]
    /// wrote ONLY the operator-local courtesy stub and the tracked bytes were
    /// never materialized.
    ///
    /// Mirrors the withheld arm of [`Repository::materialize_thread`]: `tree_hash`
    /// still names the real (unserved) state's tree so the sidecar identifies
    /// which state the stub stands in for, but `files` is empty (no tracked leaf
    /// is on disk) and `withheld = true`. Crucially this does NOT walk/stat the
    /// real tree against `dest` the way [`Repository::record_thread_manifest`]
    /// does — those files were intentionally not materialized, so stat-ing them
    /// would record phantom stat-cache entries (or fail) against a checkout that
    /// holds only the stub. The CLI's atomic `start` path calls this instead of
    /// `record_thread_manifest` when the checkout came back withheld, so a start
    /// on a Private base produces a withheld checkout + a consistent manifest
    /// rather than erroring (heddle#316 / PR #528 r9 Finding 3).
    #[instrument(skip(self), fields(thread = %thread, dest = %dest.display(), state = %state_id))]
    pub fn record_withheld_thread_manifest(
        &self,
        thread: &str,
        state_id: &ChangeId,
        dest: &Path,
    ) -> Result<ThreadManifest> {
        let state = self
            .store()
            .get_state(state_id)?
            .ok_or_else(|| HeddleError::Config(format!("state {state_id} missing")))?;
        let mut manifest =
            ThreadManifest::new(*state_id, state.tree, canonical_worktree_path(dest));
        manifest.withheld = true;
        crate::thread_manifest::write_manifest(self.heddle_dir(), thread, &manifest)
            .map_err(HeddleError::Io)?;
        debug!(
            thread = %thread,
            state_id = %state_id,
            "withheld thread manifest recorded post-materialize"
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

        // 0a. Withheld checkouts are non-capturable. A withheld checkout holds
        //     only the operator-local courtesy stub (the tracked bytes were
        //     withheld because the state's tier is not visible to the
        //     materializing audience). Capturing it would either pull the stub
        //     in as tracked content or — worse — build an empty tree (the stub
        //     is ignored, see `ignore_patterns`) and commit it, wiping the
        //     withheld state's real files. The operator cannot capture content
        //     they were never served, so refuse with a no-op and leave the
        //     thread head where it is (heddle#316).
        //
        //     The withheld status is keyed by THIS worktree root, not by the
        //     per-thread `manifest.toml` — that single file is clobbered when
        //     the same thread is materialized into a second worktree, so a
        //     manifest-level flag would let an under-tier checkout of one
        //     worktree wrongly suppress an authorized sibling worktree's
        //     capture. The per-root marker (written by `checkout_state_gated`)
        //     scopes the suppression to exactly the worktree that was withheld.
        if crate::thread_manifest::is_withheld_checkout(
            self.heddle_dir(),
            &canonical_worktree_path(root),
        ) {
            debug!(thread = %thread, "thread capture skipped (withheld checkout)");
            return Ok(ThreadCaptureOutcome::NoOp);
        }

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

/// Collect every blob/symlink leaf path (worktree-relative, forward-slash
/// joined) reachable from `tree` into `out`. Used by the checkout reconcile
/// step to enumerate the tracked content a tier serves (the `keep` set on the
/// visible path) or withholds (the `must_remove` set on the withheld path),
/// without touching disk — the path set is derived purely from the tree.
fn collect_tree_leaf_paths(
    repo: &Repository,
    tree: &Tree,
    rel_prefix: &str,
    out: &mut BTreeSet<String>,
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
                        "subtree {} missing while collecting leaf paths for {rel_path}",
                        entry.hash
                    ))
                })?;
                collect_tree_leaf_paths(repo, &subtree, &rel_path, out)?;
            }
            EntryType::Blob | EntryType::Symlink => {
                out.insert(rel_path);
            }
        }
    }
    Ok(())
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

    /// #316 / PR #528 r6: a worktree root first materialized under-tier (stub
    /// written) and later re-materialized for an authorized audience must end up
    /// with a clean tree — the real bytes present AND the stale courtesy stub
    /// removed. `materialize_tree` only writes tracked leaves, so without an
    /// explicit removal the stub would linger on disk after the visible path.
    #[test]
    fn authorized_rematerialize_removes_stale_embargo_stub() {
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

        // First: under-tier materialize of the root → only the stub lands.
        repo.materialize_thread("main", &dest, &AudienceTier::Internal)
            .unwrap();
        assert!(
            dest.join(COURTESY_STUB_FILENAME).exists(),
            "under-tier materialize must write the stub"
        );
        assert!(!dest.join("secret.rs").exists());

        // Then: re-materialize the SAME root for an authorized audience.
        let manifest = repo
            .materialize_thread(
                "main",
                &dest,
                &AudienceTier::Restricted("sec-embargo".into()),
            )
            .unwrap();

        assert!(
            dest.join("secret.rs").exists(),
            "authorized re-materialize must write the real tree"
        );
        assert!(manifest.files.contains_key("secret.rs"));
        assert!(
            !dest.join(COURTESY_STUB_FILENAME).exists(),
            "the stale courtesy stub must be removed on the authorized re-materialize"
        );
    }

    /// #316 / PR #528 r7 CLASS 1 (the leak): a root first materialized for an
    /// AUTHORIZED audience (real tree on disk) and then re-materialized
    /// UNDER-TIER must end up holding ONLY the courtesy stub — none of the prior
    /// visible tree's tracked bytes may remain next to the stub, or the checkout
    /// still contains exactly the content the gate is supposed to withhold. The
    /// reconcile step removes the prior tracked leaves (including nested ones)
    /// and prunes the directories they leave empty.
    #[test]
    fn visible_then_withheld_root_has_only_stub() {
        let repo_dir = TempDir::new().unwrap();
        let repo = Repository::init_default(repo_dir.path()).unwrap();
        fs::write(repo_dir.path().join("secret.rs"), b"fn exploit() {}\n").unwrap();
        fs::create_dir_all(repo_dir.path().join("nested")).unwrap();
        fs::write(repo_dir.path().join("nested/inner.rs"), b"fn inner() {}\n").unwrap();
        repo.snapshot(Some("embargoed fix".into()), None).unwrap();
        embargo_state_with_tier(
            &repo,
            VisibilityTier::Private {
                scope_label: "sec-embargo".into(),
            },
        );

        let dest_holder = TempDir::new().unwrap();
        let dest = dest_holder.path().join("out");

        // Visible materialize: the real tree lands — the very bytes a later
        // under-tier materialize must withhold.
        repo.materialize_thread(
            "main",
            &dest,
            &AudienceTier::Restricted("sec-embargo".into()),
        )
        .unwrap();
        assert!(dest.join("secret.rs").exists());
        assert!(dest.join("nested/inner.rs").exists());

        // Under-tier re-materialize of the SAME root — the leak case.
        repo.materialize_thread("main", &dest, &AudienceTier::Internal)
            .unwrap();

        assert!(
            dest.join(COURTESY_STUB_FILENAME).exists(),
            "withheld checkout must hold the courtesy stub"
        );
        assert!(
            !dest.join("secret.rs").exists(),
            "the prior visible tree's bytes must NOT remain next to the stub"
        );
        assert!(
            !dest.join("nested/inner.rs").exists(),
            "nested tracked leaves must be removed too"
        );
        // ONLY the stub remains: every prior tracked leaf — and the now-empty
        // directories they lived in — are gone.
        let remaining: Vec<_> = fs::read_dir(&dest)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(
            remaining.len(),
            1,
            "withheld root must contain only the courtesy stub, got {remaining:?}"
        );
        assert_eq!(remaining[0].to_str().unwrap(), COURTESY_STUB_FILENAME);
    }

    /// #316 / PR #528 r7 CLASS 1 (r6 transition, as a matrix member): a root
    /// first materialized UNDER-TIER (stub) and then re-materialized for an
    /// AUTHORIZED audience must hold the real tree and NO stale stub.
    #[test]
    fn withheld_then_visible_root_has_real_tree_no_stub() {
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

        repo.materialize_thread("main", &dest, &AudienceTier::Internal)
            .unwrap();
        assert!(dest.join(COURTESY_STUB_FILENAME).exists());
        assert!(!dest.join("secret.rs").exists());

        let manifest = repo
            .materialize_thread(
                "main",
                &dest,
                &AudienceTier::Restricted("sec-embargo".into()),
            )
            .unwrap();
        assert!(
            dest.join("secret.rs").exists(),
            "authorized re-materialize must write the real tree"
        );
        assert!(manifest.files.contains_key("secret.rs"));
        assert!(
            !dest.join(COURTESY_STUB_FILENAME).exists(),
            "the stale courtesy stub must be removed on the authorized re-materialize"
        );
    }

    /// #316 / PR #528 r7 CLASS 1 (visible→visible): re-materializing a root at a
    /// NEW visible tree must leave exactly that tree — a leaf dropped from the
    /// new tree must not linger from the prior materialize. `materialize_tree`
    /// writes the new leaves but does not remove a now-absent prior leaf; the
    /// reconcile step closes that gap.
    #[test]
    fn visible_then_visible_refreshes_tree() {
        let repo_dir = TempDir::new().unwrap();
        let repo = Repository::init_default(repo_dir.path()).unwrap();
        fs::write(repo_dir.path().join("keep.rs"), b"keep\n").unwrap();
        fs::write(repo_dir.path().join("stale.rs"), b"stale\n").unwrap();
        repo.snapshot(Some("seed".into()), None).unwrap();

        let dest_holder = TempDir::new().unwrap();
        let dest = dest_holder.path().join("out");
        repo.materialize_thread("main", &dest, &AudienceTier::Internal)
            .unwrap();
        assert!(dest.join("keep.rs").exists());
        assert!(dest.join("stale.rs").exists());

        // Advance the thread head in the MAIN repo (snapshot walks repo.root,
        // not `dest`, so the dest manifest's worktree_path stays = dest and is
        // NOT refreshed here): drop stale.rs, add fresh.rs.
        fs::remove_file(repo_dir.path().join("stale.rs")).unwrap();
        fs::write(repo_dir.path().join("fresh.rs"), b"fresh\n").unwrap();
        repo.snapshot(Some("advance".into()), None).unwrap();

        // Re-materialize the SAME root at the new (still visible) head.
        repo.materialize_thread("main", &dest, &AudienceTier::Internal)
            .unwrap();
        assert!(dest.join("keep.rs").exists(), "an unchanged leaf stays");
        assert!(dest.join("fresh.rs").exists(), "the new leaf is written");
        assert!(
            !dest.join("stale.rs").exists(),
            "a leaf dropped from the new tree must not linger from the prior materialize"
        );
        assert!(
            !dest.join(COURTESY_STUB_FILENAME).exists(),
            "a visible re-materialize writes no stub"
        );
    }

    /// #316 / PR #528 r7 CLASS 1 (withheld→withheld): two under-tier
    /// materializes of the same root leave only the stub each time, and capture
    /// stays a no-op.
    #[test]
    fn withheld_then_withheld_stays_withheld() {
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

        repo.materialize_thread("main", &dest, &AudienceTier::Internal)
            .unwrap();
        assert!(dest.join(COURTESY_STUB_FILENAME).exists());
        assert!(!dest.join("secret.rs").exists());

        // Second under-tier materialize of the same root: still only the stub.
        repo.materialize_thread("main", &dest, &AudienceTier::Internal)
            .unwrap();
        let remaining: Vec<_> = fs::read_dir(&dest)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(
            remaining.len(),
            1,
            "withheld root must contain only the courtesy stub, got {remaining:?}"
        );
        assert_eq!(remaining[0].to_str().unwrap(), COURTESY_STUB_FILENAME);
        assert!(!dest.join("secret.rs").exists());

        // Capture of the still-withheld root is a no-op.
        let outcome = repo.capture_thread_from_disk("main", &dest).unwrap();
        assert_eq!(outcome, ThreadCaptureOutcome::NoOp);
    }

    /// #316 / PR #528 r9 FINDING A: the withheld marker (and `.leaves` record)
    /// must be keyed on the root `capture_thread_from_disk` resolves at
    /// READ-time, not on a pre-materialization path. `canonical_worktree_path`
    /// falls back to its raw input when the path does not yet resolve, so a dest
    /// reached THROUGH a symlink whose leaf does not exist yet canonicalizes to
    /// the un-resolved `link/out` before the dir is made but to the resolved
    /// `real/out` after. Pre-fix the marker was written under `link/out` while
    /// capture looked it up under `real/out` → marker missed → a withheld
    /// checkout captured as a stub-only tree instead of no-oping.
    #[cfg(unix)]
    #[test]
    fn withheld_marker_keyed_on_canonical_root_for_relative_dest() {
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

        // `dest` travels through a symlink to a not-yet-existing leaf, so a
        // canonicalize BEFORE the dir is created resolves differently (falls
        // back to `link/out`) than one AFTER (`real/out`).
        let dest_holder = TempDir::new().unwrap();
        let real = dest_holder.path().join("real");
        fs::create_dir_all(&real).unwrap();
        std::os::unix::fs::symlink(&real, dest_holder.path().join("link")).unwrap();
        let dest = dest_holder.path().join("link").join("out");

        repo.materialize_thread("main", &dest, &AudienceTier::Internal)
            .unwrap();
        assert!(dest.join(COURTESY_STUB_FILENAME).exists());
        assert!(!dest.join("secret.rs").exists());

        // Capture through the symlinked path must be a NO-OP: the marker was
        // keyed on the same canonical root (`real/out`) capture resolves.
        let outcome = repo.capture_thread_from_disk("main", &dest).unwrap();
        assert_eq!(
            outcome,
            ThreadCaptureOutcome::NoOp,
            "withheld checkout reached via a symlinked path must not be capturable"
        );
    }

    /// #316 / PR #528 r8 HOLE 1: the withheld reduction must NOT depend on the
    /// clobberable per-thread `manifest.toml`. A root first materialized VISIBLE
    /// (holding `old-secret.txt`), THEN observed while a sibling worktree of the
    /// SAME thread is materialized (the event that clobbers the per-thread
    /// manifest, retargeting it at the sibling's root), THEN re-materialized
    /// WITHHELD against a LATER state whose tree no longer contains
    /// `old-secret.txt`, must still end up holding ONLY the courtesy stub. The
    /// secret is in NEITHER the withheld state's own tree NOR (post-clobber) the
    /// per-thread manifest — only the clobber-proof per-root record names it, so
    /// the reduction can only succeed by sourcing that record.
    #[test]
    fn withheld_reduction_survives_sibling_manifest_clobber() {
        let repo_dir = TempDir::new().unwrap();
        let repo = Repository::init_default(repo_dir.path()).unwrap();

        // State S1 (visible): contains the secret that must not linger later.
        fs::write(repo_dir.path().join("old-secret.txt"), b"launch codes\n").unwrap();
        repo.snapshot(Some("seed with secret".into()), None).unwrap();

        // Root A materialized VISIBLE at S1 — the real bytes land on disk and the
        // clobber-proof per-root record for A captures `old-secret.txt`.
        let a_holder = TempDir::new().unwrap();
        let root_a = a_holder.path().join("root-a");
        repo.materialize_thread("main", &root_a, &AudienceTier::Internal)
            .unwrap();
        assert!(root_a.join("old-secret.txt").exists());

        // Advance the thread to S2: the secret is REMOVED before this state, a
        // new tracked file replaces it. So `old-secret.txt` is absent from S2's
        // tree entirely.
        fs::remove_file(repo_dir.path().join("old-secret.txt")).unwrap();
        fs::write(repo_dir.path().join("kept.txt"), b"benign\n").unwrap();
        repo.snapshot(Some("drop secret, advance".into()), None)
            .unwrap();
        embargo_state_with_tier(
            &repo,
            VisibilityTier::Private {
                scope_label: "sec-embargo".into(),
            },
        );

        // A sibling worktree B of the SAME thread is materialized (authorized, at
        // S2). `materialize_thread` rewrites `threads/main/manifest.toml` keyed by
        // thread name, so this CLOBBERS A's record there — `manifest_for_worktree_root(A)`
        // now resolves to B, the precise race that reopened the leak in r7.
        let b_holder = TempDir::new().unwrap();
        let root_b = b_holder.path().join("root-b");
        repo.materialize_thread("main", &root_b, &AudienceTier::Restricted("sec-embargo".into()))
            .unwrap();
        assert!(root_b.join("kept.txt").exists());
        // Confirm the clobber really happened: the per-thread manifest no longer
        // records root A.
        assert!(
            crate::thread_manifest::manifest_for_worktree_root(
                repo.heddle_dir(),
                &canonical_worktree_path(&root_a),
            )
            .unwrap()
            .is_none(),
            "sibling materialize must have clobbered A's per-thread manifest record"
        );

        // Re-materialize root A WITHHELD (Internal can't see S2's Private tier).
        // S2's tree does not contain `old-secret.txt`, and the per-thread
        // manifest no longer names A — only the clobber-proof per-root record can
        // drive its removal.
        repo.materialize_thread("main", &root_a, &AudienceTier::Internal)
            .unwrap();

        assert!(
            root_a.join(COURTESY_STUB_FILENAME).exists(),
            "withheld checkout must hold the courtesy stub"
        );
        assert!(
            !root_a.join("old-secret.txt").exists(),
            "the prior visible tree's secret must be GONE even though the per-thread manifest was clobbered"
        );
        let remaining: Vec<_> = fs::read_dir(&root_a)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(
            remaining.len(),
            1,
            "withheld root must contain only the courtesy stub, got {remaining:?}"
        );
        assert_eq!(remaining[0].to_str().unwrap(), COURTESY_STUB_FILENAME);
    }

    /// #316 / PR #528 r9 FINDING 4: close the per-root `.leaves`-staleness CLASS.
    /// `capture_thread_from_disk` rewrites `manifest.toml` but used to leave the
    /// clobber-proof per-root `.leaves` record untouched, so a captured-but-
    /// later-withheld leaf leaked. Sequence: a visible checkout holding `{a}`;
    /// the user adds `b` and captures (head advances, `.leaves` MUST refresh to
    /// `{a, b}`); the thread then advances to a state whose tree drops `b` and is
    /// embargoed; re-materializing the SAME root WITHHELD against that state must
    /// leave ONLY the stub — `b` (on disk from the capture) must be GONE, not
    /// leaked next to the stub. The withheld state's own tree lacks `b`, so only
    /// a `.leaves` record the capture refreshed can drive `b`'s removal.
    #[test]
    fn capture_refreshes_materialized_leaves() {
        let repo_dir = TempDir::new().unwrap();
        let repo = Repository::init_default(repo_dir.path()).unwrap();

        // S1 (visible): tracked `a.txt`.
        fs::write(repo_dir.path().join("a.txt"), b"alpha\n").unwrap();
        repo.snapshot(Some("seed a".into()), None).unwrap();

        // Materialize root R visible (Internal) at S1 → disk {a.txt},
        // .leaves(R) = {a.txt}.
        let holder = TempDir::new().unwrap();
        let root = holder.path().join("root");
        repo.materialize_thread("main", &root, &AudienceTier::Internal)
            .unwrap();
        assert!(root.join("a.txt").exists());

        // User adds `b.txt` in R and captures → head advances to S2 = {a, b}.
        // The capture MUST refresh the per-root `.leaves` record to include
        // `b.txt` (the class-fix: capture rewrites the manifest AND `.leaves`).
        fs::write(root.join("b.txt"), b"beta\n").unwrap();
        match repo.capture_thread_from_disk("main", &root).unwrap() {
            ThreadCaptureOutcome::Captured { .. } => {}
            ThreadCaptureOutcome::NoOp => panic!("adding b.txt must produce a real capture"),
        }
        let leaves = crate::thread_manifest::read_materialized_leaves(
            repo.heddle_dir(),
            &canonical_worktree_path(&root),
        )
        .unwrap()
        .expect("capture must have written a per-root leaves record");
        assert!(
            leaves.contains("a.txt") && leaves.contains("b.txt"),
            "capture must refresh the per-root .leaves record to the captured tree's leaves, got {leaves:?}"
        );

        // Advance the thread to S3 whose tree LACKS b.txt: snapshot from the main
        // repo dir (which only holds a.txt and is NOT the materialized worktree,
        // so the manifest is not refreshed here), then embargo S3 Private.
        fs::write(repo_dir.path().join("a.txt"), b"alpha v2\n").unwrap();
        repo.snapshot(Some("drop b, advance".into()), None).unwrap();
        embargo_state_with_tier(
            &repo,
            VisibilityTier::Private {
                scope_label: "sec-embargo".into(),
            },
        );

        // Re-materialize R WITHHELD (Internal under-tier for the Private S3). S3's
        // own tree has no b.txt, so the withheld reduction can only remove the
        // capture-added b.txt by sourcing the refreshed per-root record.
        repo.materialize_thread("main", &root, &AudienceTier::Internal)
            .unwrap();

        assert!(
            root.join(COURTESY_STUB_FILENAME).exists(),
            "withheld checkout must hold the courtesy stub"
        );
        assert!(
            !root.join("b.txt").exists(),
            "the capture-added leaf must be removed by the withheld reduction, not leaked next to the stub"
        );
        let remaining: Vec<_> = fs::read_dir(&root)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(
            remaining.len(),
            1,
            "withheld root must contain only the courtesy stub, got {remaining:?}"
        );
        assert_eq!(remaining[0].to_str().unwrap(), COURTESY_STUB_FILENAME);
    }

    /// #316 / PR #528 r3 Finding 1: materializing an under-tier checkout writes
    /// the courtesy stub and marks the manifest `withheld`. A subsequent
    /// capture of that checkout must be a NO-OP — it must NOT pull the stub in
    /// as tracked content, and (crucially) must NOT commit an empty tree that
    /// wipes the withheld state's real files. The thread head stays put.
    #[test]
    fn capture_skips_embargo_courtesy_stub() {
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
        // Under-tier audience → only the stub lands; no real bytes, empty files.
        let manifest = repo
            .materialize_thread("main", &dest, &AudienceTier::Internal)
            .unwrap();
        assert!(
            dest.join(COURTESY_STUB_FILENAME).exists(),
            "stub must be written for the under-tier checkout"
        );
        assert!(manifest.files.is_empty(), "no tracked files in a stub checkout");
        assert!(manifest.withheld, "manifest must mark the checkout withheld");

        let head_before = repo
            .refs()
            .get_thread(&ThreadName::new("main"))
            .unwrap()
            .expect("head");

        // Capture the withheld checkout.
        let outcome = repo.capture_thread_from_disk("main", &dest).unwrap();
        assert_eq!(
            outcome,
            ThreadCaptureOutcome::NoOp,
            "a withheld checkout is non-capturable"
        );

        // Thread head must not have moved.
        let head_after = repo
            .refs()
            .get_thread(&ThreadName::new("main"))
            .unwrap()
            .expect("head");
        assert_eq!(
            head_before, head_after,
            "withheld capture must not advance the thread head"
        );

        // The thread's tree is still the real embargoed tree: it contains the
        // withheld content and NOT the courtesy stub.
        let head_state = repo.store().get_state(&head_after).unwrap().unwrap();
        let tree = repo.store().get_tree(&head_state.tree).unwrap().unwrap();
        assert!(
            !tree
                .entries()
                .iter()
                .any(|e| e.name == COURTESY_STUB_FILENAME),
            "captured tree must never contain the courtesy stub"
        );
        assert!(
            tree.entries().iter().any(|e| e.name == "secret.rs"),
            "the withheld real content must remain intact in the thread"
        );
    }

    /// #316 / PR #528 r4: the withheld status must be scoped per *worktree
    /// root*, not per thread. When one thread is materialized into TWO
    /// worktrees — an authorized one A (real bytes) and an under-tier one B
    /// (withheld stub) — the under-tier materialize of B clobbers the single
    /// per-thread `manifest.toml`. A withheld flag stored there would then
    /// wrongly suppress a capture of A, silently dropping legitimate work.
    /// With the per-worktree marker, A captures its real edits and B no-ops.
    #[test]
    fn withheld_manifest_is_per_worktree_not_per_thread() {
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

        let holder_a = TempDir::new().unwrap();
        let worktree_a = holder_a.path().join("authorized");
        let holder_b = TempDir::new().unwrap();
        let worktree_b = holder_b.path().join("under-tier");

        // Worktree A: the matching-scope holder gets the real bytes.
        let manifest_a = repo
            .materialize_thread(
                "main",
                &worktree_a,
                &AudienceTier::Restricted("sec-embargo".into()),
            )
            .unwrap();
        assert!(worktree_a.join("secret.rs").exists());
        assert!(manifest_a.files.contains_key("secret.rs"));

        // Edit A so a correct capture produces a NEW state. Without the edit,
        // capturing unchanged real content is a *legitimate* no-op and wouldn't
        // distinguish the bug (wrong withheld-suppression) from correct
        // behaviour.
        fs::write(worktree_a.join("extra.rs"), b"fn added() {}\n").unwrap();

        let head_before = repo
            .refs()
            .get_thread(&ThreadName::new("main"))
            .unwrap()
            .expect("head");

        // Worktree B: under-tier audience → stub only, withheld. This clobbers
        // the single per-thread `manifest.toml` with B's withheld record.
        let manifest_b = repo
            .materialize_thread("main", &worktree_b, &AudienceTier::Internal)
            .unwrap();
        assert!(worktree_b.join(COURTESY_STUB_FILENAME).exists());
        assert!(manifest_b.files.is_empty());

        // Capture A: must capture the real edit — its withheld status is its
        // own (none), NOT inherited from B's clobbering materialize.
        let outcome_a = repo.capture_thread_from_disk("main", &worktree_a).unwrap();
        let captured_state = match outcome_a {
            ThreadCaptureOutcome::Captured { state_id } => state_id,
            ThreadCaptureOutcome::NoOp => {
                panic!("authorized worktree A must capture its real edit, not be suppressed")
            }
        };
        let head_after_a = repo
            .refs()
            .get_thread(&ThreadName::new("main"))
            .unwrap()
            .expect("head");
        assert_ne!(head_before, head_after_a, "capture A must advance the head");
        assert_eq!(head_after_a, captured_state);
        // The captured tree carries the edit and the real content, never the stub.
        let captured_tree = repo
            .store()
            .get_tree(
                &repo
                    .store()
                    .get_state(&captured_state)
                    .unwrap()
                    .unwrap()
                    .tree,
            )
            .unwrap()
            .unwrap();
        assert!(captured_tree.entries().iter().any(|e| e.name == "extra.rs"));
        assert!(captured_tree.entries().iter().any(|e| e.name == "secret.rs"));
        assert!(
            !captured_tree
                .entries()
                .iter()
                .any(|e| e.name == COURTESY_STUB_FILENAME)
        );

        // Capture B: must be a no-op — its own worktree is withheld.
        let outcome_b = repo.capture_thread_from_disk("main", &worktree_b).unwrap();
        assert_eq!(
            outcome_b,
            ThreadCaptureOutcome::NoOp,
            "under-tier worktree B is non-capturable"
        );
        let head_after_b = repo
            .refs()
            .get_thread(&ThreadName::new("main"))
            .unwrap()
            .expect("head");
        assert_eq!(
            head_after_a, head_after_b,
            "withheld capture of B must not advance the head"
        );
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
