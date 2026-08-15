// SPDX-License-Identifier: Apache-2.0
//! Pack reader for extracting objects from packfiles.

#[cfg(test)]
use std::sync::atomic::{AtomicUsize, Ordering};
use std::{
    collections::{BTreeSet, HashMap, HashSet},
    fs::File,
    io::Read,
    path::Path,
};

use bytes::Bytes;
use heddle_format::delta::{DeltaDecoder, MAX_DELTA_OUTPUT_SIZE};

use super::{
    ObjectType, PackLogicalId, PackObjectId, PackObjectRecord, PackRepresentationHash,
    append_container_checksum, decode_tagged_entry_header, decompress_pack_payload, has_zstd_magic,
    pack_container_spec, pack_identity::LogicalIdBuilder, pack_index::PackIndex, varint,
    verify_supported_container, verify_supported_container_layout, write_container_header,
};
use crate::{
    object::ContentHash,
    store::{Result, StoreError},
};

const MAX_PACK_DELTA_OUTPUT_SIZE: usize = MAX_DELTA_OUTPUT_SIZE;
const MAX_DELTA_CHAIN_DEPTH: usize = 50;
const MMAP_THRESHOLD_BYTES: u64 = 256 * 1024;

type DecodedCompactObject = (PackObjectId, ObjectType, Vec<u8>);
type DecodedCompactObjects = Vec<DecodedCompactObject>;

/// Physical read tier for an indexed pack object.
///
/// Hot records have one independently addressable record per logical object.
/// Solid-frame records share one payload across several logical objects and
/// therefore require a frame decompression on a cache-cold read.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum PackReadTier {
    /// Independently addressable, random-access record.
    Hot,
    /// Compact payload shared by several tree or state ids.
    SolidFrame,
}

fn read_file_bytes_for_pack(path: &Path) -> Result<Bytes> {
    let file = File::open(path)?;
    let len = file.metadata()?.len();
    if len == 0 {
        return Ok(Bytes::new());
    }
    if len >= MMAP_THRESHOLD_BYTES {
        let mmap = unsafe { memmap2::MmapOptions::new().map(&file)? };
        if mmap.len() != checked_file_len_to_usize(len)? {
            return Err(StoreError::InvalidObject(
                "pack file size changed during memory mapping".to_string(),
            ));
        }
        return Ok(Bytes::from_owner(mmap));
    }
    let mut data = Vec::with_capacity(checked_file_len_to_usize(len)?);
    let mut reader = file;
    reader.read_to_end(&mut data)?;
    Ok(Bytes::from(data))
}

fn checked_file_len_to_usize(len: u64) -> Result<usize> {
    usize::try_from(len).map_err(|_| {
        StoreError::InvalidObject(format!("file length {len} exceeds platform limits"))
    })
}

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
    aliased_offsets: HashSet<u64>,
    content_end: usize,
    #[cfg(test)]
    compact_frame_reads: AtomicUsize,
}

#[derive(Debug, Clone)]
pub struct EncodedPackSubset {
    pub pack_data: Vec<u8>,
    pub index_data: Vec<u8>,
    pub encoded_bytes_copied: u64,
}

