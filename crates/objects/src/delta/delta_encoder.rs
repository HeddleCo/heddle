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

use std::collections::HashMap;

/// Minimum match length for targets >= 1024 bytes.
const MIN_MATCH_LENGTH_LARGE: usize = 16;
/// Minimum match length for small targets (< 1024 bytes).
const MIN_MATCH_LENGTH_SMALL: usize = 8;

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
    pub fn encode_with_index(
        index: &HashMap<[u8; 4], Vec<usize>>,
        base: &[u8],
        target: &[u8],
    ) -> Vec<u8> {
        if base.is_empty() {
            return Self::encode_insert(target);
        }

        let min_match = Self::min_match_for(target.len());
        let mut delta = Vec::new();
        let mut pos = 0;

        while pos < target.len() {
            if let Some((offset, length)) =
                Self::find_best_match(index, base, target, pos, min_match)
            {
                Self::emit_copy(&mut delta, offset, length);
                pos += length;
            } else {
                let start = pos;
                while pos < target.len() && pos - start < 127 {
                    if Self::find_best_match(index, base, target, pos, min_match).is_some() {
                        break;
                    }
                    pos += 1;
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
    pub fn estimate_delta_size_with_index(
        index: &HashMap<[u8; 4], Vec<usize>>,
        base: &[u8],
        target: &[u8],
    ) -> usize {
        if base.is_empty() {
            return target.len() + target.len().div_ceil(128);
        }

        let min_match = Self::min_match_for(target.len());
        let mut size = 0usize;
        let mut pos = 0;

        while pos < target.len() {
            if let Some((offset, length)) =
                Self::find_best_match(index, base, target, pos, min_match)
            {
                size += Self::copy_instruction_size(offset, length);
                pos += length;
            } else {
                let start = pos;
                while pos < target.len() && pos - start < 127 {
                    if Self::find_best_match(index, base, target, pos, min_match).is_some() {
                        break;
                    }
                    pos += 1;
                }
                size += 1 + (pos - start);
            }
        }

        size
    }

    /// Build a 4-byte hash index over the base data.
    pub fn build_index(base: &[u8]) -> HashMap<[u8; 4], Vec<usize>> {
        let mut index: HashMap<[u8; 4], Vec<usize>> = HashMap::new();

        for i in 0..base.len().saturating_sub(4) {
            let key = [base[i], base[i + 1], base[i + 2], base[i + 3]];
            index.entry(key).or_default().push(i);
        }

        index
    }

    /// Emit a Git-style copy instruction.
    ///
    /// Format: `1sssoooo [offset bytes] [size bytes]`
    /// - Bit 7: copy flag (always 1)
    /// - Bits 0-3 (o): which offset bytes (0-3) are present
    /// - Bits 4-6 (s): which size bytes (0-2) are present
    /// - If no s bits set, size = 0x10000
    fn emit_copy(delta: &mut Vec<u8>, offset: usize, length: usize) {
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
        index: &HashMap<[u8; 4], Vec<usize>>,
        base: &[u8],
        target: &[u8],
        pos: usize,
        min_match: usize,
    ) -> Option<(usize, usize)> {
        if pos + 4 > target.len() {
            return None;
        }

        let key = [
            target[pos],
            target[pos + 1],
            target[pos + 2],
            target[pos + 3],
        ];
        let offsets = index.get(&key)?;

        let mut best_offset = 0;
        let mut best_length = 0;

        for &offset in offsets {
            let length = Self::match_length(base, offset, target, pos);
            if length > best_length {
                best_length = length;
                best_offset = offset;
            }
        }

        if best_length >= min_match {
            Some((best_offset, best_length))
        } else {
            None
        }
    }

    fn match_length(base: &[u8], base_pos: usize, target: &[u8], target_pos: usize) -> usize {
        let max_len = (base.len() - base_pos).min(target.len() - target_pos);
        let mut len = 0;
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