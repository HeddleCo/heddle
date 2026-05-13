// SPDX-License-Identifier: Apache-2.0
//! Pack reader for extracting objects from packfiles.

use std::path::Path;

use super::{
    ObjectType, PackObjectId, PackObjectRecord, decompress_pack_payload, has_zstd_magic,
    pack_container_spec, pack_index::PackIndex, varint, verify_container,
};
use crate::{
    object::ContentHash,
    store::{Result, StoreError},
};

const MAX_PACK_DELTA_OUTPUT_SIZE: usize = crate::delta::MAX_DELTA_OUTPUT_SIZE;
const MAX_DELTA_CHAIN_DEPTH: usize = 50;

/// Pack reader for extracting objects.
pub struct PackReader {
    data: Vec<u8>,
    index: PackIndex,
    content_end: usize,
}

impl PackReader {
    /// Open a pack file.
    pub fn open(pack_path: &Path, index_path: &Path) -> Result<Self> {
        let pack_data = std::fs::read(pack_path)?;
        let index_data = std::fs::read(index_path)?;
        Self::from_bytes(pack_data, index_data)
    }

    pub fn from_bytes(pack_data: Vec<u8>, index_data: Vec<u8>) -> Result<Self> {
        let (_, _, content_end) = verify_container(&pack_data, pack_container_spec())?;
        let index = PackIndex::from_bytes(&index_data)?;
        Ok(Self {
            data: pack_data,
            index,
            content_end,
        })
    }

    /// List all object ids in this pack.
    pub fn list_ids(&self) -> Vec<PackObjectId> {
        self.index.ids()
    }

    pub fn list_hashes(&self) -> Vec<ContentHash> {
        self.list_ids()
            .into_iter()
            .filter_map(|id| match id {
                PackObjectId::Hash(hash) => Some(hash),
                PackObjectId::ChangeId(_) => None,
            })
            .collect()
    }

    pub fn has_object(&self, id: &PackObjectId) -> bool {
        self.index.find(id).is_some()
    }

    /// Get an object from the pack.
    pub fn get_object(&self, id: &PackObjectId) -> Result<Option<(ObjectType, Vec<u8>)>> {
        let offset = match self.index.find(id) {
            Some(offset) => offset,
            None => return Ok(None),
        };

        let record = self.read_record_at_depth(offset as usize, 0)?;
        Ok(Some((record.obj_type, record.data)))
    }

    pub fn get_hashed_object(&self, hash: &ContentHash) -> Result<Option<(ObjectType, Vec<u8>)>> {
        self.get_object(&PackObjectId::Hash(*hash))
    }

    /// Read just the type+size header for an object without
    /// decompressing its payload. Returns `Ok(None)` when the object
    /// isn't in this pack.
    ///
    /// For non-delta entries this is one varint decode at the indexed
    /// offset — much cheaper than `get_object`. Delta entries fall
    /// back to a full read because their *resolved* size requires
    /// chasing the base; in practice deltas are rare in the directory
    /// listing hot path so the fallback is acceptable.
    pub fn get_hashed_object_size(&self, hash: &ContentHash) -> Result<Option<u64>> {
        let id = PackObjectId::Hash(*hash);
        let Some(offset) = self.index.find(&id) else {
            return Ok(None);
        };
        let offset = offset as usize;
        if offset >= self.content_end {
            return Err(StoreError::InvalidObject(
                "Entry offset out of bounds".to_string(),
            ));
        }
        let (_, id_len) = PackObjectId::decode_tagged(&self.data[offset..])?;
        let header_start = offset + id_len;
        let (obj_type, uncompressed_size, _type_len) =
            super::varint::decode_type_and_size(&self.data[header_start..]).ok_or_else(|| {
                StoreError::InvalidObject("Truncated type+size varint".to_string())
            })?;
        if obj_type == ObjectType::Delta {
            // Delta entries record the *resolved* output size in the
            // type+size varint already (see `read_record_at_depth`'s
            // size-mismatch check), so we can still return without
            // decompressing the payload.
            return Ok(Some(uncompressed_size));
        }
        Ok(Some(uncompressed_size))
    }

