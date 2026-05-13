// SPDX-License-Identifier: Apache-2.0
//! Tests for compression utilities.

use super::*;

#[test]
#[cfg(feature = "zstd")]
fn test_zstd_roundtrip() {
    let data = b"Hello, World! This is a test of the compression system. ".repeat(100);
    let config = CompressionConfig::default();

    let compressed = compress(&data, &config).unwrap().unwrap();
    let decompressed = decompress(&compressed).unwrap();

    assert_eq!(data.as_slice(), decompressed.as_slice());
    assert!(compressed.len() < data.len());
}

#[test]
fn test_small_data_not_compressed() {
    let data = b"tiny"; // Below min_size
    let config = CompressionConfig::default();

    let result = compress(data, &config).unwrap();
    assert!(result.is_none());
}

#[test]
fn test_disabled_compression() {
    let data = b"Hello, World! ".repeat(100);
    let config = CompressionConfig::disabled();

    let result = compress(&data, &config).unwrap();
    assert!(result.is_none());
}

#[test]
fn test_raw_binary_with_marker_byte_is_not_treated_as_compressed() {
    let raw_zstd_marker_like = [1, 2, 3, b'b', b'i', b'n', b'a', b'r', b'y'];
    let raw_delta_marker_like = [2, 0, 0, 0, 4, b'r', b'a', b'w'];

    assert!(!is_compressed(&raw_zstd_marker_like));
    assert!(!is_compressed(&raw_delta_marker_like));
    assert_eq!(header_uncompressed_size(&raw_zstd_marker_like), None);
    assert_eq!(header_uncompressed_size(&raw_delta_marker_like), None);
    assert_eq!(
        decompress(&raw_zstd_marker_like).unwrap(),
        raw_zstd_marker_like
    );
}

#[test]
fn test_delta_roundtrip() {
    let config = CompressionConfig::default();
    let base2 = b"Hello, World! This is version 1. ";
    let target2 = b"Hello, World! This is version 2. ";

    let compressed = compress_delta(target2, base2, &config).unwrap();
    if let Some(compressed) = compressed {
        let decompressed = decompress_delta(&compressed, base2).unwrap();
        assert_eq!(target2.as_slice(), decompressed.as_slice());
    }
}

#[test]
fn test_compression_type_from_u8() {
    assert_eq!(CompressionType::from_u8(0), Some(CompressionType::None));
    assert_eq!(CompressionType::from_u8(1), Some(CompressionType::Zstd));
    assert_eq!(CompressionType::from_u8(2), Some(CompressionType::Delta));
    assert_eq!(CompressionType::from_u8(99), None);
}

#[test]
#[cfg(feature = "zstd")]
fn test_compression_info() {
    let data = b"Hello, World! ".repeat(100);
    let config = CompressionConfig::default();

    let compressed = compress(&data, &config).unwrap().unwrap();
    let info = compression_info(&compressed).unwrap();

    assert_eq!(info.0, CompressionType::Zstd);
    assert_eq!(info.1, data.len() as u64);
}

#[test]
fn test_config_from_env() {
    unsafe {
        std::env::set_var("HEDDLE_COMPRESSION", "1");
        std::env::set_var("HEDDLE_COMPRESSION_LEVEL", "10");
        std::env::set_var("HEDDLE_COMPRESSION_MIN_SIZE", "512");
    }

    let config = CompressionConfig::from_env();

    assert_eq!(config.enabled, cfg!(feature = "zstd"));
    assert_eq!(config.level, 10);
    assert_eq!(config.min_size, 512);

    unsafe {
        std::env::remove_var("HEDDLE_COMPRESSION");
        std::env::remove_var("HEDDLE_COMPRESSION_LEVEL");
        std::env::remove_var("HEDDLE_COMPRESSION_MIN_SIZE");
    }
}

#[test]
#[cfg(feature = "zstd")]
fn test_decompress_rejects_header_size_mismatch() {
    let data = b"Hello, World! This is a test of the compression system. ".repeat(100);
    let config = CompressionConfig::default();

    let mut compressed = compress(&data, &config).unwrap().unwrap();
    compressed[1..9].copy_from_slice(&((data.len() as u64) + 1).to_be_bytes());

    let error = decompress(&compressed).unwrap_err();
    assert!(matches!(error, CompressionError::CorruptedData(_)));
}

#[test]
fn test_compression_info_reads_u64_sizes() {
    let size = (u32::MAX as u64) + 123;
    let mut encoded = vec![CompressionType::Zstd as u8];
    encoded.extend_from_slice(&size.to_be_bytes());
    encoded.extend_from_slice(&[0u8; 4]);

    let info = compression_info(&encoded).unwrap();
    assert_eq!(info, (CompressionType::Zstd, size));
}

#[test]
#[cfg(feature = "zstd")]
fn test_decompress_rejects_oversized_header() {
    let compressed_payload = zstd::encode_all(&b"tiny"[..], 3).unwrap();
    let size = (256_u64 * 1024 * 1024) + 1;

    let mut encoded = vec![CompressionType::Zstd as u8];
    encoded.extend_from_slice(&size.to_be_bytes());
    encoded.extend_from_slice(&compressed_payload);

    let error = decompress(&encoded).unwrap_err();
    assert!(matches!(error, CompressionError::SizeLimitExceeded { .. }));
}

#[test]
fn test_decompress_none_header_rejects_size_mismatch() {
    let payload = b"tiny";
    let mut encoded = vec![CompressionType::None as u8];
    encoded.extend_from_slice(&((payload.len() as u64) + 1).to_be_bytes());
    encoded.extend_from_slice(payload);

    let error = decompress(&encoded).unwrap_err();
    assert!(matches!(error, CompressionError::CorruptedData(_)));
}

#[test]
fn test_decompress_none_header_rejects_oversized_size() {
    let payload = b"tiny";
    let size = (256_u64 * 1024 * 1024) + 1;
    let mut encoded = vec![CompressionType::None as u8];
    encoded.extend_from_slice(&size.to_be_bytes());
    encoded.extend_from_slice(payload);

    let error = decompress(&encoded).unwrap_err();
    assert!(matches!(error, CompressionError::SizeLimitExceeded { .. }));
}

#[test]
#[cfg(not(feature = "zstd"))]
fn test_default_config_disables_zstd_when_feature_is_absent() {
    let config = CompressionConfig::default();
    let data = b"Hello, World! This is a test of the compression system. ".repeat(100);

    assert!(!config.enabled);
    assert!(compress(&data, &config).unwrap().is_none());
}