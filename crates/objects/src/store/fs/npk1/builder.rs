// SPDX-License-Identifier: Apache-2.0

use std::{
    cmp::Reverse,
    collections::{HashMap, HashSet, VecDeque},
    fs::OpenOptions,
    io::{BufWriter, Read, Seek, SeekFrom, Write},
    path::Path,
};

use super::{
    CHECKSUM_CHUNK_BYTES, EXACT_CANDIDATES, HEADER_LEN, INDEX_MAGIC, LARGE_OFFSET_FLAG, MAGIC,
    MAX_CHAIN_DEPTH, RECORD_BLOCK_ENTRIES, TRAILER_HEADER_LEN, TRAILER_MAGIC, VERSION,
    WINDOW_BUCKET_LIMIT,
    codec::{Dictionary, encode_anchor, encode_delta, note_tree_dictionary_rows},
    invalid,
};
use crate::{
    object::{ContentHash, Tree, tree_delta},
    store::{
        HeddleError, ObjectStore, Result,
        fs::FsStore,
        pack::{RepackContext, RepackError},
    },
};

#[derive(Debug)]
pub(crate) enum Npk1BuildError {
    Store(HeddleError),
    Cancelled(RepackError),
}

impl From<HeddleError> for Npk1BuildError {
    fn from(error: HeddleError) -> Self {
        Self::Store(error)
    }
}

impl From<std::io::Error> for Npk1BuildError {
    fn from(error: std::io::Error) -> Self {
        Self::Store(error.into())
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct Npk1Build {
    pub(crate) logical_bytes: u64,
}

#[derive(Clone, Copy, Debug)]
struct TreeSketch {
    entries: usize,
    shape: u64,
    minima: [u64; 4],
}

fn mix64(mut value: u64) -> u64 {
    value ^= value >> 30;
    value = value.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    value ^= value >> 27;
    value = value.wrapping_mul(0x94d0_49bb_1331_11eb);
    value ^ (value >> 31)
}

fn name_hash(name: &str) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in name.bytes() {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(0x100_0000_01b3);
    }
    hash
}

fn tree_sketch(tree: &Tree) -> TreeSketch {
    let mut shape = 0x6e70_6b31_7368_6170u64;
    let salts = [
        0x243f_6a88_85a3_08d3,
        0x1319_8a2e_0370_7344,
        0xa409_3822_299f_31d0,
        0x082e_fa98_ec4e_6c89,
    ];
    let mut minima = [u64::MAX; 4];
    for entry in tree.entries() {
        let hash = name_hash(entry.name()) ^ entry.entry_type().to_byte() as u64;
        shape = mix64(shape ^ hash.rotate_left(17));
        for (minimum, salt) in minima.iter_mut().zip(salts) {
            *minimum = (*minimum).min(mix64(hash ^ salt));
        }
    }
    if tree.is_empty() {
        minima = salts;
    }
    TreeSketch {
        entries: tree.len(),
        shape,
        minima,
    }
}

fn size_bucket(entries: usize) -> usize {
    if entries == 0 {
        0
    } else {
        (usize::BITS - entries.leading_zeros()) as usize
    }
}

fn retain_recent(bucket: &mut VecDeque<usize>, index: usize) {
    bucket.push_back(index);
    if bucket.len() > WINDOW_BUCKET_LIMIT {
        bucket.pop_front();
    }
}

#[derive(Default)]
struct CandidateWindow {
    shapes: HashMap<(usize, u64), VecDeque<usize>>,
    minima: HashMap<(usize, usize, u64), VecDeque<usize>>,
    sizes: HashMap<usize, VecDeque<usize>>,
}

impl CandidateWindow {
    fn insert(&mut self, index: usize, sketch: TreeSketch) {
        retain_recent(
            self.shapes
                .entry((sketch.entries, sketch.shape))
                .or_default(),
            index,
        );
        let size = size_bucket(sketch.entries);
        for (band, minimum) in sketch.minima.into_iter().enumerate() {
            retain_recent(self.minima.entry((size, band, minimum)).or_default(), index);
        }
        retain_recent(self.sizes.entry(size).or_default(), index);
    }

