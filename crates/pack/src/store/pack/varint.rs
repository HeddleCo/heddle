// SPDX-License-Identifier: Apache-2.0
//! Variable-length integer encoding for compact pack headers.
//!
//! Uses Git-style MSB encoding: the high bit of each byte indicates
//! whether more bytes follow. The first byte packs the object type
//! in bits 4-6, leaving 4 low bits for size. Subsequent bytes
//! contribute 7 bits each.
//!
//! This gives 1-byte encoding for sizes up to 15, 2-byte for sizes up
//! to 2047, etc. — matching Git's pack format exactly.

use super::ObjectType;

/// Encode a type+size pair into the Git-style MSB varint format.
///
/// Layout of first byte: `CTTTSSS` where:
/// - C = continuation bit (1 = more bytes follow)
/// - TTT = object type (3 bits)
/// - SSSS = size low 4 bits
///
/// Subsequent bytes: `CSSSSSSS` (C = continuation, 7 bits of size each)
pub fn encode_type_and_size(obj_type: ObjectType, size: u64, buf: &mut Vec<u8>) {
    let type_bits = (obj_type as u8) & 0x07;
    let mut val = size;

    // First byte: type in bits 4-6, low 4 bits of size
    let low = (val & 0x0F) as u8;
    val >>= 4;

    if val == 0 {
        buf.push((type_bits << 4) | low);
    } else {
        buf.push(0x80 | (type_bits << 4) | low);
        // Remaining bytes: 7 bits of size each
        while val > 0x7F {
            buf.push(0x80 | (val & 0x7F) as u8);
            val >>= 7;
        }
        buf.push(val as u8);
    }
}

/// Decode a type+size pair from the Git-style MSB varint format.
///
/// Returns `(object_type, size, bytes_consumed)` or `None` if the data
/// is truncated or the type bits are invalid.
pub fn decode_type_and_size(data: &[u8]) -> Option<(ObjectType, u64, usize)> {
    if data.is_empty() {
        return None;
    }

    let first = data[0];
    let type_bits = (first >> 4) & 0x07;
    let obj_type = ObjectType::from_u8(type_bits)?;

    let mut size = (first & 0x0F) as u64;
    let mut shift = 4u32;
    let mut pos = 1;

    if first & 0x80 != 0 {
        loop {
            if pos >= data.len() {
                return None; // truncated
            }
            let byte = data[pos];
            pos += 1;
            size |= ((byte & 0x7F) as u64) << shift;
            shift += 7;
            if byte & 0x80 == 0 {
                break;
            }
            if shift > 63 {
                return None; // overflow
            }
        }
    }

    Some((obj_type, size, pos))
}

/// Encode a plain u64 as LEB128 varint (no type bits).
pub fn encode_varint(mut val: u64, buf: &mut Vec<u8>) {
    loop {
        let byte = (val & 0x7F) as u8;
        val >>= 7;
        if val == 0 {
            buf.push(byte);
            break;
        }
        buf.push(byte | 0x80);
    }
}

