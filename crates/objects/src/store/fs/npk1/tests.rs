// SPDX-License-Identifier: Apache-2.0

use std::{num::NonZeroUsize, sync::Arc};

use proptest::prelude::*;
use sley::{ObjectFormat as GitObjectFormat, ObjectId as GitObjectId};

use super::{
    CHECKSUM_CHUNK_BYTES, CHECKSUM_LEN, MAX_CHAIN_DEPTH, NAME_RESTART, Npk1Pack,
    TRAILER_HEADER_LEN, VERSION,
    codec::{
        decoded_name_rows, decoded_record_blocks, reset_decoded_name_rows,
        reset_decoded_record_blocks,
    },
};
use crate::{
    object::{Attribution, ContentHash, Principal, SpoolId, State, StateId, Tree, TreeEntry},
    store::{
        FsRepackOperation, FsStore, ObjectStore, RepackPolicy, RepackResourceLimits,
        RepackSchedule, RepackScheduler, TreeWrite,
    },
};

fn store() -> (tempfile::TempDir, FsStore) {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = FsStore::new(temp.path().join(".heddle"));
    store.init().expect("initialize store");
    (temp, store)
}

fn repack(store: &FsStore) {
    let limits = RepackResourceLimits::new(NonZeroUsize::MIN).with_io_rate(None);
    let scheduler = RepackScheduler::new(RepackPolicy::default(), limits);
    let operation = Arc::new(FsRepackOperation::new(store.clone()));
    let RepackSchedule::Started(handle) = scheduler.repack_now(operation).expect("schedule repack")
    else {
        panic!("repack did not start");
    };
    handle.wait().expect("complete repack");
}

fn content(version: usize, entry: usize) -> ContentHash {
    ContentHash::compute_typed("blob", format!("npk1-{version}-{entry}").as_bytes())
}

fn related_tree(version: usize, width: usize) -> Tree {
    Tree::from_entries(
        (0..width)
            .map(|entry| {
                let content_version = if entry == version % width { version } else { 0 };
                TreeEntry::file(
                    format!("entry-{entry:04}.txt"),
                    content(content_version, entry),
                    entry.is_multiple_of(11),
                )
                .expect("tree entry")
            })
            .collect(),
    )
}

fn only_npk1_path(store: &FsStore) -> std::path::PathBuf {
    store.reload_packs().expect("reload NPK1 generation");
    let paths = store
        .npk1_manager()
        .read()
        .expect("NPK1 manager")
        .file_paths()
        .into_iter()
        .map(std::path::Path::to_path_buf)
        .collect::<Vec<_>>();
    assert_eq!(paths.len(), 1, "one settled tree generation");
    paths[0].clone()
}

fn refresh_checksum_manifest(bytes: &mut [u8], trailer_offset: usize) {
    let chunk_count = u32::from_le_bytes(
        bytes[trailer_offset + 8..trailer_offset + 12]
            .try_into()
            .expect("checksum chunk count"),
    ) as usize;
    for index in 0..chunk_count {
        let start = index * CHECKSUM_CHUNK_BYTES;
        let end = (start + CHECKSUM_CHUNK_BYTES).min(trailer_offset);
        let hash = blake3::hash(&bytes[start..end]);
        let hash_offset = trailer_offset + TRAILER_HEADER_LEN + index * CHECKSUM_LEN;
        bytes[hash_offset..hash_offset + CHECKSUM_LEN].copy_from_slice(hash.as_bytes());
    }
    let checksum_offset = trailer_offset + TRAILER_HEADER_LEN + chunk_count * CHECKSUM_LEN;
    let checksum = blake3::hash(&bytes[trailer_offset..checksum_offset]);
    bytes[checksum_offset..checksum_offset + CHECKSUM_LEN].copy_from_slice(checksum.as_bytes());
}

#[test]
fn pack_roundtrip_direct_lookup_and_loose_coexistence() {
    let (_temp, store) = store();
    let trees = (0..24)
        .map(|version| related_tree(version, 192))
        .collect::<Vec<_>>();
    for tree in &trees {
        store.put_tree(tree).expect("put hot tree");
    }
    repack(&store);

    let reopened = FsStore::new(store.root());
    for tree in &trees {
        assert_eq!(
            reopened.get_tree(&tree.hash()).expect("read packed tree"),
            Some(tree.clone())
        );
    }
    let wanted = trees[17].entries()[17].clone();
    reopened.clear_recent_object_caches();
    assert_eq!(
        reopened
            .get_tree_entry(&trees[17].hash(), wanted.name())
            .expect("direct packed entry lookup"),
        Some(wanted)
    );
    assert!(
        !reopened
            .recent_trees
            .read()
            .expect("resolved-tree cache")
            .contains(&trees[17].hash()),
        "direct entry lookup must not materialize or cache the whole tree"
    );

    let loose = related_tree(99, 17);
    reopened.put_tree(&loose).expect("put later loose tree");
    assert_eq!(
        reopened.get_tree(&loose.hash()).expect("read loose tree"),
        Some(loose)
    );
    assert_eq!(
        reopened
            .get_tree(&trees[3].hash())
            .expect("read old packed tree"),
        Some(trees[3].clone())
    );
}

