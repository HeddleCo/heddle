// SPDX-License-Identifier: Apache-2.0
//! Native packed-tree research prototype.
//!
//! This is deliberately a measurement format, not a production codec. It
//! nevertheless accounts every byte needed by the directly-served layout:
//! pack header, global dictionaries, object records, and hash-to-offset index.

use super::*;
use std::collections::VecDeque;

const NAME_RESTART: usize = 128;
const RECORD_BLOCK_ENTRIES: usize = 128;
const WINDOW_BUCKET_LIMIT: usize = 64;
const EXACT_CANDIDATES: usize = 16;
const PACK_HEADER_LEN: usize = 64;
const PACK_INDEX_HEADER_LEN: usize = 4 + 256 * 4;
// The hash-sorted table maps each content hash to a 32-bit pack ordinal. A
// second ordinal-sorted table maps ordinals to record offsets, so base-distance
// references and record lengths need no hidden in-memory side structure. A
// production writer adds an 8-byte large-offset escape table beyond 2 GiB;
// none of the measured packs cross that boundary.
const PACK_INDEX_ENTRY_LEN: usize = 32 + 4;
const PACK_ORDINAL_OFFSET_LEN: usize = 4;
const PACK_INDEX_TRAILER_LEN: usize = 32;
const RECORD_LEVEL: i32 = 3;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CandidateScope {
    None,
    Parent,
    Window,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DeltaKind {
    Structural,
    Byte,
}

#[derive(Clone, Copy, Debug)]
struct NativeConfig {
    name: &'static str,
    scope: CandidateScope,
    max_depth: usize,
    compressed_blocks: bool,
    delta_kind: DeltaKind,
}

const NATIVE_CONFIGS: [NativeConfig; 9] = [
    NativeConfig {
        name: "interned-anchors",
        scope: CandidateScope::None,
        max_depth: 0,
        compressed_blocks: true,
        delta_kind: DeltaKind::Structural,
    },
    NativeConfig {
        name: "parent-d1",
        scope: CandidateScope::Parent,
        max_depth: 1,
        compressed_blocks: true,
        delta_kind: DeltaKind::Structural,
    },
    NativeConfig {
        name: "parent-d8",
        scope: CandidateScope::Parent,
        max_depth: 8,
        compressed_blocks: true,
        delta_kind: DeltaKind::Structural,
    },
    NativeConfig {
        name: "window-d1",
        scope: CandidateScope::Window,
        max_depth: 1,
        compressed_blocks: true,
        delta_kind: DeltaKind::Structural,
    },
    NativeConfig {
        name: "window-d4",
        scope: CandidateScope::Window,
        max_depth: 4,
        compressed_blocks: true,
        delta_kind: DeltaKind::Structural,
    },
    NativeConfig {
        name: "window-d8",
        scope: CandidateScope::Window,
        max_depth: 8,
        compressed_blocks: true,
        delta_kind: DeltaKind::Structural,
    },
    NativeConfig {
        name: "window-d16",
        scope: CandidateScope::Window,
        max_depth: 16,
        compressed_blocks: true,
        delta_kind: DeltaKind::Structural,
    },
    NativeConfig {
        name: "window-d8-raw",
        scope: CandidateScope::Window,
        max_depth: 8,
        compressed_blocks: false,
        delta_kind: DeltaKind::Structural,
    },
    NativeConfig {
        name: "byte-window-d8",
        scope: CandidateScope::Window,
        max_depth: 8,
        compressed_blocks: true,
        delta_kind: DeltaKind::Byte,
    },
];

const WINNER_CONFIG: usize = 6;

#[derive(Debug)]
struct NativeDictionary {
    names: Vec<String>,
    name_ids: HashMap<String, u32>,
    targets: Vec<[u8; 32]>,
    target_ids: HashMap<[u8; 32], u32>,
    encoded_name_bytes: usize,
    encoded_target_bytes: usize,
}

impl NativeDictionary {
    fn build(corpus: &RealCorpus) -> Result<Self> {
        let mut names = HashSet::new();
        let mut target_counts = HashMap::<[u8; 32], usize>::new();
        for tree in &corpus.trees {
            for entry in tree.entries() {
                names.insert(entry.name().to_string());
                if let Some(target) = native_content_target(entry) {
                    *target_counts.entry(target).or_default() += 1;
                }
            }
        }
        let mut names = names.into_iter().collect::<Vec<_>>();
        names.sort();
        ensure!(
            names.len() <= u32::MAX as usize,
            "native name dictionary overflow"
        );
        let name_ids = names
            .iter()
            .enumerate()
            .map(|(index, name)| (name.clone(), index as u32))
            .collect();

        // Frequent targets receive the shortest varints. The 32-byte table is
        // fixed-width, so frequency ordering does not cost another index.
        // A one-use target is cheaper inline (32 bytes) than as a 32-byte
        // dictionary row plus a reference. Interning begins at the first
        // actual cross-tree reuse.
        let mut counted_targets = target_counts
            .into_iter()
            .filter(|(_, count)| *count >= 2)
            .collect::<Vec<_>>();
        counted_targets.sort_by(|(left_hash, left_count), (right_hash, right_count)| {
            right_count
                .cmp(left_count)
                .then_with(|| left_hash.cmp(right_hash))
        });
        ensure!(
            counted_targets.len() <= u32::MAX as usize,
            "native target dictionary overflow"
        );
        let targets = counted_targets
            .into_iter()
            .map(|(target, _)| target)
            .collect::<Vec<_>>();
        let target_ids = targets
            .iter()
            .enumerate()
            .map(|(index, target)| (*target, index as u32))
            .collect();
        let encoded_name_bytes = encoded_name_dictionary_len(&names)?;
        let encoded_target_bytes = 4usize
            .checked_add(varint_len(targets.len()))
            .and_then(|size| size.checked_add(targets.len().checked_mul(32)?))
            .context("native target dictionary size overflow")?;
        Ok(Self {
            names,
            name_ids,
            targets,
            target_ids,
            encoded_name_bytes,
            encoded_target_bytes,
        })
    }

    fn name_id(&self, name: &str) -> u32 {
        self.name_ids[name]
    }

    fn target_id(&self, target: [u8; 32]) -> Option<u32> {
        self.target_ids.get(&target).copied()
    }
}

fn native_content_target(entry: &TreeEntry) -> Option<[u8; 32]> {
    match entry.entry_type() {
        EntryType::Blob | EntryType::Tree | EntryType::Symlink => {
            entry.content_hash().map(|hash| *hash.as_bytes())
        }
        EntryType::Gitlink | EntryType::Spoollink => None,
    }
}

fn varint_len(value: usize) -> usize {
    let mut bytes = Vec::new();
    put_varint(value, &mut bytes);
    bytes.len()
}

fn encoded_name_dictionary_len(names: &[String]) -> Result<usize> {
    let blocks = names.len().div_ceil(NAME_RESTART);
    let mut size = 4usize
        .checked_add(varint_len(names.len()))
        .and_then(|value| value.checked_add(varint_len(NAME_RESTART)))
        .and_then(|value| value.checked_add(varint_len(blocks)))
        .and_then(|value| value.checked_add(blocks.checked_mul(4)?))
        .context("native name dictionary header overflow")?;
    for block in names.chunks(NAME_RESTART) {
        let mut previous = "";
        for name in block {
            let prefix = shared_prefix(previous, name);
            let suffix = name.len() - prefix;
            size = size
                .checked_add(varint_len(prefix))
                .and_then(|value| value.checked_add(varint_len(suffix)))
                .and_then(|value| value.checked_add(suffix))
                .context("native name dictionary overflow")?;
            previous = name;
        }
    }
    Ok(size)
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
        let hash = name_hash(entry.name()) ^ (entry.entry_type().to_byte() as u64);
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

#[derive(Default)]
struct CandidateWindow {
    shapes: HashMap<(usize, u64), VecDeque<usize>>,
    minima: HashMap<(usize, usize, u64), VecDeque<usize>>,
    sizes: HashMap<usize, VecDeque<usize>>,
}

fn retain_recent(bucket: &mut VecDeque<usize>, index: usize) {
    bucket.push_back(index);
    if bucket.len() > WINDOW_BUCKET_LIMIT {
        bucket.pop_front();
    }
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
    ) -> Vec<(usize, bool)> {
        let mut candidates = HashSet::new();
        if let Some(parent) = parent {
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
                let size_difference = base.entries.abs_diff(sketch.entries);
                let score = (exact_shape as u64) << 63
                    | (matching_minima as u64) << 56
                    | (u32::MAX as usize - size_difference.min(u32::MAX as usize)) as u64;
                (candidate, Some(candidate) == parent, score)
            })
            .collect::<Vec<_>>();
        ranked.sort_by_key(|(candidate, is_parent, score)| {
            (
                std::cmp::Reverse(*is_parent),
                std::cmp::Reverse(*score),
                std::cmp::Reverse(*candidate),
            )
        });
        ranked.truncate(EXACT_CANDIDATES);
        ranked
            .into_iter()
            .map(|(candidate, is_parent, _)| (candidate, is_parent))
            .collect()
    }
}

