// SPDX-License-Identifier: Apache-2.0
//! Git-layout trees through every encoding (heddle#2018).

use super::{super::tree_canonical::decode_entry_at, *};
use crate::{
    compact,
    object::{ContentHash, TREE_HEADER_LEN, Tree, TreeEntry, tree_delta},
};

fn hash(label: &str) -> ContentHash {
    ContentHash::compute(label.as_bytes())
}

fn mode(digits: &str) -> RawGitMode {
    RawGitMode::parse(digits.as_bytes()).expect("valid mode")
}

fn file(name: &str) -> TreeEntry {
    TreeEntry::file(name, hash(name), false).expect("file")
}

fn dir(name: &str) -> TreeEntry {
    TreeEntry::directory(name, hash(name)).expect("dir")
}

/// `b.txt` before `a.txt`, plus a `100664` file and a zero-padded directory.
fn layout_tree() -> Tree {
    Tree::from_git_entries(vec![
        file("b.txt"),
        file("a.txt").with_raw_git_mode(mode("100664")).unwrap(),
        dir("lib").with_raw_git_mode(mode("040000")).unwrap(),
        file("lib.rs"),
    ])
    .expect("layout tree")
}

fn names(entries: &[&TreeEntry]) -> Vec<String> {
    entries
        .iter()
        .map(|entry| entry.name().to_string())
        .collect()
}

#[test]
fn canonical_git_source_records_nothing_and_keeps_the_native_id() {
    // `lib.rs` before `lib/` is Git's canonical order even though native byte
    // order puts `lib` first, so no source order is recorded.
    let source = vec![file("a.txt"), file("lib.rs"), dir("lib")];
    let tree = Tree::from_git_entries(source.clone()).unwrap();
    assert!(tree.source_positions().is_empty());
    assert!(!tree.has_git_layout());
    assert_eq!(tree, Tree::from_entries(source));
    assert_eq!(
        names(&tree.git_ordered_entries()),
        ["a.txt", "lib.rs", "lib"]
    );
}

#[test]
fn canonical_raw_mode_is_not_recorded() {
    let entry = file("a").with_raw_git_mode(mode("100644")).unwrap();
    assert_eq!(entry.raw_git_mode(), None);
    assert_eq!(entry, file("a"));
    let exec = TreeEntry::file("x", hash("x"), true)
        .unwrap()
        .with_raw_git_mode(mode("100755"))
        .unwrap();
    assert_eq!(exec.raw_git_mode(), None);
}

#[test]
fn raw_mode_must_read_as_the_entry_kind() {
    assert!(file("a").with_raw_git_mode(mode("040000")).is_err());
    assert!(file("a").with_raw_git_mode(mode("100755")).is_err());
    assert!(dir("d").with_raw_git_mode(mode("100664")).is_err());
}

#[test]
fn source_order_and_raw_modes_change_the_id_and_are_reported() {
    let tree = layout_tree();
    assert!(tree.has_git_layout());
    assert!(tree.requires_canonical_body());
    assert_eq!(
        names(&tree.git_ordered_entries()),
        ["b.txt", "a.txt", "lib", "lib.rs"]
    );
    let plain = Tree::from_entries(vec![
        file("a.txt"),
        file("b.txt"),
        dir("lib"),
        file("lib.rs"),
    ]);
    assert_ne!(tree.hash(), plain.hash());
    // Each field alone also changes the id.
    let order_only = Tree::from_git_entries(vec![
        file("b.txt"),
        file("a.txt"),
        dir("lib"),
        file("lib.rs"),
    ])
    .unwrap();
    let mode_only = Tree::from_git_entries(vec![
        file("a.txt").with_raw_git_mode(mode("100664")).unwrap(),
        file("b.txt"),
        file("lib.rs"),
        dir("lib"),
    ])
    .unwrap();
    assert!(order_only.source_positions().len() == 4);
    assert!(mode_only.source_positions().is_empty());
    let ids = [
        tree.hash(),
        plain.hash(),
        order_only.hash(),
        mode_only.hash(),
    ];
    for (left, right) in ids.iter().zip(ids.iter().skip(1)) {
        assert_ne!(left, right);
    }
    assert_ne!(order_only.hash(), mode_only.hash());
    assert_eq!(
        mode_only.get("a.txt").and_then(TreeEntry::git_mode),
        Some(mode("100664"))
    );
    // Checkout meaning stays canonical.
    assert!(!mode_only.get("a.txt").unwrap().is_executable());
}

