// SPDX-License-Identifier: Apache-2.0

use std::collections::HashMap;

use objects::{
    object::{Blob, ContentHash, Tree, TreeEntry},
    store::{InMemoryStore, ObjectStore},
};

use super::{
    RenameMatcherConfig, detect_merge_renames, detect_renames, detect_renames_with_stats,
    flatten_tree, infer_directory_renames,
    scoring::{delta_similarity, path_similarity},
};

mod directory;
mod matcher;
mod matcher_scale;
mod scoring;

fn put_blob(store: &InMemoryStore, content: &[u8]) -> ContentHash {
    let blob = Blob::new(content.to_vec());
    store.put_blob(&blob).unwrap()
}

fn make_tree(store: &InMemoryStore, files: &[(&str, &[u8])]) -> Tree {
    let entries: Vec<TreeEntry> = files
        .iter()
        .map(|(name, content)| {
            let hash = put_blob(store, content);
            TreeEntry::file(name.to_string(), hash, false).unwrap()
        })
        .collect();
    Tree::from_entries(entries)
}

fn make_nested_tree(store: &InMemoryStore, files: &[(&str, &[u8])]) -> Tree {
    let mut top_files: Vec<(&str, &[u8])> = Vec::new();
    let mut subdirs: HashMap<&str, Vec<(&str, &[u8])>> = HashMap::new();

    for (path, content) in files {
        if let Some((dir, rest)) = path.split_once('/') {
            subdirs.entry(dir).or_default().push((rest, content));
        } else {
            top_files.push((path, content));
        }
    }

    let mut entries: Vec<TreeEntry> = top_files
        .iter()
        .map(|(name, content)| {
            let hash = put_blob(store, content);
            TreeEntry::file(name.to_string(), hash, false).unwrap()
        })
        .collect();

    for (dir_name, sub_files) in subdirs {
        let subtree = make_nested_tree(store, &sub_files);
        let hash = store.put_tree(&subtree).unwrap();
        entries.push(TreeEntry::directory(dir_name.to_string(), hash).unwrap());
    }

    Tree::from_entries(entries)
}
