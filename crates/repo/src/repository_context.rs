// SPDX-License-Identifier: Apache-2.0
//! Context annotation helpers for attaching metadata to file and state targets.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

use objects::{
    object::{
        Annotation, AnnotationRevision, AnnotationScope, Blob, ContentHash, ContextBlob,
        ContextTarget, EntryType, State, Tree, TreeEntry,
    },
    store::ObjectStore,
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
        let blob = self.require_blob(&blob_hash)?;
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

        self.insert_leaf_at_path(&current_tree, &target.storage_path(), blob_hash)
    }

    /// Delete context at a target (optionally filtered by scope).
    ///
    /// The tombstone stays in the context tree. A concurrent amendment retains
    /// its history when merged, but deletion wins the lifecycle lattice; an
    /// explicit new annotation ID is required to restore current guidance.
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
                for annotation in &mut blob.annotations {
                    if annotation.scope.matches(scope) {
                        annotation.status = objects::object::AnnotationStatus::Deleted;
                    }
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
        let Some(mut blob) = self.get_context_blob(context_root, target)? else {
            return Ok(Some(*context_root));
        };
        for annotation in &mut blob.annotations {
            annotation.status = objects::object::AnnotationStatus::Deleted;
        }
        Ok(Some(self.set_context_blob(
            Some(context_root),
            target,
            &blob,
        )?))
    }

    /// List all context entries in the tree, optionally filtered by file prefix.
    pub fn list_context_entries(
        &self,
        context_root: &ContentHash,
        prefix: Option<&Path>,
    ) -> Result<Vec<ContextEntry>> {
        let tree = self.require_tree(context_root)?;
        let mut results = BTreeMap::new();
        self.walk_context_tree(
            &tree,
            &PathBuf::new(),
            prefix,
            &mut results,
            ContextWalkMode::CanonicalOnly,
        )?;
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

    /// Human-facing locations with unresolved concurrent annotation edits.
    pub fn context_divergences(&self, state: &State) -> Result<Vec<String>> {
        let Some(root) = self.inherit_parent_context(state)? else {
            return Ok(Vec::new());
        };
        let mut locations = Vec::new();
        for entry in self.list_context_entries(&root, None)? {
            for annotation in entry.blob.annotations {
                if !annotation.divergent_revision_ids.is_empty() {
                    locations.push(entry.target.path().unwrap_or("state context").to_string());
                }
            }
        }
        locations.sort();
        locations.dedup();
        Ok(locations)
    }

    // --- private helpers ---

    fn lookup_context_leaf_for_target(
        &self,
        root: &ContentHash,
        target: &ContextTarget,
    ) -> Result<Option<ContentHash>> {
        self.lookup_context_leaf(root, &target.storage_path())
    }

    fn lookup_context_leaf(&self, root: &ContentHash, path: &Path) -> Result<Option<ContentHash>> {
        let Some((name, rest)) = split_path(path) else {
            return Ok(None);
        };
        let tree = self.require_tree(root)?;
        let Some(entry) = tree.get(name) else {
            return Ok(None);
        };
        if rest.as_os_str().is_empty() {
            return Ok(entry.blob_hash());
        }
        if !entry.is_tree() {
            return Ok(None);
        }
        let Some(tree_hash) = entry.tree_hash() else {
            return Ok(None);
        };
        self.lookup_context_leaf(&tree_hash, rest)
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
            let subtree = match tree.get(name).and_then(TreeEntry::tree_hash) {
                Some(hash) => self.require_tree(&hash)?,
                None => Tree::new(),
            };

            let sub_hash = self.insert_leaf_at_path(&subtree, rest, blob_hash)?;
            new_tree.insert(TreeEntry::directory(name, sub_hash)?);
        }

        self.store.put_tree(&new_tree)
    }

    fn walk_context_tree(
        &self,
        tree: &Tree,
        current_path: &Path,
        prefix: Option<&Path>,
        results: &mut BTreeMap<String, (ContextTarget, ContextBlob)>,
        mode: ContextWalkMode,
    ) -> Result<()> {
        for entry in tree.entries() {
            let entry_path = current_path.join(entry.name());
            match entry.entry_type() {
                EntryType::Tree => {
                    if let Some(prefix) = prefix
                        && !prefix.starts_with(&entry_path)
                        && !entry_path.starts_with(prefix)
                        && !entry_path.starts_with("__files")
                        && !entry_path.starts_with("__states")
                    {
                        continue;
                    }
                    if let Some(tree_hash) = entry.tree_hash() {
                        let subtree = self.require_tree(&tree_hash)?;
                        self.walk_context_tree(&subtree, &entry_path, prefix, results, mode)?;
                    }
                }
                EntryType::Blob => {
                    let Some(target) = context_target_from_entry_path(&entry_path, mode) else {
                        continue;
                    };
                    if let Some(prefix) = prefix
                        && let Some(path) = target.path()
                        && !Path::new(path).starts_with(prefix)
                    {
                        continue;
                    }
                    if let Some(blob_hash) = entry.blob_hash() {
                        let blob = self.require_blob(&blob_hash)?;
                        let context = ContextBlob::decode(blob.content()).map_err(|error| {
                            HeddleError::InvalidObject(format!("invalid context blob: {error}"))
                        })?;
                        results.insert(context_entry_key(&target), (target, context));
                    }
                }
                EntryType::Symlink | EntryType::Gitlink | EntryType::Spoollink => {}
            }
        }
        Ok(())
    }

    /// Resolve the context snapshot attached to a parent state.
    pub fn inherit_parent_context(&self, parent: &State) -> Result<Option<ContentHash>> {
        Ok(self
            .latest_state_attachment(&parent.id(), crate::StateAttachmentKind::Context)?
            .and_then(|attachment| match attachment.body {
                objects::object::StateAttachmentBody::Context(hash) => Some(hash),
                _ => None,
            }))
    }

    /// Build a unioned context tree across multiple parent states for a
    /// merge snapshot. Annotations from every parent appear in the result;
    /// when the same `annotation_id` is present on more than one parent,
    /// all revisions survive and concurrent live tips remain explicit.
    ///
    /// Targets that only exist on one side propagate unchanged (single-blob
    /// pointer copy via the existing tree). Targets present on both sides
    /// are merged blob-by-blob: annotations are deduped by id, the per-id
    /// revisions are unioned, and the resulting blob is rewritten via
    /// `set_context_blob`.
    ///
    /// Returns `None` when none of the parents has any context.
    pub fn union_parent_contexts(&self, parents: &[&State]) -> Result<Option<ContentHash>> {
        // Fast paths: nothing or single-parent.
        let mut roots = Vec::new();
        for parent in parents {
            if let Some(root) = self.inherit_parent_context(parent)? {
                roots.push(root);
            }
        }
        if roots.is_empty() {
            return Ok(None);
        }
        if roots.len() == 1 {
            return Ok(roots.pop());
        }
        if roots.iter().all(|r| *r == roots[0]) {
            // All parents pointed at the same context tree; pointer copy.
            return Ok(Some(roots[0]));
        }

        // Walk every parent and merge by `context_entry_key`. Each entry's
        // blob gets unioned into a running map.
        let mut merged: BTreeMap<String, (ContextTarget, ContextBlob)> = BTreeMap::new();
        for parent_root in &roots {
            for entry in self.list_context_entries(parent_root, None)? {
                let key = context_entry_key(&entry.target);
                match merged.remove(&key) {
                    None => {
                        merged.insert(key, (entry.target, entry.blob));
                    }
                    Some((target, existing)) => {
                        let merged_blob = merge_context_blobs(existing, entry.blob)?;
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

/// Preserve every unique revision and every non-dominated tip, including on
/// tombstones. Later merges need that frontier to remain associative. Metadata
/// that cannot be combined without losing a side refuses the merge.
pub fn merge_context_blobs(left: ContextBlob, right: ContextBlob) -> Result<ContextBlob> {
    if left.format_version != right.format_version {
        return Err(HeddleError::InvalidObject("context format mismatch".into()));
    }
    let format_version = left.format_version;
    let mut by_id: BTreeMap<String, Annotation> = BTreeMap::new();
    for annotation in left.annotations.into_iter().chain(right.annotations) {
        match by_id.remove(&annotation.annotation_id) {
            None => {
                by_id.insert(annotation.annotation_id.clone(), annotation);
            }
            Some(existing) => {
                let merged = merge_annotation(existing, annotation)?;
                by_id.insert(merged.annotation_id.clone(), merged);
            }
        }
    }
    Ok(ContextBlob {
        format_version,
        annotations: by_id.into_values().collect(),
    })
}

fn merge_annotation(mut a: Annotation, b: Annotation) -> Result<Annotation> {
    let status = match (a.status, b.status) {
        (objects::object::AnnotationStatus::Deleted, _)
        | (_, objects::object::AnnotationStatus::Deleted) => {
            objects::object::AnnotationStatus::Deleted
        }
        (objects::object::AnnotationStatus::Superseded, _)
        | (_, objects::object::AnnotationStatus::Superseded) => {
            objects::object::AnnotationStatus::Superseded
        }
        _ => objects::object::AnnotationStatus::Active,
    };
    let mut a_metadata = a.clone();
    a_metadata.status = objects::object::AnnotationStatus::Active;
    a_metadata.revisions.clear();
    a_metadata.divergent_revision_ids.clear();
    let mut b_metadata = b.clone();
    b_metadata.status = objects::object::AnnotationStatus::Active;
    b_metadata.revisions.clear();
    b_metadata.divergent_revision_ids.clear();
    if a_metadata != b_metadata {
        return Err(HeddleError::InvalidObject(format!(
            "concurrent annotation metadata differs: {}",
            a.annotation_id
        )));
    }
    let identical = (a == b).then(|| a.clone());

    let a_tips: BTreeSet<String> = a
        .current_revision_ids()
        .into_iter()
        .map(str::to_owned)
        .collect();
    let b_tips: BTreeSet<String> = b
        .current_revision_ids()
        .into_iter()
        .map(str::to_owned)
        .collect();
    let a_ids: BTreeSet<&str> = a.revisions.iter().map(|r| r.revision_id.as_str()).collect();
    let b_ids: BTreeSet<&str> = b.revisions.iter().map(|r| r.revision_id.as_str()).collect();
    let mut tips = a_tips.union(&b_tips).cloned().collect::<BTreeSet<_>>();
    tips.retain(|id| {
        !(a_ids.contains(id.as_str()) && !a_tips.contains(id))
            && !(b_ids.contains(id.as_str()) && !b_tips.contains(id))
    });

    let mut revisions: BTreeMap<String, AnnotationRevision> = BTreeMap::new();
    for revision in a.revisions.into_iter().chain(b.revisions) {
        match revisions.get(&revision.revision_id) {
            Some(existing) if existing != &revision => {
                return Err(HeddleError::InvalidObject(format!(
                    "annotation revision ID has conflicting content: {}",
                    revision.revision_id
                )));
            }
            Some(_) => {}
            None => {
                revisions.insert(revision.revision_id.clone(), revision);
            }
        }
    }
    if let Some(original) = identical {
        return Ok(original);
    }
    a.revisions = revisions.into_values().collect();
    a.status = status;
    a.revisions.sort_by(|left, right| {
        (left.created_at, &left.revision_id).cmp(&(right.created_at, &right.revision_id))
    });
    if tips.len() == 1
        && let Some(tip) = tips.iter().next()
        && let Some(index) = a
            .revisions
            .iter()
            .position(|revision| &revision.revision_id == tip)
    {
        let current = a.revisions.remove(index);
        a.revisions.push(current);
    }
    a.divergent_revision_ids = if tips.len() > 1 {
        tips.into_iter().collect()
    } else {
        Vec::new()
    };
    Ok(a)
}

fn context_entry_key(target: &ContextTarget) -> String {
    match target {
        ContextTarget::File { path } => format!("file:{path}"),
        ContextTarget::State { state_id } => format!("state:{}", state_id.to_string_full()),
    }
}

#[derive(Clone, Copy)]
enum ContextWalkMode {
    CanonicalOnly,
}

fn context_target_from_entry_path(path: &Path, mode: ContextWalkMode) -> Option<ContextTarget> {
    match mode {
        ContextWalkMode::CanonicalOnly => ContextTarget::from_storage_path(path),
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
    #[cfg(feature = "tree-sitter-symbols")]
    use std::fs;

    use objects::object::{
        Annotation, AnnotationKind, AnnotationStatus, StateAttachment, StateAttachmentBody,
    };
    #[cfg(feature = "tree-sitter-symbols")]
    use objects::object::{AnnotationAnchorStatus, StalenessStatus};
    use tempfile::TempDir;

    use super::{Repository, *};

    fn setup() -> (TempDir, Repository) {
        let dir = TempDir::new().unwrap();
        let repo = crate::init_test_repository(dir.path()).unwrap();
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
            objects::object::VisibilityTier::Public,
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
        let target = ContextTarget::state(crate::test_state_id());
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

        assert_eq!(remaining.annotations.len(), 2);
        assert_eq!(remaining.annotations[0].status, AnnotationStatus::Active);
        assert_eq!(remaining.annotations[1].status, AnnotationStatus::Deleted);
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
        let state = State::new_snapshot(
            ContentHash::compute(b""),
            vec![],
            objects::object::Attribution::human(objects::object::Principal::new(
                "test",
                "test@example.com",
            )),
        );
        repo.store().put_state(&state).unwrap();
        repo.put_state_attachment(&StateAttachment {
            state_id: state.id(),
            body: StateAttachmentBody::Context(root),
            attribution: state.attribution.clone(),
            created_at: chrono::Utc::now(),
            supersedes: None,
        })
        .unwrap();
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
            objects::object::VisibilityTier::Public,
        );
        a.annotation_id = id.to_string();
        a
    }

    #[cfg(feature = "tree-sitter-symbols")]
    fn travel_annotation(
        state: &State,
        scope: AnnotationScope,
        source_hash: Option<ContentHash>,
    ) -> Annotation {
        Annotation::new(
            scope,
            AnnotationKind::Invariant,
            "Keep this guard intact".to_string(),
            vec![],
            "test@example.com".to_string(),
            1_700_000_000,
            source_hash,
            Some(state.id()),
            objects::object::VisibilityTier::Public,
        )
    }

    #[cfg(feature = "tree-sitter-symbols")]
    fn attach_context(repo: &Repository, state: &State, path: &str, annotations: Vec<Annotation>) {
        let target = ContextTarget::file(path).unwrap();
        let root = repo
            .set_context_blob(None, &target, &ContextBlob::new(annotations))
            .unwrap();
        repo.put_state_attachment(&StateAttachment {
            state_id: state.id(),
            body: StateAttachmentBody::Context(root),
            attribution: state.attribution.clone(),
            created_at: chrono::Utc::now(),
            supersedes: None,
        })
        .unwrap();
    }

    #[cfg(feature = "tree-sitter-symbols")]
    fn snapshot_src_files(repo: &Repository, files: &[(&str, &str)], message: &str) -> State {
        let mut blobs = Vec::new();
        let mut entries = Vec::new();
        for (name, source) in files {
            let blob = Blob::from_slice(source.as_bytes());
            entries.push(TreeEntry::file(*name, blob.hash(), false).unwrap());
            blobs.push(blob);
        }
        let root = if entries.is_empty() {
            Tree::new()
        } else {
            let src = Tree::from_entries(entries);
            let src_hash = repo.store().put_tree(&src).unwrap();
            Tree::from_entries(vec![TreeEntry::directory("src", src_hash).unwrap()])
        };
        repo.snapshot_tree_with_blobs_with_attribution_profiled(
            root,
            blobs,
            Some(message.to_string()),
            None,
            objects::object::Attribution::human(objects::object::Principal::new(
                "test",
                "test@example.com",
            )),
        )
        .unwrap()
        .state
    }

    #[test]
    fn inherit_parent_context_passes_through_pointer() {
        let (_dir, repo) = setup();
        let parent = state_with_context(
            &repo,
            "src/lib.rs",
            vec![make_annotation(AnnotationScope::File, "first")],
        );
        let inherited = repo.inherit_parent_context(&parent).unwrap();
        assert!(inherited.is_some());
    }

    #[test]
    fn inherit_parent_context_yields_none_when_parent_has_none() {
        let (_dir, repo) = setup();
        let parent = State::new_snapshot(
            ContentHash::compute(b""),
            vec![],
            objects::object::Attribution::human(objects::object::Principal::new(
                "test",
                "test@example.com",
            )),
        );
        assert_eq!(repo.inherit_parent_context(&parent).unwrap(), None);
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
        assert_eq!(merged, repo.inherit_parent_context(&parent_with).unwrap());
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
    fn union_parent_contexts_keeps_concurrent_revision_histories_and_marks_divergence() {
        let (_dir, repo) = setup();
        let base = ann_with_id("ann-shared", "shared base", 1);
        let mut agent_a = base.clone();
        agent_a.revise(
            AnnotationKind::Rationale,
            "agent A edit".into(),
            vec![],
            "a@example.com".into(),
            2,
            None,
            None,
        );
        let mut agent_b = base;
        agent_b.revise(
            AnnotationKind::Rationale,
            "agent B edit".into(),
            vec![],
            "b@example.com".into(),
            3,
            None,
            None,
        );
        let left = state_with_context(&repo, "src/lib.rs", vec![agent_a]);
        let right = state_with_context(&repo, "src/lib.rs", vec![agent_b]);
        let merged = repo
            .union_parent_contexts(&[&left, &right])
            .unwrap()
            .expect("merged context root");
        let entries = repo.list_context_entries(&merged, None).unwrap();
        assert_eq!(entries.len(), 1);
        let blob = &entries[0].blob;
        assert_eq!(blob.annotations.len(), 1);
        let annotation = &blob.annotations[0];
        assert_eq!(annotation.revisions.len(), 3, "both agents' edits survive");
        let contents: Vec<_> = annotation
            .revisions
            .iter()
            .map(|r| r.content.as_str())
            .collect();
        assert!(contents.contains(&"agent A edit"));
        assert!(contents.contains(&"agent B edit"));
        assert_eq!(annotation.divergent_revision_ids.len(), 2);
        let state = state_with_context(&repo, "src/lib.rs", vec![annotation.clone()]);
        assert_eq!(
            repo.context_divergences(&state).unwrap(),
            vec!["src/lib.rs"]
        );
        let reversed = repo
            .union_parent_contexts(&[&right, &left])
            .unwrap()
            .unwrap();
        let reversed_blob = repo
            .get_context_blob(&reversed, &ContextTarget::file("src/lib.rs").unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(reversed_blob.annotations[0], *annotation);
    }

    #[test]
    fn union_parent_contexts_does_not_mark_observed_successor_as_divergent() {
        let (_dir, repo) = setup();
        let mut first = ann_with_id("ann-shared", "base", 1);
        first.revise(
            AnnotationKind::Rationale,
            "first edit".into(),
            vec![],
            "a@example.com".into(),
            20,
            None,
            None,
        );
        let mut successor = first.clone();
        successor.revise(
            AnnotationKind::Rationale,
            "successor".into(),
            vec![],
            "b@example.com".into(),
            10,
            None,
            None,
        );
        let left = state_with_context(&repo, "src/lib.rs", vec![first]);
        let right = state_with_context(&repo, "src/lib.rs", vec![successor]);
        let merged = repo
            .union_parent_contexts(&[&left, &right])
            .unwrap()
            .unwrap();
        let blob = repo
            .get_context_blob(&merged, &ContextTarget::file("src/lib.rs").unwrap())
            .unwrap()
            .unwrap();
        let annotation = &blob.annotations[0];
        assert_eq!(annotation.revisions.len(), 3);
        assert!(annotation.divergent_revision_ids.is_empty());
        assert_eq!(annotation.current_revision().unwrap().content, "successor");
    }

    #[cfg(feature = "tree-sitter-symbols")]
    #[test]
    fn context_symbol_annotation_rebinds_across_file_rename() {
        let (dir, repo) = setup();
        fs::create_dir(dir.path().join("src")).unwrap();
        fs::write(
            dir.path().join("src/old.rs"),
            "fn guarded() {\n    let value = 1;\n}\n",
        )
        .unwrap();
        let first = repo.snapshot(Some("first".to_string()), None).unwrap();
        let annotation = travel_annotation(
            &first,
            AnnotationScope::Symbol {
                name: "guarded".to_string(),
                resolved_lines: Some((1, 3)),
            },
            None,
        );
        attach_context(
            &repo,
            &first,
            "src/old.rs",
            vec![
                annotation,
                travel_annotation(&first, AnnotationScope::File, None),
                travel_annotation(&first, AnnotationScope::Lines(1, 2), None),
            ],
        );

        let second = snapshot_src_files(
            &repo,
            &[("new.rs", "fn guarded() {\n    let value = 1;\n}\n")],
            "rename",
        );
        let moved_root = repo
            .inherit_parent_context(&second)
            .unwrap()
            .expect("renamed snapshot context");

        let stale_lines = repo
            .get_context_blob(&moved_root, &ContextTarget::file("src/old.rs").unwrap())
            .unwrap()
            .expect("line ranges stay on their original target");
        assert_eq!(stale_lines.annotations.len(), 1);
        assert!(matches!(
            stale_lines.annotations[0].scope,
            AnnotationScope::Lines(1, 2)
        ));
        let moved = repo
            .get_context_blob(&moved_root, &ContextTarget::file("src/new.rs").unwrap())
            .unwrap()
            .expect("context moved to renamed path");
        assert_eq!(moved.annotations.len(), 2);
        assert!(matches!(
            moved.annotations[0].scope,
            AnnotationScope::Symbol { ref name, .. } if name == "guarded"
        ));
        assert!(matches!(moved.annotations[1].scope, AnnotationScope::File));
    }

    #[cfg(feature = "tree-sitter-symbols")]
    #[test]
    fn context_symbol_annotation_rebinds_across_worktree_mkdir_rename() {
        const SOURCE: &str = "def greet(name):\n    return f\"hello {name}\"\n";
        let (dir, repo) = setup();
        fs::write(dir.path().join("lib.py"), SOURCE).unwrap();
        let first = repo.snapshot(Some("first".to_string()), None).unwrap();
        attach_context(
            &repo,
            &first,
            "lib.py",
            vec![travel_annotation(
                &first,
                AnnotationScope::Symbol {
                    name: "greet".to_string(),
                    resolved_lines: Some((1, 2)),
                },
                Some(ContentHash::compute(SOURCE.as_bytes())),
            )],
        );

        fs::create_dir(dir.path().join("pkg")).unwrap();
        fs::rename(dir.path().join("lib.py"), dir.path().join("pkg/greeter.py")).unwrap();
        let second = repo
            .snapshot(Some("mkdir+rename".to_string()), None)
            .unwrap();
        let moved_root = repo
            .inherit_parent_context(&second)
            .unwrap()
            .expect("renamed snapshot context");

        assert!(
            repo.get_context_blob(&moved_root, &ContextTarget::file("lib.py").unwrap())
                .unwrap()
                .is_none(),
            "traveled context must leave the old path empty"
        );
        let moved = repo
            .get_context_blob(&moved_root, &ContextTarget::file("pkg/greeter.py").unwrap())
            .unwrap()
            .expect("context moved to renamed path");
        assert_eq!(moved.annotations.len(), 1);
        assert!(matches!(
            moved.annotations[0].scope,
            AnnotationScope::Symbol { ref name, .. } if name == "greet"
        ));
        let status = crate::staleness::check_annotation_staleness(
            &repo,
            &moved.annotations[0],
            &ContextTarget::file("pkg/greeter.py").unwrap(),
            &second,
        )
        .unwrap();
        assert_ne!(
            status,
            StalenessStatus::FileMissing,
            "traveled annotation must resolve on the new path, got {status:?}"
        );
    }

    #[cfg(feature = "tree-sitter-symbols")]
    #[test]
    fn context_file_rename_with_two_candidates_is_ambiguous() {
        const SOURCE: &str = "fn guarded() {\n    let value = 1;\n}\n";
        let (dir, repo) = setup();
        fs::create_dir(dir.path().join("src")).unwrap();
        fs::write(dir.path().join("src/old.rs"), SOURCE).unwrap();
        let first = repo.snapshot(Some("first".to_string()), None).unwrap();
        attach_context(
            &repo,
            &first,
            "src/old.rs",
            vec![travel_annotation(
                &first,
                AnnotationScope::Symbol {
                    name: "guarded".to_string(),
                    resolved_lines: Some((1, 3)),
                },
                Some(ContentHash::compute(
                    b"fn guarded() {\n    let value = 1;\n}",
                )),
            )],
        );

        let second = snapshot_src_files(&repo, &[("a.rs", SOURCE), ("b.rs", SOURCE)], "ambiguous");
        let root = repo
            .inherit_parent_context(&second)
            .unwrap()
            .expect("context root");
        let target = ContextTarget::file("src/old.rs").unwrap();
        let blob = repo
            .get_context_blob(&root, &target)
            .unwrap()
            .expect("ambiguous context stays at its prior target");

        assert_eq!(
            blob.annotations[0].anchor_status,
            AnnotationAnchorStatus::Ambiguous {
                candidate_paths: vec!["src/a.rs".to_string(), "src/b.rs".to_string()],
            }
        );
        assert_eq!(
            crate::staleness::check_annotation_staleness(
                &repo,
                &blob.annotations[0],
                &target,
                &second,
            )
            .unwrap(),
            StalenessStatus::AmbiguousFileMove {
                candidate_paths: vec!["src/a.rs".to_string(), "src/b.rs".to_string()],
            }
        );
    }

    #[cfg(feature = "tree-sitter-symbols")]
    #[test]
    fn deleted_context_target_remains_file_missing() {
        const SOURCE: &str = "fn guarded() {\n    let value = 1;\n}\n";
        let (dir, repo) = setup();
        fs::create_dir(dir.path().join("src")).unwrap();
        fs::write(dir.path().join("src/old.rs"), SOURCE).unwrap();
        let first = repo.snapshot(Some("first".to_string()), None).unwrap();
        attach_context(
            &repo,
            &first,
            "src/old.rs",
            vec![travel_annotation(
                &first,
                AnnotationScope::File,
                Some(ContentHash::compute(SOURCE.as_bytes())),
            )],
        );

        let second = snapshot_src_files(&repo, &[], "delete");
        let root = repo
            .inherit_parent_context(&second)
            .unwrap()
            .expect("context root");
        let target = ContextTarget::file("src/old.rs").unwrap();
        let blob = repo
            .get_context_blob(&root, &target)
            .unwrap()
            .expect("orphaned context remains addressable");

        assert_eq!(
            blob.annotations[0].anchor_status,
            AnnotationAnchorStatus::Orphaned
        );
        assert_eq!(
            crate::staleness::check_annotation_staleness(
                &repo,
                &blob.annotations[0],
                &target,
                &second,
            )
            .unwrap(),
            StalenessStatus::FileMissing
        );
    }
}

#[cfg(test)]
mod review_1863 {
    use objects::object::{AnnotationKind, AnnotationStatus};
    use proptest::prelude::*;

    use super::*;
    fn base() -> Annotation {
        Annotation::new(
            AnnotationScope::File,
            AnnotationKind::Constraint,
            "base".into(),
            vec![],
            "review".into(),
            10,
            None,
            None,
            objects::object::VisibilityTier::Public,
        )
    }
    fn amend(a: &Annotation, text: &str, ts: i64) -> Annotation {
        let mut a = a.clone();
        a.revise(
            AnnotationKind::Constraint,
            text.into(),
            vec![],
            "review".into(),
            ts,
            None,
            None,
        );
        a
    }
    fn round3_seed() -> Annotation {
        Annotation::new(
            AnnotationScope::File,
            AnnotationKind::Constraint,
            "seed".into(),
            vec![],
            "review".into(),
            0,
            None,
            None,
            objects::object::VisibilityTier::Public,
        )
    }
    #[test]
    fn review_three_way_algebra_duplicate_and_resolution() {
        let seed = base();
        let a = amend(&seed, "a", 20);
        let b = amend(&seed, "b", 5);
        let c = amend(&seed, "c", 30);
        let ab = merge_annotation(a.clone(), b.clone()).unwrap();
        let abc = merge_annotation(ab.clone(), c.clone()).unwrap();
        for (x, y, z) in [
            (a.clone(), b.clone(), c.clone()),
            (a.clone(), c.clone(), b.clone()),
            (b.clone(), a.clone(), c.clone()),
            (b.clone(), c.clone(), a.clone()),
            (c.clone(), a.clone(), b.clone()),
            (c.clone(), b.clone(), a.clone()),
        ] {
            assert_eq!(
                merge_annotation(merge_annotation(x.clone(), y.clone()).unwrap(), z.clone())
                    .unwrap(),
                abc
            );
            assert_eq!(
                merge_annotation(x, merge_annotation(y, z).unwrap()).unwrap(),
                abc
            );
        }
        assert_eq!(merge_annotation(abc.clone(), abc.clone()).unwrap(), abc);
        assert_eq!(merge_annotation(abc.clone(), a).unwrap(), abc);
        assert_eq!(abc.divergent_revision_ids.len(), 3);
        let resolved = amend(&ab, "decision", 1);
        assert_eq!(merge_annotation(resolved.clone(), ab).unwrap(), resolved);
        let unresolved_c = merge_annotation(resolved, c).unwrap();
        assert_eq!(unresolved_c.divergent_revision_ids.len(), 2);
        let mut forged = b.clone();
        forged.revisions.last_mut().unwrap().content = "conflicting duplicate".into();
        assert!(merge_annotation(b, forged).is_err());
    }
    #[test]
    fn review_superseded_and_observed_active_converge() {
        let a = base();
        let mut retired = amend(&a, "final version", 20);
        retired.mark_superseded();
        let merged = merge_annotation(retired, a)
            .expect("superseding an observed revision must converge with its old replica");
        assert_eq!(merged.status, AnnotationStatus::Superseded);
        assert_eq!(merged.revisions.len(), 2);
        assert!(merged.divergent_revision_ids.is_empty());
    }
    #[test]
    fn review_delete_and_amend_not_silently_current() {
        let a = base();
        let (_dir, repo) = {
            let dir = tempfile::TempDir::new().unwrap();
            let repo = crate::init_test_repository(dir.path()).unwrap();
            (dir, repo)
        };
        let target = ContextTarget::file("deleted.rs").unwrap();
        let root = repo
            .set_context_blob(None, &target, &ContextBlob::new(vec![a.clone()]))
            .unwrap();
        let deleted_root = repo.remove_context_target(&root, &target).unwrap().unwrap();
        let deletion = repo
            .get_context_blob(&deleted_root, &target)
            .unwrap()
            .unwrap();
        let live = ContextBlob::new(vec![amend(&a, "concurrent amendment", 20)]);
        let merged = merge_context_blobs(deletion, live).unwrap();
        assert_eq!(format!("{:?}", merged.annotations[0].status), "Deleted");
        assert_eq!(merged.annotations[0].revisions.len(), 2);
        assert!(merged.annotations[0].divergent_revision_ids.is_empty());
    }

    #[test]
    fn round3_superseded_divergence_is_idempotent() {
        let seed = round3_seed();
        let a = amend(&seed, "a", 1);
        let b = amend(&seed, "b", 4);
        let mut superseded_with_divergence = merge_annotation(a.clone(), b.clone()).unwrap();
        superseded_with_divergence.mark_superseded();
        assert_eq!(
            merge_annotation(
                superseded_with_divergence.clone(),
                superseded_with_divergence.clone()
            )
            .unwrap(),
            superseded_with_divergence
        );
    }

    #[test]
    fn round3_lifecycle_merge_is_associative() {
        let seed = round3_seed();
        let a = amend(&seed, "a", 1);
        let b = amend(&seed, "b", 4);
        let c = amend(&a, "c", 2);
        let d = amend(&b, "d", 3);
        for status in [AnnotationStatus::Deleted, AnnotationStatus::Superseded] {
            let mut retired = merge_annotation(a.clone(), b.clone()).unwrap();
            retired.status = status;
            retired.divergent_revision_ids.clear();
            let left = merge_annotation(
                merge_annotation(retired.clone(), c.clone()).unwrap(),
                d.clone(),
            )
            .unwrap();
            let right =
                merge_annotation(retired, merge_annotation(c.clone(), d.clone()).unwrap()).unwrap();
            assert_eq!(left, right, "{status:?} merge must be associative");
        }
    }

    #[test]
    fn round3_delete_keeps_unresolved_frontier_for_later_merges() {
        let dir = tempfile::TempDir::new().unwrap();
        let repo = crate::init_test_repository(dir.path()).unwrap();
        let target = ContextTarget::file("deleted.rs").unwrap();
        let seed = base();
        let divergent = merge_annotation(amend(&seed, "a", 20), amend(&seed, "b", 30)).unwrap();
        let root = repo
            .set_context_blob(None, &target, &ContextBlob::new(vec![divergent.clone()]))
            .unwrap();
        let deleted = repo.remove_context_target(&root, &target).unwrap().unwrap();
        let actual = repo.get_context_blob(&deleted, &target).unwrap().unwrap();
        assert_eq!(actual.annotations[0].status, AnnotationStatus::Deleted);
        assert_eq!(
            actual.annotations[0].divergent_revision_ids,
            divergent.divergent_revision_ids
        );
    }

    proptest! {
        #![proptest_config(ProptestConfig { failure_persistence: None, .. ProptestConfig::default() })]
        #[test]
        fn round3_random_history_join_laws(
            edits in prop::collection::vec((0usize..16, 0i64..1000, 0u8..3), 3..12),
            picks in (0usize..16, 0usize..16, 0usize..16),
        ) {
            let mut history = vec![base()];
            for (parent, time, lifecycle) in edits {
                let source = &history[parent % history.len()];
                let mut next = amend(source, &format!("revision-{}", history.len()), time);
                next.status = match lifecycle {
                    0 => AnnotationStatus::Active,
                    1 => AnnotationStatus::Superseded,
                    _ => AnnotationStatus::Deleted,
                };
                history.push(next);
            }
            let a = history[picks.0 % history.len()].clone();
            let b = history[picks.1 % history.len()].clone();
            let c = history[picks.2 % history.len()].clone();
            prop_assert_eq!(merge_annotation(a.clone(), a.clone()).unwrap(), a.clone());
            prop_assert_eq!(merge_annotation(a.clone(), b.clone()).unwrap(), merge_annotation(b.clone(), a.clone()).unwrap());
            prop_assert_eq!(
                merge_annotation(merge_annotation(a.clone(), b.clone()).unwrap(), c.clone()).unwrap(),
                merge_annotation(a, merge_annotation(b, c).unwrap()).unwrap()
            );
        }
    }
}
