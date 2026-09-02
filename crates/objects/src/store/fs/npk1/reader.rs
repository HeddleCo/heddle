// SPDX-License-Identifier: Apache-2.0

use std::{
    cell::RefCell,
    collections::HashSet,
    fs,
    fs::File,
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
    time::SystemTime,
};

use memmap2::{Mmap, MmapOptions};

use super::{
    CHECKSUM_CHUNK_BYTES, CHECKSUM_LEN, HEADER_LEN, INDEX_MAGIC, LARGE_OFFSET_FLAG, MAGIC,
    MAX_CHAIN_DEPTH, RECORD_BLOCK_ENTRIES, TRAILER_HEADER_LEN, TRAILER_MAGIC, VERSION,
    checked_slice,
    codec::{
        DecodeDictionary, LookupState, NameDictionary, RecordDecoder, TargetDictionary,
        decode_anchor, decode_delta, lookup_record, parse_record_header, record_base_distance,
    },
    invalid, read_u16, read_u32, read_u64, usize_from_u64,
};
use crate::{
    object::{ContentHash, Tree, TreeEntry},
    store::{HeddleError, Result},
};

pub(crate) struct Npk1Pack {
    mmap: Mmap,
    names: NameDictionary,
    targets: TargetDictionary,
    name_offset: usize,
    target_offset: usize,
    record_dictionary_offset: usize,
    record_decoder: RecordDecoder,
    records_offset: usize,
    index_offset: usize,
    index: PackIndex,
    trailer_offset: usize,
    checksum: ChecksumManifest,
    verified_chunks: Mutex<HashSet<usize>>,
}

impl Npk1Pack {
    pub(crate) fn open(path: &Path) -> Result<Self> {
        Self::open_with_validation(path, true)
    }

    pub(super) fn open_direct(path: &Path) -> Result<Self> {
        Self::open_with_validation(path, false)
    }

