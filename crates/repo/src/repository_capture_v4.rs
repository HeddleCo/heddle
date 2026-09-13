// SPDX-License-Identifier: Apache-2.0
//! Sticky-salt V4 tree conversion at the capture chokepoint.
//!
//! The worktree walker always builds flat V3 trees (every directory is a
//! `Tree::from_entries`). When the spool policy selects
//! [`TreeScheme::V4Salted`](objects::object::TreeScheme::V4Salted) the capture
//! chokepoint post-processes that freshly-built tree into a salted per-entry
//! Merkle V4 tree, applying the **sticky-salt inheritance** rule (design
//! v4-redactable-tree §3):
//!
//! - An entry that is **unchanged** from the parent (lineage) tree — same name
//!   and byte-identical target — **inherits** the parent entry's 32-byte salt.
//! - A **new or changed** entry gets a **fresh 32-byte CSPRNG salt**.
//!
//! Because an unchanged entry keeps its salt, an unchanged subtree keeps its
//! leaf, and an unchanged subtree keeps its root — so a no-op recapture along a
//! lineage reproduces a byte-identical tree id, and changing one file changes
//! only that leaf and the Merkle path above it. Determinism is decided by
//! comparing the *converted* (post-recursion) child hash against the parent's
//! child hash, so it holds regardless of whether the walker reused the parent's
//! subtree hash or rebuilt an identical subtree from scratch.
//!
//! Capture is the ONLY v4 producer: this module never calls the salt-minting
//! `Tree::insert`; it inherits explicitly-resolved salts through
//! `Tree::from_entries_salted_v4`.

use std::collections::HashMap;

use objects::{
    error::HeddleError,
    object::{ContentHash, Tree, TreeEntry, TreeEntryTarget, TreeScheme},
    store::{ObjectStore, TreeWrite},
};

use crate::{Repository, Result};

impl Repository {
    /// Convert a freshly-built (flat V3) worktree tree plus its nested subtrees
    /// into sticky-salted V4 trees.
    ///
    /// `root` is the walker's root tree; `subtrees` are the pending V3 subtree
    /// writes the walker collected for the same capture; `parent_root` is the
    /// lineage parent's tree (V3 or V4), the salt-inheritance source.
    ///
    /// Returns `(v4_root, v4_subtrees)`: the V4 root (its hash is the new
    /// `State.tree`) and every V4 subtree that must be stored. Subtrees that
    /// were unchanged from a V4 parent are reused verbatim (already in the
    /// store) and are NOT re-emitted.
    pub(crate) fn v4ify_capture_tree(
        &self,
        root: &Tree,
        subtrees: &[TreeWrite],
        parent_root: Option<&Tree>,
    ) -> Result<(Tree, Vec<Tree>)> {
        let mut pending: HashMap<ContentHash, Tree> = HashMap::with_capacity(subtrees.len());
        for write in subtrees {
            pending.insert(write.tree.hash(), write.tree.clone());
        }
        let mut out = Vec::new();
        let v4_root = self.v4ify_tree(root, parent_root, &pending, &mut out)?;
        Ok((v4_root, out))
    }

