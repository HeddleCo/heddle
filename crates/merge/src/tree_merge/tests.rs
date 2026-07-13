// SPDX-License-Identifier: Apache-2.0

use std::collections::BTreeMap;

use objects::{
    object::{Blob, ContentHash, FileMode, Tree, TreeEntry},
    store::{InMemoryStore, ObjectStore},
};

use super::{MergeOptions, TreeMergeResult, merge_trees};

fn fixture_tree(store: &InMemoryStore, files: &[(&str, &[u8], bool)]) -> Tree {
    let files = files
        .iter()
        .map(|(path, content, executable)| {
            let hash = store.put_blob(&Blob::from_slice(content)).unwrap();
            ((*path).to_string(), hash, *executable)
        })
        .collect::<Vec<_>>();
    build_tree(store, &files)
}

fn build_tree(store: &InMemoryStore, files: &[(String, ContentHash, bool)]) -> Tree {
    let mut entries = Vec::new();
    let mut directories: BTreeMap<String, Vec<(String, ContentHash, bool)>> = BTreeMap::new();

    for (path, hash, executable) in files {
        if let Some((directory, rest)) = path.split_once('/') {
            directories.entry(directory.to_string()).or_default().push((
                rest.to_string(),
                *hash,
                *executable,
            ));
        } else {
            entries.push(TreeEntry::file(path, *hash, *executable).unwrap());
        }
    }

    for (name, descendants) in directories {
        let subtree = build_tree(store, &descendants);
        let hash = store.put_tree(&subtree).unwrap();
        entries.push(TreeEntry::directory(name, hash).unwrap());
    }

    Tree::from_entries(entries)
}

fn merge(store: &InMemoryStore, base: &Tree, ours: &Tree, theirs: &Tree) -> TreeMergeResult {
    merge_trees(store, &store, base, ours, theirs, MergeOptions::default()).unwrap()
}

fn entry_at(store: &InMemoryStore, tree: &Tree, path: &str) -> TreeEntry {
    let mut current = tree.clone();
    let mut components = path.split('/').peekable();
    loop {
        let component = components.next().unwrap();
        let entry = current
            .get(component)
            .unwrap_or_else(|| panic!("missing merged path {path:?}"))
            .clone();
        if components.peek().is_none() {
            return entry;
        }
        current = store
            .get_tree(&entry.tree_hash().expect("intermediate path must be a tree"))
            .unwrap()
            .expect("merged subtree must be stored");
    }
}

fn content_at(store: &InMemoryStore, tree: &Tree, path: &str) -> Vec<u8> {
    let entry = entry_at(store, tree, path);
    store
        .get_blob(&entry.blob_hash().expect("leaf path must be a blob"))
        .unwrap()
        .expect("merged blob must be stored")
        .into_content()
}

fn assert_missing(store: &InMemoryStore, tree: &Tree, path: &str) {
    let mut current = tree.clone();
    let mut components = path.split('/').peekable();
    while let Some(component) = components.next() {
        let Some(entry) = current.get(component) else {
            return;
        };
        if components.peek().is_none() {
            panic!("unexpected merged path {path:?}");
        }
        let Some(hash) = entry.tree_hash() else {
            return;
        };
        current = store
            .get_tree(&hash)
            .unwrap()
            .expect("merged subtree must be stored");
    }
}

#[test]
fn combines_executable_mode_and_content_changes() {
    let store = InMemoryStore::new();
    let base = fixture_tree(&store, &[("tool.sh", b"echo base\n", false)]);
    let ours = fixture_tree(&store, &[("tool.sh", b"echo base\n", true)]);
    let theirs = fixture_tree(&store, &[("tool.sh", b"echo changed\n", false)]);

    let result = merge(&store, &base, &ours, &theirs);

    assert!(result.conflicts.is_empty());
    assert_eq!(
        content_at(&store, &result.tree, "tool.sh"),
        b"echo changed\n"
    );
    assert_eq!(
        entry_at(&store, &result.tree, "tool.sh").mode(),
        FileMode::Executable
    );
}

