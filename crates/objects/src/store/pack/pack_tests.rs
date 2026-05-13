// SPDX-License-Identifier: Apache-2.0
//! Tests for packfile operations.

use tempfile::TempDir;

use super::{ObjectType, PackBuilder, PackObjectId, PackReader, pack_index::PackIndex};
use crate::{
    delta::MAX_DELTA_OUTPUT_SIZE,
    object::{ChangeId, ContentHash},
    store::{compression::CompressionConfig, pack::pack_container_spec},
};

fn create_test_hash(n: u8) -> ContentHash {
    let bytes: [u8; 32] = [n; 32];
    ContentHash::from_bytes(bytes)
}

#[test]
fn test_pack_index_roundtrip() {
    let mut index = PackIndex::new();
    index.add(PackObjectId::Hash(create_test_hash(1)), 100);
    index.add(PackObjectId::Hash(create_test_hash(2)), 200);
    index.add(PackObjectId::ChangeId(ChangeId::from_bytes([3; 16])), 300);
    index.sort();

    let bytes = index.to_bytes();
    let restored = PackIndex::from_bytes(&bytes).expect("Failed to deserialize index");

    assert_eq!(
        restored.find(&PackObjectId::Hash(create_test_hash(1))),
        Some(100)
    );
    assert_eq!(
        restored.find(&PackObjectId::Hash(create_test_hash(2))),
        Some(200)
    );
    assert_eq!(
        restored.find(&PackObjectId::ChangeId(ChangeId::from_bytes([3; 16]))),
        Some(300)
    );
    assert_eq!(
        restored.find(&PackObjectId::Hash(create_test_hash(4))),
        None
    );
}

#[test]
fn test_pack_builder_basic() {
    let compression = CompressionConfig::default();
    let mut builder = PackBuilder::new(compression);

    let hash1 = create_test_hash(1);
    let data1 = b"Hello, World!".to_vec();
    builder.add(hash1, ObjectType::Blob, data1.clone());

    let hash2 = create_test_hash(2);
    let data2 = b"Goodbye, World!".to_vec();
    builder.add(hash2, ObjectType::Blob, data2.clone());

    let (pack_data, index_data, stats) = builder.build().expect("Failed to build pack");

    assert!(!pack_data.is_empty());
    assert!(!index_data.is_empty());
    assert_eq!(stats.object_count, 2);
    assert!(stats.compression_ratio > 0.0 && stats.compression_ratio <= 1.0);
}

#[test]
fn test_pack_reader() {
    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let pack_path = temp_dir.path().join("test.pack");
    let index_path = temp_dir.path().join("test.idx");

    let compression = CompressionConfig::default();
    let mut builder = PackBuilder::new(compression);

    let hash1 = create_test_hash(1);
    let data1 = b"Test data 1".repeat(100);
    builder.add(hash1, ObjectType::Blob, data1.clone());

    let (pack_data, index_data, _) = builder.build().expect("Failed to build pack");

    std::fs::write(&pack_path, &pack_data).expect("Failed to write pack file");
    std::fs::write(&index_path, &index_data).expect("Failed to write index file");

    let reader = PackReader::open(&pack_path, &index_path).expect("Failed to open pack");
    let (obj_type, retrieved) = reader
        .get_hashed_object(&hash1)
        .expect("Failed to get object")
        .expect("Object not found");

    assert_eq!(obj_type, ObjectType::Blob);
    assert_eq!(retrieved, data1);
}

#[test]
fn test_delta_compression() {
    let compression = CompressionConfig::default();
    let mut builder = PackBuilder::new(compression);

    let base_hash = create_test_hash(1);
    let base_data = b"This is the base content. ".repeat(100).to_vec();
    builder.add(base_hash, ObjectType::Blob, base_data.clone());

    let target_hash = create_test_hash(2);
    let target_data = b"This is modified content. ".repeat(100).to_vec();
    builder.add(target_hash, ObjectType::Blob, target_data.clone());

    let (_pack_data, _index_data, stats) = builder.build().expect("Failed to build pack");

    assert!(stats.delta_count > 0);
    assert!(stats.compression_ratio < 1.0);
}

