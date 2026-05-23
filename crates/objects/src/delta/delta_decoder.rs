// SPDX-License-Identifier: Apache-2.0
//! Delta decoder for Git-style compact copy instructions.
//!
//! Copy instruction format:
//! ```text
//! Byte 0: 1oooosss
//!   o bits (4-7 after the MSB): which offset bytes follow
//!   s bits (0-2): which size bytes follow (all zero = size 0x10000)
//! [offset bytes, low to high]
//! [size bytes, low to high]
//! ```
//!
//! Insert instruction: `[length-1] [literal bytes]`

/// Maximum decoded delta size accepted by default.
pub const MAX_DELTA_OUTPUT_SIZE: usize = 128 * 1024 * 1024;

/// Errors that can occur while decoding a delta stream.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DeltaError {
    #[error("delta output exceeds max size {max_output} bytes (attempted {attempted} bytes)")]
    OutputLimitExceeded { attempted: usize, max_output: usize },

    #[error(
        "delta literal at instruction {instruction_offset} requires {length} bytes, but only {available} remain"
    )]
    TruncatedLiteral {
        instruction_offset: usize,
        length: usize,
        available: usize,
    },

    #[error(
        "delta copy at instruction {instruction_offset} needs {expected_bytes} more bytes, but only {available} remain"
    )]
    TruncatedCopyInstruction {
        instruction_offset: usize,
        expected_bytes: usize,
        available: usize,
    },

    #[error(
        "delta copy at instruction {instruction_offset} references base range {copy_offset}..{copy_end}, but base length is {base_len}"
    )]
    InvalidBaseRange {
        instruction_offset: usize,
        copy_offset: usize,
        copy_end: usize,
        base_len: usize,
    },

    #[error("reserved delta instruction 0x80 at offset {instruction_offset}")]
    ReservedInstruction { instruction_offset: usize },
}

/// Delta decoder.
#[derive(Debug)]
pub struct DeltaDecoder;

impl DeltaDecoder {
    /// Create a new delta decoder.
    pub fn new() -> Self {
        Self
    }

    /// Decode a delta to reconstruct the target from base.
    pub fn decode(base: &[u8], delta: &[u8], max_output: usize) -> Result<Vec<u8>, DeltaError> {
        let mut target = Vec::new();
        let mut pos = 0;

        while pos < delta.len() {
            let instruction_offset = pos;
            let header = delta[pos];
            pos += 1;

            if header & 0x80 == 0 {
                // Insert instruction
                let len = (header + 1) as usize;
                if pos + len > delta.len() {
                    return Err(DeltaError::TruncatedLiteral {
                        instruction_offset,
                        length: len,
                        available: delta.len().saturating_sub(pos),
                    });
                }

                Self::ensure_output_limit(target.len(), len, max_output)?;
                target.extend_from_slice(&delta[pos..pos + len]);
                pos += len;
                continue;
            }

            // Copy instruction: 1oooosss [offset bytes] [size bytes]
            // cmd=0x80 with no offset/size bits set is reserved
            if header == 0x80 {
                return Err(DeltaError::ReservedInstruction { instruction_offset });
            }

            // Count expected bytes from flag bits
            // Bits 0-3: offset byte flags, bits 4-6: size byte flags
            let expected = (header & 0x01 != 0) as usize
                + (header & 0x02 != 0) as usize
                + (header & 0x04 != 0) as usize
                + (header & 0x08 != 0) as usize
                + (header & 0x10 != 0) as usize
                + (header & 0x20 != 0) as usize
                + (header & 0x40 != 0) as usize;

            if pos + expected > delta.len() {
                return Err(DeltaError::TruncatedCopyInstruction {
                    instruction_offset,
                    expected_bytes: expected,
                    available: delta.len().saturating_sub(pos),
                });
            }

            // Decode offset (bits 0-3)
            let mut offset: usize = 0;
            if header & 0x01 != 0 {
                offset |= delta[pos] as usize;
                pos += 1;
            }
            if header & 0x02 != 0 {
                offset |= (delta[pos] as usize) << 8;
                pos += 1;
            }
            if header & 0x04 != 0 {
                offset |= (delta[pos] as usize) << 16;
                pos += 1;
            }
            if header & 0x08 != 0 {
                offset |= (delta[pos] as usize) << 24;
                pos += 1;
            }

            // Decode size (bits 4-6)
            let mut length: usize = 0;
            if header & 0x10 != 0 {
                length |= delta[pos] as usize;
                pos += 1;
            }
            if header & 0x20 != 0 {
                length |= (delta[pos] as usize) << 8;
                pos += 1;
            }
            if header & 0x40 != 0 {
                length |= (delta[pos] as usize) << 16;
                pos += 1;
            }
            // If no size bits set, size = 0x10000
            if length == 0 {
                length = 0x10000;
            }

            let copy_end = offset
                .checked_add(length)
                .ok_or(DeltaError::InvalidBaseRange {
                    instruction_offset,
                    copy_offset: offset,
                    copy_end: usize::MAX,
                    base_len: base.len(),
                })?;

            if copy_end > base.len() {
                return Err(DeltaError::InvalidBaseRange {
                    instruction_offset,
                    copy_offset: offset,
                    copy_end,
                    base_len: base.len(),
                });
            }

            Self::ensure_output_limit(target.len(), length, max_output)?;
            target.extend_from_slice(&base[offset..copy_end]);
        }

        Ok(target)
    }

    fn ensure_output_limit(
        current_len: usize,
        append_len: usize,
        max_output: usize,
    ) -> Result<(), DeltaError> {
        let attempted =
            current_len
                .checked_add(append_len)
                .ok_or(DeltaError::OutputLimitExceeded {
                    attempted: usize::MAX,
                    max_output,
                })?;

        if attempted > max_output {
            return Err(DeltaError::OutputLimitExceeded {
                attempted,
                max_output,
            });
        }

        Ok(())
    }
}

impl Default for DeltaDecoder {
    fn default() -> Self {
        Self::new()
    }
}
