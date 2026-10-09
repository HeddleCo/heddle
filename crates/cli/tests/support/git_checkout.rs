// SPDX-License-Identifier: Apache-2.0
//! Raw Git trees bypass import so checkout policy is tested independently.

use std::{collections::BTreeMap, path::Path};

use sley::{CommitObject, EntryKind, GitObjectType, ObjectId, Repository as SleyRepository};

pub fn write_commit(
    repo: &SleyRepository,
    parent: Option<ObjectId>,
    files: &[(&str, &[u8])],
) -> ObjectId {
    fn tree(repo: &SleyRepository, files: &[(Vec<&str>, &[u8])]) -> ObjectId {
        let mut editor = sley::TreeEditor::new();
        let mut directories = BTreeMap::<&str, Vec<(Vec<&str>, &[u8])>>::new();
        for (path, content) in files {
            match path.as_slice() {
                [name] => {
                    let blob = repo.write_blob(*content).expect("write fixture blob");
                    editor.upsert(*name, EntryKind::Blob, blob);
                }
                [directory, rest @ ..] => directories
                    .entry(directory)
                    .or_default()
                    .push((rest.to_vec(), content)),
                [] => panic!("empty fixture path"),
            }
        }
        for (name, files) in directories {
            editor.upsert(name, EntryKind::Tree, tree(repo, &files));
        }
        repo.write_tree(editor).expect("write fixture tree")
    }

    let files: Vec<_> = files
        .iter()
        .map(|(path, content)| (path.split('/').collect(), *content))
        .collect();
    let identity = b"Heddle Test <heddle@example.com> 0 +0000".to_vec();
    let commit = CommitObject {
        tree: tree(repo, &files),
        parents: parent.into_iter().collect(),
        author: identity.clone(),
        committer: identity,
        encoding: None,
        message: b"checkout path fixture\n".to_vec(),
    };
    repo.write_raw_object(GitObjectType::Commit, commit.write())
        .expect("write fixture commit")
}

pub struct MetadataSnapshot(Vec<(&'static str, Option<Vec<u8>>)>);

impl MetadataSnapshot {
    pub fn record(root: &Path) -> Self {
        Self(
            [
                ".heddle/config.toml",
                ".git/config",
                ".git/HEAD",
                ".git/index",
                ".git/hooks/post-checkout",
            ]
            .into_iter()
            .map(|path| (path, std::fs::read(root.join(path)).ok()))
            .collect(),
        )
    }

    pub fn assert_unchanged(&self, root: &Path) {
        for (path, before) in &self.0 {
            assert!(
                std::fs::read(root.join(path)).ok().as_ref() == before.as_ref(),
                "checkout changed {path}"
            );
        }
        assert!(!root.join(".GIT").exists(), "checkout wrote a .git alias");
    }
}