    fn open_with_validation(path: &Path, verify_all: bool) -> Result<Self> {
        let file = File::open(path)?;
        let len = usize::try_from(file.metadata()?.len())
            .map_err(|_| invalid("pack exceeds address space"))?;
        if len < HEADER_LEN + TRAILER_HEADER_LEN + CHECKSUM_LEN {
            return Err(invalid(
                "pack is shorter than its header and checksum manifest",
            ));
        }
        // SAFETY: the mapping is read-only and owns no references into a
        // mutable buffer. NPK1 files are immutable after their atomic rename;
        // repack unlinks old generations only after readers have dropped the
        // manager lock, while an existing mmap remains valid after unlink.
        let mmap = unsafe { MmapOptions::new().map(&file)? };
        let bytes = mmap.as_ref();
        if checked_slice(bytes, 0, 4, "pack magic")? != MAGIC {
            return Err(invalid("invalid pack magic"));
        }
        let version = read_u32(bytes, 4, "pack version")?;
        if version > VERSION {
            return Err(HeddleError::StorageFormatTooNew {
                storage: path.display().to_string(),
                found: version,
                supported: VERSION,
            });
        }
        if version == 0 {
            return Err(invalid("unsupported pack version"));
        }
        let object_count = read_u32(bytes, 8, "object count")? as usize;
        if bytes[12] as usize != MAX_CHAIN_DEPTH {
            return Err(invalid("unsupported chain-depth contract"));
        }
        if bytes[13] != 0 {
            return Err(invalid("non-zero reserved header field"));
        }
        if read_u16(bytes, 14, "record block size")? as usize != RECORD_BLOCK_ENTRIES {
            return Err(invalid("unsupported record block size"));
        }
        let name_offset = usize_from_u64(read_u64(bytes, 16, "name offset")?, "name offset")?;
        let target_offset = usize_from_u64(read_u64(bytes, 24, "target offset")?, "target offset")?;
        let records_offset =
            usize_from_u64(read_u64(bytes, 32, "records offset")?, "records offset")?;
        let index_offset = usize_from_u64(read_u64(bytes, 40, "index offset")?, "index offset")?;
        let trailer_offset =
            usize_from_u64(read_u64(bytes, 48, "trailer offset")?, "trailer offset")?;
        let dictionary_field = usize_from_u64(
            read_u64(bytes, 56, "record dictionary offset")?,
            "record dictionary offset",
        )?;
        let record_dictionary_offset = if version == 1 {
            if dictionary_field != 0 {
                return Err(invalid("non-zero reserved header field"));
            }
            records_offset
        } else {
            dictionary_field
        };
        if name_offset != HEADER_LEN
            || !(name_offset <= target_offset
                && target_offset <= record_dictionary_offset
                && record_dictionary_offset <= records_offset
                && records_offset <= index_offset
                && index_offset <= trailer_offset)
            || trailer_offset >= len
        {
            return Err(invalid("invalid pack section offsets"));
        }
        let checksum = decode_checksum_trailer(&bytes[trailer_offset..], trailer_offset)?;
        let mut verified_chunks = HashSet::new();
        verify_range_impl(
            bytes,
            trailer_offset,
            checksum,
            &mut verified_chunks,
            0,
            HEADER_LEN,
        )?;
        verify_range_impl(
            bytes,
            trailer_offset,
            checksum,
            &mut verified_chunks,
            name_offset,
            16,
        )?;
        verify_range_impl(
            bytes,
            trailer_offset,
            checksum,
            &mut verified_chunks,
            target_offset,
            8,
        )?;
        verify_range_impl(
            bytes,
            trailer_offset,
            checksum,
            &mut verified_chunks,
            record_dictionary_offset,
            records_offset - record_dictionary_offset,
        )?;
        let names = NameDictionary::decode(&bytes[name_offset..target_offset])?;
        let targets = TargetDictionary::decode(&bytes[target_offset..record_dictionary_offset])?;
        let record_decoder = RecordDecoder::new(&bytes[record_dictionary_offset..records_offset]);
        let index_header_len = (16 + 256 * 4).min(trailer_offset - index_offset);
        verify_range_impl(
            bytes,
            trailer_offset,
            checksum,
            &mut verified_chunks,
            index_offset,
            index_header_len,
        )?;
        let index = PackIndex::decode(&bytes[index_offset..trailer_offset], object_count)?;
        let pack = Self {
            mmap,
            names,
            targets,
            name_offset,
            target_offset,
            record_dictionary_offset,
            record_decoder,
            records_offset,
            index_offset,
            index,
            trailer_offset,
            checksum,
            verified_chunks: Mutex::new(verified_chunks),
        };
        if verify_all {
            pack.verify_range(0, trailer_offset)?;
            pack.validate_dictionaries()?;
            pack.index
                .validate(pack.index_bytes()?, index_offset - records_offset)?;
            pack.validate_record_graph()?;
        }
        Ok(pack)
    }

    pub(super) fn contains(&self, expected: &ContentHash) -> Result<bool> {
        Ok(self.find_object(expected)?.is_some())
    }