impl PackReader<'static> {
    /// Open a pack file. mmap-backed when the pack is large enough
    /// to benefit (the same threshold the loose-blob path uses for
    /// its own mmap decision); read-into-heap otherwise.
    pub fn open(pack_path: &Path, index_path: &Path) -> Result<Self> {
        Self::open_with_verification(pack_path, index_path, true)
    }

    pub(super) fn open_lazy(pack_path: &Path, index_path: &Path) -> Result<Self> {
        Self::open_with_verification(pack_path, index_path, false)
    }

    fn open_with_verification(
        pack_path: &Path,
        index_path: &Path,
        verify_checksum: bool,
    ) -> Result<Self> {
        let pack_bytes = read_file_bytes_for_pack(pack_path)?;
        let index_data = read_file_bytes_for_pack(index_path)?;
        let (_, _, content_end) = if verify_checksum {
            verify_supported_container(&pack_bytes)?
        } else {
            verify_supported_container_layout(&pack_bytes)?
        };
        let index = PackIndex::from_owned_bytes(index_data)?;
        let aliased_offsets = index.aliased_offsets()?;
        Ok(Self {
            data: PackData::Owned(pack_bytes),
            index,
            aliased_offsets,
            content_end,
            #[cfg(test)]
            compact_frame_reads: AtomicUsize::new(0),
        })
    }

    pub fn from_bytes(pack_data: impl Into<Bytes>, index_data: impl AsRef<[u8]>) -> Result<Self> {
        let pack_data = pack_data.into();
        let (_, _, content_end) = verify_supported_container(&pack_data)?;
        let index = PackIndex::from_bytes(index_data.as_ref())?;
        let aliased_offsets = index.aliased_offsets()?;
        Ok(Self {
            data: PackData::Owned(pack_data),
            index,
            aliased_offsets,
            content_end,
            #[cfg(test)]
            compact_frame_reads: AtomicUsize::new(0),
        })
    }
}

impl<'a> PackReader<'a> {
    pub fn from_slice(pack_data: &'a [u8], index_data: impl AsRef<[u8]>) -> Result<Self> {
        let (_, _, content_end) = verify_supported_container(pack_data)?;
        let index = PackIndex::from_bytes(index_data.as_ref())?;
        let aliased_offsets = index.aliased_offsets()?;
        Ok(Self {
            data: PackData::Borrowed(pack_data),
            index,
            aliased_offsets,
            content_end,
            #[cfg(test)]
            compact_frame_reads: AtomicUsize::new(0),
        })
    }

    /// List all object ids in this pack.
    pub fn list_ids(&self) -> Result<Vec<PackObjectId>> {
        self.index.ids()
    }

    /// Compute this pack's root-spool-scoped logical identity.
    ///
    /// Every logical object is decoded so delta and compact-frame physical
    /// choices cannot affect the result. The visit also validates exact index
    /// membership before an identity is returned.
    pub fn logical_id(&self) -> Result<PackLogicalId> {
        let mut identity = LogicalIdBuilder::new();
        self.visit_objects(|id, object_type, data| {
            identity.push(id, object_type, data);
            Ok(())
        })?;
        Ok(identity.finish())
    }

    /// Hash the exact finalized pack bytes used by this reader.
    pub fn representation_hash(&self) -> PackRepresentationHash {
        PackRepresentationHash::compute(self.data.as_slice())
    }

    /// List logical ids together with their physical read tier.
    ///
    /// A shared compact frame is represented by several index aliases at one
    /// record offset. Direct records have a unique offset and form the hot,
    /// random-access tier. A one-object compact frame is intentionally treated
    /// as hot here: it has no read amplification over a direct record.
    pub(super) fn indexed_read_tiers(&self) -> Result<Vec<(PackObjectId, PackReadTier)>> {
        let entries = self.index.entries()?;
        let mut aliases = HashMap::<u64, usize>::with_capacity(entries.len());
        for entry in &entries {
            *aliases.entry(entry.offset).or_default() += 1;
        }
        Ok(entries
            .into_iter()
            .map(|entry| {
                let tier = if aliases[&entry.offset] > 1 {
                    PackReadTier::SolidFrame
                } else {
                    PackReadTier::Hot
                };
                (entry.id, tier)
            })
            .collect())
    }

    /// Point-membership probe backed by the sorted pack index. This avoids
    /// enumerating every object merely to locate one hot-path tree or state.
    pub(super) fn contains_object(&self, id: &PackObjectId) -> Result<bool> {
        Ok(self.index.find(id)?.is_some())
    }

    #[cfg(test)]
    pub(super) fn compact_frame_read_count(&self) -> usize {
        self.compact_frame_reads.load(Ordering::Relaxed)
    }

    #[cfg(test)]
    fn record_compact_frame_read(&self) {
        self.compact_frame_reads.fetch_add(1, Ordering::Relaxed);
    }

