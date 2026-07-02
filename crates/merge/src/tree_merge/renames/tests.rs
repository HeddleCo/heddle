// SPDX-License-Identifier: Apache-2.0
use objects::{
    object::{Blob, ContentHash, TreeEntry},
    store::InMemoryStore,
};

use super::*;
use crate::tree_merge::rename_matcher::{
    DEFAULT_THRESHOLD, RenameMatcherConfig, delta_similarity, detect_renames,
    detect_renames_with_stats, flatten_tree, infer_directory_renames, path_similarity,
};

/// Helper: create a blob in the store and return its hash.
fn put_blob(store: &InMemoryStore, content: &[u8]) -> ContentHash {
    let blob = Blob::new(content.to_vec());
    store.put_blob(&blob).unwrap()
}

/// Helper: build a flat tree (no subdirs) from name→content pairs.
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

/// Helper: build a nested tree. Paths can contain '/'.
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
fn modified_and_renamed_detected() {
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
    assert!(r.score > DEFAULT_THRESHOLD);
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

    let rename_map =
        detect_merge_renames(&store, &base, &ours, &theirs, DEFAULT_RENAME_THRESHOLD, None)
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
fn path_similarity_same_file() {
    assert!((path_similarity("src/main.rs", "src/main.rs") - 1.0).abs() < 0.01);
}

#[test]
fn path_similarity_same_filename_different_dir() {
    let sim = path_similarity("src/utils/helpers.rs", "src/lib/helpers.rs");
    assert!(sim > 0.4);
    assert!(sim < 0.8);
}

#[test]
fn path_similarity_completely_different() {
    let sim = path_similarity("src/main.rs", "lib/config.py");
    assert!(sim < 0.1);
}

#[test]
fn delta_similarity_identical() {
    let content = b"fn main() { println!(\"hello\"); }";
    let sim = delta_similarity(content, content);
    assert!((sim - 1.0).abs() < 0.01);
}

#[test]
fn delta_similarity_completely_different() {
    let a = b"fn main() { println!(\"hello world\"); }";
    let b = b"XXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXX";
    let sim = delta_similarity(a, b);
    assert!(sim < 0.3);
}

#[test]
fn delta_similarity_slightly_modified() {
    let a = b"fn process() {\n    let data = load();\n    transform(data);\n    save(data);\n}\n";
    let b = b"fn process() {\n    let data = load();\n    transform(data);\n    save(data);\n    log(data);\n}\n";
    let sim = delta_similarity(a, b);
    assert!(sim > 0.5);
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

#[test]
fn size_based_pruning_skips_very_different_sizes() {
    let store = InMemoryStore::new();
    let small = b"x";
    let large = b"this is a very long file content that is way bigger than the small file and should not match it at all because the size difference is too large for a rename to make sense in any reasonable scenario whatsoever and we keep going to be sure";

    let base = make_tree(&store, &[("small.rs", small)]);
    let branch = make_tree(&store, &[("large.rs", large)]);

    let base_flat = flatten_tree(&store, &base, "").unwrap();
    let branch_flat = flatten_tree(&store, &branch, "").unwrap();
    let renames = detect_renames(
        &store,
        &base_flat,
        &branch_flat,
        RenameMatcherConfig::default(),
    )
    .unwrap();

    assert!(
        renames.is_empty(),
        "vastly different sized files should not match"
    );
}

#[test]
fn candidate_cap_prevents_explosion() {
    let store = InMemoryStore::new();
    let mut base_files: Vec<(String, Vec<u8>)> = Vec::new();
    let mut branch_files: Vec<(String, Vec<u8>)> = Vec::new();

    for i in 0..110 {
        base_files.push((
            format!("del_{}.rs", i),
            format!("fn del_{}() {{}}", i).into_bytes(),
        ));
        branch_files.push((
            format!("add_{}.rs", i),
            format!("fn add_{}() {{}}", i).into_bytes(),
        ));
    }

    let base_refs: Vec<(&str, &[u8])> = base_files
        .iter()
        .map(|(n, c)| (n.as_str(), c.as_slice()))
        .collect();
    let branch_refs: Vec<(&str, &[u8])> = branch_files
        .iter()
        .map(|(n, c)| (n.as_str(), c.as_slice()))
        .collect();

    let base = make_tree(&store, &base_refs);
    let branch = make_tree(&store, &branch_refs);

    let base_flat = flatten_tree(&store, &base, "").unwrap();
    let branch_flat = flatten_tree(&store, &branch, "").unwrap();

    let start = std::time::Instant::now();
    let _renames = detect_renames(
        &store,
        &base_flat,
        &branch_flat,
        RenameMatcherConfig::default(),
    )
    .unwrap();
    let elapsed = start.elapsed();

    assert!(
        elapsed.as_secs() < 5,
        "candidate cap should prevent slow scoring; took {:?}",
        elapsed
    );
}

#[test]
fn metadata_pruning_preserves_cross_extension_modified_rename() {
    let store = InMemoryStore::new();
    let base_content = b"fn transform() {\n    let value = 41;\n    println!(\"{value}\");\n}\n";
    let renamed_content = b"fn transform() {\n    let value = 42;\n    println!(\"{value}\");\n}\n";

    let mut base_files: Vec<(String, Vec<u8>)> =
        vec![("src/legacy.txt".to_string(), base_content.to_vec())];
    let mut branch_files: Vec<(String, Vec<u8>)> =
        vec![("notes/renamed.md".to_string(), renamed_content.to_vec())];

    for i in 0..40 {
        base_files.push((
            format!("noise/deleted_{i}.txt"),
            format!("deleted noise {i}\n{}", "x".repeat(48)).into_bytes(),
        ));
        branch_files.push((
            format!("noise/added_{i}.md"),
            format!("added noise {i}\n{}", "y".repeat(48)).into_bytes(),
        ));
    }

    let base_refs: Vec<(&str, &[u8])> = base_files
        .iter()
        .map(|(name, content)| (name.as_str(), content.as_slice()))
        .collect();
    let branch_refs: Vec<(&str, &[u8])> = branch_files
        .iter()
        .map(|(name, content)| (name.as_str(), content.as_slice()))
        .collect();

    let renames = detect_renames(
        &store,
        &flatten_tree(&store, &make_nested_tree(&store, &base_refs), "").unwrap(),
        &flatten_tree(&store, &make_nested_tree(&store, &branch_refs), "").unwrap(),
        RenameMatcherConfig::default(),
    )
    .unwrap();

    assert_eq!(
        renames.get("src/legacy.txt").unwrap().to_path,
        "notes/renamed.md"
    );
}

#[test]
fn rename_matcher_stats_expose_metadata_pruning() {
    let store = InMemoryStore::new();
    let target = b"fn focused() {\n    println!(\"rename me\");\n}\n";

    let mut base_files: Vec<(String, Vec<u8>)> =
        vec![("src/original.rs".to_string(), target.to_vec())];
    let mut branch_files: Vec<(String, Vec<u8>)> =
        vec![("src/renamed.rs".to_string(), target.to_vec())];

    for i in 0..50 {
        base_files.push((
            format!("src/deleted_{i}.rs"),
            format!("fn deleted_{i}() {{ {} }}", "x".repeat(32)).into_bytes(),
        ));
        branch_files.push((
            format!("docs/added_{i}.md"),
            format!("# added_{i}\n{}", "z".repeat(32)).into_bytes(),
        ));
    }

    let base_refs: Vec<(&str, &[u8])> = base_files
        .iter()
        .map(|(name, content)| (name.as_str(), content.as_slice()))
        .collect();
    let branch_refs: Vec<(&str, &[u8])> = branch_files
        .iter()
        .map(|(name, content)| (name.as_str(), content.as_slice()))
        .collect();

    let base_tree = make_nested_tree(&store, &base_refs);
    let branch_tree = make_nested_tree(&store, &branch_refs);
    let detection = detect_renames_with_stats(
        &store,
        &flatten_tree(&store, &base_tree, "").unwrap(),
        &flatten_tree(&store, &branch_tree, "").unwrap(),
        RenameMatcherConfig::default(),
    )
    .unwrap();

    assert!(detection.matches.contains_key("src/original.rs"));
    assert!(detection.stats.metadata_candidate_pairs < detection.stats.total_possible_pairs);
    assert_eq!(detection.stats.matched_pairs, detection.matches.len());
    assert!(detection.stats.exact_hash_matches >= 1);
    assert!(detection.stats.blob_loads > 0);
}

#[test]
fn directory_rename_detected_as_individual_renames() {
    let store = InMemoryStore::new();
    let files_content: &[(&str, &[u8])] = &[
        ("src/old_module/a.rs", b"fn a() {}"),
        ("src/old_module/b.rs", b"fn b() {}"),
        ("src/old_module/c.rs", b"fn c() {}"),
    ];
    let new_files: &[(&str, &[u8])] = &[
        ("src/new_module/a.rs", b"fn a() {}"),
        ("src/new_module/b.rs", b"fn b() {}"),
        ("src/new_module/c.rs", b"fn c() {}"),
    ];

    let base = make_nested_tree(&store, files_content);
    let branch = make_nested_tree(&store, new_files);

    let base_flat = flatten_tree(&store, &base, "").unwrap();
    let branch_flat = flatten_tree(&store, &branch, "").unwrap();
    let renames = detect_renames(
        &store,
        &base_flat,
        &branch_flat,
        RenameMatcherConfig::default(),
    )
    .unwrap();

    assert_eq!(renames.len(), 3);
    assert_eq!(
        renames.get("src/old_module/a.rs").unwrap().to_path,
        "src/new_module/a.rs"
    );
    assert_eq!(
        renames.get("src/old_module/b.rs").unwrap().to_path,
        "src/new_module/b.rs"
    );
    assert_eq!(
        renames.get("src/old_module/c.rs").unwrap().to_path,
        "src/new_module/c.rs"
    );
}

#[test]
fn directory_rename_inferred_from_individual_renames() {
    let store = InMemoryStore::new();
    let files: &[(&str, &[u8])] = &[
        ("src/old_module/a.rs", b"fn a() {}"),
        ("src/old_module/b.rs", b"fn b() {}"),
        ("src/old_module/c.rs", b"fn c() {}"),
    ];
    let new_files: &[(&str, &[u8])] = &[
        ("src/new_module/a.rs", b"fn a() {}"),
        ("src/new_module/b.rs", b"fn b() {}"),
        ("src/new_module/c.rs", b"fn c() {}"),
    ];

    let base = make_nested_tree(&store, files);
    let branch = make_nested_tree(&store, new_files);

    let base_flat = flatten_tree(&store, &base, "").unwrap();
    let branch_flat = flatten_tree(&store, &branch, "").unwrap();
    let renames = detect_renames(
        &store,
        &base_flat,
        &branch_flat,
        RenameMatcherConfig::default(),
    )
    .unwrap();

    let dir_renames = infer_directory_renames(&renames);
    assert_eq!(dir_renames.len(), 1);
    assert_eq!(dir_renames[0].0, "src/old_module");
    assert_eq!(dir_renames[0].1, "src/new_module");
}

#[test]
fn no_directory_rename_when_files_scatter() {
    let store = InMemoryStore::new();
    let base_files: &[(&str, &[u8])] = &[
        ("src/mod/a.rs", b"fn a() {}"),
        ("src/mod/b.rs", b"fn b() {}"),
    ];
    let branch_files: &[(&str, &[u8])] = &[
        ("src/one/a.rs", b"fn a() {}"),
        ("src/two/b.rs", b"fn b() {}"),
    ];

    let base = make_nested_tree(&store, base_files);
    let branch = make_nested_tree(&store, branch_files);

    let base_flat = flatten_tree(&store, &base, "").unwrap();
    let branch_flat = flatten_tree(&store, &branch, "").unwrap();
    let renames = detect_renames(
        &store,
        &base_flat,
        &branch_flat,
        RenameMatcherConfig::default(),
    )
    .unwrap();

    let dir_renames = infer_directory_renames(&renames);
    assert!(dir_renames.is_empty());
}