    pub(crate) fn ids(&self) -> impl Iterator<Item = ContentHash> + '_ {
        (0..self.index.object_count()).map(|index| {
            let row = self.index.entries_start + index * 36;
            let absolute = self.index_offset + row;
            let hash = std::array::from_fn(|byte| self.mmap[absolute + byte]);
            ContentHash::from_bytes(hash)
        })
    }

    fn object_ordinal(&self, expected: &ContentHash) -> Result<usize> {
        self.find_object(expected)?
            .ok_or_else(|| HeddleError::NotFound(format!("NPK1 tree {expected}")))
    }

    fn record_bounds(&self, ordinal: usize) -> Result<(usize, usize)> {
        if ordinal >= self.index.object_count() {
            return Err(invalid("record ordinal out of bounds"));
        }
        let index_bytes = self.index_bytes()?;
        let start = self
            .index
            .record_offset(index_bytes, ordinal, |offset, len| {
                self.verify_index_range(offset, len)
            })?;
        let end = self
            .index
            .record_offset(index_bytes, ordinal + 1, |offset, len| {
                self.verify_index_range(offset, len)
            })?;
        let start = usize_from_u64(start, "record offset")?;
        let len = usize_from_u64(
            end.checked_sub(start as u64)
                .ok_or_else(|| invalid("record offsets are reversed"))?,
            "record length",
        )?;
        let absolute = self
            .records_offset
            .checked_add(start)
            .ok_or_else(|| invalid("absolute record offset overflow"))?;
        Ok((absolute, len))
    }

    fn record_unverified(&self, ordinal: usize) -> Result<&[u8]> {
        let (absolute, len) = self.record_bounds(ordinal)?;
        checked_slice(&self.mmap, absolute, len, "record")
    }

    fn record(&self, ordinal: usize) -> Result<&[u8]> {
        let (absolute, len) = self.record_bounds(ordinal)?;
        self.verify_range(absolute, len)?;
        checked_slice(&self.mmap, absolute, len, "record")
    }

    fn verify_range(&self, offset: usize, len: usize) -> Result<()> {
        let mut verified = self
            .verified_chunks
            .lock()
            .map_err(|_| invalid("checksum cache lock is poisoned"))?;
        verify_range_impl(
            &self.mmap,
            self.trailer_offset,
            self.checksum,
            &mut verified,
            offset,
            len,
        )
    }

    fn verify_index_range(&self, offset: usize, len: usize) -> Result<()> {
        let absolute = self
            .index_offset
            .checked_add(offset)
            .ok_or_else(|| invalid("absolute index offset overflow"))?;
        self.verify_range(absolute, len)
    }

    fn index_bytes(&self) -> Result<&[u8]> {
        checked_slice(
            &self.mmap,
            self.index_offset,
            self.trailer_offset - self.index_offset,
            "index",
        )
    }

    fn verify_ids(&self) -> Result<()> {
        let len = self
            .index
            .object_count()
            .checked_mul(36)
            .ok_or_else(|| invalid("index entries overflow"))?;
        self.verify_index_range(self.index.entries_start, len)
    }

    fn find_object(&self, expected: &ContentHash) -> Result<Option<usize>> {
        let bytes = self.index_bytes()?;
        self.index.find(bytes, expected.as_bytes(), |offset, len| {
            self.verify_index_range(offset, len)
        })
    }

    fn mapped_dictionary(&self) -> MappedDictionary<'_> {
        MappedDictionary {
            pack: self,
            names: RefCell::new(None),
        }
    }

    fn name_ordinal(&self, wanted: &str) -> Result<Option<u32>> {
        let bytes = checked_slice(
            &self.mmap,
            self.name_offset,
            self.target_offset - self.name_offset,
            "name dictionary",
        )?;
        self.names.lookup(bytes, wanted, |offset, len| {
            let absolute = self
                .name_offset
                .checked_add(offset)
                .ok_or_else(|| invalid("absolute name dictionary offset overflow"))?;
            self.verify_range(absolute, len)
        })
    }

    fn validate_dictionaries(&self) -> Result<()> {
        let bytes = checked_slice(
            &self.mmap,
            self.name_offset,
            self.target_offset - self.name_offset,
            "name dictionary",
        )?;
        self.names.validate(bytes, |offset, len| {
            let absolute = self
                .name_offset
                .checked_add(offset)
                .ok_or_else(|| invalid("absolute name dictionary offset overflow"))?;
            self.verify_range(absolute, len)
        })
    }

    fn validate_record_graph(&self) -> Result<()> {
        let mut depths = Vec::with_capacity(self.index.object_count());
        for ordinal in 0..self.index.object_count() {
            let record = self.record_unverified(ordinal)?;
            let _ = parse_record_header(record)?;
            let depth = match record_base_distance(record)? {
                None => 0,
                Some(distance) => {
                    if distance == 0 || distance > ordinal {
                        return Err(invalid("delta base is not strictly backward"));
                    }
                    depths[ordinal - distance] + 1
                }
            };
            if depth > MAX_CHAIN_DEPTH {
                return Err(invalid("delta chain exceeds the depth bound"));
            }
            depths.push(depth);
        }
        Ok(())
    }

    pub(crate) fn resolve(&self, expected: &ContentHash) -> Result<Tree> {
        let mut ordinal = self.object_ordinal(expected)?;
        let mut chain = Vec::with_capacity(MAX_CHAIN_DEPTH + 1);
        let mut deltas = 0usize;
        loop {
            chain.push(ordinal);
            let record = self.record(ordinal)?;
            let Some(distance) = record_base_distance(record)? else {
                break;
            };
            deltas += 1;
            if deltas > MAX_CHAIN_DEPTH || distance == 0 || distance > ordinal {
                return Err(invalid("invalid or over-depth delta chain"));
            }
            ordinal -= distance;
        }
        let anchor = chain
            .pop()
            .ok_or_else(|| invalid("empty resolution chain"))?;
        let dictionary = self.mapped_dictionary();
        let mut tree = decode_anchor(self.record(anchor)?, &dictionary, &self.record_decoder)?;
        while let Some(delta) = chain.pop() {
            tree = decode_delta(
                self.record(delta)?,
                &tree,
                &dictionary,
                &self.record_decoder,
            )?;
        }
        let found = tree.hash();
        if found != *expected {
            return Err(HeddleError::Corruption {
                expected: *expected,
                found,
            });
        }
        Ok(tree)
    }

    pub(super) fn lookup(&self, expected: &ContentHash, name: &str) -> Result<Option<TreeEntry>> {
        let Some(wanted_name) = self.name_ordinal(name)? else {
            return Ok(None);
        };
        let dictionary = self.mapped_dictionary();
        let mut ordinal = self.object_ordinal(expected)?;
        let mut deltas = 0usize;
        loop {
            let (absolute, len) = self.record_bounds(ordinal)?;
            let record = checked_slice(&self.mmap, absolute, len, "record")?;
            // The tag, fixed set of prefix varints, and block count occupy at
            // most 32 bytes. Verify through them before trusting block_count
            // for the descriptor allocation, including across a chunk edge.
            self.verify_range(absolute, len.min(64))?;
            let header = parse_record_header(record)?;
            let header_len = header
                .blocks
                .first()
                .map_or(record.len(), |block| block.payload_offset);
            self.verify_range(absolute, header_len)?;
            if let Some(block_index) = header
                .blocks
                .partition_point(|block| block.first_name <= wanted_name)
                .checked_sub(1)
            {
                let block = &header.blocks[block_index];
                self.verify_range(
                    absolute
                        .checked_add(block.payload_offset)
                        .ok_or_else(|| invalid("absolute block offset overflow"))?,
                    block.stored_len,
                )?;
            }
            let (state, entry) =
                lookup_record(record, &dictionary, wanted_name, name, &self.record_decoder)?;
            match state {
                LookupState::Found => return Ok(entry),
                LookupState::Removed => return Ok(None),
                LookupState::Missing => {}
            }
            let Some(distance) = record_base_distance(record)? else {
                return Ok(None);
            };
            deltas += 1;
            if deltas > MAX_CHAIN_DEPTH || distance == 0 || distance > ordinal {
                return Err(invalid("invalid or over-depth lookup chain"));
            }
            ordinal -= distance;
        }
    }

    #[cfg(test)]
    pub(super) fn depth(&self, expected: &ContentHash) -> Result<usize> {
        let mut ordinal = self.object_ordinal(expected)?;
        let mut depth = 0usize;
        while let Some(distance) = record_base_distance(self.record(ordinal)?)? {
            depth += 1;
            if depth > MAX_CHAIN_DEPTH || distance == 0 || distance > ordinal {
                return Err(invalid("invalid test chain"));
            }
            ordinal -= distance;
        }
        Ok(depth)
    }

    #[cfg(test)]
    pub(super) fn verified_chunk_count(&self) -> Result<(usize, usize)> {
        let verified = self
            .verified_chunks
            .lock()
            .map_err(|_| invalid("checksum cache lock is poisoned"))?;
        Ok((verified.len(), self.checksum.chunk_count))
    }
}

