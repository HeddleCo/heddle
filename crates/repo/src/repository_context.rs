// SPDX-License-Identifier: Apache-2.0
//! Context annotation helpers for attaching metadata to file and state targets.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use objects::object::{
    Annotation, AnnotationScope, Blob, ContentHash, ContextBlob, ContextTarget, EntryType, State,
    Tree, TreeEntry,
};

use super::{HeddleError, Repository, Result};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContextEntry {
    pub target: ContextTarget,
    pub blob: ContextBlob,
}

impl Repository {
    /// Get the context blob for a target from the given state's context tree.
    pub fn get_context_blob(
        &self,
        context_root: &ContentHash,
        target: &ContextTarget,
    ) -> Result<Option<ContextBlob>> {
        let Some(blob_hash) = self.lookup_context_leaf_for_target(context_root, target)? else {
            return Ok(None);
        };
        let Some(blob) = self.store.get_blob(&blob_hash)? else {
            return Ok(None);
        };
        ContextBlob::decode(blob.content())
            .map(Some)
            .map_err(|e| HeddleError::InvalidObject(format!("invalid context blob: {e}")))
    }

    /// Store a context blob at a target, returning the new context tree root hash.
    ///
    /// If `context_root` is None, creates a new context tree from scratch.
    pub fn set_context_blob(
        &self,
        context_root: Option<&ContentHash>,
        target: &ContextTarget,
        blob: &ContextBlob,
    ) -> Result<ContentHash> {
        let bytes = blob
            .encode()
            .map_err(|e| HeddleError::InvalidObject(format!("encode context: {e}")))?;
        let blob_hash = self.store.put_blob(&Blob::new(bytes))?;

        let current_tree = match context_root {
            Some(root) => self.require_tree(root)?,
            None => Tree::new(),
        };

        let mut root_hash =
            self.insert_leaf_at_path(&current_tree, &target.storage_path(), blob_hash)?;

        if let (Some(existing_root), Some(legacy_path)) =
            (context_root, target.legacy_storage_path())
            && legacy_path != target.storage_path()
            && self
                .lookup_context_leaf(existing_root, &legacy_path)?
                .is_some()
        {
            root_hash = self
                .remove_leaf_at_path(&root_hash, &legacy_path)?
                .unwrap_or(root_hash);
        }

        Ok(root_hash)
    }

    /// Remove context at a target (optionally filtered by scope).
    ///
    /// Returns the new context tree root, or None if the tree is now empty.
    pub fn remove_context_at_target(
        &self,
        context_root: &ContentHash,
        target: &ContextTarget,
        scope: Option<&AnnotationScope>,
    ) -> Result<Option<ContentHash>> {
        if let Some(scope) = scope {
            if let Some(mut blob) = self.get_context_blob(context_root, target)? {
                blob.annotations.retain(|a| !a.scope.matches(scope));
                if blob.annotations.is_empty() {
                    return self.remove_context_target(context_root, target);
                }
                let new_root = self.set_context_blob(Some(context_root), target, &blob)?;
                return Ok(Some(new_root));
            }
            return Ok(Some(*context_root));
        }

        self.remove_context_target(context_root, target)
    }

    pub fn remove_context_target(
        &self,
        context_root: &ContentHash,
        target: &ContextTarget,
    ) -> Result<Option<ContentHash>> {
        let mut current = self.remove_leaf_at_path(context_root, &target.storage_path())?;
        if current.is_none()
            && let Some(legacy_path) = target.legacy_storage_path()
        {
            current = self.remove_leaf_at_path(context_root, &legacy_path)?;
        }
        Ok(current)
    }

    /// List all context entries in the tree, optionally filtered by file prefix.
    pub fn list_context_entries(
        &self,
        context_root: &ContentHash,
        prefix: Option<&Path>,
    ) -> Result<Vec<ContextEntry>> {
        let tree = match self.store.get_tree(context_root)? {
            Some(t) => t,
            None => return Ok(Vec::new()),
        };
        let mut results = BTreeMap::new();
        self.walk_context_tree(&tree, &PathBuf::new(), prefix, &mut results)?;
        Ok(results
            .into_iter()
            .map(|(_, (target, blob))| ContextEntry { target, blob })
            .collect())
    }

