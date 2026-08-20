// SPDX-License-Identifier: Apache-2.0

use std::{
    fs::OpenOptions,
    sync::Arc,
    time::{Duration, SystemTime},
};

use heddle_format::compression::CompressionConfig;
use tempfile::TempDir;

use super::PackManager;
use crate::{
    object::{ContentHash, Tree, TreeEntry},
    store::pack::{ObjectType, PackBuilder, PackObjectId, PackReadTier, StreamingPackBuilder},
};

fn write_pack(
    root: &std::path::Path,
    ordinal: usize,
) -> (std::path::PathBuf, std::path::PathBuf, ContentHash) {
    let payload = format!("pack-object-{ordinal}").into_bytes();
    let object_id = ContentHash::compute(&payload);
    let mut builder = PackBuilder::new(CompressionConfig {
        max_delta_size: 0,
        ..CompressionConfig::default()
    });
    builder.add(object_id, ObjectType::Blob, payload);
    let (pack_data, index_data, _) = builder.build().unwrap();
    let pack_path = root.join(format!("format-{ordinal:03}.pack"));
    let index_path = root.join(format!("format-{ordinal:03}.idx"));
    std::fs::write(&pack_path, pack_data).unwrap();
    std::fs::write(&index_path, index_data).unwrap();
    (pack_path, index_path, object_id)
}

fn cached_location_count(manager: &PackManager) -> usize {
    manager.object_locations.read().unwrap().locations.len()
}

#[test]
fn discovered_packs_are_ordered_oldest_to_newest() {
    let temp = TempDir::new().unwrap();
    let (old_pack, _, _) = write_pack(temp.path(), 999);
    let (new_pack, _, _) = write_pack(temp.path(), 0);
    std::fs::File::options()
        .write(true)
        .open(&old_pack)
        .unwrap()
        .set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(1))
        .unwrap();
    std::fs::File::options()
        .write(true)
        .open(&new_pack)
        .unwrap()
        .set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(2))
        .unwrap();

    let manager = PackManager::new_with_index_mode(temp.path().to_path_buf(), false);
    let discovered = manager
        .packs
        .iter()
        .map(|pack| pack.pack_path.clone())
        .collect::<Vec<_>>();
    assert_eq!(discovered, vec![old_pack, new_pack]);
}

fn write_solid_tree_pack(root: &std::path::Path, name: &str, trees: &[Tree]) {
    let pack_path = root.join(format!("{name}.pack"));
    let index_path = root.join(format!("{name}.idx"));
    let bucket_dir = root.join(format!("{name}-buckets"));
    let file = OpenOptions::new()
        .create_new(true)
        .read(true)
        .write(true)
        .open(&pack_path)
        .unwrap();
    let mut builder =
        StreamingPackBuilder::new(file, index_path, CompressionConfig::disabled(), bucket_dir)
            .unwrap();
    let ids = trees
        .iter()
        .map(|tree| PackObjectId::Hash(tree.hash()))
        .collect::<Vec<_>>();
    let frame = heddle_object_model::compact::encode_tree_frame(trees).unwrap();
    builder
        .add_shared_frame(&ids, ObjectType::Tree, frame.len(), &frame)
        .unwrap();
    let _ = builder.finalize().unwrap();
}

fn write_hot_tree_pack(
    root: &std::path::Path,
    name: &str,
    tree: &Tree,
) -> (std::path::PathBuf, std::path::PathBuf) {
    let mut builder = PackBuilder::new(CompressionConfig::disabled());
    builder.add(
        tree.hash(),
        ObjectType::Tree,
        tree.encode_canonical().unwrap(),
    );
    let (pack, index, _) = builder.build().unwrap();
    let pack_path = root.join(format!("{name}.pack"));
    let index_path = root.join(format!("{name}.idx"));
    std::fs::write(&pack_path, pack).unwrap();
    std::fs::write(&index_path, index).unwrap();
    (pack_path, index_path)
}

