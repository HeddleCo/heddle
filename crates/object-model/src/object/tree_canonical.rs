// SPDX-License-Identifier: Apache-2.0
//! Streamable canonical Tree encodings (HTR4).
//!
//! Each entry is a length-prefixed frame, so a reader can yield one entry or
//! a caller-sized page and resume at a byte offset without decoding the whole
//! tree. Version 4 stores frames raw. Version 5 groups frames into independently
//! compressed blocks with raw restart anchors and a fixed-width range index.

use sley::{ObjectFormat as GitObjectFormat, ObjectId as GitObjectId};

use super::{
    ContentHash, EntryType, FileMode, SpoolId, StateId, Tree, TreeEntry, TreeError,
    tree::{git_format_from_tag, git_format_to_tag},
    tree_stream::TreeStreamError,
};

/// Durable encoding version stored in every HTR4 header and resume cursor.
pub const TREE_ENCODING_VERSION: u8 = 4;
/// Block-compressed HTR4 variant. Readers accept both v4 and v5.
pub const TREE_BLOCK_ENCODING_VERSION: u8 = 5;
/// Frame discriminator for a single canonical tree.
pub const TREE_CANONICAL_MAGIC: &[u8; 4] = b"HTR4";
/// Lean hot-path tree anchor. The object key supplies the omitted tree hash.
pub const TREE_LEAN_MAGIC: &[u8; 4] = b"HLR1";
/// One-hop cumulative delta against a materialized tree anchor.
pub const TREE_DELTA_MAGIC: &[u8; 4] = b"HDC1";
/// Cursor version used by the HLR1 streaming reader.
pub const TREE_LEAN_ENCODING_VERSION: u8 = 6;
/// Current HDC1 body version.
pub const TREE_DELTA_ENCODING_VERSION: u8 = 1;
/// Fixed HDC1 header from the HTR4 radical spike.
pub const TREE_DELTA_HEADER_LEN: usize = 59;
/// A lineage is refreshed after 127 delta descendants.
pub const TREE_DELTA_ANCHOR_INTERVAL: u8 = 128;
/// Cumulative deltas above this operation count become anchors.
pub const TREE_DELTA_MAX_OPS: usize = 512;
/// Fixed header size: magic + version + tree id + counts.
pub const TREE_HEADER_LEN: usize = 4 + 1 + 32 + 8 + 8 + 8;
/// Small real trees do not repay block/index overhead.
pub const TREE_BLOCK_MIN_ENTRIES: usize = 18;

pub(crate) const TREE_BLOCK_ENTRIES: usize = 256;
pub(crate) const TREE_BLOCK_PREAMBLE_LEN: usize = 16;
pub(crate) const TREE_BLOCK_INDEX_LEN: usize = 24;
const TREE_BLOCK_CODEC_ZSTD: u8 = 1;
// The largest encoder-produced frame is a spoollink with u16-max name and
// spool-id fields: u32 frame length, mode, kind, u16 name length, name, u16
// spool length, spool, and the 32-byte state id.
const TREE_BLOCK_MAX_ENTRY_FRAME_LEN: usize =
    4 + 1 + 1 + 2 + u16::MAX as usize + 2 + u16::MAX as usize + 32;
pub(crate) const TREE_BLOCK_MAX_RAW_LEN: usize =
    TREE_BLOCK_ENTRIES * TREE_BLOCK_MAX_ENTRY_FRAME_LEN;
// A zstd block expands to at most 128 KiB and consumes encoded block bytes.
// This deliberately loose format bound admits all encoder output while
// preventing a tiny stored block from claiming the structural maximum.
const TREE_BLOCK_MAX_EXPANSION_RATIO: usize = 128 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct TreeBlockHeader {
    pub block_entries: usize,
    pub block_count: usize,
    pub entry_count: u64,
    pub raw_payload_len: u64,
    pub index_end: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct TreeBlockIndex {
    pub raw_offset: u64,
    pub stored_offset: u64,
    pub stored_len: usize,
    pub raw_len: usize,
}

/// Parsed HTR4 header. Counts and payload length are known before any entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TreeHeader {
    pub version: u8,
    pub tree_id: ContentHash,
    pub entry_count: u64,
    pub payload_len: u64,
    pub logical_len: u64,
}

/// True when `bytes` begin with the canonical tree discriminator.
pub fn is_canonical_tree(bytes: &[u8]) -> bool {
    bytes.starts_with(TREE_CANONICAL_MAGIC)
}

/// True when `bytes` contain a lean materialized anchor.
pub fn is_lean_tree(bytes: &[u8]) -> bool {
    bytes.starts_with(TREE_LEAN_MAGIC)
}

/// True when `bytes` contain an anchor-relative delta.
pub fn is_delta_tree(bytes: &[u8]) -> bool {
    bytes.starts_with(TREE_DELTA_MAGIC)
}

/// True when the body can be paged directly without reconstruction.
pub fn is_streamable_tree(bytes: &[u8]) -> bool {
    is_canonical_tree(bytes) || is_lean_tree(bytes)
}

impl Tree {
    /// Encode this tree as uncompressed HTR4.
    pub fn encode_canonical(&self) -> Result<Vec<u8>, TreeStreamError> {
        self.validate()?;
        let tree_id = self.hash();
        let mut payload = Vec::new();
        let mut logical_len = 0u64;
        for entry in self.entries() {
            logical_len = logical_len
                .checked_add(entry.encoded_len() as u64)
                .ok_or_else(|| TreeStreamError::Malformed("logical length overflow".into()))?;
            let frame = encode_entry_frame(entry)?;
            let frame_len = u32::try_from(frame.len()).map_err(|_| {
                TreeStreamError::Malformed(format!("entry '{}' frame exceeds u32", entry.name()))
            })?;
            payload.extend_from_slice(&frame_len.to_le_bytes());
            payload.extend_from_slice(&frame);
        }
        let mut out = Vec::with_capacity(TREE_HEADER_LEN + payload.len());
        out.extend_from_slice(TREE_CANONICAL_MAGIC);
        out.push(TREE_ENCODING_VERSION);
        out.extend_from_slice(tree_id.as_bytes());
        out.extend_from_slice(&(self.len() as u64).to_le_bytes());
        out.extend_from_slice(&(payload.len() as u64).to_le_bytes());
        out.extend_from_slice(&logical_len.to_le_bytes());
        out.extend_from_slice(&payload);
        Ok(out)
    }

    /// Decode a complete HTR4 body, validating order incrementally.
    pub fn decode_canonical(data: &[u8]) -> Result<Self, TreeStreamError> {
        let header = decode_header(data)?;
        if header.version == TREE_BLOCK_ENCODING_VERSION {
            return Self::decode_canonical_streamed(data);
        }
        let expected_len = TREE_HEADER_LEN as u64 + header.payload_len;
        if (data.len() as u64) < expected_len {
            return Err(TreeStreamError::TruncatedFrame {
                offset: data.len() as u64,
            });
        }
        if (data.len() as u64) > expected_len {
            return Err(TreeStreamError::TrailingBytes {
                extra: data.len() as u64 - expected_len,
            });
        }
        let mut entries = Vec::new();
        let mut offset = TREE_HEADER_LEN;
        let payload_end = data.len();
        for _ in 0..header.entry_count {
            let (entry, consumed) = decode_entry_at(data, offset, payload_end)?;
            entries.push(entry);
            offset += consumed;
        }
        if offset != payload_end {
            return Err(TreeStreamError::TrailingBytes {
                extra: (payload_end - offset) as u64,
            });
        }
        let tree = Tree::try_from_decoded_entries(entries)?;
        let found = tree.hash();
        if found != header.tree_id {
            return Err(TreeStreamError::HashMismatch {
                expected: header.tree_id,
                found,
            });
        }
        if tree
            .entries()
            .iter()
            .map(|entry| entry.encoded_len() as u64)
            .sum::<u64>()
            != header.logical_len
        {
            return Err(TreeStreamError::Malformed(
                "declared logical length does not match entries".into(),
            ));
        }
        Ok(tree)
    }

    /// Encode block-compressed HTR4, falling back to raw v4 when the complete
    /// object would not be smaller. Callers apply the small-tree policy.
    pub fn encode_canonical_blocked(
        &self,
        level: i32,
        min_size: usize,
    ) -> Result<Vec<u8>, TreeStreamError> {
        let raw = self.encode_canonical()?;
        if raw.len() < min_size {
            return Ok(raw);
        }
        let blocked = encode_blocked_htr4(&raw, level)?;
        if blocked.len() < raw.len() {
            Ok(blocked)
        } else {
            Ok(raw)
        }
    }
}