    /// Recursively convert `new_tree` (and its nested subtrees) to a V4 salted
    /// tree, inheriting salts from `parent_tree` where entries are unchanged.
    /// Converted subtrees (other than a verbatim-reused V4 subtree) are pushed
    /// to `out`. The scheme of `new_tree` is ignored — salts are always
    /// re-derived from the lineage parent so the result is deterministic
    /// regardless of what the walker produced.
    fn v4ify_tree(
        &self,
        new_tree: &Tree,
        parent_tree: Option<&Tree>,
        pending: &HashMap<ContentHash, Tree>,
        out: &mut Vec<Tree>,
    ) -> Result<Tree> {
        let mut entries: Vec<TreeEntry> = Vec::with_capacity(new_tree.entries().len());
        let mut salts: Vec<[u8; 32]> = Vec::with_capacity(new_tree.entries().len());

        for entry in new_tree.entries() {
            let v4_entry = match entry.target() {
                TreeEntryTarget::Tree { hash } => {
                    let child = self.resolve_capture_tree(hash, pending)?;
                    // The parent's same-name subtree, resolved for both the
                    // unchanged short-circuit and salt inheritance.
                    let parent_child = match parent_tree
                        .and_then(|parent| parent.get(entry.name()))
                        .and_then(TreeEntry::tree_hash)
                    {
                        Some(parent_hash) => {
                            self.resolve_capture_tree_opt(&parent_hash, pending)?
                        }
                        None => None,
                    };
                    // Unchanged subtree already in V4 form: reuse it verbatim
                    // (its salts are intact and it is already stored). This is
                    // the cheap common case on a V4 lineage — no deep recursion.
                    let child_v4 = if child.scheme() == TreeScheme::V4Salted
                        && parent_child.as_ref().map(Tree::hash) == Some(*hash)
                    {
                        child
                    } else {
                        let converted =
                            self.v4ify_tree(&child, parent_child.as_ref(), pending, out)?;
                        out.push(converted.clone());
                        converted
                    };
                    TreeEntry::directory(entry.name(), child_v4.hash())?
                }
                _ => entry.clone(),
            };
            let salt = inherit_or_fresh_salt(parent_tree, &v4_entry);
            entries.push(v4_entry);
            salts.push(salt);
        }

        Tree::from_entries_salted_v4(entries, salts).map_err(HeddleError::from)
    }

    /// Resolve a subtree by hash: the pending capture set first, then the store.
    fn resolve_capture_tree_opt(
        &self,
        hash: &ContentHash,
        pending: &HashMap<ContentHash, Tree>,
    ) -> Result<Option<Tree>> {
        if let Some(tree) = pending.get(hash) {
            return Ok(Some(tree.clone()));
        }
        self.store.get_tree(hash)
    }

    /// Resolve a subtree that must exist (a child named by the tree under
    /// conversion). A missing subtree is a hard error — for the capture path the
    /// pending set plus the store always contain every referenced subtree.
    fn resolve_capture_tree(
        &self,
        hash: &ContentHash,
        pending: &HashMap<ContentHash, Tree>,
    ) -> Result<Tree> {
        self.resolve_capture_tree_opt(hash, pending)?
            .ok_or_else(|| HeddleError::MissingObject {
                object_type: "tree".to_string(),
                id: hash.to_string(),
            })
    }
}

/// The salt for `entry`: the parent entry's salt when `parent_tree` is a V4
/// tree carrying a same-name entry with a byte-identical target, else a fresh
/// 256-bit CSPRNG salt. Comparing the (already-converted) target means an
/// unchanged subtree — whose converted child hash equals the parent's child
/// hash — inherits, which is what makes a no-op recapture reproduce the id.
fn inherit_or_fresh_salt(parent_tree: Option<&Tree>, entry: &TreeEntry) -> [u8; 32] {
    // Falsification hook (test 12 / §12.12): forcing every salt fresh must break
    // the no-change-recapture-identical-id invariant. Off outside tests.
    #[cfg(test)]
    if tests::force_fresh_salts() {
        return rand::random();
    }
    if let Some(parent) = parent_tree
        && parent.scheme() == TreeScheme::V4Salted
        && let Some(index) = parent
            .entries()
            .iter()
            .position(|candidate| candidate.name() == entry.name())
        && parent.entries()[index].target() == entry.target()
        && let Some(salt) = parent.salt_at(index)
    {
        return salt;
    }
    rand::random()
}

#[cfg(test)]
mod tests {
    use std::{cell::Cell, fs};

    use objects::{object::TreeScheme, store::ObjectStore};
    use tempfile::TempDir;

    use crate::{RepoConfig, Repository, TreeSchemePolicy};

    thread_local! {
        static FORCE_FRESH_SALTS: Cell<bool> = const { Cell::new(false) };
    }