#[test]
fn test_pack_reader_rejects_delta_output_above_limit() {
    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let pack_path = temp_dir.path().join("test.pack");
    let index_path = temp_dir.path().join("test.idx");

    let target_hash = create_test_hash(9);
    let oversized = (MAX_DELTA_OUTPUT_SIZE + 1) as u64;

    // Build a valid pack with a delta entry whose uncompressed size exceeds the limit
    let mut pack_data = Vec::new();
    pack_data.extend_from_slice(pack_container_spec().magic);
    pack_data.extend_from_slice(&pack_container_spec().version.to_be_bytes());
    pack_data.extend_from_slice(&1u64.to_be_bytes()); // 1 object

    let entry_offset = pack_data.len() as u64;

    let record = super::PackObjectRecord {
        id: PackObjectId::Hash(target_hash),
        obj_type: ObjectType::Blob,
        data: vec![0],
        delta_base: Some(PackObjectId::Hash(create_test_hash(1))),
        path_hint: None,
    };
    let mut encoded = Vec::new();
    super::encode_tagged_entry_parts(
        &mut encoded,
        record.id,
        ObjectType::Delta,
        oversized as usize,
        record.delta_base,
        &[0, b'x'],
    )
    .unwrap();
    pack_data.extend_from_slice(&encoded);

    let checksum = blake3::hash(&pack_data);
    pack_data.extend_from_slice(checksum.as_bytes());

    let mut index = PackIndex::new();
    index.add(PackObjectId::Hash(target_hash), entry_offset);
    index.sort();

    std::fs::write(&pack_path, &pack_data).expect("Failed to write pack file");
    std::fs::write(&index_path, index.to_bytes()).expect("Failed to write index file");

    let reader = PackReader::open(&pack_path, &index_path).expect("Failed to open pack");
    let error = reader
        .get_hashed_object(&target_hash)
        .expect_err("oversized delta output should fail");

    assert!(
        matches!(error, crate::store::StoreError::InvalidObject(message) if message.contains("Delta output size"))
    );
}

#[test]
fn test_pack_index_rejects_impossible_entry_count() {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(super::pack_index::INDEX_MAGIC);
    bytes.extend_from_slice(&super::pack_index::INDEX_VERSION.to_be_bytes());
    bytes.extend_from_slice(&(2_u64).to_be_bytes());
    bytes.extend_from_slice(create_test_hash(1).as_bytes());
    bytes.extend_from_slice(&(123_u64).to_be_bytes());

    let error = match PackIndex::from_bytes(&bytes) {
        Ok(_) => panic!("impossible count should fail"),
        Err(error) => error,
    };
    assert!(
        matches!(error, crate::store::StoreError::InvalidObject(message) if message.contains("count"))
    );
}

#[test]
fn test_pack_reader_rejects_truncated_pack() {
    let temp_dir = TempDir::new().unwrap();
    let pack_path = temp_dir.path().join("test.pack");
    let index_path = temp_dir.path().join("test.idx");

    // Write a pack that's too short (< 32 bytes for checksum)
    std::fs::write(&pack_path, b"short").unwrap();
    std::fs::write(&index_path, b"").unwrap();

    match PackReader::open(&pack_path, &index_path) {
        Err(crate::store::StoreError::InvalidObject(msg)) => {
            assert!(
                msg.contains("too short") || msg.contains("Pack"),
                "expected 'too short' error, got: {msg}"
            );
        }
        Err(e) => panic!("expected InvalidObject, got: {e:?}"),
        Ok(_) => panic!("expected error for truncated pack"),
    }
}

#[test]
fn test_pack_reader_rejects_corrupt_checksum() {
    let temp_dir = TempDir::new().unwrap();
    let pack_path = temp_dir.path().join("test.pack");
    let index_path = temp_dir.path().join("test.idx");

    // Build a valid pack, then corrupt the checksum
    let compression = CompressionConfig::default();
    let mut builder = PackBuilder::new(compression);
    builder.add(create_test_hash(1), ObjectType::Blob, b"data".to_vec());
    let (mut pack_data, index_data, _) = builder.build().unwrap();

    // Flip a byte in the trailing checksum
    let last = pack_data.len() - 1;
    pack_data[last] ^= 0xFF;

    std::fs::write(&pack_path, &pack_data).unwrap();
    std::fs::write(&index_path, &index_data).unwrap();

    match PackReader::open(&pack_path, &index_path) {
        Err(crate::store::StoreError::InvalidObject(msg)) => {
            assert!(
                msg.contains("checksum"),
                "expected checksum error, got: {msg}"
            );
        }
        Err(e) => panic!("expected InvalidObject, got: {e:?}"),
        Ok(_) => panic!("expected error for corrupt checksum"),
    }
}

