// SPDX-License-Identifier: Apache-2.0
//! Pack index for fast object lookup within packfiles.

use std::collections::HashSet;

use bytes::Bytes;

use crate::store::{
    Result,
    pack::{
        PackObjectId,
        versioned_header::{HeaderChecksum, VersionedHeader},
    },
};

pub(super) const INDEX_MAGIC: &[u8; 4] = b"LMI\0";
pub(super) const INDEX_VERSION: u32 = 4;
pub(super) const INDEX_ENTRY_LEN: usize = 32 + 8;
const STATE_ID_OFFSET_TAG: u64 = 1 << 63;
const ANNOTATED_TAG_OFFSET_TAG: u64 = 1 << 62;
const PACK_OFFSET_MASK: u64 = !(STATE_ID_OFFSET_TAG | ANNOTATED_TAG_OFFSET_TAG);

/// Entry in the pack index.
#[derive(Debug, Clone, Copy)]
pub struct IndexEntry {
    pub id: PackObjectId,
    pub offset: u64,
}

/// Pack index for fast object lookup.
#[derive(Debug)]
pub struct PackIndex {
    entries: Vec<IndexEntry>,
    encoded: Option<EncodedIndex>,
}

#[derive(Debug)]
struct EncodedIndex {
    data: Bytes,
    entries_start: usize,
    count: usize,
}

impl PackIndex {
    /// Create a new empty index.
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
            encoded: None,
        }
    }

    /// Add an entry.
    pub fn add(&mut self, id: PackObjectId, offset: u64) {
        debug_assert!(self.encoded.is_none());
        self.entries.push(IndexEntry { id, offset });
    }

    /// Sort entries by hash for binary search.
    pub fn sort(&mut self) {
        debug_assert!(self.encoded.is_none());
        self.entries.sort_by_key(|e| e.id);
    }

    /// Find an entry by hash.
    pub fn find(&self, id: &PackObjectId) -> Result<Option<u64>> {
        let Some(encoded) = &self.encoded else {
            return Ok(self
                .entries
                .binary_search_by_key(id, |entry| entry.id)
                .ok()
                .map(|index| self.entries[index].offset));
        };
        let mut low = 0;
        let mut high = encoded.count;
        while low < high {
            let middle = low + (high - low) / 2;
            let entry = encoded.entry(middle)?;
            match entry.id.cmp(id) {
                std::cmp::Ordering::Less => low = middle + 1,
                std::cmp::Ordering::Greater => high = middle,
                std::cmp::Ordering::Equal => return Ok(Some(entry.offset)),
            }
        }
        Ok(None)
    }

    /// Serialize to bytes.
    pub fn to_bytes(&self) -> Vec<u8> {
        if let Some(encoded) = &self.encoded {
            return encoded.data.to_vec();
        }
        let mut result = Vec::new();
        index_header().write_vec(&mut result, self.entries.len() as u64);
        for entry in &self.entries {
            result.extend_from_slice(&encode_index_entry(entry.id, entry.offset));
        }
        result
    }

    /// Deserialize from bytes.
    pub fn from_bytes(data: &[u8]) -> Result<Self> {
        Self::from_owned_bytes(Bytes::copy_from_slice(data))
    }

    pub fn from_owned_bytes(data: Bytes) -> Result<Self> {
        verify_index_version(&data)?;
        let header = index_header().verify(&data)?;
        let count = header.count;
        let max_entries = ((data.len() - header.header_len) / INDEX_ENTRY_LEN) as u64;
        if count > max_entries {
            return Err(crate::store::StoreError::InvalidObject(format!(
                "Index entry count {} exceeds available data capacity {}",
                count, max_entries
            )));
        }
        let count = usize::try_from(count).map_err(|_| {
            crate::store::StoreError::InvalidObject(
                "Index entry count exceeds platform limits".to_string(),
            )
        })?;
        Ok(Self {
            entries: Vec::new(),
            encoded: Some(EncodedIndex {
                data,
                entries_start: header.header_len,
                count,
            }),
        })
    }
}

impl EncodedIndex {
    fn entry(&self, index: usize) -> Result<IndexEntry> {
        let start = self.entries_start + index * INDEX_ENTRY_LEN;
        let end = start + INDEX_ENTRY_LEN;
        let bytes = self.data.get(start..end).ok_or_else(|| {
            crate::store::StoreError::InvalidObject("Index data truncated".to_string())
        })?;
        decode_index_entry(bytes)
    }
}

