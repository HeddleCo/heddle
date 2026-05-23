// SPDX-License-Identifier: Apache-2.0
//! Pack index for fast object lookup within packfiles.

use crate::store::{Result, pack::PackObjectId};

pub(super) const INDEX_MAGIC: &[u8] = b"LMI\0";
pub(super) const INDEX_VERSION: u32 = 2;
const MIN_INDEX_ENTRY_LEN: usize = 17 + 8;

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
}

impl PackIndex {
    /// Create a new empty index.
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    /// Add an entry.
    pub fn add(&mut self, id: PackObjectId, offset: u64) {
        self.entries.push(IndexEntry { id, offset });
    }

    /// Sort entries by hash for binary search.
    pub fn sort(&mut self) {
        self.entries.sort_by_key(|e| e.id);
    }

    /// Find an entry by hash.
    pub fn find(&self, id: &PackObjectId) -> Option<u64> {
        self.entries
            .binary_search_by_key(id, |e| e.id)
            .ok()
            .map(|idx| self.entries[idx].offset)
    }

    /// Serialize to bytes.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut result = Vec::new();
        result.extend_from_slice(INDEX_MAGIC);
        result.extend_from_slice(&INDEX_VERSION.to_be_bytes());
        result.extend_from_slice(&(self.entries.len() as u64).to_be_bytes());
        for entry in &self.entries {
            entry.id.encode_tagged(&mut result);
            result.extend_from_slice(&entry.offset.to_be_bytes());
        }
        result
    }

    /// Deserialize from bytes.
    pub fn from_bytes(data: &[u8]) -> Result<Self> {
        if data.len() < 16 {
            return Err(crate::store::StoreError::InvalidObject(
                "Index too short".to_string(),
            ));
        }
        if &data[0..4] != INDEX_MAGIC {
            return Err(crate::store::StoreError::InvalidObject(
                "Invalid index magic".to_string(),
            ));
        }
        let version = u32::from_be_bytes([data[4], data[5], data[6], data[7]]);
        if version != INDEX_VERSION {
            return Err(crate::store::StoreError::InvalidObject(format!(
                "Unsupported index version: {}",
                version
            )));
        }
        let count = u64::from_be_bytes([
            data[8], data[9], data[10], data[11], data[12], data[13], data[14], data[15],
        ]);
        let max_entries = ((data.len() - 16) / MIN_INDEX_ENTRY_LEN) as u64;
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
        let mut entries = Vec::with_capacity(count);
        let mut pos = 16;
        for _ in 0..count {
            let (id, id_len) = PackObjectId::decode_tagged(&data[pos..])?;
            pos += id_len;
            if pos + 8 > data.len() {
                return Err(crate::store::StoreError::InvalidObject(
                    "Index data truncated".to_string(),
                ));
            }
            let offset = u64::from_be_bytes(data[pos..pos + 8].try_into().map_err(|_| {
                crate::store::StoreError::InvalidObject("Invalid offset length".to_string())
            })?);
            entries.push(IndexEntry { id, offset });
            pos += 8;
        }
        Ok(Self { entries })
    }
}

impl PackIndex {
    /// Return all ids in this index.
    pub fn ids(&self) -> Vec<PackObjectId> {
        self.entries.iter().map(|e| e.id).collect()
    }
}

impl Default for PackIndex {
    fn default() -> Self {
        Self::new()
    }
}
