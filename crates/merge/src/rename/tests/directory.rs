// SPDX-License-Identifier: Apache-2.0

use super::*;

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