/// Parse the fixed HTR4 header. Does not read entry frames.
pub fn decode_header(data: &[u8]) -> Result<TreeHeader, TreeStreamError> {
    if data.len() < TREE_HEADER_LEN {
        return Err(TreeStreamError::TruncatedFrame { offset: 0 });
    }
    if !is_canonical_tree(data) {
        return Err(TreeStreamError::Malformed(
            "bytes are not a canonical HTR4 tree".into(),
        ));
    }
    let version = data[4];
    if version != TREE_ENCODING_VERSION && version != TREE_BLOCK_ENCODING_VERSION {
        return Err(TreeStreamError::UnsupportedVersion { found: version });
    }
    let tree_id = ContentHash::from_bytes(
        data[5..37]
            .try_into()
            .map_err(|_| TreeStreamError::Malformed("tree id slice is not 32 bytes".into()))?,
    );
    let entry_count = u64::from_le_bytes(
        data[37..45]
            .try_into()
            .map_err(|_| TreeStreamError::Malformed("entry count slice is not 8 bytes".into()))?,
    );
    let payload_len =
        u64::from_le_bytes(data[45..53].try_into().map_err(|_| {
            TreeStreamError::Malformed("payload length slice is not 8 bytes".into())
        })?);
    let logical_len =
        u64::from_le_bytes(data[53..61].try_into().map_err(|_| {
            TreeStreamError::Malformed("logical length slice is not 8 bytes".into())
        })?);
    Ok(TreeHeader {
        version,
        tree_id,
        entry_count,
        payload_len,
        logical_len,
    })
}

pub(crate) fn decode_block_header(
    header: &TreeHeader,
    preamble: &[u8],
    object_len: u64,
) -> Result<TreeBlockHeader, TreeStreamError> {
    if header.version != TREE_BLOCK_ENCODING_VERSION {
        return Err(TreeStreamError::Malformed(
            "tree does not use block compression".into(),
        ));
    }
    if preamble.len() != TREE_BLOCK_PREAMBLE_LEN {
        return Err(TreeStreamError::TruncatedFrame {
            offset: TREE_HEADER_LEN as u64,
        });
    }
    if preamble[0] != TREE_BLOCK_CODEC_ZSTD {
        return Err(TreeStreamError::Malformed(format!(
            "unsupported tree block codec {}",
            preamble[0]
        )));
    }
    if preamble[1] != 0 {
        return Err(TreeStreamError::Malformed(
            "unsupported tree block flags".into(),
        ));
    }
    let block_entries = u16::from_le_bytes([preamble[2], preamble[3]]) as usize;
    if block_entries == 0 {
        return Err(TreeStreamError::Malformed(
            "tree block size must be nonzero".into(),
        ));
    }
    if block_entries > TREE_BLOCK_ENTRIES {
        return Err(TreeStreamError::Malformed(format!(
            "tree block entry count {block_entries} exceeds maximum {TREE_BLOCK_ENTRIES}"
        )));
    }
    let block_count = u32::from_le_bytes(
        preamble[4..8]
            .try_into()
            .map_err(|_| TreeStreamError::Malformed("invalid tree block count".into()))?,
    ) as usize;
    let raw_payload_len = u64::from_le_bytes(
        preamble[8..16]
            .try_into()
            .map_err(|_| TreeStreamError::Malformed("invalid raw tree payload length".into()))?,
    );
    let expected_blocks = header.entry_count.div_ceil(block_entries as u64);
    if block_count as u64 != expected_blocks {
        return Err(TreeStreamError::Malformed(
            "tree block count does not match entry count".into(),
        ));
    }
    let index_bytes = block_count
        .checked_mul(TREE_BLOCK_INDEX_LEN)
        .ok_or_else(|| TreeStreamError::Malformed("tree block index length overflow".into()))?;
    let index_end = TREE_HEADER_LEN
        .checked_add(TREE_BLOCK_PREAMBLE_LEN)
        .and_then(|len| len.checked_add(index_bytes))
        .ok_or_else(|| TreeStreamError::Malformed("tree block index end overflow".into()))?
        as u64;
    let expected_object_len = (TREE_HEADER_LEN as u64)
        .checked_add(header.payload_len)
        .ok_or_else(|| TreeStreamError::Malformed("tree object length overflow".into()))?;
    if index_end > expected_object_len || expected_object_len != object_len {
        return Err(TreeStreamError::TruncatedFrame { offset: object_len });
    }
    Ok(TreeBlockHeader {
        block_entries,
        block_count,
        entry_count: header.entry_count,
        raw_payload_len,
        index_end,
    })
}

pub(crate) fn decode_block_index(
    bytes: &[u8],
    block: usize,
    block_header: &TreeBlockHeader,
    object_len: u64,
) -> Result<TreeBlockIndex, TreeStreamError> {
    if bytes.len() != TREE_BLOCK_INDEX_LEN || block >= block_header.block_count {
        return Err(TreeStreamError::Malformed(
            "invalid tree block index entry".into(),
        ));
    }
    let raw_offset = u64::from_le_bytes(
        bytes[0..8]
            .try_into()
            .map_err(|_| TreeStreamError::Malformed("invalid tree block raw offset".into()))?,
    );
    let stored_offset = u64::from_le_bytes(
        bytes[8..16]
            .try_into()
            .map_err(|_| TreeStreamError::Malformed("invalid tree block stored offset".into()))?,
    );
    let stored_len = u32::from_le_bytes(
        bytes[16..20]
            .try_into()
            .map_err(|_| TreeStreamError::Malformed("invalid tree block stored length".into()))?,
    ) as usize;
    let raw_len = u32::from_le_bytes(
        bytes[20..24]
            .try_into()
            .map_err(|_| TreeStreamError::Malformed("invalid tree block raw length".into()))?,
    ) as usize;
    let first_entry = (block as u64)
        .checked_mul(block_header.block_entries as u64)
        .ok_or_else(|| TreeStreamError::Malformed("tree block ordinal overflow".into()))?;
    let entries = block_header
        .entry_count
        .checked_sub(first_entry)
        .ok_or_else(|| TreeStreamError::Malformed("tree block starts past entries".into()))?
        .min(block_header.block_entries as u64) as usize;
    let max_raw_len = entries
        .checked_mul(TREE_BLOCK_MAX_ENTRY_FRAME_LEN)
        .ok_or_else(|| TreeStreamError::Malformed("tree block raw length overflow".into()))?;
    validate_block_lengths(stored_len, raw_len, max_raw_len)?;
    if stored_offset < block_header.index_end {
        return Err(TreeStreamError::Malformed(
            "invalid empty or overlapping tree block".into(),
        ));
    }
    if stored_offset
        .checked_add(stored_len as u64)
        .is_none_or(|end| end > object_len)
    {
        return Err(TreeStreamError::TruncatedFrame {
            offset: stored_offset,
        });
    }
    Ok(TreeBlockIndex {
        raw_offset,
        stored_offset,
        stored_len,
        raw_len,
    })
}

fn validate_block_lengths(
    stored_len: usize,
    raw_len: usize,
    max_raw_len: usize,
) -> Result<(), TreeStreamError> {
    if raw_len > max_raw_len {
        return Err(TreeStreamError::Malformed(format!(
            "tree block raw length {raw_len} exceeds maximum {max_raw_len}"
        )));
    }
    if stored_len == 0 || raw_len == 0 {
        return Err(TreeStreamError::Malformed(
            "invalid empty or overlapping tree block".into(),
        ));
    }
    let expansion_limit = (stored_len as u64) * (TREE_BLOCK_MAX_EXPANSION_RATIO as u64);
    if raw_len as u64 > expansion_limit {
        return Err(TreeStreamError::Malformed(format!(
            "tree block raw length {raw_len} exceeds {TREE_BLOCK_MAX_EXPANSION_RATIO}:1 expansion limit for {stored_len} stored bytes"
        )));
    }
    Ok(())
}

pub(crate) fn decode_block_payload(
    stored: &[u8],
    raw_len: usize,
) -> Result<Vec<u8>, TreeStreamError> {
    validate_block_lengths(stored.len(), raw_len, TREE_BLOCK_MAX_RAW_LEN)?;
    if stored.len() == raw_len {
        return Ok(stored.to_vec());
    }
    let anchor_end = block_anchor_end(stored)?;
    if anchor_end >= raw_len {
        return Err(TreeStreamError::Malformed(
            "compressed tree block has no tail".into(),
        ));
    }
    let tail = decompress_block_tail(&stored[anchor_end..], raw_len - anchor_end)?;
    let mut raw = Vec::with_capacity(raw_len);
    raw.extend_from_slice(&stored[..anchor_end]);
    raw.extend_from_slice(&tail);
    if raw.len() != raw_len {
        return Err(TreeStreamError::Malformed(
            "decoded tree block length mismatch".into(),
        ));
    }
    Ok(raw)
}