struct MappedDictionary<'a> {
    pack: &'a Npk1Pack,
    names: RefCell<Option<(usize, Vec<String>)>>,
}

impl DecodeDictionary for MappedDictionary<'_> {
    fn name(&self, ordinal: u32) -> Result<String> {
        let block_index = ordinal as usize / super::NAME_RESTART;
        if let Some((_, names)) = self
            .names
            .borrow()
            .as_ref()
            .filter(|(cached, _)| *cached == block_index)
        {
            return names
                .get(ordinal as usize % super::NAME_RESTART)
                .cloned()
                .ok_or_else(|| invalid("row name ordinal out of bounds"));
        }
        let bytes = checked_slice(
            &self.pack.mmap,
            self.pack.name_offset,
            self.pack.target_offset - self.pack.name_offset,
            "name dictionary",
        )?;
        let decoded = self
            .pack
            .names
            .decode_block(bytes, ordinal, |offset, len| {
                let absolute = self
                    .pack
                    .name_offset
                    .checked_add(offset)
                    .ok_or_else(|| invalid("absolute name dictionary offset overflow"))?;
                self.pack.verify_range(absolute, len)
            })?;
        let name = decoded
            .1
            .get(ordinal as usize % super::NAME_RESTART)
            .cloned()
            .ok_or_else(|| invalid("row name ordinal out of bounds"))?;
        self.names.replace(Some(decoded));
        Ok(name)
    }

    fn target(&self, ordinal: usize) -> Result<[u8; 32]> {
        let bytes = checked_slice(
            &self.pack.mmap,
            self.pack.target_offset,
            self.pack.record_dictionary_offset - self.pack.target_offset,
            "target dictionary",
        )?;
        self.pack.targets.target(bytes, ordinal, |offset, len| {
            let absolute = self
                .pack
                .target_offset
                .checked_add(offset)
                .ok_or_else(|| invalid("absolute target dictionary offset overflow"))?;
            self.pack.verify_range(absolute, len)
        })
    }

    fn target_count(&self) -> usize {
        self.pack.targets.count()
    }
}

