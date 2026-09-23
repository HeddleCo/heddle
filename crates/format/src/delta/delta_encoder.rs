// SPDX-License-Identifier: Apache-2.0
//! Delta encoder using Git-style compact copy instructions.
//!
//! Copy instruction format (identical to Git):
//! ```text
//! Byte 0: 1oooosss
//!   o bits (3-6): which offset bytes follow (up to 4 → 32-bit offset)
//!   s bits (0-2): which size bytes follow (up to 3 → 24-bit size; all zero = 0x10000)
//! [offset bytes, low to high, only present if corresponding o-bit is set]
//! [size bytes, low to high, only present if corresponding s-bit is set]
//! ```
//!
//! Insert instruction: `[length-1] [literal bytes]` (max 127 bytes per chunk).

/// Minimum match length for targets >= 1024 bytes.
const MIN_MATCH_LENGTH_LARGE: usize = 16;
/// Minimum match length for small targets (< 1024 bytes).
const MIN_MATCH_LENGTH_SMALL: usize = 8;
/// Maximum offsets to inspect for a single 4-byte key.
const MAX_MATCH_CANDIDATES: usize = 1024;
/// Compare long common prefixes in chunks before locating the exact tail.
const MATCH_CHUNK_SIZE: usize = 32;
/// Largest copy length representable by the three size bytes.
const MAX_COPY_LENGTH: usize = 0xFF_FFFF;
/// Sample one base position per 16-byte block for normal-sized objects.
const INDEX_BLOCK_SIZE: usize = 16;
/// Keep each cached index at or below 4 MiB, even for very large bases.
const MAX_INDEX_BYTES: usize = 4 * 1024 * 1024;
/// Preserve short matches in small objects, where a dense flat index is cheap.
const DENSE_INDEX_BELOW: usize = 1024;

#[derive(Clone, Copy, Debug)]
struct IndexEntry {
    key: u32,
    offset: u32,
}

/// Flat, bounded index of sampled positions in a delta base.
#[derive(Debug)]
pub struct DeltaIndex {
    entries: Vec<IndexEntry>,
}

/// Delta encoder.
#[derive(Debug)]
pub struct DeltaEncoder;

impl DeltaEncoder {
    /// Create a new delta encoder.
    pub fn new() -> Self {
        Self
    }

    /// Encode a delta from base to target.
    pub fn encode(base: &[u8], target: &[u8]) -> Vec<u8> {
        if base.is_empty() {
            return Self::encode_insert(target);
        }

        let index = Self::build_index(base);
        Self::encode_with_index(&index, base, target)
    }

    /// Encode a delta using a pre-built index (avoids rebuilding for sliding window).
    pub fn encode_with_index(index: &DeltaIndex, base: &[u8], target: &[u8]) -> Vec<u8> {
        if base.is_empty() {
            return Self::encode_insert(target);
        }

        let min_match = Self::min_match_for(target.len());
        let mut delta = Vec::new();
        let mut pos = 0;
        let mut key = Self::target_key(target, pos);

        while pos < target.len() {
            if let Some((offset, length)) =
                Self::find_best_match(index, base, target, pos, key, min_match)
            {
                Self::emit_copy(&mut delta, offset, length);
                pos += length;
                key = Self::target_key(target, pos);
            } else {
                let start = pos;
                while pos < target.len() && pos - start < 127 {
                    pos += 1;
                    key = Self::roll_target_key(key, target, pos);
                    if Self::find_best_match(index, base, target, pos, key, min_match).is_some() {
                        break;
                    }
                }

                let len = pos - start;
                delta.push(len as u8 - 1);
                delta.extend_from_slice(&target[start..pos]);
            }
        }

        delta
    }

    /// Estimate the encoded delta size without allocating the output.
    pub fn estimate_delta_size(base: &[u8], target: &[u8]) -> usize {
        if base.is_empty() {
            return target.len() + target.len().div_ceil(128);
        }

        let index = Self::build_index(base);
        Self::estimate_delta_size_with_index(&index, base, target)
    }

    /// Estimate delta size using a pre-built index (avoids rebuilding for sliding window).
    pub fn estimate_delta_size_with_index(index: &DeltaIndex, base: &[u8], target: &[u8]) -> usize {
        if base.is_empty() {
            return target.len() + target.len().div_ceil(128);
        }

        let min_match = Self::min_match_for(target.len());
        let mut size = 0usize;
        let mut pos = 0;
        let mut key = Self::target_key(target, pos);

        while pos < target.len() {
            if let Some((offset, length)) =
                Self::find_best_match(index, base, target, pos, key, min_match)
            {
                size += Self::copy_instruction_size(offset, length);
                pos += length;
                key = Self::target_key(target, pos);
            } else {
                let start = pos;
                while pos < target.len() && pos - start < 127 {
                    pos += 1;
                    key = Self::roll_target_key(key, target, pos);
                    if Self::find_best_match(index, base, target, pos, key, min_match).is_some() {
                        break;
                    }
                }
                size += 1 + (pos - start);
            }
        }

        size
    }

