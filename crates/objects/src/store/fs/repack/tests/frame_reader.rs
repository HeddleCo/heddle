// SPDX-License-Identifier: Apache-2.0

use std::io::Cursor;

use heddle_format::compression::CompressionConfig;
use heddle_object_model::compact::{encode_state_frame, encode_tree_frame, extract_state};
use tempfile::TempDir;

use super::create_store;
use crate::{
    object::{Attribution, ContentHash, Principal, State, Tree, TreeEntry},
    store::{
        ObjectStore,
        pack::{ObjectType, PackObjectId, StreamingPackBuilder},
    },
};

fn sample_trees() -> Vec<Tree> {
    let blob = ContentHash::compute_typed("blob", b"frame-reader-blob");
    vec![
        Tree::from_entries(vec![
            TreeEntry::file("readme.md", blob, false).unwrap(),
            TreeEntry::file("build.sh", blob, true).unwrap(),
        ]),
        Tree::from_entries(vec![TreeEntry::directory("src", blob).unwrap()]),
        Tree::from_entries(vec![TreeEntry::symlink("link", blob).unwrap()]),
    ]
}

fn sample_states(trees: &[Tree]) -> Vec<State> {
    let author = Attribution::human(Principal::new("Reader", "reader@example.com"));
    let first = State::new(trees[0].hash(), Vec::new(), author.clone()).with_intent("first");
    let second =
        State::new(trees[1].hash(), vec![first.id()], author.clone()).with_intent("second");
    let third = State::new(trees[2].hash(), vec![second.id()], author).with_intent("third");
    vec![first, second, third]
}

fn write_compact_pack(trees: &[Tree], states: &[State]) -> (Vec<u8>, Vec<u8>) {
    let tmp = TempDir::new().unwrap();
    let index_path = tmp.path().join("compact.idx");
    let mut builder = StreamingPackBuilder::new(
        Cursor::new(Vec::new()),
        index_path.clone(),
        CompressionConfig::disabled(),
        tmp.path().join("buckets"),
    )
    .unwrap();

    let tree_ids = trees
        .iter()
        .map(|tree| PackObjectId::Hash(tree.hash()))
        .collect::<Vec<_>>();
    let tree_frame = encode_tree_frame(trees).unwrap();
    builder
        .add_shared_frame(&tree_ids, ObjectType::Tree, tree_frame.len(), &tree_frame)
        .unwrap();

    let state_ids = states
        .iter()
        .map(|state| PackObjectId::StateId(state.id()))
        .collect::<Vec<_>>();
    let state_frame = encode_state_frame(states).unwrap();
    builder
        .add_shared_frame(
            &state_ids,
            ObjectType::State,
            state_frame.len(),
            &state_frame,
        )
        .unwrap();

    let (cursor, _) = builder.finalize().unwrap();
    (cursor.into_inner(), std::fs::read(index_path).unwrap())
}

#[test]
fn compact_reader_matches_pre_frame_canonical_bytes() {
    let trees = sample_trees();
    let states = sample_states(&trees);
    let old_trees = trees
        .iter()
        .map(|tree| rmp_serde::to_vec_named(tree).unwrap())
        .collect::<Vec<_>>();
    let old_states = states
        .iter()
        .map(|state| rmp_serde::to_vec_named(state).unwrap())
        .collect::<Vec<_>>();

    let (pack, index) = write_compact_pack(&trees, &states);
    let (_temp, store) = create_store();
    store.install_pack(&pack, &index).unwrap();
    store.clear_recent_object_caches();

    for (tree, expected) in trees.iter().zip(&old_trees) {
        let loaded = store.get_tree(&tree.hash()).unwrap().unwrap();
        assert_eq!(loaded.hash(), tree.hash());
        assert_eq!(rmp_serde::to_vec_named(&loaded).unwrap(), *expected);
    }
    for (state, expected) in states.iter().zip(&old_states) {
        let loaded = store.get_state(&state.id()).unwrap().unwrap();
        assert_eq!(loaded.id(), state.id());
        assert_eq!(rmp_serde::to_vec_named(&loaded).unwrap(), *expected);
    }
}

#[test]
fn compact_reader_rejects_every_object_after_one_corrupt_frame_byte() {
    let trees = sample_trees();
    let states = sample_states(&trees);
    let mut tree_frame = encode_tree_frame(&trees).unwrap();
    let tree_corrupt_at = tree_frame.len() / 2;
    tree_frame[tree_corrupt_at] ^= 0x01;
    let mut state_frame = encode_state_frame(&states).unwrap();
    let state_corrupt_at = state_frame.len() / 2;
    state_frame[state_corrupt_at] ^= 0x01;

    let tmp = TempDir::new().unwrap();
    let index_path = tmp.path().join("corrupt.idx");
    let mut builder = StreamingPackBuilder::new(
        Cursor::new(Vec::new()),
        index_path.clone(),
        CompressionConfig::disabled(),
        tmp.path().join("buckets"),
    )
    .unwrap();
    let tree_ids = trees
        .iter()
        .map(|tree| PackObjectId::Hash(tree.hash()))
        .collect::<Vec<_>>();
    builder
        .add_shared_frame(&tree_ids, ObjectType::Tree, tree_frame.len(), &tree_frame)
        .unwrap();
    let (cursor, _) = builder.finalize().unwrap();

    let (_temp, store) = create_store();
    let error = store
        .install_pack(&cursor.into_inner(), &std::fs::read(index_path).unwrap())
        .unwrap_err();
    assert!(
        error.to_string().contains("checksum mismatch"),
        "a corrupt compact pack must not install, got {error}"
    );
    for tree in &trees {
        assert!(
            store.get_tree(&tree.hash()).unwrap().is_none(),
            "rejected pack must not publish tree {}",
            tree.hash()
        );
    }
    for state in &states {
        let error = extract_state(&state_frame, state.id()).unwrap_err();
        assert!(
            error.to_string().contains("checksum mismatch"),
            "every contained state must fail extraction, got {error}"
        );
    }
}