#[test]
fn preserves_agreed_executable_mode_when_only_one_side_changes_content() {
    let store = InMemoryStore::new();
    let base = fixture_tree(&store, &[("tool.sh", b"echo base\n", false)]);
    let ours = fixture_tree(&store, &[("tool.sh", b"echo base\n", true)]);
    let theirs = fixture_tree(&store, &[("tool.sh", b"echo main\n", true)]);

    let result = merge(&store, &base, &ours, &theirs);

    assert!(result.conflicts.is_empty());
    assert_eq!(content_at(&store, &result.tree, "tool.sh"), b"echo main\n");
    assert_eq!(
        entry_at(&store, &result.tree, "tool.sh").mode(),
        FileMode::Executable
    );
}

#[test]
fn delete_wins_against_unchanged_file() {
    let store = InMemoryStore::new();
    let base = fixture_tree(&store, &[("file.txt", b"base", false)]);
    let ours = Tree::new();

    let result = merge(&store, &base, &ours, &base);

    assert!(result.conflicts.is_empty());
    assert_missing(&store, &result.tree, "file.txt");
}

#[test]
fn delete_modify_materializes_conflict_markers() {
    let store = InMemoryStore::new();
    let base = fixture_tree(&store, &[("file.txt", b"base", false)]);
    let ours = Tree::new();
    let theirs = fixture_tree(&store, &[("file.txt", b"changed", false)]);

    let result = merge(&store, &base, &ours, &theirs);
    let content = content_at(&store, &result.tree, "file.txt");

    assert_eq!(result.conflicts, ["file.txt"]);
    assert!(content.starts_with(b"<<<<<<< CURRENT\nchanged\n"));
    assert!(content.ends_with(b">>>>>>> INCOMING\n"));
}

#[test]
fn binary_delete_modify_materializes_conflict_markers() {
    let store = InMemoryStore::new();
    let base = fixture_tree(&store, &[("asset.bin", b"\0base\xff", false)]);
    let ours = Tree::new();
    let theirs = fixture_tree(&store, &[("asset.bin", b"\0changed\xff", false)]);

    let result = merge(&store, &base, &ours, &theirs);
    let content = content_at(&store, &result.tree, "asset.bin");

    assert_eq!(result.conflicts, ["asset.bin"]);
    assert!(content.windows(7).any(|window| window == b"<<<<<<<"));
    assert!(content.windows(7).any(|window| window == b">>>>>>>"));
}

#[test]
fn rename_delete_keeps_renamed_file_for_resolution() {
    let store = InMemoryStore::new();
    let base = fixture_tree(&store, &[("old.txt", b"shared content\n", false)]);
    let ours = fixture_tree(&store, &[("new.txt", b"shared content\n", false)]);
    let theirs = Tree::new();

    let result = merge(&store, &base, &ours, &theirs);

    assert_eq!(
        content_at(&store, &result.tree, "new.txt"),
        b"shared content\n"
    );
    assert_missing(&store, &result.tree, "old.txt");
    assert!(result.conflicts.iter().any(|conflict| {
        conflict.contains("rename/delete")
            && conflict.contains("old.txt")
            && conflict.contains("new.txt")
    }));
}

#[test]
fn directory_file_conflict_becomes_resolvable_file() {
    let store = InMemoryStore::new();
    let base = Tree::new();
    let ours = fixture_tree(&store, &[("node", b"plain file", false)]);
    let theirs = fixture_tree(&store, &[("node/child.txt", b"child", false)]);

    let result = merge(&store, &base, &ours, &theirs);
    let content = content_at(&store, &result.tree, "node");

    assert_eq!(result.conflicts, ["node"]);
    assert!(content.starts_with(b"<<<<<<< CURRENT\nplain file\n"));
    assert!(content.windows(11).any(|window| window == b"<directory>"));
    assert!(content.windows(9).any(|window| window == b"child.txt"));
}