#[derive(Clone, Copy)]
struct ChecksumManifest {
    chunk_count: usize,
}

fn decode_checksum_trailer(bytes: &[u8], data_len: usize) -> Result<ChecksumManifest> {
    if checked_slice(bytes, 0, 4, "checksum manifest magic")? != TRAILER_MAGIC {
        return Err(invalid("invalid checksum manifest magic"));
    }
    if read_u32(bytes, 4, "checksum chunk size")? as usize != CHECKSUM_CHUNK_BYTES {
        return Err(invalid("unsupported checksum chunk size"));
    }
    let chunk_count = read_u32(bytes, 8, "checksum chunk count")? as usize;
    if chunk_count != data_len.div_ceil(CHECKSUM_CHUNK_BYTES) {
        return Err(invalid("checksum chunk count mismatch"));
    }
    if read_u32(bytes, 12, "checksum manifest reserved field")? != 0 {
        return Err(invalid("non-zero checksum manifest reserved field"));
    }
    let hashes_len = chunk_count
        .checked_mul(CHECKSUM_LEN)
        .ok_or_else(|| invalid("checksum manifest size overflow"))?;
    let checksum_offset = TRAILER_HEADER_LEN
        .checked_add(hashes_len)
        .ok_or_else(|| invalid("checksum manifest size overflow"))?;
    if checksum_offset
        .checked_add(CHECKSUM_LEN)
        .is_none_or(|expected| expected != bytes.len())
    {
        return Err(invalid("checksum manifest length mismatch"));
    }
    if blake3::hash(&bytes[..checksum_offset]).as_bytes()
        != checked_slice(
            bytes,
            checksum_offset,
            CHECKSUM_LEN,
            "checksum manifest checksum",
        )?
    {
        return Err(invalid("checksum manifest checksum mismatch"));
    }
    Ok(ChecksumManifest { chunk_count })
}

fn verify_range_impl(
    bytes: &[u8],
    data_len: usize,
    manifest: ChecksumManifest,
    verified: &mut HashSet<usize>,
    offset: usize,
    len: usize,
) -> Result<()> {
    let end = offset
        .checked_add(len)
        .ok_or_else(|| invalid("checksum range overflow"))?;
    if end > data_len {
        return Err(invalid("checksum range extends into the manifest"));
    }
    if len == 0 {
        return Ok(());
    }
    let first = offset / CHECKSUM_CHUNK_BYTES;
    let last = (end - 1) / CHECKSUM_CHUNK_BYTES;
    for index in first..=last {
        if verified.contains(&index) {
            continue;
        }
        if index >= manifest.chunk_count {
            return Err(invalid("checksum chunk ordinal out of bounds"));
        }
        let start = index
            .checked_mul(CHECKSUM_CHUNK_BYTES)
            .ok_or_else(|| invalid("checksum chunk offset overflow"))?;
        let chunk_end = start.saturating_add(CHECKSUM_CHUNK_BYTES).min(data_len);
        let chunk = checked_slice(bytes, start, chunk_end - start, "checksum chunk")?;
        let hash_offset = data_len
            .checked_add(TRAILER_HEADER_LEN)
            .and_then(|offset| {
                index
                    .checked_mul(CHECKSUM_LEN)
                    .and_then(|row| offset.checked_add(row))
            })
            .ok_or_else(|| invalid("checksum hash offset overflow"))?;
        let expected = checked_slice(bytes, hash_offset, CHECKSUM_LEN, "checksum chunk hash")?;
        if blake3::hash(chunk).as_bytes() != expected {
            return Err(invalid(format!("checksum mismatch in chunk {index}")));
        }
        verified.insert(index);
    }
    Ok(())
}