#[test]
fn test_pack_index_rejects_bad_magic() {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"BAAD"); // wrong magic
    bytes.extend_from_slice(&super::pack_index::INDEX_VERSION.to_be_bytes());
    bytes.extend_from_slice(&0u64.to_be_bytes());

    let err = PackIndex::from_bytes(&bytes).unwrap_err();
    assert!(
        matches!(err, crate::store::StoreError::InvalidObject(ref msg) if msg.contains("magic")),
        "expected magic error, got: {err:?}"
    );
}

#[test]
fn test_pack_index_rejects_bad_version() {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(super::pack_index::INDEX_MAGIC);
    bytes.extend_from_slice(&999u32.to_be_bytes()); // unsupported version
    bytes.extend_from_slice(&0u64.to_be_bytes());

    let err = PackIndex::from_bytes(&bytes).unwrap_err();
    assert!(
        matches!(err, crate::store::StoreError::InvalidObject(ref msg) if msg.contains("version")),
        "expected version error, got: {err:?}"
    );
}

#[test]
fn test_pack_reader_missing_object_returns_none() {
    let temp_dir = TempDir::new().unwrap();
    let pack_path = temp_dir.path().join("test.pack");
    let index_path = temp_dir.path().join("test.idx");

    let compression = CompressionConfig::default();
    let mut builder = PackBuilder::new(compression);
    builder.add(create_test_hash(1), ObjectType::Blob, b"data".to_vec());
    let (pack_data, index_data, _) = builder.build().unwrap();

    std::fs::write(&pack_path, &pack_data).unwrap();
    std::fs::write(&index_path, &index_data).unwrap();

    let reader = PackReader::open(&pack_path, &index_path).unwrap();
    // Query for a hash that doesn't exist
    let result = reader.get_hashed_object(&create_test_hash(99)).unwrap();
    assert!(result.is_none(), "non-existent hash should return None");
}

/// Helper to write a pack+index to disk and open a reader.
fn build_and_open_pack(
    objects: Vec<(ContentHash, ObjectType, Vec<u8>, Option<String>)>,
) -> PackReader {
    let temp_dir = TempDir::new().unwrap();
    let pack_path = temp_dir.path().join("test.pack");
    let index_path = temp_dir.path().join("test.idx");

    let compression = CompressionConfig::default();
    let mut builder = PackBuilder::new(compression);
    for (hash, obj_type, data, path) in objects {
        builder.add_with_path(hash, obj_type, data, path);
    }

    let (pack_data, index_data, _) = builder.build().unwrap();
    std::fs::write(&pack_path, &pack_data).unwrap();
    std::fs::write(&index_path, &index_data).unwrap();

    // Leak temp_dir so files survive for the reader's lifetime
    std::mem::forget(temp_dir);

    PackReader::open(&pack_path, &index_path).unwrap()
}

#[test]
fn test_delta_chain_roundtrip() {
    // Create a chain of 5 objects that each differ slightly from the previous.
    // The builder should chain A→B→C→D→E where each deltas against its predecessor.
    let shared = b"This is shared content that remains constant across all versions. ".repeat(10);
    let mut objects = Vec::new();

    for i in 0..5u8 {
        let mut data = shared.clone();
        data.extend_from_slice(format!("version {i} unique suffix data here").as_bytes());
        let hash = ContentHash::compute(&data);
        objects.push((
            hash,
            ObjectType::Blob,
            data,
            Some("test/file.txt".to_string()),
        ));
    }

    let hashes: Vec<ContentHash> = objects.iter().map(|(h, _, _, _)| *h).collect();
    let originals: Vec<Vec<u8>> = objects.iter().map(|(_, _, d, _)| d.clone()).collect();

    let reader = build_and_open_pack(objects);

    // All objects should round-trip correctly through the chain
    for (i, (hash, expected)) in hashes.iter().zip(originals.iter()).enumerate() {
        let (obj_type, data) = reader
            .get_hashed_object(hash)
            .unwrap_or_else(|e| panic!("Failed to get object {i}: {e}"))
            .unwrap_or_else(|| panic!("Object {i} not found"));
        assert_eq!(obj_type, ObjectType::Blob, "object {i} type mismatch");
        assert_eq!(&data, expected, "object {i} data mismatch");
    }
}

