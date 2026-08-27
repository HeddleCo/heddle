// SPDX-License-Identifier: Apache-2.0
//! HTR4 block-compression measurement and format-validation harness.
//!
//! Real-repository measurements exercise the production store codec and a
//! file-backed `TreeEntryReader`. Synthetic measurements retain the original
//! tunable prototype for comparison. Each block keeps its first complete HTR4
//! frame raw as a restart anchor and compresses only the tail.

use std::{
    collections::{BTreeSet, HashMap, HashSet},
    env,
    fs::File,
    hint::black_box,
    io::{BufRead, BufReader, BufWriter, Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail, ensure};
use bytes::Bytes;
use heddle_format::compression::{
    CompressionConfig, CompressionDictionary, compress_with_dictionary,
};
use objects::object::{
    BytesTreeSource, ContentHash, EntryType, FileMode, FileTreeSource, SpoolId, StateId,
    TREE_BLOCK_ENCODING_VERSION, TREE_HEADER_LEN, Tree, TreeEntry, TreeEntryReader, TreePageLimits,
};
use sley::{ObjectFormat as GitObjectFormat, ObjectId as GitObjectId};
use tempfile::{NamedTempFile, TempDir};

const SEED: u64 = 0x6874_7234_5f62_656e;
const SIZES: [usize; 5] = [1, 10, 100, 1_000, 10_000];
const SAMPLE_COUNT: usize = 15;
const CALIBRATION_TIME: Duration = Duration::from_millis(25);
const SAMPLE_TIME: Duration = Duration::from_millis(40);
const DEFAULT_REAL_TREE_LIMIT: usize = 4_000;
const REAL_FILE_SAMPLE_LIMIT: usize = 64;
const RADICAL_ANCHOR_INTERVAL: usize = 128;
const RADICAL_MAX_OPS: usize = 512;
const RADICAL_HEADER_LEN: usize = 59;
const RADICAL_MAGIC: &[u8; 4] = b"HDC1";
const LEAN_MAGIC: &[u8; 4] = b"HLR1";

const BLOCK_MAGIC: &[u8; 4] = b"HTB1";
const BLOCK_VERSION: u8 = 1;
const BLOCK_CODEC_ZSTD: u8 = 1;
const BLOCK_HEADER_LEN: usize = 72;
const BLOCK_INDEX_LEN: usize = 24;
const BLOCK_LEVEL: i32 = 3;
const TRAINED_DICTIONARY_ID: u32 = 0x4854_4231;
const LEGACY_DICTIONARY_ID: u32 = 1;
const LEGACY_DICTIONARY: &[u8] =
    include_bytes!("../../format/src/compression/dictionaries/tree-state-v1.zdict");

#[derive(Clone, Copy)]
struct SplitMix64(u64);

impl SplitMix64 {
    fn new(seed: u64) -> Self {
        Self(seed)
    }

    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut value = self.0;
        value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        value ^ (value >> 31)
    }

    fn fill(&mut self, bytes: &mut [u8]) {
        for chunk in bytes.chunks_mut(8) {
            let random = self.next().to_le_bytes();
            chunk.copy_from_slice(&random[..chunk.len()]);
        }
    }
}

fn content_hash(index: usize, salt: u64) -> ContentHash {
    let mut random = SplitMix64::new(SEED ^ (index as u64).rotate_left(17) ^ salt);
    let mut content = [0u8; 96];
    random.fill(&mut content);
    ContentHash::compute(&content)
}

fn oid(index: usize) -> GitObjectId {
    let sha256 = (index / 20) % 2 == 1;
    let len = if sha256 { 32 } else { 20 };
    let format = if sha256 {
        GitObjectFormat::Sha256
    } else {
        GitObjectFormat::Sha1
    };
    let mut bytes = [0u8; 32];
    SplitMix64::new(SEED ^ index as u64 ^ 0x6f69_645f_7361_6c74).fill(&mut bytes[..len]);
    GitObjectId::from_raw(format, &bytes[..len]).expect("valid deterministic git oid")
}

fn entry(index: usize) -> TreeEntry {
    let stems = [
        "README",
        "src__object__tree",
        "crates__cli__status",
        "wire_protocol",
        "integration_spec",
        "config_local",
        "snapshot_manifest",
        "test_fixture",
    ];
    let mut name_rng = SplitMix64::new(SEED ^ index as u64);
    let suffix = name_rng.next() as u32;
    let residue = index % 20;
    let extension = match residue {
        2 => "dir",
        3 => "link",
        4 => "submodule",
        5 => "spool",
        _ => ["rs", "toml", "md", "json"][index % 4],
    };
    let name = format!(
        "{index:05}_{}_{suffix:08x}.{extension}",
        stems[index % stems.len()]
    );

    match residue {
        1 => TreeEntry::file(name, content_hash(index, 1), true),
        2 => TreeEntry::directory(name, content_hash(index, 2)),
        3 => TreeEntry::symlink(name, content_hash(index, 3)),
        4 => TreeEntry::gitlink(name, oid(index)),
        5 => TreeEntry::spoollink(
            name,
            SpoolId::parse(format!("bench/child-{}", index % 64)).expect("valid spool id"),
            StateId::from_bytes(*content_hash(index, 5).as_bytes()),
        ),
        _ => TreeEntry::file(name, content_hash(index, 0), false),
    }
    .expect("valid deterministic tree entry")
}

fn fixture(entries: usize) -> Tree {
    Tree::from_entries((0..entries).map(entry).collect())
}

fn training_dictionary() -> Result<Vec<u8>> {
    // A disjoint ordinal range prevents the measured objects from appearing in
    // the training set while retaining the same mix of entry shapes.
    let samples = (0..192)
        .map(|sample| {
            let start = 100_000 + sample * 64;
            let tree = Tree::from_entries((start..start + 64).map(entry).collect());
            let htr4 = tree.encode_canonical()?;
            Ok(htr4[TREE_HEADER_LEN..].to_vec())
        })
        .collect::<Result<Vec<_>, objects::object::TreeStreamError>>()?;
    zstd::dict::from_samples(&samples, 8 * 1024).context("train HTR4 block dictionary")
}

#[derive(Clone, Copy)]
struct Dictionary<'a> {
    name: &'static str,
    id: u32,
    bytes: &'a [u8],
    encoder: Option<&'a zstd::dict::EncoderDictionary<'static>>,
    decoder: Option<&'a zstd::dict::DecoderDictionary<'static>>,
}

#[derive(Clone, Copy, Debug)]
struct BlockHeader {
    block_entries: usize,
    tree_id: ContentHash,
    entry_count: usize,
    raw_payload_len: usize,
    logical_len: u64,
    block_count: usize,
    dictionary_id: u32,
}

#[derive(Clone, Copy, Debug)]
struct BlockIndex {
    first_entry: usize,
    offset: usize,
    stored_len: usize,
    raw_len: usize,
}

fn read_u16(bytes: &[u8], offset: usize) -> Result<u16> {
    Ok(u16::from_le_bytes(
        bytes
            .get(offset..offset + 2)
            .context("truncated u16")?
            .try_into()?,
    ))
}

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32> {
    Ok(u32::from_le_bytes(
        bytes
            .get(offset..offset + 4)
            .context("truncated u32")?
            .try_into()?,
    ))
}

fn read_u64(bytes: &[u8], offset: usize) -> Result<u64> {
    Ok(u64::from_le_bytes(
        bytes
            .get(offset..offset + 8)
            .context("truncated u64")?
            .try_into()?,
    ))
}

fn usize_from(value: u64, field: &str) -> Result<usize> {
    usize::try_from(value).with_context(|| format!("{field} exceeds usize"))
}

fn frame_ranges(htr4: &[u8]) -> Result<Vec<(usize, usize)>> {
    let header = objects::object::decode_header(htr4)?;
    let entry_count = usize_from(header.entry_count, "entry count")?;
    let expected_len = TREE_HEADER_LEN
        .checked_add(usize_from(header.payload_len, "payload length")?)
        .context("HTR4 length overflow")?;
    ensure!(htr4.len() == expected_len, "HTR4 length mismatch");
    let mut ranges = Vec::with_capacity(entry_count);
    let mut offset = TREE_HEADER_LEN;
    for _ in 0..entry_count {
        let frame_len = read_u32(htr4, offset)? as usize;
        let end = offset
            .checked_add(4)
            .and_then(|start| start.checked_add(frame_len))
            .context("frame end overflow")?;
        ensure!(end <= htr4.len(), "truncated HTR4 entry frame");
        ranges.push((offset, end));
        offset = end;
    }
    ensure!(offset == htr4.len(), "trailing HTR4 payload bytes");
    Ok(ranges)
}

fn encode_blocked(
    tree: &Tree,
    block_entries: usize,
    dictionary: Dictionary<'_>,
) -> Result<Vec<u8>> {
    let htr4 = tree.encode_canonical()?;
    encode_blocked_htr4(&htr4, block_entries, dictionary)
}

fn encode_blocked_htr4(
    htr4: &[u8],
    block_entries: usize,
    dictionary: Dictionary<'_>,
) -> Result<Vec<u8>> {
    ensure!(block_entries > 0 && block_entries <= u16::MAX as usize);
    let htr4_header = objects::object::decode_header(htr4)?;
    let ranges = frame_ranges(htr4)?;
    let block_count = ranges.len().div_ceil(block_entries);
    let mut compressor = if let Some(prepared) = dictionary.encoder {
        zstd::bulk::Compressor::with_prepared_dictionary(prepared)?
    } else {
        zstd::bulk::Compressor::new(BLOCK_LEVEL)?
    };
    let mut blocks = Vec::with_capacity(block_count);
    for chunk in ranges.chunks(block_entries) {
        let start = chunk.first().context("empty block")?.0;
        let anchor_end = chunk.first().context("empty block")?.1;
        let end = chunk.last().context("empty block")?.1;
        let raw = &htr4[start..end];
        let anchor = &htr4[start..anchor_end];
        let compressed_tail = compressor.compress(&htr4[anchor_end..end])?;
        blocks.push(if anchor.len() + compressed_tail.len() < raw.len() {
            let mut stored = Vec::with_capacity(anchor.len() + compressed_tail.len());
            stored.extend_from_slice(anchor);
            stored.extend_from_slice(&compressed_tail);
            stored
        } else {
            raw.to_vec()
        });
    }

    let index_bytes = block_count
        .checked_mul(BLOCK_INDEX_LEN)
        .context("block index overflow")?;
    let mut payload_offset = BLOCK_HEADER_LEN
        .checked_add(index_bytes)
        .context("block payload offset overflow")?;
    let capacity = payload_offset
        .checked_add(blocks.iter().map(Vec::len).sum::<usize>())
        .context("blocked tree length overflow")?;
    let mut out = Vec::with_capacity(capacity);
    out.extend_from_slice(BLOCK_MAGIC);
    out.push(BLOCK_VERSION);
    out.push(BLOCK_CODEC_ZSTD);
    out.extend_from_slice(&(block_entries as u16).to_le_bytes());
    out.extend_from_slice(htr4_header.tree_id.as_bytes());
    out.extend_from_slice(&htr4_header.entry_count.to_le_bytes());
    out.extend_from_slice(&htr4_header.payload_len.to_le_bytes());
    out.extend_from_slice(&htr4_header.logical_len.to_le_bytes());
    out.extend_from_slice(&(block_count as u32).to_le_bytes());
    out.extend_from_slice(&dictionary.id.to_le_bytes());
    ensure!(out.len() == BLOCK_HEADER_LEN);

    for (block, stored) in blocks.iter().enumerate() {
        let first_entry = block * block_entries;
        let chunk = &ranges[first_entry..(first_entry + block_entries).min(ranges.len())];
        let raw_len = chunk.last().context("empty indexed block")?.1 - chunk[0].0;
        out.extend_from_slice(&(first_entry as u64).to_le_bytes());
        out.extend_from_slice(&(payload_offset as u64).to_le_bytes());
        out.extend_from_slice(&(stored.len() as u32).to_le_bytes());
        out.extend_from_slice(&(raw_len as u32).to_le_bytes());
        payload_offset += stored.len();
    }
    for block in blocks {
        out.extend_from_slice(&block);
    }
    ensure!(out.len() == capacity);
    Ok(out)
}