    /// True while a test has forced the salt-inheritance path off (falsification).
    pub(super) fn force_fresh_salts() -> bool {
        FORCE_FRESH_SALTS.with(Cell::get)
    }

    fn with_forced_fresh_salts<T>(body: impl FnOnce() -> T) -> T {
        FORCE_FRESH_SALTS.with(|flag| flag.set(true));
        let out = body();
        FORCE_FRESH_SALTS.with(|flag| flag.set(false));
        out
    }

    /// A repo whose `[policies] tree_scheme` is set to `scheme`.
    fn repo_with_scheme(scheme: TreeSchemePolicy) -> (TempDir, Repository) {
        let temp = TempDir::new().unwrap();
        let repo = Repository::init_default(temp.path()).unwrap();
        let config_path = repo.heddle_dir().join("config.toml");
        let mut config = RepoConfig::load_for_repository(&config_path).unwrap();
        config.policies.tree_scheme = scheme;
        config.save(&config_path).unwrap();
        let repo = Repository::open(temp.path()).unwrap();
        (temp, repo)
    }

    /// The salt the captured tree assigned to the top-level entry `name`.
    fn root_salt_for(
        repo: &Repository,
        tree_hash: &objects::object::ContentHash,
        name: &str,
    ) -> [u8; 32] {
        let tree = repo.store().get_tree(tree_hash).unwrap().expect("tree");
        let index = tree
            .entries()
            .iter()
            .position(|entry| entry.name() == name)
            .expect("entry present");
        tree.salt_at(index).expect("v4 tree carries salts")
    }

    #[test]
    fn v4_spool_produces_salted_trees() {
        let (temp, repo) = repo_with_scheme(TreeSchemePolicy::V4);
        fs::write(temp.path().join("readme.md"), b"hello\n").unwrap();
        let state = repo.snapshot(Some("first".into()), None).unwrap();
        let tree = repo.store().get_tree(&state.tree).unwrap().expect("tree");
        assert_eq!(tree.scheme(), TreeScheme::V4Salted);
        assert_eq!(tree.salts().len(), tree.entries().len());
    }

    #[test]
    fn v3_spool_captures_flat_trees() {
        let (temp, repo) = repo_with_scheme(TreeSchemePolicy::V3);
        fs::write(temp.path().join("readme.md"), b"hello\n").unwrap();
        let state = repo.snapshot(Some("first".into()), None).unwrap();
        let tree = repo.store().get_tree(&state.tree).unwrap().expect("tree");
        assert_eq!(tree.scheme(), TreeScheme::V3Flat);
        assert!(tree.salts().is_empty());
        // A v3 spool captures byte-identical to a direct v3 build of the same
        // worktree — the scheme flag is the only difference from v4.
        let (_temp2, repo3) = repo_with_scheme(TreeSchemePolicy::V3);
        fs::write(_temp2.path().join("readme.md"), b"hello\n").unwrap();
        let state3 = repo3.snapshot(Some("first".into()), None).unwrap();
        assert_eq!(
            state.tree, state3.tree,
            "v3 capture must be content-addressed"
        );
    }

    #[test]
    fn v4_recapture_with_no_change_is_identical_id() {
        let (temp, repo) = repo_with_scheme(TreeSchemePolicy::V4);
        fs::write(temp.path().join("a.txt"), b"alpha\n").unwrap();
        fs::write(temp.path().join("b.txt"), b"bravo\n").unwrap();
        fs::create_dir(temp.path().join("dir")).unwrap();
        fs::write(temp.path().join("dir/c.txt"), b"charlie\n").unwrap();
        let first = repo.snapshot(Some("first".into()), None).unwrap();
        let second = repo.snapshot(Some("second".into()), None).unwrap();
        assert_eq!(
            first.tree, second.tree,
            "a no-op recapture along a lineage must reproduce the same v4 tree id"
        );
    }

