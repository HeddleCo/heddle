// SPDX-License-Identifier: Apache-2.0
//! Prototype reftable-style binary model for the refs spike (HeddleCo/heddle#21).
//!
//! Parallel to [`PackedRefsModel`](super::PackedRefsModel) (line-oriented text) but stored as a
//! binary file with a fixed header, per-section offset indexes, and sorted
//! variable-length records. Designed for O(log N) cold lookup without parsing
//! the whole payload.
//!
//! **On-disk layout (little-endian throughout):**
//!
//! ```text
//! Header (16 bytes):
//!   magic         "REFT01\0\0"   (8)
//!   thread_count  u32             (4)
//!   marker_count  u32             (4)
//!
//! Thread index:  [u32; thread_count] — each entry is a byte offset from the
//!                                       start of the file to the start of the
//!                                       corresponding thread record.
//! Thread block:  thread_count records, each:
//!                  name_len  u16
//!                  name      [u8; name_len]   (UTF-8, no `refs/threads/` prefix)
//!                  id        [u8; 16]         (ChangeId raw bytes)
//!                Records appear in ascending name order.
//!
//! Marker index:  [u32; marker_count] — same shape, into the marker block.
//! Marker block:  marker_count records, same layout, sorted.
//!
//! Footer (8 bytes):
//!   magic         "REFT01\0\0"
//! ```
//!
//! **Status:** spike prototype. Not wired through `RefManager`; only the
//! in-memory model + serializer + lookup primitives exist, because the spike's
//! deliverable is a ship-or-defer decision (see `docs/design/reftable-spike.md`),
//! not a production backend.

use objects::object::ChangeId;

/// Magic bytes at the start (and end) of a serialized reftable.
pub const MAGIC: &[u8; 8] = b"REFT01\0\0";

/// On-disk header size in bytes: 8 magic + 4 thread_count + 4 marker_count.
pub const HEADER_LEN: usize = 16;

/// On-disk footer size in bytes: 8 magic.
pub const FOOTER_LEN: usize = 8;

const ID_LEN: usize = 16;

#[derive(Debug, thiserror::Error)]
pub enum ReftableError {
    #[error("reftable is truncated or malformed at offset {0}")]
    Truncated(usize),
    #[error("reftable magic bytes missing or wrong")]
    BadMagic,
    #[error("reftable record name is not valid UTF-8")]
    BadUtf8,
}

/// Sorted, binary-format model of repository refs (threads + markers).
///
/// In-memory the records are held as sorted `Vec`s of `(name, ChangeId)` so
/// mutation stays simple. On disk they serialize to the layout documented at
/// the module level.
#[derive(Debug)]
pub struct ReftableModel {
    threads: Vec<(String, ChangeId)>,
    markers: Vec<(String, ChangeId)>,
}

