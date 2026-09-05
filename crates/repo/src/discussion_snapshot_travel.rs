// SPDX-License-Identifier: Apache-2.0
//! Snapshot-time persistence for discussion anchor travel.

#![cfg(feature = "tree-sitter-symbols")]

use std::{collections::HashMap, path::PathBuf};

use objects::{
    object::{
        Blob, CollaborationAnchor, CollaborationAnchorStatus, CollaborationIdempotencyKey,
        CollaborationOperationBodyV1, CollaborationOperationEnvelope, ContentHash, Discussion,
        DiscussionResolution, DiscussionsBlob, EntryType, MaterializedDiscussion, State,
        StateAttachmentBody, StateId, Tree,
    },
    store::ObjectStore,
};
use oplog::OpLogBackend;
use refs::RefBackend;

use crate::{
    CollaborationStore, HeddleError, Repository, Result,
    discussion_anchor_travel::{travel_anchors, travel_symbol_anchor},
};

impl<R, O, S> Repository<R, O, S>
where
    R: RefBackend,
    O: OpLogBackend,
    S: ObjectStore,
{
    pub(crate) fn compute_and_persist_discussion_anchor_travel(
        &self,
        parent_state: &State,
        new_state: &State,
        new_tree: &Tree,
        source_blobs: Option<&HashMap<ContentHash, &[u8]>>,
        source_trees: Option<&HashMap<ContentHash, &Tree>>,
    ) -> Result<Option<ContentHash>> {
        let parent_discussions_hash = self
            .latest_state_attachment(&parent_state.id(), crate::StateAttachmentKind::Discussions)?
            .and_then(|attachment| match attachment.body {
                StateAttachmentBody::Discussions(hash) => Some(hash),
                _ => None,
            });
        let collaboration_store = self
            .heddle_dir()
            .join("collaboration")
            .exists()
            .then(|| CollaborationStore::open(self.heddle_dir()))
            .transpose()?;
        let collaboration_discussions = match &collaboration_store {
            Some(store) => store
                .materialize()?
                .discussions
                .into_values()
                .filter(|discussion| {
                    discussion.resolution.is_none()
                        && matches!(discussion.anchor, CollaborationAnchor::Symbol { .. })
                })
                .collect::<Vec<_>>(),
            None => Vec::new(),
        };

        if parent_discussions_hash.is_none() && collaboration_discussions.is_empty() {
            return Ok(None);
        }

        let new_files = self.collect_tree_file_bytes(new_tree, source_blobs, source_trees)?;
        if let Some(store) = &collaboration_store {
            self.persist_collaboration_anchor_travel(
                store,
                new_state,
                &new_files,
                collaboration_discussions,
            )?;
        }

        let Some(parent_discussions_hash) = parent_discussions_hash else {
            return Ok(None);
        };
        let parent_blob = self
            .store()
            .get_blob(&parent_discussions_hash)?
            .ok_or_else(|| missing_object("blob", parent_discussions_hash))?;
        let mut discussions = DiscussionsBlob::decode(parent_blob.content()).map_err(|err| {
            HeddleError::Serialization(format!("decode parent discussions blob: {err}"))
        })?;
        let open_discussions: Vec<Discussion> = discussions
            .discussions
            .iter()
            .filter(|discussion| matches!(discussion.resolution, DiscussionResolution::Open))
            .cloned()
            .collect();

        if open_discussions.is_empty() {
            return Ok(Some(parent_discussions_hash));
        }

        let baseline_files = self.collect_discussion_baseline_file_bytes(&open_discussions)?;
        let mut updates = Vec::new();
        for (opened_against_state, discussions) in
            group_discussions_by_opened_state(open_discussions)
        {
            let old_files = baseline_files.get(&opened_against_state).ok_or_else(|| {
                HeddleError::Config(format!(
                    "missing discussion baseline files for state {opened_against_state}"
                ))
            })?;
            updates.extend(travel_anchors(old_files, &new_files, &discussions));
        }

        for update in updates {
            if let Some(discussion) = discussions
                .discussions
                .iter_mut()
                .find(|discussion| discussion.id == update.discussion_id)
            {
                discussion.anchor = update.new_anchor;
                discussion.body_changed_since_open = update.body_changed_since_open;
                discussion.anchor_ambiguous = update.ambiguous;
                discussion.orphaned = update.orphaned;
            }
        }

        let bytes = discussions
            .encode()
            .map_err(|err| HeddleError::Serialization(format!("encode discussions blob: {err}")))?;
        let hash = self.store().put_blob(&Blob::new(bytes))?;
        Ok(Some(hash))
    }

    fn persist_collaboration_anchor_travel(
        &self,
        store: &CollaborationStore,
        new_state: &State,
        new_files: &HashMap<String, Vec<u8>>,
        discussions: Vec<MaterializedDiscussion>,
    ) -> Result<()> {
        let mut baseline_files = HashMap::new();
        for discussion in discussions {
            let CollaborationAnchor::Symbol {
                state_id,
                path,
                symbol,
            } = &discussion.anchor
            else {
                continue;
            };
            if *state_id == new_state.id() {
                continue;
            }
            if !baseline_files.contains_key(state_id) {
                let baseline_state = self
                    .store()
                    .get_state(state_id)?
                    .ok_or_else(|| missing_state(*state_id))?;
                let baseline_tree = self
                    .store()
                    .get_tree(&baseline_state.tree)?
                    .ok_or_else(|| missing_object("tree", baseline_state.tree))?;
                baseline_files.insert(
                    *state_id,
                    self.collect_tree_file_bytes(&baseline_tree, None, None)?,
                );
            }
            let old_files = baseline_files.get(state_id).ok_or_else(|| {
                HeddleError::Config(format!(
                    "missing collaboration discussion baseline files for state {state_id}"
                ))
            })?;
            let original = objects::object::SymbolAnchor::new(path, symbol);
            let update = travel_symbol_anchor(old_files, new_files, &original);
            let status = if update.ambiguous {
                CollaborationAnchorStatus::Ambiguous
            } else if update.orphaned {
                CollaborationAnchorStatus::Orphaned
            } else if update.new_anchor != original {
                CollaborationAnchorStatus::Moved
            } else {
                CollaborationAnchorStatus::Current
            };
            let resolved_state_id = if matches!(
                status,
                CollaborationAnchorStatus::Current | CollaborationAnchorStatus::Moved
            ) {
                new_state.id()
            } else {
                *state_id
            };
            let operation = CollaborationOperationEnvelope::new(
                discussion.discussion_id,
                discussion.heads.iter().copied().collect(),
                CollaborationIdempotencyKey::new(format!("anchor-travel:{}", new_state.id()))
                    .map_err(|error| HeddleError::InvalidObject(error.to_string()))?,
                new_state.attribution.clone(),
                new_state.created_at.timestamp_millis(),
                CollaborationOperationBodyV1::RebindAnchor {
                    anchor: CollaborationAnchor::Symbol {
                        state_id: resolved_state_id,
                        path: update.new_anchor.file,
                        symbol: update.new_anchor.symbol,
                    },
                    status,
                    body_changed_since_open: discussion.body_changed_since_open
                        || update.body_changed_since_open,
                },
            )
            .map_err(|error| HeddleError::InvalidObject(error.to_string()))?;
            store.write_operation(&operation)?;
        }
        Ok(())
    }

    pub(crate) fn collect_tree_file_bytes(
        &self,
        tree: &Tree,
        source_blobs: Option<&HashMap<ContentHash, &[u8]>>,
        source_trees: Option<&HashMap<ContentHash, &Tree>>,
    ) -> Result<HashMap<String, Vec<u8>>> {
        let mut files = HashMap::new();
        self.collect_tree_file_bytes_inner(
            tree,
            PathBuf::new(),
            source_blobs,
            source_trees,
            &mut files,
        )?;
        Ok(files)
    }

    fn collect_discussion_baseline_file_bytes(
        &self,
        discussions: &[Discussion],
    ) -> Result<HashMap<StateId, HashMap<String, Vec<u8>>>> {
        let mut baselines = HashMap::new();
        for discussion in discussions {
            if baselines.contains_key(&discussion.opened_against_state) {
                continue;
            }
            let baseline_state = self
                .store()
                .get_state(&discussion.opened_against_state)?
                .ok_or_else(|| missing_state(discussion.opened_against_state))?;
            let baseline_tree = self
                .store()
                .get_tree(&baseline_state.tree)?
                .ok_or_else(|| missing_object("tree", baseline_state.tree))?;
            baselines.insert(
                discussion.opened_against_state,
                self.collect_tree_file_bytes(&baseline_tree, None, None)?,
            );
        }
        Ok(baselines)
    }

    fn collect_tree_file_bytes_inner(
        &self,
        tree: &Tree,
        prefix: PathBuf,
        source_blobs: Option<&HashMap<ContentHash, &[u8]>>,
        source_trees: Option<&HashMap<ContentHash, &Tree>>,
        files: &mut HashMap<String, Vec<u8>>,
    ) -> Result<()> {
        for entry in tree.entries() {
            let path = prefix.join(entry.name());
            match entry.entry_type() {
                EntryType::Blob => {
                    let Some(path) = path.to_str() else {
                        continue;
                    };
                    let Some(hash) = entry.blob_hash() else {
                        continue;
                    };
                    let bytes = match source_blobs.and_then(|blobs| blobs.get(&hash).copied()) {
                        Some(bytes) => bytes.to_vec(),
                        None => self
                            .store()
                            .get_blob(&hash)?
                            .ok_or_else(|| missing_object("blob", hash))?
                            .content()
                            .to_vec(),
                    };
                    files.insert(path.to_string(), bytes);
                }
                EntryType::Tree => {
                    let Some(hash) = entry.tree_hash() else {
                        continue;
                    };
                    // Worktree captures keep new subtrees in the pending
                    // artifact until the pack commits. Overlay those here
                    // the same way semantic index does — a store miss used
                    // to abort travel and silently inherit the prior blob.
                    if let Some(subtree) = source_trees.and_then(|trees| trees.get(&hash).copied())
                    {
                        self.collect_tree_file_bytes_inner(
                            subtree,
                            path,
                            source_blobs,
                            source_trees,
                            files,
                        )?;
                    } else {
                        let subtree = self
                            .store()
                            .get_tree(&hash)?
                            .ok_or_else(|| missing_object("tree", hash))?;
                        self.collect_tree_file_bytes_inner(
                            &subtree,
                            path,
                            source_blobs,
                            source_trees,
                            files,
                        )?;
                    }
                }
                EntryType::Symlink => {}
                EntryType::Gitlink => {}
                EntryType::Spoollink => {}
            }
        }
        Ok(())
    }
}

