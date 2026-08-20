// SPDX-License-Identifier: Apache-2.0
//! Streamable canonical Tree encoding (HTR4).
//!
//! Each entry is a length-prefixed frame, so a reader can yield one entry or
//! a caller-sized page and resume at a byte offset without decoding the prefix.
//! Whole-object compression is not part of this encoding: a resume cursor must
//! be able to seek without decompressing earlier frames.

use sley::{ObjectFormat as GitObjectFormat, ObjectId as GitObjectId};

use super::tree::{git_format_from_tag, git_format_to_tag};
use super::tree_stream::TreeStreamError;
use super::{
    ContentHash, EntryType, FileMode, SpoolId, StateId, Tree, TreeEntry, TreeError,
};

/// Durable encoding version stored in every HTR4 header and resume cursor.
pub const TREE_ENCODING_VERSION: u8 = 4;
/// Frame discriminator for a single canonical tree.
pub const TREE_CANONICAL_MAGIC: &[u8; 4] = b"HTR4";
/// Fixed header size: magic + version + tree id + counts.
pub const TREE_HEADER_LEN: usize = 4 + 1 + 32 + 8 + 8 + 8;

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
                TreeStreamError::Malformed(format!(
                    "entry '{}' frame exceeds u32",
                    entry.name()
                ))
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
        let expected_len = TREE_HEADER_LEN as u64 + header.payload_len;
        if data.len() as u64 != expected_len {
            return Err(TreeStreamError::TrailingBytes {
                extra: (data.len() as u64).saturating_sub(expected_len),
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
    if version != TREE_ENCODING_VERSION {
        return Err(TreeStreamError::UnsupportedVersion { found: version });
    }
    let tree_id = ContentHash::from_bytes(data[5..37].try_into().map_err(|_| {
        TreeStreamError::Malformed("tree id slice is not 32 bytes".into())
    })?);
    let entry_count = u64::from_le_bytes(data[37..45].try_into().map_err(|_| {
        TreeStreamError::Malformed("entry count slice is not 8 bytes".into())
    })?);
    let payload_len = u64::from_le_bytes(data[45..53].try_into().map_err(|_| {
        TreeStreamError::Malformed("payload length slice is not 8 bytes".into())
    })?);
    let logical_len = u64::from_le_bytes(data[53..61].try_into().map_err(|_| {
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
    let frame_len = u32::from_le_bytes(data[offset..offset + 4].try_into().map_err(|_| {
        TreeStreamError::Malformed("frame length slice is not 4 bytes".into())
    })?) as usize;
    let frame_start = offset + 4;
    let frame_end = frame_start
        .checked_add(frame_len)
        .ok_or_else(|| TreeStreamError::TruncatedFrame {
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
            let spool_len = u16::try_from(spool_bytes.len()).map_err(|_| {
                TreeStreamError::Malformed("spool id exceeds u16".into())
            })?;
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
    let bytes: [u8; 32] = payload.try_into().map_err(|_| {
        TreeStreamError::Malformed("malformed tree entry object id".into())
    })?;
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
    let state = StateId::from_bytes(payload[spool_end..state_end].try_into().map_err(|_| {
        TreeStreamError::Malformed("spoollink state id is not 32 bytes".into())
    })?);
    TreeEntry::spoollink(name, spool_id, state).map_err(TreeStreamError::from)
}