impl ReftableModel {
    pub fn new() -> Self {
        Self {
            threads: Vec::new(),
            markers: Vec::new(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.threads.is_empty() && self.markers.is_empty()
    }

    pub fn thread_count(&self) -> usize {
        self.threads.len()
    }

    pub fn marker_count(&self) -> usize {
        self.markers.len()
    }

    pub fn set_thread(&mut self, name: &str, id: ChangeId) {
        upsert_sorted(&mut self.threads, name, id);
    }

    pub fn set_marker(&mut self, name: &str, id: ChangeId) {
        upsert_sorted(&mut self.markers, name, id);
    }

    pub fn remove_thread(&mut self, name: &str) -> Option<ChangeId> {
        remove_sorted(&mut self.threads, name)
    }

    pub fn remove_marker(&mut self, name: &str) -> Option<ChangeId> {
        remove_sorted(&mut self.markers, name)
    }

    pub fn get_thread(&self, name: &str) -> Option<ChangeId> {
        find_sorted(&self.threads, name)
    }

    pub fn get_marker(&self, name: &str) -> Option<ChangeId> {
        find_sorted(&self.markers, name)
    }

    pub fn list_threads(&self) -> Vec<String> {
        self.threads.iter().map(|(n, _)| n.clone()).collect()
    }

    pub fn list_markers(&self) -> Vec<String> {
        self.markers.iter().map(|(n, _)| n.clone()).collect()
    }

    /// Serialize to the binary on-disk layout described at the module level.
    pub fn to_bytes(&self) -> Vec<u8> {
        let thread_count = self.threads.len();
        let marker_count = self.markers.len();

        let thread_index_len = thread_count * 4;
        let marker_index_len = marker_count * 4;

        let thread_block_start = HEADER_LEN + thread_index_len;
        let thread_block_len = block_byte_len(&self.threads);
        let marker_index_start = thread_block_start + thread_block_len;
        let marker_block_start = marker_index_start + marker_index_len;
        let marker_block_len = block_byte_len(&self.markers);
        let footer_start = marker_block_start + marker_block_len;
        let total_len = footer_start + FOOTER_LEN;

        let mut out = Vec::with_capacity(total_len);
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&(thread_count as u32).to_le_bytes());
        out.extend_from_slice(&(marker_count as u32).to_le_bytes());

        // Thread index — we need offsets, so build the block first into a
        // scratch buffer while recording each record's start offset.
        let (thread_offsets, thread_block_bytes) = encode_block(&self.threads, thread_block_start);
        for off in &thread_offsets {
            out.extend_from_slice(&off.to_le_bytes());
        }
        out.extend_from_slice(&thread_block_bytes);

        let (marker_offsets, marker_block_bytes) = encode_block(&self.markers, marker_block_start);
        for off in &marker_offsets {
            out.extend_from_slice(&off.to_le_bytes());
        }
        out.extend_from_slice(&marker_block_bytes);

        out.extend_from_slice(MAGIC);
        debug_assert_eq!(out.len(), total_len);
        out
    }

    /// Deserialize from the binary on-disk layout.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, ReftableError> {
        let (thread_count, marker_count) = parse_header(bytes)?;
        let thread_index_start = HEADER_LEN;
        let thread_block_start = thread_index_start + thread_count * 4;
        let (threads, thread_block_end) = decode_block(bytes, thread_block_start, thread_count)?;
        let marker_index_start = thread_block_end;
        let marker_block_start = marker_index_start + marker_count * 4;
        let (markers, marker_block_end) = decode_block(bytes, marker_block_start, marker_count)?;

        let footer_start = marker_block_end;
        if bytes.len() < footer_start + FOOTER_LEN {
            return Err(ReftableError::Truncated(footer_start));
        }
        if &bytes[footer_start..footer_start + FOOTER_LEN] != MAGIC {
            return Err(ReftableError::BadMagic);
        }

        Ok(Self { threads, markers })
    }

    /// Cold-lookup helper: binary-search a single thread by name directly
    /// against the serialized bytes, without materialising the full model.
    pub fn lookup_thread_in_bytes(
        bytes: &[u8],
        name: &str,
    ) -> Result<Option<ChangeId>, ReftableError> {
        let (thread_count, _marker_count) = parse_header(bytes)?;
        binary_search_block(bytes, HEADER_LEN, thread_count, name)
    }

    /// Cold-lookup helper for markers; see [`lookup_thread_in_bytes`].
    pub fn lookup_marker_in_bytes(
        bytes: &[u8],
        name: &str,
    ) -> Result<Option<ChangeId>, ReftableError> {
        let (thread_count, marker_count) = parse_header(bytes)?;
        let thread_index_start = HEADER_LEN;
        let thread_block_start = thread_index_start + thread_count * 4;
        let thread_block_end = thread_block_start
            + block_byte_len_from_index(
                bytes,
                thread_index_start,
                thread_count,
                thread_block_start,
            )?;
        let marker_index_start = thread_block_end;
        binary_search_block(bytes, marker_index_start, marker_count, name)
    }
}

impl Default for ReftableModel {
    fn default() -> Self {
        Self::new()
    }
}

// -- record helpers ---------------------------------------------------------

fn upsert_sorted(records: &mut Vec<(String, ChangeId)>, name: &str, id: ChangeId) {
    match records.binary_search_by(|(n, _)| n.as_str().cmp(name)) {
        Ok(idx) => records[idx].1 = id,
        Err(idx) => records.insert(idx, (name.to_string(), id)),
    }
}

fn remove_sorted(records: &mut Vec<(String, ChangeId)>, name: &str) -> Option<ChangeId> {
    match records.binary_search_by(|(n, _)| n.as_str().cmp(name)) {
        Ok(idx) => Some(records.remove(idx).1),
        Err(_) => None,
    }
}