#[test]
fn duplicate_names_are_rejected_naming_the_entry() {
    let error = Tree::from_git_entries(vec![file("same"), file("other"), file("same")])
        .expect_err("duplicates");
    assert!(error.to_string().contains("'same'"), "{error}");
}

#[test]
fn layout_roundtrips_through_htr4_streamed_and_msgpack() {
    let tree = layout_tree();
    let id = tree.hash();
    let canonical = tree.encode_canonical().unwrap();
    let decoded = Tree::decode_canonical(&canonical).unwrap();
    assert_eq!(decoded, tree);
    assert_eq!(decoded.hash(), id);
    let streamed = Tree::decode_canonical_streamed(&canonical).unwrap();
    assert_eq!(streamed, tree);
    let msgpack = rmp_serde::to_vec_named(&tree).unwrap();
    let decoded: Tree = rmp_serde::from_slice(&msgpack).unwrap();
    assert_eq!(decoded, tree);
    assert_eq!(decoded.hash(), id);
}

#[test]
fn layout_survives_positional_msgpack_and_rejects_unknown_fields() {
    // A positional encoder must not shift `git_mode` into the skipped spool
    // slots: the tree always serializes as a map.
    let tree = layout_tree();
    let positional = rmp_serde::to_vec(&tree).unwrap();
    assert_eq!(rmp_serde::to_vec_named(&tree).unwrap(), positional);
    let decoded: Tree = rmp_serde::from_slice(&positional).unwrap();
    assert_eq!(decoded, tree);

    // A field this binary does not understand is refused, not dropped.
    #[derive(serde::Serialize)]
    struct Future {
        version: u8,
        entries: Vec<u8>,
        future_layout: u8,
    }
    let future = rmp_serde::to_vec_named(&Future {
        version: 3,
        entries: Vec::new(),
        future_layout: 1,
    })
    .unwrap();
    assert!(rmp_serde::from_slice::<Tree>(&future).is_err());
}

#[cfg(feature = "zstd")]
#[test]
fn layout_roundtrips_through_blocked_htr4() {
    let mut entries = (0..40)
        .rev()
        .map(|index| file(&format!("f{index:02}")))
        .collect::<Vec<_>>();
    entries.push(file("odd").with_raw_git_mode(mode("100600")).unwrap());
    let tree = Tree::from_git_entries(entries).unwrap();
    let blocked = tree.encode_canonical_blocked(3, 0).unwrap();
    assert_eq!(Tree::decode_canonical(&blocked).unwrap(), tree);
}

#[test]
fn salted_v4_trees_never_carry_a_layout() {
    // A salted tree is native: building one drops recorded raw modes, so a
    // capture produces the same leaves whichever walker path built it.
    let raw = file("a").with_raw_git_mode(mode("100664")).unwrap();
    let tree =
        Tree::from_entries_salted_v4(vec![raw.clone(), file("b")], vec![[1; 32], [2; 32]]).unwrap();
    let plain =
        Tree::from_entries_salted_v4(vec![file("a"), file("b")], vec![[1; 32], [2; 32]]).unwrap();
    assert_eq!(tree.hash(), plain.hash());
    assert!(!tree.has_git_layout());
    // And a decoded V4 body may not smuggle one in.
    let error =
        Tree::try_from_decoded_entries_salted_v4(vec![raw, file("b")], vec![[1; 32], [2; 32]])
            .expect_err("raw mode on v4");
    assert!(error.to_string().contains("v4 trees"), "{error}");
}

#[test]
fn native_construction_drops_raw_modes() {
    // Every capture walker path builds its directories with `from_entries`;
    // whether an entry was cloned from the imported tree (raw mode) or
    // rebuilt from disk, the native tree is the same.
    let cloned = Tree::from_entries(vec![
        file("a.txt").with_raw_git_mode(mode("100664")).unwrap(),
        file("b.txt"),
    ]);
    let rebuilt = Tree::from_entries(vec![file("a.txt"), file("b.txt")]);
    assert_eq!(cloned, rebuilt);
    assert_eq!(cloned.hash(), rebuilt.hash());
}

