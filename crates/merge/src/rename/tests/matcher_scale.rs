// SPDX-License-Identifier: Apache-2.0

use std::sync::atomic::{AtomicUsize, Ordering};

use super::*;

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

    assert!(
        start.elapsed().as_secs() < 5,
        "candidate cap should prevent slow scoring"
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
fn semantic_scorer_hook_is_used_for_eligible_pairs() {
    static CALLS: AtomicUsize = AtomicUsize::new(0);

    fn scorer(_from: &str, _to: &str, _from_content: &[u8], _to_content: &[u8]) -> f64 {
        CALLS.fetch_add(1, Ordering::SeqCst);
        0.75
    }

    let store = InMemoryStore::new();
    let base_content =
        b"fn process() {\n    let data = load();\n    transform(data);\n    save(data);\n}\n";
    let renamed_content =
        b"fn process_new() {\n    let data = load();\n    transform(data);\n    log(data);\n}\n";
    let base = make_nested_tree(&store, &[("src/process.rs", base_content)]);
    let branch = make_nested_tree(&store, &[("lib/process_new.rs", renamed_content)]);
    let base_flat = flatten_tree(&store, &base, "").unwrap();
    let branch_flat = flatten_tree(&store, &branch, "").unwrap();
    let detection = detect_renames_with_stats(
        &store,
        &base_flat,
        &branch_flat,
        RenameMatcherConfig::default().with_semantic_scorer(scorer),
    )
    .unwrap();

    assert!(CALLS.load(Ordering::SeqCst) > 0);
    assert!(detection.stats.semantic_scored_pairs > 0);
}