fn encode_adaptive(
    tree: &Tree,
    block_entries: usize,
    dictionary: Dictionary<'_>,
) -> Result<Vec<u8>> {
    let htr4 = tree.encode_canonical()?;
    let blocked = encode_blocked_htr4(&htr4, block_entries, dictionary)?;
    Ok(if blocked.len() < htr4.len() {
        blocked
    } else {
        htr4
    })
}

fn parse_block_header(bytes: &[u8], dictionary: Dictionary<'_>) -> Result<BlockHeader> {
    parse_block_header_with_len(bytes, bytes.len(), dictionary)
}

fn parse_block_header_with_len(
    bytes: &[u8],
    stored_object_len: usize,
    dictionary: Dictionary<'_>,
) -> Result<BlockHeader> {
    ensure!(bytes.len() >= BLOCK_HEADER_LEN, "truncated HTB1 header");
    ensure!(&bytes[..4] == BLOCK_MAGIC, "not an HTB1 tree");
    ensure!(bytes[4] == BLOCK_VERSION, "unsupported HTB1 version");
    ensure!(bytes[5] == BLOCK_CODEC_ZSTD, "unsupported HTB1 codec");
    let block_entries = read_u16(bytes, 6)? as usize;
    ensure!(block_entries > 0, "zero entries per block");
    let mut tree_id = [0; 32];
    tree_id.copy_from_slice(&bytes[8..40]);
    let entry_count = usize_from(read_u64(bytes, 40)?, "entry count")?;
    let raw_payload_len = usize_from(read_u64(bytes, 48)?, "raw payload length")?;
    let logical_len = read_u64(bytes, 56)?;
    let block_count = read_u32(bytes, 64)? as usize;
    let dictionary_id = read_u32(bytes, 68)?;
    ensure!(dictionary_id == dictionary.id, "HTB1 dictionary mismatch");
    ensure!(block_count == entry_count.div_ceil(block_entries));
    ensure!(
        BLOCK_HEADER_LEN
            .checked_add(block_count * BLOCK_INDEX_LEN)
            .is_some_and(|index_end| index_end <= stored_object_len),
        "truncated HTB1 index"
    );
    Ok(BlockHeader {
        block_entries,
        tree_id: ContentHash::from_bytes(tree_id),
        entry_count,
        raw_payload_len,
        logical_len,
        block_count,
        dictionary_id,
    })
}

fn parse_block_index_entry(
    bytes: &[u8],
    at: usize,
    stored_object_len: usize,
    header: BlockHeader,
    block: usize,
) -> Result<BlockIndex> {
    let entry = BlockIndex {
        first_entry: usize_from(read_u64(bytes, at)?, "first entry")?,
        offset: usize_from(read_u64(bytes, at + 8)?, "block offset")?,
        stored_len: read_u32(bytes, at + 16)? as usize,
        raw_len: read_u32(bytes, at + 20)? as usize,
    };
    ensure!(entry.first_entry == block * header.block_entries);
    let index_end = BLOCK_HEADER_LEN + header.block_count * BLOCK_INDEX_LEN;
    ensure!(entry.offset >= index_end, "block overlaps index");
    ensure!(entry.stored_len > 0 && entry.raw_len > 0, "empty block");
    ensure!(
        entry
            .offset
            .checked_add(entry.stored_len)
            .is_some_and(|end| end <= stored_object_len),
        "truncated block payload"
    );
    Ok(entry)
}

fn parse_block_index(bytes: &[u8], header: BlockHeader, block: usize) -> Result<BlockIndex> {
    ensure!(block < header.block_count, "block index out of bounds");
    let at = BLOCK_HEADER_LEN + block * BLOCK_INDEX_LEN;
    parse_block_index_entry(bytes, at, bytes.len(), header, block)
}

fn decompress_block(
    bytes: &[u8],
    index: BlockIndex,
    decoder: &mut zstd::bulk::Decompressor<'_>,
) -> Result<Vec<u8>> {
    let stored = &bytes[index.offset..index.offset + index.stored_len];
    decompress_stored_block(stored, index.raw_len, decoder)
}

fn decompress_stored_block(
    stored: &[u8],
    raw_len: usize,
    decoder: &mut zstd::bulk::Decompressor<'_>,
) -> Result<Vec<u8>> {
    let raw = if stored.len() == raw_len {
        stored.to_vec()
    } else {
        let anchor_end = raw_anchor_end(stored)?;
        ensure!(anchor_end < raw_len, "compressed block has no tail");
        let mut raw = Vec::with_capacity(raw_len);
        raw.extend_from_slice(&stored[..anchor_end]);
        raw.extend_from_slice(&decoder.decompress(&stored[anchor_end..], raw_len - anchor_end)?);
        raw
    };
    ensure!(raw.len() == raw_len, "block decoded length mismatch");
    Ok(raw)
}

fn raw_anchor_end(stored: &[u8]) -> Result<usize> {
    let frame_len = read_u32(stored, 0)? as usize;
    let end = 4usize
        .checked_add(frame_len)
        .context("anchor frame length overflow")?;
    ensure!(end <= stored.len(), "truncated raw block anchor");
    Ok(end)
}

fn decode_entry_frame(frame: &[u8]) -> Result<TreeEntry> {
    ensure!(frame.len() >= 4, "truncated entry frame");
    let mode = FileMode::from_byte(frame[0]).context("invalid entry mode")?;
    let kind = EntryType::from_byte(frame[1]).context("invalid entry kind")?;
    let name_len = read_u16(frame, 2)? as usize;
    let name_end = 4usize
        .checked_add(name_len)
        .context("name length overflow")?;
    let name = std::str::from_utf8(frame.get(4..name_end).context("truncated entry name")?)?;
    let payload = frame.get(name_end..).context("truncated entry payload")?;
    let entry = match kind {
        EntryType::Blob | EntryType::Tree | EntryType::Symlink => {
            ensure!(payload.len() == 32, "invalid content hash length");
            let hash = ContentHash::from_bytes(payload.try_into()?);
            match kind {
                EntryType::Blob => TreeEntry::file(name, hash, mode == FileMode::Executable)?,
                EntryType::Tree => TreeEntry::directory(name, hash)?,
                EntryType::Symlink => TreeEntry::symlink(name, hash)?,
                _ => unreachable!(),
            }
        }
        EntryType::Gitlink => {
            let (&format, oid_bytes) = payload.split_first().context("missing gitlink format")?;
            let format = match format {
                1 => GitObjectFormat::Sha1,
                2 => GitObjectFormat::Sha256,
                _ => bail!("invalid gitlink format"),
            };
            TreeEntry::gitlink(name, GitObjectId::from_raw(format, oid_bytes)?)?
        }
        EntryType::Spoollink => {
            let spool_len = read_u16(payload, 0)? as usize;
            let spool_end = 2usize
                .checked_add(spool_len)
                .context("spool length overflow")?;
            let spool =
                std::str::from_utf8(payload.get(2..spool_end).context("truncated spool id")?)?;
            let state: [u8; 32] = payload
                .get(spool_end..)
                .context("missing spool state")?
                .try_into()
                .context("invalid spool state length")?;
            TreeEntry::spoollink(name, SpoolId::parse(spool)?, StateId::from_bytes(state))?
        }
    };
    ensure!(entry.mode() == mode, "entry kind/mode mismatch");
    Ok(entry)
}

fn decode_frames(raw: &[u8]) -> Result<(Vec<TreeEntry>, u64)> {
    let mut entries = Vec::new();
    let mut logical_len = 0u64;
    let mut offset = 0usize;
    while offset < raw.len() {
        let frame_len = read_u32(raw, offset)? as usize;
        let start = offset.checked_add(4).context("frame start overflow")?;
        let end = start.checked_add(frame_len).context("frame end overflow")?;
        let frame = raw.get(start..end).context("truncated entry frame")?;
        let entry = decode_entry_frame(frame)?;
        logical_len = logical_len
            .checked_add(semantic_encoded_len(&entry) as u64)
            .context("logical length overflow")?;
        entries.push(entry);
        offset = end;
    }
    ensure!(offset == raw.len());
    Ok((entries, logical_len))
}

fn semantic_encoded_len(entry: &TreeEntry) -> usize {
    let target_len = match entry.entry_type() {
        EntryType::Blob | EntryType::Tree | EntryType::Symlink => 32,
        EntryType::Gitlink => entry
            .gitlink_target()
            .expect("decoded gitlink has a target")
            .as_bytes()
            .len(),
        EntryType::Spoollink => {
            let (spool, state) = entry
                .spoollink_target()
                .expect("decoded spoollink has a target");
            4 + spool.as_str().len() + state.as_bytes().len()
        }
    };
    3 + entry.name().len() + target_len
}

