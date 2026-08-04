// SPDX-License-Identifier: Apache-2.0
//! Pack index for fast object lookup within packfiles.

use bytes::Bytes;

use crate::store::{
    Result,
    pack::{
        PackObjectId,
        versioned_header::{HeaderChecksum, VersionedHeader},
    },
};

pub(super) const INDEX_MAGIC: &[u8; 4] = b"LMI\0";
pub(super) const INDEX_VERSION: u32 = 2;
const INDEX_ENTRY_LEN: usize = 33 + 8;

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
            entry.id.encode_tagged(&mut result);
            result.extend_from_slice(&entry.offset.to_be_bytes());
        }
        result
    }

    /// Deserialize from bytes.
    pub fn from_bytes(data: &[u8]) -> Result<Self> {
        Self::from_owned_bytes(Bytes::copy_from_slice(data))
    }

    pub fn from_owned_bytes(data: Bytes) -> Result<Self> {
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
        let (id, id_len) = PackObjectId::decode_tagged(bytes)?;
        let offset = u64::from_be_bytes(bytes[id_len..].try_into().map_err(|_| {
            crate::store::StoreError::InvalidObject("Invalid offset length".to_string())
        })?);
        Ok(IndexEntry { id, offset })
    }
}

impl PackIndex {
    /// Return all ids in this index.
    pub fn ids(&self) -> Result<Vec<PackObjectId>> {
        if let Some(encoded) = &self.encoded {
            return (0..encoded.count)
                .map(|index| encoded.entry(index).map(|entry| entry.id))
                .collect();
        }
        Ok(self.entries.iter().map(|entry| entry.id).collect())
    }
}

impl Default for PackIndex {
    fn default() -> Self {
        Self::new()
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