fn missing_object(object_type: &str, hash: ContentHash) -> HeddleError {
    HeddleError::MissingObject {
        object_type: object_type.to_string(),
        id: hash.to_hex(),
    }
}

fn missing_state(state_id: StateId) -> HeddleError {
    HeddleError::MissingObject {
        object_type: "state".to_string(),
        id: state_id.to_string_full(),
    }
}

fn group_discussions_by_opened_state(
    discussions: Vec<Discussion>,
) -> HashMap<StateId, Vec<Discussion>> {
    let mut grouped = HashMap::new();
    for discussion in discussions {
        grouped
            .entry(discussion.opened_against_state)
            .or_insert_with(Vec::new)
            .push(discussion);
    }
    grouped
}

#[cfg(test)]
mod tests {
    use std::fs;

    use objects::object::{
        Attribution, Discussion, DiscussionTurn, Principal, StateAttachment, StateId, SymbolAnchor,
        TreeEntry, VisibilityTier,
    };
    use tempfile::TempDir;

    use super::*;

    fn create_test_repo() -> (TempDir, Repository) {
        let temp_dir = TempDir::new().unwrap();
        let repo = Repository::init_default(temp_dir.path()).unwrap();
        (temp_dir, repo)
    }

    fn discussion(id: &str, state: StateId, file: &str, symbol: &str) -> Discussion {
        Discussion {
            id: id.to_string(),
            anchor: SymbolAnchor::new(file, symbol),
            opened_against_state: state,
            opened_at: 1_700_000_000,
            thread_ref: None,
            turns: vec![DiscussionTurn {
                author: Principal::new("Alice", "alice@example.com"),
                body: "please check this".to_string(),
                posted_at: 1_700_000_000,
                references: Vec::new(),
            }],
            resolution: DiscussionResolution::Open,
            body_changed_since_open: false,
            anchor_ambiguous: false,
            orphaned: false,
            visibility: VisibilityTier::default(),
            resolved_annotation_id: None,
        }
    }