#[test]
fn version_one_pack_without_a_record_dictionary_still_roundtrips() {
    let (_temp, store) = store();
    let tree = related_tree(3, 16);
    store.put_tree(&tree).expect("put v1 fixture tree");
    repack(&store);
    let path = only_npk1_path(&store);
    let mut bytes = std::fs::read(&path).expect("read v2 fixture pack");
    let records_offset =
        u64::from_le_bytes(bytes[32..40].try_into().expect("records offset")) as usize;
    let dictionary_offset =
        u64::from_le_bytes(bytes[56..64].try_into().expect("record dictionary offset")) as usize;
    assert_eq!(
        dictionary_offset, records_offset,
        "fixture has no dictionary"
    );
    bytes[4..8].copy_from_slice(&1u32.to_le_bytes());
    bytes[56..64].fill(0);
    let trailer_offset =
        u64::from_le_bytes(bytes[48..56].try_into().expect("trailer offset")) as usize;
    refresh_checksum_manifest(&mut bytes, trailer_offset);
    std::fs::write(&path, bytes).expect("write checksummed v1 fixture");

    let pack = Npk1Pack::open(&path).expect("open v1 pack");
    assert_eq!(pack.resolve(&tree.hash()).expect("resolve v1 tree"), tree);
}

#[test]
fn direct_lookup_decodes_only_the_selected_record_block() {
    let (_temp, store) = store();
    let tree = related_tree(7, 300);
    store.put_tree(&tree).expect("put multi-block tree");
    repack(&store);
    let pack = Npk1Pack::open(&only_npk1_path(&store)).expect("open multi-block pack");
    let wanted = tree.entries()[200].clone();

    reset_decoded_record_blocks();
    assert_eq!(
        pack.lookup(&tree.hash(), wanted.name())
            .expect("direct block lookup"),
        Some(wanted)
    );
    assert_eq!(decoded_record_blocks(), 1);
}

#[test]
fn direct_name_lookup_seeks_restart_blocks() {
    let (_temp, store) = store();
    let tree = related_tree(9, NAME_RESTART * 3);
    store.put_tree(&tree).expect("put restart-spanning tree");
    repack(&store);
    let pack = Npk1Pack::open_direct(&only_npk1_path(&store)).expect("open pack directly");

    for index in [0, 127, 128, 255, 256, 383] {
        let wanted = tree.entries()[index].clone();
        reset_decoded_name_rows();
        assert_eq!(
            pack.lookup(&tree.hash(), wanted.name())
                .expect("lookup restart-spanning name"),
            Some(wanted)
        );
        assert!(
            decoded_name_rows() <= NAME_RESTART,
            "name lookup decoded more than one restart block"
        );
    }

    reset_decoded_name_rows();
    reset_decoded_record_blocks();
    assert_eq!(
        pack.lookup(&tree.hash(), "entry-0127a.txt")
            .expect("lookup absent dictionary name"),
        None
    );
    assert!(decoded_name_rows() <= NAME_RESTART);
    assert_eq!(
        decoded_record_blocks(),
        0,
        "an absent dictionary name must not touch a record block"
    );
}

#[test]
fn direct_open_does_not_touch_every_dictionary_chunk() {
    let (_temp, store) = store();
    let tree = related_tree(11, 10_000);
    store.put_tree(&tree).expect("put large-name-count tree");
    repack(&store);

    let pack = Npk1Pack::open_direct(&only_npk1_path(&store)).expect("open pack directly");
    let (verified, total) = pack.verified_chunk_count().expect("checksum cache size");
    assert!(
        total > verified,
        "fixture must contain untouched data chunks"
    );
}