    pub fn list_hashes(&self) -> Result<Vec<ContentHash>> {
        Ok(self
            .list_ids()?
            .into_iter()
            .filter_map(|id| match id {
                PackObjectId::Hash(hash) => Some(hash),
                PackObjectId::StateId(_) | PackObjectId::AnnotatedTag(_) => None,
            })
            .collect())
    }

    pub fn has_object(&self, id: &PackObjectId) -> Result<bool> {
        Ok(self.index.find(id)?.is_some())
    }

    /// Compressed payload bytes used by unique physical records of `obj_type`.
    ///
    /// Shared compact frames have one record offset indexed by many logical
    /// ids, so offsets are deduplicated before bytes are counted.
    pub fn encoded_payload_bytes(&self, obj_type: ObjectType) -> Result<u64> {
        let mut offsets = BTreeSet::new();
        for id in self.index.ids()? {
            if let Some(offset) = self.index.find(&id)? {
                offsets.insert(checked_index_offset(offset)?);
            }
        }
        let mut bytes = 0u64;
        for offset in offsets {
            let header = decode_tagged_entry_header(self.content_from(offset)?)?;
            if header.obj_type == obj_type {
                bytes = bytes.saturating_add(header.compressed_size as u64);
            }
        }
        Ok(bytes)
    }

    /// Visit every logical object while decoding each shared frame once.
    ///
    /// The index-to-frame membership is checked exactly before objects are
    /// yielded, closing both missing-entry and stale-alias corruption paths.
    pub fn visit_objects(
        &self,
        mut visitor: impl FnMut(PackObjectId, ObjectType, &[u8]) -> Result<()>,
    ) -> Result<()> {
        let mut locations = std::collections::BTreeMap::<usize, Vec<PackObjectId>>::new();
        for id in self.index.ids()? {
            let offset = self
                .index
                .find(&id)?
                .ok_or_else(|| StoreError::InvalidObject("indexed object disappeared".into()))?;
            locations
                .entry(checked_index_offset(offset)?)
                .or_default()
                .push(id);
        }
        for (offset, indexed_ids) in locations {
            if let Some(objects) = self.read_compact_objects_at(offset)? {
                let actual = objects.iter().map(|(id, _, _)| *id).collect::<HashSet<_>>();
                let indexed = indexed_ids.iter().copied().collect::<HashSet<_>>();
                if actual != indexed
                    || actual.len() != objects.len()
                    || indexed.len() != indexed_ids.len()
                {
                    return Err(StoreError::InvalidObject(
                        "compact frame object set differs from its index".into(),
                    ));
                }
                for (id, object_type, data) in objects {
                    visitor(id, object_type, &data)?;
                }
                continue;
            }
            if indexed_ids.len() != 1 {
                return Err(StoreError::InvalidObject(
                    "ordinary pack record is indexed by multiple object ids".into(),
                ));
            }
            let id = indexed_ids[0];
            let (object_type, data) = self
                .get_object(&id)?
                .ok_or_else(|| StoreError::InvalidObject("indexed object is missing".into()))?;
            visitor(id, object_type, &data)?;
        }
        Ok(())
    }