pub(crate) fn block_anchor_end(stored: &[u8]) -> Result<usize, TreeStreamError> {
    let len_bytes = stored
        .get(..4)
        .ok_or(TreeStreamError::TruncatedFrame { offset: 0 })?;
    let frame_len = u32::from_le_bytes(
        len_bytes
            .try_into()
            .map_err(|_| TreeStreamError::Malformed("invalid anchor frame length".into()))?,
    ) as usize;
    let end = 4usize
        .checked_add(frame_len)
        .ok_or(TreeStreamError::TruncatedFrame { offset: 0 })?;
    if end > stored.len() {
        return Err(TreeStreamError::TruncatedFrame { offset: 0 });
    }
    Ok(end)
}

fn encode_blocked_htr4(raw: &[u8], level: i32) -> Result<Vec<u8>, TreeStreamError> {
    let header = decode_header(raw)?;
    if header.version != TREE_ENCODING_VERSION {
        return Err(TreeStreamError::Malformed(
            "block encoder requires raw HTR4".into(),
        ));
    }
    let ranges = entry_frame_ranges(raw, header.entry_count)?;
    let block_count = ranges.len().div_ceil(TREE_BLOCK_ENTRIES);
    let block_count_u32 = u32::try_from(block_count)
        .map_err(|_| TreeStreamError::Malformed("tree has too many blocks".into()))?;
    let mut blocks = Vec::with_capacity(block_count);
    for chunk in ranges.chunks(TREE_BLOCK_ENTRIES) {
        let (start, anchor_end) = *chunk
            .first()
            .ok_or_else(|| TreeStreamError::Malformed("empty tree block".into()))?;
        let end = chunk
            .last()
            .map(|range| range.1)
            .ok_or_else(|| TreeStreamError::Malformed("empty tree block".into()))?;
        let block = &raw[start..end];
        let compressed_tail = compress_block_tail(&raw[anchor_end..end], level)?;
        if anchor_end - start + compressed_tail.len() < block.len() {
            let mut stored = Vec::with_capacity(anchor_end - start + compressed_tail.len());
            stored.extend_from_slice(&raw[start..anchor_end]);
            stored.extend_from_slice(&compressed_tail);
            blocks.push(stored);
        } else {
            blocks.push(block.to_vec());
        }
    }

    let index_bytes = block_count
        .checked_mul(TREE_BLOCK_INDEX_LEN)
        .ok_or_else(|| TreeStreamError::Malformed("tree block index length overflow".into()))?;
    let stored_payload_len = TREE_BLOCK_PREAMBLE_LEN
        .checked_add(index_bytes)
        .and_then(|len| {
            blocks
                .iter()
                .try_fold(len, |total, block| total.checked_add(block.len()))
        })
        .ok_or_else(|| TreeStreamError::Malformed("blocked tree length overflow".into()))?;
    let mut out = Vec::with_capacity(TREE_HEADER_LEN + stored_payload_len);
    out.extend_from_slice(TREE_CANONICAL_MAGIC);
    out.push(TREE_BLOCK_ENCODING_VERSION);
    out.extend_from_slice(header.tree_id.as_bytes());
    out.extend_from_slice(&header.entry_count.to_le_bytes());
    out.extend_from_slice(&(stored_payload_len as u64).to_le_bytes());
    out.extend_from_slice(&header.logical_len.to_le_bytes());
    out.push(TREE_BLOCK_CODEC_ZSTD);
    out.push(0);
    out.extend_from_slice(&(TREE_BLOCK_ENTRIES as u16).to_le_bytes());
    out.extend_from_slice(&block_count_u32.to_le_bytes());
    out.extend_from_slice(&header.payload_len.to_le_bytes());

    let mut stored_offset = TREE_HEADER_LEN
        .checked_add(TREE_BLOCK_PREAMBLE_LEN)
        .and_then(|len| len.checked_add(index_bytes))
        .ok_or_else(|| TreeStreamError::Malformed("tree block offset overflow".into()))?;
    for (block, stored) in blocks.iter().enumerate() {
        let first_entry = block * TREE_BLOCK_ENTRIES;
        let chunk = &ranges[first_entry..(first_entry + TREE_BLOCK_ENTRIES).min(ranges.len())];
        let raw_offset = chunk[0].0 - TREE_HEADER_LEN;
        let raw_len = chunk[chunk.len() - 1].1 - chunk[0].0;
        let stored_len = u32::try_from(stored.len())
            .map_err(|_| TreeStreamError::Malformed("stored tree block exceeds u32".into()))?;
        let raw_len = u32::try_from(raw_len)
            .map_err(|_| TreeStreamError::Malformed("raw tree block exceeds u32".into()))?;
        out.extend_from_slice(&(raw_offset as u64).to_le_bytes());
        out.extend_from_slice(&(stored_offset as u64).to_le_bytes());
        out.extend_from_slice(&stored_len.to_le_bytes());
        out.extend_from_slice(&raw_len.to_le_bytes());
        stored_offset = stored_offset
            .checked_add(stored.len())
            .ok_or_else(|| TreeStreamError::Malformed("tree block offset overflow".into()))?;
    }
    for block in blocks {
        out.extend_from_slice(&block);
    }
    Ok(out)
}

fn entry_frame_ranges(
    raw: &[u8],
    entry_count: u64,
) -> Result<Vec<(usize, usize)>, TreeStreamError> {
    let count = usize::try_from(entry_count)
        .map_err(|_| TreeStreamError::Malformed("tree entry count exceeds usize".into()))?;
    let mut ranges = Vec::with_capacity(count);
    let mut offset = TREE_HEADER_LEN;
    for _ in 0..count {
        let len_bytes = raw
            .get(offset..offset + 4)
            .ok_or(TreeStreamError::TruncatedFrame {
                offset: offset as u64,
            })?;
        let frame_len = u32::from_le_bytes(
            len_bytes
                .try_into()
                .map_err(|_| TreeStreamError::Malformed("invalid frame length".into()))?,
        ) as usize;
        let end = offset
            .checked_add(4)
            .and_then(|start| start.checked_add(frame_len))
            .ok_or(TreeStreamError::TruncatedFrame {
                offset: offset as u64,
            })?;
        if end > raw.len() {
            return Err(TreeStreamError::TruncatedFrame {
                offset: offset as u64,
            });
        }
        ranges.push((offset, end));
        offset = end;
    }
    if offset != raw.len() {
        return Err(TreeStreamError::TrailingBytes {
            extra: raw.len().abs_diff(offset) as u64,
        });
    }
    Ok(ranges)
}

#[cfg(feature = "zstd")]
fn compress_block_tail(data: &[u8], level: i32) -> Result<Vec<u8>, TreeStreamError> {
    zstd::bulk::compress(data, level)
        .map_err(|error| TreeStreamError::Compression(error.to_string()))
}

#[cfg(not(feature = "zstd"))]
fn compress_block_tail(_data: &[u8], _level: i32) -> Result<Vec<u8>, TreeStreamError> {
    Err(TreeStreamError::Compression(
        "zstd support is not compiled into this build".into(),
    ))
}

#[cfg(feature = "zstd")]
fn decompress_block_tail(data: &[u8], capacity: usize) -> Result<Vec<u8>, TreeStreamError> {
    if capacity > TREE_BLOCK_MAX_RAW_LEN {
        return Err(TreeStreamError::Malformed(format!(
            "tree block tail length {capacity} exceeds maximum {TREE_BLOCK_MAX_RAW_LEN}"
        )));
    }
    let decoded = zstd::bulk::decompress(data, capacity)
        .map_err(|error| TreeStreamError::Compression(error.to_string()))?;
    if decoded.len() != capacity {
        return Err(TreeStreamError::Malformed(
            "decoded tree block tail length mismatch".into(),
        ));
    }
    Ok(decoded)
}

#[cfg(not(feature = "zstd"))]
fn decompress_block_tail(_data: &[u8], _capacity: usize) -> Result<Vec<u8>, TreeStreamError> {
    Err(TreeStreamError::Compression(
        "zstd support is not compiled into this build".into(),
    ))
}

/// A cumulative HDC1 operation against a materialized anchor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TreeDeltaOp {
    Remove(String),
    Upsert(TreeEntry),
}

impl TreeDeltaOp {
    pub fn name(&self) -> &str {
        match self {
            Self::Remove(name) => name,
            Self::Upsert(entry) => entry.name(),
        }
    }
}

/// Parsed HDC1 header, including its bounded first-entry and first-100 porch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TreeDeltaHeader {
    pub anchor: ContentHash,
    pub result_count: usize,
    pub op_count: usize,
    pub first_op_count: usize,
    pub first_base_count: usize,
    pub first_end: usize,
    pub hundred_op_count: usize,
    pub hundred_base_count: usize,
    pub hundred_end: usize,
}