/// Decode a plain LEB128 varint. Returns `(value, bytes_consumed)`.
pub fn decode_varint(data: &[u8]) -> Option<(u64, usize)> {
    let mut val: u64 = 0;
    let mut shift = 0u32;

    for (i, &byte) in data.iter().enumerate() {
        val |= ((byte & 0x7F) as u64) << shift;
        shift += 7;
        if byte & 0x80 == 0 {
            return Some((val, i + 1));
        }
        if shift > 63 {
            return None; // overflow
        }
    }

    None // truncated
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_type_and_size_small() {
        // Size fits in 4 bits — should be 1 byte
        let mut buf = Vec::new();
        encode_type_and_size(ObjectType::Blob, 7, &mut buf);
        assert_eq!(buf.len(), 1);

        let (obj_type, size, consumed) = decode_type_and_size(&buf).unwrap();
        assert_eq!(obj_type, ObjectType::Blob);
        assert_eq!(size, 7);
        assert_eq!(consumed, 1);
    }

    #[test]
    fn test_type_and_size_medium() {
        // Size 256 — needs 2 bytes
        let mut buf = Vec::new();
        encode_type_and_size(ObjectType::Tree, 256, &mut buf);
        assert_eq!(buf.len(), 2);

        let (obj_type, size, consumed) = decode_type_and_size(&buf).unwrap();
        assert_eq!(obj_type, ObjectType::Tree);
        assert_eq!(size, 256);
        assert_eq!(consumed, 2);
    }

    #[test]
    fn test_type_and_size_large() {
        let mut buf = Vec::new();
        encode_type_and_size(ObjectType::Blob, 1_000_000, &mut buf);

        let (obj_type, size, consumed) = decode_type_and_size(&buf).unwrap();
        assert_eq!(obj_type, ObjectType::Blob);
        assert_eq!(size, 1_000_000);
        assert_eq!(consumed, buf.len());
    }

    #[test]
    fn test_type_and_size_zero() {
        let mut buf = Vec::new();
        encode_type_and_size(ObjectType::State, 0, &mut buf);
        assert_eq!(buf.len(), 1);

        let (obj_type, size, _) = decode_type_and_size(&buf).unwrap();
        assert_eq!(obj_type, ObjectType::State);
        assert_eq!(size, 0);
    }

    #[test]
    fn test_type_and_size_all_types() {
        for (type_val, obj_type) in [
            (0, ObjectType::Blob),
            (1, ObjectType::Tree),
            (2, ObjectType::State),
            (3, ObjectType::Action),
            (4, ObjectType::Delta),
            (5, ObjectType::StateAttachment),
            (6, ObjectType::SnapshotCommit),
        ] {
            let mut buf = Vec::new();
            encode_type_and_size(obj_type, 42, &mut buf);
            let (decoded_type, size, _) = decode_type_and_size(&buf).unwrap();
            assert_eq!(decoded_type, obj_type, "type {type_val} roundtrip failed");
            assert_eq!(size, 42);
        }
    }

    #[test]
    fn test_type_and_size_max_15_one_byte() {
        // 15 is max size for single-byte encoding (4 bits)
        let mut buf = Vec::new();
        encode_type_and_size(ObjectType::Blob, 15, &mut buf);
        assert_eq!(buf.len(), 1);
    }

    #[test]
    fn test_type_and_size_16_two_bytes() {
        // 16 overflows 4 bits → needs continuation
        let mut buf = Vec::new();
        encode_type_and_size(ObjectType::Blob, 16, &mut buf);
        assert_eq!(buf.len(), 2);
    }

    #[test]
    fn test_varint_roundtrip() {
        let test_values: Vec<u64> = vec![0, 1, 127, 128, 255, 16383, 16384, 1_000_000, u64::MAX];

        for val in test_values {
            let mut buf = Vec::new();
            encode_varint(val, &mut buf);
            let (decoded, consumed) = decode_varint(&buf).unwrap();
            assert_eq!(decoded, val, "varint roundtrip failed for {val}");
            assert_eq!(consumed, buf.len());
        }
    }

    #[test]
    fn test_varint_sizes() {
        // 0-127: 1 byte
        let mut buf = Vec::new();
        encode_varint(127, &mut buf);
        assert_eq!(buf.len(), 1);

        // 128-16383: 2 bytes
        buf.clear();
        encode_varint(128, &mut buf);
        assert_eq!(buf.len(), 2);

        // 16384+: 3 bytes
        buf.clear();
        encode_varint(16384, &mut buf);
        assert_eq!(buf.len(), 3);
    }

    #[test]
    fn test_decode_truncated() {
        // Continuation bit set but no more data
        assert!(decode_type_and_size(&[0x80]).is_none());
        assert!(decode_varint(&[0x80]).is_none());
    }

    #[test]
    fn test_decode_empty() {
        assert!(decode_type_and_size(&[]).is_none());
        assert!(decode_varint(&[]).is_none());
    }

    #[test]
    fn test_invalid_type_rejected() {
        // Type 7 is unassigned.
        let buf = [0x70]; // type = 7, size = 0
        assert!(decode_type_and_size(&buf).is_none());
    }
}