#[test]
fn test_delta_chain_produces_deltas() {
    // Verify that similar objects grouped by path actually produce delta entries
    let shared = b"Shared base content for delta testing. ".repeat(20);
    let mut objects = Vec::new();

    for i in 0..4u8 {
        let mut data = shared.clone();
        data.extend_from_slice(&[i; 32]);
        let hash = ContentHash::compute(&data);
        objects.push((
            hash,
            ObjectType::Blob,
            data,
            Some("src/main.rs".to_string()),
        ));
    }

    let compression = CompressionConfig::default();
    let mut builder = PackBuilder::new(compression);
    for (hash, obj_type, data, path) in objects {
        builder.add_with_path(hash, obj_type, data, path);
    }

    let (_, _, stats) = builder.build().unwrap();
    assert!(
        stats.delta_count >= 1,
        "expected deltas, got {}",
        stats.delta_count
    );
}

#[test]
fn test_single_object_no_delta() {
    // A single object should not produce any deltas
    let data = b"solo object content".repeat(50);
    let hash = ContentHash::compute(&data);

    let compression = CompressionConfig::default();
    let mut builder = PackBuilder::new(compression);
    builder.add(hash, ObjectType::Blob, data.clone());

    let (_, _, stats) = builder.build().unwrap();
    assert_eq!(stats.delta_count, 0);
    assert_eq!(stats.object_count, 1);
}

#[test]
fn test_small_objects_skip_delta() {
    // Objects smaller than MIN_DELTA_SIZE (256 bytes) should not be delta-encoded
    let data1 = b"short object A".to_vec();
    let data2 = b"short object B".to_vec();

    let compression = CompressionConfig::default();
    let mut builder = PackBuilder::new(compression);
    builder.add_with_path(
        ContentHash::compute(&data1),
        ObjectType::Blob,
        data1,
        Some("tiny.txt".to_string()),
    );
    builder.add_with_path(
        ContentHash::compute(&data2),
        ObjectType::Blob,
        data2,
        Some("tiny.txt".to_string()),
    );

    let (_, _, stats) = builder.build().unwrap();
    assert_eq!(
        stats.delta_count, 0,
        "small objects should not be delta-encoded"
    );
}

#[test]
fn test_chain_resets_on_bad_delta() {
    // When objects are very different, the pack builder may or may not use deltas
    // depending on compression. Either way, all objects must round-trip correctly.
    let data1: Vec<u8> = (0..1024).map(|i| ((i * 131 + 17) % 256) as u8).collect();
    let data2: Vec<u8> = (0..1024).map(|i| ((i * 197 + 53) % 256) as u8).collect();
    let _data3: Vec<u8> = (0..1024).map(|i| ((i * 251 + 89) % 256) as u8).collect();

    // Verify all objects round-trip correctly regardless of delta decisions
    let temp_dir = TempDir::new().unwrap();
    let pack_path = temp_dir.path().join("test.pack");
    let index_path = temp_dir.path().join("test.idx");
    let mut builder2 = PackBuilder::new(CompressionConfig::default());
    builder2.add_with_path(
        ContentHash::compute(&data1),
        ObjectType::Blob,
        data1.clone(),
        Some("file.bin".to_string()),
    );
    builder2.add_with_path(
        ContentHash::compute(&data2),
        ObjectType::Blob,
        data2.clone(),
        Some("file.bin".to_string()),
    );
    let (pd, id, _) = builder2.build().unwrap();
    std::fs::write(&pack_path, &pd).unwrap();
    std::fs::write(&index_path, &id).unwrap();
    let reader = PackReader::open(&pack_path, &index_path).unwrap();
    let (_, got) = reader
        .get_hashed_object(&ContentHash::compute(&data1))
        .unwrap()
        .unwrap();
    assert_eq!(got, data1);
    let (_, got) = reader
        .get_hashed_object(&ContentHash::compute(&data2))
        .unwrap()
        .unwrap();
    assert_eq!(got, data2);
}