    pub fn find_annotation(
        &self,
        context_root: &ContentHash,
        annotation_id: &str,
    ) -> Result<Option<(ContextTarget, ContextBlob, usize)>> {
        for entry in self.list_context_entries(context_root, None)? {
            if let Some(index) = entry
                .blob
                .annotations
                .iter()
                .position(|annotation| annotation.annotation_id == annotation_id)
            {
                return Ok(Some((entry.target, entry.blob, index)));
            }
        }
        Ok(None)
    }

    // --- private helpers ---

    fn lookup_context_leaf_for_target(
        &self,
        root: &ContentHash,
        target: &ContextTarget,
    ) -> Result<Option<ContentHash>> {
        let storage_path = target.storage_path();
        if let Some(hash) = self.lookup_context_leaf(root, &storage_path)? {
            return Ok(Some(hash));
        }
        if let Some(legacy_path) = target.legacy_storage_path() {
            return self.lookup_context_leaf(root, &legacy_path);
        }
        Ok(None)
    }

    fn lookup_context_leaf(&self, root: &ContentHash, path: &Path) -> Result<Option<ContentHash>> {
        let Some((name, rest)) = split_path(path) else {
            return Ok(None);
        };
        let Some(tree) = self.store.get_tree(root)? else {
            return Ok(None);
        };
        let Some(entry) = tree.get(name) else {
            return Ok(None);
        };
        if rest.as_os_str().is_empty() {
            return Ok(entry.is_blob().then_some(entry.hash));
        }
        if !entry.is_tree() {
            return Ok(None);
        }
        self.lookup_context_leaf(&entry.hash, rest)
    }

    fn insert_leaf_at_path(
        &self,
        tree: &Tree,
        path: &Path,
        blob_hash: ContentHash,
    ) -> Result<ContentHash> {
        let Some((name, rest)) = split_path(path) else {
            return Err(HeddleError::InvalidObject("empty path".to_string()));
        };

        let mut new_tree = tree.clone();

        if rest.as_os_str().is_empty() {
            new_tree.insert(TreeEntry::file(name, blob_hash, false)?);
        } else {
            let subtree = tree
                .get(name)
                .filter(|e| e.is_tree())
                .and_then(|e| self.store.get_tree(&e.hash).ok().flatten())
                .unwrap_or_default();

            let sub_hash = self.insert_leaf_at_path(&subtree, rest, blob_hash)?;
            new_tree.insert(TreeEntry::directory(name, sub_hash)?);
        }

        self.store.put_tree(&new_tree)
    }

    fn remove_leaf_at_path(&self, root: &ContentHash, path: &Path) -> Result<Option<ContentHash>> {
        let Some(tree) = self.store.get_tree(root)? else {
            return Ok(None);
        };
        let Some((name, rest)) = split_path(path) else {
            return Ok(None);
        };

        let mut new_tree = tree.clone();

        if rest.as_os_str().is_empty() {
            new_tree.remove(name);
        } else {
            let Some(entry) = tree.get(name) else {
                return Ok(Some(*root));
            };
            if !entry.is_tree() {
                return Ok(Some(*root));
            }
            match self.remove_leaf_at_path(&entry.hash, rest)? {
                Some(sub_hash) => {
                    new_tree.insert(TreeEntry::directory(name, sub_hash)?);
                }
                None => {
                    new_tree.remove(name);
                }
            }
        }

        if new_tree.is_empty() {
            Ok(None)
        } else {
            Ok(Some(self.store.put_tree(&new_tree)?))
        }
    }