#[test]
fn loose_hlr1_and_hdc1_settle_to_identical_npk1_trees() {
    let (_temp, store) = store();
    let anchor = related_tree(0, 240);
    let anchor_hash = store.put_tree(&anchor).expect("put HLR1 anchor");
    let anchor_body = store
        .get_tree_serialized(&anchor_hash)
        .expect("read HLR1 anchor")
        .expect("HLR1 anchor body");
    assert!(crate::object::is_lean_tree(&anchor_body));

    let descendant = related_tree(17, 240);
    let encoded = store
        .encode_tree_write(&TreeWrite::descendant(descendant.clone(), anchor_hash))
        .expect("encode HDC1 descendant");
    assert!(crate::object::is_delta_tree(&encoded.data));
    store
        .put_tree_serialized(&encoded.data, encoded.hash)
        .expect("put HDC1 descendant");
    assert_eq!(
        store.get_tree(&anchor_hash).expect("decode loose HLR1"),
        Some(anchor.clone())
    );
    assert_eq!(
        store.get_tree(&encoded.hash).expect("decode loose HDC1"),
        Some(descendant.clone())
    );

    repack(&store);
    store.reload_packs().expect("reload settled generation");
    let manager = store.npk1_manager().read().expect("NPK1 manager");
    assert!(manager.has_tree(&anchor_hash).expect("settled HLR1 anchor"));
    assert!(
        manager
            .has_tree(&encoded.hash)
            .expect("settled HDC1 result")
    );
    drop(manager);
    let trees = super::super::fs_paths::trees_dir(store.root());
    assert!(!super::super::fs_paths::hash_path(&trees, &anchor_hash).exists());
    assert!(!super::super::fs_paths::hash_path(&trees, &encoded.hash).exists());

    let reopened = FsStore::new(store.root());
    assert_eq!(
        reopened
            .get_tree(&anchor_hash)
            .expect("decode settled HLR1 tree from NPK1"),
        Some(anchor)
    );
    assert_eq!(
        reopened
            .get_tree(&encoded.hash)
            .expect("decode settled HDC1 tree from NPK1"),
        Some(descendant)
    );
}

#[test]
fn direct_lookup_honors_remove_and_upsert_deltas() {
    let (_temp, store) = store();
    let base = related_tree(0, 160);
    let mut current = base.clone();
    current.remove("entry-0042.txt");
    let added = TreeEntry::file("new.txt", content(91, 91), false).expect("new entry");
    current.insert(added.clone());
    let attribution = Attribution::human(Principal::new("NPK1", "npk1@example.com"));
    let base_state = State::new(base.hash(), Vec::new(), attribution.clone());
    let current_state = State::new(current.hash(), vec![base_state.id()], attribution);
    store.put_tree(&base).expect("put base tree");
    store.put_state(&base_state).expect("put base state");
    store.put_tree(&current).expect("put current tree");
    store.put_state(&current_state).expect("put current state");
    repack(&store);

    let reopened = FsStore::new(store.root());
    assert_eq!(
        reopened
            .get_tree_entry(&current.hash(), "entry-0042.txt")
            .expect("removed entry lookup"),
        None
    );
    assert_eq!(
        reopened
            .get_tree_entry(&current.hash(), "new.txt")
            .expect("upserted entry lookup"),
        Some(added)
    );
}

#[test]
fn mixed_entry_kinds_and_unicode_names_roundtrip() {
    let (_temp, store) = store();
    let tree = Tree::from_entries(vec![
        TreeEntry::file("plain.txt", content(1, 1), false).expect("plain file"),
        TreeEntry::file("ä-executable", content(2, 2), true).expect("executable"),
        TreeEntry::directory("å-directory", content(3, 3)).expect("directory"),
        TreeEntry::symlink("link", content(4, 4)).expect("symlink"),
        TreeEntry::gitlink(
            "submodule",
            GitObjectId::from_raw(GitObjectFormat::Sha1, &[5; 20]).expect("git object id"),
        )
        .expect("gitlink"),
        TreeEntry::spoollink(
            "spool",
            SpoolId::parse("team/child").expect("spool id"),
            StateId::from_bytes([6; 32]),
        )
        .expect("spoollink"),
    ]);
    store.put_tree(&tree).expect("put mixed tree");
    repack(&store);
    let reopened = FsStore::new(store.root());
    assert_eq!(
        reopened.get_tree(&tree.hash()).expect("read mixed tree"),
        Some(tree.clone())
    );
    for expected in tree.entries() {
        assert_eq!(
            reopened
                .get_tree_entry(&tree.hash(), expected.name())
                .expect("read mixed entry"),
            Some(expected.clone())
        );
    }
}