    fn attach_discussions_to_main_head(
        repo: &Repository,
        state: &State,
        discussions: Vec<Discussion>,
    ) -> State {
        let bytes = DiscussionsBlob::new(discussions).encode().unwrap();
        let hash = repo.store().put_blob(&Blob::new(bytes)).unwrap();
        repo.put_state_attachment(&StateAttachment {
            state_id: state.id(),
            body: StateAttachmentBody::Discussions(hash),
            attribution: state.attribution.clone(),
            created_at: chrono::Utc::now(),
            supersedes: None,
        })
        .unwrap();
        state.clone()
    }

    fn read_discussions(repo: &Repository, state: &State) -> DiscussionsBlob {
        let hash = repo
            .latest_state_attachment(&state.id(), crate::StateAttachmentKind::Discussions)
            .unwrap()
            .and_then(|attachment| match attachment.body {
                StateAttachmentBody::Discussions(hash) => Some(hash),
                _ => None,
            })
            .expect("snapshot should carry discussions");
        let blob = repo.store().get_blob(&hash).unwrap().unwrap();
        DiscussionsBlob::decode(blob.content()).unwrap()
    }

    #[test]
    fn snapshot_marks_discussion_body_changed_since_open() {
        let (temp, repo) = create_test_repo();
        fs::write(
            temp.path().join("src.rs"),
            "fn foo() {\n    let x = 1;\n}\n",
        )
        .unwrap();
        let first = repo
            .snapshot_with_attribution(
                Some("first".to_string()),
                None,
                Attribution::human(Principal::new("Alice", "alice@example.com")),
            )
            .unwrap();
        attach_discussions_to_main_head(
            &repo,
            &first,
            vec![discussion("d1", first.id(), "src.rs", "foo")],
        );

        fs::write(
            temp.path().join("src.rs"),
            "fn foo() {\n    let x = 2;\n}\n",
        )
        .unwrap();
        let second = repo
            .snapshot_with_attribution(
                Some("second".to_string()),
                None,
                Attribution::human(Principal::new("Alice", "alice@example.com")),
            )
            .unwrap();

        let persisted = read_discussions(&repo, &second);
        assert!(persisted.discussions[0].body_changed_since_open);
        assert!(!persisted.discussions[0].orphaned);
    }