    fn walk_context_tree(
        &self,
        tree: &Tree,
        current_path: &Path,
        prefix: Option<&Path>,
        results: &mut BTreeMap<String, (ContextTarget, ContextBlob)>,
    ) -> Result<()> {
        for entry in tree.entries() {
            let entry_path = current_path.join(&entry.name);
            match entry.entry_type {
                EntryType::Tree => {
                    if let Some(prefix) = prefix
                        && !prefix.starts_with(&entry_path)
                        && !entry_path.starts_with(prefix)
                        && !entry_path.starts_with("__files")
                        && !entry_path.starts_with("__states")
                    {
                        continue;
                    }
                    if let Some(subtree) = self.store.get_tree(&entry.hash)? {
                        self.walk_context_tree(&subtree, &entry_path, prefix, results)?;
                    }
                }
                EntryType::Blob => {
                    let Some(target) = ContextTarget::from_storage_path(&entry_path) else {
                        continue;
                    };
                    if let Some(prefix) = prefix
                        && let Some(path) = target.path()
                        && !Path::new(path).starts_with(prefix)
                    {
                        continue;
                    }
                    if let Some(blob) = self.store.get_blob(&entry.hash)?
                        && let Ok(context) = ContextBlob::decode(blob.content())
                    {
                        results.insert(context_entry_key(&target), (target, context));
                    }
                }
                EntryType::Symlink => {}
            }
        }
        Ok(())
    }

    /// Carry a single parent state's context tree forward onto a new
    /// snapshot. Because context trees are content-addressed, this is a
    /// pointer copy: the new state's `context` field gets the same
    /// `ContentHash` as the parent. Annotations attached upstream remain
    /// active at the new state, and the existing on-demand staleness check
    /// (which compares the stored `source_hash` against the current bytes
    /// at the anchor) naturally reports drift caused by the new tree.
    ///
    /// Returns `None` when the parent has no context tree.
    pub fn inherit_parent_context(parent: &State) -> Option<ContentHash> {
        parent.context
    }

    /// Build a unioned context tree across multiple parent states for a
    /// merge snapshot. Annotations from every parent appear in the result;
    /// when the same `annotation_id` is present on more than one parent the
    /// revision with the latest `created_at` wins (with a stable tiebreak
    /// on revision_id so the merge stays deterministic).
    ///
    /// Targets that only exist on one side propagate unchanged (single-blob
    /// pointer copy via the existing tree). Targets present on both sides
    /// are merged blob-by-blob: annotations are deduped by id, the per-id
    /// revisions are picked by latest-`created_at`, and the resulting blob
    /// is rewritten via `set_context_blob`.
    ///
    /// Returns `None` when none of the parents has any context.
    pub fn union_parent_contexts(&self, parents: &[&State]) -> Result<Option<ContentHash>> {
        // Fast paths: nothing or single-parent.
        let mut roots: Vec<ContentHash> = parents.iter().filter_map(|p| p.context).collect();
        if roots.is_empty() {
            return Ok(None);
        }
        if roots.len() == 1 {
            return Ok(Some(roots.pop().expect("len == 1")));
        }
        if roots.iter().all(|r| *r == roots[0]) {
            // All parents pointed at the same context tree; pointer copy.
            return Ok(Some(roots[0]));
        }

        // Walk every parent and merge by `context_entry_key`. Each entry's
        // blob gets unioned into a running map; ties are broken by the
        // revision-comparator below.
        let mut merged: BTreeMap<String, (ContextTarget, ContextBlob)> = BTreeMap::new();
        for parent_root in &roots {
            for entry in self.list_context_entries(parent_root, None)? {
                let key = context_entry_key(&entry.target);
                match merged.remove(&key) {
                    None => {
                        merged.insert(key, (entry.target, entry.blob));
                    }
                    Some((target, existing)) => {
                        let merged_blob = merge_context_blobs(existing, entry.blob);
                        merged.insert(key, (target, merged_blob));
                    }
                }
            }
        }

        if merged.is_empty() {
            return Ok(None);
        }

        // Rebuild the tree from scratch by writing each blob.
        let mut root: Option<ContentHash> = None;
        for (_, (target, blob)) in merged {
            if blob.annotations.is_empty() {
                continue;
            }
            let new_root = self.set_context_blob(root.as_ref(), &target, &blob)?;
            root = Some(new_root);
        }

        Ok(root)
    }
}