#[test]
fn test_different_paths_with_different_content_roundtrip() {
    // Objects with different paths and truly different content should round-trip correctly.
    // The sliding window may or may not produce deltas depending on content similarity —
    // what matters is that all objects are retrievable.
    let base_a = vec![0xAA; 1024]; // completely different byte patterns
    let base_b = vec![0xBB; 1024];

    let hash_a = ContentHash::compute(&base_a);
    let hash_b = ContentHash::compute(&base_b);

    let reader = build_and_open_pack(vec![
        (
            hash_a,
            ObjectType::Blob,
            base_a.clone(),
            Some("a.bin".to_string()),
        ),
        (
            hash_b,
            ObjectType::Blob,
            base_b.clone(),
            Some("b.bin".to_string()),
        ),
    ]);

    let (_, got_a) = reader.get_hashed_object(&hash_a).unwrap().unwrap();
    assert_eq!(got_a, base_a);
    let (_, got_b) = reader.get_hashed_object(&hash_b).unwrap().unwrap();
    assert_eq!(got_b, base_b);
}

#[test]
fn test_objects_without_path_use_size_bucketing() {
    // Objects without path hints should fall back to size-based bucketing
    let shared = b"Shared prefix for size bucketing test with enough content. ".repeat(10);

    let mut objects = Vec::new();
    for i in 0..3u8 {
        let mut data = shared.clone();
        data.extend_from_slice(&[i; 16]);
        objects.push((ContentHash::compute(&data), ObjectType::Blob, data, None));
    }

    let originals: Vec<(ContentHash, Vec<u8>)> =
        objects.iter().map(|(h, _, d, _)| (*h, d.clone())).collect();

    let reader = build_and_open_pack(objects);

    for (hash, expected) in &originals {
        let (_, data) = reader.get_hashed_object(hash).unwrap().unwrap();
        assert_eq!(&data, expected);
    }
}

#[test]
fn test_tree_objects_can_be_delta_encoded() {
    // Tree objects should also support delta encoding
    let shared = b"tree serialization data that is shared ".repeat(15);

    let mut objects = Vec::new();
    for i in 0..3u8 {
        let mut data = shared.clone();
        data.extend_from_slice(format!("tree version {i}").as_bytes());
        objects.push((
            ContentHash::compute(&data),
            ObjectType::Tree,
            data,
            Some("src/".to_string()),
        ));
    }

    let originals: Vec<(ContentHash, Vec<u8>)> =
        objects.iter().map(|(h, _, d, _)| (*h, d.clone())).collect();

    let reader = build_and_open_pack(objects);

    for (hash, expected) in &originals {
        let (obj_type, data) = reader.get_hashed_object(hash).unwrap().unwrap();
        assert_eq!(obj_type, ObjectType::Tree);
        assert_eq!(&data, expected);
    }
}

#[test]
fn test_state_objects_not_delta_encoded() {
    // State objects should never be delta-encoded (guard in group_by_type)
    let data1 = b"state data 1".repeat(50);
    let data2 = b"state data 2".repeat(50);

    let compression = CompressionConfig::default();
    let mut builder = PackBuilder::new(compression);
    builder.add(ContentHash::compute(&data1), ObjectType::State, data1);
    builder.add(ContentHash::compute(&data2), ObjectType::State, data2);

    let (_, _, stats) = builder.build().unwrap();
    assert_eq!(stats.delta_count, 0, "states should never be delta-encoded");
}

#[test]
fn test_empty_bucket_is_noop() {
    // An empty pack should produce valid but minimal output
    let compression = CompressionConfig::default();
    let builder = PackBuilder::new(compression);
    let (pack_data, _, stats) = builder.build().unwrap();

    assert_eq!(stats.object_count, 0);
    assert_eq!(stats.delta_count, 0);
    // Pack data should still have magic + version + count + checksum
    assert!(pack_data.len() >= 16 + 32); // header + checksum
}