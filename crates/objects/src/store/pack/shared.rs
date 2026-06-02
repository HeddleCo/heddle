// SPDX-License-Identifier: Apache-2.0
use super::{ObjectType, varint};
use crate::{
    object::{ChangeId, ContentHash},
    store::{Result, StoreError, compression::CompressionConfig},
};

pub const PACK_CHECKSUM_LEN: usize = 32;
pub const MAX_PACK_OBJECT_OUTPUT_SIZE: usize = 1024 * 1024 * 1024;
#[cfg(feature = "zstd")]
pub(super) const PACK_DECOMPRESSION_INITIAL_CAP: usize = 4 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Ord, PartialOrd)]
pub enum PackObjectId {
    Hash(ContentHash),
    ChangeId(ChangeId),
}

impl PackObjectId {
    pub fn encode_tagged(self, buf: &mut Vec<u8>) {
        match self {
            Self::Hash(hash) => {
                buf.push(0);
                buf.extend_from_slice(hash.as_bytes());
            }
            Self::ChangeId(change_id) => {
                buf.push(1);
                buf.extend_from_slice(change_id.as_bytes());
            }
        }
    }

    pub fn decode_tagged(data: &[u8]) -> Result<(Self, usize)> {
        let Some(tag) = data.first().copied() else {
            return Err(StoreError::InvalidObject(
                "missing pack object id tag".to_string(),
            ));
        };
        match tag {
            0 => {
                if data.len() < 33 {
                    return Err(StoreError::InvalidObject(
                        "hash pack object id truncated".to_string(),
                    ));
                }
                let hash = ContentHash::from_bytes(data[1..33].try_into().map_err(|_| {
                    StoreError::InvalidObject("invalid hash id length".to_string())
                })?);
                Ok((Self::Hash(hash), 33))
            }
            1 => {
                if data.len() < 17 {
                    return Err(StoreError::InvalidObject(
                        "change id pack object id truncated".to_string(),
                    ));
                }
                let change_id = ChangeId::from_bytes(data[1..17].try_into().map_err(|_| {
                    StoreError::InvalidObject("invalid change id length".to_string())
                })?);
                Ok((Self::ChangeId(change_id), 17))
            }
            _ => Err(StoreError::InvalidObject(format!(
                "unknown pack object id tag {tag}"
            ))),
        }
    }
}

#[derive(Debug, Clone)]
pub struct PackObjectRecord {
    pub id: PackObjectId,
    pub obj_type: ObjectType,
    pub data: Vec<u8>,
    pub delta_base: Option<PackObjectId>,
    pub path_hint: Option<String>,
}

#[derive(Debug, Clone, Copy)]
pub struct PackContainerSpec {
    pub magic: &'static [u8; 4],
    pub version: u32,
}

#[derive(Debug, Clone)]
pub struct PackEntryHeader {
    pub id: PackObjectId,
    pub obj_type: ObjectType,
    pub uncompressed_size: usize,
    pub compressed_size: usize,
    pub delta_base: Option<PackObjectId>,
    pub header_len: usize,
}

pub fn write_container_header(buf: &mut Vec<u8>, spec: PackContainerSpec, count: u64) {
    buf.extend_from_slice(spec.magic);
    buf.extend_from_slice(&spec.version.to_be_bytes());
    buf.extend_from_slice(&count.to_be_bytes());
}

pub fn verify_container(data: &[u8], spec: PackContainerSpec) -> Result<(u64, usize, usize)> {
    if data.len() < 16 + PACK_CHECKSUM_LEN {
        return Err(StoreError::InvalidObject("Pack too short".to_string()));
    }
    if &data[..4] != spec.magic {
        return Err(StoreError::InvalidObject("Invalid pack magic".to_string()));
    }
    let version = u32::from_be_bytes([data[4], data[5], data[6], data[7]]);
    if version != spec.version {
        return Err(StoreError::InvalidObject(format!(
            "Unsupported pack version: {}",
            version
        )));
    }

    let content_end = data.len() - PACK_CHECKSUM_LEN;
    let content = &data[..content_end];
    let stored_checksum = &data[content_end..];
    let computed_checksum = blake3::hash(content);
    if computed_checksum.as_bytes() != stored_checksum {
        return Err(StoreError::InvalidObject(
            "Pack checksum mismatch".to_string(),
        ));
    }

    let count = u64::from_be_bytes([
        data[8], data[9], data[10], data[11], data[12], data[13], data[14], data[15],
    ]);
    Ok((count, 16, content_end))
}

pub fn append_container_checksum(buf: &mut Vec<u8>) {
    let checksum = blake3::hash(buf);
    buf.extend_from_slice(checksum.as_bytes());
}