    /// Copy a validated subset of non-delta encoded entries into a standalone
    /// hosted transport pack without decoding or recompressing their bodies.
    ///
    /// `Ok(None)` is a safe fallback signal: an expected object is absent,
    /// duplicated, has a different type/size, is delta encoded, or names a
    /// repository-local entry that may not cross the hosted pack boundary.
    pub fn copy_hosted_encoded_subset(
        &self,
        expected: &[(PackObjectId, ObjectType, u64)],
    ) -> Result<Option<EncodedPackSubset>> {
        if expected.is_empty() {
            return Ok(None);
        }
        let mut unique = HashSet::with_capacity(expected.len());
        if expected.iter().any(|(id, obj_type, _)| {
            !unique.insert(*id)
                || matches!(
                    obj_type,
                    ObjectType::Delta | ObjectType::StateAttachment | ObjectType::SnapshotCommit
                )
        }) {
            return Ok(None);
        }

        let mut pack_data = Vec::new();
        write_container_header(&mut pack_data, pack_container_spec(), expected.len() as u64);
        let mut index = PackIndex::new();
        let mut encoded_bytes_copied = 0u64;
        for (expected_id, expected_type, expected_size) in expected {
            let Some(offset) = self.index.find(expected_id)? else {
                return Ok(None);
            };
            let offset = checked_index_offset(offset)?;
            if offset >= self.content_end {
                return Err(StoreError::InvalidObject(
                    "Entry offset out of bounds".to_string(),
                ));
            }
            let header = decode_tagged_entry_header(self.content_from(offset)?)?;
            if self.read_compact_objects_at(offset)?.is_some() {
                return Ok(None);
            }
            let expected_size = usize::try_from(*expected_size).ok();
            if header.id != *expected_id
                || header.obj_type != *expected_type
                || Some(header.uncompressed_size) != expected_size
                || matches!(
                    header.obj_type,
                    ObjectType::Delta | ObjectType::StateAttachment | ObjectType::SnapshotCommit
                )
            {
                return Ok(None);
            }
            let encoded_len = header
                .header_len
                .checked_add(header.compressed_size)
                .ok_or_else(|| {
                    StoreError::InvalidObject("pack entry length overflow".to_string())
                })?;
            let encoded_end = offset
                .checked_add(encoded_len)
                .ok_or_else(|| StoreError::InvalidObject("pack entry end overflow".to_string()))?;
            if encoded_end > self.content_end {
                return Err(StoreError::InvalidObject(
                    "pack entry extends beyond content boundary".to_string(),
                ));
            }
            let output_offset = u64::try_from(pack_data.len()).map_err(|_| {
                StoreError::InvalidObject("reused pack offset exceeds u64".to_string())
            })?;
            index.add(*expected_id, output_offset);
            pack_data.extend_from_slice(&self.data.as_slice()[offset..encoded_end]);
            encoded_bytes_copied = encoded_bytes_copied
                .checked_add(u64::try_from(encoded_len).map_err(|_| {
                    StoreError::InvalidObject("encoded pack entry length exceeds u64".to_string())
                })?)
                .ok_or_else(|| {
                    StoreError::InvalidObject("encoded reused byte count overflow".to_string())
                })?;
        }
        index.sort();
        append_container_checksum(&mut pack_data);
        Ok(Some(EncodedPackSubset {
            pack_data,
            index_data: index.to_bytes(),
            encoded_bytes_copied,
        }))
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
        let offset = match self.index.find(id)? {
            Some(offset) => checked_index_offset(offset)?,
            None => return Ok(None),
        };

        let record = self.read_record_at_depth(id, offset, 0)?;
        Ok(Some((record.obj_type, record.data)))
    }

    pub fn get_hashed_object(&self, hash: &ContentHash) -> Result<Option<(ObjectType, Vec<u8>)>> {
        self.get_object(&PackObjectId::Hash(*hash))
    }