#[test]
fn same_meaning_ignores_the_raw_mode_but_not_the_target() {
    let raw = file("a").with_raw_git_mode(mode("100664")).unwrap();
    assert_ne!(raw, file("a"));
    assert!(raw.same_meaning(&file("a")));
    assert!(!raw.same_meaning(&TreeEntry::file("a", hash("a"), true).unwrap()));
    assert!(!raw.same_meaning(&TreeEntry::file("a", hash("other"), false).unwrap()));
}

#[test]
fn salt_less_forms_refuse_a_layout_tree() {
    let tree = layout_tree();
    assert!(tree.encode_lean().is_err());
    assert!(compact::encode_tree_frame(std::slice::from_ref(&tree)).is_err());
    let anchor = Tree::from_entries(vec![file("a.txt")]);
    let ops = tree_delta(&anchor, &tree);
    assert!(
        crate::object::encode_tree_delta(anchor.hash(), &anchor, &tree, &ops).is_err(),
        "HDC1 must not carry a git layout"
    );
}

#[test]
fn mutation_drops_the_whole_layout() {
    let mut tree = layout_tree();
    tree.insert(file("c.txt"));
    assert!(!tree.has_git_layout());
    tree.validate().unwrap();

    let mut removed = layout_tree();
    removed.remove("b.txt");
    assert!(!removed.has_git_layout());
    removed.validate().unwrap();
}

#[test]
fn recorded_canonical_order_is_rejected_on_decode() {
    // Same entries, positions equal to Git's canonical order: a second
    // encoding of an ordinary tree, so it must not decode.
    let entries = vec![file("a"), file("b")];
    let error = Tree::try_from_decoded_layout(entries, vec![0, 1]).expect_err("canonical order");
    assert!(error.to_string().contains("canonical order"), "{error}");
    assert!(Tree::try_from_decoded_layout(vec![file("a"), file("b")], vec![1, 1]).is_err());
    assert!(Tree::try_from_decoded_layout(vec![file("a"), file("b")], vec![1]).is_err());
    assert!(Tree::try_from_decoded_layout(vec![file("a"), file("b")], vec![1, 2]).is_err());
}

#[test]
fn a_frame_recording_a_canonical_raw_mode_is_rejected() {
    let tree =
        Tree::from_git_entries(vec![file("a").with_raw_git_mode(mode("100664")).unwrap()]).unwrap();
    let mut body = tree.encode_canonical().unwrap();
    // The trailer's u32 mode value sits five bytes from the end; rewrite it to
    // the canonical 100644.
    let end = body.len();
    body[end - 5..end - 1].copy_from_slice(&0o100644u32.to_le_bytes());
    let error = Tree::decode_canonical(&body).expect_err("canonical raw mode");
    assert!(error.to_string().contains("canonical"), "{error}");
}

#[test]
fn partial_source_positions_are_rejected_on_decode() {
    let tree = layout_tree();
    let body = tree.encode_canonical().unwrap();
    // Re-encode the first frame without its position flag and trailer.
    let ordinary = Tree::from_entries(tree.entries().to_vec());
    let ordinary_body = ordinary.encode_canonical().unwrap();
    let first_layout = decode_entry_at(&body, TREE_HEADER_LEN, body.len()).unwrap();
    let first_plain =
        decode_entry_at(&ordinary_body, TREE_HEADER_LEN, ordinary_body.len()).unwrap();
    let mut forged = body[..TREE_HEADER_LEN].to_vec();
    forged.extend_from_slice(&ordinary_body[TREE_HEADER_LEN..TREE_HEADER_LEN + first_plain.2]);
    forged.extend_from_slice(&body[TREE_HEADER_LEN + first_layout.2..]);
    let payload_len = (forged.len() - TREE_HEADER_LEN) as u64;
    forged[45..53].copy_from_slice(&payload_len.to_le_bytes());
    let error = Tree::decode_canonical(&forged).expect_err("partial positions");
    assert!(error.to_string().contains("every entry or none"), "{error}");
}
