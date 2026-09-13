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
fn visible_source_pack_proves_hidden_leaves_without_reading_their_content() {
    use crate::object::{
        AudienceTier, EntryRedactions, EntryVisibilityEntry, PartialTree, PartialTreeLeaf,
        VisibilityTier, encode_redacted_projection, visible,
    };
    let open = Blob::new(b"visible bytes".to_vec());
    let secret = Blob::new(b"hidden bytes".to_vec());
    let hidden_tree = ContentHash::compute(b"hidden subtree deliberately absent");
    let tree = Tree::from_entries_salted_v4(
        vec![
            TreeEntry::directory("private-dir", hidden_tree).expect("directory"),
            TreeEntry::file("secret-name.txt", secret.hash(), false).expect("secret"),
            TreeEntry::file("visible.txt", open.hash(), false).expect("visible"),
        ],
        vec![[1; 32], [2; 32], [3; 32]],
    )
    .expect("salted root");
    let state = State::new_snapshot(
        tree.hash(),
        vec![],
        Attribution::human(Principal::new("owner", "owner@example.test")),
    );
    let mut redactions = EntryRedactions::default();
    for name in ["private-dir", "secret-name.txt"] {
        redactions.extend_overrides(
            &[EntryVisibilityEntry {
                tree_id: tree.hash(),
                leaf_hash: tree.v4_leaf_hash_for(name).expect("leaf"),
                tier: VisibilityTier::Private {
                    scope_label: "security".into(),
                },
            }],
            |tier| visible(tier, &AudienceTier::Internal),
        );
    }
    // The withheld bytes are absent from the source. Descending into the
    // hidden directory or reading its sibling blob must fail this build.
    let source = SelectedSource {
        entries: vec![
            (
                PackObjectId::Hash(tree.hash()),
                ObjectType::Tree,
                tree.encode_canonical().expect("tree"),
            ),
            (
                PackObjectId::Hash(open.hash()),
                ObjectType::Blob,
                open.content().to_vec(),
            ),
        ],
        blob_reads: std::cell::Cell::new(0),
    };
    let temp = tempfile::tempdir().expect("temporary pack");
    let path = temp.path().join("visible.pack");
    let index = temp.path().join("visible.idx");
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(&path)
        .expect("pack file");
    let builder = StreamingPackBuilder::new(
        file,
        index.clone(),
        Default::default(),
        temp.path().join("buckets"),
    )
    .expect("builder");
    build_visible_source_pack(builder, &source, &state, &redactions, 16, 65536)
        .expect("visible closure requires no hidden bytes");
    assert_eq!(source.blob_reads.get(), 1);
    let packed = PackReader::open(&path, &index).expect("pack reader");
    let verified = packed
        .validate_visible_source_closure(&state, 16, 65536)
        .expect("verified disclosure");
    assert_eq!(verified.objects.len(), 3);
    assert_eq!(verified.partial_trees.len(), 1);
    assert_eq!(verified.partial_trees[0].declared_root(), state.tree);
    assert_eq!(verified.partial_trees[0].redacted_count(), 2);
    assert!(
        packed
            .get_hashed_object(&secret.hash())
            .expect("lookup hidden blob")
            .is_none()
    );
    assert!(
        packed
            .get_hashed_object(&hidden_tree)
            .expect("lookup hidden tree")
            .is_none()
    );
    assert!(
        packed.validate_source_closure(&state, 16, 65536).is_err(),
        "partial disclosure must not become complete source availability"
    );
    let mut records = Vec::new();
    packed
        .visit_objects(|id, kind, bytes| {
            if kind == ObjectType::Tree {
                assert!(
                    !bytes
                        .windows(b"secret-name.txt".len())
                        .any(|part| part == b"secret-name.txt")
                );
            }
            records.push((id, kind, bytes.to_vec()));
            Ok(())
        })
        .expect("inspect disclosed records");
    let mut extra = records.clone();
    extra.push((
        PackObjectId::Hash(secret.hash()),
        ObjectType::Blob,
        secret.into_content(),
    ));
    assert!(
        reader(extra)
            .validate_visible_source_closure(&state, 16, 65536)
            .is_err(),
        "hidden bytes cannot be smuggled alongside a valid partial proof"
    );
    let partial = &verified.partial_trees[0];
    let mut forged = partial.leaves().to_vec();
    let hidden = forged
        .iter_mut()
        .find(|leaf| matches!(leaf, PartialTreeLeaf::Redacted { .. }))
        .expect("redacted leaf");
    *hidden = PartialTreeLeaf::Redacted {
        leaf_hash: ContentHash::compute(b"forged commitment"),
    };
    let forged = PartialTree::new(state.tree, forged);
    let forged = encode_redacted_projection(&forged).expect("encode malformed proof fixture");
    let root = records
        .iter_mut()
        .find(|(id, _, _)| *id == PackObjectId::Hash(state.tree))
        .expect("root record");
    root.2 = forged;
    assert!(
        reader(records)
            .validate_visible_source_closure(&state, 16, 65536)
            .is_err(),
        "a changed opaque commitment no longer proves the selected root"
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