#[derive(Clone, Copy)]
struct PackIndex {
    object_count: usize,
    escape_count: usize,
    entries_start: usize,
    offsets_start: usize,
    escapes_start: usize,
    checksum_offset: usize,
    len: usize,
}

impl PackIndex {
    fn decode(bytes: &[u8], expected_objects: usize) -> Result<Self> {
        if checked_slice(bytes, 0, 4, "index magic")? != INDEX_MAGIC {
            return Err(invalid("invalid index magic"));
        }
        let object_count = read_u32(bytes, 4, "index object count")? as usize;
        if object_count != expected_objects {
            return Err(invalid("index object count disagrees with pack header"));
        }
        let escape_count = read_u32(bytes, 8, "large-offset count")? as usize;
        if read_u32(bytes, 12, "index reserved field")? != 0 {
            return Err(invalid("non-zero index reserved field"));
        }
        let entries_start = 16usize + 256 * 4;
        let entries_len = object_count
            .checked_mul(36)
            .ok_or_else(|| invalid("index entries overflow"))?;
        let offsets_start = entries_start
            .checked_add(entries_len)
            .ok_or_else(|| invalid("index offset table position overflow"))?;
        let offsets_len = object_count
            .checked_add(1)
            .and_then(|count| count.checked_mul(4))
            .ok_or_else(|| invalid("record offset table overflow"))?;
        let escapes_start = offsets_start
            .checked_add(offsets_len)
            .ok_or_else(|| invalid("large-offset table position overflow"))?;
        let escapes_len = escape_count
            .checked_mul(8)
            .ok_or_else(|| invalid("large-offset table overflow"))?;
        let checksum_offset = escapes_start
            .checked_add(escapes_len)
            .ok_or_else(|| invalid("index checksum position overflow"))?;
        if checksum_offset
            .checked_add(CHECKSUM_LEN)
            .is_none_or(|expected| expected != bytes.len())
        {
            return Err(invalid("index length mismatch"));
        }

        let mut previous_fanout = 0u32;
        for index in 0..256 {
            let count = read_u32(bytes, 16 + index * 4, "fanout row")?;
            if count < previous_fanout {
                return Err(invalid("index fanout is not monotonic"));
            }
            previous_fanout = count;
        }
        if previous_fanout as usize != object_count {
            return Err(invalid("index fanout total mismatch"));
        }
        Ok(Self {
            object_count,
            escape_count,
            entries_start,
            offsets_start,
            escapes_start,
            checksum_offset,
            len: bytes.len(),
        })
    }

    fn object_count(self) -> usize {
        self.object_count
    }

    fn find(
        self,
        bytes: &[u8],
        wanted: &[u8; 32],
        mut verify: impl FnMut(usize, usize) -> Result<()>,
    ) -> Result<Option<usize>> {
        self.check_bytes(bytes)?;
        let bucket = wanted[0] as usize;
        let fanout_row = 16 + bucket * 4;
        verify(fanout_row, 4)?;
        let mut high = read_u32(bytes, fanout_row, "fanout row")? as usize;
        let mut low = if bucket == 0 {
            0
        } else {
            verify(fanout_row - 4, 4)?;
            read_u32(bytes, fanout_row - 4, "fanout row")? as usize
        };
        while low < high {
            let middle = low + (high - low) / 2;
            let offset = self.entry_offset(middle)?;
            verify(offset, 36)?;
            let hash = self.hash(bytes, middle)?;
            match hash.cmp(wanted) {
                std::cmp::Ordering::Less => low = middle + 1,
                std::cmp::Ordering::Greater => high = middle,
                std::cmp::Ordering::Equal => {
                    let ordinal = read_u32(bytes, offset + 32, "record ordinal")? as usize;
                    if ordinal >= self.object_count {
                        return Err(invalid("record ordinal out of bounds"));
                    }
                    return Ok(Some(ordinal));
                }
            }
        }
        Ok(None)
    }