impl Tree {
    /// Encode a cheap HLR1 materialized anchor. The content hash deliberately
    /// stays outside the body and must be supplied by the object store while
    /// decoding.
    pub fn encode_lean(&self) -> Result<Vec<u8>, TreeStreamError> {
        self.validate()?;
        encode_lean_entries(self.entries())
    }

    /// Decode a complete HLR1 anchor and validate it against its object key.
    pub fn decode_lean(data: &[u8], expected: ContentHash) -> Result<Self, TreeStreamError> {
        let (entries, consumed) = decode_lean_entries(data, usize::MAX)?;
        if consumed != data.len() {
            return Err(TreeStreamError::TrailingBytes {
                extra: data.len().abs_diff(consumed) as u64,
            });
        }
        let tree = Tree::try_from_decoded_entries(entries)?;
        let found = tree.hash();
        if found != expected {
            return Err(TreeStreamError::HashMismatch { expected, found });
        }
        Ok(tree)
    }
}

/// Decode at most `wanted` entries from the compact HLR1 porch. The returned
/// byte count is the exact prefix a file-backed reader had to consume.
pub fn decode_lean_prefix(
    data: &[u8],
    wanted: usize,
) -> Result<(Vec<TreeEntry>, usize), TreeStreamError> {
    decode_lean_entries(data, wanted)
}

fn encode_lean_entries(entries: &[TreeEntry]) -> Result<Vec<u8>, TreeStreamError> {
    let mut out = Vec::new();
    out.extend_from_slice(TREE_LEAN_MAGIC);
    put_varint(entries.len(), &mut out);
    let mut previous = "";
    for entry in entries {
        encode_lean_entry(entry, previous, &mut out)?;
        previous = entry.name();
    }
    Ok(out)
}

fn decode_lean_entries(
    data: &[u8],
    wanted: usize,
) -> Result<(Vec<TreeEntry>, usize), TreeStreamError> {
    if !is_lean_tree(data) {
        return Err(TreeStreamError::Malformed(
            "bytes are not an HLR1 tree anchor".into(),
        ));
    }
    let mut offset = TREE_LEAN_MAGIC.len();
    let count = take_varint(data, &mut offset)?;
    let wanted = wanted.min(count);
    let mut entries = Vec::with_capacity(wanted);
    let mut previous = String::new();
    for _ in 0..wanted {
        let entry = decode_compact_entry(data, &mut offset, &previous)?;
        if !previous.is_empty() && previous.as_str() >= entry.name() {
            return Err(TreeError::InvalidStructure(
                "entries must be strictly sorted by name".into(),
            )
            .into());
        }
        previous = entry.name().to_string();
        entries.push(entry);
    }
    if wanted == count && offset != data.len() {
        return Err(TreeStreamError::TrailingBytes {
            extra: data.len().abs_diff(offset) as u64,
        });
    }
    Ok((entries, offset))
}

