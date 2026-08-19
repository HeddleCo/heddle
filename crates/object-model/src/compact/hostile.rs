// SPDX-License-Identifier: Apache-2.0

use super::{decode_blob_frame, decode_state_frame, decode_tree_frame};

/// Matches the packer writer split (`FRAME_LIMIT` in `objects`).
const MAX_COMPACT_COUNT: usize = 12 * 1024 * 1024;

#[test]
fn blob_frame_rejects_declared_count_above_object_cap() {
    let mut payload = Vec::new();
    put_u64(&mut payload, (MAX_COMPACT_COUNT + 1) as u64);
    let error = decode_blob_frame(&frame(b"HCB2", &payload)).unwrap_err();
    assert!(
        error.to_string().contains("exceeds maximum"),
        "hostile blob count must fail at the count gate before with_capacity, got {error}"
    );
}

#[test]
fn tree_entry_count_that_fits_remaining_bytes_still_fails_encoded_floor() {
    let mut payload = Vec::new();
    put_u64(&mut payload, 1);
    put_u64(&mut payload, 100);
    payload.resize(payload.len() + 100, 0);
    let error = decode_tree_frame(&frame(b"HCT1", &payload)).unwrap_err();
    assert!(
        error.to_string().contains("bytes per item"),
        "tree entry count that only fits a 1-byte remaining() check must fail before with_capacity, got {error}"
    );
}

#[test]
fn state_frame_count_that_fits_remaining_bytes_still_fails_column_floor() {
    let mut payload = Vec::new();
    put_u64(&mut payload, 200);
    payload.resize(payload.len() + 200, 0);
    let error = decode_state_frame(&frame(b"HCS1", &payload)).unwrap_err();
    assert!(
        error.to_string().contains("bytes per item"),
        "state count that only fits a 1-byte remaining() check must fail before blank_state materialization, got {error}"
    );
}

fn frame(magic: &[u8; 4], after_magic: &[u8]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(4 + after_magic.len() + 32);
    bytes.extend_from_slice(magic);
    bytes.extend_from_slice(after_magic);
    let digest = blake3::hash(&bytes);
    bytes.extend_from_slice(digest.as_bytes());
    bytes
}

fn put_u64(out: &mut Vec<u8>, mut value: u64) {
    while value >= 0x80 {
        out.push((value as u8) | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}
