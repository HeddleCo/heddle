// SPDX-License-Identifier: Apache-2.0

#[cfg(test)]
mod tests {
    use std::fs;

    use heddle_format::compression::CompressionConfig;
    use tempfile::TempDir;

    use super::SnapshotPackManager;
    use crate::{
        object::ContentHash,
        store::pack::{ObjectType, PackBuilder, PackManager},
    };

    #[test]
    fn objects_owned_pack_wrapper_preserves_pack_bytes_and_reads() {
        let temp = TempDir::new().unwrap();
        let packs_dir = temp.path().join("packs");
        fs::create_dir_all(&packs_dir).unwrap();

        let payload = b"issue-1122-pack-snapshot-seam".to_vec();
        let hash = ContentHash::compute(&payload);
        let mut builder = PackBuilder::new(CompressionConfig::disabled());
        builder.add(hash, ObjectType::Blob, payload.clone());
        let (pack_data, index_data, _) = builder.build().unwrap();

        assert_eq!(
            blake3::hash(&pack_data).to_hex().as_str(),
            "332afc6e60a35973800c50e7599bcb41ed055b730d41acfc7f9f1cd574408ccd",
            "pack bytes must match the pre-refactor main baseline"
        );
        assert_eq!(
            blake3::hash(&index_data).to_hex().as_str(),
            "5baf8125e8db75055da475cc41f4bcc6ec90d0452c266adf3ebc440cce38b32b",
            "index bytes must match the pre-refactor main baseline"
        );

        let pack_path = packs_dir.join("fixture.pack");
        let index_path = packs_dir.join("fixture.idx");
        fs::write(&pack_path, &pack_data).unwrap();
        fs::write(&index_path, &index_data).unwrap();

        let format_manager = PackManager::new(packs_dir.clone());
        let mut snapshot_manager = SnapshotPackManager::new(packs_dir);
        assert_eq!(
            snapshot_manager.get_hashed_object(&hash).unwrap(),
            format_manager.get_hashed_object(&hash).unwrap()
        );
        assert_eq!(
            snapshot_manager.get_hashed_object(&hash).unwrap(),
            Some((ObjectType::Blob, payload))
        );

        snapshot_manager.reload().unwrap();
        assert_eq!(fs::read(pack_path).unwrap(), pack_data);
        assert_eq!(fs::read(index_path).unwrap(), index_data);
    }
}