    fn read_record_at_depth(&self, offset: usize, depth: usize) -> Result<PackObjectRecord> {
        if offset >= self.content_end {
            return Err(StoreError::InvalidObject(
                "Entry offset out of bounds".to_string(),
            ));
        }

        let (id, id_len) = PackObjectId::decode_tagged(&self.data[offset..])?;
        let header_start = offset + id_len;

        let (obj_type, uncompressed_size, type_len) =
            varint::decode_type_and_size(&self.data[header_start..]).ok_or_else(|| {
                StoreError::InvalidObject("Truncated type+size varint".to_string())
            })?;
        let uncompressed_size = uncompressed_size as usize;

        let varint_start = header_start + type_len;
        let (compressed_size, comp_len) = varint::decode_varint(&self.data[varint_start..])
            .ok_or_else(|| {
                StoreError::InvalidObject("Truncated compressed_size varint".to_string())
            })?;
        let compressed_size = compressed_size as usize;

        let mut data_start = varint_start + comp_len;

        // Delta entries carry a tagged base id in pack v2.
        let base_id = if obj_type == ObjectType::Delta {
            let (base_id, base_len) = PackObjectId::decode_tagged(&self.data[data_start..])?;
            data_start += base_len;
            Some(base_id)
        } else {
            None
        };

        let data_end = data_start + compressed_size;
        if data_end > self.content_end {
            return Err(StoreError::InvalidObject(
                "Entry data out of bounds".to_string(),
            ));
        }

        let stored_data = &self.data[data_start..data_end];

        // Raw zstd (no wrapper). For non-delta entries, decompress
        // if sizes differ. For delta entries, the stored data IS the delta
        // payload (possibly zstd-compressed); check for zstd magic.
        let decompressed = if obj_type == ObjectType::Delta {
            if has_zstd_magic(stored_data) {
                decompress_pack_payload(stored_data, 0)?
            } else {
                stored_data.to_vec()
            }
        } else if compressed_size != uncompressed_size {
            decompress_pack_payload(stored_data, uncompressed_size)?
        } else {
            stored_data.to_vec()
        };

        let (resolved_type, final_data) = if obj_type == ObjectType::Delta {
            self.read_delta_record(base_id, &decompressed, uncompressed_size, depth)?
        } else {
            (obj_type, decompressed)
        };

        if final_data.len() != uncompressed_size {
            return Err(StoreError::InvalidObject(format!(
                "Size mismatch: expected {}, got {}",
                uncompressed_size,
                final_data.len()
            )));
        }

        Ok(PackObjectRecord {
            id,
            obj_type: resolved_type,
            data: final_data,
            delta_base: None,
            path_hint: None,
        })
    }

    fn read_delta_record(
        &self,
        base_id: Option<PackObjectId>,
        delta: &[u8],
        uncompressed_size: usize,
        depth: usize,
    ) -> Result<(ObjectType, Vec<u8>)> {
        if depth > MAX_DELTA_CHAIN_DEPTH {
            return Err(StoreError::InvalidObject(format!(
                "Delta chain depth {} exceeds max {}",
                depth, MAX_DELTA_CHAIN_DEPTH
            )));
        }

        if uncompressed_size > MAX_PACK_DELTA_OUTPUT_SIZE {
            return Err(StoreError::InvalidObject(format!(
                "Delta output size {} exceeds max {}",
                uncompressed_size, MAX_PACK_DELTA_OUTPUT_SIZE
            )));
        }

        let base_hash = Self::require_delta_base_hash(base_id)?;
        let base_offset = self
            .index
            .find(&PackObjectId::Hash(base_hash))
            .ok_or_else(|| StoreError::NotFound(base_hash.to_string()))?;
        let base_record = self.read_record_at_depth(base_offset as usize, depth + 1)?;
        let base_type = base_record.obj_type;
        let base_data = base_record.data;

        let decoded = crate::delta::DeltaDecoder::decode(&base_data, delta, uncompressed_size)
            .map_err(|error| StoreError::InvalidObject(format!("Delta decode failed: {error}")))?;

        Ok((base_type, decoded))
    }

    fn require_delta_base_hash(base_id: Option<PackObjectId>) -> Result<ContentHash> {
        match base_id {
            Some(PackObjectId::Hash(hash)) => Ok(hash),
            Some(PackObjectId::ChangeId(_)) => Err(StoreError::InvalidObject(
                "pack delta base must be hash-backed content".into(),
            )),
            None => Err(StoreError::InvalidObject(
                "pack object type is Delta but base hash is missing".into(),
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::PackReader;
    use crate::store::StoreError;

    #[test]
    fn test_require_delta_base_hash_rejects_missing_hash() {
        let error =
            PackReader::require_delta_base_hash(None).expect_err("missing hash should fail");

        assert!(
            matches!(error, StoreError::InvalidObject(message) if message == "pack object type is Delta but base hash is missing")
        );
    }
}