pub(super) fn encode_index_entry(id: PackObjectId, offset: u64) -> [u8; INDEX_ENTRY_LEN] {
    assert!(
        offset <= PACK_OFFSET_MASK,
        "pack index offset exceeds the 62-bit format limit"
    );
    let mut bytes = [0u8; INDEX_ENTRY_LEN];
    let tagged_offset = match id {
        PackObjectId::Hash(hash) => {
            bytes[..32].copy_from_slice(hash.as_bytes());
            offset
        }
        PackObjectId::StateId(state_id) => {
            bytes[..32].copy_from_slice(state_id.as_bytes());
            offset | STATE_ID_OFFSET_TAG
        }
        PackObjectId::AnnotatedTag(hash) => {
            bytes[..32].copy_from_slice(hash.as_bytes());
            offset | ANNOTATED_TAG_OFFSET_TAG
        }
    };
    bytes[32..].copy_from_slice(&tagged_offset.to_be_bytes());
    bytes
}

fn decode_index_entry(bytes: &[u8]) -> Result<IndexEntry> {
    let raw_id: [u8; 32] = bytes[..32].try_into().map_err(|_| {
        crate::store::StoreError::InvalidObject("Invalid index id length".to_string())
    })?;
    let tagged_offset = u64::from_be_bytes(bytes[32..].try_into().map_err(|_| {
        crate::store::StoreError::InvalidObject("Invalid offset length".to_string())
    })?);
    let id = if tagged_offset & STATE_ID_OFFSET_TAG != 0 {
        PackObjectId::StateId(crate::object::StateId::from_bytes(raw_id))
    } else if tagged_offset & ANNOTATED_TAG_OFFSET_TAG != 0 {
        PackObjectId::AnnotatedTag(crate::object::ContentHash::from_bytes(raw_id))
    } else {
        PackObjectId::Hash(crate::object::ContentHash::from_bytes(raw_id))
    };
    Ok(IndexEntry {
        id,
        offset: tagged_offset & PACK_OFFSET_MASK,
    })
}

impl PackIndex {
    /// Return all decoded index entries.
    pub(super) fn entries(&self) -> Result<Vec<IndexEntry>> {
        if let Some(encoded) = &self.encoded {
            return (0..encoded.count)
                .map(|index| encoded.entry(index))
                .collect();
        }
        Ok(self.entries.clone())
    }

    /// Return all ids in this index.
    pub fn ids(&self) -> Result<Vec<PackObjectId>> {
        Ok(self.entries()?.into_iter().map(|entry| entry.id).collect())
    }

    pub(super) fn aliased_offsets(&self) -> Result<HashSet<u64>> {
        let mut seen = HashSet::new();
        let mut aliases = HashSet::new();
        for entry in self.entries()? {
            if !seen.insert(entry.offset) {
                aliases.insert(entry.offset);
            }
        }
        Ok(aliases)
    }
}

impl Default for PackIndex {
    fn default() -> Self {
        Self::new()
    }
}

fn verify_index_version(data: &[u8]) -> Result<()> {
    if data.len() < 8 || &data[..4] != INDEX_MAGIC {
        index_header().verify_layout(data)?;
        unreachable!("invalid index header must have returned an error")
    }
    let version = u32::from_be_bytes(data[4..8].try_into().map_err(|_| {
        crate::store::StoreError::InvalidObject("Index version field is truncated".to_string())
    })?);
    if version == INDEX_VERSION {
        Ok(())
    } else if version > INDEX_VERSION {
        Err(crate::store::StoreError::InvalidObject(format!(
            "pack index uses format version {version}, but this binary supports {INDEX_VERSION}; upgrade heddle"
        )))
    } else {
        Err(crate::store::StoreError::InvalidObject(format!(
            "pack index uses unsupported format version {version}; run `heddle migrate`"
        )))
    }
}

pub(super) fn index_header() -> VersionedHeader {
    VersionedHeader {
        magic: INDEX_MAGIC,
        version: INDEX_VERSION,
        checksum: HeaderChecksum::None,
        too_short: "Index too short",
        invalid_magic: "Invalid index magic",
        unsupported_version: "Unsupported index version",
        checksum_mismatch: "",
    }
}