#[test]
fn file_directory_conflict_becomes_resolvable_file() {
    let store = InMemoryStore::new();
    let base = Tree::new();
    let ours = fixture_tree(&store, &[("node/child.txt", b"child", false)]);
    let theirs = fixture_tree(&store, &[("node", b"plain file", false)]);

    let result = merge(&store, &base, &ours, &theirs);
    let content = content_at(&store, &result.tree, "node");

    assert_eq!(result.conflicts, ["node"]);
    assert!(content.windows(11).any(|window| window == b"<directory>"));
    assert!(content.ends_with(b"plain file\n>>>>>>> INCOMING\n"));
}

#[test]
fn preserves_files_across_disjoint_subdirectories() {
    let store = InMemoryStore::new();
    let base = fixture_tree(&store, &[("src/lib.rs", b"base", false)]);
    let ours = fixture_tree(
        &store,
        &[
            ("src/lib.rs", b"base", false),
            ("src/util/helpers.rs", b"helpers", false),
        ],
    );
    let theirs = fixture_tree(
        &store,
        &[
            ("src/lib.rs", b"base", false),
            ("tests/test.rs", b"test", false),
        ],
    );

    let result = merge(&store, &base, &ours, &theirs);

    assert!(result.conflicts.is_empty());
    assert_eq!(content_at(&store, &result.tree, "src/lib.rs"), b"base");
    assert_eq!(
        content_at(&store, &result.tree, "src/util/helpers.rs"),
        b"helpers"
    );
    assert_eq!(content_at(&store, &result.tree, "tests/test.rs"), b"test");
}

#[test]
fn carries_modifications_through_directory_restructure() {
    let store = InMemoryStore::new();
    let main_base = b"fn main() {\n    run();\n}\n";
    let lib_base = b"pub mod store;\n";
    let base = fixture_tree(
        &store,
        &[
            ("src/main.rs", main_base, false),
            ("src/lib.rs", lib_base, false),
        ],
    );
    let ours = fixture_tree(
        &store,
        &[
            ("crates/core/src/main.rs", main_base, false),
            ("crates/core/src/lib.rs", lib_base, false),
        ],
    );
    let theirs = fixture_tree(
        &store,
        &[
            (
                "src/main.rs",
                b"fn main() {\n    run();\n    report();\n}\n",
                false,
            ),
            ("src/lib.rs", b"pub mod store;\npub mod delta;\n", false),
        ],
    );

    let result = merge(&store, &base, &ours, &theirs);

    assert!(result.conflicts.is_empty(), "{:?}", result.conflicts);
    assert!(
        content_at(&store, &result.tree, "crates/core/src/main.rs")
            .windows(6)
            .any(|w| w == b"report")
    );
    assert!(
        content_at(&store, &result.tree, "crates/core/src/lib.rs")
            .windows(5)
            .any(|w| w == b"delta")
    );
    assert_missing(&store, &result.tree, "src/main.rs");
    assert_missing(&store, &result.tree, "src/lib.rs");
}

#[test]
fn preserves_deeply_nested_additions_from_both_sides() {
    let store = InMemoryStore::new();
    let base = fixture_tree(&store, &[("README.md", b"project", false)]);
    let ours = fixture_tree(
        &store,
        &[
            ("README.md", b"project", false),
            ("src/store/pack/builder.rs", b"builder", false),
        ],
    );
    let theirs = fixture_tree(
        &store,
        &[
            ("README.md", b"project", false),
            ("src/protocol/delta/encoder.rs", b"encoder", false),
        ],
    );

    let result = merge(&store, &base, &ours, &theirs);

    assert!(result.conflicts.is_empty());
    assert_eq!(content_at(&store, &result.tree, "README.md"), b"project");
    assert_eq!(
        content_at(&store, &result.tree, "src/store/pack/builder.rs"),
        b"builder"
    );
    assert_eq!(
        content_at(&store, &result.tree, "src/protocol/delta/encoder.rs"),
        b"encoder"
    );
}