    fn hash(self, bytes: &[u8], index: usize) -> Result<[u8; 32]> {
        self.check_bytes(bytes)?;
        let offset = self.entry_offset(index)?;
        checked_slice(bytes, offset, 32, "index hash")?
            .try_into()
            .map_err(|_| invalid("invalid index hash"))
    }

    fn record_offset_range(self, ordinal: usize) -> Result<(usize, usize)> {
        if ordinal > self.object_count {
            return Err(invalid("record offset ordinal out of bounds"));
        }
        let offset = self
            .offsets_start
            .checked_add(
                ordinal
                    .checked_mul(4)
                    .ok_or_else(|| invalid("record offset table overflow"))?,
            )
            .ok_or_else(|| invalid("record offset table overflow"))?;
        Ok((offset, 4))
    }

    fn record_offset(
        self,
        bytes: &[u8],
        ordinal: usize,
        mut verify: impl FnMut(usize, usize) -> Result<()>,
    ) -> Result<u64> {
        self.check_bytes(bytes)?;
        let (row, len) = self.record_offset_range(ordinal)?;
        verify(row, len)?;
        let encoded = read_u32(bytes, row, "record offset")?;
        if encoded & LARGE_OFFSET_FLAG == 0 {
            return Ok(encoded as u64);
        }
        let escape = (encoded & !LARGE_OFFSET_FLAG) as usize;
        if escape >= self.escape_count {
            return Err(invalid("large record offset ordinal out of bounds"));
        }
        let offset = self
            .escapes_start
            .checked_add(
                escape
                    .checked_mul(8)
                    .ok_or_else(|| invalid("large-offset table overflow"))?,
            )
            .ok_or_else(|| invalid("large-offset table overflow"))?;
        verify(offset, 8)?;
        read_u64(bytes, offset, "large record offset")
    }

    fn validate(self, bytes: &[u8], records_len: usize) -> Result<()> {
        self.check_bytes(bytes)?;
        if blake3::hash(&bytes[..self.checksum_offset]).as_bytes()
            != checked_slice(bytes, self.checksum_offset, CHECKSUM_LEN, "index checksum")?
        {
            return Err(invalid("index checksum mismatch"));
        }
        let mut seen_ordinals = HashSet::with_capacity(self.object_count);
        let mut previous_hash = None;
        let mut bucket_start = 0usize;
        for bucket in 0..256usize {
            let bucket_end = read_u32(bytes, 16 + bucket * 4, "fanout row")? as usize;
            for index in bucket_start..bucket_end {
                let hash = self.hash(bytes, index)?;
                if hash[0] as usize != bucket
                    || previous_hash.is_some_and(|previous| previous >= hash)
                {
                    return Err(invalid("index hashes or fanout are not strictly sorted"));
                }
                previous_hash = Some(hash);
                let ordinal = read_u32(bytes, self.entry_offset(index)? + 32, "record ordinal")?;
                if ordinal as usize >= self.object_count || !seen_ordinals.insert(ordinal) {
                    return Err(invalid("record ordinal is duplicate or out of bounds"));
                }
            }
            bucket_start = bucket_end;
        }

        let mut previous_offset = None;
        for ordinal in 0..=self.object_count {
            let offset = self.record_offset(bytes, ordinal, |_, _| Ok(()))?;
            if ordinal == 0 && offset != 0 {
                return Err(invalid("record offsets do not start at zero"));
            }
            if previous_offset.is_some_and(|previous| previous >= offset) && ordinal > 0 {
                return Err(invalid("record offsets are not strictly increasing"));
            }
            previous_offset = Some(offset);
        }
        if previous_offset != Some(records_len as u64) {
            return Err(invalid("record offsets do not span the record section"));
        }
        Ok(())
    }

    fn entry_offset(self, index: usize) -> Result<usize> {
        if index >= self.object_count {
            return Err(invalid("index entry ordinal out of bounds"));
        }
        self.entries_start
            .checked_add(
                index
                    .checked_mul(36)
                    .ok_or_else(|| invalid("index entry offset overflow"))?,
            )
            .ok_or_else(|| invalid("index entry offset overflow"))
    }

