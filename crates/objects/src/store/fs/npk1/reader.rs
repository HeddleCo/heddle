// SPDX-License-Identifier: Apache-2.0

use std::{
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
        Dictionary, LookupState, RecordDecoder, decode_anchor, decode_delta, lookup_record,
        parse_record_header, record_base_distance,
    },
    invalid, read_u16, read_u32, read_u64, usize_from_u64,
};
use crate::{
    object::{ContentHash, Tree, TreeEntry},
    store::{HeddleError, Result},
};

type DecodedIndex = (Vec<([u8; 32], u32)>, Vec<u64>);

pub(crate) struct Npk1Pack {
    mmap: Mmap,
    dictionary: Dictionary,
    record_decoder: RecordDecoder,
    records_offset: usize,
    record_offsets: Vec<u64>,
    object_index: Vec<([u8; 32], u32)>,
    trailer_offset: usize,
    chunk_hashes: Vec<[u8; CHECKSUM_LEN]>,
    verified_chunks: Mutex<Vec<bool>>,
}

impl Npk1Pack {
    pub(crate) fn open(path: &Path) -> Result<Self> {
        Self::open_with_validation(path, true)
    }

    fn open_direct(path: &Path) -> Result<Self> {
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
        let chunk_hashes = decode_checksum_trailer(&bytes[trailer_offset..], trailer_offset)?;
        let mut verified_chunks = vec![false; chunk_hashes.len()];
        verify_range_impl(
            bytes,
            trailer_offset,
            &chunk_hashes,
            &mut verified_chunks,
            0,
            HEADER_LEN,
        )?;
        verify_range_impl(
            bytes,
            trailer_offset,
            &chunk_hashes,
            &mut verified_chunks,
            name_offset,
            records_offset - name_offset,
        )?;
        verify_range_impl(
            bytes,
            trailer_offset,
            &chunk_hashes,
            &mut verified_chunks,
            index_offset,
            trailer_offset - index_offset,
        )?;
        let dictionary = Dictionary::decode(
            &bytes[name_offset..target_offset],
            &bytes[target_offset..record_dictionary_offset],
        )?;
        let record_decoder = RecordDecoder::new(&bytes[record_dictionary_offset..records_offset]);
        let (object_index, record_offsets) = decode_index(
            &bytes[index_offset..trailer_offset],
            object_count,
            index_offset - records_offset,
        )?;
        let pack = Self {
            mmap,
            dictionary,
            record_decoder,
            records_offset,
            record_offsets,
            object_index,
            trailer_offset,
            chunk_hashes,
            verified_chunks: Mutex::new(verified_chunks),
        };
        if verify_all {
            pack.verify_range(0, trailer_offset)?;
            pack.validate_record_graph()?;
        }
        Ok(pack)
    }

    pub(super) fn contains(&self, expected: &ContentHash) -> bool {
        self.object_index
            .binary_search_by_key(expected.as_bytes(), |(hash, _)| *hash)
            .is_ok()
    }