pub fn encode_tagged_entry(
    buf: &mut Vec<u8>,
    record: &PackObjectRecord,
    stored_type: ObjectType,
    compressed: &[u8],
) -> Result<()> {
    encode_tagged_entry_parts(
        buf,
        record.id,
        stored_type,
        record.data.len(),
        record.delta_base,
        compressed,
    )
}

pub fn encode_tagged_entry_parts(
    buf: &mut Vec<u8>,
    id: PackObjectId,
    stored_type: ObjectType,
    uncompressed_size: usize,
    delta_base: Option<PackObjectId>,
    compressed: &[u8],
) -> Result<()> {
    id.encode_tagged(buf);
    varint::encode_type_and_size(stored_type, uncompressed_size as u64, buf);
    varint::encode_varint(compressed.len() as u64, buf);
    if stored_type == ObjectType::Delta {
        let Some(base) = delta_base else {
            return Err(StoreError::InvalidObject(
                "Delta entry missing base id".to_string(),
            ));
        };
        base.encode_tagged(buf);
    }
    buf.extend_from_slice(compressed);
    Ok(())
}

pub fn decode_tagged_entry_header(data: &[u8]) -> Result<PackEntryHeader> {
    let (id, id_len) = PackObjectId::decode_tagged(data)?;
    let (obj_type, uncompressed_size, type_len) = varint::decode_type_and_size(&data[id_len..])
        .ok_or_else(|| StoreError::InvalidObject("Truncated type+size varint".to_string()))?;
    let varint_start = id_len + type_len;
    let (compressed_size, comp_len) = varint::decode_varint(&data[varint_start..])
        .ok_or_else(|| StoreError::InvalidObject("Truncated compressed_size varint".to_string()))?;
    let mut header_len = varint_start + comp_len;

    let delta_base = if obj_type == ObjectType::Delta {
        let (base, base_len) = PackObjectId::decode_tagged(&data[header_len..])?;
        header_len += base_len;
        Some(base)
    } else {
        None
    };

    Ok(PackEntryHeader {
        id,
        obj_type,
        uncompressed_size: uncompressed_size as usize,
        compressed_size: compressed_size as usize,
        delta_base,
        header_len,
    })
}

pub fn try_decode_tagged_entry_header(data: &[u8]) -> Result<Option<PackEntryHeader>> {
    let Some(tag) = data.first().copied() else {
        return Ok(None);
    };

    let (id, id_len) =
        match tag {
            0 => {
                if data.len() < 33 {
                    return Ok(None);
                }
                let hash = ContentHash::from_bytes(data[1..33].try_into().map_err(|_| {
                    StoreError::InvalidObject("invalid hash id length".to_string())
                })?);
                (PackObjectId::Hash(hash), 33)
            }
            1 => {
                if data.len() < 17 {
                    return Ok(None);
                }
                let change_id = ChangeId::from_bytes(data[1..17].try_into().map_err(|_| {
                    StoreError::InvalidObject("invalid change id length".to_string())
                })?);
                (PackObjectId::ChangeId(change_id), 17)
            }
            _ => {
                return Err(StoreError::InvalidObject(format!(
                    "unknown pack object id tag {tag}"
                )));
            }
        };

    let Some((obj_type, uncompressed_size, type_len)) =
        varint::decode_type_and_size(&data[id_len..])
    else {
        return Ok(None);
    };
    let varint_start = id_len + type_len;
    let Some((compressed_size, comp_len)) = varint::decode_varint(&data[varint_start..]) else {
        return Ok(None);
    };
    let mut header_len = varint_start + comp_len;

    let delta_base = if obj_type == ObjectType::Delta {
        let Some(base_tag) = data.get(header_len).copied() else {
            return Ok(None);
        };
        let (base, base_len) = match base_tag {
            0 => {
                let end = header_len + 33;
                if data.len() < end {
                    return Ok(None);
                }
                let hash = ContentHash::from_bytes(data[header_len + 1..end].try_into().map_err(
                    |_| StoreError::InvalidObject("invalid hash id length".to_string()),
                )?);
                (PackObjectId::Hash(hash), 33)
            }
            1 => {
                let end = header_len + 17;
                if data.len() < end {
                    return Ok(None);
                }
                let change_id =
                    ChangeId::from_bytes(data[header_len + 1..end].try_into().map_err(|_| {
                        StoreError::InvalidObject("invalid change id length".to_string())
                    })?);
                (PackObjectId::ChangeId(change_id), 17)
            }
            _ => {
                return Err(StoreError::InvalidObject(format!(
                    "unknown pack object id tag {base_tag}"
                )));
            }
        };
        header_len += base_len;
        Some(base)
    } else {
        None
    };

    Ok(Some(PackEntryHeader {
        id,
        obj_type,
        uncompressed_size: uncompressed_size as usize,
        compressed_size: compressed_size as usize,
        delta_base,
        header_len,
    }))
}

