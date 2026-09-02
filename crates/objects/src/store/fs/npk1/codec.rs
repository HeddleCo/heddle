// SPDX-License-Identifier: Apache-2.0

use std::collections::{HashMap, HashSet};

use sley::{ObjectFormat as GitObjectFormat, ObjectId as GitObjectId};

use super::{
    NAME_MAGIC, NAME_RESTART, RECORD_BLOCK_ENTRIES, TARGET_MAGIC, checked_slice, invalid,
    put_varint, read_u16, read_u32, shared_prefix, take_varint,
};
#[cfg(feature = "zstd")]
use crate::store::HeddleError;
use crate::{
    object::{
        ContentHash, EntryType, FileMode, SpoolId, StateId, Tree, TreeDeltaOp, TreeEntry,
        apply_tree_delta,
    },
    store::Result,
};

#[cfg(test)]
thread_local! {
    static DECODED_RECORD_BLOCKS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static DECODED_NAME_ROWS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[derive(Debug)]
pub(super) struct Dictionary {
    pub(super) names: Vec<String>,
    pub(super) name_ids: HashMap<String, u32>,
    pub(super) targets: Vec<[u8; 32]>,
    pub(super) target_ids: HashMap<[u8; 32], u32>,
}

impl Dictionary {
    pub(super) fn from_counts(
        mut names: Vec<String>,
        target_counts: HashMap<[u8; 32], u32>,
    ) -> Result<Self> {
        names.sort();
        names.dedup();
        if names.len() > u32::MAX as usize {
            return Err(invalid("name dictionary exceeds u32 ordinals"));
        }
        let name_ids = names
            .iter()
            .enumerate()
            .map(|(index, name)| (name.clone(), index as u32))
            .collect();

        let mut repeated_targets = target_counts
            .into_iter()
            .filter(|(_, count)| *count >= 2)
            .collect::<Vec<_>>();
        repeated_targets.sort_by(|(left_hash, left_count), (right_hash, right_count)| {
            right_count
                .cmp(left_count)
                .then_with(|| left_hash.cmp(right_hash))
        });
        if repeated_targets.len() > u32::MAX as usize {
            return Err(invalid("target dictionary exceeds u32 ordinals"));
        }
        let targets = repeated_targets
            .into_iter()
            .map(|(target, _)| target)
            .collect::<Vec<_>>();
        let target_ids = targets
            .iter()
            .enumerate()
            .map(|(index, target)| (*target, index as u32))
            .collect();
        Ok(Self {
            names,
            name_ids,
            targets,
            target_ids,
        })
    }

    pub(super) fn name_id(&self, name: &str) -> Result<u32> {
        self.name_ids
            .get(name)
            .copied()
            .ok_or_else(|| invalid(format!("name is absent from dictionary: {name:?}")))
    }

    pub(super) fn target_id(&self, target: [u8; 32]) -> Option<u32> {
        self.target_ids.get(&target).copied()
    }

    pub(super) fn encode_names(&self) -> Result<Vec<u8>> {
        let block_count = self.names.len().div_ceil(NAME_RESTART);
        let count = u32::try_from(self.names.len())
            .map_err(|_| invalid("name dictionary count overflow"))?;
        let blocks = u32::try_from(block_count)
            .map_err(|_| invalid("name dictionary restart count overflow"))?;
        let mut payload = Vec::new();
        let mut offsets = Vec::with_capacity(block_count);
        for block in self.names.chunks(NAME_RESTART) {
            offsets.push(
                u32::try_from(payload.len())
                    .map_err(|_| invalid("name dictionary exceeds 4 GiB"))?,
            );
            let mut previous = "";
            for name in block {
                let prefix = shared_prefix(previous, name);
                put_varint(prefix, &mut payload);
                put_varint(name.len() - prefix, &mut payload);
                payload.extend_from_slice(&name.as_bytes()[prefix..]);
                previous = name;
            }
        }
        let mut out = Vec::with_capacity(16 + offsets.len() * 4 + payload.len());
        out.extend_from_slice(NAME_MAGIC);
        out.extend_from_slice(&count.to_le_bytes());
        out.extend_from_slice(&(NAME_RESTART as u16).to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&blocks.to_le_bytes());
        for offset in offsets {
            out.extend_from_slice(&offset.to_le_bytes());
        }
        out.extend_from_slice(&payload);
        Ok(out)
    }

    pub(super) fn encode_targets(&self) -> Result<Vec<u8>> {
        let count = u32::try_from(self.targets.len())
            .map_err(|_| invalid("target dictionary count overflow"))?;
        let mut out = Vec::with_capacity(8 + self.targets.len() * 32);
        out.extend_from_slice(TARGET_MAGIC);
        out.extend_from_slice(&count.to_le_bytes());
        for target in &self.targets {
            out.extend_from_slice(target);
        }
        Ok(out)
    }
}

#[derive(Clone, Copy, Debug)]
pub(super) struct NameDictionary {
    count: usize,
    block_count: usize,
    payload_start: usize,
    len: usize,
}

impl NameDictionary {
    pub(super) fn decode(bytes: &[u8]) -> Result<Self> {
        if checked_slice(bytes, 0, 4, "name dictionary magic")? != NAME_MAGIC {
            return Err(invalid("invalid name dictionary magic"));
        }
        let count = read_u32(bytes, 4, "name count")? as usize;
        if count > bytes.len() / 2 {
            return Err(invalid("name count exceeds dictionary bytes"));
        }
        let restart = read_u16(bytes, 8, "name restart interval")? as usize;
        if restart != NAME_RESTART {
            return Err(invalid("unsupported name restart interval"));
        }
        if read_u16(bytes, 10, "name dictionary reserved field")? != 0 {
            return Err(invalid("non-zero name dictionary reserved field"));
        }
        let block_count = read_u32(bytes, 12, "name restart count")? as usize;
        if block_count != count.div_ceil(NAME_RESTART) {
            return Err(invalid("name restart count mismatch"));
        }
        let offsets_len = block_count
            .checked_mul(4)
            .ok_or_else(|| invalid("name restart table overflow"))?;
        let payload_start = 16usize
            .checked_add(offsets_len)
            .ok_or_else(|| invalid("name dictionary header overflow"))?;
        if payload_start > bytes.len() || (count == 0) != (payload_start == bytes.len()) {
            return Err(invalid("truncated name dictionary payload"));
        }
        Ok(Self {
            count,
            block_count,
            payload_start,
            len: bytes.len(),
        })
    }

    pub(super) fn lookup(
        &self,
        bytes: &[u8],
        wanted: &str,
        mut verify: impl FnMut(usize, usize) -> Result<()>,
    ) -> Result<Option<u32>> {
        self.check_bytes(bytes)?;
        let mut low = 0usize;
        let mut high = self.block_count;
        while low < high {
            let middle = low + (high - low) / 2;
            let (start, end) = self.block_bounds(bytes, middle, &mut verify)?;
            verify(start, end - start)?;
            let first = first_name(checked_slice(
                bytes,
                start,
                end - start,
                "name restart block",
            )?)?;
            if first <= wanted {
                low = middle + 1;
            } else {
                high = middle;
            }
        }
        let Some(block_index) = low.checked_sub(1) else {
            return Ok(None);
        };
        let (start, end) = self.block_bounds(bytes, block_index, &mut verify)?;
        verify(start, end - start)?;
        let block = checked_slice(bytes, start, end - start, "name restart block")?;
        let rows = NAME_RESTART.min(self.count - block_index * NAME_RESTART);
        let mut offset = 0usize;
        let mut previous = String::new();
        for row in 0..rows {
            decode_name_row(block, &mut offset, row, &mut previous)?;
            match previous.as_str().cmp(wanted) {
                std::cmp::Ordering::Less => {}
                std::cmp::Ordering::Equal => {
                    let ordinal = block_index
                        .checked_mul(NAME_RESTART)
                        .and_then(|value| value.checked_add(row))
                        .ok_or_else(|| invalid("name dictionary ordinal overflow"))?;
                    return u32::try_from(ordinal)
                        .map(Some)
                        .map_err(|_| invalid("name dictionary ordinal overflow"));
                }
                std::cmp::Ordering::Greater => return Ok(None),
            }
        }
        if offset != block.len() {
            return Err(invalid("name restart block has trailing bytes"));
        }
        Ok(None)
    }

    pub(super) fn decode_block(
        &self,
        bytes: &[u8],
        ordinal: u32,
        mut verify: impl FnMut(usize, usize) -> Result<()>,
    ) -> Result<(usize, Vec<String>)> {
        self.check_bytes(bytes)?;
        let ordinal = ordinal as usize;
        if ordinal >= self.count {
            return Err(invalid("row name ordinal out of bounds"));
        }
        let block_index = ordinal / NAME_RESTART;
        let (start, end) = self.block_bounds(bytes, block_index, &mut verify)?;
        verify(start, end - start)?;
        let block = checked_slice(bytes, start, end - start, "name restart block")?;
        let rows = NAME_RESTART.min(self.count - block_index * NAME_RESTART);
        let mut names = Vec::with_capacity(rows);
        let mut offset = 0usize;
        let mut previous = String::new();
        for row in 0..rows {
            decode_name_row(block, &mut offset, row, &mut previous)?;
            names.push(previous.clone());
        }
        if offset != block.len() {
            return Err(invalid("name restart block has trailing bytes"));
        }
        Ok((block_index, names))
    }

    pub(super) fn validate(
        &self,
        bytes: &[u8],
        mut verify: impl FnMut(usize, usize) -> Result<()>,
    ) -> Result<()> {
        self.check_bytes(bytes)?;
        let mut previous_global = String::new();
        let mut decoded = 0usize;
        for block_index in 0..self.block_count {
            let (start, end) = self.block_bounds(bytes, block_index, &mut verify)?;
            verify(start, end - start)?;
            let block = checked_slice(bytes, start, end - start, "name restart block")?;
            let rows = NAME_RESTART.min(self.count - decoded);
            let mut offset = 0usize;
            let mut previous = String::new();
            for row in 0..rows {
                decode_name_row(block, &mut offset, row, &mut previous)?;
                if decoded > 0 && previous_global >= previous {
                    return Err(invalid("name dictionary is not strictly sorted"));
                }
                previous_global.clone_from(&previous);
                decoded += 1;
            }
            if offset != block.len() {
                return Err(invalid("name restart block has trailing bytes"));
            }
        }
        if decoded != self.count {
            return Err(invalid("name dictionary count mismatch"));
        }
        Ok(())
    }

    fn check_bytes(&self, bytes: &[u8]) -> Result<()> {
        if bytes.len() != self.len {
            return Err(invalid("name dictionary view length changed"));
        }
        Ok(())
    }

    fn block_bounds(
        &self,
        bytes: &[u8],
        block_index: usize,
        verify: &mut impl FnMut(usize, usize) -> Result<()>,
    ) -> Result<(usize, usize)> {
        if block_index >= self.block_count {
            return Err(invalid("name restart block ordinal out of bounds"));
        }
        let row = 16usize
            .checked_add(
                block_index
                    .checked_mul(4)
                    .ok_or_else(|| invalid("name restart table overflow"))?,
            )
            .ok_or_else(|| invalid("name restart table overflow"))?;
        let table_len = if block_index + 1 < self.block_count {
            8
        } else {
            4
        };
        verify(row, table_len)?;
        let start = self
            .payload_start
            .checked_add(read_u32(bytes, row, "name restart offset")? as usize)
            .ok_or_else(|| invalid("name restart offset overflow"))?;
        let end = if block_index + 1 < self.block_count {
            self.payload_start
                .checked_add(read_u32(bytes, row + 4, "name restart offset")? as usize)
                .ok_or_else(|| invalid("name restart offset overflow"))?
        } else {
            self.len
        };
        if (block_index == 0 && start != self.payload_start) || start >= end || end > self.len {
            return Err(invalid("invalid name restart offsets"));
        }
        Ok((start, end))
    }
}

fn first_name(block: &[u8]) -> Result<&str> {
    let mut offset = 0usize;
    if take_varint(block, &mut offset)? != 0 {
        return Err(invalid("name restart row has a prefix"));
    }
    let suffix_len = take_varint(block, &mut offset)?;
    let suffix = checked_slice(block, offset, suffix_len, "name suffix")?;
    std::str::from_utf8(suffix)
        .map_err(|error| invalid(format!("name suffix is not UTF-8: {error}")))
}

fn decode_name_row(
    block: &[u8],
    offset: &mut usize,
    row: usize,
    previous: &mut String,
) -> Result<()> {
    #[cfg(test)]
    DECODED_NAME_ROWS.with(|count| count.set(count.get().saturating_add(1)));
    let prefix = take_varint(block, offset)?;
    let suffix_len = take_varint(block, offset)?;
    if row == 0 && prefix != 0 {
        return Err(invalid("name restart row has a prefix"));
    }
    if prefix > previous.len() || !previous.is_char_boundary(prefix) {
        return Err(invalid("name prefix is not a valid string boundary"));
    }
    let suffix = checked_slice(block, *offset, suffix_len, "name suffix")?;
    *offset += suffix_len;
    let suffix = std::str::from_utf8(suffix)
        .map_err(|error| invalid(format!("name suffix is not UTF-8: {error}")))?;
    previous.truncate(prefix);
    previous.push_str(suffix);
    Ok(())
}

#[derive(Clone, Copy, Debug)]
pub(super) struct TargetDictionary {
    count: usize,
    len: usize,
}

impl TargetDictionary {
    pub(super) fn decode(bytes: &[u8]) -> Result<Self> {
        if checked_slice(bytes, 0, 4, "target dictionary magic")? != TARGET_MAGIC {
            return Err(invalid("invalid target dictionary magic"));
        }
        let count = read_u32(bytes, 4, "target count")? as usize;
        let expected = 8usize
            .checked_add(
                count
                    .checked_mul(32)
                    .ok_or_else(|| invalid("target dictionary size overflow"))?,
            )
            .ok_or_else(|| invalid("target dictionary size overflow"))?;
        if bytes.len() != expected {
            return Err(invalid("target dictionary length mismatch"));
        }
        Ok(Self {
            count,
            len: bytes.len(),
        })
    }

    pub(super) fn target(
        &self,
        bytes: &[u8],
        ordinal: usize,
        mut verify: impl FnMut(usize, usize) -> Result<()>,
    ) -> Result<[u8; 32]> {
        if bytes.len() != self.len || ordinal >= self.count {
            return Err(invalid("target dictionary ordinal out of bounds"));
        }
        let offset = 8usize
            .checked_add(
                ordinal
                    .checked_mul(32)
                    .ok_or_else(|| invalid("target dictionary offset overflow"))?,
            )
            .ok_or_else(|| invalid("target dictionary offset overflow"))?;
        verify(offset, 32)?;
        checked_slice(bytes, offset, 32, "target dictionary row")?
            .try_into()
            .map_err(|_| invalid("invalid target dictionary row"))
    }

    pub(super) fn count(&self) -> usize {
        self.count
    }
}

pub(super) trait DecodeDictionary {
    fn name(&self, ordinal: u32) -> Result<String>;
    fn target(&self, ordinal: usize) -> Result<[u8; 32]>;
    fn target_count(&self) -> usize;
}

fn content_target(entry: &TreeEntry) -> Option<[u8; 32]> {
    match entry.entry_type() {
        EntryType::Blob | EntryType::Tree | EntryType::Symlink => {
            entry.content_hash().map(|hash| *hash.as_bytes())
        }
        EntryType::Gitlink | EntryType::Spoollink => None,
    }
}

fn encode_entry(entry: &TreeEntry, dictionary: &Dictionary, out: &mut Vec<u8>) -> Result<()> {
    out.push((entry.mode().to_byte() << 3) | entry.entry_type().to_byte());
    match entry.entry_type() {
        EntryType::Blob | EntryType::Tree | EntryType::Symlink => {
            let target = content_target(entry)
                .ok_or_else(|| invalid("content entry is missing its hash"))?;
            if let Some(target_id) = dictionary.target_id(target) {
                put_varint(target_id as usize + 1, out);
            } else {
                put_varint(0, out);
                out.extend_from_slice(&target);
            }
        }
        EntryType::Gitlink => {
            let target = entry
                .gitlink_target()
                .ok_or_else(|| invalid("gitlink is missing its target"))?;
            out.push(match target.format() {
                GitObjectFormat::Sha1 => 1,
                GitObjectFormat::Sha256 => 2,
            });
            out.extend_from_slice(target.as_bytes());
        }
        EntryType::Spoollink => {
            let (spool, state) = entry
                .spoollink_target()
                .ok_or_else(|| invalid("spoollink is missing its target"))?;
            put_varint(spool.as_str().len(), out);
            out.extend_from_slice(spool.as_str().as_bytes());
            out.extend_from_slice(state.as_bytes());
        }
    }
    Ok(())
}

fn decode_entry(
    bytes: &[u8],
    offset: &mut usize,
    name: &str,
    dictionary: &impl DecodeDictionary,
) -> Result<TreeEntry> {
    let tag = *bytes
        .get(*offset)
        .ok_or_else(|| invalid("truncated entry tag"))?;
    *offset += 1;
    let mode = FileMode::from_byte(tag >> 3).ok_or_else(|| invalid("invalid entry mode"))?;
    let kind = EntryType::from_byte(tag & 0x07).ok_or_else(|| invalid("invalid entry kind"))?;
    let entry = match kind {
        EntryType::Blob | EntryType::Tree | EntryType::Symlink => {
            let encoded_target = take_varint(bytes, offset)?;
            let target = if encoded_target == 0 {
                let raw: [u8; 32] = checked_slice(bytes, *offset, 32, "inline target")?
                    .try_into()
                    .map_err(|_| invalid("invalid inline target"))?;
                *offset += 32;
                ContentHash::from_bytes(raw)
            } else {
                ContentHash::from_bytes(dictionary.target(encoded_target - 1)?)
            };
            match kind {
                EntryType::Blob => TreeEntry::file(name, target, mode == FileMode::Executable)?,
                EntryType::Tree => TreeEntry::directory(name, target)?,
                EntryType::Symlink => TreeEntry::symlink(name, target)?,
                EntryType::Gitlink | EntryType::Spoollink => {
                    return Err(invalid("content entry kind changed while decoding"));
                }
            }
        }
        EntryType::Gitlink => {
            let format = match *bytes
                .get(*offset)
                .ok_or_else(|| invalid("truncated gitlink format"))?
            {
                1 => GitObjectFormat::Sha1,
                2 => GitObjectFormat::Sha256,
                _ => return Err(invalid("invalid gitlink format")),
            };
            *offset += 1;
            let target_len = match format {
                GitObjectFormat::Sha1 => 20,
                GitObjectFormat::Sha256 => 32,
            };
            let target = GitObjectId::from_raw(
                format,
                checked_slice(bytes, *offset, target_len, "gitlink target")?,
            )
            .map_err(|error| invalid(format!("invalid gitlink target: {error}")))?;
            *offset += target_len;
            TreeEntry::gitlink(name, target)?
        }
        EntryType::Spoollink => {
            let spool_len = take_varint(bytes, offset)?;
            let spool = std::str::from_utf8(checked_slice(bytes, *offset, spool_len, "spool id")?)
                .map_err(|error| invalid(format!("spool id is not UTF-8: {error}")))?;
            *offset += spool_len;
            let state: [u8; 32] = checked_slice(bytes, *offset, 32, "spool state")?
                .try_into()
                .map_err(|_| invalid("invalid spool state"))?;
            *offset += 32;
            let spool = SpoolId::parse(spool)
                .map_err(|error| invalid(format!("invalid spool id: {error}")))?;
            TreeEntry::spoollink(name, spool, StateId::from_bytes(state))?
        }
    };
    if entry.mode() != mode {
        return Err(invalid("entry mode and kind disagree"));
    }
    Ok(entry)
}

fn skip_entry(bytes: &[u8], offset: &mut usize, dictionary: &impl DecodeDictionary) -> Result<()> {
    let tag = *bytes
        .get(*offset)
        .ok_or_else(|| invalid("truncated entry tag"))?;
    *offset += 1;
    let _mode = FileMode::from_byte(tag >> 3).ok_or_else(|| invalid("invalid entry mode"))?;
    let kind = EntryType::from_byte(tag & 0x07).ok_or_else(|| invalid("invalid entry kind"))?;
    match kind {
        EntryType::Blob | EntryType::Tree | EntryType::Symlink => {
            let encoded_target = take_varint(bytes, offset)?;
            if encoded_target == 0 {
                let _ = checked_slice(bytes, *offset, 32, "inline target")?;
                *offset += 32;
            } else if encoded_target > dictionary.target_count() {
                return Err(invalid("target dictionary ordinal out of bounds"));
            }
        }
        EntryType::Gitlink => {
            let target_len = match *bytes
                .get(*offset)
                .ok_or_else(|| invalid("truncated gitlink format"))?
            {
                1 => 20,
                2 => 32,
                _ => return Err(invalid("invalid gitlink format")),
            };
            *offset += 1;
            let _ = checked_slice(bytes, *offset, target_len, "gitlink target")?;
            *offset += target_len;
        }
        EntryType::Spoollink => {
            let spool_len = take_varint(bytes, offset)?;
            let spool = checked_slice(bytes, *offset, spool_len, "spool id")?;
            let _ = std::str::from_utf8(spool)
                .map_err(|error| invalid(format!("spool id is not UTF-8: {error}")))?;
            *offset += spool_len;
            let _ = checked_slice(bytes, *offset, 32, "spool state")?;
            *offset += 32;
        }
    }
    Ok(())
}

#[derive(Clone, Debug)]
pub(super) struct BlockDescriptor {
    pub(super) first_name: u32,
    pub(super) raw_len: usize,
    pub(super) stored_len: usize,
    pub(super) payload_offset: usize,
}

pub(super) struct RecordHeader {
    pub(super) tag: u8,
    pub(super) prefix: Vec<usize>,
    pub(super) blocks: Vec<BlockDescriptor>,
}

pub(super) struct RawRecord {
    tag: u8,
    prefix: Vec<usize>,
    blocks: Vec<(u32, Vec<u8>)>,
}

impl RawRecord {
    pub(super) fn encode(&self, encoder: &mut RecordEncoder) -> Result<Vec<u8>> {
        let mut stored_blocks = Vec::with_capacity(self.blocks.len());
        for (first_name, raw) in &self.blocks {
            let encoded = encoder.compress_block(raw)?;
            stored_blocks.push((*first_name, raw.len(), encoded));
        }
        let mut out = Vec::new();
        out.push(self.tag);
        for value in &self.prefix {
            put_varint(*value, &mut out);
        }
        put_varint(stored_blocks.len(), &mut out);
        for (first_name, raw_len, stored) in &stored_blocks {
            put_varint(*first_name as usize, &mut out);
            put_varint(*raw_len, &mut out);
            put_varint(stored.len(), &mut out);
        }
        for (_, _, stored) in stored_blocks {
            out.extend_from_slice(&stored);
        }
        Ok(out)
    }

    #[cfg(feature = "zstd")]
    pub(super) fn samples(&self) -> impl Iterator<Item = &[u8]> {
        self.blocks.iter().map(|(_, raw)| raw.as_slice())
    }
}

pub(super) struct RecordEncoder {
    #[cfg(feature = "zstd")]
    compressor: zstd::bulk::Compressor<'static>,
}

impl RecordEncoder {
    pub(super) fn new(dictionary: &[u8]) -> Result<Self> {
        #[cfg(feature = "zstd")]
        {
            let compressor =
                zstd::bulk::Compressor::with_dictionary(super::RECORD_LEVEL, dictionary)
                    .map_err(|error| HeddleError::Compression(error.to_string()))?;
            Ok(Self { compressor })
        }
        #[cfg(not(feature = "zstd"))]
        {
            let _ = dictionary;
            Ok(Self {})
        }
    }

    fn compress_block(&mut self, raw: &[u8]) -> Result<Vec<u8>> {
        #[cfg(feature = "zstd")]
        {
            let candidate = self
                .compressor
                .compress(raw)
                .map_err(|error| HeddleError::Compression(error.to_string()))?;
            if candidate.len() < raw.len() {
                return Ok(candidate);
            }
        }
        Ok(raw.to_vec())
    }
}

pub(super) struct RecordDecoder {
    #[cfg(feature = "zstd")]
    dictionary: Option<zstd::dict::DecoderDictionary<'static>>,
}

impl RecordDecoder {
    pub(super) fn new(dictionary: &[u8]) -> Self {
        #[cfg(feature = "zstd")]
        {
            Self {
                dictionary: (!dictionary.is_empty())
                    .then(|| zstd::dict::DecoderDictionary::copy(dictionary)),
            }
        }
        #[cfg(not(feature = "zstd"))]
        {
            let _ = dictionary;
            Self {}
        }
    }

    fn decompress(&self, stored: &[u8], raw_len: usize) -> Result<Vec<u8>> {
        #[cfg(feature = "zstd")]
        {
            let decoded = match &self.dictionary {
                Some(dictionary) => {
                    let mut decompressor =
                        zstd::bulk::Decompressor::with_prepared_dictionary(dictionary)
                            .map_err(|error| HeddleError::Compression(error.to_string()))?;
                    decompressor.decompress(stored, raw_len)
                }
                None => zstd::bulk::decompress(stored, raw_len),
            }
            .map_err(|error| HeddleError::Compression(error.to_string()))?;
            if decoded.len() != raw_len {
                return Err(invalid("decoded block length mismatch"));
            }
            Ok(decoded)
        }
        #[cfg(not(feature = "zstd"))]
        {
            let _ = (stored, raw_len);
            Err(invalid(
                "compressed record requires a build with zstd support",
            ))
        }
    }
}

pub(super) fn parse_record_header(bytes: &[u8]) -> Result<RecordHeader> {
    let tag = *bytes.first().ok_or_else(|| invalid("empty record"))?;
    if tag != b'A' && tag != b'D' {
        return Err(invalid("unknown record tag"));
    }
    let mut offset = 1usize;
    let prefix_fields = if tag == b'A' { 1 } else { 3 };
    let mut prefix = Vec::with_capacity(prefix_fields);
    for _ in 0..prefix_fields {
        prefix.push(take_varint(bytes, &mut offset)?);
    }
    let block_count = take_varint(bytes, &mut offset)?;
    if block_count > bytes.len() / 3 {
        return Err(invalid("record block count exceeds record bytes"));
    }
    let mut blocks = Vec::with_capacity(block_count);
    let mut previous_name = None;
    for _ in 0..block_count {
        let first_name = u32::try_from(take_varint(bytes, &mut offset)?)
            .map_err(|_| invalid("block name ordinal overflow"))?;
        if previous_name.is_some_and(|previous| previous >= first_name) {
            return Err(invalid("record block names are not strictly sorted"));
        }
        previous_name = Some(first_name);
        let raw_len = take_varint(bytes, &mut offset)?;
        let stored_len = take_varint(bytes, &mut offset)?;
        if stored_len > raw_len {
            return Err(invalid("record block expands on disk"));
        }
        blocks.push(BlockDescriptor {
            first_name,
            raw_len,
            stored_len,
            payload_offset: 0,
        });
    }
    for block in &mut blocks {
        block.payload_offset = offset;
        offset = offset
            .checked_add(block.stored_len)
            .ok_or_else(|| invalid("record length overflow"))?;
    }
    if offset != bytes.len() {
        return Err(invalid("record length mismatch"));
    }
    Ok(RecordHeader {
        tag,
        prefix,
        blocks,
    })
}

pub(super) fn decode_block(
    bytes: &[u8],
    block: &BlockDescriptor,
    decoder: &RecordDecoder,
) -> Result<Vec<u8>> {
    #[cfg(test)]
    DECODED_RECORD_BLOCKS.with(|count| count.set(count.get().saturating_add(1)));
    let stored = checked_slice(
        bytes,
        block.payload_offset,
        block.stored_len,
        "record block",
    )?;
    if block.stored_len == block.raw_len {
        return Ok(stored.to_vec());
    }
    decoder.decompress(stored, block.raw_len)
}

pub(super) fn encode_anchor(tree: &Tree, dictionary: &Dictionary) -> Result<RawRecord> {
    let mut blocks = Vec::new();
    for entries in tree.entries().chunks(RECORD_BLOCK_ENTRIES) {
        let first_name = entries
            .first()
            .map(|entry| dictionary.name_id(entry.name()))
            .transpose()?
            .unwrap_or_default();
        let mut previous = 0u32;
        let mut raw = Vec::new();
        for (ordinal, entry) in entries.iter().enumerate() {
            let name = dictionary.name_id(entry.name())?;
            let encoded_name = if ordinal == 0 {
                name
            } else {
                name.checked_sub(previous)
                    .ok_or_else(|| invalid("anchor names are not sorted"))?
            };
            put_varint(encoded_name as usize, &mut raw);
            encode_entry(entry, dictionary, &mut raw)?;
            previous = name;
        }
        blocks.push((first_name, raw));
    }
    Ok(RawRecord {
        tag: b'A',
        prefix: vec![tree.len()],
        blocks,
    })
}

pub(super) fn encode_delta(
    base_distance: usize,
    current: &Tree,
    ops: &[TreeDeltaOp],
    dictionary: &Dictionary,
) -> Result<RawRecord> {
    if base_distance == 0 {
        return Err(invalid("delta base distance is zero"));
    }
    let mut blocks = Vec::new();
    for operations in ops.chunks(RECORD_BLOCK_ENTRIES) {
        let first_name = operations
            .first()
            .map(|op| dictionary.name_id(op.name()))
            .transpose()?
            .unwrap_or_default();
        let mut previous = 0u32;
        let mut raw = Vec::new();
        for (ordinal, op) in operations.iter().enumerate() {
            let name = dictionary.name_id(op.name())?;
            let encoded_name = if ordinal == 0 {
                name
            } else {
                name.checked_sub(previous)
                    .ok_or_else(|| invalid("delta names are not sorted"))?
            };
            put_varint(encoded_name as usize, &mut raw);
            match op {
                TreeDeltaOp::Remove(_) => raw.push(0),
                TreeDeltaOp::Upsert(entry) => {
                    raw.push(1);
                    encode_entry(entry, dictionary, &mut raw)?;
                }
            }
            previous = name;
        }
        blocks.push((first_name, raw));
    }
    Ok(RawRecord {
        tag: b'D',
        prefix: vec![base_distance, current.len(), ops.len()],
        blocks,
    })
}

pub(super) fn record_base_distance(bytes: &[u8]) -> Result<Option<usize>> {
    match bytes.first() {
        Some(b'A') => Ok(None),
        Some(b'D') => {
            let mut offset = 1usize;
            Ok(Some(take_varint(bytes, &mut offset)?))
        }
        _ => Err(invalid("invalid record tag")),
    }
}

fn decode_rows(
    bytes: &[u8],
    header: &RecordHeader,
    dictionary: &impl DecodeDictionary,
    decoder: &RecordDecoder,
) -> Result<Vec<TreeDeltaOp>> {
    let mut ops = Vec::new();
    let mut previous_global = None;
    for block in &header.blocks {
        let raw = decode_block(bytes, block, decoder)?;
        let mut offset = 0usize;
        let mut name_id = 0u32;
        let mut ordinal = 0usize;
        while offset < raw.len() {
            let encoded_name = u32::try_from(take_varint(&raw, &mut offset)?)
                .map_err(|_| invalid("row name ordinal overflow"))?;
            name_id = if ordinal == 0 {
                encoded_name
            } else {
                name_id
                    .checked_add(encoded_name)
                    .ok_or_else(|| invalid("row name ordinal overflow"))?
            };
            if previous_global.is_some_and(|previous| previous >= name_id) {
                return Err(invalid("record rows are not strictly sorted"));
            }
            previous_global = Some(name_id);
            let name = dictionary.name(name_id)?;
            if header.tag == b'D' {
                let opcode = *raw
                    .get(offset)
                    .ok_or_else(|| invalid("truncated delta opcode"))?;
                offset += 1;
                ops.push(match opcode {
                    0 => TreeDeltaOp::Remove(name),
                    1 => TreeDeltaOp::Upsert(decode_entry(&raw, &mut offset, &name, dictionary)?),
                    _ => return Err(invalid("invalid delta opcode")),
                });
            } else {
                ops.push(TreeDeltaOp::Upsert(decode_entry(
                    &raw,
                    &mut offset,
                    &name,
                    dictionary,
                )?));
            }
            ordinal += 1;
        }
    }
    Ok(ops)
}

pub(super) fn decode_anchor(
    bytes: &[u8],
    dictionary: &impl DecodeDictionary,
    decoder: &RecordDecoder,
) -> Result<Tree> {
    let header = parse_record_header(bytes)?;
    if header.tag != b'A' {
        return Err(invalid("anchor record expected"));
    }
    let expected_entries = header.prefix[0];
    let ops = decode_rows(bytes, &header, dictionary, decoder)?;
    if ops.len() != expected_entries {
        return Err(invalid("anchor entry count mismatch"));
    }
    let entries = ops
        .into_iter()
        .map(|op| match op {
            TreeDeltaOp::Upsert(entry) => Ok(entry),
            TreeDeltaOp::Remove(_) => Err(invalid("anchor contains a remove operation")),
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(Tree::try_from_decoded_entries(entries)?)
}

pub(super) fn decode_delta(
    bytes: &[u8],
    base: &Tree,
    dictionary: &impl DecodeDictionary,
    decoder: &RecordDecoder,
) -> Result<Tree> {
    let header = parse_record_header(bytes)?;
    if header.tag != b'D' {
        return Err(invalid("delta record expected"));
    }
    let result_entries = header.prefix[1];
    let expected_ops = header.prefix[2];
    let ops = decode_rows(bytes, &header, dictionary, decoder)?;
    if ops.len() != expected_ops {
        return Err(invalid("delta operation count mismatch"));
    }
    let tree = apply_tree_delta(base, &ops)?;
    if tree.len() != result_entries {
        return Err(invalid("delta result count mismatch"));
    }
    Ok(tree)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum LookupState {
    Missing,
    Removed,
    Found,
}

pub(super) fn lookup_record(
    bytes: &[u8],
    dictionary: &impl DecodeDictionary,
    wanted_name: u32,
    wanted: &str,
    decoder: &RecordDecoder,
) -> Result<(LookupState, Option<TreeEntry>)> {
    let header = parse_record_header(bytes)?;
    let Some(block_index) = header
        .blocks
        .partition_point(|block| block.first_name <= wanted_name)
        .checked_sub(1)
    else {
        return Ok((LookupState::Missing, None));
    };
    let raw = decode_block(bytes, &header.blocks[block_index], decoder)?;
    let mut offset = 0usize;
    let mut name_id = 0u32;
    let mut ordinal = 0usize;
    while offset < raw.len() {
        let encoded_name = u32::try_from(take_varint(&raw, &mut offset)?)
            .map_err(|_| invalid("lookup name ordinal overflow"))?;
        name_id = if ordinal == 0 {
            encoded_name
        } else {
            name_id
                .checked_add(encoded_name)
                .ok_or_else(|| invalid("lookup name ordinal overflow"))?
        };
        if name_id > wanted_name {
            break;
        }
        if header.tag == b'D' {
            let opcode = *raw
                .get(offset)
                .ok_or_else(|| invalid("truncated lookup opcode"))?;
            offset += 1;
            match opcode {
                0 => {
                    if name_id == wanted_name {
                        return Ok((LookupState::Removed, None));
                    }
                }
                1 => {
                    if name_id == wanted_name {
                        let entry = decode_entry(&raw, &mut offset, wanted, dictionary)?;
                        return Ok((LookupState::Found, Some(entry)));
                    }
                    skip_entry(&raw, &mut offset, dictionary)?;
                }
                _ => return Err(invalid("invalid lookup opcode")),
            }
        } else {
            if name_id == wanted_name {
                let entry = decode_entry(&raw, &mut offset, wanted, dictionary)?;
                return Ok((LookupState::Found, Some(entry)));
            }
            skip_entry(&raw, &mut offset, dictionary)?;
        }
        ordinal += 1;
    }
    Ok((LookupState::Missing, None))
}

#[cfg(test)]
pub(super) fn reset_decoded_record_blocks() {
    DECODED_RECORD_BLOCKS.with(|count| count.set(0));
}

#[cfg(test)]
pub(super) fn decoded_record_blocks() -> usize {
    DECODED_RECORD_BLOCKS.with(std::cell::Cell::get)
}

#[cfg(test)]
pub(super) fn reset_decoded_name_rows() {
    DECODED_NAME_ROWS.with(|count| count.set(0));
}

#[cfg(test)]
pub(super) fn decoded_name_rows() -> usize {
    DECODED_NAME_ROWS.with(std::cell::Cell::get)
}

pub(super) fn note_tree_dictionary_rows(
    tree: &Tree,
    names: &mut HashSet<String>,
    targets: &mut HashMap<[u8; 32], u32>,
) {
    for entry in tree.entries() {
        names.insert(entry.name().to_string());
        if let Some(target) = content_target(entry) {
            let count = targets.entry(target).or_default();
            *count = count.saturating_add(1);
        }
    }
}
