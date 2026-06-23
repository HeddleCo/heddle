// SPDX-License-Identifier: Apache-2.0

use super::*;

#[test]
fn exact_hash_rename_detected() {
    let store = InMemoryStore::new();
    let content = b"fn main() { println!(\"hello\"); }";
    let base = make_tree(&store, &[("old.rs", content)]);
    let branch = make_tree(&store, &[("new.rs", content)]);
    let renames = detect_renames(
        &store,
        &flatten_tree(&store, &base, "").unwrap(),
        &flatten_tree(&store, &branch, "").unwrap(),
        RenameMatcherConfig::default(),
    )
    .unwrap();

    assert_eq!(renames.len(), 1);
    let r = renames.get("old.rs").unwrap();
    assert_eq!(r.from_path, "old.rs");
    assert_eq!(r.to_path, "new.rs");
    assert_eq!(r.score, 1.0);
}

#[test]
fn modified_and_renamed_detected_without_semantic_scorer() {
    let store = InMemoryStore::new();
    let base_content = b"fn main() {\n    println!(\"hello\");\n}\n";
    let renamed_content = b"fn main() {\n    println!(\"hello world\");\n}\n";
    let base = make_tree(&store, &[("old.rs", base_content)]);
    let branch = make_tree(&store, &[("new.rs", renamed_content)]);

    let base_flat = flatten_tree(&store, &base, "").unwrap();
    let branch_flat = flatten_tree(&store, &branch, "").unwrap();
    let renames = detect_renames(
        &store,
        &base_flat,
        &branch_flat,
        RenameMatcherConfig::default(),
    )
    .unwrap();

    assert_eq!(renames.len(), 1);
    let r = renames.get("old.rs").unwrap();
    assert_eq!(r.to_path, "new.rs");
    assert!(r.score > RenameMatcherConfig::default().threshold);
    assert!(r.score < 1.0);
}

#[test]
fn dissimilar_files_not_matched() {
    let store = InMemoryStore::new();
    let base_content = b"fn main() { println!(\"hello\"); }";
    let added_content = b"completely different content that has nothing in common whatsoever with the original file or its structure or purpose";
    let base = make_tree(&store, &[("old.rs", base_content)]);
    let branch = make_tree(&store, &[("new.rs", added_content)]);

    let base_flat = flatten_tree(&store, &base, "").unwrap();
    let branch_flat = flatten_tree(&store, &branch, "").unwrap();
    let renames = detect_renames(
        &store,
        &base_flat,
        &branch_flat,
        RenameMatcherConfig::default(),
    )
    .unwrap();

    assert!(renames.is_empty());
}

#[test]
fn cross_directory_rename_detected() {
    let store = InMemoryStore::new();
    let content = b"pub fn helper() -> u32 { 42 }";
    let base = make_nested_tree(&store, &[("src/utils/helpers.rs", content)]);
    let branch = make_nested_tree(&store, &[("src/lib/helpers.rs", content)]);
    let base_flat = flatten_tree(&store, &base, "").unwrap();
    let branch_flat = flatten_tree(&store, &branch, "").unwrap();
    let renames = detect_renames(
        &store,
        &base_flat,
        &branch_flat,
        RenameMatcherConfig::default(),
    )
    .unwrap();

    assert_eq!(renames.len(), 1);
    let r = renames.get("src/utils/helpers.rs").unwrap();
    assert_eq!(r.to_path, "src/lib/helpers.rs");
    assert_eq!(r.score, 1.0);
}

#[test]
fn extension_mismatch_can_still_match_with_same_content() {
    let store = InMemoryStore::new();
    let content = b"some generic content that is the same in both files and is long enough to be meaningful for delta comparison purposes here we go";
    let base = make_tree(&store, &[("data.txt", content)]);
    let branch = make_tree(&store, &[("data.md", content)]);
    let base_flat = flatten_tree(&store, &base, "").unwrap();
    let branch_flat = flatten_tree(&store, &branch, "").unwrap();
    let renames = detect_renames(
        &store,
        &base_flat,
        &branch_flat,
        RenameMatcherConfig::default(),
    )
    .unwrap();

    assert_eq!(renames.len(), 1);
    assert_eq!(renames.get("data.txt").unwrap().score, 1.0);
}