    pub(crate) fn ids(&self) -> impl Iterator<Item = ContentHash> + '_ {
        self.object_index
            .iter()
            .map(|(hash, _)| ContentHash::from_bytes(*hash))
    }

    fn object_ordinal(&self, expected: &ContentHash) -> Result<usize> {
        let index = self
            .object_index
            .binary_search_by_key(expected.as_bytes(), |(hash, _)| *hash)
            .map_err(|_| HeddleError::NotFound(format!("NPK1 tree {expected}")))?;
        Ok(self.object_index[index].1 as usize)
    }

    fn record_bounds(&self, ordinal: usize) -> Result<(usize, usize)> {
        let start = *self
            .record_offsets
            .get(ordinal)
            .ok_or_else(|| invalid("record ordinal out of bounds"))?;
        let end = *self
            .record_offsets
            .get(ordinal + 1)
            .ok_or_else(|| invalid("record end ordinal out of bounds"))?;
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
            &self.chunk_hashes,
            &mut verified,
            offset,
            len,
        )
    }

    fn validate_record_graph(&self) -> Result<()> {
        let mut depths = Vec::with_capacity(self.object_index.len());
        for ordinal in 0..self.object_index.len() {
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
        let mut tree = decode_anchor(self.record(anchor)?, &self.dictionary, &self.record_decoder)?;
        while let Some(delta) = chain.pop() {
            tree = decode_delta(
                self.record(delta)?,
                &tree,
                &self.dictionary,
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
        let Some(wanted_name) = self.dictionary.name_ids.get(name).copied() else {
            return Ok(None);
        };
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
                lookup_record(record, &self.dictionary, wanted_name, &self.record_decoder)?;
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
}

fn decode_checksum_trailer(bytes: &[u8], data_len: usize) -> Result<Vec<[u8; CHECKSUM_LEN]>> {
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
    let mut hashes = Vec::with_capacity(chunk_count);
    for index in 0..chunk_count {
        hashes.push(
            checked_slice(
                bytes,
                TRAILER_HEADER_LEN + index * CHECKSUM_LEN,
                CHECKSUM_LEN,
                "checksum chunk hash",
            )?
            .try_into()
            .map_err(|_| invalid("invalid checksum chunk hash"))?,
        );
    }
    Ok(hashes)
}

fn verify_range_impl(
    bytes: &[u8],
    data_len: usize,
    hashes: &[[u8; CHECKSUM_LEN]],
    verified: &mut [bool],
    offset: usize,
    len: usize,
) -> Result<()> {
    if hashes.len() != verified.len() {
        return Err(invalid("checksum cache length mismatch"));
    }
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
        if *verified
            .get(index)
            .ok_or_else(|| invalid("checksum chunk ordinal out of bounds"))?
        {
            continue;
        }
        let start = index
            .checked_mul(CHECKSUM_CHUNK_BYTES)
            .ok_or_else(|| invalid("checksum chunk offset overflow"))?;
        let chunk_end = start.saturating_add(CHECKSUM_CHUNK_BYTES).min(data_len);
        let chunk = checked_slice(bytes, start, chunk_end - start, "checksum chunk")?;
        let expected = hashes
            .get(index)
            .ok_or_else(|| invalid("checksum hash ordinal out of bounds"))?;
        if blake3::hash(chunk).as_bytes() != expected {
            return Err(invalid(format!("checksum mismatch in chunk {index}")));
        }
        verified[index] = true;
    }
    Ok(())
}

fn decode_index(bytes: &[u8], expected_objects: usize, records_len: usize) -> Result<DecodedIndex> {
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
    if blake3::hash(&bytes[..checksum_offset]).as_bytes()
        != checked_slice(bytes, checksum_offset, CHECKSUM_LEN, "index checksum")?
    {
        return Err(invalid("index checksum mismatch"));
    }

    let mut fanout = [0u32; 256];
    let mut previous_fanout = 0u32;
    for (index, count) in fanout.iter_mut().enumerate() {
        *count = read_u32(bytes, 16 + index * 4, "fanout row")?;
        if *count < previous_fanout {
            return Err(invalid("index fanout is not monotonic"));
        }
        previous_fanout = *count;
    }
    if fanout[255] as usize != object_count {
        return Err(invalid("index fanout total mismatch"));
    }
    let mut object_index = Vec::with_capacity(object_count);
    let mut seen_ordinals = HashSet::with_capacity(object_count);
    for index in 0..object_count {
        let offset = entries_start + index * 36;
        let hash: [u8; 32] = checked_slice(bytes, offset, 32, "index hash")?
            .try_into()
            .map_err(|_| invalid("invalid index hash"))?;
        if object_index
            .last()
            .is_some_and(|(previous, _)| previous >= &hash)
        {
            return Err(invalid("index hashes are not strictly sorted"));
        }
        let ordinal = read_u32(bytes, offset + 32, "record ordinal")?;
        if ordinal as usize >= object_count || !seen_ordinals.insert(ordinal) {
            return Err(invalid("record ordinal is duplicate or out of bounds"));
        }
        object_index.push((hash, ordinal));
    }
    let mut recomputed_fanout = [0u32; 256];
    for (hash, _) in &object_index {
        recomputed_fanout[hash[0] as usize] += 1;
    }
    let mut cumulative = 0u32;
    for count in &mut recomputed_fanout {
        cumulative += *count;
        *count = cumulative;
    }
    if recomputed_fanout != fanout {
        return Err(invalid("index fanout does not match its hashes"));
    }

    let escapes = (0..escape_count)
        .map(|index| read_u64(bytes, escapes_start + index * 8, "large record offset"))
        .collect::<Result<Vec<_>>>()?;
    let mut record_offsets = Vec::with_capacity(object_count + 1);
    for index in 0..=object_count {
        let encoded = read_u32(bytes, offsets_start + index * 4, "record offset")?;
        let offset = if encoded & LARGE_OFFSET_FLAG == 0 {
            encoded as u64
        } else {
            let escape = (encoded & !LARGE_OFFSET_FLAG) as usize;
            *escapes
                .get(escape)
                .ok_or_else(|| invalid("large record offset ordinal out of bounds"))?
        };
        if record_offsets
            .last()
            .is_some_and(|previous| *previous >= offset)
            && index > 0
        {
            return Err(invalid("record offsets are not strictly increasing"));
        }
        record_offsets.push(offset);
    }
    if record_offsets.first() != Some(&0) || record_offsets.last() != Some(&(records_len as u64)) {
        return Err(invalid("record offsets do not span the record section"));
    }
    Ok((object_index, record_offsets))
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
            if cached.open()?.contains(hash) {
                return Ok(true);
            }
        }
        Ok(false)
    }

    pub(crate) fn get_tree(&self, hash: &ContentHash) -> Result<Option<Tree>> {
        for cached in self.packs.iter().rev() {
            let pack = cached.open()?;
            if pack.contains(hash) {
                return pack.resolve(hash).map(Some);
            }
        }
        Ok(None)
    }

    pub(crate) fn get_entry(&self, hash: &ContentHash, name: &str) -> Result<Option<TreeEntry>> {
        for cached in self.packs.iter().rev() {
            let pack = cached.open()?;
            if pack.contains(hash) {
                return pack.lookup(hash, name);
            }
        }
        Ok(None)
    }

    pub(crate) fn list_ids(&self) -> Result<Vec<ContentHash>> {
        let mut ids = HashSet::new();
        for cached in &self.packs {
            ids.extend(cached.open()?.ids());
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