#[derive(Clone, Debug)]
struct NativePlan {
    base: Option<usize>,
    depth: usize,
    record_len: usize,
    op_count: usize,
    cross_object: bool,
}

struct VariantBuild {
    config: NativeConfig,
    plans: Vec<Option<NativePlan>>,
    record_bytes: usize,
}

struct CandidateRecord {
    base: usize,
    is_parent: bool,
    op_count: usize,
    raw: Vec<u8>,
    blocked: Vec<u8>,
    byte: Option<Vec<u8>>,
}

#[derive(Clone, Debug)]
struct BlockDescriptor {
    first_name: u32,
    raw_len: usize,
    stored_len: usize,
    payload_offset: usize,
}

fn encode_native_entry(entry: &TreeEntry, dictionary: &NativeDictionary, out: &mut Vec<u8>) {
    out.push((entry.mode().to_byte() << 3) | entry.entry_type().to_byte());
    match entry.entry_type() {
        EntryType::Blob | EntryType::Tree | EntryType::Symlink => {
            let target = native_content_target(entry).expect("native content target");
            if let Some(target_id) = dictionary.target_id(target) {
                put_varint(target_id as usize + 1, out);
            } else {
                put_varint(0, out);
                out.extend_from_slice(&target);
            }
        }
        EntryType::Gitlink => {
            let target = entry.gitlink_target().expect("native gitlink target");
            out.push(match target.format() {
                GitObjectFormat::Sha1 => 1,
                GitObjectFormat::Sha256 => 2,
            });
            out.extend_from_slice(target.as_bytes());
        }
        EntryType::Spoollink => {
            let (spool, state) = entry.spoollink_target().expect("native spoollink target");
            put_varint(spool.as_str().len(), out);
            out.extend_from_slice(spool.as_str().as_bytes());
            out.extend_from_slice(state.as_bytes());
        }
    }
}

