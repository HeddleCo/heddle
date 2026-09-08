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
    let (mut pack, index, _) = builder.build().expect("pack");
    let secret = Blob::new(b"private transcript".to_vec());
    pack.truncate(pack.len() - PACK_CHECKSUM_LEN);
    encode_tagged_entry_parts(
        &mut pack,
        PackObjectId::Hash(secret.hash()),
        ObjectType::Blob,
        secret.size(),
        None,
        secret.content(),
    )
    .expect("unindexed record");
    append_container_checksum(&mut pack);
    let reader =
        PackReader::from_bytes(pack, index).expect("well-formed container with trailing record");
    assert!(
        reader.validate_source_closure(&state, 16, 65536).is_err(),
        "an unindexed record must not cross the selected disclosure boundary"
    );
}

struct SelectedSource {
    entries: Vec<(PackObjectId, ObjectType, Vec<u8>)>,
    blob_reads: std::cell::Cell<usize>,
}
impl crate::object::ObjectSource for SelectedSource {
    fn get_tree(&self, hash: &ContentHash) -> crate::store::Result<Option<Tree>> {
        let bytes = self
            .entries
            .iter()
            .find(|(id, kind, _)| *id == PackObjectId::Hash(*hash) && *kind == ObjectType::Tree);
        Ok(bytes
            .map(|(_, _, bytes)| Tree::decode_canonical(bytes))
            .transpose()?)
    }
    fn get_state(&self, _: &StateId) -> crate::store::Result<Option<State>> {
        panic!("source publication must never read historical States");
    }
    fn get_blob(&self, hash: &ContentHash) -> crate::store::Result<Option<Blob>> {
        self.blob_reads.set(self.blob_reads.get() + 1);
        let bytes = self
            .entries
            .iter()
            .find(|(id, kind, _)| *id == PackObjectId::Hash(*hash) && *kind == ObjectType::Blob);
        Ok(bytes.map(|(_, _, bytes)| Blob::new(bytes.clone())))
    }
    fn decoded_blob_len(&self, hash: &ContentHash) -> crate::store::Result<Option<u64>> {
        Ok(self
            .entries
            .iter()
            .find(|(id, kind, _)| *id == PackObjectId::Hash(*hash) && *kind == ObjectType::Blob)
            .map(|(_, _, bytes)| bytes.len() as u64))
    }
}
fn export_selected(
    source: &SelectedSource,
    state: &State,
    max_objects: usize,
    max_bytes: u64,
) -> crate::store::Result<PackReader<'static>> {
    let dir = tempfile::tempdir().expect("source spool");
    let index_path = dir.path().join("index");
    let builder = StreamingPackBuilder::new(
        std::io::Cursor::new(Vec::new()),
        index_path.clone(),
        CompressionConfig::default(),
        dir.path().join("buckets"),
    )?;
    let (pack, stats) = build_source_pack(builder, source, state, max_objects, max_bytes)?;
    assert_eq!(stats.object_count, 3);
    PackReader::from_bytes(pack.into_inner(), std::fs::read(index_path)?)
}
#[test]
fn source_export_reads_only_selected_content_and_emits_a_complete_pack() {
    let (state, entries) = fixture();
    let source = SelectedSource {
        entries,
        blob_reads: std::cell::Cell::new(0),
    };
    let reader = export_selected(&source, &state, 16, 65536).expect("source-only export");
    assert_eq!(
        source.blob_reads.get(),
        1,
        "a file and symlink sharing content need only one read"
    );
    assert_eq!(
        reader
            .validate_source_closure(&state, 16, 65536)
            .expect("exact closure")
            .len(),
        3
    );
}
#[test]
fn source_export_checks_address_and_budget_before_publication() {
    let (state, entries) = fixture();
    let mut source = SelectedSource {
        entries,
        blob_reads: std::cell::Cell::new(0),
    };
    assert!(
        export_selected(&source, &state, 2, 65536).is_err(),
        "object budget must apply before writing the excess object"
    );
    assert_eq!(
        source.blob_reads.get(),
        0,
        "known excess objects must not be loaded"
    );
    assert!(
        export_selected(&source, &state, 16, 1).is_err(),
        "decoded byte budget"
    );
    assert_eq!(source.blob_reads.get(), 0);
    source.entries[2].2 = b"wrong bytes under the original address".to_vec();
    assert!(
        export_selected(&source, &state, 16, 65536).is_err(),
        "producer must reject a corrupt source address"
    );
}