fn put_varint(mut value: usize, out: &mut Vec<u8>) {
    while value >= 0x80 {
        out.push((value as u8) | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

fn take_varint(bytes: &[u8], offset: &mut usize) -> Result<usize> {
    let mut value = 0usize;
    for shift in (0..usize::BITS).step_by(7) {
        let byte = *bytes.get(*offset).context("truncated varint")?;
        *offset += 1;
        value |= ((byte & 0x7f) as usize)
            .checked_shl(shift)
            .context("varint overflow")?;
        if byte & 0x80 == 0 {
            return Ok(value);
        }
    }
    bail!("varint overflow")
}

fn shared_prefix(left: &str, right: &str) -> usize {
    left.as_bytes()
        .iter()
        .zip(right.as_bytes())
        .take_while(|(left, right)| left == right)
        .count()
}

fn encode_compact_entry(entry: &TreeEntry, previous_name: &str, out: &mut Vec<u8>) {
    out.push((entry.mode().to_byte() << 3) | entry.entry_type().to_byte());
    let prefix = shared_prefix(previous_name, entry.name());
    put_varint(prefix, out);
    put_varint(entry.name().len() - prefix, out);
    out.extend_from_slice(&entry.name().as_bytes()[prefix..]);
    match entry.entry_type() {
        EntryType::Blob | EntryType::Tree | EntryType::Symlink => out.extend_from_slice(
            entry
                .content_hash()
                .expect("content-addressed compact entry")
                .as_bytes(),
        ),
        EntryType::Gitlink => {
            let target = entry.gitlink_target().expect("compact gitlink target");
            out.push(match target.format() {
                GitObjectFormat::Sha1 => 1,
                GitObjectFormat::Sha256 => 2,
            });
            out.extend_from_slice(target.as_bytes());
        }
        EntryType::Spoollink => {
            let (spool, state) = entry.spoollink_target().expect("compact spoollink target");
            put_varint(spool.as_str().len(), out);
            out.extend_from_slice(spool.as_str().as_bytes());
            out.extend_from_slice(state.as_bytes());
        }
    }
}

fn decode_compact_entry(
    bytes: &[u8],
    offset: &mut usize,
    previous_name: &str,
) -> Result<TreeEntry> {
    let tag = *bytes.get(*offset).context("truncated compact entry tag")?;
    *offset += 1;
    let mode = FileMode::from_byte(tag >> 3).context("invalid compact entry mode")?;
    let kind = EntryType::from_byte(tag & 0x07).context("invalid compact entry kind")?;
    let prefix = take_varint(bytes, offset)?;
    let suffix_len = take_varint(bytes, offset)?;
    ensure!(
        prefix <= previous_name.len(),
        "compact name prefix exceeds predecessor"
    );
    let suffix_end = offset
        .checked_add(suffix_len)
        .context("compact name length overflow")?;
    let suffix = bytes
        .get(*offset..suffix_end)
        .context("truncated compact name suffix")?;
    let mut name = previous_name.as_bytes()[..prefix].to_vec();
    name.extend_from_slice(suffix);
    let name = String::from_utf8(name)?;
    *offset = suffix_end;
    let entry = match kind {
        EntryType::Blob | EntryType::Tree | EntryType::Symlink => {
            let end = offset.checked_add(32).context("compact hash overflow")?;
            let hash = ContentHash::from_bytes(
                bytes
                    .get(*offset..end)
                    .context("truncated compact content hash")?
                    .try_into()?,
            );
            *offset = end;
            match kind {
                EntryType::Blob => TreeEntry::file(name, hash, mode == FileMode::Executable)?,
                EntryType::Tree => TreeEntry::directory(name, hash)?,
                EntryType::Symlink => TreeEntry::symlink(name, hash)?,
                _ => unreachable!(),
            }
        }
        EntryType::Gitlink => {
            let format = match *bytes
                .get(*offset)
                .context("missing compact gitlink format")?
            {
                1 => GitObjectFormat::Sha1,
                2 => GitObjectFormat::Sha256,
                _ => bail!("invalid compact gitlink format"),
            };
            *offset += 1;
            let oid_len = match format {
                GitObjectFormat::Sha1 => 20,
                GitObjectFormat::Sha256 => 32,
            };
            let end = offset
                .checked_add(oid_len)
                .context("compact gitlink length overflow")?;
            let target = GitObjectId::from_raw(
                format,
                bytes
                    .get(*offset..end)
                    .context("truncated compact gitlink")?,
            )?;
            *offset = end;
            TreeEntry::gitlink(name, target)?
        }
        EntryType::Spoollink => {
            let spool_len = take_varint(bytes, offset)?;
            let spool_end = offset
                .checked_add(spool_len)
                .context("compact spool length overflow")?;
            let spool = std::str::from_utf8(
                bytes
                    .get(*offset..spool_end)
                    .context("truncated compact spool id")?,
            )?;
            *offset = spool_end;
            let state_end = offset.checked_add(32).context("compact state overflow")?;
            let state = StateId::from_bytes(
                bytes
                    .get(*offset..state_end)
                    .context("truncated compact spool state")?
                    .try_into()?,
            );
            *offset = state_end;
            TreeEntry::spoollink(name, SpoolId::parse(spool)?, state)?
        }
    };
    ensure!(entry.mode() == mode, "compact entry kind/mode mismatch");
    Ok(entry)
}

fn encode_lean(tree: &Tree) -> Vec<u8> {
    encode_lean_entries(tree.entries())
}

fn encode_lean_prefix(tree: &Tree, count: usize) -> Vec<u8> {
    encode_lean_entries(&tree.entries()[..count.min(tree.len())])
}

fn encode_lean_entries(entries: &[TreeEntry]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(LEAN_MAGIC);
    put_varint(entries.len(), &mut out);
    let mut previous = "";
    for entry in entries {
        encode_compact_entry(entry, previous, &mut out);
        previous = entry.name();
    }
    out
}

fn decode_lean(bytes: &[u8], expected: ContentHash) -> Result<Tree> {
    ensure!(bytes.starts_with(LEAN_MAGIC), "not a lean tree anchor");
    let mut offset = LEAN_MAGIC.len();
    let count = take_varint(bytes, &mut offset)?;
    let mut entries = Vec::with_capacity(count);
    let mut previous = String::new();
    for _ in 0..count {
        let entry = decode_compact_entry(bytes, &mut offset, &previous)?;
        previous = entry.name().to_string();
        entries.push(entry);
    }
    ensure!(offset == bytes.len(), "trailing lean tree bytes");
    let tree = Tree::try_from_decoded_entries(entries)?;
    ensure!(tree.hash() == expected, "lean tree hash mismatch");
    Ok(tree)
}

#[derive(Clone, Debug)]
enum DeltaOp {
    Remove(String),
    Upsert(TreeEntry),
}

impl DeltaOp {
    fn name(&self) -> &str {
        match self {
            Self::Remove(name) => name,
            Self::Upsert(entry) => entry.name(),
        }
    }
}

fn tree_delta(anchor: &Tree, current: &Tree) -> Vec<DeltaOp> {
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
                        ops.push(DeltaOp::Remove(anchor_entry.name().to_string()));
                        anchor_index += 1;
                    }
                    std::cmp::Ordering::Greater => {
                        ops.push(DeltaOp::Upsert(current_entry.clone()));
                        current_index += 1;
                    }
                    std::cmp::Ordering::Equal => {
                        if anchor_entry != current_entry {
                            ops.push(DeltaOp::Upsert(current_entry.clone()));
                        }
                        anchor_index += 1;
                        current_index += 1;
                    }
                }
            }
            (Some(anchor_entry), None) => {
                ops.push(DeltaOp::Remove(anchor_entry.name().to_string()));
                anchor_index += 1;
            }
            (None, Some(current_entry)) => {
                ops.push(DeltaOp::Upsert(current_entry.clone()));
                current_index += 1;
            }
            (None, None) => break,
        }
    }
    ops
}

fn apply_delta(anchor: &Tree, ops: &[DeltaOp]) -> Result<Tree> {
    let mut entries = Vec::with_capacity(anchor.len() + ops.len());
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
                    if let DeltaOp::Upsert(entry) = op {
                        entries.push(entry.clone());
                    }
                    op_index += 1;
                }
                std::cmp::Ordering::Equal => {
                    if let DeltaOp::Upsert(entry) = op {
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
                if let DeltaOp::Upsert(entry) = op {
                    entries.push(entry.clone());
                }
                op_index += 1;
            }
            (None, None) => break,
        }
    }
    Ok(Tree::try_from_decoded_entries(entries)?)
}

fn delta_prefix_counts(anchor: &Tree, current: &Tree, ops: &[DeltaOp], count: usize) -> (u16, u16) {
    if current.is_empty() {
        return (0, 0);
    }
    let boundary = current.entries()[count.min(current.len()) - 1].name();
    let op_count = ops.partition_point(|op| op.name() <= boundary);
    let base_count = anchor
        .entries()
        .partition_point(|entry| entry.name() <= boundary);
    (
        u16::try_from(op_count).expect("radical op bound fits u16"),
        u16::try_from(base_count).expect("prefix base count fits u16"),
    )
}

fn encode_delta(anchor_id: ContentHash, anchor: &Tree, current: &Tree, ops: &[DeltaOp]) -> Vec<u8> {
    let (first_ops, first_base) = delta_prefix_counts(anchor, current, ops, 1);
    let (hundred_ops, hundred_base) = delta_prefix_counts(anchor, current, ops, 100);
    let mut body = Vec::new();
    let mut ends = Vec::with_capacity(ops.len());
    let mut previous = "";
    for op in ops {
        match op {
            DeltaOp::Remove(name) => {
                body.push(0);
                let prefix = shared_prefix(previous, name);
                put_varint(prefix, &mut body);
                put_varint(name.len() - prefix, &mut body);
                body.extend_from_slice(&name.as_bytes()[prefix..]);
            }
            DeltaOp::Upsert(entry) => {
                body.push(1);
                encode_compact_entry(entry, previous, &mut body);
            }
        }
        previous = op.name();
        ends.push(body.len());
    }
    let end_for = |count: u16| {
        if count == 0 {
            RADICAL_HEADER_LEN
        } else {
            RADICAL_HEADER_LEN + ends[count as usize - 1]
        }
    };
    let mut out = Vec::with_capacity(RADICAL_HEADER_LEN + body.len());
    out.extend_from_slice(RADICAL_MAGIC);
    out.push(1);
    out.extend_from_slice(anchor_id.as_bytes());
    out.extend_from_slice(&(current.len() as u32).to_le_bytes());
    out.extend_from_slice(&(ops.len() as u16).to_le_bytes());
    out.extend_from_slice(&first_ops.to_le_bytes());
    out.extend_from_slice(&first_base.to_le_bytes());
    out.extend_from_slice(&(end_for(first_ops) as u32).to_le_bytes());
    out.extend_from_slice(&hundred_ops.to_le_bytes());
    out.extend_from_slice(&hundred_base.to_le_bytes());
    out.extend_from_slice(&(end_for(hundred_ops) as u32).to_le_bytes());
    debug_assert_eq!(out.len(), RADICAL_HEADER_LEN);
    out.extend_from_slice(&body);
    out
}

fn decode_delta_ops(
    bytes: &[u8],
    wanted: usize,
) -> Result<(ContentHash, usize, Vec<DeltaOp>, usize)> {
    ensure!(
        bytes.len() >= RADICAL_HEADER_LEN,
        "truncated radical delta header"
    );
    ensure!(bytes.starts_with(RADICAL_MAGIC), "not a radical tree delta");
    ensure!(bytes[4] == 1, "unsupported radical delta version");
    let anchor = ContentHash::from_bytes(bytes[5..37].try_into()?);
    let result_count = read_u32(bytes, 37)? as usize;
    let op_count = read_u16(bytes, 41)? as usize;
    ensure!(wanted <= op_count, "partial delta op count exceeds object");
    let mut offset = RADICAL_HEADER_LEN;
    let mut previous = String::new();
    let mut ops = Vec::with_capacity(wanted);
    for _ in 0..wanted {
        let opcode = *bytes.get(offset).context("truncated radical delta op")?;
        offset += 1;
        let op = match opcode {
            0 => {
                let prefix = take_varint(bytes, &mut offset)?;
                let suffix_len = take_varint(bytes, &mut offset)?;
                ensure!(
                    prefix <= previous.len(),
                    "delta name prefix exceeds predecessor"
                );
                let end = offset
                    .checked_add(suffix_len)
                    .context("delta name overflow")?;
                let mut name = previous.as_bytes()[..prefix].to_vec();
                name.extend_from_slice(bytes.get(offset..end).context("truncated delta name")?);
                offset = end;
                DeltaOp::Remove(String::from_utf8(name)?)
            }
            1 => DeltaOp::Upsert(decode_compact_entry(bytes, &mut offset, &previous)?),
            _ => bail!("invalid radical delta opcode"),
        };
        previous = op.name().to_string();
        ops.push(op);
    }
    Ok((anchor, result_count, ops, offset))
}

fn decode_delta(bytes: &[u8], anchor: &Tree, expected: ContentHash) -> Result<Tree> {
    let op_count = read_u16(bytes, 41)? as usize;
    let (anchor_id, result_count, ops, consumed) = decode_delta_ops(bytes, op_count)?;
    ensure!(consumed == bytes.len(), "trailing radical delta bytes");
    ensure!(anchor.hash() == anchor_id, "radical delta anchor mismatch");
    let tree = apply_delta(anchor, &ops)?;
    ensure!(
        tree.len() == result_count,
        "radical delta entry count mismatch"
    );
    ensure!(tree.hash() == expected, "radical delta tree hash mismatch");
    Ok(tree)
}

