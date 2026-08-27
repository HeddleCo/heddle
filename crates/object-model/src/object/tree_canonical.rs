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
/// Fixed header size: magic + version + tree id + counts.
pub const TREE_HEADER_LEN: usize = 4 + 1 + 32 + 8 + 8 + 8;
/// Small real trees do not repay block/index overhead.
pub const TREE_BLOCK_MIN_ENTRIES: usize = 18;

pub(crate) const TREE_BLOCK_ENTRIES: usize = 256;
pub(crate) const TREE_BLOCK_PREAMBLE_LEN: usize = 16;
pub(crate) const TREE_BLOCK_INDEX_LEN: usize = 24;
const TREE_BLOCK_CODEC_ZSTD: u8 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct TreeBlockHeader {
    pub block_entries: usize,
    pub block_count: usize,
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
    if stored_len == 0 || raw_len == 0 || stored_offset < block_header.index_end {
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

pub(crate) fn decode_block_payload(
    stored: &[u8],
    raw_len: usize,
) -> Result<Vec<u8>, TreeStreamError> {
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
}