#[test]
fn rename_on_one_side_receives_other_sides_modification() {
    let store = InMemoryStore::new();
    let base_content = b"fn main() {\n    println!(\"hello\");\n}\n";
    let base = fixture_tree(&store, &[("foo.rs", base_content, false)]);
    let ours = fixture_tree(&store, &[("bar.rs", base_content, false)]);
    let theirs = fixture_tree(
        &store,
        &[(
            "foo.rs",
            b"fn main() {\n    println!(\"hello world\");\n}\n",
            false,
        )],
    );

    let result = merge(&store, &base, &ours, &theirs);

    assert!(result.conflicts.is_empty());
    assert!(
        content_at(&store, &result.tree, "bar.rs")
            .windows(11)
            .any(|w| w == b"hello world")
    );
    assert_missing(&store, &result.tree, "foo.rs");
}

#[test]
fn divergent_renames_preserve_both_destinations_and_conflict() {
    let store = InMemoryStore::new();
    let content = b"pub fn shared() {}\n";
    let base = fixture_tree(&store, &[("shared.rs", content, false)]);
    let ours = fixture_tree(&store, &[("alpha.rs", content, false)]);
    let theirs = fixture_tree(&store, &[("beta.rs", content, false)]);

    let result = merge(&store, &base, &ours, &theirs);

    assert_eq!(content_at(&store, &result.tree, "alpha.rs"), content);
    assert_eq!(content_at(&store, &result.tree, "beta.rs"), content);
    assert!(
        result
            .conflicts
            .iter()
            .any(|conflict| conflict.contains("rename/rename"))
    );
    assert_missing(&store, &result.tree, "shared.rs");
}

#[test]
fn same_side_rename_and_modify_keeps_new_path_and_content() {
    let store = InMemoryStore::new();
    let base = fixture_tree(
        &store,
        &[("foo.rs", b"fn process() {\n    step_one();\n}\n", false)],
    );
    let ours = fixture_tree(
        &store,
        &[(
            "bar.rs",
            b"fn process() {\n    step_one();\n    step_two();\n}\n",
            false,
        )],
    );

    let result = merge(&store, &base, &ours, &base);

    assert!(result.conflicts.is_empty());
    assert!(
        content_at(&store, &result.tree, "bar.rs")
            .windows(8)
            .any(|w| w == b"step_two")
    );
    assert_missing(&store, &result.tree, "foo.rs");
}

#[test]
fn cross_directory_rename_receives_other_sides_modification() {
    let store = InMemoryStore::new();
    let base_content = b"pub fn helper() -> u32 { 42 }\n";
    let base = fixture_tree(&store, &[("src/utils.rs", base_content, false)]);
    let ours = fixture_tree(&store, &[("src/lib/utils.rs", base_content, false)]);
    let theirs = fixture_tree(
        &store,
        &[(
            "src/utils.rs",
            b"pub fn helper() -> u32 { 99 }\npub fn new_fn() {}\n",
            false,
        )],
    );

    let result = merge(&store, &base, &ours, &theirs);

    assert!(result.conflicts.is_empty());
    assert!(
        content_at(&store, &result.tree, "src/lib/utils.rs")
            .windows(6)
            .any(|w| w == b"new_fn")
    );
    assert_missing(&store, &result.tree, "src/utils.rs");
}

#[test]
fn pure_rename_coexists_with_unrelated_addition() {
    let store = InMemoryStore::new();
    let base = fixture_tree(&store, &[("foo.rs", b"fn original() {}\n", false)]);
    let ours = fixture_tree(&store, &[("bar.rs", b"fn original() {}\n", false)]);
    let theirs = fixture_tree(
        &store,
        &[
            ("foo.rs", b"fn original() {}\n", false),
            ("baz.rs", b"fn new_stuff() {}\n", false),
        ],
    );

    let result = merge(&store, &base, &ours, &theirs);

    assert!(result.conflicts.is_empty());
    assert_eq!(
        content_at(&store, &result.tree, "bar.rs"),
        b"fn original() {}\n"
    );
    assert_eq!(
        content_at(&store, &result.tree, "baz.rs"),
        b"fn new_stuff() {}\n"
    );
    assert_missing(&store, &result.tree, "foo.rs");
}