    #[test]
    fn packed_snapshot_preserves_semantic_index_and_discussions() {
        let (temp, repo) = create_test_repo();
        fs::write(
            temp.path().join("src.rs"),
            "fn foo() {\n    let x = 1;\n}\n",
        )
        .unwrap();
        let first = repo
            .snapshot_with_attribution(
                Some("first".to_string()),
                None,
                Attribution::human(Principal::new("Alice", "alice@example.com")),
            )
            .unwrap();
        attach_discussions_to_main_head(
            &repo,
            &first,
            vec![discussion("d1", first.id(), "src.rs", "foo")],
        );

        let blob = Blob::from_slice(b"fn foo() {\n    let x = 2;\n}\n");
        let tree = Tree::from_entries(vec![TreeEntry::file("src.rs", blob.hash(), false).unwrap()]);
        let second = repo
            .snapshot_tree_with_blobs_with_attribution_profiled(
                tree,
                vec![blob],
                Some("packed second".to_string()),
                None,
                Attribution::human(Principal::new("Alice", "alice@example.com")),
            )
            .unwrap()
            .state;

        assert!(
            repo.attached_semantic_index(&second.id())
                .unwrap()
                .is_some(),
            "packed capture must persist its eager semantic-index attachment"
        );
        let discussions = read_discussions(&repo, &second);
        assert_eq!(discussions.discussions.len(), 1);
        assert!(discussions.discussions[0].body_changed_since_open);
    }

    #[test]
    fn snapshot_keeps_body_changed_since_open_after_later_noop_transition() {
        let (temp, repo) = create_test_repo();
        fs::write(
            temp.path().join("src.rs"),
            "fn foo() {\n    let x = 1;\n}\n\nfn bar() {\n    let y = 1;\n}\n",
        )
        .unwrap();
        let first = repo
            .snapshot_with_attribution(
                Some("first".to_string()),
                None,
                Attribution::human(Principal::new("Alice", "alice@example.com")),
            )
            .unwrap();
        attach_discussions_to_main_head(
            &repo,
            &first,
            vec![discussion("d1", first.id(), "src.rs", "foo")],
        );

        fs::write(
            temp.path().join("src.rs"),
            "fn foo() {\n    let x = 2;\n}\n\nfn bar() {\n    let y = 1;\n}\n",
        )
        .unwrap();
        let second = repo
            .snapshot_with_attribution(
                Some("second".to_string()),
                None,
                Attribution::human(Principal::new("Alice", "alice@example.com")),
            )
            .unwrap();

        let persisted_second = read_discussions(&repo, &second);
        assert!(persisted_second.discussions[0].body_changed_since_open);
        assert!(!persisted_second.discussions[0].orphaned);

        fs::write(
            temp.path().join("src.rs"),
            "fn foo() {\n    let x = 2;\n}\n\nfn bar() {\n    let y = 2;\n}\n",
        )
        .unwrap();
        let third = repo
            .snapshot_with_attribution(
                Some("third".to_string()),
                None,
                Attribution::human(Principal::new("Alice", "alice@example.com")),
            )
            .unwrap();

        let persisted_third = read_discussions(&repo, &third);
        assert!(persisted_third.discussions[0].body_changed_since_open);
        assert!(!persisted_third.discussions[0].orphaned);
    }