    fn check_bytes(self, bytes: &[u8]) -> Result<()> {
        if bytes.len() != self.len {
            return Err(invalid("index view length changed"));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct PackFingerprint {
    path: PathBuf,
    len: u64,
    modified: Option<SystemTime>,
}

struct CachedPack {
    fingerprint: PackFingerprint,
    pack: OnceLock<std::result::Result<Npk1Pack, String>>,
}

impl CachedPack {
    fn open(&self) -> Result<&Npk1Pack> {
        match self.pack.get_or_init(|| {
            Npk1Pack::open_direct(&self.fingerprint.path).map_err(|error| error.to_string())
        }) {
            Ok(pack) => Ok(pack),
            Err(error) => Err(invalid(format!(
                "cannot open {}: {error}",
                self.fingerprint.path.display()
            ))),
        }
    }
}

pub(crate) struct Npk1Manager {
    packs_dir: PathBuf,
    packs: Vec<CachedPack>,
    fingerprints: Vec<PackFingerprint>,
}

impl Npk1Manager {
    pub(crate) fn new(packs_dir: PathBuf) -> Self {
        let fingerprints = discover(&packs_dir).unwrap_or_default();
        let packs = cached(&fingerprints);
        Self {
            packs_dir,
            packs,
            fingerprints,
        }
    }

    pub(crate) fn reload(&mut self) -> Result<()> {
        let fingerprints = discover(&self.packs_dir)?;
        self.packs = cached(&fingerprints);
        self.fingerprints = fingerprints;
        Ok(())
    }

    pub(crate) fn needs_reload(&self) -> Result<bool> {
        Ok(discover(&self.packs_dir)? != self.fingerprints)
    }

    pub(crate) fn file_paths(&self) -> Vec<&Path> {
        self.fingerprints
            .iter()
            .map(|fingerprint| fingerprint.path.as_path())
            .collect()
    }

    pub(crate) fn has_tree(&self, hash: &ContentHash) -> Result<bool> {
        for cached in self.packs.iter().rev() {
            if cached.open()?.contains(hash)? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    pub(crate) fn get_tree(&self, hash: &ContentHash) -> Result<Option<Tree>> {
        for cached in self.packs.iter().rev() {
            let pack = cached.open()?;
            if pack.contains(hash)? {
                return pack.resolve(hash).map(Some);
            }
        }
        Ok(None)
    }

    pub(crate) fn get_entry(&self, hash: &ContentHash, name: &str) -> Result<Option<TreeEntry>> {
        for cached in self.packs.iter().rev() {
            let pack = cached.open()?;
            if pack.contains(hash)? {
                return pack.lookup(hash, name);
            }
        }
        Ok(None)
    }

    pub(crate) fn list_ids(&self) -> Result<Vec<ContentHash>> {
        let mut ids = HashSet::new();
        for cached in &self.packs {
            let pack = cached.open()?;
            pack.verify_ids()?;
            ids.extend(pack.ids());
        }
        let mut ids = ids.into_iter().collect::<Vec<_>>();
        ids.sort();
        Ok(ids)
    }
}

fn cached(fingerprints: &[PackFingerprint]) -> Vec<CachedPack> {
    fingerprints
        .iter()
        .cloned()
        .map(|fingerprint| CachedPack {
            fingerprint,
            pack: OnceLock::new(),
        })
        .collect()
}

fn discover(packs_dir: &Path) -> Result<Vec<PackFingerprint>> {
    if !packs_dir.exists() {
        return Ok(Vec::new());
    }
    let mut packs = Vec::new();
    for entry in fs::read_dir(packs_dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().is_none_or(|extension| extension != "npk") {
            continue;
        }
        let metadata = entry.metadata()?;
        if !metadata.is_file() {
            continue;
        }
        packs.push(PackFingerprint {
            path,
            len: metadata.len(),
            modified: metadata.modified().ok(),
        });
    }
    packs.sort_by(|left, right| {
        left.modified
            .cmp(&right.modified)
            .then_with(|| left.path.cmp(&right.path))
    });
    Ok(packs)
}