fn block_decoder(dictionary: Dictionary<'_>) -> Result<zstd::bulk::Decompressor<'_>> {
    if let Some(prepared) = dictionary.decoder {
        Ok(zstd::bulk::Decompressor::with_prepared_dictionary(
            prepared,
        )?)
    } else {
        Ok(zstd::bulk::Decompressor::new()?)
    }
}

fn decode_blocked(bytes: &[u8], dictionary: Dictionary<'_>) -> Result<Tree> {
    let header = parse_block_header(bytes, dictionary)?;
    let mut decoder = block_decoder(dictionary)?;
    let mut entries = Vec::with_capacity(header.entry_count);
    let mut raw_payload_len = 0usize;
    let mut logical_len = 0u64;
    let mut expected_offset = BLOCK_HEADER_LEN + header.block_count * BLOCK_INDEX_LEN;
    for block in 0..header.block_count {
        let index = parse_block_index(bytes, header, block)?;
        ensure!(
            index.offset == expected_offset,
            "non-contiguous block payload"
        );
        expected_offset += index.stored_len;
        let raw = decompress_block(bytes, index, &mut decoder)?;
        raw_payload_len += raw.len();
        let (mut decoded, block_logical_len) = decode_frames(&raw)?;
        let expected_entries = header
            .block_entries
            .min(header.entry_count - block * header.block_entries);
        ensure!(
            decoded.len() == expected_entries,
            "block entry count mismatch"
        );
        logical_len += block_logical_len;
        entries.append(&mut decoded);
    }
    ensure!(expected_offset == bytes.len(), "trailing HTB1 bytes");
    ensure!(raw_payload_len == header.raw_payload_len);
    ensure!(logical_len == header.logical_len);
    ensure!(entries.len() == header.entry_count);
    let tree = Tree::try_from_decoded_entries(entries)?;
    ensure!(tree.hash() == header.tree_id, "HTB1 tree hash mismatch");
    black_box(header.dictionary_id);
    Ok(tree)
}

fn decode_adaptive(bytes: &[u8], dictionary: Dictionary<'_>) -> Result<Tree> {
    if bytes.starts_with(BLOCK_MAGIC) {
        decode_blocked(bytes, dictionary)
    } else {
        Ok(Tree::decode_canonical(bytes)?)
    }
}

struct PartialRead {
    entries: Vec<TreeEntry>,
    bytes_read: usize,
}

fn partial_blocked(
    bytes: &[u8],
    dictionary: Dictionary<'_>,
    first: usize,
    count: usize,
) -> Result<PartialRead> {
    let header = parse_block_header(bytes, dictionary)?;
    ensure!(count > 0 && first + count <= header.entry_count);
    let first_block = first / header.block_entries;
    let last_block = (first + count - 1) / header.block_entries;
    let mut decoder = block_decoder(dictionary)?;
    let mut selected = Vec::with_capacity(count);
    let mut bytes_read = BLOCK_HEADER_LEN;
    for block in first_block..=last_block {
        let index = parse_block_index(bytes, header, block)?;
        bytes_read += BLOCK_INDEX_LEN;
        let stored = &bytes[index.offset..index.offset + index.stored_len];
        let wanted_start = first.saturating_sub(index.first_entry);
        let wanted_end = (first + count - index.first_entry)
            .min(header.block_entries)
            .min(header.entry_count - index.first_entry);
        let only_anchor = wanted_start == 0 && wanted_end == 1;
        let raw = if index.stored_len == index.raw_len {
            let (entries, consumed) = decode_frame_range(stored, wanted_start, wanted_end)?;
            bytes_read += consumed;
            selected.extend(entries);
            continue;
        } else if only_anchor {
            let anchor_end = raw_anchor_end(stored)?;
            bytes_read += anchor_end;
            let (entries, consumed) = decode_frame_range(stored, 0, 1)?;
            ensure!(consumed == anchor_end);
            selected.extend(entries);
            continue;
        } else {
            bytes_read += index.stored_len;
            decompress_block(bytes, index, &mut decoder)?
        };
        let (entries, _) = decode_frame_range(&raw, wanted_start, wanted_end)?;
        selected.extend(entries);
    }
    ensure!(selected.len() == count);
    Ok(PartialRead {
        entries: selected,
        bytes_read,
    })
}

fn decode_frame_range(
    raw: &[u8],
    wanted_start: usize,
    wanted_end: usize,
) -> Result<(Vec<TreeEntry>, usize)> {
    ensure!(wanted_start < wanted_end, "empty frame range");
    let mut entries = Vec::with_capacity(wanted_end - wanted_start);
    let mut offset = 0usize;
    for ordinal in 0..wanted_end {
        let frame_len = read_u32(raw, offset)? as usize;
        let start = offset.checked_add(4).context("frame start overflow")?;
        let end = start.checked_add(frame_len).context("frame end overflow")?;
        let frame = raw.get(start..end).context("truncated ranged frame")?;
        if ordinal >= wanted_start {
            entries.push(decode_entry_frame(frame)?);
        }
        offset = end;
    }
    Ok((entries, offset))
}

fn partial_htr4(bytes: &Bytes, tree_id: ContentHash, count: usize) -> PartialRead {
    let mut reader = TreeEntryReader::open(
        BytesTreeSource::verified_placement(bytes.clone()),
        tree_id,
        None,
    )
    .expect("open HTR4 reader");
    let page = reader
        .next_page(TreePageLimits::new(count, usize::MAX).expect("page limits"))
        .expect("decode partial HTR4 page")
        .expect("nonempty page");
    let bytes_read = reader.bytes_read() as usize;
    PartialRead {
        entries: page.entries,
        bytes_read,
    }
}

fn partial_adaptive(
    bytes: &Bytes,
    dictionary: Dictionary<'_>,
    tree_id: ContentHash,
    count: usize,
) -> PartialRead {
    if bytes.starts_with(BLOCK_MAGIC) {
        partial_blocked(bytes, dictionary, 0, count).expect("partial HTB1")
    } else {
        partial_htr4(bytes, tree_id, count)
    }
}

fn partial_whole_zstd(compressed: &[u8], tree_id: ContentHash, count: usize) -> PartialRead {
    let htr4 = Bytes::from(zstd::decode_all(compressed).expect("decompress whole HTR4"));
    let mut read = partial_htr4(&htr4, tree_id, count);
    read.bytes_read = compressed.len();
    read
}

fn partial_production_file(path: &Path, tree_id: ContentHash, count: usize) -> Result<PartialRead> {
    let file = File::open(path)?;
    let len = file.metadata()?.len();
    let mut reader =
        TreeEntryReader::open(FileTreeSource::sequential_verify(file, len), tree_id, None)?;
    let page = reader
        .next_page(TreePageLimits::new(count, usize::MAX)?)?
        .context("nonempty production tree page")?;
    Ok(PartialRead {
        entries: page.entries,
        bytes_read: reader.bytes_read() as usize,
    })
}

fn partial_lean_bytes(bytes: &[u8], count: usize) -> Result<PartialRead> {
    ensure!(bytes.starts_with(LEAN_MAGIC), "not a lean tree anchor");
    let mut offset = LEAN_MAGIC.len();
    let entries = take_varint(bytes, &mut offset)?;
    let wanted = count.min(entries);
    let mut decoded = Vec::with_capacity(wanted);
    let mut previous = String::new();
    for _ in 0..wanted {
        let entry = decode_compact_entry(bytes, &mut offset, &previous)?;
        previous = entry.name().to_string();
        decoded.push(entry);
    }
    Ok(PartialRead {
        entries: decoded,
        bytes_read: offset,
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RadicalMode {
    LeanAnchor,
    Htr4Anchor,
    Delta,
}

#[derive(Clone, Debug)]
struct RadicalPlan {
    mode: RadicalMode,
    anchor: usize,
    epoch_depth: usize,
    settled_len: usize,
    hot_len: usize,
    op_count: usize,
}

struct RadicalCorpus {
    plans: Vec<RadicalPlan>,
    lean_total: usize,
    settled_total: usize,
    hot_total: usize,
    bases: usize,
    deltas: usize,
    parent_coverage: usize,
    max_ops: usize,
}

fn build_radical_corpus(corpus: &RealCorpus) -> Result<RadicalCorpus> {
    let config = CompressionConfig::default();
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
    let parent_coverage = parent_indexes
        .iter()
        .filter(|parent| parent.is_some())
        .count();
    let tree_hashes = corpus.trees.iter().map(Tree::hash).collect::<Vec<_>>();
    let mut lean_sizes = Vec::with_capacity(corpus.trees.len());
    let mut adaptive_sizes = Vec::with_capacity(corpus.trees.len());
    let mut lean_total = 0usize;
    for tree in &corpus.trees {
        let lean = encode_lean(tree);
        let (_, adaptive) = objects::store::codec::encode_tree(tree, &config)?;
        lean_total += lean.len();
        lean_sizes.push(lean.len());
        adaptive_sizes.push(adaptive.len());
    }
    let mut plans: Vec<Option<RadicalPlan>> = vec![None; corpus.trees.len()];
    for start in 0..corpus.trees.len() {
        if plans[start].is_some() {
            continue;
        }
        let mut chain = Vec::new();
        let mut cursor = start;
        let mut seen = HashSet::new();
        while plans[cursor].is_none() && seen.insert(cursor) {
            chain.push(cursor);
            let Some(parent) = parent_indexes[cursor] else {
                break;
            };
            cursor = parent;
        }
        while let Some(index) = chain.pop() {
            let lean_len = lean_sizes[index];
            let htr4_with_porch = adaptive_sizes[index]
                .checked_add(encode_lean_prefix(&corpus.trees[index], 100).len())
                .context("radical anchor size overflow")?;
            let (base_mode, base_len) = if lean_len <= htr4_with_porch {
                (RadicalMode::LeanAnchor, lean_len)
            } else {
                (RadicalMode::Htr4Anchor, htr4_with_porch)
            };
            let mut plan = RadicalPlan {
                mode: base_mode,
                anchor: index,
                epoch_depth: 0,
                settled_len: base_len,
                hot_len: lean_len,
                op_count: 0,
            };
            if let Some(parent) = parent_indexes[index]
                && let Some(parent_plan) = plans[parent].as_ref()
                && parent_plan.epoch_depth + 1 < RADICAL_ANCHOR_INTERVAL
            {
                let anchor = parent_plan.anchor;
                let ops = tree_delta(&corpus.trees[anchor], &corpus.trees[index]);
                if ops.len() <= RADICAL_MAX_OPS {
                    let encoded = encode_delta(
                        tree_hashes[anchor],
                        &corpus.trees[anchor],
                        &corpus.trees[index],
                        &ops,
                    );
                    let prefix_is_bounded =
                        read_u16(&encoded, 45)? <= 1 && read_u16(&encoded, 53)? <= 100;
                    if prefix_is_bounded && encoded.len() < base_len {
                        plan = RadicalPlan {
                            mode: RadicalMode::Delta,
                            anchor,
                            epoch_depth: parent_plan.epoch_depth + 1,
                            settled_len: encoded.len(),
                            hot_len: encoded.len(),
                            op_count: ops.len(),
                        };
                    }
                }
            }
            plans[index] = Some(plan);
        }
    }
    let plans = plans
        .into_iter()
        .enumerate()
        .map(|(index, plan)| plan.with_context(|| format!("unplanned tree {index}")))
        .collect::<Result<Vec<_>>>()?;
    let settled_total = plans.iter().map(|plan| plan.settled_len).sum();
    let hot_total = plans.iter().map(|plan| plan.hot_len).sum();
    let bases = plans
        .iter()
        .filter(|plan| plan.mode != RadicalMode::Delta)
        .count();
    let deltas = plans.len() - bases;
    let max_ops = plans.iter().map(|plan| plan.op_count).max().unwrap_or(0);
    Ok(RadicalCorpus {
        plans,
        lean_total,
        settled_total,
        hot_total,
        bases,
        deltas,
        parent_coverage,
        max_ops,
    })
}

struct RadicalFileFixture {
    raw: NamedTempFile,
    delta: NamedTempFile,
    anchor: NamedTempFile,
    anchor_lean: bool,
    anchor_id: ContentHash,
    expected: ContentHash,
    entries: usize,
}

fn radical_file_fixture(
    corpus: &RealCorpus,
    radical: &RadicalCorpus,
    index: usize,
) -> Result<RadicalFileFixture> {
    let plan = &radical.plans[index];
    ensure!(
        plan.mode == RadicalMode::Delta,
        "radical fixture must be a delta"
    );
    let anchor_plan = &radical.plans[plan.anchor];
    ensure!(
        anchor_plan.mode != RadicalMode::Delta,
        "radical anchor must be materialized"
    );
    let anchor_tree = &corpus.trees[plan.anchor];
    let current = &corpus.trees[index];
    let ops = tree_delta(anchor_tree, current);
    let delta = encode_delta(anchor_tree.hash(), anchor_tree, current, &ops);
    let (anchor_lean, anchor) = if anchor_plan.mode == RadicalMode::LeanAnchor {
        (true, encode_lean(anchor_tree))
    } else {
        (true, encode_lean_prefix(anchor_tree, 100))
    };
    ensure!(decode_delta(&delta, anchor_tree, current.hash())? == *current);
    Ok(RadicalFileFixture {
        raw: write_temp(&current.encode_canonical()?)?,
        delta: write_temp(&delta)?,
        anchor: write_temp(&anchor)?,
        anchor_lean,
        anchor_id: anchor_tree.hash(),
        expected: current.hash(),
        entries: current.len(),
    })
}

fn measure_radical_partial(
    fixtures: &[&RadicalFileFixture],
    count: usize,
) -> Result<(Timing, Timing, f64, f64)> {
    ensure!(
        !fixtures.is_empty(),
        "no radical fixtures for {count}-entry read"
    );
    let mut raw_index = 0usize;
    let raw_timing = measure(|| {
        let fixture = &fixtures[raw_index % fixtures.len()];
        raw_index += 1;
        let read = partial_production_file(fixture.raw.path(), fixture.expected, count)
            .expect("file-backed raw radical comparison");
        black_box(read.entries[count - 1].name().len())
    });
    let mut radical_index = 0usize;
    let radical_timing = measure(|| {
        let fixture = &fixtures[radical_index % fixtures.len()];
        radical_index += 1;
        let read = partial_radical_file(fixture, count).expect("file-backed radical partial");
        black_box(read.entries[count - 1].name().len())
    });
    let mut raw_bytes = 0usize;
    let mut radical_bytes = 0usize;
    for fixture in fixtures {
        let raw = partial_production_file(fixture.raw.path(), fixture.expected, count)?;
        let radical = partial_radical_file(fixture, count)?;
        ensure!(
            raw.entries == radical.entries,
            "radical file partial mismatch"
        );
        raw_bytes += raw.bytes_read;
        radical_bytes += radical.bytes_read;
    }
    Ok((
        raw_timing,
        radical_timing,
        raw_bytes as f64 / fixtures.len() as f64,
        radical_bytes as f64 / fixtures.len() as f64,
    ))
}

fn partial_radical_file(fixture: &RadicalFileFixture, count: usize) -> Result<PartialRead> {
    let mut delta_file = File::open(fixture.delta.path())?;
    let mut header = [0u8; RADICAL_HEADER_LEN];
    delta_file.read_exact(&mut header)?;
    let (op_count, base_count, end) = if count == 1 {
        (
            read_u16(&header, 43)? as usize,
            read_u16(&header, 45)? as usize,
            read_u32(&header, 47)? as usize,
        )
    } else {
        (
            read_u16(&header, 51)? as usize,
            read_u16(&header, 53)? as usize,
            read_u32(&header, 55)? as usize,
        )
    };
    ensure!(end >= RADICAL_HEADER_LEN, "invalid radical partial end");
    let mut prefix = Vec::with_capacity(end);
    prefix.extend_from_slice(&header);
    prefix.resize(end, 0);
    delta_file.read_exact(&mut prefix[RADICAL_HEADER_LEN..])?;
    let (anchor_id, result_count, ops, consumed) = decode_delta_ops(&prefix, op_count)?;
    ensure!(consumed == end, "radical partial index mismatch");
    ensure!(
        anchor_id == fixture.anchor_id,
        "radical partial anchor mismatch"
    );
    let base = if base_count == 0 {
        PartialRead {
            entries: Vec::new(),
            bytes_read: 0,
        }
    } else if fixture.anchor_lean {
        let bytes = std::fs::read(fixture.anchor.path())?;
        partial_lean_bytes(&bytes, base_count)?
    } else {
        partial_production_file(fixture.anchor.path(), fixture.anchor_id, base_count)?
    };
    let tree = apply_delta(&Tree::from_entries(base.entries), &ops)?;
    let wanted = count.min(result_count);
    ensure!(
        tree.len() >= wanted,
        "radical partial reconstruction is short"
    );
    let entries = tree.entries()[..wanted].to_vec();
    if wanted == result_count {
        ensure!(Tree::from_entries(entries.clone()).hash() == fixture.expected);
    }
    Ok(PartialRead {
        entries,
        bytes_read: end + base.bytes_read,
    })
}

struct RealCorpus {
    label: String,
    path: PathBuf,
    object_format: GitObjectFormat,
    discovered_trees: usize,
    skipped_trees: usize,
    tree_ids: Vec<String>,
    trees: Vec<Tree>,
    parent_tree_ids: HashMap<String, String>,
    git_loose_bytes: usize,
    git_packed_bytes: usize,
    git_pack_files: usize,
}

fn git_output(repo: &Path, arguments: &[&str]) -> Result<Vec<u8>> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(arguments)
        .output()
        .with_context(|| format!("run git in {}", repo.display()))?;
    ensure!(
        output.status.success(),
        "git {} failed in {}: {}",
        arguments.join(" "),
        repo.display(),
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(output.stdout)
}

fn real_object_format(repo: &Path) -> Result<GitObjectFormat> {
    let output = git_output(repo, &["rev-parse", "--show-object-format"])?;
    match std::str::from_utf8(&output)?.trim() {
        "sha1" => Ok(GitObjectFormat::Sha1),
        "sha256" => Ok(GitObjectFormat::Sha256),
        format => bail!("unsupported Git object format {format}"),
    }
}

fn sample_evenly<T: Clone>(values: &[T], limit: usize) -> Vec<T> {
    if values.len() <= limit {
        return values.to_vec();
    }
    if limit == 1 {
        return vec![values[values.len() / 2].clone()];
    }
    (0..limit)
        .map(|index| values[index * (values.len() - 1) / (limit - 1)].clone())
        .collect()
}

fn discover_tree_ids(repo: &Path, limit: usize) -> Result<(usize, Vec<String>)> {
    let output = git_output(
        repo,
        &[
            "cat-file",
            "--batch-all-objects",
            "--batch-check=%(objectname) %(objecttype)",
        ],
    )?;
    let mut tree_ids = BTreeSet::new();
    for line in output.split(|byte| *byte == b'\n') {
        let mut fields = line.split(|byte| *byte == b' ');
        let Some(oid) = fields.next() else {
            continue;
        };
        if fields.next() == Some(b"tree") {
            tree_ids.insert(std::str::from_utf8(oid)?.to_string());
        }
    }
    let discovered = tree_ids.len();
    let tree_ids = if limit == 0 {
        tree_ids.into_iter().collect()
    } else {
        sample_evenly(&tree_ids.into_iter().collect::<Vec<_>>(), limit)
    };
    Ok((discovered, tree_ids))
}

fn discover_parent_tree_ids(repo: &Path, selected: &[String]) -> Result<HashMap<String, String>> {
    let wanted = selected.iter().map(String::as_str).collect::<HashSet<_>>();
    let mut child = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args([
            "log",
            "--all",
            "--topo-order",
            "--reverse",
            "--raw",
            "-r",
            "-t",
            "--root",
            "--no-abbrev",
            "--diff-merges=first-parent",
            "--format=COMMIT %H %T %P",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("walk Git tree history in {}", repo.display()))?;
    let stdout = child.stdout.take().context("Git log stdout")?;
    let mut roots = HashMap::<String, String>::new();
    let mut parents = HashMap::new();
    for line in BufReader::new(stdout).lines() {
        let line = line?;
        if let Some(rest) = line.strip_prefix("COMMIT ") {
            let fields = rest.split_ascii_whitespace().collect::<Vec<_>>();
            ensure!(fields.len() >= 2, "malformed Git commit/tree record");
            let commit = fields[0];
            let root = fields[1];
            if let Some(parent_commit) = fields.get(2)
                && let Some(parent_root) = roots.get(*parent_commit)
                && wanted.contains(root)
                && root != parent_root
            {
                parents
                    .entry(root.to_string())
                    .or_insert_with(|| parent_root.clone());
            }
            roots.insert(commit.to_string(), root.to_string());
            continue;
        }
        if !line.starts_with(':') {
            continue;
        }
        let fields = line.split_ascii_whitespace().collect::<Vec<_>>();
        if fields.len() < 5 || fields[0] != ":040000" || fields[1] != "040000" {
            continue;
        }
        let old = fields[2];
        let new = fields[3];
        if wanted.contains(new) && old != new && !old.bytes().all(|byte| byte == b'0') {
            parents
                .entry(new.to_string())
                .or_insert_with(|| old.to_string());
        }
    }
    let output = child.wait_with_output()?;
    ensure!(
        output.status.success(),
        "git log tree-history walk failed in {}: {}",
        repo.display(),
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(parents)
}

fn git_loose_sizes(repo: &Path, tree_ids: &[String]) -> Result<usize> {
    let temp = TempDir::new()?;
    let object_dir = temp.path().join("objects");
    std::fs::create_dir(&object_dir)?;
    let mut pack = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args([
            "pack-objects",
            "--stdout",
            "--compression=1",
            "--no-reuse-delta",
            "--no-reuse-object",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let pack_stdout = pack.stdout.take().context("Git pack-objects stdout")?;
    let unpack = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["unpack-objects", "-r"])
        .env("GIT_OBJECT_DIRECTORY", &object_dir)
        .stdin(Stdio::from(pack_stdout))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut requests = BufWriter::new(pack.stdin.take().context("Git pack-objects stdin")?);
    for oid in tree_ids {
        writeln!(requests, "{oid}")?;
    }
    drop(requests);
    let unpack_output = unpack.wait_with_output()?;
    let pack_output = pack.wait_with_output()?;
    ensure!(
        pack_output.status.success(),
        "git pack-objects failed in {}: {}",
        repo.display(),
        String::from_utf8_lossy(&pack_output.stderr)
    );
    ensure!(
        unpack_output.status.success(),
        "git unpack-objects failed in {}: {}",
        repo.display(),
        String::from_utf8_lossy(&unpack_output.stderr)
    );

    let mut total = 0usize;
    for oid in tree_ids {
        ensure!(oid.len() > 2, "invalid Git object id {oid}");
        let path = object_dir.join(&oid[..2]).join(&oid[2..]);
        total = total
            .checked_add(usize_from(
                std::fs::metadata(&path)?.len(),
                "Git loose size",
            )?)
            .context("Git loose total overflow")?;
    }
    Ok(total)
}

fn git_packed_sizes(repo: &Path, tree_ids: &[String]) -> Result<(usize, usize)> {
    let git_dir = PathBuf::from(
        std::str::from_utf8(&git_output(repo, &["rev-parse", "--absolute-git-dir"])?)?.trim(),
    );
    let pack_dir = git_dir.join("objects/pack");
    let mut indexes = std::fs::read_dir(&pack_dir)
        .with_context(|| format!("read Git pack directory {}", pack_dir.display()))?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.extension().is_some_and(|extension| extension == "idx"))
        .collect::<Vec<_>>();
    indexes.sort();
    ensure!(
        !indexes.is_empty(),
        "{} has no pack indexes; run `git -C {} gc --prune=now` first",
        repo.display(),
        repo.display()
    );

    let wanted = tree_ids.iter().map(String::as_str).collect::<HashSet<_>>();
    let mut sizes = HashMap::with_capacity(wanted.len());
    for index in &indexes {
        let output = Command::new("git")
            .args(["verify-pack", "-v"])
            .arg(index)
            .output()
            .with_context(|| format!("verify Git pack {}", index.display()))?;
        ensure!(
            output.status.success(),
            "git verify-pack failed for {}: {}",
            index.display(),
            String::from_utf8_lossy(&output.stderr)
        );
        for line in output.stdout.split(|byte| *byte == b'\n') {
            let fields = line
                .split(|byte| byte.is_ascii_whitespace())
                .filter(|field| !field.is_empty())
                .collect::<Vec<_>>();
            if fields.len() < 5 || fields[1] != b"tree" {
                continue;
            }
            let oid = std::str::from_utf8(fields[0])?;
            if wanted.contains(oid) {
                let packed_size = std::str::from_utf8(fields[3])?.parse::<usize>()?;
                sizes.entry(oid.to_string()).or_insert(packed_size);
            }
        }
    }
    let missing = tree_ids
        .iter()
        .filter(|oid| !sizes.contains_key(oid.as_str()))
        .take(3)
        .cloned()
        .collect::<Vec<_>>();
    ensure!(
        missing.is_empty(),
        "{} selected tree objects are not packed (examples: {}); run `git -C {} gc --prune=now` first",
        tree_ids.len() - sizes.len(),
        missing.join(","),
        repo.display()
    );
    Ok((sizes.values().sum(), indexes.len()))
}

fn parse_git_tree(body: &[u8], format: GitObjectFormat) -> Result<Option<Tree>> {
    let oid_len = match format {
        GitObjectFormat::Sha1 => 20,
        GitObjectFormat::Sha256 => 32,
    };
    let mut entries = Vec::new();
    let mut offset = 0usize;
    while offset < body.len() {
        let mode_end = body[offset..]
            .iter()
            .position(|byte| *byte == b' ')
            .map(|relative| offset + relative)
            .context("Git tree mode terminator")?;
        let mode = u32::from_str_radix(std::str::from_utf8(&body[offset..mode_end])?, 8)?;
        let name_start = mode_end + 1;
        let name_end = body[name_start..]
            .iter()
            .position(|byte| *byte == 0)
            .map(|relative| name_start + relative)
            .context("Git tree name terminator")?;
        let Some(name) = std::str::from_utf8(&body[name_start..name_end]).ok() else {
            return Ok(None);
        };
        let oid_start = name_end + 1;
        let oid_end = oid_start
            .checked_add(oid_len)
            .context("Git tree oid length overflow")?;
        let oid = body
            .get(oid_start..oid_end)
            .context("truncated Git tree oid")?;
        let mapped_hash = ContentHash::compute(oid);
        let entry = match mode {
            0o040000 => TreeEntry::directory(name, mapped_hash),
            0o120000 => TreeEntry::symlink(name, mapped_hash),
            0o160000 => TreeEntry::gitlink(name, GitObjectId::from_raw(format, oid)?),
            value if value & 0o170000 == 0o100000 => {
                TreeEntry::file(name, mapped_hash, value & 0o111 != 0)
            }
            _ => return Ok(None),
        };
        let Ok(entry) = entry else {
            return Ok(None);
        };
        entries.push(entry);
        offset = oid_end;
    }
    let tree = Tree::from_entries(entries);
    if tree
        .entries()
        .windows(2)
        .any(|pair| pair[0].name() == pair[1].name())
    {
        return Ok(None);
    }
    Ok(Some(tree))
}

fn load_real_corpus(path: &Path, limit: usize) -> Result<RealCorpus> {
    let path = path.canonicalize()?;
    let label = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("repository")
        .to_string();
    let object_format = real_object_format(&path)?;
    let (discovered_trees, tree_ids) = discover_tree_ids(&path, limit)?;
    let parent_tree_ids = discover_parent_tree_ids(&path, &tree_ids)?;
    let mut child = Command::new("git")
        .arg("-C")
        .arg(&path)
        .args(["cat-file", "--batch"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let stdin = child.stdin.take().context("Git cat-file stdin")?;
    let mut requests = BufWriter::new(stdin);
    let stdout = child.stdout.take().context("Git cat-file stdout")?;
    let mut reader = BufReader::new(stdout);
    let mut trees = Vec::with_capacity(tree_ids.len());
    let mut imported_tree_ids = Vec::with_capacity(tree_ids.len());
    let mut skipped_trees = 0usize;
    for expected_oid in &tree_ids {
        writeln!(requests, "{expected_oid}")?;
        requests.flush()?;
        let mut header = String::new();
        reader.read_line(&mut header)?;
        let mut fields = header.split_whitespace();
        let oid = fields.next().context("Git batch object id")?;
        let kind = fields.next().context("Git batch object type")?;
        let size: usize = fields.next().context("Git batch object size")?.parse()?;
        ensure!(
            oid == expected_oid && kind == "tree",
            "unexpected Git batch response"
        );
        let mut body = vec![0u8; size];
        reader.read_exact(&mut body)?;
        let mut delimiter = [0u8; 1];
        reader.read_exact(&mut delimiter)?;
        ensure!(delimiter == *b"\n", "missing Git batch delimiter");
        if let Some(tree) = parse_git_tree(&body, object_format)? {
            imported_tree_ids.push(expected_oid.clone());
            trees.push(tree);
        } else {
            skipped_trees += 1;
        }
    }
    drop(requests);
    let output = child.wait_with_output()?;
    ensure!(
        output.status.success(),
        "Git tree batch failed in {}: {}",
        path.display(),
        String::from_utf8_lossy(&output.stderr)
    );
    let git_loose_bytes = git_loose_sizes(&path, &imported_tree_ids)?;
    let (git_packed_bytes, git_pack_files) = git_packed_sizes(&path, &imported_tree_ids)?;
    Ok(RealCorpus {
        label,
        path,
        object_format,
        discovered_trees,
        skipped_trees,
        tree_ids: imported_tree_ids,
        trees,
        parent_tree_ids,
        git_loose_bytes,
        git_packed_bytes,
        git_pack_files,
    })
}

struct RealFileFixture {
    raw: NamedTempFile,
    adaptive: NamedTempFile,
    tree_id: ContentHash,
    entries: usize,
}

fn write_temp(bytes: &[u8]) -> Result<NamedTempFile> {
    let mut file = NamedTempFile::new()?;
    file.write_all(bytes)?;
    file.as_file_mut().flush()?;
    Ok(file)
}

fn percentile(sorted: &[usize], numerator: usize, denominator: usize) -> usize {
    if sorted.is_empty() {
        return 0;
    }
    sorted[(sorted.len() - 1) * numerator / denominator]
}

#[derive(Debug)]
struct Timing {
    median_ns: f64,
    mean_ns: f64,
    stddev_ns: f64,
    cv_percent: f64,
    iterations_per_sample: u64,
}

fn run_iterations<F, T>(operation: &mut F, iterations: u64) -> Duration
where
    F: FnMut() -> T,
{
    let started = Instant::now();
    for _ in 0..iterations {
        black_box(operation());
    }
    started.elapsed()
}

fn measure<F, T>(mut operation: F) -> Timing
where
    F: FnMut() -> T,
{
    let mut iterations = 1u64;
    let elapsed = loop {
        let elapsed = run_iterations(&mut operation, iterations);
        if elapsed >= CALIBRATION_TIME || iterations >= (1 << 30) {
            break elapsed;
        }
        iterations = iterations.saturating_mul(2);
    };
    let elapsed_ns = elapsed.as_nanos().max(1) as u64;
    iterations = iterations
        .saturating_mul(SAMPLE_TIME.as_nanos() as u64)
        .div_ceil(elapsed_ns)
        .max(1);

    let mut samples = Vec::with_capacity(SAMPLE_COUNT);
    for _ in 0..SAMPLE_COUNT {
        let elapsed = run_iterations(&mut operation, iterations);
        samples.push(elapsed.as_nanos() as f64 / iterations as f64);
    }
    samples.sort_by(f64::total_cmp);
    let median_ns = samples[samples.len() / 2];
    let mean_ns = samples.iter().sum::<f64>() / samples.len() as f64;
    let variance = samples
        .iter()
        .map(|sample| (sample - mean_ns).powi(2))
        .sum::<f64>()
        / (samples.len() - 1) as f64;
    let stddev_ns = variance.sqrt();
    Timing {
        median_ns,
        mean_ns,
        stddev_ns,
        cv_percent: stddev_ns / mean_ns * 100.0,
        iterations_per_sample: iterations,
    }
}

fn print_timing(entries: usize, operation: &str, timing: &Timing) {
    println!(
        "TIMING,{entries},{operation},{:.3},{:.3},{:.3},{:.3},{}",
        timing.median_ns,
        timing.mean_ns,
        timing.stddev_ns,
        timing.cv_percent,
        timing.iterations_per_sample
    );
}

fn historical_loose(tree: &Tree) -> Vec<u8> {
    let positional = rmp_serde::to_vec(tree).expect("encode positional MessagePack");
    compress_with_dictionary(
        &positional,
        &CompressionConfig::default(),
        CompressionDictionary::TreeStateV1,
    )
    .expect("compress historical loose-tree body")
    .unwrap_or(positional)
}

fn measure_real_partial(
    fixtures: &[&RealFileFixture],
    count: usize,
) -> Result<(Timing, Timing, f64, f64)> {
    ensure!(
        !fixtures.is_empty(),
        "no real file fixtures for {count}-entry read"
    );
    let mut raw_index = 0usize;
    let raw_timing = measure(|| {
        let fixture = &fixtures[raw_index % fixtures.len()];
        raw_index += 1;
        let read = partial_production_file(fixture.raw.path(), fixture.tree_id, count)
            .expect("file-backed raw read");
        black_box(read.entries[count - 1].name().len())
    });
    let mut adaptive_index = 0usize;
    let adaptive_timing = measure(|| {
        let fixture = &fixtures[adaptive_index % fixtures.len()];
        adaptive_index += 1;
        let read = partial_production_file(fixture.adaptive.path(), fixture.tree_id, count)
            .expect("file-backed adaptive read");
        black_box(read.entries[count - 1].name().len())
    });

    let mut raw_bytes = 0usize;
    let mut adaptive_bytes = 0usize;
    for fixture in fixtures {
        let raw = partial_production_file(fixture.raw.path(), fixture.tree_id, count)?;
        let adaptive = partial_production_file(fixture.adaptive.path(), fixture.tree_id, count)?;
        ensure!(
            raw.entries == adaptive.entries,
            "file-backed partial mismatch"
        );
        raw_bytes += raw.bytes_read;
        adaptive_bytes += adaptive.bytes_read;
    }
    Ok((
        raw_timing,
        adaptive_timing,
        raw_bytes as f64 / fixtures.len() as f64,
        adaptive_bytes as f64 / fixtures.len() as f64,
    ))
}

fn validate_real_corpora(corpora: &[RealCorpus]) -> Result<()> {
    ensure!(!corpora.is_empty(), "no real repositories supplied");
    println!(
        "REAL_REPO,label,path,object_format,discovered_trees,measured_trees,skipped_trees,pack_files,min_entries,p50_entries,p95_entries,max_entries,min_name,max_name,mean_name,blob_normal,blob_exec,tree,symlink,gitlink"
    );
    println!(
        "REAL_SIZE,label,trees,htr4_v5,htr4_raw,git_loose,git_packed,historical_loose,v5_over_git_loose,v5_over_git_packed,compressed_trees,raw_fallback_trees"
    );
    println!(
        "RADICAL_SIZE,label,trees,parent_covered,bases,deltas,max_ops,lean_all,hot_bytes,settled_bytes,settled_over_git_loose,settled_over_git_packed,settled_over_raw,settled_over_v5"
    );

    let mut aggregate_raw = 0usize;
    let mut aggregate_historical = 0usize;
    let mut aggregate_adaptive = 0usize;
    let mut aggregate_git_loose = 0usize;
    let mut aggregate_git_packed = 0usize;
    let mut aggregate_trees = 0usize;
    let mut aggregate_compressed = 0usize;
    let mut aggregate_lean = 0usize;
    let mut aggregate_radical_hot = 0usize;
    let mut aggregate_radical_settled = 0usize;
    let mut aggregate_radical_bases = 0usize;
    let mut aggregate_radical_deltas = 0usize;
    let mut aggregate_parent_coverage = 0usize;
    let mut aggregate_radical_max_ops = 0usize;
    let mut file_candidates = Vec::new();
    let mut radical_file_fixtures = Vec::new();
    let config = CompressionConfig::default();

    for corpus in corpora {
        ensure!(
            !corpus.trees.is_empty(),
            "{} yielded no importable trees",
            corpus.label
        );
        let mut entry_counts = corpus.trees.iter().map(Tree::len).collect::<Vec<_>>();
        entry_counts.sort_unstable();
        let mut name_lengths = Vec::new();
        let mut variants = [0usize; 5];
        let mut raw_total = 0usize;
        let mut historical_total = 0usize;
        let mut adaptive_total = 0usize;
        let mut compressed_trees = 0usize;
        let mut raw_fallback_trees = 0usize;
        let mut eligible_files = Vec::new();

        for tree in &corpus.trees {
            for entry in tree.entries() {
                name_lengths.push(entry.name().len());
                let bucket = match (entry.entry_type(), entry.mode()) {
                    (EntryType::Blob, FileMode::Normal) => 0,
                    (EntryType::Blob, FileMode::Executable) => 1,
                    (EntryType::Tree, _) => 2,
                    (EntryType::Symlink, _) => 3,
                    (EntryType::Gitlink, _) => 4,
                    (EntryType::Spoollink, _) => continue,
                    _ => continue,
                };
                variants[bucket] += 1;
            }
            let raw = tree.encode_canonical()?;
            let historical = historical_loose(tree);
            let (_, adaptive) = objects::store::codec::encode_tree(tree, &config)?;
            raw_total += raw.len();
            historical_total += historical.len();
            adaptive_total += adaptive.len();
            if adaptive.get(4) == Some(&TREE_BLOCK_ENCODING_VERSION) {
                compressed_trees += 1;
                if !tree.is_empty() {
                    eligible_files.push((tree.len(), tree.hash(), raw, adaptive));
                }
            } else {
                raw_fallback_trees += 1;
            }
        }

        name_lengths.sort_unstable();
        let mean_name = if name_lengths.is_empty() {
            0.0
        } else {
            name_lengths.iter().sum::<usize>() as f64 / name_lengths.len() as f64
        };
        println!(
            "REAL_REPO,{},{},{:?},{},{},{},{},{},{},{},{},{},{},{:.2},{},{},{},{},{}",
            corpus.label,
            corpus.path.display(),
            corpus.object_format,
            corpus.discovered_trees,
            corpus.trees.len(),
            corpus.skipped_trees,
            corpus.git_pack_files,
            entry_counts[0],
            percentile(&entry_counts, 50, 100),
            percentile(&entry_counts, 95, 100),
            entry_counts[entry_counts.len() - 1],
            name_lengths.first().copied().unwrap_or(0),
            name_lengths.last().copied().unwrap_or(0),
            mean_name,
            variants[0],
            variants[1],
            variants[2],
            variants[3],
            variants[4],
        );
        println!(
            "REAL_SIZE,{},{},{},{},{},{},{},{:.6},{:.6},{},{}",
            corpus.label,
            corpus.trees.len(),
            adaptive_total,
            raw_total,
            corpus.git_loose_bytes,
            corpus.git_packed_bytes,
            historical_total,
            adaptive_total as f64 / corpus.git_loose_bytes as f64,
            adaptive_total as f64 / corpus.git_packed_bytes as f64,
            compressed_trees,
            raw_fallback_trees,
        );
        let radical = build_radical_corpus(corpus)?;
        println!(
            "RADICAL_SIZE,{},{},{},{},{},{},{},{},{},{:.6},{:.6},{:.6},{:.6}",
            corpus.label,
            corpus.trees.len(),
            radical.parent_coverage,
            radical.bases,
            radical.deltas,
            radical.max_ops,
            radical.lean_total,
            radical.hot_total,
            radical.settled_total,
            radical.settled_total as f64 / corpus.git_loose_bytes as f64,
            radical.settled_total as f64 / corpus.git_packed_bytes as f64,
            radical.settled_total as f64 / raw_total as f64,
            radical.settled_total as f64 / adaptive_total as f64,
        );
        let delta_candidates = radical
            .plans
            .iter()
            .enumerate()
            .filter(|(index, plan)| {
                plan.mode == RadicalMode::Delta && !corpus.trees[*index].is_empty()
            })
            .map(|(index, _)| index)
            .collect::<Vec<_>>();
        let per_corpus = (REAL_FILE_SAMPLE_LIMIT / corpora.len()).max(1);
        for index in sample_evenly(&delta_candidates, per_corpus) {
            radical_file_fixtures.push(radical_file_fixture(corpus, &radical, index)?);
        }
        aggregate_raw += raw_total;
        aggregate_historical += historical_total;
        aggregate_adaptive += adaptive_total;
        aggregate_git_loose += corpus.git_loose_bytes;
        aggregate_git_packed += corpus.git_packed_bytes;
        aggregate_trees += corpus.trees.len();
        aggregate_compressed += compressed_trees;
        aggregate_lean += radical.lean_total;
        aggregate_radical_hot += radical.hot_total;
        aggregate_radical_settled += radical.settled_total;
        aggregate_radical_bases += radical.bases;
        aggregate_radical_deltas += radical.deltas;
        aggregate_parent_coverage += radical.parent_coverage;
        aggregate_radical_max_ops = aggregate_radical_max_ops.max(radical.max_ops);

        eligible_files.sort_by_key(|candidate| candidate.0);
        file_candidates.extend(sample_evenly(&eligible_files, REAL_FILE_SAMPLE_LIMIT));
    }

    println!(
        "REAL_SIZE,ALL,{aggregate_trees},{aggregate_adaptive},{aggregate_raw},{aggregate_git_loose},{aggregate_git_packed},{aggregate_historical},{:.6},{:.6},{aggregate_compressed},{}",
        aggregate_adaptive as f64 / aggregate_git_loose as f64,
        aggregate_adaptive as f64 / aggregate_git_packed as f64,
        aggregate_trees - aggregate_compressed,
    );
    println!(
        "RADICAL_SIZE,ALL,{aggregate_trees},{aggregate_parent_coverage},{aggregate_radical_bases},{aggregate_radical_deltas},{aggregate_radical_max_ops},{aggregate_lean},{aggregate_radical_hot},{aggregate_radical_settled},{:.6},{:.6},{:.6},{:.6}",
        aggregate_radical_settled as f64 / aggregate_git_loose as f64,
        aggregate_radical_settled as f64 / aggregate_git_packed as f64,
        aggregate_radical_settled as f64 / aggregate_raw as f64,
        aggregate_radical_settled as f64 / aggregate_adaptive as f64,
    );
    ensure!(
        aggregate_adaptive < aggregate_historical,
        "real-tree adaptive size does not beat historical loose compression"
    );

    file_candidates.sort_by_key(|candidate| candidate.0);
    let file_candidates = sample_evenly(&file_candidates, REAL_FILE_SAMPLE_LIMIT);
    let mut file_fixtures = Vec::with_capacity(file_candidates.len());
    for (entries, tree_id, raw, adaptive) in file_candidates {
        file_fixtures.push(RealFileFixture {
            raw: write_temp(&raw)?,
            adaptive: write_temp(&adaptive)?,
            tree_id,
            entries,
        });
    }
    println!(
        "REAL_FILE_PARTIAL,count,trees,min_tree_entries,max_tree_entries,raw_mean_bytes,adaptive_mean_bytes,raw_median_ns,adaptive_median_ns,time_ratio"
    );
    for count in [1usize, 100] {
        let eligible = file_fixtures
            .iter()
            .filter(|fixture| fixture.entries >= count)
            .collect::<Vec<_>>();
        ensure!(
            !eligible.is_empty(),
            "no compressed real trees with {count} entries"
        );
        let (raw_timing, adaptive_timing, raw_bytes, adaptive_bytes) =
            measure_real_partial(&eligible, count)?;
        let ratio = adaptive_timing.median_ns / raw_timing.median_ns;
        println!(
            "REAL_FILE_PARTIAL,{count},{},{},{},{raw_bytes:.2},{adaptive_bytes:.2},{:.3},{:.3},{ratio:.3}",
            eligible.len(),
            eligible
                .iter()
                .map(|fixture| fixture.entries)
                .min()
                .unwrap_or(0),
            eligible
                .iter()
                .map(|fixture| fixture.entries)
                .max()
                .unwrap_or(0),
            raw_timing.median_ns,
            adaptive_timing.median_ns,
        );
        ensure!(
            ratio <= 2.0,
            "file-backed {count}-entry partial read is {ratio:.3}x raw HTR4"
        );
    }
    println!(
        "RADICAL_FILE_PARTIAL,count,trees,min_tree_entries,max_tree_entries,raw_mean_bytes,radical_mean_bytes,byte_ratio,raw_median_ns,radical_median_ns,time_ratio"
    );
    let mut radical_partial_within_2x = true;
    for count in [1usize, 100] {
        let eligible = radical_file_fixtures
            .iter()
            .filter(|fixture| fixture.entries >= count)
            .collect::<Vec<_>>();
        ensure!(
            !eligible.is_empty(),
            "no radical trees with {count} entries"
        );
        let (raw_timing, radical_timing, raw_bytes, radical_bytes) =
            measure_radical_partial(&eligible, count)?;
        let byte_ratio = radical_bytes / raw_bytes;
        let time_ratio = radical_timing.median_ns / raw_timing.median_ns;
        radical_partial_within_2x &= byte_ratio <= 2.0 && time_ratio <= 2.0;
        println!(
            "RADICAL_FILE_PARTIAL,{count},{},{},{},{raw_bytes:.2},{radical_bytes:.2},{byte_ratio:.3},{:.3},{:.3},{time_ratio:.3}",
            eligible.len(),
            eligible
                .iter()
                .map(|fixture| fixture.entries)
                .min()
                .unwrap_or(0),
            eligible
                .iter()
                .map(|fixture| fixture.entries)
                .max()
                .unwrap_or(0),
            raw_timing.median_ns,
            radical_timing.median_ns,
        );
    }
    println!(
        "REAL_VALIDATION,size_beats_historical=true,file_partial_within_2x=true,radical_smaller_than_git_loose={},radical_partial_within_2x={radical_partial_within_2x}",
        aggregate_radical_settled < aggregate_git_loose,
    );
    Ok(())
}

fn self_check(dictionary: Dictionary<'_>, block_entries: usize) -> Result<()> {
    let tree = fixture(1_000);
    let lean = encode_lean(&tree);
    ensure!(decode_lean(&lean, tree.hash())? == tree);
    let mut changed = tree.clone();
    let changed_name = tree.entries()[500].name().to_string();
    changed.insert(TreeEntry::file(changed_name, content_hash(500, 99), false)?);
    let ops = tree_delta(&tree, &changed);
    ensure!(ops.len() == 1, "one-entry radical self-check delta");
    let delta = encode_delta(tree.hash(), &tree, &changed, &ops);
    ensure!(decode_delta(&delta, &tree, changed.hash())? == changed);
    ensure!(decode_delta(&delta, &tree, tree.hash()).is_err());
    let blocked = encode_blocked(&tree, block_entries, dictionary)?;
    ensure!(decode_blocked(&blocked, dictionary)? == tree);
    let range_start = block_entries.saturating_sub(20);
    let partial = partial_blocked(&blocked, dictionary, range_start, 100)?;
    ensure!(partial.entries == tree.entries()[range_start..range_start + 100]);
    if block_entries < tree.len() {
        let resumed = partial_blocked(&blocked, dictionary, block_entries, 1)?;
        ensure!(resumed.entries == tree.entries()[block_entries..block_entries + 1]);
    }

    let mut bad_hash = blocked.clone();
    bad_hash[8] ^= 1;
    ensure!(decode_blocked(&bad_hash, dictionary).is_err());
    let mut bad_payload = blocked;
    let last = bad_payload.len() - 1;
    bad_payload[last] ^= 1;
    ensure!(decode_blocked(&bad_payload, dictionary).is_err());
    Ok(())
}

fn measure_radical_capture() -> Result<()> {
    let anchor = fixture(100_000);
    let mut current = anchor.clone();
    let changed_name = anchor.entries()[50_000].name().to_string();
    current.insert(TreeEntry::file(
        changed_name,
        content_hash(50_000, 0xfeed),
        false,
    )?);
    let ops = tree_delta(&anchor, &current);
    ensure!(
        ops.len() == 1,
        "100k radical capture fixture must change one entry"
    );
    let raw = current.encode_canonical()?;
    let v5 = objects::store::codec::encode_tree(&current, &CompressionConfig::default())?.1;
    let lean = encode_lean(&current);
    let anchor_id = anchor.hash();
    let delta = encode_delta(anchor_id, &anchor, &current, &ops);
    ensure!(decode_lean(&lean, current.hash())? == current);
    ensure!(decode_delta(&delta, &anchor, current.hash())? == current);
    println!(
        "RADICAL_CAPTURE,entries,changes,htr4_raw_bytes,htr4_v5_bytes,lean_bytes,delta_bytes,operation,median_ns,mean_ns,stddev_ns,cv_percent,iterations_per_sample"
    );
    for (operation, timing) in [
        ("lean_anchor_encode", measure(|| encode_lean(&current))),
        (
            "htr4_raw_encode",
            measure(|| current.encode_canonical().expect("100k raw encode")),
        ),
        (
            "htr4_v5_encode",
            measure(|| {
                objects::store::codec::encode_tree(&current, &CompressionConfig::default())
                    .expect("100k v5 encode")
                    .1
            }),
        ),
        (
            "htr4_raw_decode",
            measure(|| Tree::decode_canonical(&raw).expect("100k raw decode")),
        ),
        (
            "htr4_v5_decode",
            measure(|| Tree::decode_canonical(&v5).expect("100k v5 decode")),
        ),
        (
            "known_delta_encode",
            measure(|| encode_delta(anchor_id, &anchor, &current, &ops)),
        ),
        (
            "diff_and_delta_encode",
            measure(|| {
                let measured_ops = tree_delta(&anchor, &current);
                encode_delta(anchor_id, &anchor, &current, &measured_ops)
            }),
        ),
        (
            "lean_anchor_decode",
            measure(|| decode_lean(&lean, current.hash()).expect("100k lean decode")),
        ),
        (
            "delta_decode",
            measure(|| decode_delta(&delta, &anchor, current.hash()).expect("100k delta decode")),
        ),
    ] {
        println!(
            "RADICAL_CAPTURE,100000,1,{},{},{},{},{operation},{:.3},{:.3},{:.3},{:.3},{}",
            raw.len(),
            v5.len(),
            lean.len(),
            delta.len(),
            timing.median_ns,
            timing.mean_ns,
            timing.stddev_ns,
            timing.cv_percent,
            timing.iterations_per_sample,
        );
    }
    Ok(())
}

fn tune(dictionaries: &[Dictionary<'_>]) -> Result<()> {
    let tree = fixture(10_000);
    let htr4 = tree.encode_canonical()?;
    let historical = historical_loose(&tree);
    println!(
        "TUNE,block_entries,dictionary,encoded_bytes,vs_raw_percent,vs_historical_percent,first_1_bytes,first_100_bytes,first_1_median_ns,first_100_median_ns"
    );
    for dictionary in dictionaries {
        for block_entries in [1, 8, 16, 32, 64, 128, 256, 512] {
            let blocked = Bytes::from(encode_blocked(&tree, block_entries, *dictionary)?);
            let first_1 = partial_blocked(&blocked, *dictionary, 0, 1)?;
            let first_100 = partial_blocked(&blocked, *dictionary, 0, 100)?;
            let first_1_timing = measure(|| {
                let read =
                    partial_blocked(black_box(&blocked), *dictionary, 0, 1).expect("partial");
                black_box(read.entries[0].name());
            });
            let first_100_timing = measure(|| {
                let read =
                    partial_blocked(black_box(&blocked), *dictionary, 0, 100).expect("partial");
                black_box(read.entries[99].name());
            });
            println!(
                "TUNE,{block_entries},{},{},{:.3},{:.3},{},{},{:.3},{:.3}",
                dictionary.name,
                blocked.len(),
                100.0 * (blocked.len() as f64 / htr4.len() as f64 - 1.0),
                100.0 * (blocked.len() as f64 / historical.len() as f64 - 1.0),
                first_1.bytes_read,
                first_100.bytes_read,
                first_1_timing.median_ns,
                first_100_timing.median_ns,
            );
        }
    }
    Ok(())
}

fn crossover(block_entries: usize, dictionary: Dictionary<'_>) -> Result<()> {
    let mut first_raw_win = None;
    let mut last_raw_loss = None;
    let mut first_historical_win = None;
    let mut last_historical_loss = None;
    for entries in 1..=512 {
        let tree = fixture(entries);
        let htr4 = tree.encode_canonical()?;
        let blocked = encode_blocked_htr4(&htr4, block_entries, dictionary)?;
        let historical = historical_loose(&tree);
        if blocked.len() < htr4.len() {
            first_raw_win.get_or_insert(entries);
        } else {
            last_raw_loss = Some(entries);
        }
        if blocked.len() < historical.len() {
            first_historical_win.get_or_insert(entries);
        } else {
            last_historical_loss = Some(entries);
        }
    }
    println!(
        "CROSSOVER,block_entries={},dictionary={},first_smaller_than_raw={},stable_smaller_than_raw_from={},first_smaller_than_historical={},stable_smaller_than_historical_from={},sweep_end=512",
        block_entries,
        dictionary.name,
        first_raw_win.map_or_else(|| "none".into(), |value| value.to_string()),
        last_raw_loss.map_or(1, |value| value + 1),
        first_historical_win.map_or_else(|| "none".into(), |value| value.to_string()),
        last_historical_loss.map_or(1, |value| value + 1),
    );
    Ok(())
}

fn main() -> Result<()> {
    let trained = training_dictionary()?;
    // A production decoder would retain these prepared dictionaries in a
    // process-wide lazy cell. Preparation is outside all retained samples.
    let legacy_encoder = zstd::dict::EncoderDictionary::copy(LEGACY_DICTIONARY, BLOCK_LEVEL);
    let legacy_decoder = zstd::dict::DecoderDictionary::copy(LEGACY_DICTIONARY);
    let trained_encoder = zstd::dict::EncoderDictionary::copy(&trained, BLOCK_LEVEL);
    let trained_decoder = zstd::dict::DecoderDictionary::copy(&trained);
    let dictionaries = [
        Dictionary {
            name: "none",
            id: 0,
            bytes: &[],
            encoder: None,
            decoder: None,
        },
        Dictionary {
            name: "legacy",
            id: LEGACY_DICTIONARY_ID,
            bytes: LEGACY_DICTIONARY,
            encoder: Some(&legacy_encoder),
            decoder: Some(&legacy_decoder),
        },
        Dictionary {
            name: "trained_htr4",
            id: TRAINED_DICTIONARY_ID,
            bytes: &trained,
            encoder: Some(&trained_encoder),
            decoder: Some(&trained_decoder),
        },
    ];
    let dictionary_name = env::var("HTR4_DICTIONARY").unwrap_or_else(|_| "none".into());
    let dictionary = *dictionaries
        .iter()
        .find(|dictionary| dictionary.name == dictionary_name)
        .with_context(|| format!("unknown HTR4_DICTIONARY={dictionary_name}"))?;
    let block_entries = env::var("HTR4_BLOCK_ENTRIES")
        .ok()
        .map(|value| value.parse())
        .transpose()?
        .unwrap_or(256usize);
    let real_tree_limit = env::var("HTR4_REAL_TREE_LIMIT")
        .ok()
        .map(|value| value.parse())
        .transpose()?
        .unwrap_or(DEFAULT_REAL_TREE_LIMIT);
    let real_paths = {
        let supplied = env::args().skip(1).map(PathBuf::from).collect::<Vec<_>>();
        if supplied.is_empty() {
            vec![PathBuf::from(".")]
        } else {
            supplied
        }
    };
    let real_corpora = real_paths
        .iter()
        .map(|path| load_real_corpus(path, real_tree_limit))
        .collect::<Result<Vec<_>>>()?;

    self_check(dictionary, block_entries)?;
    println!("META,seed,0x{SEED:016x}");
    println!("META,samples,{SAMPLE_COUNT}");
    println!("META,target_sample_ms,{}", SAMPLE_TIME.as_millis());
    println!("META,block_entries,{block_entries}");
    println!("META,real_tree_limit_per_repo,{real_tree_limit}");
    println!("META,git_loose,actual_git_loose_object_file_bytes(tree_header+body)");
    println!(
        "META,git_packed,sum_verify_pack_object_record_bytes_excluding_pack_and_index_fixed_overhead"
    );
    println!("META,dictionary,{}", dictionary.name);
    println!("META,dictionary_bytes,{}", dictionary.bytes.len());
    println!("META,dictionary_blake3,{}", blake3::hash(dictionary.bytes));
    println!(
        "META,self_check,v5_roundtrip+range+hash_corruption+payload_corruption;radical_lean+delta_roundtrip+hash_mismatch_ok"
    );
    validate_real_corpora(&real_corpora)?;
    measure_radical_capture()?;
    println!(
        "FIXTURE,entries,min_name,max_name,mean_name,blob_normal,blob_exec,tree,symlink,gitlink,spoollink"
    );
    println!(
        "SIZE,entries,htr4_raw,historical_loose,htr4_whole_zstd3,htb1_blocked,adaptive_size,adaptive_mode"
    );
    println!(
        "TIMING,entries,operation,median_ns,mean_ns,stddev_ns,cv_percent,iterations_per_sample"
    );

    let fixtures: Vec<(usize, Tree)> = SIZES
        .into_iter()
        .map(|entries| (entries, fixture(entries)))
        .collect();

    for (entries, tree) in &fixtures {
        let mut min_name = usize::MAX;
        let mut max_name = 0usize;
        let mut total_name = 0usize;
        let mut variants = [0usize; 6];
        for entry in tree.entries() {
            min_name = min_name.min(entry.name().len());
            max_name = max_name.max(entry.name().len());
            total_name += entry.name().len();
            let bucket = match (entry.entry_type(), entry.mode()) {
                (EntryType::Blob, FileMode::Normal) => 0,
                (EntryType::Blob, FileMode::Executable) => 1,
                (EntryType::Tree, _) => 2,
                (EntryType::Symlink, _) => 3,
                (EntryType::Gitlink, _) => 4,
                (EntryType::Spoollink, _) => 5,
                _ => unreachable!("validated entry kind/mode"),
            };
            variants[bucket] += 1;
        }
        println!(
            "FIXTURE,{entries},{min_name},{max_name},{:.2},{},{},{},{},{},{}",
            total_name as f64 / *entries as f64,
            variants[0],
            variants[1],
            variants[2],
            variants[3],
            variants[4],
            variants[5]
        );

        let htr4 = tree.encode_canonical()?;
        let historical = historical_loose(tree);
        let whole_zstd = zstd::encode_all(htr4.as_slice(), BLOCK_LEVEL)?;
        let blocked = encode_blocked(tree, block_entries, dictionary)?;
        let adaptive = if blocked.len() < htr4.len() {
            blocked.clone()
        } else {
            htr4.clone()
        };
        let mode = if adaptive.starts_with(BLOCK_MAGIC) {
            "blocked"
        } else {
            "raw"
        };
        ensure!(Tree::decode_canonical(&htr4)? == *tree);
        ensure!(decode_blocked(&blocked, dictionary)? == *tree);
        ensure!(decode_adaptive(&adaptive, dictionary)? == *tree);
        let whole_decoded = zstd::decode_all(whole_zstd.as_slice())?;
        ensure!(Tree::decode_canonical(&whole_decoded)? == *tree);
        println!(
            "SIZE,{entries},{},{},{},{},{},{}",
            htr4.len(),
            historical.len(),
            whole_zstd.len(),
            blocked.len(),
            adaptive.len(),
            mode,
        );

        print_timing(
            *entries,
            "htr4_encode",
            &measure(|| tree.encode_canonical().expect("encode HTR4")),
        );
        print_timing(
            *entries,
            "whole_zstd_encode",
            &measure(|| {
                let raw = tree.encode_canonical().expect("encode HTR4");
                zstd::encode_all(raw.as_slice(), BLOCK_LEVEL).expect("compress whole HTR4")
            }),
        );
        print_timing(
            *entries,
            "adaptive_block_encode",
            &measure(|| {
                encode_adaptive(tree, block_entries, dictionary).expect("encode adaptive HTB1")
            }),
        );
        print_timing(
            *entries,
            "htr4_decode",
            &measure(|| Tree::decode_canonical(black_box(&htr4)).expect("decode HTR4")),
        );
        print_timing(
            *entries,
            "whole_zstd_decode",
            &measure(|| {
                let raw = zstd::decode_all(black_box(whole_zstd.as_slice()))
                    .expect("decompress whole HTR4");
                Tree::decode_canonical(&raw).expect("decode whole HTR4")
            }),
        );
        print_timing(
            *entries,
            "adaptive_block_decode",
            &measure(|| {
                decode_adaptive(black_box(&adaptive), dictionary).expect("decode adaptive HTB1")
            }),
        );
    }

    let (entries, tree) = fixtures.last().context("10k fixture")?;
    let htr4 = Bytes::from(tree.encode_canonical()?);
    let whole_zstd = zstd::encode_all(htr4.as_ref(), BLOCK_LEVEL)?;
    let adaptive = Bytes::from(encode_adaptive(tree, block_entries, dictionary)?);
    let tree_id = tree.hash();
    println!("PARTIAL,entries,count,format,bytes_read,median_ns");
    for count in [1usize, 100] {
        let raw_read = partial_htr4(&htr4, tree_id, count);
        let raw_timing = measure(|| {
            let read = partial_htr4(&htr4, tree_id, count);
            black_box(read.entries[count - 1].name());
        });
        println!(
            "PARTIAL,{entries},{count},htr4_raw,{},{:.3}",
            raw_read.bytes_read, raw_timing.median_ns
        );

        let block_read = partial_adaptive(&adaptive, dictionary, tree_id, count);
        let block_timing = measure(|| {
            let read = partial_adaptive(&adaptive, dictionary, tree_id, count);
            black_box(read.entries[count - 1].name());
        });
        println!(
            "PARTIAL,{entries},{count},adaptive_block,{},{:.3}",
            block_read.bytes_read, block_timing.median_ns
        );

        let whole_read = partial_whole_zstd(&whole_zstd, tree_id, count);
        let whole_timing = measure(|| {
            let read = partial_whole_zstd(&whole_zstd, tree_id, count);
            black_box(read.entries[count - 1].name());
        });
        println!(
            "PARTIAL,{entries},{count},whole_zstd,{},{:.3}",
            whole_read.bytes_read, whole_timing.median_ns
        );
    }

    crossover(block_entries, dictionary)?;
    if env::var_os("HTR4_TUNE").is_some() {
        tune(&dictionaries)?;
    }
    Ok(())
}
