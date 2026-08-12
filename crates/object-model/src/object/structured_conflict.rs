// SPDX-License-Identifier: Apache-2.0
//! Merge conflicts as content-addressed, inspectable regions.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};

use crate::object::{
    blob::Blob,
    hash::{ContentHash, StateId},
};

const CONFLICT_ID_DOMAIN: &[u8] = b"heddle-conflict-region-v1\0";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StructuredConflict {
    pub format_version: u8,
    pub conflicts: Vec<ConflictRegion>,
}

impl StructuredConflict {
    pub const FORMAT_VERSION: u8 = 2;

    pub fn new(conflicts: Vec<ConflictRegion>) -> Self {
        Self {
            format_version: Self::FORMAT_VERSION,
            conflicts,
        }
    }

    pub fn encode(&self) -> Result<Vec<u8>, ConflictError> {
        rmp_serde::to_vec(self).map_err(|err| ConflictError::Encoding(err.to_string()))
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, ConflictError> {
        let blob: Self =
            rmp_serde::from_slice(bytes).map_err(|err| ConflictError::Encoding(err.to_string()))?;
        blob.validate()?;
        Ok(blob)
    }

    pub fn validate(&self) -> Result<(), ConflictError> {
        if self.format_version != Self::FORMAT_VERSION {
            return Err(ConflictError::UnsupportedVersion(self.format_version));
        }
        let mut ids = HashSet::new();
        for conflict in &self.conflicts {
            conflict.validate()?;
            if !ids.insert(&conflict.id) {
                return Err(ConflictError::DuplicateId(conflict.id.clone()));
            }
        }
        Ok(())
    }
}

/// One independently addressable conflict region in a file.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConflictRegion {
    /// Deterministic BLAKE3 address derived from the anchor and three hunk hashes.
    pub id: String,
    pub path: String,
    /// Stable semantic address when the source language can be parsed.
    #[serde(default)]
    pub symbol: Option<String>,
    /// Disambiguates byte-identical conflicts under the same anchor.
    pub occurrence: u32,
    /// Range occupied by the rendered marker block.
    pub merged_range: ConflictRange,
    pub base: ConflictSide,
    pub ours: ConflictSide,
    pub theirs: ConflictSide,
}

impl ConflictRegion {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        path: impl Into<String>,
        symbol: Option<String>,
        occurrence: u32,
        merged_range: ConflictRange,
        base: ConflictSide,
        ours: ConflictSide,
        theirs: ConflictSide,
    ) -> Result<Self, ConflictError> {
        let path = path.into();
        let id = stable_conflict_id(
            &path,
            symbol.as_deref(),
            occurrence,
            &base.hunk_hash,
            &ours.hunk_hash,
            &theirs.hunk_hash,
        );
        let conflict = Self {
            id,
            path,
            symbol,
            occurrence,
            merged_range,
            base,
            ours,
            theirs,
        };
        conflict.validate()?;
        Ok(conflict)
    }

    pub fn validate(&self) -> Result<(), ConflictError> {
        if self.path.is_empty() {
            return Err(ConflictError::EmptyPath);
        }
        if self.symbol.as_ref().is_some_and(String::is_empty) {
            return Err(ConflictError::EmptySymbol);
        }
        self.merged_range.validate()?;
        if self.merged_range.is_empty() {
            return Err(ConflictError::EmptyMergedRange);
        }
        self.base.validate()?;
        self.ours.validate()?;
        self.theirs.validate()?;
        let expected = stable_conflict_id(
            &self.path,
            self.symbol.as_deref(),
            self.occurrence,
            &self.base.hunk_hash,
            &self.ours.hunk_hash,
            &self.theirs.hunk_hash,
        );
        if self.id != expected {
            return Err(ConflictError::IdMismatch {
                actual: self.id.clone(),
                expected,
            });
        }
        Ok(())
    }
}

/// Zero-based, half-open line range (`start_line..end_line`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConflictRange {
    pub start_line: u32,
    pub end_line: u32,
}

impl ConflictRange {
    pub fn new(start_line: usize, end_line: usize) -> Result<Self, ConflictError> {
        let start_line = u32::try_from(start_line).map_err(|_| ConflictError::RangeOverflow)?;
        let end_line = u32::try_from(end_line).map_err(|_| ConflictError::RangeOverflow)?;
        let range = Self {
            start_line,
            end_line,
        };
        range.validate()?;
        Ok(range)
    }

    pub fn validate(self) -> Result<(), ConflictError> {
        if self.start_line > self.end_line {
            return Err(ConflictError::InvalidRange {
                start: self.start_line,
                end: self.end_line,
            });
        }
        Ok(())
    }
}

/// Provenance and integrity information for one side of a conflict region.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConflictSide {
    pub source_state: StateId,
    /// `None` means the path was absent in this source state.
    #[serde(default)]
    pub blob_id: Option<ContentHash>,
    pub range: ConflictRange,
    /// BLAKE3 of the exact bytes selected by `range` from `blob_id`.
    pub hunk_hash: ContentHash,
}