#[test]
fn object_locator_is_lazy_for_incremental_and_reloaded_packs() {
    let temp = TempDir::new().unwrap();
    let mut manager = PackManager::new_with_index_mode(temp.path().to_path_buf(), false);
    for ordinal in 0..8 {
        let (pack_path, index_path, _) = write_pack(temp.path(), ordinal);
        manager.add_pack(pack_path, index_path).unwrap();
    }

    let ids = manager.list_all_ids().unwrap();
    assert_eq!(ids.len(), 8);
    assert_eq!(cached_location_count(&manager), 0);
    for id in &ids {
        assert!(manager.get_object(id).unwrap().is_some());
    }
    assert_eq!(
        cached_location_count(&manager),
        0,
        "point reads must not materialize the global object map"
    );
    for id in &ids {
        assert!(manager.has_object_id(id));
    }
    assert_eq!(cached_location_count(&manager), 8);

    let reloaded = PackManager::new_with_index_mode(temp.path().to_path_buf(), false);
    assert_eq!(reloaded.pack_count(), 8);
    assert_eq!(cached_location_count(&reloaded), 0);
    assert!(
        reloaded
            .packs
            .iter()
            .all(|pack| pack.reader.get().is_none())
    );
    for id in &ids {
        assert!(reloaded.has_object_id(id));
        assert!(reloaded.get_object(id).unwrap().is_some());
    }
}

#[test]
fn adding_a_pack_remains_point_addressable_without_a_global_index() {
    let temp = TempDir::new().unwrap();
    let mut manager = PackManager::new_with_index_mode(temp.path().to_path_buf(), false);
    let (pack_path, index_path, id) = write_pack(temp.path(), 1);

    assert!(!manager.has_object(&id));
    assert_eq!(cached_location_count(&manager), 0);
    manager.add_pack(pack_path, index_path).unwrap();
    assert_eq!(cached_location_count(&manager), 0);
    assert!(manager.has_object(&id));
    assert!(manager.get_hashed_object(&id).unwrap().is_some());
}

#[test]
fn random_access_record_wins_over_solid_frame_for_concurrent_reads() {
    let temp = TempDir::new().unwrap();
    let blob = ContentHash::compute_typed("blob", b"hot-tier-payload");
    let trees = vec![
        Tree::from_entries(vec![TreeEntry::file("a", blob, false).unwrap()]),
        Tree::from_entries(vec![TreeEntry::file("b", blob, true).unwrap()]),
    ];
    write_solid_tree_pack(temp.path(), "a-solid", &trees);
    let mut manager = PackManager::new_with_index_mode(temp.path().to_path_buf(), false);
    let hot_id = PackObjectId::Hash(trees[0].hash());
    let solid_id = PackObjectId::Hash(trees[1].hash());
    assert_eq!(
        manager.object_read_tier(&hot_id).unwrap(),
        Some(PackReadTier::SolidFrame)
    );

    let (hot_pack, hot_index) = write_hot_tree_pack(temp.path(), "z-hot", &trees[0]);
    manager.add_pack(hot_pack, hot_index).unwrap();
    assert_eq!(
        manager.object_read_tier(&hot_id).unwrap(),
        Some(PackReadTier::Hot)
    );
    assert_eq!(
        manager.object_read_tier(&solid_id).unwrap(),
        Some(PackReadTier::SolidFrame)
    );
    let manager = Arc::new(manager);

    let expected = Arc::new(trees[0].encode_canonical().unwrap());
    let readers = (0..8)
        .map(|_| {
            let manager = Arc::clone(&manager);
            let expected = Arc::clone(&expected);
            std::thread::spawn(move || {
                for _ in 0..32 {
                    let (object_type, bytes) = manager.get_object(&hot_id).unwrap().unwrap();
                    assert_eq!(object_type, ObjectType::Tree);
                    assert_eq!(bytes, *expected);
                }
            })
        })
        .collect::<Vec<_>>();
    for reader in readers {
        reader.join().unwrap();
    }

    let solid = manager
        .packs
        .iter()
        .find(|pack| pack.pack_path.ends_with("a-solid.pack"))
        .unwrap()
        .reader()
        .unwrap();
    assert_eq!(
        solid.compact_frame_read_count(),
        0,
        "hot-tier point reads must not decompress the duplicate solid frame"
    );
    assert!(manager.get_object(&solid_id).unwrap().is_some());
    assert_eq!(solid.compact_frame_read_count(), 1);
}
