// SPDX-License-Identifier: Apache-2.0
use super::{DeltaDecoder, DeltaEncoder, DeltaError, MAX_DELTA_OUTPUT_SIZE, compute_delta};

#[test]
fn test_delta_roundtrip() {
    let base = b"Hello, World! This is a test of the delta compression system.";
    let target = b"Hello, World! This is a modified test of the delta compression system.";

    let delta = DeltaEncoder::encode(base, target);
    let decoded = DeltaDecoder::decode(base, &delta, MAX_DELTA_OUTPUT_SIZE).unwrap();

    assert_eq!(decoded, target);
}

#[test]
fn test_delta_empty_base() {
    let base = b"";
    let target = b"Hello, World!";

    let delta = DeltaEncoder::encode(base, target);
    let decoded = DeltaDecoder::decode(base, &delta, MAX_DELTA_OUTPUT_SIZE).unwrap();

    assert_eq!(decoded, target);
}

#[test]
fn test_delta_same_content() {
    let base = b"Identical content";
    let target = b"Identical content";

    let delta = DeltaEncoder::encode(base, target);
    let decoded = DeltaDecoder::decode(base, &delta, MAX_DELTA_OUTPUT_SIZE).unwrap();

    assert_eq!(decoded, target);
}

#[test]
fn test_delta_no_match() {
    let base = b"Completely different content that doesn't match at all";
    let target = b"Something entirely different without any overlap whatsoever";

    let delta = DeltaEncoder::encode(base, target);
    let decoded = DeltaDecoder::decode(base, &delta, MAX_DELTA_OUTPUT_SIZE).unwrap();

    assert_eq!(decoded, target);
}

#[test]
fn test_compute_delta() {
    let base = b"Line 1\nLine 2\nLine 3\nLine 4\nLine 5";
    let target = b"Line 1\nLine 2 modified\nLine 3\nLine 4\nLine 5\nLine 6";

    let result = compute_delta(base, target);
    assert!(result.is_some());

    let (delta, ratio) = result.unwrap();
    assert!(ratio < 1.0, "delta should be smaller than target");

    let decoded = DeltaDecoder::decode(base, &delta, MAX_DELTA_OUTPUT_SIZE).unwrap();
    assert_eq!(decoded, target);
}

#[test]
fn test_compute_delta_not_beneficial() {
    let base = b"AAAAAAAAAAAAAAAAAAAAAAAA";
    let target = b"BBBBBBBBBBBBBBBBBBBBBBBB";

    let result = compute_delta(base, target);
    if let Some((_, ratio)) = result {
        assert!(
            ratio >= 0.9,
            "should not use delta for very different content"
        );
    }
}

/// Test copy instructions at various offsets including beyond the old 14-bit limit.
#[test]
fn test_copy_instruction_offset_roundtrip() {
    let test_offsets: Vec<usize> =
        vec![0, 63, 64, 127, 128, 200, 255, 16383, 16384, 65535, 100_000];
    const PATTERN_SIZE: usize = 20;
    let max_offset = *test_offsets.iter().max().unwrap();
    let base_size = max_offset + PATTERN_SIZE + 1;
    let mut base = vec![0u8; base_size];

    for &offset in &test_offsets {
        for i in 0..PATTERN_SIZE {
            base[offset + i] = ((offset + i) % 256) as u8;
        }
    }

    let mut target = Vec::new();
    for &offset in &test_offsets {
        target.extend_from_slice(&base[offset..offset + PATTERN_SIZE]);
    }

    let delta = DeltaEncoder::encode(&base, &target);
    let decoded = DeltaDecoder::decode(&base, &delta, MAX_DELTA_OUTPUT_SIZE)
        .expect("delta decode should succeed");

    assert_eq!(
        decoded, target,
        "decoded content should match target for offsets {:?}",
        test_offsets
    );
}