#[test]
fn repacking_a_pack_only_generation_retains_every_tree() {
    let (_temp, store) = store();
    let mut trees = (0..12)
        .map(|version| related_tree(version, 96))
        .collect::<Vec<_>>();
    for tree in &trees {
        store.put_tree(tree).expect("put first-generation tree");
    }
    repack(&store);
    for tree in &trees {
        assert!(
            !store
                .root()
                .join("objects/trees")
                .join(&tree.hash().to_hex()[..2])
                .join(&tree.hash().to_hex()[2..])
                .exists()
        );
    }

    let later = related_tree(77, 96);
    store.put_tree(&later).expect("put later loose tree");
    trees.push(later);
    repack(&store);
    let reopened = FsStore::new(store.root());
    for tree in &trees {
        assert_eq!(
            reopened
                .get_tree(&tree.hash())
                .expect("read after second repack"),
            Some(tree.clone())
        );
    }
    reopened.reload_packs().expect("reload final generation");
    assert_eq!(
        reopened
            .npk1_manager()
            .read()
            .expect("NPK1 manager")
            .file_paths()
            .len(),
        1,
        "repack must retire the superseded tree generation"
    );
}

#[test]
fn every_chain_obeys_the_depth_bound() {
    let (_temp, store) = store();
    let trees = (0..48)
        .map(|version| related_tree(version, 256))
        .collect::<Vec<_>>();
    for tree in &trees {
        store.put_tree(tree).expect("put tree");
    }
    repack(&store);
    let pack = Npk1Pack::open(&only_npk1_path(&store)).expect("open NPK1");
    let mut deepest = 0usize;
    for tree in &trees {
        let depth = pack.depth(&tree.hash()).expect("chain depth");
        assert!(depth <= MAX_CHAIN_DEPTH);
        deepest = deepest.max(depth);
    }
    assert_eq!(deepest, MAX_CHAIN_DEPTH, "fixture must exercise the bound");
}

#[test]
fn interrupted_unpublished_write_leaves_the_readable_generation_unchanged() {
    let (_temp, store) = store();
    let tree = related_tree(1, 32);
    store.put_tree(&tree).expect("put tree");
    repack(&store);

    let interrupted = store.root().join("packs/.repack-interrupted");
    std::fs::create_dir(&interrupted).expect("create interrupted staging");
    std::fs::write(interrupted.join("replacement.npk"), b"NPK1 interrupted")
        .expect("write partial staged pack");
    let reopened = FsStore::new(store.root());
    assert_eq!(
        reopened
            .get_tree(&tree.hash())
            .expect("read valid generation"),
        Some(tree)
    );
}

#[test]
fn corruption_is_rejected_before_record_decode() {
    let (_temp, store) = store();
    let tree = related_tree(4, 64);
    store.put_tree(&tree).expect("put tree");
    repack(&store);
    let path = only_npk1_path(&store);
    let mut bytes = std::fs::read(&path).expect("read NPK1");
    let middle = bytes.len() / 2;
    bytes[middle] ^= 0x80;
    std::fs::write(&path, bytes).expect("damage NPK1");
    assert!(Npk1Pack::open(&path).is_err());
}