    /// Build a flat index over sampled base positions, capped at 4 MiB.
    pub fn build_index(base: &[u8]) -> DeltaIndex {
        if base.len() < 4 {
            return DeltaIndex {
                entries: Vec::new(),
            };
        }

        // Git copy offsets are 32-bit. Larger bases can still be represented by
        // inserts, but only their addressable prefix may be indexed for copies.
        let last_offset = (base.len() - 4).min(u32::MAX as usize);
        let max_entries = MAX_INDEX_BYTES / size_of::<IndexEntry>();
        // Sixteen bytes matches the large-object minimum match length. A
        // shifted target may need up to 15 literal bytes before it reaches a
        // sampled base position; larger objects use a wider stride to fit.
        let stride = if base.len() < DENSE_INDEX_BELOW {
            1
        } else {
            (last_offset + 1)
                .div_ceil(max_entries)
                .max(INDEX_BLOCK_SIZE)
                .next_multiple_of(INDEX_BLOCK_SIZE)
        };
        let mut entries = Vec::with_capacity(last_offset / stride + 1);

        for offset in (0..=last_offset).step_by(stride) {
            let key = u32::from_be_bytes([
                base[offset],
                base[offset + 1],
                base[offset + 2],
                base[offset + 3],
            ]);
            entries.push(IndexEntry {
                key,
                offset: offset as u32,
            });
        }
        entries.sort_unstable_by_key(|entry| (entry.key, entry.offset));
        DeltaIndex { entries }
    }

    /// Emit a Git-style copy instruction.
    ///
    /// Format: `1sssoooo [offset bytes] [size bytes]`
    /// - Bit 7: copy flag (always 1)
    /// - Bits 0-3 (o): which offset bytes (0-3) are present
    /// - Bits 4-6 (s): which size bytes (0-2) are present
    /// - If no s bits set, size = 0x10000
    fn emit_copy(delta: &mut Vec<u8>, offset: usize, length: usize) {
        let mut remaining = length;
        let mut offset = offset;
        while remaining > 0 {
            let chunk = remaining.min(MAX_COPY_LENGTH);
            Self::emit_copy_instruction(delta, offset, chunk);
            offset += chunk;
            remaining -= chunk;
        }
    }

    fn emit_copy_instruction(delta: &mut Vec<u8>, offset: usize, length: usize) {
        let mut cmd: u8 = 0x80;
        let offset = offset as u32;
        let length = length as u32;

        // Offset byte flags: bits 0-3
        // Always emit at least offset byte 0 to avoid the reserved cmd=0x80
        // (which occurs when offset=0 and length=0x10000).
        cmd |= 0x01; // always include offset byte 0
        if offset & 0xFF00 != 0 {
            cmd |= 0x02;
        }
        if offset & 0xFF_0000 != 0 {
            cmd |= 0x04;
        }
        if offset & 0xFF00_0000 != 0 {
            cmd |= 0x08;
        }

        // Size byte flags: bits 4-6
        // Special case: size == 0x10000 is encoded as no size bytes (all s bits zero)
        if length != 0x10000 {
            if length & 0xFF != 0 {
                cmd |= 0x10;
            }
            if length & 0xFF00 != 0 {
                cmd |= 0x20;
            }
            if length & 0xFF_0000 != 0 {
                cmd |= 0x40;
            }
        }

        delta.push(cmd);

        // Emit offset bytes (low to high), only those flagged
        delta.push(offset as u8); // always present (bit 0 always set)
        if offset & 0xFF00 != 0 {
            delta.push((offset >> 8) as u8);
        }
        if offset & 0xFF_0000 != 0 {
            delta.push((offset >> 16) as u8);
        }
        if offset & 0xFF00_0000 != 0 {
            delta.push((offset >> 24) as u8);
        }

        // Emit size bytes (low to high), only those flagged
        if length != 0x10000 {
            if length & 0xFF != 0 {
                delta.push(length as u8);
            }
            if length & 0xFF00 != 0 {
                delta.push((length >> 8) as u8);
            }
            if length & 0xFF_0000 != 0 {
                delta.push((length >> 16) as u8);
            }
        }
    }

    /// Calculate the byte size of a Git-style copy instruction.
    fn copy_instruction_size(offset: usize, length: usize) -> usize {
        let mut remaining = length;
        let mut offset = offset;
        let mut size = 0;
        while remaining > 0 {
            let chunk = remaining.min(MAX_COPY_LENGTH);
            size += Self::copy_instruction_size_one(offset, chunk);
            offset += chunk;
            remaining -= chunk;
        }
        size
    }