impl ConflictSide {
    pub fn new(
        source_state: StateId,
        blob_id: Option<ContentHash>,
        range: ConflictRange,
        blob_bytes: &[u8],
    ) -> Result<Self, ConflictError> {
        let hunk = slice_lines(blob_bytes, range)?;
        let side = Self {
            source_state,
            blob_id,
            range,
            hunk_hash: ContentHash::compute(hunk),
        };
        side.verify_blob(blob_bytes)?;
        Ok(side)
    }

    pub fn validate(&self) -> Result<(), ConflictError> {
        self.range.validate()?;
        if self.blob_id.is_none() && !self.range.is_empty() {
            return Err(ConflictError::AbsentBlobHasLines);
        }
        Ok(())
    }

    /// Verify both the whole-blob address and the selected hunk hash.
    pub fn verify_blob(&self, blob_bytes: &[u8]) -> Result<(), ConflictError> {
        match self.blob_id {
            Some(expected) if Blob::from_slice(blob_bytes).hash() != expected => {
                return Err(ConflictError::BlobHashMismatch);
            }
            None if !blob_bytes.is_empty() => return Err(ConflictError::UnexpectedBlobBytes),
            _ => {}
        }
        let hunk = slice_lines(blob_bytes, self.range)?;
        if ContentHash::compute(hunk) != self.hunk_hash {
            return Err(ConflictError::HunkHashMismatch);
        }
        Ok(())
    }
}

impl ConflictRange {
    fn is_empty(self) -> bool {
        self.start_line == self.end_line
    }
}

fn stable_conflict_id(
    path: &str,
    symbol: Option<&str>,
    occurrence: u32,
    base: &ContentHash,
    ours: &ContentHash,
    theirs: &ContentHash,
) -> String {
    let mut bytes = Vec::with_capacity(CONFLICT_ID_DOMAIN.len() + path.len() + 128);
    bytes.extend_from_slice(CONFLICT_ID_DOMAIN);
    push_field(&mut bytes, path.as_bytes());
    push_field(&mut bytes, symbol.unwrap_or("").as_bytes());
    bytes.extend_from_slice(&occurrence.to_le_bytes());
    bytes.extend_from_slice(base.as_bytes());
    bytes.extend_from_slice(ours.as_bytes());
    bytes.extend_from_slice(theirs.as_bytes());
    format!("conflict-{}", ContentHash::compute(&bytes).to_hex())
}

fn push_field(bytes: &mut Vec<u8>, field: &[u8]) {
    bytes.extend_from_slice(&(field.len() as u64).to_le_bytes());
    bytes.extend_from_slice(field);
}

fn slice_lines(bytes: &[u8], range: ConflictRange) -> Result<&[u8], ConflictError> {
    range.validate()?;
    let start = line_offset(bytes, range.start_line).ok_or(ConflictError::RangeOutOfBounds)?;
    let end = line_offset(bytes, range.end_line).ok_or(ConflictError::RangeOutOfBounds)?;
    Ok(&bytes[start..end])
}

fn line_offset(bytes: &[u8], line: u32) -> Option<usize> {
    if line == 0 {
        return Some(0);
    }
    let mut current = 0u32;
    for (index, byte) in bytes.iter().enumerate() {
        if *byte == b'\n' {
            current += 1;
            if current == line {
                return Some(index + 1);
            }
        }
    }
    (current == line || (bytes.last().is_some_and(|byte| *byte != b'\n') && current + 1 == line))
        .then_some(bytes.len())
}

#[derive(Debug, thiserror::Error)]
pub enum ConflictError {
    #[error("unsupported structured conflict version {0}")]
    UnsupportedVersion(u8),
    #[error("conflict path must not be empty")]
    EmptyPath,
    #[error("conflict symbol must be absent rather than empty")]
    EmptySymbol,
    #[error("rendered conflict marker range must not be empty")]
    EmptyMergedRange,
    #[error("duplicate conflict id {0}")]
    DuplicateId(String),
    #[error("conflict id {actual} does not match stable address {expected}")]
    IdMismatch { actual: String, expected: String },
    #[error("conflict range {start}..{end} is invalid")]
    InvalidRange { start: u32, end: u32 },
    #[error("conflict range exceeds u32 line addressing")]
    RangeOverflow,
    #[error("conflict range exceeds source blob")]
    RangeOutOfBounds,
    #[error("an absent source blob cannot contain hunk lines")]
    AbsentBlobHasLines,
    #[error("source blob bytes do not match the recorded BLAKE3 id")]
    BlobHashMismatch,
    #[error("source hunk bytes do not match the recorded BLAKE3 hash")]
    HunkHashMismatch,
    #[error("bytes were supplied for an absent source blob")]
    UnexpectedBlobBytes,
    #[error("structured conflict encoding error: {0}")]
    Encoding(String),
}