    #[test]
    fn snapshot_marks_discussion_orphaned_when_anchor_disappears() {
        let (temp, repo) = create_test_repo();
        fs::write(
            temp.path().join("src.rs"),
            "fn foo() {\n    let x = 1;\n}\n",
        )
        .unwrap();
        let first = repo
            .snapshot_with_attribution(
                Some("first".to_string()),
                None,
                Attribution::human(Principal::new("Alice", "alice@example.com")),
            )
            .unwrap();
        attach_discussions_to_main_head(
            &repo,
            &first,
            vec![discussion("d1", first.id(), "src.rs", "foo")],
        );

        fs::write(temp.path().join("src.rs"), "// foo was deleted\n").unwrap();
        let second = repo
            .snapshot_with_attribution(
                Some("second".to_string()),
                None,
                Attribution::human(Principal::new("Alice", "alice@example.com")),
            )
            .unwrap();

        let persisted = read_discussions(&repo, &second);
        assert!(!persisted.discussions[0].body_changed_since_open);
        assert!(persisted.discussions[0].orphaned);
    }

    #[test]
    fn snapshot_persists_in_file_symbol_rename() {
        let (temp, repo) = create_test_repo();
        fs::write(
            temp.path().join("src.rs"),
            "fn foo() {\n    let x = 1;\n}\n",
        )
        .unwrap();
        let first = repo
            .snapshot_with_attribution(
                Some("first".to_string()),
                None,
                Attribution::human(Principal::new("Alice", "alice@example.com")),
            )
            .unwrap();
        attach_discussions_to_main_head(
            &repo,
            &first,
            vec![discussion("d1", first.id(), "src.rs", "foo")],
        );

        fs::write(
            temp.path().join("src.rs"),
            "fn bar() {\n    let x = 1;\n}\n",
        )
        .unwrap();
        let second = repo
            .snapshot_with_attribution(
                Some("second".to_string()),
                None,
                Attribution::human(Principal::new("Alice", "alice@example.com")),
            )
            .unwrap();

        let persisted = read_discussions(&repo, &second);
        assert_eq!(persisted.discussions[0].anchor.symbol, "bar");
        assert!(!persisted.discussions[0].anchor_ambiguous);
        assert!(!persisted.discussions[0].orphaned);
    }

    #[test]
    fn snapshot_persists_ambiguous_symbol_rename_without_picking() {
        let (temp, repo) = create_test_repo();
        fs::write(
            temp.path().join("src.rs"),
            "fn foo() {\n    let x = 1;\n}\n",
        )
        .unwrap();
        let first = repo
            .snapshot_with_attribution(
                Some("first".to_string()),
                None,
                Attribution::human(Principal::new("Alice", "alice@example.com")),
            )
            .unwrap();
        attach_discussions_to_main_head(
            &repo,
            &first,
            vec![discussion("d1", first.id(), "src.rs", "foo")],
        );

        fs::write(
            temp.path().join("src.rs"),
            concat!(
                "fn bar() {\n    let x = 1;\n}\n",
                "fn baz() {\n    let x = 1;\n}\n",
            ),
        )
        .unwrap();
        let second = repo
            .snapshot_with_attribution(
                Some("ambiguous rename".to_string()),
                None,
                Attribution::human(Principal::new("Alice", "alice@example.com")),
            )
            .unwrap();

        let persisted = read_discussions(&repo, &second);
        assert_eq!(persisted.discussions[0].anchor.symbol, "foo");
        assert!(persisted.discussions[0].anchor_ambiguous);
        assert!(!persisted.discussions[0].orphaned);
    }
}
