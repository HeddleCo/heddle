// SPDX-License-Identifier: Apache-2.0

use heddle_format::compression::CompressionConfig;
use tempfile::TempDir;

use super::PackManager;
use crate::{
    object::ContentHash,
    store::pack::{ObjectType, PackBuilder},
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
        assert!(manager.has_object_id(id));
        assert!(manager.get_object(id).unwrap().is_some());
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
fn adding_a_pack_extends_a_completed_lazy_index() {
    let temp = TempDir::new().unwrap();
    let mut manager = PackManager::new_with_index_mode(temp.path().to_path_buf(), false);
    let (pack_path, index_path, id) = write_pack(temp.path(), 1);

    assert!(!manager.has_object(&id));
    assert_eq!(cached_location_count(&manager), 0);
    manager.add_pack(pack_path, index_path).unwrap();
    assert_eq!(cached_location_count(&manager), 1);
    assert!(manager.has_object(&id));
    assert!(manager.get_hashed_object(&id).unwrap().is_some());
}