/// Merge two `ContextBlob`s by unioning their annotations on
/// `annotation_id`. When an id appears in both, the annotation with the
/// later current-revision `created_at` wins; ties are broken by
/// `revision_id` to keep the result deterministic. The resulting blob
/// preserves `format_version` from `left`.
fn merge_context_blobs(left: ContextBlob, right: ContextBlob) -> ContextBlob {
    let format_version = left.format_version;
    let mut by_id: BTreeMap<String, Annotation> = BTreeMap::new();
    for annotation in left.annotations.into_iter().chain(right.annotations) {
        match by_id.remove(&annotation.annotation_id) {
            None => {
                by_id.insert(annotation.annotation_id.clone(), annotation);
            }
            Some(existing) => {
                let winner = pick_newer_annotation(existing, annotation);
                by_id.insert(winner.annotation_id.clone(), winner);
            }
        }
    }
    ContextBlob {
        format_version,
        annotations: by_id.into_values().collect(),
    }
}

fn pick_newer_annotation(a: Annotation, b: Annotation) -> Annotation {
    let ts_a = a
        .current_revision()
        .map(|r| r.created_at)
        .unwrap_or(i64::MIN);
    let ts_b = b
        .current_revision()
        .map(|r| r.created_at)
        .unwrap_or(i64::MIN);
    if ts_a > ts_b {
        a
    } else if ts_b > ts_a {
        b
    } else {
        // Deterministic tiebreak on the revision_id of each side's current
        // revision (lexicographic — revision_ids are stable strings).
        let rev_a = a
            .current_revision()
            .map(|r| r.revision_id.as_str())
            .unwrap_or("");
        let rev_b = b
            .current_revision()
            .map(|r| r.revision_id.as_str())
            .unwrap_or("");
        if rev_a >= rev_b { a } else { b }
    }
}

fn context_entry_key(target: &ContextTarget) -> String {
    match target {
        ContextTarget::File { path } => format!("file:{path}"),
        ContextTarget::State { change_id } => format!("state:{}", change_id.to_string_full()),
    }
}

fn split_path(path: &Path) -> Option<(&str, &Path)> {
    let mut components = path.components();
    let first = components.next()?;
    let std::path::Component::Normal(name) = first else {
        return None;
    };
    Some((name.to_str()?, components.as_path()))
}

#[cfg(test)]
mod tests {
    use objects::object::{Annotation, AnnotationKind, ChangeId};
    use tempfile::TempDir;

    use super::{Repository, *};

    fn setup() -> (TempDir, Repository) {
        let dir = TempDir::new().unwrap();
        let repo = Repository::init_default(dir.path()).unwrap();
        (dir, repo)
    }

    fn make_annotation(scope: AnnotationScope, content: &str) -> Annotation {
        Annotation::new(
            scope,
            AnnotationKind::Rationale,
            content.to_string(),
            vec![],
            "test@example.com".to_string(),
            1700000000,
            None,
            None,
        )
    }

    #[test]
    fn get_and_set_context_blob_for_file_target() {
        let (_dir, repo) = setup();
        let target = ContextTarget::file("src/main.rs").unwrap();
        let blob = ContextBlob::new(vec![make_annotation(AnnotationScope::File, "Entry point")]);

        let root = repo.set_context_blob(None, &target, &blob).unwrap();
        let retrieved = repo.get_context_blob(&root, &target).unwrap().unwrap();

        assert_eq!(retrieved, blob);
    }