    /// Read an object's logical type from pack headers without reading or
    /// decoding its payload.
    ///
    /// Delta entries inherit the type of their base, so this follows only the
    /// tagged base ids and headers until it reaches a non-delta entry. No
    /// compressed or delta payload bytes are decoded. Missing hashes return
    /// `Ok(None)`.
    pub fn get_hashed_object_type(&self, hash: &ContentHash) -> Result<Option<ObjectType>> {
        let id = PackObjectId::Hash(*hash);
        let Some(offset) = self.index.find(&id)? else {
            return Ok(None);
        };
        self.read_object_type_at_depth(&id, checked_index_offset(offset)?, 0)
            .map(Some)
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
        let Some(offset) = self.index.find(id)? else {
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
        let header_start = checked_index_add(offset, id_len, "record header start")?;
        let (encoded_type, uncompressed_size, type_len) =
            varint::decode_type_and_size(self.content_from(header_start)?).ok_or_else(|| {
                StoreError::InvalidObject("Truncated type+size varint".to_string())
            })?;
        let obj_type = decoded_entry_type(record_id, encoded_type)?;
        let uncompressed_size = checked_decoded_size("uncompressed_size", uncompressed_size)?;
        let varint_start = checked_index_add(header_start, type_len, "compressed_size start")?;
        let (compressed_size, comp_len) = varint::decode_varint(self.content_from(varint_start)?)
            .ok_or_else(truncated_compressed_size_varint)?;
        let compressed_size = checked_decoded_size("compressed_size", compressed_size)?;

        // Fast path: non-delta entry stored uncompressed. The most
        // common shape for snapshot-time packs (the builder skips
        // the delta search for unrelated blobs).
        if record_id == *id && obj_type != ObjectType::Delta && compressed_size == uncompressed_size
        {
            let data_start = checked_index_add(varint_start, comp_len, "entry data start")?;
            let data_end = checked_data_end(data_start, compressed_size, self.content_end)?;
            let data = &self.data.as_slice()[data_start..data_end];
            if !is_compact_frame(data) {
                return Ok(Some((obj_type, self.data.slice(data_start..data_end))));
            }
        }

        // Slow path: defer to the full record reader (it handles
        // decompression + delta chains) and Bytes-wrap the Vec.
        // Bytes::from(Vec) is a single Arc allocation, no body copy.
        let record = self.read_record_at_depth(id, offset, 0)?;
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
        let Some(offset) = self.index.find(&id)? else {
            return Ok(None);
        };
        let offset = checked_index_offset(offset)?;
        if offset >= self.content_end {
            return Err(StoreError::InvalidObject(
                "Entry offset out of bounds".to_string(),
            ));
        }
        let (record_id, id_len) = PackObjectId::decode_tagged(self.content_from(offset)?)?;
        let header_start = checked_index_add(offset, id_len, "record header start")?;
        let (obj_type, uncompressed_size, _type_len) = super::varint::decode_type_and_size(
            self.content_from(header_start)?,
        )
        .ok_or_else(|| StoreError::InvalidObject("Truncated type+size varint".to_string()))?;
        if (obj_type == ObjectType::Blob && self.aliased_offsets.contains(&(offset as u64)))
            || matches!(obj_type, ObjectType::Tree | ObjectType::State)
        {
            let Some((_, data)) = self.get_object(&id)? else {
                return Ok(None);
            };
            return Ok(Some(data.len() as u64));
        }
        verify_record_id_matches(&id, &record_id)?;
        if obj_type == ObjectType::Delta {
            // Delta entries record the *resolved* output size in the
            // type+size varint already (see `read_record_at_depth`'s
            // size-mismatch check), so we can still return without
            // decompressing the payload.
            return Ok(Some(uncompressed_size));
        }
        Ok(Some(uncompressed_size))
    }

    fn read_object_type_at_depth(
        &self,
        requested_id: &PackObjectId,
        offset: usize,
        depth: usize,
    ) -> Result<ObjectType> {
        if depth > MAX_DELTA_CHAIN_DEPTH {
            return Err(StoreError::InvalidObject(format!(
                "Delta chain depth {depth} exceeds max {MAX_DELTA_CHAIN_DEPTH}"
            )));
        }
        if offset >= self.content_end {
            return Err(StoreError::InvalidObject(
                "Entry offset out of bounds".to_string(),
            ));
        }

        let header = decode_tagged_entry_header(self.content_from(offset)?)?;
        if header.id != *requested_id {
            return self
                .read_record_at_depth(requested_id, offset, depth)
                .map(|record| record.obj_type);
        }
        if header.obj_type != ObjectType::Delta {
            return Ok(header.obj_type);
        }

        let base_hash = Self::require_delta_base_hash(header.delta_base)?;
        let base_id = PackObjectId::Hash(base_hash);
        let base_offset = self
            .index
            .find(&base_id)?
            .ok_or_else(|| StoreError::NotFound(base_hash.to_string()))?;
        self.read_object_type_at_depth(&base_id, checked_index_offset(base_offset)?, depth + 1)
    }

    fn read_record_at_depth(
        &self,
        requested_id: &PackObjectId,
        offset: usize,
        depth: usize,
    ) -> Result<PackObjectRecord> {
        if offset >= self.content_end {
            return Err(StoreError::InvalidObject(
                "Entry offset out of bounds".to_string(),
            ));
        }

        let (id, id_len) = PackObjectId::decode_tagged(self.content_from(offset)?)?;
        let header_start = checked_index_add(offset, id_len, "record header start")?;

        let (encoded_type, uncompressed_size, type_len) =
            varint::decode_type_and_size(self.content_from(header_start)?).ok_or_else(|| {
                StoreError::InvalidObject("Truncated type+size varint".to_string())
            })?;
        let obj_type = decoded_entry_type(id, encoded_type)?;
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

        let shared_blob =
            obj_type == ObjectType::Blob && self.aliased_offsets.contains(&(offset as u64));
        if obj_type != ObjectType::Delta && (shared_blob || is_compact_frame(&decompressed)) {
            #[cfg(test)]
            self.record_compact_frame_read();
            if let Some(data) =
                decode_compact_object(requested_id, obj_type, &decompressed, shared_blob)?
            {
                return Ok(PackObjectRecord {
                    id: *requested_id,
                    obj_type,
                    data,
                    delta_base: None,
                    path_hint: None,
                });
            }
        }
        verify_record_id_matches(requested_id, &id)?;
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

    fn read_compact_objects_at(&self, offset: usize) -> Result<Option<DecodedCompactObjects>> {
        if offset >= self.content_end {
            return Err(StoreError::InvalidObject(
                "Entry offset out of bounds".to_string(),
            ));
        }
        let header = decode_tagged_entry_header(self.content_from(offset)?)?;
        if !matches!(
            header.obj_type,
            ObjectType::Blob | ObjectType::Tree | ObjectType::State
        ) {
            return Ok(None);
        }
        let shared_blob =
            header.obj_type == ObjectType::Blob && self.aliased_offsets.contains(&(offset as u64));
        if header.obj_type == ObjectType::Blob && !shared_blob {
            return Ok(None);
        }
        let data_start = checked_index_add(offset, header.header_len, "entry data start")?;
        let data_end = checked_data_end(data_start, header.compressed_size, self.content_end)?;
        let stored = &self.data.as_slice()[data_start..data_end];
        let data = if header.compressed_size != header.uncompressed_size {
            decompress_pack_payload(stored, header.uncompressed_size)?
        } else {
            stored.to_vec()
        };
        if data.len() != header.uncompressed_size {
            return Err(StoreError::InvalidObject(format!(
                "Size mismatch: expected {}, got {}",
                header.uncompressed_size,
                data.len()
            )));
        }
        #[cfg(test)]
        if shared_blob || is_compact_frame(&data) {
            self.record_compact_frame_read();
        }
        decode_compact_objects(header.obj_type, &data, shared_blob)
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
            .find(&PackObjectId::Hash(base_hash))?
            .ok_or_else(|| StoreError::NotFound(base_hash.to_string()))?;
        let base_offset = checked_index_offset(base_offset)?;
        let base_id = PackObjectId::Hash(base_hash);
        let base_record = self.read_record_at_depth(&base_id, base_offset, depth + 1)?;
        let base_type = base_record.obj_type;
        let base_data = base_record.data;

        let decoded = DeltaDecoder::decode(&base_data, delta, uncompressed_size)
            .map_err(|error| StoreError::InvalidObject(format!("Delta decode failed: {error}")))?;

        Ok((base_type, decoded))
    }

    fn require_delta_base_hash(base_id: Option<PackObjectId>) -> Result<ContentHash> {
        match base_id {
            Some(PackObjectId::Hash(hash)) => Ok(hash),
            Some(PackObjectId::StateId(_) | PackObjectId::AnnotatedTag(_)) => Err(
                StoreError::InvalidObject("pack delta base must be hash-backed content".into()),
            ),
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
    let size = usize::try_from(size).map_err(|_| {
        StoreError::InvalidObject(format!("Decoded {field} exceeds platform limits"))
    })?;
    if field == "uncompressed_size" && size > super::shared::MAX_PACK_OBJECT_OUTPUT_SIZE {
        return Err(StoreError::InvalidObject(format!(
            "Pack object output size {size} exceeds max {}",
            super::shared::MAX_PACK_OBJECT_OUTPUT_SIZE
        )));
    }
    Ok(size)
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

fn decoded_entry_type(id: PackObjectId, encoded: ObjectType) -> Result<ObjectType> {
    if matches!(id, PackObjectId::AnnotatedTag(_)) {
        if encoded != ObjectType::Blob {
            return Err(StoreError::InvalidObject(
                "annotated-tag pack entry has invalid encoded type".to_string(),
            ));
        }
        Ok(ObjectType::AnnotatedTag)
    } else {
        Ok(encoded)
    }
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

fn is_compact_frame(data: &[u8]) -> bool {
    heddle_object_model::compact::is_blob_frame(data)
        || heddle_object_model::compact::is_tree_frame(data)
        || heddle_object_model::compact::is_state_frame(data)
}

fn decode_compact_object(
    requested_id: &PackObjectId,
    obj_type: ObjectType,
    data: &[u8],
    require_blob_frame: bool,
) -> Result<Option<Vec<u8>>> {
    let Some(objects) = decode_compact_objects(obj_type, data, require_blob_frame)? else {
        return Ok(None);
    };
    objects
        .into_iter()
        .find_map(|(id, _, bytes)| (id == *requested_id).then_some(bytes))
        .map(Some)
        .ok_or_else(|| compact_index_miss(requested_id))
}

fn decode_compact_objects(
    obj_type: ObjectType,
    data: &[u8],
    require_blob_frame: bool,
) -> Result<Option<DecodedCompactObjects>> {
    match obj_type {
        ObjectType::Blob if require_blob_frame => {
            heddle_object_model::compact::decode_blob_frame(data)
                .map_err(|error| StoreError::InvalidObject(error.to_string()))?
                .into_iter()
                .map(|(hash, body)| Ok((PackObjectId::Hash(hash), ObjectType::Blob, body.to_vec())))
                .collect::<Result<Vec<_>>>()
                .map(Some)
        }
        ObjectType::Blob => Ok(None),
        ObjectType::Tree if heddle_object_model::compact::is_tree_frame(data) => {
            heddle_object_model::compact::decode_tree_frame(data)
                .map_err(|error| StoreError::InvalidObject(error.to_string()))?
                .into_iter()
                .map(|tree| {
                    let id = PackObjectId::Hash(tree.hash());
                    let bytes = rmp_serde::to_vec_named(&tree)
                        .map_err(|error| StoreError::InvalidObject(error.to_string()))?;
                    Ok((id, ObjectType::Tree, bytes))
                })
                .collect::<Result<Vec<_>>>()
                .map(Some)
        }
        ObjectType::State if heddle_object_model::compact::is_state_frame(data) => {
            heddle_object_model::compact::decode_state_frame(data)
                .map_err(|error| StoreError::InvalidObject(error.to_string()))?
                .into_iter()
                .map(|state| {
                    let id = PackObjectId::StateId(state.state_id);
                    let bytes = rmp_serde::to_vec_named(&state)
                        .map_err(|error| StoreError::InvalidObject(error.to_string()))?;
                    Ok((id, ObjectType::State, bytes))
                })
                .collect::<Result<Vec<_>>>()
                .map(Some)
        }
        _ if is_compact_frame(data) => Err(StoreError::InvalidObject(
            "compact frame magic does not match its pack object type".into(),
        )),
        _ => Ok(None),
    }
}

fn compact_index_miss(id: &PackObjectId) -> StoreError {
    StoreError::InvalidObject(format!(
        "compact frame does not contain indexed object {id:?}"
    ))
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
