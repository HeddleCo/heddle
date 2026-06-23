// SPDX-License-Identifier: Apache-2.0

use ::merge::rename::{detect_renames_with_stats, flatten_tree};
use objects::{
    object::{Blob, ContentHash, Tree, TreeEntry},
    store::{InMemoryStore, ObjectStore},
};

use super::rename_matcher_config;

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

#[test]
fn merge_renames_semantic_adapter_invokes_semantic_scorer() {
    let store = InMemoryStore::new();
    let base_content = br#"
pub fn process_items(items: &[i32]) -> i32 {
    let mut total = 0;
    for item in items {
        total += *item;
    }
    total
}
"#;
    let renamed_content = br#"
pub fn summarize_items(items: &[i32]) -> i32 {
    let mut sum = 0;
    for value in items {
        sum += *value;
    }
    sum
}
"#;

    let base = make_tree(&store, &[("process.rs", base_content)]);
    let branch = make_tree(&store, &[("summary.rs", renamed_content)]);
    let detection = detect_renames_with_stats(
        &store,
        &flatten_tree(&store, &base, "").unwrap(),
        &flatten_tree(&store, &branch, "").unwrap(),
        rename_matcher_config(),
    )
    .unwrap();

    assert!(detection.stats.semantic_scored_pairs > 0);
    assert_eq!(
        detection.matches.get("process.rs").unwrap().to_path,
        "summary.rs"
    );
}