    fn candidates(
        &self,
        sketch: TreeSketch,
        sketches: &[TreeSketch],
        parent: Option<usize>,
        ordinals: &[usize],
    ) -> Vec<(usize, bool)> {
        let mut candidates = HashSet::new();
        if let Some(parent) = parent
            && ordinals
                .get(parent)
                .is_some_and(|ordinal| *ordinal != usize::MAX)
        {
            candidates.insert(parent);
        }
        if let Some(bucket) = self.shapes.get(&(sketch.entries, sketch.shape)) {
            candidates.extend(bucket.iter().copied());
        }
        let size = size_bucket(sketch.entries);
        for (band, minimum) in sketch.minima.into_iter().enumerate() {
            if let Some(bucket) = self.minima.get(&(size, band, minimum)) {
                candidates.extend(bucket.iter().copied());
            }
        }
        for neighboring_size in size.saturating_sub(1)..=size.saturating_add(1) {
            if let Some(bucket) = self.sizes.get(&neighboring_size) {
                candidates.extend(bucket.iter().copied());
            }
        }
        let mut ranked = candidates
            .into_iter()
            .filter(|candidate| {
                let base_entries = sketches[*candidate].entries;
                let difference = base_entries.abs_diff(sketch.entries);
                difference <= sketch.entries.max(base_entries).div_ceil(2).max(64)
                    || Some(*candidate) == parent
            })
            .map(|candidate| {
                let base = sketches[candidate];
                let exact_shape = base.entries == sketch.entries && base.shape == sketch.shape;
                let matching_minima = base
                    .minima
                    .iter()
                    .zip(sketch.minima)
                    .filter(|(left, right)| **left == *right)
                    .count();
                let difference = base.entries.abs_diff(sketch.entries);
                let score = (exact_shape as u64) << 63
                    | (matching_minima as u64) << 56
                    | (u32::MAX as usize - difference.min(u32::MAX as usize)) as u64;
                (
                    candidate,
                    Some(candidate) == parent,
                    score,
                    ordinals[candidate],
                )
            })
            .collect::<Vec<_>>();
        ranked.sort_by_key(|(candidate, is_parent, score, ordinal)| {
            (
                Reverse(*is_parent),
                Reverse(*score),
                Reverse(*ordinal),
                Reverse(*candidate),
            )
        });
        ranked.truncate(EXACT_CANDIDATES);
        ranked
            .into_iter()
            .map(|(candidate, is_parent, _, _)| (candidate, is_parent))
            .collect()
    }
}

#[derive(Clone, Copy)]
struct Plan {
    depth: usize,
}

struct CandidateRecord {
    base: usize,
    depth: usize,
    bytes: Vec<u8>,
}

fn load_tree(store: &FsStore, hash: ContentHash) -> Result<Tree> {
    ObjectStore::get_tree(store, &hash)?
        .ok_or_else(|| invalid(format!("tree disappeared while packing: {hash}")))
}

pub(crate) fn build_npk1_pack(
    store: &FsStore,
    hashes: &[ContentHash],
    historical_parents: &HashMap<ContentHash, ContentHash>,
    output: &Path,
    context: &RepackContext,
) -> std::result::Result<Npk1Build, Npk1BuildError> {
    if hashes.is_empty() {
        return Err(invalid("cannot build an empty tree pack").into());
    }
    if hashes.len() > u32::MAX as usize {
        return Err(invalid("object count exceeds u32 ordinals").into());
    }
    let indexes = hashes
        .iter()
        .enumerate()
        .map(|(index, hash)| (*hash, index))
        .collect::<HashMap<_, _>>();
    if indexes.len() != hashes.len() {
        return Err(invalid("input contains duplicate tree hashes").into());
    }
    let parent_indexes = hashes
        .iter()
        .map(|hash| {
            historical_parents
                .get(hash)
                .and_then(|parent| indexes.get(parent).copied())
                .filter(|parent| hashes[*parent] != *hash)
        })
        .collect::<Vec<_>>();

    let mut names = HashSet::new();
    let mut target_counts = HashMap::new();
    let mut sketches = Vec::with_capacity(hashes.len());
    let mut logical_bytes = 0u64;
    for hash in hashes {
        let tree = load_tree(store, *hash)?;
        note_tree_dictionary_rows(&tree, &mut names, &mut target_counts);
        sketches.push(tree_sketch(&tree));
        logical_bytes = logical_bytes
            .saturating_add(tree.encode_lean().map_err(HeddleError::from)?.len() as u64);
        context
            .checkpoint(tree.len() as u64)
            .map_err(Npk1BuildError::Cancelled)?;
    }
    let dictionary = Dictionary::from_counts(names.into_iter().collect(), target_counts)?;
    let name_bytes = dictionary.encode_names()?;
    let target_bytes = dictionary.encode_targets()?;

    let file = OpenOptions::new()
        .create_new(true)
        .read(true)
        .write(true)
        .open(output)?;
    let mut writer = BufWriter::new(file);
    writer.write_all(&[0u8; HEADER_LEN])?;
    let name_offset = HEADER_LEN as u64;
    writer.write_all(&name_bytes)?;
    let target_offset = name_offset.saturating_add(name_bytes.len() as u64);
    writer.write_all(&target_bytes)?;
    let records_offset = target_offset.saturating_add(target_bytes.len() as u64);

    let mut plans = vec![None::<Plan>; hashes.len()];
    let mut ordinals = vec![usize::MAX; hashes.len()];
    let mut order = Vec::with_capacity(hashes.len());
    let mut record_offsets = Vec::with_capacity(hashes.len() + 1);
    let mut record_position = 0u64;
    let mut window = CandidateWindow::default();

    for start in 0..hashes.len() {
        if plans[start].is_some() {
            continue;
        }
        let mut lineage = Vec::new();
        let mut cursor = start;
        let mut seen = HashSet::new();
        while plans[cursor].is_none() && seen.insert(cursor) {
            lineage.push(cursor);
            let Some(parent) = parent_indexes[cursor] else {
                break;
            };
            cursor = parent;
        }
        while let Some(index) = lineage.pop() {
            if plans[index].is_some() {
                continue;
            }
            let ordinal = order.len();
            ordinals[index] = ordinal;
            let current = load_tree(store, hashes[index])?;
            let anchor = encode_anchor(&current, &dictionary)?;
            let candidate_indexes =
                window.candidates(sketches[index], &sketches, parent_indexes[index], &ordinals);
            let mut candidates = Vec::with_capacity(candidate_indexes.len());
            for (base, _) in candidate_indexes {
                let base_ordinal = ordinals[base];
                if base_ordinal == usize::MAX || base_ordinal >= ordinal {
                    continue;
                }
                let base_plan =
                    plans[base].ok_or_else(|| invalid("candidate has no completed plan"))?;
                let depth = base_plan.depth + 1;
                if depth > MAX_CHAIN_DEPTH {
                    continue;
                }
                let base_tree = load_tree(store, hashes[base])?;
                let ops = tree_delta(&base_tree, &current);
                let bytes = encode_delta(ordinal - base_ordinal, &current, &ops, &dictionary)?;
                context
                    .checkpoint(bytes.len() as u64)
                    .map_err(Npk1BuildError::Cancelled)?;
                candidates.push(CandidateRecord { base, depth, bytes });
            }
            let selected = candidates
                .into_iter()
                .filter(|candidate| candidate.bytes.len() < anchor.len())
                .min_by_key(|candidate| (candidate.bytes.len(), Reverse(ordinals[candidate.base])));
            let (record, depth) = match selected {
                Some(candidate) => (candidate.bytes, candidate.depth),
                None => (anchor, 0),
            };
            record_offsets.push(record_position);
            writer.write_all(&record)?;
            record_position = record_position
                .checked_add(record.len() as u64)
                .ok_or_else(|| invalid("record section exceeds u64"))?;
            plans[index] = Some(Plan { depth });
            order.push(index);
            window.insert(index, sketches[index]);
        }
    }
    if order.len() != hashes.len() {
        return Err(invalid("not every input tree received a record").into());
    }
    record_offsets.push(record_position);
    let index_offset = records_offset
        .checked_add(record_position)
        .ok_or_else(|| invalid("index offset overflow"))?;
    let index_bytes = encode_index(hashes, &order, &record_offsets)?;
    writer.write_all(&index_bytes)?;
    let trailer_offset = index_offset
        .checked_add(index_bytes.len() as u64)
        .ok_or_else(|| invalid("pack trailer offset overflow"))?;

    writer.seek(SeekFrom::Start(0))?;
    writer.write_all(&encode_header(
        hashes.len(),
        name_offset,
        target_offset,
        records_offset,
        index_offset,
        trailer_offset,
    )?)?;
    writer.flush()?;
    let mut file = writer
        .into_inner()
        .map_err(|error| HeddleError::Io(error.into_error()))?;
    let trailer = encode_checksum_trailer(&mut file, trailer_offset)?;
    file.seek(SeekFrom::Start(trailer_offset))?;
    file.write_all(&trailer)?;
    file.sync_all()?;
    Ok(Npk1Build { logical_bytes })
}

fn encode_header(
    object_count: usize,
    name_offset: u64,
    target_offset: u64,
    records_offset: u64,
    index_offset: u64,
    trailer_offset: u64,
) -> Result<[u8; HEADER_LEN]> {
    let object_count =
        u32::try_from(object_count).map_err(|_| invalid("object count exceeds header field"))?;
    let mut header = [0u8; HEADER_LEN];
    header[0..4].copy_from_slice(MAGIC);
    header[4..8].copy_from_slice(&VERSION.to_le_bytes());
    header[8..12].copy_from_slice(&object_count.to_le_bytes());
    header[12] = MAX_CHAIN_DEPTH as u8;
    header[14..16].copy_from_slice(&(RECORD_BLOCK_ENTRIES as u16).to_le_bytes());
    header[16..24].copy_from_slice(&name_offset.to_le_bytes());
    header[24..32].copy_from_slice(&target_offset.to_le_bytes());
    header[32..40].copy_from_slice(&records_offset.to_le_bytes());
    header[40..48].copy_from_slice(&index_offset.to_le_bytes());
    header[48..56].copy_from_slice(&trailer_offset.to_le_bytes());
    Ok(header)
}

fn encode_index(
    hashes: &[ContentHash],
    order: &[usize],
    record_offsets: &[u64],
) -> Result<Vec<u8>> {
    let mut by_hash = order
        .iter()
        .enumerate()
        .map(|(ordinal, source)| (*hashes[*source].as_bytes(), ordinal))
        .collect::<Vec<_>>();
    by_hash.sort_by_key(|(hash, _)| *hash);
    if by_hash.windows(2).any(|pair| pair[0].0 >= pair[1].0) {
        return Err(invalid("object index hashes are not unique"));
    }
    let mut fanout = [0u32; 256];
    for (hash, _) in &by_hash {
        fanout[hash[0] as usize] = fanout[hash[0] as usize].saturating_add(1);
    }
    let mut cumulative = 0u32;
    for count in &mut fanout {
        cumulative = cumulative
            .checked_add(*count)
            .ok_or_else(|| invalid("index fanout overflow"))?;
        *count = cumulative;
    }

    let mut encoded_offsets = Vec::with_capacity(record_offsets.len());
    let mut escapes = Vec::new();
    for offset in record_offsets {
        if *offset < LARGE_OFFSET_FLAG as u64 {
            encoded_offsets.push(*offset as u32);
        } else {
            let escape = u32::try_from(escapes.len())
                .map_err(|_| invalid("large-offset escape table overflow"))?;
            if escape >= LARGE_OFFSET_FLAG {
                return Err(invalid("large-offset escape ordinal overflow"));
            }
            encoded_offsets.push(LARGE_OFFSET_FLAG | escape);
            escapes.push(*offset);
        }
    }
    let object_count =
        u32::try_from(hashes.len()).map_err(|_| invalid("index object count overflow"))?;
    let escape_count =
        u32::try_from(escapes.len()).map_err(|_| invalid("index escape count overflow"))?;
    let mut out = Vec::new();
    out.extend_from_slice(INDEX_MAGIC);
    out.extend_from_slice(&object_count.to_le_bytes());
    out.extend_from_slice(&escape_count.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    for count in fanout {
        out.extend_from_slice(&count.to_le_bytes());
    }
    for (hash, ordinal) in by_hash {
        out.extend_from_slice(&hash);
        out.extend_from_slice(
            &u32::try_from(ordinal)
                .map_err(|_| invalid("index ordinal overflow"))?
                .to_le_bytes(),
        );
    }
    for offset in encoded_offsets {
        out.extend_from_slice(&offset.to_le_bytes());
    }
    for offset in escapes {
        out.extend_from_slice(&offset.to_le_bytes());
    }
    let checksum = blake3::hash(&out);
    out.extend_from_slice(checksum.as_bytes());
    Ok(out)
}

fn encode_checksum_trailer(file: &mut std::fs::File, len: u64) -> Result<Vec<u8>> {
    file.seek(SeekFrom::Start(0))?;
    let mut remaining = len;
    let mut hashes = Vec::new();
    let mut buffer = vec![0u8; CHECKSUM_CHUNK_BYTES];
    while remaining > 0 {
        let wanted = usize::try_from(remaining.min(buffer.len() as u64))
            .map_err(|_| invalid("checksum chunk exceeds address space"))?;
        file.read_exact(&mut buffer[..wanted]).map_err(|error| {
            if error.kind() == std::io::ErrorKind::UnexpectedEof {
                invalid("pack ended before its checksum trailer")
            } else {
                HeddleError::Io(error)
            }
        })?;
        if wanted == 0 {
            return Err(invalid("pack ended before its checksum trailer"));
        }
        hashes.push(*blake3::hash(&buffer[..wanted]).as_bytes());
        remaining -= wanted as u64;
    }
    let chunk_count =
        u32::try_from(hashes.len()).map_err(|_| invalid("checksum chunk count overflow"))?;
    let mut trailer = Vec::with_capacity(
        TRAILER_HEADER_LEN + hashes.len() * super::CHECKSUM_LEN + super::CHECKSUM_LEN,
    );
    trailer.extend_from_slice(TRAILER_MAGIC);
    trailer.extend_from_slice(&(CHECKSUM_CHUNK_BYTES as u32).to_le_bytes());
    trailer.extend_from_slice(&chunk_count.to_le_bytes());
    trailer.extend_from_slice(&0u32.to_le_bytes());
    for hash in hashes {
        trailer.extend_from_slice(&hash);
    }
    let checksum = blake3::hash(&trailer);
    trailer.extend_from_slice(checksum.as_bytes());
    Ok(trailer)
}