    #[test]
    fn v4_changing_one_file_changes_only_that_leaf() {
        let (temp, repo) = repo_with_scheme(TreeSchemePolicy::V4);
        fs::write(temp.path().join("a.txt"), b"alpha\n").unwrap();
        fs::write(temp.path().join("b.txt"), b"bravo\n").unwrap();
        let first = repo.snapshot(Some("first".into()), None).unwrap();
        let a_salt_1 = root_salt_for(&repo, &first.tree, "a.txt");
        let b_salt_1 = root_salt_for(&repo, &first.tree, "b.txt");

        fs::write(temp.path().join("a.txt"), b"alpha-modified\n").unwrap();
        let second = repo.snapshot(Some("second".into()), None).unwrap();
        assert_ne!(first.tree, second.tree, "content change must change the id");
        let a_salt_2 = root_salt_for(&repo, &second.tree, "a.txt");
        let b_salt_2 = root_salt_for(&repo, &second.tree, "b.txt");

        assert_ne!(a_salt_1, a_salt_2, "the changed file must get a fresh salt");
        assert_eq!(
            b_salt_1, b_salt_2,
            "an unchanged sibling must inherit its parent's salt"
        );
    }

    #[test]
    fn v4_new_file_gets_fresh_salt_and_siblings_inherit() {
        let (temp, repo) = repo_with_scheme(TreeSchemePolicy::V4);
        fs::write(temp.path().join("a.txt"), b"alpha\n").unwrap();
        let first = repo.snapshot(Some("first".into()), None).unwrap();
        let a_salt_1 = root_salt_for(&repo, &first.tree, "a.txt");

        fs::write(temp.path().join("new.txt"), b"new\n").unwrap();
        let second = repo.snapshot(Some("second".into()), None).unwrap();
        // The new entry exists (fresh salt, nothing to inherit) and the existing
        // entry kept its salt.
        let _new_salt = root_salt_for(&repo, &second.tree, "new.txt");
        let a_salt_2 = root_salt_for(&repo, &second.tree, "a.txt");
        assert_eq!(
            a_salt_1, a_salt_2,
            "sibling salt must be inherited across a new-file capture"
        );
    }

    #[test]
    fn falsify_forcing_fresh_salts_breaks_recapture_identity() {
        let (temp, repo) = repo_with_scheme(TreeSchemePolicy::V4);
        fs::write(temp.path().join("a.txt"), b"alpha\n").unwrap();
        fs::write(temp.path().join("b.txt"), b"bravo\n").unwrap();
        let first = repo.snapshot(Some("first".into()), None).unwrap();
        // With inheritance disabled, a no-op recapture mints all-new salts and
        // so must NOT reproduce the id — proving inheritance is load-bearing.
        let second =
            with_forced_fresh_salts(|| repo.snapshot(Some("second".into()), None).unwrap());
        assert_ne!(
            first.tree, second.tree,
            "forcing fresh salts must break the no-change-recapture-identical-id invariant"
        );
    }

    #[test]
    fn single_file_fast_path_on_v4_spool_does_not_spuriously_conflict() {
        use objects::worktree::WorktreeStatus;
        let (temp, repo) = repo_with_scheme(TreeSchemePolicy::V4);
        fs::write(temp.path().join("a.txt"), b"1\n").unwrap();
        fs::write(temp.path().join("b.txt"), b"1\n").unwrap();
        let _first = repo.snapshot(Some("first".into()), None).unwrap(); // baseline V4
        fs::write(temp.path().join("a.txt"), b"2\n").unwrap();
        // Route through the single-file fast path (`rewrite_single_tracked_file`).
        // Before the fix it rebuilt via `insert` on the V4 baseline (minting a
        // salt → V4 root) while the revalidation fingerprint walks V3, so the
        // ids never matched and the capture spuriously Conflicted.
        let status = WorktreeStatus {
            modified: vec![std::path::PathBuf::from("a.txt")],
            added: Vec::new(),
            deleted: Vec::new(),
        };
        let exec = repo
            .snapshot_with_attribution_profiled_from_status(
                Some("edit".into()),
                None,
                repo.get_attribution().unwrap(),
                status,
                false,
            )
            .expect("single-file fast-path capture must not spuriously Conflict on a v4 spool");
        let tree = repo.store().get_tree(&exec.state.tree).unwrap().unwrap();
        assert_eq!(tree.scheme(), TreeScheme::V4Salted);
    }