/// Test that large offsets (> 16KB) produce smaller deltas than the old format
/// would have, since the old format capped offsets at 14 bits.
#[test]
fn test_large_offset_copy_efficiency() {
    let mut base = vec![0u8; 200_000];
    for (i, b) in base.iter_mut().enumerate() {
        *b = (i % 251) as u8;
    }
    // Target copies 500 bytes from offset 150_000
    let target = base[150_000..150_500].to_vec();

    let delta = DeltaEncoder::encode(&base, &target);
    let decoded = DeltaDecoder::decode(&base, &delta, MAX_DELTA_OUTPUT_SIZE).unwrap();
    assert_eq!(decoded, target);

    // Delta should be much smaller than the target (a compact copy instruction)
    assert!(
        delta.len() < 20,
        "delta for 500-byte copy should be tiny, got {} bytes",
        delta.len()
    );
}

#[test]
fn test_delta_decode_rejects_output_beyond_limit() {
    // Build a valid delta: insert 2 bytes "xy", then copy 4 bytes from offset 0
    // Total output = 6 bytes, limit = 3
    let base = b"abcdef";
    // Insert "xy": header=1 (length 2), then 'x', 'y'
    // Copy 4 from offset 0: flag=0x80|0x01(offset byte0)|0x10(size byte0) = 0x91,
    // offset_byte=0x00, size_byte=0x04
    let delta = vec![1, b'x', b'y', 0x91, 0x00, 0x04];

    let error = DeltaDecoder::decode(base, &delta, 3).expect_err("delta should exceed limit");

    assert_eq!(
        error,
        DeltaError::OutputLimitExceeded {
            attempted: 6,
            max_output: 3,
        }
    );
}

#[test]
fn test_delta_decode_reports_structured_errors() {
    let error = DeltaDecoder::decode(b"", &[2, b'a'], MAX_DELTA_OUTPUT_SIZE)
        .expect_err("delta should fail with truncated literal");

    assert_eq!(
        error,
        DeltaError::TruncatedLiteral {
            instruction_offset: 0,
            length: 3,
            available: 1,
        }
    );
}

#[test]
fn test_delta_decode_rejects_reserved_instruction() {
    // 0x80 with no offset/size bits set is reserved
    let error = DeltaDecoder::decode(b"abcd", &[0x80], MAX_DELTA_OUTPUT_SIZE)
        .expect_err("reserved instruction should fail");

    assert_eq!(
        error,
        DeltaError::ReservedInstruction {
            instruction_offset: 0,
        }
    );
}

#[test]
fn test_delta_decode_truncated_copy() {
    // 0x91 = copy with offset byte 0 + size byte 0, needs 2 more bytes but delta ends
    let error = DeltaDecoder::decode(b"abcd", &[0x91], MAX_DELTA_OUTPUT_SIZE)
        .expect_err("truncated copy should fail");

    assert!(matches!(error, DeltaError::TruncatedCopyInstruction { .. }));
}

#[test]
fn test_estimate_delta_size_matches_encode() {
    let base = b"Hello, World! This is a test of the delta compression system. ".repeat(10);
    let target =
        b"Hello, World! This is a modified test of the delta compression system. ".repeat(10);

    let actual_delta = DeltaEncoder::encode(&base, &target);
    let estimated = DeltaEncoder::estimate_delta_size(&base, &target);

    assert_eq!(
        estimated,
        actual_delta.len(),
        "estimate should exactly match actual encode size"
    );
}

#[test]
fn test_estimate_delta_size_empty_base() {
    let base = b"";
    let target = b"Hello, World! Some new content here.";

    let actual_delta = DeltaEncoder::encode(base, target);
    let estimated = DeltaEncoder::estimate_delta_size(base, target);

    assert_eq!(estimated, actual_delta.len());
}

#[test]
fn test_estimate_delta_size_identical() {
    let data = b"Identical content that is long enough to trigger copy instructions in the delta. "
        .repeat(5);

    let actual_delta = DeltaEncoder::encode(&data, &data);
    let estimated = DeltaEncoder::estimate_delta_size(&data, &data);

    assert_eq!(estimated, actual_delta.len());
}