fn put_varint(mut value: usize, out: &mut Vec<u8>) {
    while value >= 0x80 {
        out.push((value as u8) | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

pub(crate) fn take_varint(bytes: &[u8], offset: &mut usize) -> Result<usize, TreeStreamError> {
    let mut value = 0usize;
    for shift in (0..usize::BITS).step_by(7) {
        let byte = *bytes.get(*offset).ok_or(TreeStreamError::TruncatedFrame {
            offset: *offset as u64,
        })?;
        *offset += 1;
        value |= ((byte & 0x7f) as usize)
            .checked_shl(shift)
            .ok_or_else(|| TreeStreamError::Malformed("varint overflow".into()))?;
        if byte & 0x80 == 0 {
            return Ok(value);
        }
    }
    Err(TreeStreamError::Malformed("varint overflow".into()))
}

fn shared_prefix(left: &str, right: &str) -> usize {
    left.as_bytes()
        .iter()
        .zip(right.as_bytes())
        .take_while(|(left, right)| left == right)
        .count()
}

/// Append one HLR1 entry using `previous_name` as its prefix-compression base.
/// Store readers use this to expose a lazily merged HDC1 body as HLR1 bytes.
pub fn encode_lean_entry(
    entry: &TreeEntry,
    previous_name: &str,
    out: &mut Vec<u8>,
) -> Result<(), TreeStreamError> {
    out.push((entry.mode().to_byte() << 3) | entry.entry_type().to_byte());
    let prefix = shared_prefix(previous_name, entry.name());
    put_varint(prefix, out);
    put_varint(entry.name().len() - prefix, out);
    out.extend_from_slice(&entry.name().as_bytes()[prefix..]);
    match entry.entry_type() {
        EntryType::Blob | EntryType::Tree | EntryType::Symlink => {
            let hash = entry.content_hash().ok_or_else(|| {
                TreeStreamError::Malformed("compact entry is missing its content hash".into())
            })?;
            out.extend_from_slice(hash.as_bytes());
        }
        EntryType::Gitlink => {
            let target = entry.gitlink_target().ok_or_else(|| {
                TreeStreamError::Malformed("compact gitlink is missing its target".into())
            })?;
            out.push(git_format_to_tag(target.format()));
            out.extend_from_slice(target.as_bytes());
        }
        EntryType::Spoollink => {
            let (spool, state) = entry.spoollink_target().ok_or_else(|| {
                TreeStreamError::Malformed("compact spoollink is missing its target".into())
            })?;
            put_varint(spool.as_str().len(), out);
            out.extend_from_slice(spool.as_str().as_bytes());
            out.extend_from_slice(state.as_bytes());
        }
    }
    Ok(())
}

pub(crate) fn decode_compact_entry(
    bytes: &[u8],
    offset: &mut usize,
    previous_name: &str,
) -> Result<TreeEntry, TreeStreamError> {
    let tag = *bytes.get(*offset).ok_or(TreeStreamError::TruncatedFrame {
        offset: *offset as u64,
    })?;
    *offset += 1;
    let mode = FileMode::from_byte(tag >> 3).ok_or_else(|| {
        TreeStreamError::Malformed(format!("invalid compact entry mode {}", tag >> 3))
    })?;
    let kind = EntryType::from_byte(tag & 0x07).ok_or_else(|| {
        TreeStreamError::Malformed(format!("invalid compact entry kind {}", tag & 0x07))
    })?;
    let prefix = take_varint(bytes, offset)?;
    let suffix_len = take_varint(bytes, offset)?;
    if prefix > previous_name.len() {
        return Err(TreeStreamError::Malformed(
            "compact name prefix exceeds predecessor".into(),
        ));
    }
    let suffix_end = offset
        .checked_add(suffix_len)
        .ok_or_else(|| TreeStreamError::Malformed("compact name length overflow".into()))?;
    let suffix = bytes
        .get(*offset..suffix_end)
        .ok_or(TreeStreamError::TruncatedFrame {
            offset: *offset as u64,
        })?;
    let mut name = previous_name.as_bytes()[..prefix].to_vec();
    name.extend_from_slice(suffix);
    let name = String::from_utf8(name)
        .map_err(|_| TreeStreamError::Malformed("compact entry name is not UTF-8".into()))?;
    *offset = suffix_end;
    let entry = match kind {
        EntryType::Blob | EntryType::Tree | EntryType::Symlink => {
            let end = offset
                .checked_add(32)
                .ok_or_else(|| TreeStreamError::Malformed("compact hash overflow".into()))?;
            let hash = ContentHash::from_bytes(
                bytes
                    .get(*offset..end)
                    .ok_or(TreeStreamError::TruncatedFrame {
                        offset: *offset as u64,
                    })?
                    .try_into()
                    .map_err(|_| {
                        TreeStreamError::Malformed("compact hash is not 32 bytes".into())
                    })?,
            );
            *offset = end;
            match kind {
                EntryType::Blob => TreeEntry::file(name, hash, mode == FileMode::Executable)?,
                EntryType::Tree => TreeEntry::directory(name, hash)?,
                EntryType::Symlink => TreeEntry::symlink(name, hash)?,
                EntryType::Gitlink | EntryType::Spoollink => {
                    return Err(TreeStreamError::Malformed(
                        "invalid compact content-addressed kind".into(),
                    ));
                }
            }
        }
        EntryType::Gitlink => {
            let format_tag = *bytes.get(*offset).ok_or(TreeStreamError::TruncatedFrame {
                offset: *offset as u64,
            })?;
            *offset += 1;
            let format = git_format_from_tag(format_tag)?;
            let oid_len = match format {
                GitObjectFormat::Sha1 => 20,
                GitObjectFormat::Sha256 => 32,
            };
            let end = offset.checked_add(oid_len).ok_or_else(|| {
                TreeStreamError::Malformed("compact gitlink length overflow".into())
            })?;
            let target = GitObjectId::from_raw(
                format,
                bytes
                    .get(*offset..end)
                    .ok_or(TreeStreamError::TruncatedFrame {
                        offset: *offset as u64,
                    })?,
            )
            .map_err(|error| {
                TreeStreamError::Malformed(format!("invalid compact gitlink: {error}"))
            })?;
            *offset = end;
            TreeEntry::gitlink(name, target)?
        }
        EntryType::Spoollink => {
            let spool_len = take_varint(bytes, offset)?;
            let spool_end = offset.checked_add(spool_len).ok_or_else(|| {
                TreeStreamError::Malformed("compact spool length overflow".into())
            })?;
            let spool = std::str::from_utf8(bytes.get(*offset..spool_end).ok_or(
                TreeStreamError::TruncatedFrame {
                    offset: *offset as u64,
                },
            )?)
            .map_err(|_| TreeStreamError::Malformed("compact spool id is not UTF-8".into()))?;
            *offset = spool_end;
            let state_end = offset
                .checked_add(32)
                .ok_or_else(|| TreeStreamError::Malformed("compact state overflow".into()))?;
            let state = StateId::from_bytes(
                bytes
                    .get(*offset..state_end)
                    .ok_or(TreeStreamError::TruncatedFrame {
                        offset: *offset as u64,
                    })?
                    .try_into()
                    .map_err(|_| {
                        TreeStreamError::Malformed("compact state is not 32 bytes".into())
                    })?,
            );
            *offset = state_end;
            let spool_id = SpoolId::parse(spool).map_err(|error| {
                TreeStreamError::Malformed(format!("invalid compact spool id: {error}"))
            })?;
            TreeEntry::spoollink(name, spool_id, state)?
        }
    };
    if entry.mode() != mode {
        return Err(TreeStreamError::Malformed(format!(
            "compact entry kind/mode mismatch for {}: {kind:?}/{mode:?}",
            entry.name()
        )));
    }
    Ok(entry)
}

/// Compute the sorted cumulative edit from `anchor` to `current`.
pub fn tree_delta(anchor: &Tree, current: &Tree) -> Vec<TreeDeltaOp> {
    let mut ops = Vec::new();
    let mut anchor_index = 0usize;
    let mut current_index = 0usize;
    while anchor_index < anchor.len() || current_index < current.len() {
        match (
            anchor.entries().get(anchor_index),
            current.entries().get(current_index),
        ) {
            (Some(anchor_entry), Some(current_entry)) => {
                match anchor_entry.name().cmp(current_entry.name()) {
                    std::cmp::Ordering::Less => {
                        ops.push(TreeDeltaOp::Remove(anchor_entry.name().to_string()));
                        anchor_index += 1;
                    }
                    std::cmp::Ordering::Greater => {
                        ops.push(TreeDeltaOp::Upsert(current_entry.clone()));
                        current_index += 1;
                    }
                    std::cmp::Ordering::Equal => {
                        if anchor_entry != current_entry {
                            ops.push(TreeDeltaOp::Upsert(current_entry.clone()));
                        }
                        anchor_index += 1;
                        current_index += 1;
                    }
                }
            }
            (Some(anchor_entry), None) => {
                ops.push(TreeDeltaOp::Remove(anchor_entry.name().to_string()));
                anchor_index += 1;
            }
            (None, Some(current_entry)) => {
                ops.push(TreeDeltaOp::Upsert(current_entry.clone()));
                current_index += 1;
            }
            (None, None) => break,
        }
    }
    ops
}

/// Apply a sorted cumulative delta to its materialized anchor.
pub fn apply_tree_delta(anchor: &Tree, ops: &[TreeDeltaOp]) -> Result<Tree, TreeStreamError> {
    let mut entries = Vec::with_capacity(anchor.len().saturating_add(ops.len()));
    let mut anchor_index = 0usize;
    let mut op_index = 0usize;
    while anchor_index < anchor.len() || op_index < ops.len() {
        match (anchor.entries().get(anchor_index), ops.get(op_index)) {
            (Some(anchor_entry), Some(op)) => match anchor_entry.name().cmp(op.name()) {
                std::cmp::Ordering::Less => {
                    entries.push(anchor_entry.clone());
                    anchor_index += 1;
                }
                std::cmp::Ordering::Greater => {
                    if let TreeDeltaOp::Upsert(entry) = op {
                        entries.push(entry.clone());
                    }
                    op_index += 1;
                }
                std::cmp::Ordering::Equal => {
                    if let TreeDeltaOp::Upsert(entry) = op {
                        entries.push(entry.clone());
                    }
                    anchor_index += 1;
                    op_index += 1;
                }
            },
            (Some(anchor_entry), None) => {
                entries.push(anchor_entry.clone());
                anchor_index += 1;
            }
            (None, Some(op)) => {
                if let TreeDeltaOp::Upsert(entry) = op {
                    entries.push(entry.clone());
                }
                op_index += 1;
            }
            (None, None) => break,
        }
    }
    Tree::try_from_decoded_entries(entries).map_err(TreeStreamError::from)
}

fn delta_prefix_counts(
    anchor: &Tree,
    current: &Tree,
    ops: &[TreeDeltaOp],
    count: usize,
) -> Result<(u16, u16), TreeStreamError> {
    if current.is_empty() {
        return Ok((0, 0));
    }
    let boundary = current.entries()[count.min(current.len()) - 1].name();
    let op_count = ops.partition_point(|op| op.name() <= boundary);
    let base_count = anchor
        .entries()
        .partition_point(|entry| entry.name() <= boundary);
    Ok((
        u16::try_from(op_count)
            .map_err(|_| TreeStreamError::Malformed("delta porch op count exceeds u16".into()))?,
        u16::try_from(base_count).map_err(|_| {
            TreeStreamError::Malformed("delta porch anchor count exceeds u16".into())
        })?,
    ))
}

/// Encode a one-hop HDC1 body against `anchor`.
pub fn encode_tree_delta(
    anchor_id: ContentHash,
    anchor: &Tree,
    current: &Tree,
    ops: &[TreeDeltaOp],
) -> Result<Vec<u8>, TreeStreamError> {
    anchor.validate()?;
    current.validate()?;
    if anchor.hash() != anchor_id {
        return Err(TreeStreamError::HashMismatch {
            expected: anchor_id,
            found: anchor.hash(),
        });
    }
    if ops.len() > TREE_DELTA_MAX_OPS {
        return Err(TreeStreamError::Malformed(format!(
            "tree delta has {} operations; maximum is {TREE_DELTA_MAX_OPS}",
            ops.len()
        )));
    }
    if apply_tree_delta(anchor, ops)? != *current {
        return Err(TreeStreamError::Malformed(
            "tree delta operations do not reconstruct the result".into(),
        ));
    }
    let result_count = u32::try_from(current.len())
        .map_err(|_| TreeStreamError::Malformed("delta result count exceeds u32".into()))?;
    let op_count = u16::try_from(ops.len())
        .map_err(|_| TreeStreamError::Malformed("delta operation count exceeds u16".into()))?;
    let (first_ops, first_base) = delta_prefix_counts(anchor, current, ops, 1)?;
    let (hundred_ops, hundred_base) = delta_prefix_counts(anchor, current, ops, 100)?;
    let mut body = Vec::new();
    let mut ends = Vec::with_capacity(ops.len());
    let mut previous = "";
    for op in ops {
        match op {
            TreeDeltaOp::Remove(name) => {
                body.push(0);
                let prefix = shared_prefix(previous, name);
                put_varint(prefix, &mut body);
                put_varint(name.len() - prefix, &mut body);
                body.extend_from_slice(&name.as_bytes()[prefix..]);
            }
            TreeDeltaOp::Upsert(entry) => {
                body.push(1);
                encode_lean_entry(entry, previous, &mut body)?;
            }
        }
        if !previous.is_empty() && previous >= op.name() {
            return Err(TreeStreamError::Malformed(
                "tree delta operations must be strictly sorted".into(),
            ));
        }
        previous = op.name();
        ends.push(body.len());
    }
    let end_for = |count: u16| -> Result<u32, TreeStreamError> {
        let end = if count == 0 {
            TREE_DELTA_HEADER_LEN
        } else {
            TREE_DELTA_HEADER_LEN
                .checked_add(*ends.get(count as usize - 1).ok_or_else(|| {
                    TreeStreamError::Malformed("delta porch exceeds operation count".into())
                })?)
                .ok_or_else(|| TreeStreamError::Malformed("delta porch offset overflow".into()))?
        };
        u32::try_from(end)
            .map_err(|_| TreeStreamError::Malformed("delta porch offset exceeds u32".into()))
    };
    let mut out = Vec::with_capacity(TREE_DELTA_HEADER_LEN + body.len());
    out.extend_from_slice(TREE_DELTA_MAGIC);
    out.push(TREE_DELTA_ENCODING_VERSION);
    out.extend_from_slice(anchor_id.as_bytes());
    out.extend_from_slice(&result_count.to_le_bytes());
    out.extend_from_slice(&op_count.to_le_bytes());
    out.extend_from_slice(&first_ops.to_le_bytes());
    out.extend_from_slice(&first_base.to_le_bytes());
    out.extend_from_slice(&end_for(first_ops)?.to_le_bytes());
    out.extend_from_slice(&hundred_ops.to_le_bytes());
    out.extend_from_slice(&hundred_base.to_le_bytes());
    out.extend_from_slice(&end_for(hundred_ops)?.to_le_bytes());
    if out.len() != TREE_DELTA_HEADER_LEN {
        return Err(TreeStreamError::Malformed(
            "internal HDC1 header length mismatch".into(),
        ));
    }
    out.extend_from_slice(&body);
    Ok(out)
}

/// Parse and validate the fixed HDC1 header without reading its operations.
pub fn decode_tree_delta_header(data: &[u8]) -> Result<TreeDeltaHeader, TreeStreamError> {
    decode_tree_delta_header_prefix(data, data.len())
}

/// Parse an HDC1 header from a bounded prefix while validating its offsets
/// against the complete file length.
pub fn decode_tree_delta_header_prefix(
    data: &[u8],
    object_len: usize,
) -> Result<TreeDeltaHeader, TreeStreamError> {
    if data.len() < TREE_DELTA_HEADER_LEN {
        return Err(TreeStreamError::TruncatedFrame { offset: 0 });
    }
    if !is_delta_tree(data) {
        return Err(TreeStreamError::Malformed(
            "bytes are not an HDC1 tree delta".into(),
        ));
    }
    if data[4] != TREE_DELTA_ENCODING_VERSION {
        return Err(TreeStreamError::UnsupportedVersion { found: data[4] });
    }
    let anchor = ContentHash::from_bytes(
        data[5..37]
            .try_into()
            .map_err(|_| TreeStreamError::Malformed("delta anchor hash is not 32 bytes".into()))?,
    );
    let header = TreeDeltaHeader {
        anchor,
        result_count: read_u32_at(data, 37)? as usize,
        op_count: read_u16_at(data, 41)? as usize,
        first_op_count: read_u16_at(data, 43)? as usize,
        first_base_count: read_u16_at(data, 45)? as usize,
        first_end: read_u32_at(data, 47)? as usize,
        hundred_op_count: read_u16_at(data, 51)? as usize,
        hundred_base_count: read_u16_at(data, 53)? as usize,
        hundred_end: read_u32_at(data, 55)? as usize,
    };
    if header.op_count > TREE_DELTA_MAX_OPS
        || header.first_op_count > header.op_count
        || header.hundred_op_count > header.op_count
        || header.first_end < TREE_DELTA_HEADER_LEN
        || header.hundred_end < header.first_end
        || header.hundred_end > object_len
    {
        return Err(TreeStreamError::Malformed(
            "invalid HDC1 porch or operation bounds".into(),
        ));
    }
    Ok(header)
}

/// Decode `wanted` HDC1 operations. Used by bounded porch reads as well as
/// full reconstruction.
pub fn decode_tree_delta_ops(
    data: &[u8],
    wanted: usize,
) -> Result<(TreeDeltaHeader, Vec<TreeDeltaOp>, usize), TreeStreamError> {
    decode_tree_delta_ops_prefix(data, data.len(), wanted)
}

/// Decode an HDC1 operation porch without transferring the rest of the body.
pub fn decode_tree_delta_ops_prefix(
    data: &[u8],
    object_len: usize,
    wanted: usize,
) -> Result<(TreeDeltaHeader, Vec<TreeDeltaOp>, usize), TreeStreamError> {
    let header = decode_tree_delta_header_prefix(data, object_len)?;
    if wanted > header.op_count {
        return Err(TreeStreamError::Malformed(
            "partial delta operation count exceeds object".into(),
        ));
    }
    let mut offset = TREE_DELTA_HEADER_LEN;
    let mut previous = String::new();
    let mut ops = Vec::with_capacity(wanted);
    for _ in 0..wanted {
        let opcode = *data.get(offset).ok_or(TreeStreamError::TruncatedFrame {
            offset: offset as u64,
        })?;
        offset += 1;
        let op =
            match opcode {
                0 => {
                    let prefix = take_varint(data, &mut offset)?;
                    let suffix_len = take_varint(data, &mut offset)?;
                    if prefix > previous.len() {
                        return Err(TreeStreamError::Malformed(
                            "delta name prefix exceeds predecessor".into(),
                        ));
                    }
                    let end = offset.checked_add(suffix_len).ok_or_else(|| {
                        TreeStreamError::Malformed("delta name length overflow".into())
                    })?;
                    let mut name = previous.as_bytes()[..prefix].to_vec();
                    name.extend_from_slice(data.get(offset..end).ok_or(
                        TreeStreamError::TruncatedFrame {
                            offset: offset as u64,
                        },
                    )?);
                    offset = end;
                    TreeDeltaOp::Remove(String::from_utf8(name).map_err(|_| {
                        TreeStreamError::Malformed("delta name is not UTF-8".into())
                    })?)
                }
                1 => TreeDeltaOp::Upsert(decode_compact_entry(data, &mut offset, &previous)?),
                _ => {
                    return Err(TreeStreamError::Malformed(format!(
                        "invalid tree delta opcode {opcode}"
                    )));
                }
            };
        if !previous.is_empty() && previous.as_str() >= op.name() {
            return Err(TreeStreamError::Malformed(
                "tree delta operations must be strictly sorted".into(),
            ));
        }
        previous = op.name().to_string();
        ops.push(op);
    }
    if (wanted == header.first_op_count && offset != header.first_end)
        || (wanted == header.hundred_op_count && offset != header.hundred_end)
    {
        return Err(TreeStreamError::Malformed(
            "HDC1 porch offset does not match decoded operations".into(),
        ));
    }
    Ok((header, ops, offset))
}

/// Reconstruct an HDC1 tree from exactly one materialized anchor and validate
/// the result against its external object key.
pub fn decode_tree_delta(
    data: &[u8],
    anchor: &Tree,
    expected: ContentHash,
) -> Result<Tree, TreeStreamError> {
    let header = decode_tree_delta_header(data)?;
    let (decoded_header, ops, consumed) = decode_tree_delta_ops(data, header.op_count)?;
    if consumed != data.len() {
        return Err(TreeStreamError::TrailingBytes {
            extra: data.len().abs_diff(consumed) as u64,
        });
    }
    let anchor_found = anchor.hash();
    if anchor_found != decoded_header.anchor {
        return Err(TreeStreamError::HashMismatch {
            expected: decoded_header.anchor,
            found: anchor_found,
        });
    }
    let tree = apply_tree_delta(anchor, &ops)?;
    if tree.len() != decoded_header.result_count {
        return Err(TreeStreamError::Malformed(
            "delta result count does not match reconstructed tree".into(),
        ));
    }
    let found = tree.hash();
    if found != expected {
        return Err(TreeStreamError::HashMismatch { expected, found });
    }
    Ok(tree)
}

fn read_u16_at(data: &[u8], offset: usize) -> Result<u16, TreeStreamError> {
    Ok(u16::from_le_bytes(
        data.get(offset..offset + 2)
            .ok_or(TreeStreamError::TruncatedFrame {
                offset: offset as u64,
            })?
            .try_into()
            .map_err(|_| TreeStreamError::Malformed("invalid u16 field".into()))?,
    ))
}

fn read_u32_at(data: &[u8], offset: usize) -> Result<u32, TreeStreamError> {
    Ok(u32::from_le_bytes(
        data.get(offset..offset + 4)
            .ok_or(TreeStreamError::TruncatedFrame {
                offset: offset as u64,
            })?
            .try_into()
            .map_err(|_| TreeStreamError::Malformed("invalid u32 field".into()))?,
    ))
}

pub(crate) fn encode_entry_frame(entry: &TreeEntry) -> Result<Vec<u8>, TreeStreamError> {
    let name = entry.name().as_bytes();
    let name_len = u16::try_from(name.len()).map_err(|_| {
        TreeStreamError::Malformed(format!("entry name '{}' exceeds u16", entry.name()))
    })?;
    let mut frame = Vec::new();
    frame.push(entry.mode().to_byte());
    frame.push(entry.entry_type().to_byte());
    frame.extend_from_slice(&name_len.to_le_bytes());
    frame.extend_from_slice(name);
    encode_target(&mut frame, entry)?;
    Ok(frame)
}

pub(crate) fn decode_entry_at(
    data: &[u8],
    offset: usize,
    payload_end: usize,
) -> Result<(TreeEntry, usize), TreeStreamError> {
    if offset + 4 > payload_end {
        return Err(TreeStreamError::TruncatedFrame {
            offset: offset as u64,
        });
    }
    let frame_len = u32::from_le_bytes(
        data[offset..offset + 4]
            .try_into()
            .map_err(|_| TreeStreamError::Malformed("frame length slice is not 4 bytes".into()))?,
    ) as usize;
    let frame_start = offset + 4;
    let frame_end = frame_start
        .checked_add(frame_len)
        .ok_or(TreeStreamError::TruncatedFrame {
            offset: offset as u64,
        })?;
    if frame_end > payload_end {
        return Err(TreeStreamError::TruncatedFrame {
            offset: offset as u64,
        });
    }
    let entry = decode_entry_frame(&data[frame_start..frame_end])?;
    Ok((entry, 4 + frame_len))
}

pub(crate) fn decode_entry_frame(frame: &[u8]) -> Result<TreeEntry, TreeStreamError> {
    if frame.len() < 4 {
        return Err(TreeStreamError::TruncatedFrame { offset: 0 });
    }
    let mode = FileMode::from_byte(frame[0]).ok_or_else(|| {
        TreeStreamError::Malformed(format!("malformed tree entry mode {}", frame[0]))
    })?;
    let kind = EntryType::from_byte(frame[1]).ok_or_else(|| {
        TreeStreamError::Malformed(format!("malformed tree entry kind {}", frame[1]))
    })?;
    let name_len = u16::from_le_bytes([frame[2], frame[3]]) as usize;
    let name_end = 4 + name_len;
    if frame.len() < name_end {
        return Err(TreeStreamError::TruncatedFrame { offset: 0 });
    }
    let name = std::str::from_utf8(&frame[4..name_end])
        .map_err(|_| TreeStreamError::Malformed("tree entry name is not UTF-8".into()))?
        .to_string();
    let entry = decode_target(name, kind, mode, &frame[name_end..])?;
    if entry.mode() != mode {
        return Err(TreeStreamError::Malformed(format!(
            "tree kind/mode mismatch for {}: {kind:?}/{mode:?}",
            entry.name()
        )));
    }
    Ok(entry)
}

fn encode_target(frame: &mut Vec<u8>, entry: &TreeEntry) -> Result<(), TreeStreamError> {
    match entry.entry_type() {
        EntryType::Blob | EntryType::Tree | EntryType::Symlink => {
            frame.extend_from_slice(entry.require_content_hash().as_bytes());
        }
        EntryType::Gitlink => {
            let target = entry.gitlink_target().ok_or_else(|| {
                TreeStreamError::Malformed("gitlink entry is missing target".into())
            })?;
            frame.push(git_format_to_tag(target.format()));
            frame.extend_from_slice(target.as_bytes());
        }
        EntryType::Spoollink => {
            let (spool, state) = entry.spoollink_target().ok_or_else(|| {
                TreeStreamError::Malformed("spoollink entry is missing target".into())
            })?;
            let spool_bytes = spool.as_str().as_bytes();
            let spool_len = u16::try_from(spool_bytes.len())
                .map_err(|_| TreeStreamError::Malformed("spool id exceeds u16".into()))?;
            frame.extend_from_slice(&spool_len.to_le_bytes());
            frame.extend_from_slice(spool_bytes);
            frame.extend_from_slice(state.as_bytes());
        }
    }
    Ok(())
}

fn decode_target(
    name: String,
    kind: EntryType,
    mode: FileMode,
    payload: &[u8],
) -> Result<TreeEntry, TreeStreamError> {
    match kind {
        EntryType::Blob => TreeEntry::file(name, take_hash(payload)?, mode == FileMode::Executable)
            .map_err(TreeStreamError::from),
        EntryType::Tree => {
            TreeEntry::directory(name, take_hash(payload)?).map_err(TreeStreamError::from)
        }
        EntryType::Symlink => {
            TreeEntry::symlink(name, take_hash(payload)?).map_err(TreeStreamError::from)
        }
        EntryType::Gitlink => decode_gitlink(name, payload),
        EntryType::Spoollink => decode_spoollink(name, payload),
    }
}

fn take_hash(payload: &[u8]) -> Result<ContentHash, TreeStreamError> {
    let bytes: [u8; 32] = payload
        .try_into()
        .map_err(|_| TreeStreamError::Malformed("malformed tree entry object id".into()))?;
    Ok(ContentHash::from_bytes(bytes))
}

fn decode_gitlink(name: String, payload: &[u8]) -> Result<TreeEntry, TreeStreamError> {
    if payload.is_empty() {
        return Err(TreeStreamError::Malformed(
            "malformed tree entry object id".into(),
        ));
    }
    let format = git_format_from_tag(payload[0])?;
    let oid = &payload[1..];
    let expected = match format {
        GitObjectFormat::Sha1 => 20,
        GitObjectFormat::Sha256 => 32,
    };
    if oid.len() != expected {
        return Err(TreeStreamError::Malformed(
            "malformed tree entry object id".into(),
        ));
    }
    let target = GitObjectId::from_raw(format, oid)
        .map_err(|err| TreeError::InvalidStructure(format!("invalid gitlink target: {err}")))?;
    TreeEntry::gitlink(name, target).map_err(TreeStreamError::from)
}

fn decode_spoollink(name: String, payload: &[u8]) -> Result<TreeEntry, TreeStreamError> {
    if payload.len() < 2 {
        return Err(TreeStreamError::TruncatedFrame { offset: 0 });
    }
    let spool_len = u16::from_le_bytes([payload[0], payload[1]]) as usize;
    let spool_end = 2 + spool_len;
    let state_end = spool_end + 32;
    if payload.len() != state_end {
        return Err(TreeStreamError::Malformed(
            "malformed tree entry object id".into(),
        ));
    }
    let spool = std::str::from_utf8(&payload[2..spool_end])
        .map_err(|_| TreeStreamError::Malformed("spool id is not UTF-8".into()))?;
    let spool_id = SpoolId::parse(spool)
        .map_err(|err| TreeStreamError::Malformed(format!("invalid spool id: {err}")))?;
    let state =
        StateId::from_bytes(payload[spool_end..state_end].try_into().map_err(|_| {
            TreeStreamError::Malformed("spoollink state id is not 32 bytes".into())
        })?);
    TreeEntry::spoollink(name, spool_id, state).map_err(TreeStreamError::from)
}

#[cfg(all(test, feature = "zstd"))]
mod block_tests {
    use super::*;

    fn fixture(entries: usize) -> Tree {
        Tree::from_entries(
            (0..entries)
                .map(|index| {
                    TreeEntry::file(
                        format!("module_{index:04}.rs"),
                        ContentHash::compute(format!("blob-{index}").as_bytes()),
                        false,
                    )
                    .expect("fixture entry")
                })
                .collect(),
        )
    }

    fn maximum_size_block_fixture() -> Tree {
        let name_prefix = "n".repeat(u16::MAX as usize - 5);
        let spool_id = SpoolId::parse(format!("s/{}", "s".repeat(u16::MAX as usize - 2)))
            .expect("maximum-size spool id");
        assert_eq!(spool_id.as_str().len(), u16::MAX as usize);
        Tree::from_entries(
            (0..TREE_BLOCK_ENTRIES)
                .map(|index| {
                    let name = format!("{name_prefix}{index:05}");
                    assert_eq!(name.len(), u16::MAX as usize);
                    TreeEntry::spoollink(name, spool_id.clone(), StateId::from_bytes([7; 32]))
                        .expect("maximum-size fixture entry")
                })
                .collect(),
        )
    }

    #[test]
    fn blocked_encoder_falls_back_for_the_complete_object() {
        let tree = fixture(1);
        let encoded = tree.encode_canonical_blocked(3, 0).expect("encode");
        assert_eq!(encoded[4], TREE_ENCODING_VERSION);
    }

    #[test]
    fn final_single_entry_block_is_stored_raw() {
        let tree = fixture(TREE_BLOCK_ENTRIES + 1);
        let encoded = tree.encode_canonical_blocked(3, 0).expect("encode");
        assert_eq!(encoded[4], TREE_BLOCK_ENCODING_VERSION);
        let header = decode_header(&encoded).expect("header");
        let preamble = &encoded[TREE_HEADER_LEN..TREE_HEADER_LEN + TREE_BLOCK_PREAMBLE_LEN];
        let block_header =
            decode_block_header(&header, preamble, encoded.len() as u64).expect("block header");
        let second_index = TREE_HEADER_LEN + TREE_BLOCK_PREAMBLE_LEN + TREE_BLOCK_INDEX_LEN;
        let index = decode_block_index(
            &encoded[second_index..second_index + TREE_BLOCK_INDEX_LEN],
            1,
            &block_header,
            encoded.len() as u64,
        )
        .expect("second block");
        assert_eq!(index.stored_len, index.raw_len);
    }

    #[test]
    fn trailing_bytes_in_a_raw_block_are_rejected() {
        let tree = fixture(TREE_BLOCK_ENTRIES + 1);
        let mut encoded = tree.encode_canonical_blocked(3, 0).expect("encode");
        let second_index = TREE_HEADER_LEN + TREE_BLOCK_PREAMBLE_LEN + TREE_BLOCK_INDEX_LEN;
        let stored_len_offset = second_index + 16;
        let stored_len = u32::from_le_bytes(
            encoded[stored_len_offset..stored_len_offset + 4]
                .try_into()
                .expect("stored length"),
        );
        encoded[stored_len_offset..stored_len_offset + 4]
            .copy_from_slice(&(stored_len + 1).to_le_bytes());
        let payload_len =
            u64::from_le_bytes(encoded[45..53].try_into().expect("stored payload length"));
        encoded[45..53].copy_from_slice(&(payload_len + 1).to_le_bytes());
        encoded.push(0);

        assert!(Tree::decode_canonical(&encoded).is_err());
    }

    #[test]
    fn block_raw_length_above_encoder_ceiling_is_rejected_in_the_index() {
        let tree = fixture(TREE_BLOCK_ENTRIES);
        let encoded = tree.encode_canonical_blocked(3, 0).expect("encode");
        let header = decode_header(&encoded).expect("header");
        let preamble = &encoded[TREE_HEADER_LEN..TREE_HEADER_LEN + TREE_BLOCK_PREAMBLE_LEN];
        let block_header =
            decode_block_header(&header, preamble, encoded.len() as u64).expect("block header");
        let index_offset = TREE_HEADER_LEN + TREE_BLOCK_PREAMBLE_LEN;
        let mut index: [u8; TREE_BLOCK_INDEX_LEN] = encoded
            [index_offset..index_offset + TREE_BLOCK_INDEX_LEN]
            .try_into()
            .expect("index");
        index[20..24].copy_from_slice(&u32::MAX.to_le_bytes());

        let error = decode_block_index(&index, 0, &block_header, encoded.len() as u64)
            .expect_err("attacker-controlled raw length must fail at the index boundary");
        assert!(
            matches!(error, TreeStreamError::Malformed(ref message) if message.contains("raw length") && message.contains("exceeds maximum")),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn block_entry_count_above_encoder_limit_is_rejected() {
        let tree = fixture(TREE_BLOCK_ENTRIES);
        let mut encoded = tree.encode_canonical_blocked(3, 0).expect("encode");
        let block_entries_offset = TREE_HEADER_LEN + 2;
        encoded[block_entries_offset..block_entries_offset + 2]
            .copy_from_slice(&u16::MAX.to_le_bytes());

        let error = Tree::decode_canonical(&encoded)
            .expect_err("block entry count above the encoder limit must fail");
        assert!(
            matches!(error, TreeStreamError::Malformed(ref message) if message.contains("entry count") && message.contains("exceeds maximum")),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn short_final_block_uses_its_actual_entry_count_for_the_ceiling() {
        let tree = fixture(TREE_BLOCK_ENTRIES + 1);
        let encoded = tree.encode_canonical_blocked(3, 0).expect("encode");
        let header = decode_header(&encoded).expect("header");
        let preamble = &encoded[TREE_HEADER_LEN..TREE_HEADER_LEN + TREE_BLOCK_PREAMBLE_LEN];
        let block_header =
            decode_block_header(&header, preamble, encoded.len() as u64).expect("block header");
        let index_offset = TREE_HEADER_LEN + TREE_BLOCK_PREAMBLE_LEN + TREE_BLOCK_INDEX_LEN;
        let mut index: [u8; TREE_BLOCK_INDEX_LEN] = encoded
            [index_offset..index_offset + TREE_BLOCK_INDEX_LEN]
            .try_into()
            .expect("final index");
        let one_entry_max_plus_one =
            u32::try_from(TREE_BLOCK_MAX_ENTRY_FRAME_LEN + 1).expect("one over final block");
        index[20..24].copy_from_slice(&one_entry_max_plus_one.to_le_bytes());

        let error = decode_block_index(&index, 1, &block_header, encoded.len() as u64)
            .expect_err("short final block must use its actual structural ceiling");
        let expected_max = format!("maximum {TREE_BLOCK_MAX_ENTRY_FRAME_LEN}");
        assert!(
            matches!(error, TreeStreamError::Malformed(ref message) if message.contains("raw length") && message.contains(&expected_max)),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn block_raw_length_above_zstd_expansion_bound_is_rejected() {
        let stored_offset = 128u64;
        let stored_len = 1u32;
        let raw_len = u32::try_from(TREE_BLOCK_MAX_EXPANSION_RATIO + 1).expect("raw length");
        let mut index = [0u8; TREE_BLOCK_INDEX_LEN];
        index[8..16].copy_from_slice(&stored_offset.to_le_bytes());
        index[16..20].copy_from_slice(&stored_len.to_le_bytes());
        index[20..24].copy_from_slice(&raw_len.to_le_bytes());
        let block_header = TreeBlockHeader {
            block_entries: TREE_BLOCK_ENTRIES,
            block_count: 1,
            entry_count: 1,
            raw_payload_len: raw_len as u64,
            index_end: stored_offset,
        };

        let error = decode_block_index(&index, 0, &block_header, stored_offset + stored_len as u64)
            .expect_err("excessive zstd expansion must fail at the index boundary");
        assert!(
            matches!(error, TreeStreamError::Malformed(ref message) if message.contains("expansion limit")),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn payload_and_zstd_helpers_recheck_the_structural_ceiling() {
        let over_limit = TREE_BLOCK_MAX_RAW_LEN + 1;
        let payload_error = decode_block_payload(&[0], over_limit)
            .expect_err("payload helper must reject an unchecked raw length");
        assert!(
            matches!(payload_error, TreeStreamError::Malformed(ref message) if message.contains("raw length")),
            "unexpected error: {payload_error}"
        );
        let zstd_error = decompress_block_tail(&[], over_limit)
            .expect_err("zstd helper must cap its output independently");
        assert!(
            matches!(zstd_error, TreeStreamError::Malformed(ref message) if message.contains("tail length")),
            "unexpected error: {zstd_error}"
        );
    }

    #[test]
    fn maximum_size_encoder_block_round_trips_and_one_over_is_rejected() {
        let tree = maximum_size_block_fixture();
        let raw = tree.encode_canonical().expect("encode raw");
        assert_eq!(raw.len() - TREE_HEADER_LEN, TREE_BLOCK_MAX_RAW_LEN);

        let blocked = tree.encode_canonical_blocked(3, 0).expect("encode blocked");
        assert_eq!(blocked[4], TREE_BLOCK_ENCODING_VERSION);
        let header = decode_header(&blocked).expect("header");
        let preamble = &blocked[TREE_HEADER_LEN..TREE_HEADER_LEN + TREE_BLOCK_PREAMBLE_LEN];
        let block_header =
            decode_block_header(&header, preamble, blocked.len() as u64).expect("block header");
        let index_offset = TREE_HEADER_LEN + TREE_BLOCK_PREAMBLE_LEN;
        let index = decode_block_index(
            &blocked[index_offset..index_offset + TREE_BLOCK_INDEX_LEN],
            0,
            &block_header,
            blocked.len() as u64,
        )
        .expect("maximum-size block index");
        assert_eq!(index.raw_len, TREE_BLOCK_MAX_RAW_LEN);

        let decoded = Tree::decode_canonical(&blocked).expect("decode maximum-size block");
        assert_eq!(decoded, tree);
        assert_eq!(decoded.encode_canonical().expect("re-encode decoded"), raw);
        assert_eq!(
            decoded
                .encode_canonical_blocked(3, 0)
                .expect("re-encode blocked"),
            blocked
        );

        let mut over_limit = blocked;
        let raw_len_offset = index_offset + 20;
        let one_over = u32::try_from(TREE_BLOCK_MAX_RAW_LEN + 1).expect("one over");
        over_limit[raw_len_offset..raw_len_offset + 4].copy_from_slice(&one_over.to_le_bytes());
        let error = Tree::decode_canonical(&over_limit)
            .expect_err("one byte above the block ceiling must fail");
        assert!(
            matches!(error, TreeStreamError::Malformed(ref message) if message.contains("raw length")),
            "unexpected error: {error}"
        );
    }
}