pub fn compress_pack_payload(data: &[u8], config: &CompressionConfig) -> Result<Vec<u8>> {
    if !config.enabled || data.len() < config.min_size {
        return Ok(data.to_vec());
    }
    #[cfg(feature = "zstd")]
    {
        match zstd::encode_all(data, config.level) {
            Ok(compressed) if compressed.len() < data.len() => Ok(compressed),
            _ => Ok(data.to_vec()),
        }
    }
    #[cfg(not(feature = "zstd"))]
    {
        let _ = config;
        Ok(data.to_vec())
    }
}

pub fn decompress_pack_payload(data: &[u8], expected_size: usize) -> Result<Vec<u8>> {
    #[cfg(feature = "zstd")]
    {
        decompress_pack_payload_with_limit(data, expected_size, MAX_PACK_OBJECT_OUTPUT_SIZE)
    }
    #[cfg(not(feature = "zstd"))]
    {
        reject_pack_object_output_over_limit(expected_size, MAX_PACK_OBJECT_OUTPUT_SIZE)?;
        reject_pack_object_output_over_limit(data.len(), MAX_PACK_OBJECT_OUTPUT_SIZE)?;
        Ok(data.to_vec())
    }
}

#[cfg(feature = "zstd")]
pub(super) fn decompress_pack_payload_with_limit(
    data: &[u8],
    expected_size: usize,
    max_output_size: usize,
) -> Result<Vec<u8>> {
    use std::io::Read;

    // Pack objects may be raw blobs, so this bound must be materially
    // larger than the delta-output limit. It is also intentionally
    // above the protocol default and loose-compression cap, while
    // still bounding one untrusted pack record to a finite allocation.
    reject_pack_object_output_over_limit(expected_size, max_output_size)?;

    let mut decoder = zstd::stream::read::Decoder::new(data)
        .map_err(|e| StoreError::InvalidObject(format!("zstd decode init failed: {e}")))?;
    let capacity = initial_decompression_capacity(data.len(), expected_size, max_output_size);
    let mut buf = Vec::with_capacity(capacity);
    let mut chunk = [0u8; 8192];

    loop {
        let bytes_read = decoder
            .read(&mut chunk)
            .map_err(|e| StoreError::InvalidObject(format!("zstd decompression failed: {e}")))?;
        if bytes_read == 0 {
            break;
        }

        let next_len = buf.len().checked_add(bytes_read).ok_or_else(|| {
            StoreError::InvalidObject("Pack object output size overflows".to_string())
        })?;
        reject_pack_object_output_over_limit(next_len, max_output_size)?;
        buf.extend_from_slice(&chunk[..bytes_read]);
    }

    Ok(buf)
}

#[cfg(feature = "zstd")]
fn initial_decompression_capacity(
    compressed_len: usize,
    expected_size: usize,
    max_output_size: usize,
) -> usize {
    let hint = if expected_size > 0 {
        expected_size
    } else {
        compressed_len.saturating_mul(2)
    };
    hint.min(PACK_DECOMPRESSION_INITIAL_CAP)
        .min(max_output_size)
}

fn reject_pack_object_output_over_limit(size: usize, max: usize) -> Result<()> {
    if size > max {
        return Err(StoreError::InvalidObject(format!(
            "Pack object output size {size} exceeds max {max}"
        )));
    }
    Ok(())
}

pub fn has_zstd_magic(data: &[u8]) -> bool {
    data.len() >= 4 && data[..4] == [0x28, 0xB5, 0x2F, 0xFD]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tagged_pack_object_ids_round_trip() {
        let ids = [
            PackObjectId::Hash(ContentHash::compute(b"hash-object")),
            PackObjectId::ChangeId(ChangeId::generate()),
        ];

        for id in ids {
            let mut encoded = Vec::new();
            id.encode_tagged(&mut encoded);
            let (decoded, consumed) = PackObjectId::decode_tagged(&encoded).unwrap();
            assert_eq!(decoded, id);
            assert_eq!(consumed, encoded.len());
        }
    }

    #[test]
    fn tagged_entry_header_round_trips_mixed_identity() {
        let record = PackObjectRecord {
            id: PackObjectId::ChangeId(ChangeId::generate()),
            obj_type: ObjectType::State,
            data: vec![1, 2, 3, 4, 5],
            delta_base: None,
            path_hint: None,
        };

        let mut encoded = Vec::new();
        encode_tagged_entry(&mut encoded, &record, record.obj_type, &record.data).unwrap();
        let decoded = decode_tagged_entry_header(&encoded).unwrap();

        assert_eq!(decoded.id, record.id);
        assert_eq!(decoded.obj_type, ObjectType::State);
        assert_eq!(decoded.uncompressed_size, 5);
        assert_eq!(decoded.compressed_size, 5);
        assert_eq!(decoded.delta_base, None);
    }
}