    fn copy_instruction_size_one(offset: usize, length: usize) -> usize {
        let offset = offset as u32;
        let length = length as u32;
        let mut n = 1 + 1; // flag byte + offset byte 0 (always present)

        // Additional offset bytes (bits 1-3)
        if offset & 0xFF00 != 0 {
            n += 1;
        }
        if offset & 0xFF_0000 != 0 {
            n += 1;
        }
        if offset & 0xFF00_0000 != 0 {
            n += 1;
        }

        // Size bytes (bits 4-6); 0x10000 = no bytes
        if length != 0x10000 {
            if length & 0xFF != 0 {
                n += 1;
            }
            if length & 0xFF00 != 0 {
                n += 1;
            }
            if length & 0xFF_0000 != 0 {
                n += 1;
            }
        }

        n
    }

    /// Choose minimum match length based on target size.
    fn min_match_for(target_len: usize) -> usize {
        if target_len < 1024 {
            MIN_MATCH_LENGTH_SMALL
        } else {
            MIN_MATCH_LENGTH_LARGE
        }
    }

    fn encode_insert(data: &[u8]) -> Vec<u8> {
        let mut delta = Vec::new();
        for chunk in data.chunks(128) {
            delta.push((chunk.len() - 1) as u8);
            delta.extend_from_slice(chunk);
        }
        delta
    }

    fn find_best_match(
        index: &DeltaIndex,
        base: &[u8],
        target: &[u8],
        pos: usize,
        key: Option<u32>,
        min_match: usize,
    ) -> Option<(usize, usize)> {
        let key = key?;
        let found = index
            .entries
            .binary_search_by_key(&key, |entry| entry.key)
            .ok()?;
        let first = index.entries[..found].partition_point(|entry| entry.key < key);
        let last = found + index.entries[found..].partition_point(|entry| entry.key == key);
        let offsets = &index.entries[first..last];

        let mut best_offset = 0;
        let mut best_length = 0;

        let target_remaining = target.len() - pos;
        let recent_start = offsets.len().saturating_sub(MAX_MATCH_CANDIDATES);
        let mut examined = 0usize;

        if recent_start > 0 {
            let offset = offsets[0].offset as usize;
            let length = Self::match_length(base, offset, target, pos);
            if length > best_length {
                best_length = length;
                best_offset = offset;
            }
            if length == target_remaining {
                return Some((best_offset, best_length));
            }
            examined += 1;
        }

        let remaining_budget = MAX_MATCH_CANDIDATES - examined;
        let start = offsets.len().saturating_sub(remaining_budget);
        for entry in &offsets[start..] {
            let offset = entry.offset as usize;
            let length = Self::match_length(base, offset, target, pos);
            if length > best_length {
                best_length = length;
                best_offset = offset;
            }
            if length == target_remaining {
                break;
            }
        }

        if best_length >= min_match {
            Some((best_offset, best_length))
        } else {
            None
        }
    }

    fn target_key(target: &[u8], pos: usize) -> Option<u32> {
        let bytes = target.get(pos..pos.checked_add(4)?)?;
        Some(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    fn roll_target_key(key: Option<u32>, target: &[u8], pos: usize) -> Option<u32> {
        let next_byte = *target.get(pos.checked_add(3)?)?;
        Some((key? << 8) | u32::from(next_byte))
    }

    fn match_length(base: &[u8], base_pos: usize, target: &[u8], target_pos: usize) -> usize {
        // A long match may need several copy instructions. Keep every next
        // instruction's starting offset within the 32-bit wire field.
        let max_len = (base.len() - base_pos).min(target.len() - target_pos).min(
            (u32::MAX as usize)
                .saturating_sub(base_pos)
                .saturating_add(1),
        );
        let mut len = 0;
        while len + MATCH_CHUNK_SIZE <= max_len
            && base[base_pos + len..base_pos + len + MATCH_CHUNK_SIZE]
                == target[target_pos + len..target_pos + len + MATCH_CHUNK_SIZE]
        {
            len += MATCH_CHUNK_SIZE;
        }
        while len < max_len && base[base_pos + len] == target[target_pos + len] {
            len += 1;
        }
        len
    }
}

impl Default for DeltaEncoder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::{DeltaEncoder, IndexEntry, MAX_INDEX_BYTES};

    #[test]
    fn index_memory_is_bounded() {
        let base = vec![0u8; 16 * 1024 * 1024];
        let index = DeltaEncoder::build_index(&base);
        assert!(index.entries.capacity() * size_of::<IndexEntry>() <= MAX_INDEX_BYTES);
    }
}
