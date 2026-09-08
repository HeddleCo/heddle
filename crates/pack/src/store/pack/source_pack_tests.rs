// SPDX-License-Identifier: Apache-2.0
use heddle_format::compression::CompressionConfig;

use super::*;
use crate::object::{Attribution, Blob, ContentHash, Principal, State, StateId, Tree, TreeEntry};

fn fixture() -> (State, Vec<(PackObjectId, ObjectType, Vec<u8>)>) {
    let blob = Blob::new(b"selected source".to_vec());
    let tree = Tree::from_entries(vec![
        TreeEntry::file("file.rs", blob.hash(), false).expect("file"),
        TreeEntry::symlink("link", blob.hash()).expect("symlink"),
        TreeEntry::spoollink(
            "private-child",
            crate::object::SpoolId::parse("owner/private-child").expect("spool ID"),
            StateId::from_bytes([4; 32]),
        )
        .expect("spoollink"),
    ]);
    let mut state = State::new_snapshot(
        tree.hash(),
        vec![StateId::from_bytes([3; 32])],
        Attribution::human(Principal::new("owner", "owner@example.test")),
    );
    // A source upload does not implicitly opt into provenance or history.
    state.provenance = Some(ContentHash::compute(b"private provenance"));
    let entries = vec![
        (
            PackObjectId::StateId(state.id()),
            ObjectType::State,
            state.encode_current_msgpack().expect("State"),
        ),
        (
            PackObjectId::Hash(tree.hash()),
            ObjectType::Tree,
            tree.encode_canonical().expect("tree"),
        ),
        (
            PackObjectId::Hash(blob.hash()),
            ObjectType::Blob,
            blob.into_content(),
        ),
    ];
    (state, entries)
}
fn reader(entries: Vec<(PackObjectId, ObjectType, Vec<u8>)>) -> PackReader<'static> {
    let mut builder = PackBuilder::for_repack(CompressionConfig::default(), 0);
    for (id, kind, data) in entries {
        builder.add_id(id, kind, data);
    }
    let (pack, index, _) = builder.build().expect("pack");
    PackReader::from_bytes(pack, index).expect("reader")
}
#[test]
fn selected_source_closure_is_complete_without_private_history_or_child_spools() {
    let (state, entries) = fixture();
    let verified = reader(entries)
        .validate_source_closure(&state, 16, 65536)
        .expect("complete selected source");
    assert_eq!(
        verified.len(),
        3,
        "symlink reuse is deduplicated, external spools and provenance are not traversed"
    );
}
#[test]
fn publication_rejects_missing_source_and_unselected_objects() {
    let (state, entries) = fixture();
    let mut missing = entries.clone();
    missing.pop();
    assert!(
        reader(missing)
            .validate_source_closure(&state, 16, 65536)
            .is_err(),
        "missing content must not produce an availability receipt"
    );
    let mut extra = entries.clone();
    let secret = Blob::new(b"private transcript".to_vec());
    extra.push((
        PackObjectId::Hash(secret.hash()),
        ObjectType::Blob,
        secret.into_content(),
    ));
    assert!(
        reader(extra)
            .validate_source_closure(&state, 16, 65536)
            .is_err(),
        "unselected content cannot persist inside a source pack"
    );
    let mut false_hash = entries.clone();
    false_hash[2].2 = b"changed content".to_vec();
    assert!(
        reader(false_hash)
            .validate_source_closure(&state, 16, 65536)
            .is_err(),
        "pack checksum alone does not verify logical object identity"
    );
    assert!(
        reader(entries.clone())
            .validate_source_closure(&state, 2, 65536)
            .is_err(),
        "object budget"
    );
    assert!(
        reader(entries)
            .validate_source_closure(&state, 16, 1)
            .is_err(),
        "decoded byte budget"
    );
}
#[test]
fn publication_rejects_unindexed_bytes_even_when_selected_objects_are_complete() {
    let (state, entries) = fixture();
    let mut builder = PackBuilder::for_repack(CompressionConfig::default(), 0);
    for (id, kind, data) in entries {
        builder.add_id(id, kind, data);
    }
    let secret = Blob::new(b"private transcript".to_vec());
    let secret_id = PackObjectId::Hash(secret.hash());
    builder.add_id(secret_id, ObjectType::Blob, secret.into_content());
    let (pack, index, _) = builder.build().expect("pack");
    let original = PackIndex::from_bytes(&index).expect("index");
    let mut partial = PackIndex::new();
    for entry in original.entries().expect("entries") {
        if entry.id != secret_id {
            partial.add(entry.id, entry.offset);
        }
    }
    partial.sort();
    let reader =
        PackReader::from_bytes(pack, partial.to_bytes()).expect("well-formed partial index");
    assert!(
        reader.validate_source_closure(&state, 16, 65536).is_err(),
        "an unindexed record must not cross the selected disclosure boundary"
    );
}