#[test]
fn no_renames_when_no_deletions() {
    let store = InMemoryStore::new();
    let content = b"fn main() {}";
    let base = make_tree(&store, &[("existing.rs", content)]);
    let branch = make_tree(
        &store,
        &[("existing.rs", content), ("new.rs", b"fn new() {}")],
    );
    let base_flat = flatten_tree(&store, &base, "").unwrap();
    let branch_flat = flatten_tree(&store, &branch, "").unwrap();
    let renames = detect_renames(
        &store,
        &base_flat,
        &branch_flat,
        RenameMatcherConfig::default(),
    )
    .unwrap();

    assert!(renames.is_empty());
}

#[test]
fn no_renames_when_no_additions() {
    let store = InMemoryStore::new();
    let base = make_tree(&store, &[("a.rs", b"fn a() {}"), ("b.rs", b"fn b() {}")]);
    let branch = make_tree(&store, &[("a.rs", b"fn a() {}")]);
    let base_flat = flatten_tree(&store, &base, "").unwrap();
    let branch_flat = flatten_tree(&store, &branch, "").unwrap();
    let renames = detect_renames(
        &store,
        &base_flat,
        &branch_flat,
        RenameMatcherConfig::default(),
    )
    .unwrap();

    assert!(renames.is_empty());
}

#[test]
fn greedy_matching_picks_best_score() {
    let store = InMemoryStore::new();
    let original = b"fn process_data() {\n    let x = compute();\n    transform(x)\n}\n";
    let good_rename =
        b"fn process_data() {\n    let x = compute();\n    transform(x);\n    log(x)\n}\n";
    let bad_rename = b"totally unrelated content here that shares nothing with the original";
    let base = make_tree(&store, &[("process.rs", original)]);
    let branch = make_tree(
        &store,
        &[("processing.rs", good_rename), ("unrelated.rs", bad_rename)],
    );

    let base_flat = flatten_tree(&store, &base, "").unwrap();
    let branch_flat = flatten_tree(&store, &branch, "").unwrap();
    let renames = detect_renames(
        &store,
        &base_flat,
        &branch_flat,
        RenameMatcherConfig::default(),
    )
    .unwrap();

    assert_eq!(renames.len(), 1);
    assert_eq!(renames.get("process.rs").unwrap().to_path, "processing.rs");
}

#[test]
fn bidirectional_rename_detection() {
    let store = InMemoryStore::new();
    let content_a = b"pub fn widget() -> bool { true }";
    let content_b = b"pub struct Config { debug: bool }";
    let base = make_tree(
        &store,
        &[("widget.rs", content_a), ("config.rs", content_b)],
    );
    let ours = make_nested_tree(
        &store,
        &[
            ("components/widget.rs", content_a),
            ("config.rs", content_b),
        ],
    );
    let theirs = make_nested_tree(
        &store,
        &[("widget.rs", content_a), ("settings/config.rs", content_b)],
    );

    let rename_map = detect_merge_renames(
        &store,
        &base,
        &ours,
        &theirs,
        RenameMatcherConfig::default(),
    )
    .unwrap();

    assert_eq!(rename_map.our_renames.len(), 1);
    assert_eq!(
        rename_map.our_renames.get("widget.rs").unwrap().to_path,
        "components/widget.rs"
    );
    assert_eq!(rename_map.their_renames.len(), 1);
    assert_eq!(
        rename_map.their_renames.get("config.rs").unwrap().to_path,
        "settings/config.rs"
    );
}

#[test]
fn flatten_tree_single_level() {
    let store = InMemoryStore::new();
    let tree = make_tree(&store, &[("a.rs", b"a"), ("b.rs", b"b")]);
    let flat = flatten_tree(&store, &tree, "").unwrap();
    assert_eq!(flat.len(), 2);
    assert!(flat.contains_key("a.rs"));
    assert!(flat.contains_key("b.rs"));
}

#[test]
fn flatten_tree_nested() {
    let store = InMemoryStore::new();
    let tree = make_nested_tree(
        &store,
        &[
            ("src/main.rs", b"main"),
            ("src/lib/utils.rs", b"utils"),
            ("README.md", b"readme"),
        ],
    );
    let flat = flatten_tree(&store, &tree, "").unwrap();
    assert_eq!(flat.len(), 3);
    assert!(flat.contains_key("src/main.rs"));
    assert!(flat.contains_key("src/lib/utils.rs"));
    assert!(flat.contains_key("README.md"));
}