    #[test]
    fn supports_state_targets() {
        let (_dir, repo) = setup();
        let target = ContextTarget::state(ChangeId::generate());
        let blob = ContextBlob::new(vec![make_annotation(AnnotationScope::File, "review note")]);

        let root = repo.set_context_blob(None, &target, &blob).unwrap();
        let retrieved = repo.get_context_blob(&root, &target).unwrap().unwrap();
        assert_eq!(retrieved, blob);
    }

    #[test]
    fn remove_context_blob_by_scope() {
        let (_dir, repo) = setup();
        let target = ContextTarget::file("src/lib.rs").unwrap();
        let blob = ContextBlob::new(vec![
            make_annotation(AnnotationScope::File, "file-level"),
            make_annotation(AnnotationScope::Lines(1, 10), "range-level"),
        ]);

        let root = repo.set_context_blob(None, &target, &blob).unwrap();
        let new_root = repo
            .remove_context_at_target(&root, &target, Some(&AnnotationScope::Lines(1, 10)))
            .unwrap()
            .unwrap();
        let remaining = repo.get_context_blob(&new_root, &target).unwrap().unwrap();

        assert_eq!(remaining.annotations.len(), 1);
        assert_eq!(
            remaining
                .annotations
                .first()
                .unwrap()
                .current_revision()
                .unwrap()
                .content,
            "file-level"
        );
    }

    #[test]
    fn list_context_entries_filters_by_prefix() {
        let (_dir, repo) = setup();
        let target1 = ContextTarget::file("src/main.rs").unwrap();
        let target2 = ContextTarget::file("src/lib.rs").unwrap();
        let target3 = ContextTarget::file("tests/test.rs").unwrap();
        let blob1 = ContextBlob::new(vec![make_annotation(AnnotationScope::File, "first")]);
        let blob2 = ContextBlob::new(vec![make_annotation(AnnotationScope::File, "second")]);
        let blob3 = ContextBlob::new(vec![make_annotation(AnnotationScope::File, "third")]);

        let root1 = repo.set_context_blob(None, &target1, &blob1).unwrap();
        let root2 = repo
            .set_context_blob(Some(&root1), &target2, &blob2)
            .unwrap();
        let root3 = repo
            .set_context_blob(Some(&root2), &target3, &blob3)
            .unwrap();

        let all = repo.list_context_entries(&root3, None).unwrap();
        assert_eq!(all.len(), 3);

        let src_only = repo
            .list_context_entries(&root3, Some(Path::new("src")))
            .unwrap();
        assert_eq!(src_only.len(), 2);

        let exact_root_file = repo
            .list_context_entries(&root3, Some(Path::new("tests/test.rs")))
            .unwrap();
        assert_eq!(exact_root_file.len(), 1);
    }

    #[test]
    fn find_annotation_returns_target_and_index() {
        let (_dir, repo) = setup();
        let target = ContextTarget::file("src/main.rs").unwrap();
        let blob = ContextBlob::new(vec![make_annotation(AnnotationScope::File, "first")]);
        let annotation_id = blob.annotations[0].annotation_id.clone();
        let root = repo.set_context_blob(None, &target, &blob).unwrap();

        let found = repo
            .find_annotation(&root, &annotation_id)
            .unwrap()
            .unwrap();
        assert_eq!(found.0, target);
        assert_eq!(found.2, 0);
    }

    /// Build a synthetic State whose `context` field points at a freshly
    /// rooted context tree containing the given annotations on a single
    /// file target. The state's `tree` is left at its default — the helpers
    /// under test never inspect it.
    fn state_with_context(repo: &Repository, path: &str, anns: Vec<Annotation>) -> State {
        let target = ContextTarget::file(path).unwrap();
        let blob = ContextBlob::new(anns);
        let root = repo.set_context_blob(None, &target, &blob).unwrap();
        let mut state = State::new_snapshot(
            ContentHash::compute(b""),
            vec![],
            objects::object::Attribution::human(objects::object::Principal::new(
                "test",
                "test@example.com",
            )),
        );
        state = state.with_context(root);
        state
    }

