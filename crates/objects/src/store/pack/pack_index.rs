// SPDX-License-Identifier: Apache-2.0
//! Pack index for fast object lookup within packfiles.

use std::{
    fs::File,
    io::{BufReader, Read},
    path::Path,
};

use crate::store::{
    Result,
    pack::{
        PackObjectId,
        versioned_header::{HeaderChecksum, VersionedHeader},
    },
};

pub(super) const INDEX_MAGIC: &[u8; 4] = b"LMI\0";
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
        index_header().write_vec(&mut result, self.entries.len() as u64);
        for entry in &self.entries {
            entry.id.encode_tagged(&mut result);
            result.extend_from_slice(&entry.offset.to_be_bytes());
        }
        result
    }

    /// Deserialize from bytes.
    pub fn from_bytes(data: &[u8]) -> Result<Self> {
        let header = index_header().verify(data)?;
        let count = header.count;
        let max_entries = ((data.len() - header.header_len) / MIN_INDEX_ENTRY_LEN) as u64;
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
        let mut pos = header.header_len;
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

    /// Open and deserialize an index file without first copying the
    /// whole `.idx` into a temporary `Vec<u8>`.
    pub(super) fn open(path: &Path) -> Result<Self> {
        let file = File::open(path).map_err(crate::store::StoreError::from)?;
        let len = file
            .metadata()
            .map_err(crate::store::StoreError::from)?
            .len();
        let mut reader = BufReader::new(file);
        let count = read_index_header(&mut reader)?;
        validate_index_capacity(count, len)?;
        let count = usize::try_from(count).map_err(|_| {
            crate::store::StoreError::InvalidObject(
                "Index entry count exceeds platform limits".to_string(),
            )
        })?;
        let mut entries = Vec::with_capacity(count);
        for _ in 0..count {
            let id = read_tagged_id(&mut reader)?;
            let mut offset_bytes = [0u8; 8];
            read_exact_invalid(&mut reader, &mut offset_bytes, "Index data truncated")?;
            entries.push(IndexEntry {
                id,
                offset: u64::from_be_bytes(offset_bytes),
            });
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

fn validate_index_capacity(count: u64, data_len: u64) -> Result<()> {
    if data_len < super::versioned_header::VERSIONED_HEADER_LEN as u64 {
        return Err(crate::store::StoreError::InvalidObject(
            "Index too short".to_string(),
        ));
    }
    let available = data_len - super::versioned_header::VERSIONED_HEADER_LEN as u64;
    let max_entries = available / MIN_INDEX_ENTRY_LEN as u64;
    if count > max_entries {
        return Err(crate::store::StoreError::InvalidObject(format!(
            "Index entry count {} exceeds available data capacity {}",
            count, max_entries
        )));
    }
    Ok(())
}

fn read_index_header<R: Read>(reader: &mut R) -> Result<u64> {
    let mut header = [0u8; super::versioned_header::VERSIONED_HEADER_LEN];
    read_exact_invalid(reader, &mut header, "Index too short")?;
    if &header[..4] != INDEX_MAGIC {
        return Err(crate::store::StoreError::InvalidObject(
            "Invalid index magic".to_string(),
        ));
    }

    let version = u32::from_be_bytes([header[4], header[5], header[6], header[7]]);
    if version != INDEX_VERSION {
        return Err(crate::store::StoreError::InvalidObject(format!(
            "Unsupported index version: {version}"
        )));
    }

    Ok(u64::from_be_bytes([
        header[8], header[9], header[10], header[11], header[12], header[13], header[14],
        header[15],
    ]))
}

fn read_tagged_id<R: Read>(reader: &mut R) -> Result<PackObjectId> {
    let mut tag = [0u8; 1];
    read_exact_invalid(reader, &mut tag, "missing pack object id tag")?;
    match tag[0] {
        0 => {
            let mut bytes = [0u8; 32];
            read_exact_invalid(reader, &mut bytes, "hash pack object id truncated")?;
            Ok(PackObjectId::Hash(crate::object::ContentHash::from_bytes(
                bytes,
            )))
        }
        1 => {
            let mut bytes = [0u8; 16];
            read_exact_invalid(reader, &mut bytes, "change id pack object id truncated")?;
            Ok(PackObjectId::ChangeId(crate::object::ChangeId::from_bytes(
                bytes,
            )))
        }
        tag => Err(crate::store::StoreError::InvalidObject(format!(
            "unknown pack object id tag {tag}"
        ))),
    }
}

fn read_exact_invalid<R: Read>(reader: &mut R, buf: &mut [u8], message: &str) -> Result<()> {
    match reader.read_exact(buf) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => {
            Err(crate::store::StoreError::InvalidObject(message.to_string()))
        }
        Err(error) => Err(crate::store::StoreError::from(error)),
    }
}
