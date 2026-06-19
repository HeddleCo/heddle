// SPDX-License-Identifier: Apache-2.0
//! Snapshot-time persistence for discussion anchor travel.

#![cfg(feature = "tree-sitter-symbols")]

use std::{collections::HashMap, path::PathBuf};

use objects::{
    object::{
        Blob, ChangeId, ContentHash, Discussion, DiscussionResolution, DiscussionsBlob, EntryType,
        State, Tree,
    },
    store::BlockingObjectStore,
};
use oplog::BlockingOpLogBackend;
use refs::RefBackend;

use crate::{HeddleError, Repository, Result, discussion_anchor_travel::travel_anchors};

impl<R, O, S> Repository<R, O, S>
where
    R: RefBackend,
    O: BlockingOpLogBackend,
    S: BlockingObjectStore,
{
    pub(crate) fn compute_and_persist_discussion_anchor_travel(
        &self,
        parent_state: &State,
        new_tree: &Tree,
    ) -> Result<Option<ContentHash>> {
        let Some(parent_discussions_hash) = parent_state.discussions else {
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

        let new_files = self.collect_tree_file_bytes(new_tree)?;
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
                discussion.orphaned = update.orphaned;
            }
        }

        let bytes = discussions
            .encode()
            .map_err(|err| HeddleError::Serialization(format!("encode discussions blob: {err}")))?;
        let hash = self.store().put_blob(&Blob::new(bytes))?;
        Ok(Some(hash))
    }

    fn collect_tree_file_bytes(&self, tree: &Tree) -> Result<HashMap<String, Vec<u8>>> {
        let mut files = HashMap::new();
        self.collect_tree_file_bytes_inner(tree, PathBuf::new(), &mut files)?;
        Ok(files)
    }

    fn collect_discussion_baseline_file_bytes(
        &self,
        discussions: &[Discussion],
    ) -> Result<HashMap<ChangeId, HashMap<String, Vec<u8>>>> {
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
                self.collect_tree_file_bytes(&baseline_tree)?,
            );
        }
        Ok(baselines)
    }

    fn collect_tree_file_bytes_inner(
        &self,
        tree: &Tree,
        prefix: PathBuf,
        files: &mut HashMap<String, Vec<u8>>,
    ) -> Result<()> {
        for entry in tree.entries() {
            let path = prefix.join(&entry.name);
            match entry.entry_type {
                EntryType::Blob => {
                    let Some(path) = path.to_str() else {
                        continue;
                    };
                    let blob = self
                        .store()
                        .get_blob(&entry.hash)?
                        .ok_or_else(|| missing_object("blob", entry.hash))?;
                    files.insert(path.to_string(), blob.content().to_vec());
                }
                EntryType::Tree => {
                    let subtree = self
                        .store()
                        .get_tree(&entry.hash)?
                        .ok_or_else(|| missing_object("tree", entry.hash))?;
                    self.collect_tree_file_bytes_inner(&subtree, path, files)?;
                }
                EntryType::Symlink => {}
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

fn missing_state(change_id: ChangeId) -> HeddleError {
    HeddleError::MissingObject {
        object_type: "state".to_string(),
        id: change_id.to_string_full(),
    }
}

fn group_discussions_by_opened_state(
    discussions: Vec<Discussion>,
) -> HashMap<ChangeId, Vec<Discussion>> {
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
        Attribution, ChangeId, Discussion, DiscussionTurn, Principal, SymbolAnchor, ThreadName,
        VisibilityTier,
    };
    use refs::Head;
    use tempfile::TempDir;

    use super::*;

    fn create_test_repo() -> (TempDir, Repository) {
        let temp_dir = TempDir::new().unwrap();
        let repo = Repository::init_default(temp_dir.path()).unwrap();
        (temp_dir, repo)
    }

    fn discussion(id: &str, state: ChangeId, file: &str, symbol: &str) -> Discussion {
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
            }],
            resolution: DiscussionResolution::Open,
            body_changed_since_open: false,
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
        let mut decorated = state.clone().with_discussions(hash);
        repo.put_authored_state(&mut decorated).unwrap();
        repo.refs()
            .set_thread(&ThreadName::new("main"), &decorated.change_id)
            .unwrap();
        repo.refs()
            .write_head(&Head::Attached {
                thread: ThreadName::new("main"),
            })
            .unwrap();
        decorated
    }

    fn read_discussions(repo: &Repository, state: &State) -> DiscussionsBlob {
        let hash = state
            .discussions
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
            vec![discussion("d1", first.change_id, "src.rs", "foo")],
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
            vec![discussion("d1", first.change_id, "src.rs", "foo")],
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
            vec![discussion("d1", first.change_id, "src.rs", "foo")],
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
        assert!(!persisted.discussions[0].body_changed_since_open);
        assert!(persisted.discussions[0].orphaned);
    }
}
