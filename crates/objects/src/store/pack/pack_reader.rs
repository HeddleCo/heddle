// SPDX-License-Identifier: Apache-2.0
//! Pack reader for extracting objects from packfiles.

use std::path::Path;

use bytes::Bytes;

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
///
/// `data` is a refcounted [`Bytes`] view of the pack file. For
/// uncompressed entries we hand back a zero-copy `Bytes::slice` into
/// this buffer — no per-blob memcpy, no per-blob allocation. Mmap-
/// backed `Bytes` (via [`Bytes::from_owner`] on the
/// `memmap2::Mmap`) survives across reads without copying the
/// whole pack into the heap.
enum PackData<'a> {
    Borrowed(&'a [u8]),
    Owned(Bytes),
}

impl<'a> PackData<'a> {
    fn as_slice(&self) -> &[u8] {
        match self {
            Self::Borrowed(data) => data,
            Self::Owned(data) => data,
        }
    }

    fn slice(&self, range: std::ops::Range<usize>) -> Bytes {
        match self {
            Self::Borrowed(data) => Bytes::copy_from_slice(&data[range]),
            Self::Owned(data) => data.slice(range),
        }
    }
}

pub struct PackReader<'a> {
    data: PackData<'a>,
    index: PackIndex,
    content_end: usize,
}

impl PackReader<'static> {
    /// Open a pack file. mmap-backed when the pack is large enough
    /// to benefit (the same threshold the loose-blob path uses for
    /// its own mmap decision); read-into-heap otherwise.
    pub fn open(pack_path: &Path, index_path: &Path) -> Result<Self> {
        let pack_bytes = crate::store::fs::read_file_bytes_for_pack(pack_path)?;
        let index_data = std::fs::read(index_path)?;
        let (_, _, content_end) = verify_container(&pack_bytes, pack_container_spec())?;
        let index = PackIndex::from_bytes(&index_data)?;
        Ok(Self {
            data: PackData::Owned(pack_bytes),
            index,
            content_end,
        })
    }

    pub fn from_bytes(pack_data: impl Into<Bytes>, index_data: impl AsRef<[u8]>) -> Result<Self> {
        let pack_data = pack_data.into();
        let (_, _, content_end) = verify_container(&pack_data, pack_container_spec())?;
        let index = PackIndex::from_bytes(index_data.as_ref())?;
        Ok(Self {
            data: PackData::Owned(pack_data),
            index,
            content_end,
        })
    }
}