fn find_sorted(records: &[(String, ChangeId)], name: &str) -> Option<ChangeId> {
    records
        .binary_search_by(|(n, _)| n.as_str().cmp(name))
        .ok()
        .map(|idx| records[idx].1)
}

fn record_byte_len(name: &str) -> usize {
    2 + name.len() + ID_LEN
}

fn block_byte_len(records: &[(String, ChangeId)]) -> usize {
    records.iter().map(|(n, _)| record_byte_len(n)).sum()
}

fn encode_block(records: &[(String, ChangeId)], block_start: usize) -> (Vec<u32>, Vec<u8>) {
    let mut offsets = Vec::with_capacity(records.len());
    let mut bytes = Vec::with_capacity(block_byte_len(records));
    for (name, id) in records {
        offsets.push((block_start + bytes.len()) as u32);
        let name_bytes = name.as_bytes();
        bytes.extend_from_slice(&(name_bytes.len() as u16).to_le_bytes());
        bytes.extend_from_slice(name_bytes);
        bytes.extend_from_slice(id.as_bytes());
    }
    (offsets, bytes)
}

fn parse_header(bytes: &[u8]) -> Result<(usize, usize), ReftableError> {
    if bytes.len() < HEADER_LEN {
        return Err(ReftableError::Truncated(0));
    }
    if &bytes[..MAGIC.len()] != MAGIC {
        return Err(ReftableError::BadMagic);
    }
    let thread_count = u32::from_le_bytes(bytes[8..12].try_into().unwrap()) as usize;
    let marker_count = u32::from_le_bytes(bytes[12..16].try_into().unwrap()) as usize;
    Ok((thread_count, marker_count))
}

fn read_record(bytes: &[u8], offset: usize) -> Result<(String, ChangeId, usize), ReftableError> {
    if bytes.len() < offset + 2 {
        return Err(ReftableError::Truncated(offset));
    }
    let name_len = u16::from_le_bytes(bytes[offset..offset + 2].try_into().unwrap()) as usize;
    let name_start = offset + 2;
    let name_end = name_start + name_len;
    let id_end = name_end + ID_LEN;
    if bytes.len() < id_end {
        return Err(ReftableError::Truncated(offset));
    }
    let name = std::str::from_utf8(&bytes[name_start..name_end])
        .map_err(|_| ReftableError::BadUtf8)?
        .to_string();
    let mut id_bytes = [0u8; ID_LEN];
    id_bytes.copy_from_slice(&bytes[name_end..id_end]);
    Ok((name, ChangeId::from_bytes(id_bytes), id_end))
}

fn decode_block(
    bytes: &[u8],
    block_start: usize,
    count: usize,
) -> Result<(Vec<(String, ChangeId)>, usize), ReftableError> {
    let mut out = Vec::with_capacity(count);
    let mut cursor = block_start;
    for _ in 0..count {
        let (name, id, next) = read_record(bytes, cursor)?;
        out.push((name, id));
        cursor = next;
    }
    Ok((out, cursor))
}

fn block_byte_len_from_index(
    bytes: &[u8],
    index_start: usize,
    count: usize,
    block_start: usize,
) -> Result<usize, ReftableError> {
    if count == 0 {
        return Ok(0);
    }
    let last_off = read_index_entry(bytes, index_start, count - 1)? as usize;
    let (_, _, after_last) = read_record(bytes, last_off)?;
    Ok(after_last - block_start)
}

fn read_index_entry(bytes: &[u8], index_start: usize, idx: usize) -> Result<u32, ReftableError> {
    let off = index_start + idx * 4;
    if bytes.len() < off + 4 {
        return Err(ReftableError::Truncated(off));
    }
    Ok(u32::from_le_bytes(bytes[off..off + 4].try_into().unwrap()))
}

fn binary_search_block(
    bytes: &[u8],
    index_start: usize,
    count: usize,
    name: &str,
) -> Result<Option<ChangeId>, ReftableError> {
    if count == 0 {
        return Ok(None);
    }
    let mut lo = 0usize;
    let mut hi = count;
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        let record_off = read_index_entry(bytes, index_start, mid)? as usize;
        let (mid_name, id, _) = read_record(bytes, record_off)?;
        match mid_name.as_str().cmp(name) {
            std::cmp::Ordering::Equal => return Ok(Some(id)),
            std::cmp::Ordering::Less => lo = mid + 1,
            std::cmp::Ordering::Greater => hi = mid,
        }
    }
    Ok(None)
}