#[test]
fn test_estimate_delta_size_no_overlap() {
    let base = b"AAAA BBBB CCCC DDDD EEEE FFFF GGGG HHHH IIII JJJJ KKKK";
    let target = b"1111 2222 3333 4444 5555 6666 7777 8888 9999 0000 !!!!";

    let actual_delta = DeltaEncoder::encode(base, target);
    let estimated = DeltaEncoder::estimate_delta_size(base, target);

    assert_eq!(estimated, actual_delta.len());
}

#[test]
fn test_estimate_delta_size_large_offset() {
    let mut base = vec![0u8; 200_000];
    for (i, b) in base.iter_mut().enumerate() {
        *b = (i % 251) as u8;
    }
    let target = base[150_000..150_500].to_vec();

    let actual_delta = DeltaEncoder::encode(&base, &target);
    let estimated = DeltaEncoder::estimate_delta_size(&base, &target);

    assert_eq!(estimated, actual_delta.len());
}

#[test]
fn test_estimate_delta_size_empty_target() {
    let base = b"some base content";
    let target = b"";

    let actual_delta = DeltaEncoder::encode(base, target);
    let estimated = DeltaEncoder::estimate_delta_size(base, target);

    assert_eq!(estimated, actual_delta.len());
}

/// Test copy instruction size boundaries: sizes that cross byte boundaries.
#[test]
fn test_copy_size_boundaries() {
    for &copy_len in &[8usize, 255, 256, 1000, 65535, 65536] {
        let base = vec![0xABu8; copy_len + 100];
        // Build a target that matches exactly `copy_len` bytes from offset 50
        let target = base[50..50 + copy_len].to_vec();

        let delta = DeltaEncoder::encode(&base, &target);
        let decoded = DeltaDecoder::decode(&base, &delta, MAX_DELTA_OUTPUT_SIZE)
            .unwrap_or_else(|e| panic!("decode failed for copy_len={copy_len}: {e}"));

        assert_eq!(decoded, target, "roundtrip failed for copy_len={copy_len}");

        // Verify estimate matches
        let estimated = DeltaEncoder::estimate_delta_size(&base, &target);
        assert_eq!(
            estimated,
            delta.len(),
            "estimate mismatch for copy_len={copy_len}"
        );
    }
}

/// Test that small objects (< 1024 bytes) use the lower match threshold.
#[test]
fn test_small_object_adaptive_match_length() {
    // Create base and target with an 8-byte match (below the old 16-byte threshold)
    let mut base = vec![0u8; 100];
    base[10..18].copy_from_slice(b"MATCHME!");

    let mut target = vec![1u8; 50]; // Different fill
    target[20..28].copy_from_slice(b"MATCHME!");

    let delta = DeltaEncoder::encode(&base, &target);
    let decoded = DeltaDecoder::decode(&base, &delta, MAX_DELTA_OUTPUT_SIZE).unwrap();
    assert_eq!(decoded, target);

    // The delta should use a copy instruction for the 8-byte match,
    // making it smaller than the target itself
    assert!(
        delta.len() < target.len(),
        "delta ({}) should be smaller than target ({}) with 8-byte match",
        delta.len(),
        target.len()
    );
}

/// Test encode_with_index produces identical results to encode.
#[test]
fn test_encode_with_index_matches_encode() {
    let base = b"Hello, World! This is a test of the delta compression system.".repeat(5);
    let target =
        b"Hello, World! This is a MODIFIED test of the delta compression system.".repeat(5);

    let direct = DeltaEncoder::encode(&base, &target);
    let index = DeltaEncoder::build_index(&base);
    let with_index = DeltaEncoder::encode_with_index(&index, &base, &target);

    assert_eq!(direct, with_index);
}

/// Test estimate_delta_size_with_index produces identical results.
#[test]
fn test_estimate_with_index_matches_estimate() {
    let base = b"Hello, World! This is a test of the delta compression system.".repeat(5);
    let target =
        b"Hello, World! This is a MODIFIED test of the delta compression system.".repeat(5);

    let direct = DeltaEncoder::estimate_delta_size(&base, &target);
    let index = DeltaEncoder::build_index(&base);
    let with_index = DeltaEncoder::estimate_delta_size_with_index(&index, &base, &target);

    assert_eq!(direct, with_index);
}