    #[test]
    fn if_changed_on_v4_spool_runs_authoritative_rewalk_for_noop() {
        use super::super::repository_snapshot::{
            authoritative_rewalk_count, authoritative_rewalk_count_reset,
        };
        let (temp, repo) = repo_with_scheme(TreeSchemePolicy::V4);
        fs::write(temp.path().join("a.txt"), b"1\n").unwrap();
        let _first = repo.snapshot(Some("first".into()), None).unwrap(); // baseline V4
        authoritative_rewalk_count_reset();
        // A no-op `*_if_changed` on a v4 spool must still run the authoritative
        // monitor-off re-walk before reporting NoChanges. Before the fix the
        // scheme-mixed compare (V3 walk vs V4 baseline) was never equal, so the
        // re-walk was skipped and fail-closed was weakened.
        let err = repo
            .snapshot_with_attribution_profiled_if_changed(
                Some("noop".into()),
                None,
                repo.get_attribution().unwrap(),
            )
            .unwrap_err();
        assert!(matches!(err, objects::error::HeddleError::NoChanges));
        assert!(
            authoritative_rewalk_count() >= 1,
            "the authoritative re-walk must run on a v4 no-op if_changed capture"
        );
    }

    #[test]
    fn merge_tip_on_v4_spool_is_salted_and_leaf_stable_to_first_parent() {
        let (temp, repo) = repo_with_scheme(TreeSchemePolicy::V4);
        fs::write(temp.path().join("a.txt"), b"1\n").unwrap();
        fs::write(temp.path().join("b.txt"), b"1\n").unwrap();
        let a = repo.snapshot(Some("a".into()), None).unwrap();
        fs::write(temp.path().join("a.txt"), b"2\n").unwrap();
        let b = repo.snapshot(Some("b".into()), None).unwrap(); // head = B = first parent
        let b_tree = repo.store().get_tree(&b.tree).unwrap().unwrap();

        // Merge current head B with parent A. The worktree content equals B, so
        // (Leg 2.5) the salted merge tip must inherit B's salts entry-for-entry
        // and reproduce B's V4 tree id exactly.
        let merge = repo
            .snapshot_merge_with_attribution(
                &a.id(),
                Some("merge".into()),
                None,
                repo.get_attribution().unwrap(),
                None,
                false,
            )
            .unwrap();
        let merge_tree = repo.store().get_tree(&merge.tree).unwrap().unwrap();
        assert_eq!(
            merge_tree.scheme(),
            TreeScheme::V4Salted,
            "merge tip must be salted on a v4 spool"
        );
        // Leaf-stable to the first parent: unchanged entries keep the parent salt.
        assert_eq!(
            merge_tree.v4_leaf_hash_for("a.txt"),
            b_tree.v4_leaf_hash_for("a.txt")
        );
        assert_eq!(
            merge_tree.v4_leaf_hash_for("b.txt"),
            b_tree.v4_leaf_hash_for("b.txt")
        );
        assert_eq!(
            merge.tree, b.tree,
            "a merge tip over content identical to the first parent is leaf-stable"
        );

        // Re-merging the same unchanged content stays leaf-stable.
        let merge2 = repo
            .snapshot_merge_with_attribution(
                &a.id(),
                Some("merge2".into()),
                None,
                repo.get_attribution().unwrap(),
                None,
                false,
            )
            .unwrap();
        assert_eq!(
            merge2.tree, merge.tree,
            "re-merging unchanged content must be leaf-stable"
        );
    }
}