#[test]
fn direct_lookup_verifies_only_the_touched_record_chunks() {
    let (_temp, store) = store();
    let trees = (0..96usize)
        .map(|version| {
            Tree::from_entries(
                (0..256usize)
                    .map(|entry| {
                        TreeEntry::file(
                            format!("tree-{version:03}-entry-{entry:04}"),
                            content(version, entry),
                            false,
                        )
                        .expect("large test entry")
                    })
                    .collect(),
            )
        })
        .collect::<Vec<_>>();
    for tree in &trees {
        store.put_tree(tree).expect("put large test tree");
    }
    repack(&store);

    let path = only_npk1_path(&store);
    let mut bytes = std::fs::read(&path).expect("read NPK1");
    assert_eq!(
        u32::from_le_bytes(bytes[4..8].try_into().expect("pack version")),
        VERSION
    );
    let object_count = u32::from_le_bytes(bytes[8..12].try_into().expect("object count")) as usize;
    let records_offset =
        u64::from_le_bytes(bytes[32..40].try_into().expect("records offset")) as usize;
    let index_offset = u64::from_le_bytes(bytes[40..48].try_into().expect("index offset")) as usize;
    let dictionary_offset =
        u64::from_le_bytes(bytes[56..64].try_into().expect("record dictionary offset")) as usize;
    #[cfg(feature = "zstd")]
    assert!(
        dictionary_offset < records_offset,
        "large v2 fixture must carry a trained record dictionary"
    );
    #[cfg(not(feature = "zstd"))]
    assert_eq!(dictionary_offset, records_offset);
    let entries_start = index_offset + 16 + 256 * 4;
    let offsets_start = entries_start + object_count * 36;
    let indexed_hash = |ordinal: u32| {
        (0..object_count)
            .find_map(|index| {
                let row = entries_start + index * 36;
                let found = u32::from_le_bytes(
                    bytes[row + 32..row + 36]
                        .try_into()
                        .expect("record ordinal"),
                );
                (found == ordinal).then(|| {
                    ContentHash::from_bytes(
                        bytes[row..row + 32].try_into().expect("indexed tree hash"),
                    )
                })
            })
            .expect("indexed ordinal")
    };
    let first_hash = indexed_hash(0);
    let damaged_ordinal = (object_count / 2) as u32;
    let damaged_hash = indexed_hash(damaged_ordinal);
    let record_offset = |ordinal: usize| {
        u32::from_le_bytes(
            bytes[offsets_start + ordinal * 4..offsets_start + ordinal * 4 + 4]
                .try_into()
                .expect("record offset"),
        ) as usize
    };
    let damaged_start = records_offset + record_offset(damaged_ordinal as usize);
    let damaged_end = records_offset + record_offset(damaged_ordinal as usize + 1);
    let damaged_byte = damaged_start + (damaged_end - damaged_start) / 2;
    assert_ne!(
        records_offset / CHECKSUM_CHUNK_BYTES,
        damaged_byte / CHECKSUM_CHUNK_BYTES,
        "fixture must place the damaged record outside the first record chunk"
    );
    bytes[damaged_byte] ^= 0x40;
    std::fs::write(&path, bytes).expect("damage one record chunk");
    assert!(
        Npk1Pack::open(&path).is_err(),
        "strict verification sees every chunk"
    );

    let first = trees
        .iter()
        .find(|tree| tree.hash() == first_hash)
        .expect("first ordinal tree");
    let damaged = trees
        .iter()
        .find(|tree| tree.hash() == damaged_hash)
        .expect("damaged ordinal tree");
    let reopened = FsStore::new(store.root());
    assert_eq!(
        reopened
            .get_tree_entry(&first_hash, first.entries()[0].name())
            .expect("unrelated direct lookup"),
        Some(first.entries()[0].clone()),
        "an unrelated lookup must not scan the damaged chunk"
    );
    assert!(
        reopened
            .get_tree_entry(&damaged_hash, damaged.entries()[0].name())
            .is_err(),
        "the lookup that touches the damaged chunk must reject it"
    );
}

#[test]
fn resolved_tree_is_validated_against_the_indexed_hash() {
    let (_temp, store) = store();
    let tree = related_tree(7, 40);
    store.put_tree(&tree).expect("put tree");
    repack(&store);
    let path = only_npk1_path(&store);
    let mut bytes = std::fs::read(&path).expect("read NPK1");
    let index_offset = u64::from_le_bytes(bytes[40..48].try_into().expect("index offset")) as usize;
    let trailer_offset =
        u64::from_le_bytes(bytes[48..56].try_into().expect("trailer offset")) as usize;
    let index_hash_offset = index_offset + 16 + 256 * 4;
    let mut forged = *tree.hash().as_bytes();
    forged[31] ^= 0x01;
    bytes[index_hash_offset..index_hash_offset + 32].copy_from_slice(&forged);
    let index_checksum_offset = trailer_offset - 32;
    let index_checksum = blake3::hash(&bytes[index_offset..index_checksum_offset]);
    bytes[index_checksum_offset..trailer_offset].copy_from_slice(index_checksum.as_bytes());
    refresh_checksum_manifest(&mut bytes, trailer_offset);
    std::fs::write(&path, bytes).expect("write checksummed forged index");

    let pack = Npk1Pack::open(&path).expect("checksums and structure remain valid");
    let forged = ContentHash::from_bytes(forged);
    assert!(
        pack.resolve(&forged).is_err(),
        "semantic hash validation must reject an index that lies about a record"
    );
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(12))]

    #[test]
    fn repack_preserves_every_retrievable_tree(
        versions in prop::collection::vec(0usize..64, 1..24),
        width in 4usize..48,
    ) {
        let (_temp, store) = store();
        let mut trees = versions
            .into_iter()
            .map(|version| related_tree(version, width))
            .collect::<Vec<_>>();
        trees.sort_by_key(Tree::hash);
        trees.dedup_by_key(|tree| tree.hash());
        for tree in &trees {
            store.put_tree(tree).expect("put property tree");
        }
        let before = trees
            .iter()
            .map(|tree| {
                (
                    tree.hash(),
                    store.get_tree(&tree.hash()).expect("read before repack"),
                )
            })
            .collect::<Vec<_>>();
        repack(&store);
        let reopened = FsStore::new(store.root());
        for (hash, expected) in before {
            prop_assert_eq!(
                reopened.get_tree(&hash).expect("read after repack"),
                expected
            );
        }
    }
}