impl<'a> PackReader<'a> {
    pub fn from_slice(pack_data: &'a [u8], index_data: impl AsRef<[u8]>) -> Result<Self> {
        let (_, _, content_end) = verify_container(pack_data, pack_container_spec())?;
        let index = PackIndex::from_bytes(index_data.as_ref())?;
        Ok(Self {
            data: PackData::Borrowed(pack_data),
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
    ///
    /// Verifies that the tagged id at the indexed offset matches
    /// `id` before returning. A stale `.idx` file (e.g., overwritten
    /// in place after a pack rebuild) can otherwise route a request
    /// for hash `A` to a record physically located at hash `B`'s
    /// offset — same shape, different content, no error signal.
    /// This cheap 32-byte id comparison catches that without paying
    /// a full content-hash recompute on every read; corruption
    /// strictly *inside* the record body is a separate failure mode
    /// surfaced via the consumer-side hash verify (see
    /// `FsStore::loose_blob_path` for the blob equivalent).
    pub fn get_object(&self, id: &PackObjectId) -> Result<Option<(ObjectType, Vec<u8>)>> {
        let offset = match self.index.find(id) {
            Some(offset) => checked_index_offset(offset)?,
            None => return Ok(None),
        };

        let record = self.read_record_at_depth(offset, 0)?;
        verify_record_id_matches(id, &record.id)?;
        Ok(Some((record.obj_type, record.data)))
    }

    pub fn get_hashed_object(&self, hash: &ContentHash) -> Result<Option<(ObjectType, Vec<u8>)>> {
        self.get_object(&PackObjectId::Hash(*hash))
    }

    /// Zero-copy fast path: when the entry is non-delta and stored
    /// uncompressed, returns `Bytes::slice` into the pack's
    /// (mmap-backed) buffer — no allocation, no memcpy. Compressed
    /// or delta entries fall back to `get_object` and wrap the
    /// resulting `Vec<u8>` in a `Bytes` (one Arc, no body copy).
    ///
    /// Use this from the hot read path. The 10 MB benchmark gap
    /// between the mount and vanilla FS at the 1 MB+ tier is the
    /// per-blob memcpy this method eliminates.
    pub fn get_object_bytes(&self, id: &PackObjectId) -> Result<Option<(ObjectType, Bytes)>> {
        let Some(offset) = self.index.find(id) else {
            return Ok(None);
        };
        let offset = checked_index_offset(offset)?;
        if offset >= self.content_end {
            return Err(StoreError::InvalidObject(
                "Entry offset out of bounds".to_string(),
            ));
        }

        // Verify the tagged id at the indexed offset matches the
        // requested id — guards against stale-index misrouting (see
        // `get_object` for the long-form rationale). 32-byte
        // compare; cheaper than the size+varint decode that follows.
        let (record_id, id_len) = PackObjectId::decode_tagged(self.content_from(offset)?)?;
        verify_record_id_matches(id, &record_id)?;
        let header_start = checked_index_add(offset, id_len, "record header start")?;
        let (obj_type, uncompressed_size, type_len) =
            varint::decode_type_and_size(self.content_from(header_start)?).ok_or_else(|| {
                StoreError::InvalidObject("Truncated type+size varint".to_string())
            })?;
        let uncompressed_size = checked_decoded_size("uncompressed_size", uncompressed_size)?;
        let varint_start = checked_index_add(header_start, type_len, "compressed_size start")?;
        let (compressed_size, comp_len) = varint::decode_varint(self.content_from(varint_start)?)
            .ok_or_else(truncated_compressed_size_varint)?;
        let compressed_size = checked_decoded_size("compressed_size", compressed_size)?;

        // Fast path: non-delta entry stored uncompressed. The most
        // common shape for snapshot-time packs (the builder skips
        // the delta search for unrelated blobs).
        if obj_type != ObjectType::Delta && compressed_size == uncompressed_size {
            let data_start = checked_index_add(varint_start, comp_len, "entry data start")?;
            let data_end = checked_data_end(data_start, compressed_size, self.content_end)?;
            return Ok(Some((obj_type, self.data.slice(data_start..data_end))));
        }

        // Slow path: defer to the full record reader (it handles
        // decompression + delta chains) and Bytes-wrap the Vec.
        // Bytes::from(Vec) is a single Arc allocation, no body copy.
        let record = self.read_record_at_depth(offset, 0)?;
        Ok(Some((record.obj_type, Bytes::from(record.data))))
    }

    pub fn get_hashed_object_bytes(
        &self,
        hash: &ContentHash,
    ) -> Result<Option<(ObjectType, Bytes)>> {
        self.get_object_bytes(&PackObjectId::Hash(*hash))
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
        let offset = checked_index_offset(offset)?;
        if offset >= self.content_end {
            return Err(StoreError::InvalidObject(
                "Entry offset out of bounds".to_string(),
            ));
        }
        let (record_id, id_len) = PackObjectId::decode_tagged(self.content_from(offset)?)?;
        verify_record_id_matches(&id, &record_id)?;
        let header_start = checked_index_add(offset, id_len, "record header start")?;
        let (obj_type, uncompressed_size, _type_len) = super::varint::decode_type_and_size(
            self.content_from(header_start)?,
        )
        .ok_or_else(|| StoreError::InvalidObject("Truncated type+size varint".to_string()))?;
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

        let (id, id_len) = PackObjectId::decode_tagged(self.content_from(offset)?)?;
        let header_start = checked_index_add(offset, id_len, "record header start")?;

        let (obj_type, uncompressed_size, type_len) =
            varint::decode_type_and_size(self.content_from(header_start)?).ok_or_else(|| {
                StoreError::InvalidObject("Truncated type+size varint".to_string())
            })?;
        let uncompressed_size = checked_decoded_size("uncompressed_size", uncompressed_size)?;

        let varint_start = checked_index_add(header_start, type_len, "compressed_size start")?;
        let (compressed_size, comp_len) = varint::decode_varint(self.content_from(varint_start)?)
            .ok_or_else(truncated_compressed_size_varint)?;
        let compressed_size = checked_decoded_size("compressed_size", compressed_size)?;

        let mut data_start = checked_index_add(varint_start, comp_len, "entry data start")?;

        // Delta entries carry a tagged base id in pack v2.
        let base_id = if obj_type == ObjectType::Delta {
            let (base_id, base_len) = PackObjectId::decode_tagged(self.content_from(data_start)?)?;
            data_start = checked_index_add(data_start, base_len, "delta data start")?;
            Some(base_id)
        } else {
            None
        };

        let data_end = checked_data_end(data_start, compressed_size, self.content_end)?;

        let stored_data = &self.data.as_slice()[data_start..data_end];

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
        let base_offset = checked_index_offset(base_offset)?;
        let base_record = self.read_record_at_depth(base_offset, depth + 1)?;
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

    fn content_from(&self, offset: usize) -> Result<&[u8]> {
        if offset > self.content_end {
            return Err(StoreError::InvalidObject(
                "Entry header out of bounds".to_string(),
            ));
        }
        Ok(&self.data.as_slice()[offset..self.content_end])
    }
}

fn checked_index_offset(offset: u64) -> Result<usize> {
    usize::try_from(offset)
        .map_err(|_| StoreError::InvalidObject("Entry offset exceeds platform limits".to_string()))
}

fn checked_decoded_size(field: &str, size: u64) -> Result<usize> {
    usize::try_from(size)
        .map_err(|_| StoreError::InvalidObject(format!("Decoded {field} exceeds platform limits")))
}

fn checked_index_add(start: usize, len: usize, field: &str) -> Result<usize> {
    start.checked_add(len).ok_or_else(|| {
        StoreError::InvalidObject(format!("{field} offset overflows platform limits"))
    })
}

fn checked_data_end(
    data_start: usize,
    compressed_size: usize,
    content_end: usize,
) -> Result<usize> {
    let data_end = data_start.checked_add(compressed_size).ok_or_else(|| {
        StoreError::InvalidObject("Entry data range overflows platform limits".to_string())
    })?;
    if data_end > content_end {
        return Err(StoreError::InvalidObject(
            "Entry data out of bounds".to_string(),
        ));
    }
    Ok(data_end)
}

fn truncated_compressed_size_varint() -> StoreError {
    StoreError::InvalidObject("Truncated compressed_size varint".to_string())
}

/// Reject a record whose tagged id at the indexed offset doesn't
/// match the id the caller asked for. The pack format stores its
/// records `[tagged_id, type+size, compressed_size, payload]` so the
/// tagged id is the cheapest available authenticator of "we landed
/// on the right record"; a stale or hand-edited `.idx` that points
/// at the *wrong* record produces a mismatch here and we surface it
/// as a real error instead of silently routing the caller to whatever
/// bytes happened to be at the bad offset.
fn verify_record_id_matches(requested: &PackObjectId, found: &PackObjectId) -> Result<()> {
    if requested == found {
        return Ok(());
    }
    Err(StoreError::InvalidObject(format!(
        "pack index routed lookup for {requested:?} to record tagged {found:?} \
         — index is stale or corrupt; the loose-store path will re-promote on \
         the next read"
    )))
}

#[cfg(test)]
mod tests {
    use super::{PackObjectId, PackReader, verify_record_id_matches};
    use crate::{object::ContentHash, store::StoreError};

    #[test]
    fn test_require_delta_base_hash_rejects_missing_hash() {
        let error =
            PackReader::require_delta_base_hash(None).expect_err("missing hash should fail");

        assert!(
            matches!(error, StoreError::InvalidObject(message) if message == "pack object type is Delta but base hash is missing")
        );
    }

    #[test]
    fn verify_record_id_matches_accepts_identical_ids() {
        let id = PackObjectId::Hash(ContentHash::from_bytes([7u8; 32]));
        verify_record_id_matches(&id, &id).expect("matching ids must verify");
    }

    #[test]
    fn verify_record_id_matches_rejects_mismatched_ids() {
        let asked = PackObjectId::Hash(ContentHash::from_bytes([7u8; 32]));
        let found = PackObjectId::Hash(ContentHash::from_bytes([8u8; 32]));
        let error = verify_record_id_matches(&asked, &found)
            .expect_err("mismatched record id must error rather than silently route");
        assert!(
            matches!(&error, StoreError::InvalidObject(message) if message.contains("stale or corrupt")),
            "stale-index mismatch must surface as InvalidObject with the diagnostic phrase, got: {error:?}",
        );
    }
}