    fn ann_with_id(id: &str, content: &str, created_at: i64) -> Annotation {
        let mut a = Annotation::new(
            AnnotationScope::File,
            AnnotationKind::Rationale,
            content.to_string(),
            vec![],
            "test@example.com".to_string(),
            created_at,
            None,
            None,
        );
        a.annotation_id = id.to_string();
        a
    }

    #[test]
    fn inherit_parent_context_passes_through_pointer() {
        let (_dir, repo) = setup();
        let parent = state_with_context(
            &repo,
            "src/lib.rs",
            vec![make_annotation(AnnotationScope::File, "first")],
        );
        let inherited = Repository::inherit_parent_context(&parent);
        assert_eq!(inherited, parent.context);
    }

    #[test]
    fn inherit_parent_context_yields_none_when_parent_has_none() {
        let parent = State::new_snapshot(
            ContentHash::compute(b""),
            vec![],
            objects::object::Attribution::human(objects::object::Principal::new(
                "test",
                "test@example.com",
            )),
        );
        assert_eq!(Repository::inherit_parent_context(&parent), None);
    }

    #[test]
    fn union_parent_contexts_returns_none_for_empty_parents() {
        let (_dir, repo) = setup();
        let p = State::new_snapshot(
            ContentHash::compute(b""),
            vec![],
            objects::object::Attribution::human(objects::object::Principal::new(
                "test",
                "test@example.com",
            )),
        );
        let merged = repo.union_parent_contexts(&[&p, &p]).unwrap();
        assert_eq!(merged, None);
    }

    #[test]
    fn union_parent_contexts_pointer_copies_when_one_side_has_context() {
        let (_dir, repo) = setup();
        let parent_with = state_with_context(
            &repo,
            "src/lib.rs",
            vec![make_annotation(AnnotationScope::File, "first")],
        );
        let parent_without = State::new_snapshot(
            ContentHash::compute(b""),
            vec![],
            objects::object::Attribution::human(objects::object::Principal::new(
                "test",
                "test@example.com",
            )),
        );
        let merged = repo
            .union_parent_contexts(&[&parent_with, &parent_without])
            .unwrap();
        assert_eq!(merged, parent_with.context);
    }

    #[test]
    fn union_parent_contexts_carries_disjoint_annotations() {
        let (_dir, repo) = setup();
        let left = state_with_context(
            &repo,
            "src/lib.rs",
            vec![ann_with_id("ann-a", "left side", 1)],
        );
        let right = state_with_context(
            &repo,
            "src/main.rs",
            vec![ann_with_id("ann-b", "right side", 1)],
        );
        let merged = repo
            .union_parent_contexts(&[&left, &right])
            .unwrap()
            .expect("merged context root");
        let entries = repo.list_context_entries(&merged, None).unwrap();
        assert_eq!(entries.len(), 2);
        let mut ids: Vec<String> = entries
            .iter()
            .flat_map(|e| e.blob.annotations.iter().map(|a| a.annotation_id.clone()))
            .collect();
        ids.sort();
        assert_eq!(ids, vec!["ann-a".to_string(), "ann-b".to_string()]);
    }

    #[test]
    fn union_parent_contexts_dedupes_same_id_with_newest_revision_wins() {
        let (_dir, repo) = setup();
        let older = ann_with_id("ann-shared", "older content", 1);
        let newer = ann_with_id("ann-shared", "newer content", 9);
        let left = state_with_context(&repo, "src/lib.rs", vec![older]);
        let right = state_with_context(&repo, "src/lib.rs", vec![newer]);
        let merged = repo
            .union_parent_contexts(&[&left, &right])
            .unwrap()
            .expect("merged context root");
        let entries = repo.list_context_entries(&merged, None).unwrap();
        assert_eq!(entries.len(), 1);
        let blob = &entries[0].blob;
        assert_eq!(blob.annotations.len(), 1);
        let revision = blob.annotations[0]
            .current_revision()
            .expect("annotation has a revision");
        assert_eq!(revision.content, "newer content");
    }
}