fn finish_native_record(
    tag: u8,
    prefix: &[usize],
    raw_blocks: Vec<(u32, Vec<u8>)>,
    compressed: bool,
) -> Result<Vec<u8>> {
    let mut stored_blocks = Vec::with_capacity(raw_blocks.len());
    for (first_name, raw) in raw_blocks {
        let encoded = if compressed && !raw.is_empty() {
            let candidate = zstd::bulk::compress(&raw, RECORD_LEVEL)?;
            if candidate.len() < raw.len() {
                candidate
            } else {
                raw.clone()
            }
        } else {
            raw.clone()
        };
        stored_blocks.push((first_name, raw.len(), encoded));
    }
    let mut out = Vec::new();
    out.push(tag);
    for value in prefix {
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

fn encode_native_anchor(
    tree: &Tree,
    dictionary: &NativeDictionary,
    compressed: bool,
) -> Result<Vec<u8>> {
    let mut blocks = Vec::new();
    for entries in tree.entries().chunks(RECORD_BLOCK_ENTRIES) {
        let first_name = entries
            .first()
            .map(|entry| dictionary.name_id(entry.name()))
            .unwrap_or(0);
        let mut previous = 0u32;
        let mut raw = Vec::new();
        for (ordinal, entry) in entries.iter().enumerate() {
            let name = dictionary.name_id(entry.name());
            let encoded_name = if ordinal == 0 { name } else { name - previous };
            put_varint(encoded_name as usize, &mut raw);
            encode_native_entry(entry, dictionary, &mut raw);
            previous = name;
        }
        blocks.push((first_name, raw));
    }
    finish_native_record(b'A', &[tree.len()], blocks, compressed)
}

fn encode_native_delta(
    base_distance: usize,
    current: &Tree,
    ops: &[DeltaOp],
    dictionary: &NativeDictionary,
    compressed: bool,
) -> Result<Vec<u8>> {
    let mut blocks = Vec::new();
    for operations in ops.chunks(RECORD_BLOCK_ENTRIES) {
        let first_name = operations
            .first()
            .map(|op| dictionary.name_id(op.name()))
            .unwrap_or(0);
        let mut previous = 0u32;
        let mut raw = Vec::new();
        for (ordinal, op) in operations.iter().enumerate() {
            let name = dictionary.name_id(op.name());
            let encoded_name = if ordinal == 0 { name } else { name - previous };
            put_varint(encoded_name as usize, &mut raw);
            match op {
                DeltaOp::Remove(_) => raw.push(0),
                DeltaOp::Upsert(entry) => {
                    raw.push(1);
                    encode_native_entry(entry, dictionary, &mut raw);
                }
            }
            previous = name;
        }
        blocks.push((first_name, raw));
    }
    finish_native_record(
        b'D',
        &[base_distance, current.len(), ops.len()],
        blocks,
        compressed,
    )
}

fn flush_byte_insert(insert: &mut Vec<u8>, commands: &mut Vec<u8>) {
    if insert.is_empty() {
        return;
    }
    commands.push(0);
    put_varint(insert.len(), commands);
    commands.append(insert);
}

fn byte_match_key(bytes: &[u8], offset: usize) -> Option<u64> {
    let key: [u8; 8] = bytes.get(offset..offset + 8)?.try_into().ok()?;
    Some(u64::from_le_bytes(key))
}

fn encode_byte_delta(base_distance: usize, base: &[u8], target: &[u8]) -> Result<Vec<u8>> {
    let mut matches = HashMap::<u64, Vec<usize>>::new();
    for offset in (0..base.len().saturating_sub(7)).step_by(4) {
        let key = byte_match_key(base, offset).context("byte delta base key")?;
        let positions = matches.entry(key).or_default();
        if positions.len() < 4 {
            positions.push(offset);
        }
    }
    let mut commands = Vec::new();
    let mut insert = Vec::new();
    let mut target_offset = 0usize;
    while target_offset < target.len() {
        let mut best = None;
        if let Some(key) = byte_match_key(target, target_offset)
            && let Some(positions) = matches.get(&key)
        {
            for base_offset in positions {
                let mut length = 8usize;
                while base_offset + length < base.len()
                    && target_offset + length < target.len()
                    && base[base_offset + length] == target[target_offset + length]
                {
                    length += 1;
                }
                if best.is_none_or(|(_, best_length)| length > best_length) {
                    best = Some((*base_offset, length));
                }
            }
        }
        if let Some((base_offset, length)) = best {
            flush_byte_insert(&mut insert, &mut commands);
            commands.push(1);
            put_varint(base_offset, &mut commands);
            put_varint(length, &mut commands);
            target_offset += length;
        } else {
            insert.push(target[target_offset]);
            target_offset += 1;
        }
    }
    flush_byte_insert(&mut insert, &mut commands);
    let compressed = zstd::bulk::compress(&commands, RECORD_LEVEL)?;
    let stored = if compressed.len() < commands.len() {
        compressed
    } else {
        commands.clone()
    };
    let mut out = Vec::new();
    out.push(b'B');
    put_varint(base_distance, &mut out);
    put_varint(target.len(), &mut out);
    put_varint(commands.len(), &mut out);
    put_varint(stored.len(), &mut out);
    out.extend_from_slice(&stored);
    Ok(out)
}

fn apply_byte_delta(bytes: &[u8], base: &[u8]) -> Result<Vec<u8>> {
    ensure!(bytes.first() == Some(&b'B'), "not a native byte delta");
    let mut offset = 1usize;
    black_box(take_varint(bytes, &mut offset)?);
    let target_len = take_varint(bytes, &mut offset)?;
    let command_len = take_varint(bytes, &mut offset)?;
    let stored_len = take_varint(bytes, &mut offset)?;
    ensure!(
        offset + stored_len == bytes.len(),
        "native byte delta length"
    );
    let commands = if stored_len == command_len {
        bytes[offset..].to_vec()
    } else {
        zstd::bulk::decompress(&bytes[offset..], command_len)?
    };
    let mut command_offset = 0usize;
    let mut target = Vec::with_capacity(target_len);
    while command_offset < commands.len() {
        let opcode = commands[command_offset];
        command_offset += 1;
        match opcode {
            0 => {
                let length = take_varint(&commands, &mut command_offset)?;
                let end = command_offset
                    .checked_add(length)
                    .context("native byte insert overflow")?;
                target.extend_from_slice(
                    commands
                        .get(command_offset..end)
                        .context("truncated native byte insert")?,
                );
                command_offset = end;
            }
            1 => {
                let base_offset = take_varint(&commands, &mut command_offset)?;
                let length = take_varint(&commands, &mut command_offset)?;
                let end = base_offset
                    .checked_add(length)
                    .context("native byte copy overflow")?;
                target.extend_from_slice(
                    base.get(base_offset..end)
                        .context("native byte copy out of bounds")?,
                );
            }
            _ => bail!("invalid native byte delta opcode"),
        }
    }
    ensure!(
        target.len() == target_len,
        "native byte delta target length"
    );
    Ok(target)
}

fn decode_native_entry(
    bytes: &[u8],
    offset: &mut usize,
    name: &str,
    dictionary: &NativeDictionary,
) -> Result<TreeEntry> {
    let tag = *bytes.get(*offset).context("truncated native entry tag")?;
    *offset += 1;
    let mode = FileMode::from_byte(tag >> 3).context("invalid native entry mode")?;
    let kind = EntryType::from_byte(tag & 0x07).context("invalid native entry kind")?;
    let entry = match kind {
        EntryType::Blob | EntryType::Tree | EntryType::Symlink => {
            let encoded_target = take_varint(bytes, offset)?;
            let target = if encoded_target == 0 {
                let end = offset
                    .checked_add(32)
                    .context("native inline target overflow")?;
                let target = ContentHash::from_bytes(
                    bytes
                        .get(*offset..end)
                        .context("truncated native inline target")?
                        .try_into()?,
                );
                *offset = end;
                target
            } else {
                ContentHash::from_bytes(
                    *dictionary
                        .targets
                        .get(encoded_target - 1)
                        .context("native target id out of bounds")?,
                )
            };
            match kind {
                EntryType::Blob => TreeEntry::file(name, target, mode == FileMode::Executable)?,
                EntryType::Tree => TreeEntry::directory(name, target)?,
                EntryType::Symlink => TreeEntry::symlink(name, target)?,
                _ => unreachable!(),
            }
        }
        EntryType::Gitlink => {
            let format = match *bytes
                .get(*offset)
                .context("missing native gitlink format")?
            {
                1 => GitObjectFormat::Sha1,
                2 => GitObjectFormat::Sha256,
                _ => bail!("invalid native gitlink format"),
            };
            *offset += 1;
            let target_len = match format {
                GitObjectFormat::Sha1 => 20,
                GitObjectFormat::Sha256 => 32,
            };
            let end = offset
                .checked_add(target_len)
                .context("native gitlink overflow")?;
            let target = GitObjectId::from_raw(
                format,
                bytes
                    .get(*offset..end)
                    .context("truncated native gitlink")?,
            )?;
            *offset = end;
            TreeEntry::gitlink(name, target)?
        }
        EntryType::Spoollink => {
            let spool_len = take_varint(bytes, offset)?;
            let spool_end = offset
                .checked_add(spool_len)
                .context("native spoollink overflow")?;
            let spool = std::str::from_utf8(
                bytes
                    .get(*offset..spool_end)
                    .context("truncated native spool id")?,
            )?;
            *offset = spool_end;
            let state_end = offset.checked_add(32).context("native state overflow")?;
            let state = StateId::from_bytes(
                bytes
                    .get(*offset..state_end)
                    .context("truncated native spool state")?
                    .try_into()?,
            );
            *offset = state_end;
            TreeEntry::spoollink(name, SpoolId::parse(spool)?, state)?
        }
    };
    ensure!(entry.mode() == mode, "native entry mode mismatch");
    Ok(entry)
}

fn parse_record_header(bytes: &[u8]) -> Result<(u8, Vec<usize>, Vec<BlockDescriptor>, usize)> {
    let tag = *bytes.first().context("empty native record")?;
    ensure!(tag == b'A' || tag == b'D', "unknown native record tag");
    let mut offset = 1usize;
    let prefix_fields = if tag == b'A' { 1 } else { 3 };
    let mut prefix = Vec::with_capacity(prefix_fields);
    for _ in 0..prefix_fields {
        prefix.push(take_varint(bytes, &mut offset)?);
    }
    let block_count = take_varint(bytes, &mut offset)?;
    let mut blocks = Vec::with_capacity(block_count);
    for _ in 0..block_count {
        let first_name = u32::try_from(take_varint(bytes, &mut offset)?)
            .context("native first name id overflow")?;
        let raw_len = take_varint(bytes, &mut offset)?;
        let stored_len = take_varint(bytes, &mut offset)?;
        blocks.push(BlockDescriptor {
            first_name,
            raw_len,
            stored_len,
            payload_offset: 0,
        });
    }
    let header_len = offset;
    for block in &mut blocks {
        block.payload_offset = offset;
        offset = offset
            .checked_add(block.stored_len)
            .context("native record length overflow")?;
    }
    ensure!(offset == bytes.len(), "native record length mismatch");
    Ok((tag, prefix, blocks, header_len))
}

fn decode_native_block(bytes: &[u8], block: &BlockDescriptor) -> Result<Vec<u8>> {
    let stored = bytes
        .get(block.payload_offset..block.payload_offset + block.stored_len)
        .context("truncated native record block")?;
    if block.stored_len == block.raw_len {
        Ok(stored.to_vec())
    } else {
        Ok(zstd::bulk::decompress(stored, block.raw_len)?)
    }
}

fn decode_anchor_record(bytes: &[u8], dictionary: &NativeDictionary) -> Result<Tree> {
    let (tag, prefix, blocks, _) = parse_record_header(bytes)?;
    ensure!(tag == b'A', "native anchor expected");
    let expected_entries = prefix[0];
    let mut entries = Vec::with_capacity(expected_entries);
    for block in &blocks {
        let raw = decode_native_block(bytes, block)?;
        let mut offset = 0usize;
        let mut name_id = 0u32;
        let mut ordinal = 0usize;
        while offset < raw.len() {
            let encoded_name = u32::try_from(take_varint(&raw, &mut offset)?)?;
            name_id = if ordinal == 0 {
                encoded_name
            } else {
                name_id
                    .checked_add(encoded_name)
                    .context("native name id overflow")?
            };
            let name = dictionary
                .names
                .get(name_id as usize)
                .context("native name id out of bounds")?;
            entries.push(decode_native_entry(&raw, &mut offset, name, dictionary)?);
            ordinal += 1;
        }
    }
    ensure!(
        entries.len() == expected_entries,
        "native anchor entry mismatch"
    );
    Ok(Tree::try_from_decoded_entries(entries)?)
}

fn decode_delta_record(bytes: &[u8], base: &Tree, dictionary: &NativeDictionary) -> Result<Tree> {
    let (tag, prefix, blocks, _) = parse_record_header(bytes)?;
    ensure!(tag == b'D', "native delta expected");
    let result_entries = prefix[1];
    let expected_ops = prefix[2];
    let mut ops = Vec::with_capacity(expected_ops);
    for block in &blocks {
        let raw = decode_native_block(bytes, block)?;
        let mut offset = 0usize;
        let mut name_id = 0u32;
        let mut ordinal = 0usize;
        while offset < raw.len() {
            let encoded_name = u32::try_from(take_varint(&raw, &mut offset)?)?;
            name_id = if ordinal == 0 {
                encoded_name
            } else {
                name_id
                    .checked_add(encoded_name)
                    .context("native delta name id overflow")?
            };
            let name = dictionary
                .names
                .get(name_id as usize)
                .context("native delta name id out of bounds")?;
            let opcode = *raw.get(offset).context("truncated native delta opcode")?;
            offset += 1;
            ops.push(match opcode {
                0 => DeltaOp::Remove(name.clone()),
                1 => DeltaOp::Upsert(decode_native_entry(&raw, &mut offset, name, dictionary)?),
                _ => bail!("invalid native delta opcode"),
            });
            ordinal += 1;
        }
    }
    ensure!(ops.len() == expected_ops, "native delta op count mismatch");
    let tree = apply_delta(base, &ops)?;
    ensure!(
        tree.len() == result_entries,
        "native delta result count mismatch"
    );
    Ok(tree)
}

fn record_base_distance(bytes: &[u8]) -> Result<Option<usize>> {
    match bytes.first() {
        Some(b'A') => Ok(None),
        Some(b'D') => {
            let mut offset = 1usize;
            Ok(Some(take_varint(bytes, &mut offset)?))
        }
        _ => bail!("invalid native record"),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LookupState {
    Missing,
    Removed,
    Found,
}

fn lookup_record(
    bytes: &[u8],
    dictionary: &NativeDictionary,
    wanted_name: u32,
) -> Result<(LookupState, Option<TreeEntry>, usize)> {
    let (tag, _, blocks, header_len) = parse_record_header(bytes)?;
    let Some(block_index) = blocks
        .partition_point(|block| block.first_name <= wanted_name)
        .checked_sub(1)
    else {
        return Ok((LookupState::Missing, None, header_len));
    };
    let block = &blocks[block_index];
    let raw = decode_native_block(bytes, block)?;
    let mut offset = 0usize;
    let mut name_id = 0u32;
    let mut ordinal = 0usize;
    while offset < raw.len() {
        let encoded_name = u32::try_from(take_varint(&raw, &mut offset)?)?;
        name_id = if ordinal == 0 {
            encoded_name
        } else {
            name_id
                .checked_add(encoded_name)
                .context("native lookup name overflow")?
        };
        let name = dictionary
            .names
            .get(name_id as usize)
            .context("native lookup name id out of bounds")?;
        if tag == b'D' {
            let opcode = *raw.get(offset).context("truncated native lookup opcode")?;
            offset += 1;
            match opcode {
                0 => {
                    if name_id == wanted_name {
                        return Ok((LookupState::Removed, None, header_len + block.stored_len));
                    }
                }
                1 => {
                    let entry = decode_native_entry(&raw, &mut offset, name, dictionary)?;
                    if name_id == wanted_name {
                        return Ok((
                            LookupState::Found,
                            Some(entry),
                            header_len + block.stored_len,
                        ));
                    }
                }
                _ => bail!("invalid native lookup opcode"),
            }
        } else {
            let entry = decode_native_entry(&raw, &mut offset, name, dictionary)?;
            if name_id == wanted_name {
                return Ok((
                    LookupState::Found,
                    Some(entry),
                    header_len + block.stored_len,
                ));
            }
        }
        if name_id > wanted_name {
            break;
        }
        ordinal += 1;
    }
    Ok((LookupState::Missing, None, header_len + block.stored_len))
}

struct NativePackedCorpus {
    dictionary: NativeDictionary,
    record_bytes: Vec<u8>,
    record_offsets: Vec<(usize, usize)>,
    object_index: Vec<([u8; 32], usize)>,
}

impl NativePackedCorpus {
    fn record(&self, ordinal: usize) -> Result<&[u8]> {
        let (offset, length) = *self
            .record_offsets
            .get(ordinal)
            .context("native record ordinal out of bounds")?;
        self.record_bytes
            .get(offset..offset + length)
            .context("native record offset out of bounds")
    }

    fn object_ordinal(&self, expected: ContentHash) -> Result<usize> {
        let key = *expected.as_bytes();
        let index = self
            .object_index
            .binary_search_by_key(&key, |(hash, _)| *hash)
            .map_err(|_| anyhow::anyhow!("native pack object index miss"))?;
        Ok(self.object_index[index].1)
    }

    fn resolve(&self, expected: ContentHash) -> Result<Tree> {
        let mut ordinal = self.object_ordinal(expected)?;
        let mut chain = Vec::new();
        loop {
            chain.push(ordinal);
            let record = self.record(ordinal)?;
            let Some(distance) = record_base_distance(record)? else {
                break;
            };
            ensure!(
                distance > 0 && distance <= ordinal,
                "invalid native base distance"
            );
            ordinal -= distance;
        }
        let anchor = chain.pop().context("empty native chain")?;
        let mut tree = decode_anchor_record(self.record(anchor)?, &self.dictionary)?;
        while let Some(delta) = chain.pop() {
            tree = decode_delta_record(self.record(delta)?, &tree, &self.dictionary)?;
        }
        ensure!(
            tree.hash() == expected,
            "native resolved tree hash mismatch"
        );
        Ok(tree)
    }

    fn lookup(
        &self,
        expected: ContentHash,
        name: &str,
    ) -> Result<(Option<TreeEntry>, usize, usize)> {
        let wanted_name = self.dictionary.name_id(name);
        let mut ordinal = self.object_ordinal(expected)?;
        let mut bytes_read = 0usize;
        let mut records_read = 0usize;
        loop {
            let record = self.record(ordinal)?;
            let (state, entry, read) = lookup_record(record, &self.dictionary, wanted_name)?;
            bytes_read += read;
            records_read += 1;
            match state {
                LookupState::Found => return Ok((entry, bytes_read, records_read)),
                LookupState::Removed => return Ok((None, bytes_read, records_read)),
                LookupState::Missing => {}
            }
            let Some(distance) = record_base_distance(record)? else {
                return Ok((None, bytes_read, records_read));
            };
            ensure!(
                distance > 0 && distance <= ordinal,
                "invalid native lookup base"
            );
            ordinal -= distance;
        }
    }
}

#[derive(Clone, Debug)]
pub(super) struct NativeVariantResult {
    pub name: &'static str,
    pub record_bytes: usize,
    pub total_bytes: usize,
    pub anchors: usize,
    pub deltas: usize,
    pub cross_deltas: usize,
    pub max_depth: usize,
    pub p50_depth: usize,
    pub p95_depth: usize,
    pub depth_histogram: Vec<usize>,
    pub chain_record_bytes: Vec<usize>,
    pub total_chain_ops: usize,
}

#[derive(Clone, Debug)]
pub(super) struct NativeReadResult {
    pub samples: usize,
    pub mean_chain_bytes: f64,
    pub p95_chain_bytes: usize,
    pub mean_lookup_bytes: f64,
    pub mean_lookup_records: f64,
    pub mean_chain_ops: f64,
    pub resolve_median_ns: f64,
    pub lookup_median_ns: f64,
}

#[derive(Clone, Debug)]
pub(super) struct NativePackResult {
    pub variants: Vec<NativeVariantResult>,
    pub names: usize,
    pub targets: usize,
    pub name_dictionary_bytes: usize,
    pub target_dictionary_bytes: usize,
    pub tree_index_bytes: usize,
    pub build_ms: f64,
    pub exploration_build_ms: f64,
    pub byte_delta_build_ms: f64,
    pub candidate_pairs: usize,
    pub read: NativeReadResult,
}

fn pack_index_bytes(trees: usize) -> Result<usize> {
    PACK_INDEX_HEADER_LEN
        .checked_add(
            trees
                .checked_mul(PACK_INDEX_ENTRY_LEN)
                .context("native pack index entries overflow")?,
        )
        .and_then(|size| {
            size.checked_add(trees.checked_add(1)?.checked_mul(PACK_ORDINAL_OFFSET_LEN)?)
        })
        .and_then(|size| size.checked_add(PACK_INDEX_TRAILER_LEN))
        .context("native pack index overflow")
}

fn chain_record_bytes(plans: &[NativePlan], start: usize) -> usize {
    let mut total = 0usize;
    let mut cursor = start;
    loop {
        let plan = &plans[cursor];
        total += plan.record_len;
        let Some(base) = plan.base else {
            break;
        };
        cursor = base;
    }
    total
}

fn chain_ops(plans: &[NativePlan], start: usize) -> usize {
    let mut total = 0usize;
    let mut cursor = start;
    loop {
        let plan = &plans[cursor];
        total += plan.op_count;
        let Some(base) = plan.base else {
            break;
        };
        cursor = base;
    }
    total
}

fn measure_reads(
    corpus: &RealCorpus,
    pack: &NativePackedCorpus,
    plans: &[NativePlan],
) -> Result<NativeReadResult> {
    let candidates = corpus
        .trees
        .iter()
        .enumerate()
        .filter(|(_, tree)| !tree.is_empty())
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    let samples = sample_evenly(&candidates, 16);
    ensure!(!samples.is_empty(), "native read sample is empty");
    for index in &samples {
        let resolved = pack.resolve(corpus.trees[*index].hash())?;
        ensure!(
            resolved == corpus.trees[*index],
            "native pack roundtrip mismatch"
        );
        let wanted = &corpus.trees[*index].entries()[corpus.trees[*index].len() / 2];
        let (found, _, _) = pack.lookup(corpus.trees[*index].hash(), wanted.name())?;
        ensure!(
            found.as_ref() == Some(wanted),
            "native pack lookup mismatch"
        );
    }

    let mut resolve_index = 0usize;
    let resolve_timing = measure(|| {
        let index = samples[resolve_index % samples.len()];
        resolve_index += 1;
        let tree = pack
            .resolve(corpus.trees[index].hash())
            .expect("native measured resolve");
        black_box(tree.len())
    });
    let mut lookup_index = 0usize;
    let lookup_timing = measure(|| {
        let index = samples[lookup_index % samples.len()];
        lookup_index += 1;
        let wanted = &corpus.trees[index].entries()[corpus.trees[index].len() / 2];
        let (entry, _, _) = pack
            .lookup(corpus.trees[index].hash(), wanted.name())
            .expect("native measured lookup");
        black_box(entry.expect("native measured entry").name().len())
    });
    let mut chain_bytes = samples
        .iter()
        .map(|index| chain_record_bytes(plans, *index))
        .collect::<Vec<_>>();
    chain_bytes.sort_unstable();
    let mut lookup_bytes = 0usize;
    let mut lookup_records = 0usize;
    let mut applied_ops = 0usize;
    for index in &samples {
        let wanted = &corpus.trees[*index].entries()[corpus.trees[*index].len() / 2];
        let (_, bytes, records) = pack.lookup(corpus.trees[*index].hash(), wanted.name())?;
        lookup_bytes += bytes;
        lookup_records += records;
        applied_ops += chain_ops(plans, *index);
    }
    Ok(NativeReadResult {
        samples: samples.len(),
        mean_chain_bytes: chain_bytes.iter().sum::<usize>() as f64 / samples.len() as f64,
        p95_chain_bytes: percentile(&chain_bytes, 95, 100),
        mean_lookup_bytes: lookup_bytes as f64 / samples.len() as f64,
        mean_lookup_records: lookup_records as f64 / samples.len() as f64,
        mean_chain_ops: applied_ops as f64 / samples.len() as f64,
        resolve_median_ns: resolve_timing.median_ns,
        lookup_median_ns: lookup_timing.median_ns,
    })
}

pub(super) fn build_native_pack(corpus: &RealCorpus) -> Result<NativePackResult> {
    let started = Instant::now();
    let dictionary = NativeDictionary::build(corpus)?;
    let sketches = corpus.trees.iter().map(tree_sketch).collect::<Vec<_>>();
    let indexes = corpus
        .tree_ids
        .iter()
        .enumerate()
        .map(|(index, oid)| (oid.as_str(), index))
        .collect::<HashMap<_, _>>();
    let parent_indexes = corpus
        .tree_ids
        .iter()
        .map(|oid| {
            corpus
                .parent_tree_ids
                .get(oid)
                .and_then(|parent| indexes.get(parent.as_str()).copied())
        })
        .collect::<Vec<_>>();
    let mut variants = NATIVE_CONFIGS
        .iter()
        .map(|config| VariantBuild {
            config: *config,
            plans: vec![None; corpus.trees.len()],
            record_bytes: 0,
        })
        .collect::<Vec<_>>();
    let mut window = CandidateWindow::default();
    let mut order = Vec::with_capacity(corpus.trees.len());
    let mut ordinals = vec![usize::MAX; corpus.trees.len()];
    let mut records = vec![Vec::new(); corpus.trees.len()];
    let mut candidate_pairs = 0usize;
    let mut byte_delta_checked = false;
    let mut byte_delta_build_time = Duration::ZERO;

    for start in 0..corpus.trees.len() {
        if variants[0].plans[start].is_some() {
            continue;
        }
        let mut lineage = Vec::new();
        let mut cursor = start;
        let mut seen = HashSet::new();
        while variants[0].plans[cursor].is_none() && seen.insert(cursor) {
            lineage.push(cursor);
            let Some(parent) = parent_indexes[cursor] else {
                break;
            };
            cursor = parent;
        }
        while let Some(index) = lineage.pop() {
            let ordinal = order.len();
            ordinals[index] = ordinal;
            let anchor_raw = encode_native_anchor(&corpus.trees[index], &dictionary, false)?;
            let anchor_blocked = encode_native_anchor(&corpus.trees[index], &dictionary, true)?;
            let candidate_indexes =
                window.candidates(sketches[index], &sketches, parent_indexes[index]);
            let mut candidates = Vec::with_capacity(candidate_indexes.len());
            for (base, is_parent) in candidate_indexes {
                if ordinals[base] == usize::MAX || ordinals[base] >= ordinal {
                    continue;
                }
                let ops = tree_delta(&corpus.trees[base], &corpus.trees[index]);
                let distance = ordinal - ordinals[base];
                let raw =
                    encode_native_delta(distance, &corpus.trees[index], &ops, &dictionary, false)?;
                let blocked =
                    encode_native_delta(distance, &corpus.trees[index], &ops, &dictionary, true)?;
                candidates.push(CandidateRecord {
                    base,
                    is_parent,
                    op_count: ops.len(),
                    raw,
                    blocked,
                    byte: None,
                });
            }
            candidate_pairs += candidates.len();
            let byte_started = Instant::now();
            let mut byte_candidates = (0..candidates.len()).collect::<Vec<_>>();
            byte_candidates.sort_by_key(|candidate| candidates[*candidate].blocked.len());
            byte_candidates.truncate(4);
            if let Some(parent_candidate) =
                candidates.iter().position(|candidate| candidate.is_parent)
                && !byte_candidates.contains(&parent_candidate)
            {
                byte_candidates.push(parent_candidate);
            }
            for candidate_index in byte_candidates {
                let base = candidates[candidate_index].base;
                let base_anchor = encode_native_anchor(&corpus.trees[base], &dictionary, false)?;
                let byte = encode_byte_delta(ordinal - ordinals[base], &base_anchor, &anchor_raw)?;
                if !byte_delta_checked {
                    ensure!(
                        apply_byte_delta(&byte, &base_anchor)? == anchor_raw,
                        "native byte delta roundtrip mismatch"
                    );
                    byte_delta_checked = true;
                }
                candidates[candidate_index].byte = Some(byte);
            }
            byte_delta_build_time += byte_started.elapsed();

            for variant in &mut variants {
                let anchor_len = if variant.config.compressed_blocks {
                    anchor_blocked.len()
                } else {
                    anchor_raw.len()
                };
                let mut selected = NativePlan {
                    base: None,
                    depth: 0,
                    record_len: anchor_len,
                    op_count: 0,
                    cross_object: false,
                };
                if variant.config.scope != CandidateScope::None {
                    for candidate in &candidates {
                        if variant.config.scope == CandidateScope::Parent && !candidate.is_parent {
                            continue;
                        }
                        let Some(base_plan) = variant.plans[candidate.base].as_ref() else {
                            continue;
                        };
                        let depth = base_plan.depth + 1;
                        if depth > variant.config.max_depth {
                            continue;
                        }
                        let record_len = match variant.config.delta_kind {
                            DeltaKind::Structural if variant.config.compressed_blocks => {
                                candidate.blocked.len()
                            }
                            DeltaKind::Structural => candidate.raw.len(),
                            DeltaKind::Byte => {
                                let Some(byte) = candidate.byte.as_ref() else {
                                    continue;
                                };
                                byte.len()
                            }
                        };
                        if record_len < selected.record_len {
                            selected = NativePlan {
                                base: Some(candidate.base),
                                depth,
                                record_len,
                                op_count: candidate.op_count,
                                cross_object: !candidate.is_parent,
                            };
                        }
                    }
                }
                variant.record_bytes = variant
                    .record_bytes
                    .checked_add(selected.record_len)
                    .context("native record total overflow")?;
                variant.plans[index] = Some(selected);
            }

            let winner = variants[WINNER_CONFIG].plans[index]
                .as_ref()
                .context("native winner plan missing")?;
            records[index] = if let Some(base) = winner.base {
                let candidate = candidates
                    .iter_mut()
                    .find(|candidate| candidate.base == base)
                    .context("native winner candidate missing")?;
                std::mem::take(&mut candidate.blocked)
            } else {
                anchor_blocked
            };
            order.push(index);
            window.insert(index, sketches[index]);
        }
    }

    let tree_index_bytes = pack_index_bytes(corpus.trees.len())?;
    let common_bytes = PACK_HEADER_LEN
        .checked_add(dictionary.encoded_name_bytes)
        .and_then(|size| size.checked_add(dictionary.encoded_target_bytes))
        .and_then(|size| size.checked_add(tree_index_bytes))
        .context("native common pack bytes overflow")?;
    let mut results = Vec::with_capacity(variants.len());
    let mut winner_plans = None;
    for (variant_index, variant) in variants.into_iter().enumerate() {
        let plans = variant
            .plans
            .into_iter()
            .enumerate()
            .map(|(index, plan)| plan.with_context(|| format!("native unplanned tree {index}")))
            .collect::<Result<Vec<_>>>()?;
        let mut depths = plans.iter().map(|plan| plan.depth).collect::<Vec<_>>();
        depths.sort_unstable();
        let anchors = plans.iter().filter(|plan| plan.base.is_none()).count();
        let cross_deltas = plans.iter().filter(|plan| plan.cross_object).count();
        let mut depth_histogram = vec![0usize; depths.last().copied().unwrap_or(0) + 1];
        for plan in &plans {
            depth_histogram[plan.depth] += 1;
        }
        let mut chain_record_bytes = (0..plans.len())
            .map(|index| chain_record_bytes(&plans, index))
            .collect::<Vec<_>>();
        chain_record_bytes.sort_unstable();
        let total_chain_ops = (0..plans.len()).map(|index| chain_ops(&plans, index)).sum();
        results.push(NativeVariantResult {
            name: variant.config.name,
            record_bytes: variant.record_bytes,
            total_bytes: common_bytes
                .checked_add(variant.record_bytes)
                .context("native total pack bytes overflow")?,
            anchors,
            deltas: plans.len() - anchors,
            cross_deltas,
            max_depth: depths.last().copied().unwrap_or(0),
            p50_depth: percentile(&depths, 50, 100),
            p95_depth: percentile(&depths, 95, 100),
            depth_histogram,
            chain_record_bytes,
            total_chain_ops,
        });
        if variant_index == WINNER_CONFIG {
            winner_plans = Some(plans);
        }
    }
    let winner_plans = winner_plans.context("native winner plans absent")?;
    let mut record_bytes = Vec::with_capacity(results[WINNER_CONFIG].record_bytes);
    let mut record_offsets = Vec::with_capacity(order.len());
    for source_index in &order {
        let offset = record_bytes.len();
        record_bytes.extend_from_slice(&records[*source_index]);
        record_offsets.push((offset, records[*source_index].len()));
    }
    ensure!(
        record_bytes.len() == results[WINNER_CONFIG].record_bytes,
        "native concatenated record byte mismatch"
    );
    let mut object_index = order
        .iter()
        .enumerate()
        .map(|(ordinal, source_index)| (*corpus.trees[*source_index].hash().as_bytes(), ordinal))
        .collect::<Vec<_>>();
    object_index.sort_by_key(|(hash, _)| *hash);
    let pack = NativePackedCorpus {
        dictionary,
        record_bytes,
        record_offsets,
        object_index,
    };
    let exploration_build_time = started.elapsed();
    let build_ms = exploration_build_time
        .saturating_sub(byte_delta_build_time)
        .as_secs_f64()
        * 1_000.0;
    let read = measure_reads(corpus, &pack, &winner_plans)?;
    Ok(NativePackResult {
        variants: results,
        names: pack.dictionary.names.len(),
        targets: pack.dictionary.targets.len(),
        name_dictionary_bytes: pack.dictionary.encoded_name_bytes,
        target_dictionary_bytes: pack.dictionary.encoded_target_bytes,
        tree_index_bytes,
        build_ms,
        exploration_build_ms: exploration_build_time.as_secs_f64() * 1_000.0,
        byte_delta_build_ms: byte_delta_build_time.as_secs_f64() * 1_000.0,
        candidate_pairs,
        read,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dictionary_corpus(trees: Vec<Tree>) -> RealCorpus {
        RealCorpus {
            label: "native-pack-test".into(),
            path: PathBuf::from("."),
            object_format: GitObjectFormat::Sha1,
            discovered_trees: trees.len(),
            skipped_trees: 0,
            tree_ids: (0..trees.len())
                .map(|index| format!("{index:040x}"))
                .collect(),
            trees,
            parent_tree_ids: HashMap::new(),
            git_loose_bytes: 0,
            git_packed_bytes: 0,
            git_pack_files: 0,
            git_pack_depths: Vec::new(),
            git_pack_chain_bytes: Vec::new(),
        }
    }

    #[test]
    fn concatenated_pack_resolves_and_seeks_through_hash_index() {
        let anchor = fixture(32);
        let mut current = anchor.clone();
        let changed_name = anchor.entries()[17].name().to_string();
        current.insert(
            TreeEntry::file(changed_name.clone(), content_hash(17, 0xbeef), false)
                .expect("changed native test entry"),
        );
        let corpus = dictionary_corpus(vec![anchor.clone(), current.clone()]);
        let dictionary = NativeDictionary::build(&corpus).expect("native test dictionary");
        let anchor_record =
            encode_native_anchor(&anchor, &dictionary, true).expect("native anchor record");
        let delta_record = encode_native_delta(
            1,
            &current,
            &tree_delta(&anchor, &current),
            &dictionary,
            true,
        )
        .expect("native delta record");
        let mut record_bytes = anchor_record.clone();
        record_bytes.extend_from_slice(&delta_record);
        let mut object_index = vec![
            (*anchor.hash().as_bytes(), 0),
            (*current.hash().as_bytes(), 1),
        ];
        object_index.sort_by_key(|(hash, _)| *hash);
        let pack = NativePackedCorpus {
            dictionary,
            record_bytes,
            record_offsets: vec![
                (0, anchor_record.len()),
                (anchor_record.len(), delta_record.len()),
            ],
            object_index,
        };

        assert_eq!(
            pack.resolve(current.hash())
                .expect("resolve native test tree"),
            current
        );
        let expected = current
            .entries()
            .iter()
            .find(|entry| entry.name() == changed_name)
            .expect("changed entry");
        let (found, bytes_read, records_read) = pack
            .lookup(current.hash(), &changed_name)
            .expect("seek native test entry");
        assert_eq!(found.as_ref(), Some(expected));
        assert!(bytes_read > 0);
        assert_eq!(records_read, 1);
    }

    #[test]
    fn byte_delta_roundtrips_copy_and_insert_commands() {
        let base = b"header:alpha-alpha-alpha;body:0123456789;tail";
        let target = b"header:alpha-alpha-alpha;body:0123-CHANGED-6789;tail";
        let delta = encode_byte_delta(1, base, target).expect("encode native byte delta");
        assert_eq!(
            apply_byte_delta(&delta, base).expect("apply native byte delta"),
            target
        );
    }
}
